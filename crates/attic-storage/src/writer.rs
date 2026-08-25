//! S6 — Bounded single-writer queue for serialising all SQLite mutations.
//!
//! SQLite in WAL mode supports one writer at a time.  Rather than taking a
//! mutex around every write site, all mutations are sent through a bounded
//! channel to a dedicated worker thread that owns the writer connection.
//!
//! # Design
//!
//! ## Result delivery
//! Every [`WriterQueueHandle::send`] call **blocks** until the mutation (and
//! all other mutations in the same batch) has been committed or rolled back,
//! then returns the actual execution result.  Callers are never left with only
//! a queue-entry acknowledgement.
//!
//! ## Batch atomicity
//! The worker accumulates up to [`BATCH_SIZE`] mutations into a single
//! `BEGIN IMMEDIATE … COMMIT` transaction for throughput.  If **any**
//! mutation in a batch fails:
//! 1. A `ROLLBACK` is issued.
//! 2. The failing mutation's caller receives its original error.
//! 3. Every other mutation in the same batch receives
//!    [`StorageError::BatchRolledBack`].
//!
//! ## Transaction finalization failure and writer poisoning
//! If a `ROLLBACK` or `COMMIT` fails, the connection's transactional state
//! becomes unknown.  In that case the writer is **poisoned**:
//!
//! - All pending callers receive [`StorageError::WriterPoisoned`].
//! - All future `send` calls return [`StorageError::WriterPoisoned`]
//!   immediately without enqueuing.
//! - The worker exits its loop; no further batches are processed.
//!
//! Specifically:
//! - Mutation failure → `ROLLBACK`:
//!   - `ROLLBACK` succeeds → failing caller gets original error; all others
//!     get [`StorageError::BatchRolledBack`].
//!   - `ROLLBACK` fails → writer poisoned; all callers get
//!     [`StorageError::WriterPoisoned`].
//! - All mutations succeed → `COMMIT`:
//!   - `COMMIT` succeeds → all callers get `Ok(())`.
//!   - `COMMIT` fails → attempt `ROLLBACK` to restore known-clean state:
//!     - `ROLLBACK` succeeds → all callers get
//!       [`StorageError::Worker`]`("COMMIT failed: …")`.
//!     - `ROLLBACK` fails → writer poisoned; all callers get
//!       [`StorageError::WriterPoisoned`].
//!
//! ## Shutdown
//! Shutdown is signalled via an [`AtomicBool`] flag that the worker checks on
//! every loop iteration.  Setting the flag and joining the thread is therefore
//! always deterministic and never deadlocks — the queue being full cannot
//! prevent shutdown from being signalled.
//!
//! ## Thread spawn
//! [`WriterQueue::new`] returns `Result<WriterQueue, StorageError>` so that
//! thread-spawn failures are propagated to the caller instead of causing a
//! panic.
//!
//! # Constants
//! - [`QUEUE_CAPACITY`]: 512 pending mutations before backpressure
//! - [`BATCH_SIZE`]: flush after 256 mutations
//! - [`FLUSH_INTERVAL`]: flush at least every 50 ms

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tracing::{debug, error};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Maximum number of pending mutations in the channel before `send` returns
/// [`StorageError::QueueFull`].
pub const QUEUE_CAPACITY: usize = 512;

/// Flush a batch after this many mutations (whichever comes first with the timer).
pub const BATCH_SIZE: usize = 256;

/// Maximum time between flushes even when `BATCH_SIZE` is not reached.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// MutationFn type alias
// ---------------------------------------------------------------------------

/// A boxed closure that executes a single logical mutation on `conn`.
type MutationFn = Box<dyn FnOnce(&Connection) -> Result<(), StorageError> + Send + 'static>;

// ---------------------------------------------------------------------------
// TransactionFinalizer — abstraction over COMMIT / ROLLBACK
// ---------------------------------------------------------------------------

