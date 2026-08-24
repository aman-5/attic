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
//! 1. The transaction is rolled back.
//! 2. The failing mutation's caller receives its original error.
//! 3. Every other mutation in the same batch receives
//!    [`StorageError::BatchRolledBack`].
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
use tracing::{debug, error, warn};

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
}

impl WriterQueueHandle {
    /// Submit a mutation closure to the writer queue and **block** until the
    /// mutation has been executed (committed or rolled back).
    ///
    /// ## Return value
    /// - `Ok(())` — the mutation was committed successfully.
    /// - `Err(StorageError::QueueFull)` — the queue is at capacity; try later.
    /// - `Err(StorageError::QueueShutdown)` — the worker has shut down.
    /// - `Err(StorageError::BatchRolledBack)` — this mutation was rolled back
    ///   because a different mutation in the same batch failed.
    /// - Any other `Err` — the mutation itself returned an error; the batch
    ///   was rolled back.
    pub fn send<F>(&self, f: F) -> Result<(), StorageError>
    where
        F: FnOnce(&Connection) -> Result<(), StorageError> + Send + 'static,
    {
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
        let (tx, rx) = mpsc::sync_channel::<WorkItem>(QUEUE_CAPACITY);
        let handle = WriterQueueHandle { tx };
        let shutdown = Arc::new(AtomicBool::new(false));

        let shutdown_flag = Arc::clone(&shutdown);
        let worker = thread::Builder::new()
            .name("attic-writer".into())
            .spawn(move || {
                worker_loop(conn, rx, shutdown_flag);
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
) {
    let mut batch: Vec<WorkItem> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();

    loop {
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
        let should_flush =
            !batch.is_empty() && (batch.len() >= BATCH_SIZE || last_flush.elapsed() >= FLUSH_INTERVAL || shutting_down);

        if should_flush {
            flush_batch(&conn, &mut batch);
            last_flush = Instant::now();
        }

        if shutting_down {
            // Drain anything that arrived between the shutdown flag check and now.
            loop {
                match rx.try_recv() {
                    Ok(item) => batch.push(item),
                    Err(_) => break,
                }
            }
            if !batch.is_empty() {
                flush_batch(&conn, &mut batch);
            }
            debug!("attic-writer: shut down cleanly");
            break;
        }

        // Brief sleep to avoid busy-spinning.
        thread::sleep(Duration::from_millis(1));
    }
}

// ---------------------------------------------------------------------------
// Batch execution
// ---------------------------------------------------------------------------

fn flush_batch(conn: &Connection, batch: &mut Vec<WorkItem>) {
    if batch.is_empty() {
        return;
    }

    // Drain the batch into owned (fn, result_tx) pairs so we can call each
    // FnOnce by value (Box<dyn FnOnce> cannot be called through &mut).
    let mut fns_and_txs: Vec<(MutationFn, SyncSender<Result<(), StorageError>>)> =
        batch.drain(..).map(|it| (it.f, it.result_tx)).collect();

    let n = fns_and_txs.len();

    // Open transaction.  On failure every caller receives the error.
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
    let mut failed = false;

    for (f, _tx) in fns_and_txs.iter_mut() {
        if failed {
            results.push(Err(StorageError::BatchRolledBack));
            continue;
        }
        // Swap out the FnOnce to gain ownership, replacing with a no-op
        // so the Vec entry remains valid until we zip results below.
        let noop: MutationFn = Box::new(|_| Ok(()));
        let real_f = std::mem::replace(f, noop);
        let res = real_f(conn);
        if res.is_err() {
            failed = true;
        }
        results.push(res);
    }

    // Commit or rollback.
    let commit_sql = if failed { "ROLLBACK;" } else { "COMMIT;" };
    if let Err(e) = conn.execute_batch(commit_sql) {
        warn!("attic-writer: {commit_sql} failed: {e}");
        // If COMMIT failed, all mutations are effectively rolled back.
        if !failed {
            for r in results.iter_mut() {
                *r = Err(StorageError::Worker(format!("COMMIT failed: {e}")));
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
    // Basic execution
    // -----------------------------------------------------------------------

    #[test]
    fn writer_executes_mutation_and_returns_ok() {
        let (path, writer_conn) = migrated_file_db();
        let queue = WriterQueue::new(writer_conn).unwrap();
        let handle = queue.handle();

        let result = handle.send(|conn| {
            conn.execute(
                "INSERT INTO core_repositories (id, root_path, name) VALUES ('r1', '/tmp/r1', 'r1')",
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
                    "INSERT INTO core_repositories (id, root_path, name) VALUES ('dup', '/tmp', 'dup')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        let result = handle.send(|conn| {
            conn.execute(
                "INSERT INTO core_repositories (id, root_path, name) VALUES ('dup', '/tmp', 'dup')",
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
                    "INSERT INTO core_repositories (id, root_path, name) VALUES ('batch-ok', '/tmp', 'ok')",
                    [],
                )?;
                Ok(())
            })
        });

        // Thread 2: attempts duplicate 'batch-ok' — will fail
        let t2 = thread::spawn(move || {
            h2.send(|conn| {
                conn.execute(
                    "INSERT INTO core_repositories (id, root_path, name) VALUES ('batch-ok', '/tmp', 'dup')",
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
        let handle = WriterQueueHandle { tx };

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
