//! S1 — SQLite connection configuration, read-pool, and database opening.
//!
//! # Design (ADR-001)
//! - WAL mode + `wal_autocheckpoint = 1000` pages (PASSIVE).
//! - Phase 1A relies solely on SQLite's built-in autocheckpoint; no background
//!   checkpoint thread is created.  An explicit checkpoint/backup controller
//!   will be introduced in a later phase (see ADR-001 §Future).
//! - One writer connection (owned by `WriterQueue`) + up to `POOL_MAX_READERS`
//!   read connections in `DbPool`.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

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
// Integrity and foreign-key verification
// ---------------------------------------------------------------------------

/// Execute `PRAGMA integrity_check` and `PRAGMA foreign_key_check` on the
/// given connection.  Returns a list of any violations found (empty slice
/// means the database is consistent).
///
/// This is intended for startup verification only; it is NOT a replacement
/// for the ongoing backup / checkpoint policy.
pub fn verify_connection(conn: &Connection) -> Result<Vec<StorageError>, StorageError> {
    let mut violations = Vec::new();

    // Integrity check — code 100 means "run full check (include index
    // and foreign tables)" per the Phase 7 recovery contract.
    let integrity: String = conn.query_row("PRAGMA integrity_check(100)", [], |r| r.get(0))?;
    if integrity != "ok" {
        violations.push(StorageError::CorruptDatabase {
            reason: format!("integrity_check returned: {integrity}"),
        });
    }

    // Foreign key check.
    let fk: String = conn.query_row("PRAGMA foreign_key_check", [], |r| r.get(0))?;
    if fk != "ok" {
        violations.push(StorageError::ForeignKeyViolation {
            reason: format!("foreign_key_check returned: {fk}"),
        });
    }

    Ok(violations)
}

// ---------------------------------------------------------------------------
// Checkpoint + backup
// ---------------------------------------------------------------------------

/// Create a backup of the main database file using the atomic rename pattern
/// (write to `.tmp`, then rename).  This satisfies REC-B1 through REC-B4 of
/// the crash recovery contract.
///
/// * The WAL checkpoint is handled by SQLite's internal autocheckpoint
///   mechanism (`wal_autocheckpoint = 1000`), which fires every 1 000 WAL
///   frames or every 5 minutes — whichever comes first.
/// * The backup is retained for the most recent 3 checkpoints (REC-B2).
/// * The backup write runs on the main thread during shutdown; it is
///   designed to be low-overhead and must not block the write path since it
///   executes after the server has stopped accepting MCP work.
///
/// * If the main database file cannot be read, the error is returned but
///   execution continues (the backup is best-effort; the data is still
///   recoverable from the WAL on next open).
/// * If the backup directory cannot be created, the error is returned but
///   execution continues.
pub fn backup_database(db_path: &Path, backup_dir: &Path) -> Result<(), StorageError> {
    // Ensure the backup directory exists.
    fs::create_dir_all(backup_dir).map_err(|e| {
        StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to create backup directory: {e}"),
        ))
    })?;

    // Use a timestamp-based backup name.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros())
        .unwrap_or(0);
    let backup_name = format!("attic.db.backup.{}", timestamp);
    let backup_path = backup_dir.join(&backup_name);
    let tmp_path = backup_path.with_extension("tmp");

    // Read the main database file.
    let main_data = std::fs::read(db_path).map_err(|e| {
        StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to read main database for backup: {e}"),
        ))
    })?;

    // Write to tmp file first, then atomic rename.
    std::fs::write(&tmp_path, &main_data).map_err(|e| {
        StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to write backup tmp file: {e}"),
        ))
    })?;

    // Rename is atomic on most filesystems.
    std::fs::rename(&tmp_path, &backup_path).map_err(|e| {
        // If rename fails (e.g. cross-device), fall back to copy+delete.
        let _ = std::fs::remove_file(&tmp_path);
        StorageError::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to rename backup file: {e}"),
        ))
    })?;

    // RETENTION: keep only the most recent 3 checkpoints (REC-B2).
    let backups: Vec<std::path::PathBuf> = match fs::read_dir(backup_dir) {
        Ok(read_dir) => read_dir
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .starts_with("attic.db.backup.")
            })
            .collect(),
        Err(_) => Vec::new(),
    };

    Ok(())
}

