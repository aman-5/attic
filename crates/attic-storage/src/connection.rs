//! S1 — SQLite connection configuration, read-pool, and WAL checkpoint management.
//!
//! # Design (ADR-001)
//! - WAL mode + `wal_autocheckpoint = 1000` pages (passive)
//! - A background thread fires a PASSIVE checkpoint every 5 minutes
//! - One writer connection (owned by `WriterQueue`) + N read connections in `DbPool`

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use tracing::{debug, warn};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// PRAGMA configuration
// ---------------------------------------------------------------------------

/// Apply all required PRAGMAs to a freshly opened connection.
///
/// Must be called on **every** connection (writer and readers alike).
pub fn configure_connection(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "
        PRAGMA journal_mode      = WAL;
        PRAGMA wal_autocheckpoint = 1000;
        PRAGMA synchronous       = NORMAL;
        PRAGMA foreign_keys      = ON;
        PRAGMA busy_timeout      = 5000;
        PRAGMA cache_size        = -32768;
        PRAGMA temp_store        = MEMORY;
        PRAGMA mmap_size         = 536870912;
        ",
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

/// Open a read-write connection at `path` and apply PRAGMAs.
pub fn open_rw(path: &Path) -> Result<Connection, StorageError> {
    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    Ok(conn)
}

/// Open a read-only connection at `path` and apply PRAGMAs.
pub fn open_ro(path: &Path) -> Result<Connection, StorageError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    configure_connection(&conn)?;
    Ok(conn)
}

// ---------------------------------------------------------------------------
// DbPool — shared read-connection pool
// ---------------------------------------------------------------------------

/// A simple pool of read-only SQLite connections.
///
/// Connections are created lazily up to the configured pool size and returned
/// to the pool after use.  For Phase 1A a single-element pool is sufficient.
#[derive(Clone)]
pub struct DbPool {
    path: Arc<std::path::PathBuf>,
    inner: Arc<Mutex<Vec<Connection>>>,
}

impl DbPool {
    fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: Arc::new(path.into()),
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Execute a closure with a read-only connection borrowed from the pool.
    ///
    /// The connection is returned to the pool when `f` completes.
    pub fn with_reader<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> Result<T, StorageError>,
    {
        // Try to pop an existing connection; open a new one if the pool is empty.
        let conn = {
            let mut guard = self.inner.lock().expect("DbPool mutex poisoned");
            guard.pop()
        };

        let conn = match conn {
            Some(c) => c,
            None => open_ro(&self.path)?,
        };

        let result = f(&conn);

        // Return the connection to the pool (even on error).
        {
            let mut guard = self.inner.lock().expect("DbPool mutex poisoned");
            guard.push(conn);
        }

        result
    }
}

// ---------------------------------------------------------------------------
// open_db — primary entry point
// ---------------------------------------------------------------------------

/// Open (or create) the Attic database at `path`.
///
/// Returns the **writer** connection plus a [`DbPool`] for reads.
/// Also spawns a background thread that runs a PASSIVE WAL checkpoint every
/// 5 minutes (ADR-001).
pub fn open_db(path: impl AsRef<Path>) -> Result<(Connection, DbPool), StorageError> {
    let path = path.as_ref().to_path_buf();

    let writer = open_rw(&path)?;
    let pool = DbPool::new(path.clone());

    // Spawn background WAL checkpoint thread (ADR-001).
    let checkpoint_path = path.clone();
    thread::Builder::new()
        .name("attic-wal-checkpoint".into())
        .spawn(move || {
            loop {
                thread::sleep(Duration::from_secs(300)); // 5 minutes
                match open_rw(&checkpoint_path) {
                    Ok(conn) => {
                        match conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);") {
                            Ok(_) => debug!("WAL checkpoint (PASSIVE) completed"),
                            Err(e) => warn!("WAL checkpoint error: {e}"),
                        }
                    }
                    Err(e) => warn!("WAL checkpoint: could not open connection: {e}"),
                }
            }
        })
        .expect("failed to spawn WAL checkpoint thread");

    Ok((writer, pool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_connection_configures_without_error() {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).expect("configure should succeed");
    }

    #[test]
    fn pool_with_reader_returns_value() {
        // Use a temp file so open_ro can work (in-memory doesn't support read-only flags).
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_test_{}.db", uuid::Uuid::new_v4()));
        // Create the file first via rw open.
        let conn = open_rw(&path).unwrap();
        configure_connection(&conn).unwrap();
        drop(conn);

        let pool = DbPool::new(&path);
        let result = pool.with_reader(|c| {
            let v: i64 = c.query_row("SELECT 42", [], |r| r.get(0))?;
            Ok(v)
        });
        assert_eq!(result.unwrap(), 42);

        // Cleanup
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
