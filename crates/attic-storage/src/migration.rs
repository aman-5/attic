//! S2 — Idempotent schema migration runner.
//!
//! The single migration `0001_initial.sql` is embedded at compile time.
//! `run_migrations` is safe to call on every startup; it checks whether
//! the migration has already been applied before executing it.

use rusqlite::Connection;
use tracing::info;

use crate::error::StorageError;

/// The initial schema, embedded from `migrations/0001_initial.sql`.
const MIGRATION_0001: &str = include_str!("../../../migrations/0001_initial.sql");

/// The version tag recorded in `core_schema_migrations` after applying the initial migration.
const VERSION_0001: &str = "0001";

/// Apply all pending migrations to `conn`.
///
/// This function is **idempotent** — calling it multiple times on the same
/// database is safe and produces no side-effects after the first run.
pub fn run_migrations(conn: &Connection) -> Result<(), StorageError> {
    // Ensure the tracking table exists (bootstraps a brand-new database).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS core_schema_migrations (
            version     TEXT PRIMARY KEY NOT NULL,
            applied_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
        );",
    )?;

    // Check whether migration 0001 has already been applied.
    let already_applied: bool = conn.query_row(
        "SELECT COUNT(*) FROM core_schema_migrations WHERE version = ?1",
        rusqlite::params![VERSION_0001],
        |row| row.get::<_, i64>(0),
    )? > 0;

    if already_applied {
        info!("migration {VERSION_0001} already applied — skipping");
        return Ok(());
    }

    // Run the migration inside an explicit transaction so we can roll back on failure.
    conn.execute_batch("BEGIN IMMEDIATE;")?;

    let result = (|| -> Result<(), StorageError> {
        conn.execute_batch(MIGRATION_0001)?;

        // Record the migration in the tracking table.
        conn.execute(
            "INSERT INTO core_schema_migrations (version) VALUES (?1)",
            rusqlite::params![VERSION_0001],
        )?;

        // Record an entry in the ops migration log (created by the migration itself).
        conn.execute(
            "INSERT INTO ops_migration_log (migration_version, status, notes)
             VALUES (?1, 'applied', 'initial schema')",
            rusqlite::params![VERSION_0001],
        )?;

        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            info!("migration {VERSION_0001} applied successfully");
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(StorageError::Migration {
                message: format!("migration {VERSION_0001} failed and was rolled back: {e}"),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use rusqlite::Connection;

    fn in_memory_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        conn
    }

    #[test]
    fn migration_applies_on_empty_db() {
        let conn = in_memory_conn();
        run_migrations(&conn).expect("first run should succeed");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "exactly one migration row expected");
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = in_memory_conn();
        run_migrations(&conn).expect("first run");
        run_migrations(&conn).expect("second run should be a no-op");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_schema_migrations",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "still exactly one migration row after second run");
    }

    #[test]
    fn core_tables_exist_after_migration() {
        let conn = in_memory_conn();
        run_migrations(&conn).unwrap();

        for table in &[
            "core_repositories",
            "core_source_revisions",
            "core_index_generations",
            "core_file_identities",
            "core_file_occurrences",
            "ops_server_state",
            "ops_migration_log",
        ] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "table '{table}' should exist after migration");
        }
    }
}