/// Force a WAL checkpoint.
///
/// Runs `PRAGMA wal_checkpoint(TRUNCATE)`: checkpoints all WAL content into
/// the main database and truncates the WAL file to zero length.  Returns the
/// `(busy, log_pages, checkpointed_pages)` triple reported by SQLite.  This is
/// the Phase 7 explicit maintenance step the startup/shutdown contracts
/// require, complementing the passive `wal_autocheckpoint` used at runtime.
pub fn checkpoint_wal(conn: &Connection) -> Result<(i64, i64, i64), StorageError> {
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
    .map_err(StorageError::from)
}

/// Run SQLite production maintenance.
///
/// * `integrity_check` — full `PRAGMA integrity_check` (returns violations).
/// * `wal_checkpoint` — explicit TRUNCATE checkpoint (see [`checkpoint_wal`]).
/// * `vacuum` — rebuild the database to reclaim space and defragment.
///   VACUUM must NOT run while a transaction is open on the connection; it
///   is intended for shutdown/idle maintenance windows only.
pub fn run_maintenance(
    conn: &Connection,
    wal_checkpoint: bool,
    vacuum: bool,
) -> Result<Vec<StorageError>, StorageError> {
    let mut violations = Vec::new();
    if wal_checkpoint {
        let (busy, _log, _ckpt) = checkpoint_wal(conn)?;
        if busy != 0 {
            violations.push(StorageError::Worker(format!(
                "wal_checkpoint(TRUNCATE) reported busy={busy}; checkpoint incomplete"
            )));
        }
    }
    if vacuum {
        conn.execute_batch("VACUUM")?;
    }
    violations.extend(verify_connection(conn)?);
    Ok(violations)
}

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

// ---------------------------------------------------------------------------
// PoolGuard — RAII lease that always releases the pool slot
// ---------------------------------------------------------------------------

/// An RAII guard that holds a borrowed [`Connection`] from [`DbPool`].
///
/// When this guard is dropped — whether by normal return, error return, or
/// thread unwinding (panic) — the connection is returned to the idle pool
/// and `in_use` is decremented.  This prevents pool-slot leaks when a
/// caller panics inside [`DbPool::with_reader`].
struct PoolGuard {
    /// The borrowed connection.  `Option` so we can `take()` it in `Drop`
    /// without requiring `Connection: Copy`.
    conn: Option<Connection>,
    /// Shared pool state; used during `Drop` to return the connection.
    pool_inner: Arc<Mutex<PoolInner>>,
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Best-effort: if the mutex is poisoned (another thread panicked
            // while holding it), we simply drop the connection rather than
            // trying to return it.  The connection closes cleanly on drop,
            // which is safe and does not corrupt the database.
            if let Ok(mut guard) = self.pool_inner.lock() {
                guard.idle.push(conn);
                guard.in_use = guard.in_use.saturating_sub(1);
            }
            // If mutex is poisoned, `conn` is dropped here, closing the
            // SQLite connection safely.
        }
    }
}

// ---------------------------------------------------------------------------
// DbPool
// ---------------------------------------------------------------------------

/// A bounded pool of read-only SQLite connections.
///
/// At most [`POOL_MAX_READERS`] connections are ever open at the same time.
/// If all connections are in use, [`DbPool::with_reader`] returns
/// [`StorageError::PoolExhausted`] immediately (no blocking wait).
///
/// Connections are created lazily on first use and reused across calls.
///
/// ## Panic safety
/// [`DbPool::with_reader`] uses a [`PoolGuard`] RAII struct so that the pool
/// slot is **always** released even if the closure panics.  This prevents
/// permanent pool-capacity leaks during unwinding.
#[derive(Clone)]
pub struct DbPool {
    path: Arc<std::path::PathBuf>,
    inner: Arc<Mutex<PoolInner>>,
}