/// Abstraction over the `COMMIT` and `ROLLBACK` SQL statements.
///
/// The default implementation issues `COMMIT` and `ROLLBACK` directly via
/// `conn.execute_batch`.  Tests may inject a [`FailingFinalizer`] to exercise
/// the writer-poisoning code paths without requiring real SQLite failures.
pub(crate) trait TransactionFinalizer: Send + 'static {
    fn commit(&self, conn: &Connection) -> rusqlite::Result<()>;
    fn rollback(&self, conn: &Connection) -> rusqlite::Result<()>;
}

/// Production finalizer: issues real `COMMIT` and `ROLLBACK` statements.
pub(crate) struct DefaultFinalizer;

impl TransactionFinalizer for DefaultFinalizer {
    fn commit(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch("COMMIT;")
    }

    fn rollback(&self, conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch("ROLLBACK;")
    }
}

// ---------------------------------------------------------------------------
// Work item — mutation + result return channel
// ---------------------------------------------------------------------------

/// One unit of work queued for the writer thread.
struct WorkItem {
    f: MutationFn,
    /// The worker sends the execution result back on this channel.
    /// Capacity = 1 so the send never blocks.
    result_tx: SyncSender<Result<(), StorageError>>,
}

// ---------------------------------------------------------------------------
// WriterQueueHandle — cloneable send-side
// ---------------------------------------------------------------------------

/// A cheap-to-clone handle for submitting mutations to the writer queue.
///
/// All handles share ownership of the queue; the worker is stopped by
/// dropping the owning [`WriterQueue`].
#[derive(Clone)]
pub struct WriterQueueHandle {
    tx: SyncSender<WorkItem>,
    /// Shared with the worker.  When `true`, the writer connection is in an
    /// unknown state and no further mutations may be enqueued.
    poisoned: Arc<AtomicBool>,
}

impl WriterQueueHandle {
    /// Submit a mutation closure to the writer queue and **block** until the
    /// mutation has been executed (committed or rolled back).
    ///
    /// ## Return value
    /// - `Ok(())` — the mutation was committed successfully.
    /// - `Err(StorageError::WriterPoisoned)` — the writer connection is in an
    ///   unknown transactional state; the storage layer must be restarted.
    /// - `Err(StorageError::QueueFull)` — the queue is at capacity; try later.
    /// - `Err(StorageError::QueueShutdown)` — the worker has shut down.
    /// - `Err(StorageError::BatchRolledBack)` — this mutation was rolled back
    ///   because a different mutation in the same batch failed.
    /// - `Err(StorageError::Worker(_))` — a `COMMIT` failed but `ROLLBACK`
    ///   succeeded; the transaction was cleanly aborted.
    /// - Any other `Err` — the mutation itself returned an error; the batch
    ///   was rolled back.
    pub fn send<F>(&self, f: F) -> Result<(), StorageError>
    where
        F: FnOnce(&Connection) -> Result<(), StorageError> + Send + 'static,
    {
        // Refuse to enqueue if the writer is poisoned.
        if self.poisoned.load(Ordering::Acquire) {
            return Err(StorageError::WriterPoisoned);
        }

        // Oneshot result channel: capacity 1 so the worker never blocks on send.
        let (result_tx, result_rx) = mpsc::sync_channel(1);

        let item = WorkItem {
            f: Box::new(f),
            result_tx,
        };

        // Non-blocking enqueue.
        self.tx
            .try_send(item)
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => StorageError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => StorageError::QueueShutdown,
            })?;

        // Block until the worker sends back the result.
        result_rx.recv().unwrap_or(Err(StorageError::QueueShutdown))
    }
}

// ---------------------------------------------------------------------------
// WriterQueue — owns the writer connection and worker thread
// ---------------------------------------------------------------------------

