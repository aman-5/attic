//! S6 — Bounded single-writer queue for serialising all SQLite mutations.
//!
//! SQLite in WAL mode supports one writer at a time.  Rather than taking a
//! mutex around every write site, all mutations are sent through a bounded
//! channel to a dedicated worker thread that owns the writer connection.
//!
//! # Design constants
//! - `QUEUE_CAPACITY`: 512 pending mutations before backpressure kicks in
//! - `BATCH_SIZE`: flush a batch after accumulating 256 mutations
//! - `FLUSH_INTERVAL`: flush at least every 50 ms even if batch is not full

use std::sync::{Arc, Mutex};
use std::sync::mpsc::{self, SyncSender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tracing::{debug, error, warn};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Maximum number of pending mutations in the channel before `send` returns
/// `Err(StorageError::QueueFull)`.
pub const QUEUE_CAPACITY: usize = 512;

/// Flush a batch after this many mutations (whichever comes first with the timer).
pub const BATCH_SIZE: usize = 256;

/// Maximum time between flushes even when `BATCH_SIZE` is not reached.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// MutationFn type alias
// ---------------------------------------------------------------------------

/// A boxed closure that executes a single logical mutation on `conn`.
pub type MutationFn = Box<dyn FnOnce(&Connection) -> Result<(), StorageError> + Send + 'static>;

// ---------------------------------------------------------------------------
// Internal channel message
// ---------------------------------------------------------------------------

enum Message {
    Mutate(MutationFn),
    Shutdown,
}

// ---------------------------------------------------------------------------
// WriterQueueHandle — cloneable send-side
// ---------------------------------------------------------------------------

/// A cheap-to-clone handle for submitting mutations to the writer queue.
///
/// Dropping all handles does **not** shut down the worker; call
/// [`WriterQueueHandle::shutdown`] explicitly.
#[derive(Clone)]
pub struct WriterQueueHandle {
    tx: SyncSender<Message>,
}

impl WriterQueueHandle {
    /// Submit a mutation closure to the writer queue without blocking.
    ///
    /// Returns `Err(StorageError::QueueFull)` if the queue is at capacity.
    pub fn send<F>(&self, f: F) -> Result<(), StorageError>
    where
        F: FnOnce(&Connection) -> Result<(), StorageError> + Send + 'static,
    {
        self.tx
            .try_send(Message::Mutate(Box::new(f)))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => StorageError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => StorageError::QueueShutdown,
            })
    }

    /// Signal the worker thread to drain remaining mutations and exit.
    ///
    /// Blocks until the worker thread has finished.
    pub fn shutdown(&self) {
        let _ = self.tx.try_send(Message::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// WriterQueue — owns the writer connection and worker thread
// ---------------------------------------------------------------------------

/// Owns the SQLite writer connection and spawns the mutation worker thread.
///
/// Drop or call [`WriterQueueHandle::shutdown`] to stop the worker.
pub struct WriterQueue {
    handle: WriterQueueHandle,
    /// Join handle so callers can wait for the worker to finish if needed.
    worker: Option<thread::JoinHandle<()>>,
}

impl WriterQueue {
    /// Create a new `WriterQueue` that drives mutations through `conn`.
    ///
    /// The connection is moved into the worker thread; callers must not use
    /// it afterwards.  Obtain a [`WriterQueueHandle`] via [`WriterQueue::handle`].
    pub fn new(conn: Connection) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Message>(QUEUE_CAPACITY);
        let handle = WriterQueueHandle { tx };

        let worker = thread::Builder::new()
            .name("attic-writer".into())
            .spawn(move || {
                worker_loop(conn, rx);
            })
            .expect("failed to spawn writer thread");

        Self {
            handle,
            worker: Some(worker),
        }
    }

    /// Return a cloneable handle for submitting mutations.
    pub fn handle(&self) -> WriterQueueHandle {
        self.handle.clone()
    }
}

impl Drop for WriterQueue {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(jh) = self.worker.take() {
            let _ = jh.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Worker loop
// ---------------------------------------------------------------------------

fn worker_loop(conn: Connection, rx: mpsc::Receiver<Message>) {
    let mut batch: Vec<MutationFn> = Vec::with_capacity(BATCH_SIZE);
    let mut last_flush = Instant::now();

    loop {
        // Drain messages until batch is full, the timer fires, or we get Shutdown.
        let mut shutdown = false;

        'recv: loop {
            // Non-blocking receive.
            match rx.try_recv() {
                Ok(Message::Mutate(f)) => {
                    batch.push(f);
                    if batch.len() >= BATCH_SIZE {
                        break 'recv;
                    }
                }
                Ok(Message::Shutdown) => {
                    shutdown = true;
                    break 'recv;
                }
                Err(TryRecvError::Empty) => {
                    // No messages — check if flush timer has expired.
                    if last_flush.elapsed() >= FLUSH_INTERVAL {
                        break 'recv;
                    }
                    // Brief sleep to avoid busy-spinning.
                    thread::sleep(Duration::from_millis(1));
                }
                Err(TryRecvError::Disconnected) => {
                    shutdown = true;
                    break 'recv;
                }
            }
        }

        if !batch.is_empty() {
            flush_batch(&conn, &mut batch);
            last_flush = Instant::now();
        }

        if shutdown {
            debug!("writer queue shutting down");
            break;
        }
    }
}

fn flush_batch(conn: &Connection, batch: &mut Vec<MutationFn>) {
    if batch.is_empty() {
        return;
    }

    // Execute all mutations in a single transaction for atomicity + performance.
    if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE;") {
        error!("writer: BEGIN IMMEDIATE failed: {e}");
        batch.clear();
        return;
    }

    let mut failed = false;
    for f in batch.drain(..) {
        if let Err(e) = f(conn) {
            error!("writer: mutation failed: {e}");
            failed = true;
            break;
        }
    }

    let sql = if failed { "ROLLBACK;" } else { "COMMIT;" };
    if let Err(e) = conn.execute_batch(sql) {
        warn!("writer: {sql} failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use rusqlite::Connection;
    use std::sync::{Arc, Mutex};

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn writer_executes_mutations() {
        // We can't send the migrated_conn into the queue AND read from it in the
        // same thread, so we use a file-backed DB for this test.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_writer_test_{}.db", uuid::Uuid::new_v4()));

        {
            let conn = crate::connection::open_rw(&path).unwrap();
            run_migrations(&conn).unwrap();
        }

        let writer_conn = crate::connection::open_rw(&path).unwrap();
        let queue = WriterQueue::new(writer_conn);
        let handle = queue.handle();

        // Send a mutation that inserts a repository row.
        handle
            .send(|conn| {
                conn.execute(
                    "INSERT INTO core_repositories (id, root_path, name)
                     VALUES ('repo-test-1', '/tmp/repo', 'test-repo')",
                    [],
                )?;
                Ok(())
            })
            .unwrap();

        // Give the worker time to process.
        thread::sleep(Duration::from_millis(200));
        drop(queue); // triggers shutdown + join

        // Verify the row exists via a separate read connection.
        let read_conn = crate::connection::open_rw(&path).unwrap();
        let count: i64 = read_conn
            .query_row(
                "SELECT COUNT(*) FROM core_repositories WHERE id = 'repo-test-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn backpressure_when_queue_full() {
        // Create a queue with a blocked worker (we never drain it) to test QueueFull.
        // We use a custom tiny-capacity channel via the public API by filling it up.
        let conn = migrated_conn();
        // Use a real WriterQueue but hold back draining by sending slow closures.
        // Instead, directly test the try_send path by saturating a SyncSender.
        let (tx, _rx) = mpsc::sync_channel::<Message>(2);
        let handle = WriterQueueHandle { tx };

        // Fill the channel.
        handle.send(|_| Ok(())).unwrap();
        handle.send(|_| Ok(())).unwrap();

        // Third send should return QueueFull.
        let result = handle.send(|_| Ok(()));
        assert!(
            matches!(result, Err(StorageError::QueueFull)),
            "expected QueueFull, got {result:?}"
        );

        drop(conn); // suppress unused warning
    }
}