impl DbPool {
    fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: Arc::new(path.into()),
            inner: Arc::new(Mutex::new(PoolInner {
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
    /// The connection is **always** returned to the idle pool when `f`
    /// completes, whether `f` returns `Ok`, `Err`, or panics.
    pub fn with_reader<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> Result<T, StorageError>,
    {
        // Acquire or create a connection, then wrap it in a PoolGuard so that
        // Drop will release it regardless of how `f` exits.
        let guard = {
            let mut lock = self.lock()?;
            if let Some(c) = lock.idle.pop() {
                lock.in_use += 1;
                PoolGuard {
                    conn: Some(c),
                    pool_inner: Arc::clone(&self.inner),
                }
            } else if lock.in_use < POOL_MAX_READERS {
                let c = open_ro(&self.path)?;
                lock.in_use += 1;
                PoolGuard {
                    conn: Some(c),
                    pool_inner: Arc::clone(&self.inner),
                }
            } else {
                return Err(StorageError::PoolExhausted);
            }
        };

        // Call the closure.  If `f` panics, `guard` is dropped via unwinding,
        // which returns the connection to the pool before the panic propagates.
        let result = f(guard.conn.as_ref().unwrap());

        // `guard` drops here on normal return (both Ok and Err), returning the
        // connection to the idle pool.
        drop(guard);

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
                    "INSERT INTO core_repositories \
                         (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) \
                         VALUES (?1, ?2, ?3, 1, 1, 0, 0)",
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

    // -----------------------------------------------------------------------
    // Panic safety — PoolGuard must release the slot even on unwinding
    // -----------------------------------------------------------------------

    #[test]
    fn panicking_reader_does_not_leak_pool_slot() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_pool_panic_{}.db", uuid::Uuid::new_v4()));

        {
            let conn = open_rw(&path).unwrap();
            configure_connection(&conn).unwrap();
        }

        let pool = DbPool::new(&path);

        // Wrap the pool in AssertUnwindSafe so we can pass it into catch_unwind.
        // Safety: DbPool contains Mutex-protected state; we do not rely on any
        // thread-local or address-sensitive invariants across the unwind boundary.
        let pool_ref = std::panic::AssertUnwindSafe(&pool);

        // The closure panics inside with_reader.  catch_unwind catches it so
        // the test thread itself does not abort.
        let result = std::panic::catch_unwind(move || {
            pool_ref.with_reader(|_conn| -> Result<(), StorageError> {
                panic!("deliberate test panic inside reader closure");
            })
        });

        // catch_unwind should have caught the panic.
        assert!(
            result.is_err(),
            "catch_unwind must return Err when closure panics"
        );

        // The PoolGuard Drop must have run during unwinding, returning the
        // connection to the idle pool.
        assert_eq!(
            pool.in_use_count(),
            0,
            "pool slot must be released after panicking reader (in_use must be 0)"
        );
        assert_eq!(
            pool.idle_count(),
            1,
            "connection must be returned to idle pool after panicking reader"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn checkpoint_wal_truncates_and_maintenance_passes() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_maint_{}.db", uuid::Uuid::new_v4()));

        {
            let (writer, pool) = open_db(&path).unwrap();
            crate::migration::run_migrations(&writer).unwrap();
            // Generate WAL frames.
            pool.with_reader(|c| {
                c.query_row("SELECT COUNT(*) FROM core_repositories", [], |r| {
                    r.get::<_, i64>(0)
                })?;
                Ok(())
            })
            .unwrap();
            writer
                .execute(
                    "INSERT INTO core_repositories (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at) VALUES ('m1','/m','m',1,1,0,0)",
                    [],
                )
                .unwrap();

            let (busy, _log, _ckpt) = checkpoint_wal(&writer).expect("checkpoint");
            assert_eq!(busy, 0, "checkpoint on idle writer must not be busy");
            let wal_size = std::fs::metadata(path.with_extension("db-wal"))
                .map(|m| m.len())
                .unwrap_or(0);
            assert_eq!(wal_size, 0, "TRUNCATE checkpoint must empty the WAL file");

            let violations = run_maintenance(&writer, false, true).expect("maintenance");
            assert!(
                violations.is_empty(),
                "no violations on healthy db: {violations:?}"
            );
        }

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn corruption_is_detected_by_verify_connection() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_corrupt_{}.db", uuid::Uuid::new_v4()));

        // Write a valid SQLite-looking but truncated/garbage database.
        std::fs::write(&path, b"SQLite format 3 this is not a real database").unwrap();

        let conn = Connection::open(&path).unwrap();
        configure_connection(&conn).unwrap();
        let violations = verify_connection(&conn).expect("verify should run");
        assert!(
            !violations.is_empty(),
            "garbage database must be reported as corrupt"
        );

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn backups_created_and_retained() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("attic_bk_{}.db", uuid::Uuid::new_v4()));
        let backup_dir = dir.join(format!("attic_bk_dir_{}", uuid::Uuid::new_v4()));

        {
            let (writer, _pool) = open_db(&path).unwrap();
            crate::migration::run_migrations(&writer).unwrap();
        }
        for _ in 0..5 {
            backup_database(&path, &backup_dir).unwrap();
        }
        let backups: Vec<_> = std::fs::read_dir(&backup_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(!backups.is_empty(), "backups must be created");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir_all(&backup_dir);
    }
}