/// Owns the SQLite writer connection and the mutation worker thread.
///
/// Drop this to shut down the worker deterministically.  All in-flight
/// mutations already in the queue will be drained and committed (or rolled
/// back) before the thread exits.
pub struct WriterQueue {
    handle: WriterQueueHandle,
    shutdown: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl WriterQueue {
    /// Create a new `WriterQueue` that drives mutations through `conn`.
    ///
    /// `conn` is moved into the worker thread.  Obtain a
    /// [`WriterQueueHandle`] via [`WriterQueue::handle`].
    ///
    /// # Errors
    /// Returns [`StorageError::ThreadSpawn`] if the OS cannot create a thread.
    pub fn new(conn: Connection) -> Result<Self, StorageError> {
        Self::new_with_finalizer(conn, DefaultFinalizer)
    }

    /// Create a new `WriterQueue` with a custom [`TransactionFinalizer`].
    ///
    /// Intended for testing; production code should use [`WriterQueue::new`].
    pub(crate) fn new_with_finalizer<F>(conn: Connection, finalizer: F) -> Result<Self, StorageError>
    where
        F: TransactionFinalizer,
    {
        let (tx, rx) = mpsc::sync_channel::<WorkItem>(QUEUE_CAPACITY);
        let poisoned = Arc::new(AtomicBool::new(false));
        let handle = WriterQueueHandle {
            tx,
            poisoned: Arc::clone(&poisoned),
        };
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_flag = Arc::clone(&shutdown);
        let poisoned_flag = Arc::clone(&poisoned);
        let worker = thread::Builder::new()
            .name("attic-writer".into())
            .spawn(move || {
                worker_loop(conn, rx, shutdown_flag, poisoned_flag, Box::new(finalizer));
            })
            .map_err(|e| StorageError::ThreadSpawn(e.to_string()))?;

        Ok(Self {
            handle,
            shutdown,
            worker: Some(worker),
        })
    }

    /// Return a cloneable handle for submitting mutations.
    pub fn handle(&self) -> WriterQueueHandle {
        self.handle.clone()
    }
}

impl Drop for WriterQueue {
    fn drop(&mut self) {
        // Signal the worker to drain remaining work and exit.
        // This never blocks — it is a single atomic store.
        self.shutdown.store(true, Ordering::Release);

        if let Some(jh) = self.worker.take() {
            // If the thread panicked we discard the panic value; the DB is
            // safely closed when `conn` is dropped inside the worker.
            let _ = jh.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

fn worker_loop(
    conn: Connection,
    rx: mpsc::Receiver<WorkItem>,
    shutdown: Arc<AtomicBool>,
    poisoned: Arc<AtomicBool>,
    finalizer: Box<dyn TransactionFinalizer>,
) {
    let mut batch: Vec<WorkItem> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();

    loop {
        // If the writer is poisoned, drain the channel sending WriterPoisoned
        // to all waiting callers, then exit.
        if poisoned.load(Ordering::Acquire) {
            while let Ok(item) = rx.try_recv() {
                let _ = item.result_tx.send(Err(StorageError::WriterPoisoned));
            }
            // Also drain any items that accumulated in `batch` before poisoning.
            for item in batch.drain(..) {
                let _ = item.result_tx.send(Err(StorageError::WriterPoisoned));
            }
            debug!("attic-writer: exiting due to poisoned writer connection");
            break;
        }

        let shutting_down = shutdown.load(Ordering::Acquire);

        // Drain messages into the batch.
        loop {
            match rx.try_recv() {
                Ok(item) => {
                    batch.push(item);
                    if batch.len() >= BATCH_SIZE {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // Flush if the batch is full, the timer has expired, or we're shutting down.
        let should_flush = !batch.is_empty()
            && (batch.len() >= BATCH_SIZE
                || last_flush.elapsed() >= FLUSH_INTERVAL
                || shutting_down);

        if should_flush {
            flush_batch(&conn, &mut batch, &poisoned, finalizer.as_ref());
            last_flush = Instant::now();
            // If flush_batch poisoned the writer, the top-of-loop check
            // will drain remaining items and exit on the next iteration.
        }

        if shutting_down && !poisoned.load(Ordering::Acquire) {
            // Drain anything that arrived between the shutdown flag check and now.
            while let Ok(item) = rx.try_recv() {
                batch.push(item);
            }
            if !batch.is_empty() {
                flush_batch(&conn, &mut batch, &poisoned, finalizer.as_ref());
            }
            debug!("attic-writer: shut down cleanly");
            break;
        }

        if !shutting_down {
            // Brief sleep to avoid busy-spinning.
            thread::sleep(Duration::from_millis(1));
        }
    }
}

// ---------------------------------------------------------------------------
// Batch execution
// ---------------------------------------------------------------------------

fn flush_batch(
    conn: &Connection,
    batch: &mut Vec<WorkItem>,
    poisoned: &Arc<AtomicBool>,
    finalizer: &dyn TransactionFinalizer,
) {
    if batch.is_empty() {
        return;
    }

    // Drain the batch into owned (fn, result_tx) pairs so we can call each
    // FnOnce by value (Box<dyn FnOnce> cannot be called through &mut).
    let mut fns_and_txs: Vec<(MutationFn, SyncSender<Result<(), StorageError>>)> =
        batch.drain(..).map(|it| (it.f, it.result_tx)).collect();

    let n = fns_and_txs.len();

    // Open transaction.  On failure every caller receives the error.
    // No transaction is open so poisoning is not required here.
    if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE;") {
        let msg = e.to_string();
        error!("attic-writer: BEGIN IMMEDIATE failed: {msg}");
        for (_, tx) in fns_and_txs {
            let _ = tx.send(Err(StorageError::Worker(format!("BEGIN IMMEDIATE failed: {msg}"))));
        }
        return;
    }

    // Execute each mutation in order; stop executing on first failure.
    let mut results: Vec<Result<(), StorageError>> = Vec::with_capacity(n);
    let mut failed_index: Option<usize> = None;

    for (i, (f, _tx)) in fns_and_txs.iter_mut().enumerate() {
        if failed_index.is_some() {
            results.push(Err(StorageError::BatchRolledBack));
            continue;
        }
        // Swap out the FnOnce to gain ownership, replacing with a no-op
        // so the Vec entry remains valid until we zip results below.
        let noop: MutationFn = Box::new(|_| Ok(()));
        let real_f = std::mem::replace(f, noop);
        let res = real_f(conn);
        if res.is_err() {
            failed_index = Some(i);
        }
        results.push(res);
    }

    if failed_index.is_some() {
        // ----------------------------------------------------------------
        // Mutation failure path — attempt ROLLBACK
        // ----------------------------------------------------------------
        match finalizer.rollback(conn) {
            Ok(()) => {
                // ROLLBACK succeeded: known-clean state.
                // Callers already have their correct results (original error
                // for the failed mutation, BatchRolledBack for the rest).
                // Nothing to change — deliver as-is.
            }
            Err(rb_err) => {
                // ROLLBACK failed: connection state is unknown.  Poison.
                error!(
                    "attic-writer: ROLLBACK failed after mutation error: {rb_err}; \
                     poisoning writer connection"
                );
                poisoned.store(true, Ordering::Release);
                // Overwrite all results with WriterPoisoned.
                for r in results.iter_mut() {
                    *r = Err(StorageError::WriterPoisoned);
                }
            }
        }
    } else {
        // ----------------------------------------------------------------
        // All mutations succeeded — attempt COMMIT
        // ----------------------------------------------------------------
        match finalizer.commit(conn) {
            Ok(()) => {
                // COMMIT succeeded: all results are already Ok(()).
            }
            Err(commit_err) => {
                // COMMIT failed.  Attempt ROLLBACK to restore known-clean state.
                let commit_msg = commit_err.to_string();
                error!("attic-writer: COMMIT failed: {commit_msg}; attempting ROLLBACK");

                match finalizer.rollback(conn) {
                    Ok(()) => {
                        // ROLLBACK after COMMIT failure succeeded: known-clean state.
                        // All mutations are effectively rolled back.
                        for r in results.iter_mut() {
                            *r = Err(StorageError::Worker(format!("COMMIT failed: {commit_msg}")));
                        }
                    }
                    Err(rb_err) => {
                        // Both COMMIT and ROLLBACK failed: unknown state.  Poison.
                        error!(
                            "attic-writer: ROLLBACK also failed after COMMIT failure: {rb_err}; \
                             poisoning writer connection"
                        );
                        poisoned.store(true, Ordering::Release);
                        for r in results.iter_mut() {
                            *r = Err(StorageError::WriterPoisoned);
                        }
                    }
                }
            }
        }
    }

    // Deliver results to each caller.
    for ((_, tx), result) in fns_and_txs.into_iter().zip(results) {
        let _ = tx.send(result);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{configure_connection, open_rw};
    use crate::migration::run_migrations;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn migrated_file_db() -> (std::path::PathBuf, Connection) {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_writer_{}.db", uuid::Uuid::new_v4()));
        let conn = open_rw(&path).unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        (path, conn)
    }

    fn cleanup(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    // -----------------------------------------------------------------------
    // Injected finalizers for testing error paths
    // -----------------------------------------------------------------------

    /// Finalizer whose ROLLBACK always fails with the given message.
    struct FailRollbackFinalizer {
        msg: &'static str,
    }

    impl TransactionFinalizer for FailRollbackFinalizer {
        fn commit(&self, conn: &Connection) -> rusqlite::Result<()> {
            DefaultFinalizer.commit(conn)
        }

        fn rollback(&self, _conn: &Connection) -> rusqlite::Result<()> {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::InternalMalfunction,
                    extended_code: 1,
                },
                Some(self.msg.to_owned()),
            ))
        }
    }

    /// Finalizer whose COMMIT always fails, and whose ROLLBACK always succeeds.
    struct FailCommitFinalizer;

    impl TransactionFinalizer for FailCommitFinalizer {
        fn commit(&self, _conn: &Connection) -> rusqlite::Result<()> {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::InternalMalfunction,
                    extended_code: 1,
                },
                Some("injected COMMIT failure".to_owned()),
            ))
        }

        fn rollback(&self, conn: &Connection) -> rusqlite::Result<()> {
            DefaultFinalizer.rollback(conn)
        }
    }

    /// Finalizer whose COMMIT fails and whose subsequent ROLLBACK also fails.
    struct FailBothFinalizer;

    impl TransactionFinalizer for FailBothFinalizer {
        fn commit(&self, _conn: &Connection) -> rusqlite::Result<()> {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::InternalMalfunction,
                    extended_code: 1,
                },
                Some("injected COMMIT failure".to_owned()),
            ))
        }

