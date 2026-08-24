//! S1 — SQLite connection configuration, read-pool, and database opening.
//!
//! # Design (ADR-001)
//! - WAL mode + `wal_autocheckpoint = 1000` pages (PASSIVE).
//! - Phase 1A relies solely on SQLite's built-in autocheckpoint; no background
//!   checkpoint thread is created.  An explicit checkpoint/backup controller
//!   will be introduced in a later phase (see ADR-001 §Future).
//! - One writer connection (owned by `WriterQueue`) + up to `POOL_MAX_READERS`
//!   read connections in `DbPool`.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Pool capacity
// ---------------------------------------------------------------------------

/// Maximum number of concurrent read connections the pool will open.
///
/// Attempting [`DbPool::with_reader`] while all connections are in use returns
/// [`StorageError::PoolExhausted`].
pub const POOL_MAX_READERS: usize = 8;

// ---------------------------------------------------------------------------
// PRAGMA configuration
// ---------------------------------------------------------------------------

/// Apply all required PRAGMAs to a freshly opened connection.
///
/// Must be called on **every** connection (writer and readers alike).
pub fn configure_connection(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "
        PRAGMA journal_mode       = WAL;
        PRAGMA wal_autocheckpoint = 1000;
        PRAGMA synchronous        = NORMAL;
        PRAGMA foreign_keys       = ON;
        PRAGMA busy_timeout       = 5000;
        PRAGMA cache_size         = -32768;
        PRAGMA temp_store         = MEMORY;
        PRAGMA mmap_size          = 536870912;
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
// DbPool — bounded read-connection pool
// ---------------------------------------------------------------------------

struct PoolInner {
    /// Idle connections ready to be borrowed.
    idle: Vec<Connection>,
    /// Number of connections currently checked out (in use).
    in_use: usize,
}

/// A bounded pool of read-only SQLite connections.
///
/// At most [`POOL_MAX_READERS`] connections are ever open at the same time.
/// If all connections are in use, [`DbPool::with_reader`] returns
/// [`StorageError::PoolExhausted`] immediately (no blocking wait).
///
/// Connections are created lazily on first use and reused across calls.
#[derive(Clone)]
pub struct DbPool {
    path: std::sync::Arc<std::path::PathBuf>,
    inner: std::sync::Arc<Mutex<PoolInner>>,
}

impl DbPool {
    fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: std::sync::Arc::new(path.into()),
            inner: std::sync::Arc::new(Mutex::new(PoolInner {
                idle: Vec::with_capacity(POOL_MAX_READERS),
                in_use: 0,
            })),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, PoolInner>, StorageError> {
        self.inner
            .lock()
            .map_err(|e| StorageError::MutexPoisoned(e.to_string()))
    }

    /// Execute a closure with a read-only connection borrowed from the pool.
    ///
    /// - If an idle connection is available, it is reused.
    /// - If none are idle but the pool is not at capacity, a new connection
    ///   is opened.
    /// - If the pool is at capacity and all connections are in use,
    ///   [`StorageError::PoolExhausted`] is returned immediately.
    ///
    /// The connection is always returned to the idle pool when `f` completes,
    /// regardless of whether `f` returns `Ok` or `Err`.
    pub fn with_reader<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> Result<T, StorageError>,
    {
        // Acquire or create a connection.
        let conn = {
            let mut guard = self.lock()?;
            if let Some(c) = guard.idle.pop() {
                guard.in_use += 1;
                c
            } else if guard.in_use < POOL_MAX_READERS {
                let c = open_ro(&self.path)?;
                guard.in_use += 1;
                c
            } else {
                return Err(StorageError::PoolExhausted);
            }
        };

        let result = f(&conn);

        // Return the connection to the idle pool.
        if let Ok(mut guard) = self.inner.lock() {
            guard.idle.push(conn);
            guard.in_use = guard.in_use.saturating_sub(1);
        }
        // If the mutex is poisoned during return, the connection is dropped
        // (closed), which is safe — it does not corrupt the database.

        result
    }

    /// Returns the number of connections currently in use (checked out).
    ///
    /// Primarily useful in tests.
    #[cfg(test)]
    pub fn in_use_count(&self) -> usize {
        self.inner.lock().map(|g| g.in_use).unwrap_or(0)
    }

    /// Returns the number of idle connections in the pool.
    ///
    /// Primarily useful in tests.
    #[cfg(test)]
    pub fn idle_count(&self) -> usize {
        self.inner.lock().map(|g| g.idle.len()).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// open_db — primary entry point
// ---------------------------------------------------------------------------

/// Open (or create) the Attic database at `path`.
///
/// Returns the **writer** connection plus a [`DbPool`] for reads.
///
/// Phase 1A relies solely on `wal_autocheckpoint = 1000` (PASSIVE) for
/// checkpoint management.  No background thread is spawned here.  An
/// explicit checkpoint/backup controller will be introduced in a later phase.
pub fn open_db(path: impl AsRef<Path>) -> Result<(Connection, DbPool), StorageError> {
    let path = path.as_ref().to_path_buf();
    let writer = open_rw(&path)?;
    let pool = DbPool::new(path);
    Ok((writer, pool))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_connection_configures_without_error() {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).expect("configure_connection should succeed on in-memory DB");
    }

    #[test]
    fn pool_with_reader_returns_value() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_pool_read_{}.db", uuid::Uuid::new_v4()));

        // Create and initialise the file via a rw connection.
        {
            let conn = open_rw(&path).unwrap();
            configure_connection(&conn).unwrap();
        }

        let pool = DbPool::new(&path);
        let result = pool.with_reader(|c| {
            let v: i64 = c.query_row("SELECT 42", [], |r| r.get(0))?;
            Ok(v)
        });
        assert_eq!(result.unwrap(), 42);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn pool_connection_returned_after_use() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_pool_return_{}.db", uuid::Uuid::new_v4()));

        {
            let conn = open_rw(&path).unwrap();
            configure_connection(&conn).unwrap();
        }

        let pool = DbPool::new(&path);

        assert_eq!(pool.idle_count(), 0);
        assert_eq!(pool.in_use_count(), 0);

        pool.with_reader(|_| Ok::<_, StorageError>(())).unwrap();

        // After use the connection should be back in the idle pool.
        assert_eq!(pool.idle_count(), 1);
        assert_eq!(pool.in_use_count(), 0);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn pool_exhausted_when_at_capacity() {
        // Build a pool at a tiny path; we will not actually open connections —
        // instead we manipulate in_use directly to simulate saturation.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_pool_exhaust_{}.db", uuid::Uuid::new_v4()));

        {
            let conn = open_rw(&path).unwrap();
            configure_connection(&conn).unwrap();
        }

        let pool = DbPool::new(&path);

        // Saturate the pool by marking all slots in-use directly.
        {
            let mut guard = pool.inner.lock().unwrap();
            guard.in_use = POOL_MAX_READERS;
        }

        let result = pool.with_reader(|_| Ok::<_, StorageError>(()));
        assert!(
            matches!(result, Err(StorageError::PoolExhausted)),
            "expected PoolExhausted, got {result:?}"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn open_db_returns_writer_and_pool() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_open_db_{}.db", uuid::Uuid::new_v4()));

        let (writer, _pool) = open_db(&path).expect("open_db should succeed");
        // Verify the writer connection is functional.
        let v: i64 = writer
            .query_row("SELECT 1", [], |r| r.get(0))
            .expect("writer connection should execute queries");
        assert_eq!(v, 1);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn wal_autocheckpoint_pragma_is_set() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_wal_pragma_{}.db", uuid::Uuid::new_v4()));

        let (writer, _pool) = open_db(&path).expect("open_db should succeed");
        let val: i64 = writer
            .query_row("PRAGMA wal_autocheckpoint", [], |r| r.get(0))
            .expect("wal_autocheckpoint pragma should be readable");
        assert_eq!(val, 1000, "wal_autocheckpoint should be 1000");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn db_reopen_preserves_data() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_reopen_{}.db", uuid::Uuid::new_v4()));

        // Write data.
        {
            let (writer, _pool) = open_db(&path).unwrap();
            crate::migration::run_migrations(&writer).unwrap();
            writer
                .execute(
                    "INSERT INTO core_repositories (id, root_path, name) VALUES (?1, ?2, ?3)",
                    rusqlite::params!["reopen-repo-1", "/tmp/reopen", "reopen-test"],
                )
                .unwrap();
        }

        // Reopen and verify.
        {
            let (writer, _pool) = open_db(&path).unwrap();
            let count: i64 = writer
                .query_row(
                    "SELECT COUNT(*) FROM core_repositories WHERE id = 'reopen-repo-1'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "data must survive DB close/reopen");
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }
}