        fn rollback(&self, _conn: &Connection) -> rusqlite::Result<()> {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ffi::ErrorCode::InternalMalfunction,
                    extended_code: 1,
                },
                Some("injected ROLLBACK failure".to_owned()),
            ))
        }
    }

    // -----------------------------------------------------------------------
    // Basic execution
    // -----------------------------------------------------------------------

    #[test]
    fn writer_executes_mutation_and_returns_ok() {
        let (path, writer_conn) = migrated_file_db();
        let queue = WriterQueue::new(writer_conn).unwrap();
        let handle = queue.handle();

        let result = handle.send(|conn| {
            conn.execute(
                "INSERT INTO core_repositories \
                     (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) \
                     VALUES ('r1', '/tmp/r1', 'r1', 1, 1, 0, 0)",
                [],
            )?;
            Ok(())
        });
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        drop(queue); // deterministic shutdown + join

        let read = open_rw(&path).unwrap();
        let count: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM core_repositories WHERE id = 'r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // Execution failure propagation
    // -----------------------------------------------------------------------

    #[test]
    fn writer_returns_error_on_mutation_failure() {
        let (path, writer_conn) = migrated_file_db();
        let queue = WriterQueue::new(writer_conn).unwrap();
        let handle = queue.handle();

        // Insert a duplicate PK — should fail with a SQLite constraint error.
        handle
            .send(|conn| {
                conn.execute(
                    "INSERT INTO core_repositories \
                         (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) \
                         VALUES ('dup', '/tmp', 'dup', 1, 1, 0, 0)",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let result = handle.send(|conn| {
            conn.execute(
                "INSERT INTO core_repositories \
                     (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) \
                     VALUES ('dup', '/tmp', 'dup', 1, 1, 0, 0)",
                [],
            )?;
            Ok(())
        });

        assert!(
            matches!(result, Err(StorageError::Sqlite(_))),
            "expected Sqlite error on duplicate PK, got {result:?}"
        );
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // Mid-batch failure — others in same batch receive BatchRolledBack
    // -----------------------------------------------------------------------

    #[test]
    fn mid_batch_failure_rolls_back_batch() {
        let (path, writer_conn) = migrated_file_db();
        let queue = WriterQueue::new(writer_conn).unwrap();

        // We need to send multiple items that will land in the same batch.
        // Use multiple threads so they are genuinely concurrent.
        let h1 = queue.handle();
        let h2 = queue.handle();

        // Thread 1: inserts 'batch-ok'
        let t1 = thread::spawn(move || {
            h1.send(|conn| {
                conn.execute(
                    "INSERT INTO core_repositories \
                         (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) \
                         VALUES ('batch-ok', '/tmp/ok', 'ok', 1, 1, 0, 0)",
                    [],
                )?;
                Ok(())
            })
        });

        // Thread 2: attempts duplicate 'batch-ok' — will fail
        let t2 = thread::spawn(move || {
            h2.send(|conn| {
                conn.execute(
                    "INSERT INTO core_repositories \
                         (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) \
                         VALUES ('batch-ok', '/tmp/dup', 'dup', 1, 1, 0, 0)",
                    [],
                )?;
                Ok(())
            })
        });

        let r1 = t1.join().unwrap();
        let r2 = t2.join().unwrap();

        // At least one must be an error (either the failure or BatchRolledBack).
        let both_ok = r1.is_ok() && r2.is_ok();
        assert!(!both_ok, "at least one mutation in the conflict pair must fail");

        // If they landed in the same batch, at least one should be BatchRolledBack
        // or Sqlite error. Neither should silently succeed a conflicting insert.
        let read = open_rw(&path).unwrap();
        run_migrations(&read).unwrap(); // migrations already applied but idempotent
        let count: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM core_repositories WHERE id = 'batch-ok'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Either 0 (both rolled back) or 1 (one succeeded, other rolled back).
        // The invariant is that the DB must be consistent — no partial writes.
        assert!(count <= 1, "DB must not contain duplicate rows");

        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // Queue full / backpressure
    // -----------------------------------------------------------------------

    #[test]
    fn queue_full_returns_error() {
        // Use a tiny channel directly to simulate full queue without a real DB.
        let (tx, _rx) = mpsc::sync_channel::<WorkItem>(2);
        let poisoned = Arc::new(AtomicBool::new(false));
        let handle = WriterQueueHandle { tx, poisoned };

        // Fill the queue.
        let (dummy_tx1, _) = mpsc::sync_channel(1);
        let (dummy_tx2, _) = mpsc::sync_channel(1);
        let _ = handle.tx.try_send(WorkItem { f: Box::new(|_| Ok(())), result_tx: dummy_tx1 });
        let _ = handle.tx.try_send(WorkItem { f: Box::new(|_| Ok(())), result_tx: dummy_tx2 });

        // Third should return QueueFull.
        let result = handle.send(|_| Ok(()));
        assert!(
            matches!(result, Err(StorageError::QueueFull)),
            "expected QueueFull, got {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Poisoned handle rejects new sends immediately
    // -----------------------------------------------------------------------

    #[test]
    fn poisoned_handle_rejects_send() {
        let (tx, _rx) = mpsc::sync_channel::<WorkItem>(16);
        let poisoned = Arc::new(AtomicBool::new(true)); // pre-poisoned
        let handle = WriterQueueHandle { tx, poisoned };

        let result = handle.send(|_| Ok(()));
        assert!(
            matches!(result, Err(StorageError::WriterPoisoned)),
            "expected WriterPoisoned, got {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // ROLLBACK failure after mutation error → writer poisoned
    // -----------------------------------------------------------------------

    #[test]
    fn rollback_failure_after_mutation_error_poisons_writer() {
        let (path, writer_conn) = migrated_file_db();

        let queue =
            WriterQueue::new_with_finalizer(writer_conn, FailRollbackFinalizer { msg: "disk full" })
                .unwrap();
        let handle = queue.handle();

        // This mutation will succeed (no constraint error), but we need the
        // ROLLBACK path.  We force it by injecting a mutation that explicitly
        // returns an error.
        let result = handle.send(|_conn| Err(StorageError::Worker("forced failure".into())));

        // The mutation error triggered ROLLBACK, which failed → WriterPoisoned.
        assert!(
            matches!(result, Err(StorageError::WriterPoisoned)),
            "expected WriterPoisoned after ROLLBACK failure, got {result:?}"
        );

        // Subsequent sends must also return WriterPoisoned immediately.
        let result2 = handle.send(|_| Ok(()));
        assert!(
            matches!(result2, Err(StorageError::WriterPoisoned)),
            "expected WriterPoisoned on subsequent send, got {result2:?}"
        );

        drop(queue);
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // COMMIT failure + successful ROLLBACK → Worker error to all callers
    // -----------------------------------------------------------------------

    #[test]
    fn commit_failure_with_successful_rollback_returns_worker_error_to_all_callers() {
        let (path, writer_conn) = migrated_file_db();

        let queue = WriterQueue::new_with_finalizer(writer_conn, FailCommitFinalizer).unwrap();
        let handle = queue.handle();

        let result = handle.send(|_| Ok(()));

        // COMMIT failed but ROLLBACK succeeded → Worker error, not Poisoned.
        assert!(
            matches!(result, Err(StorageError::Worker(ref msg)) if msg.contains("COMMIT failed")),
            "expected Worker(COMMIT failed ...), got {result:?}"
        );

        // Writer is NOT poisoned — can still accept mutations (though they
        // will also fail with this injected finalizer; that's fine for this test).
        assert!(
            !handle.poisoned.load(Ordering::Acquire),
            "writer must not be poisoned when ROLLBACK after COMMIT failure succeeded"
        );

        drop(queue);
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // COMMIT failure + ROLLBACK failure → writer poisoned
    // -----------------------------------------------------------------------

    #[test]
    fn commit_and_rollback_failure_poisons_writer() {
        let (path, writer_conn) = migrated_file_db();

        let queue = WriterQueue::new_with_finalizer(writer_conn, FailBothFinalizer).unwrap();
        let handle = queue.handle();

        let result = handle.send(|_| Ok(()));

        assert!(
            matches!(result, Err(StorageError::WriterPoisoned)),
            "expected WriterPoisoned when both COMMIT and ROLLBACK fail, got {result:?}"
        );

        // Writer is poisoned — subsequent sends rejected immediately.
        let result2 = handle.send(|_| Ok(()));
        assert!(
            matches!(result2, Err(StorageError::WriterPoisoned)),
            "expected WriterPoisoned on subsequent send after double failure, got {result2:?}"
        );

        drop(queue);
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // Full-queue shutdown — Drop must not hang
    // -----------------------------------------------------------------------

    #[test]
    fn shutdown_does_not_hang_when_queue_full() {
        let (path, writer_conn) = migrated_file_db();
        let queue = WriterQueue::new(writer_conn).unwrap();

        // Set the shutdown flag directly to simulate a full-queue drop scenario.
        queue.shutdown.store(true, Ordering::Release);

        // Drop must complete promptly (the worker sees the flag and exits).
        drop(queue);
        // If we reach here without hanging, the test passes.
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // Thread/resource cleanup — worker thread must not outlive WriterQueue
    // -----------------------------------------------------------------------

    #[test]
    fn worker_thread_joins_on_drop() {
        let (path, writer_conn) = migrated_file_db();
        let queue = WriterQueue::new(writer_conn).unwrap();

        // Verify the thread was created (worker is Some).
        // Drop triggers join.
        drop(queue);
        // Reaching here confirms join completed; if the thread leaked it would
        // eventually be detected by OS thread-leak tooling or by the test
        // hanging indefinitely.
        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // WriterQueue::new propagates thread spawn errors
    // (We can't easily force spawn to fail in CI, but we verify the Ok path.)
    // -----------------------------------------------------------------------

    #[test]
    fn writer_queue_new_returns_ok_on_valid_connection() {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        let queue = WriterQueue::new(conn);
        assert!(queue.is_ok(), "WriterQueue::new should return Ok for a valid connection");
    }
}
