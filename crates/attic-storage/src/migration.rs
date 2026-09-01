//! S2 — Idempotent schema migration runner.
//!
//! Migrations are embedded at compile time and applied in order.  Each
//! migration SQL file is responsible for recording its own entry in
//! `core_schema_migrations`; this runner only guards against re-applying an
//! already-applied migration.

use rusqlite::Connection;
use tracing::info;

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Embedded migrations
// ---------------------------------------------------------------------------

const MIGRATION_0001: &str = include_str!("../../../migrations/0001_initial.sql");
const VERSION_0001: &str = "0001_initial";

const MIGRATION_0002: &str = include_str!("../../../migrations/0002_phase1d.sql");
const VERSION_0002: &str = "0002_phase1d";

const MIGRATION_0003: &str = include_str!("../../../migrations/0003_phase2.sql");
const VERSION_0003: &str = "0003_phase2";

const MIGRATION_0004: &str = include_str!("../../../migrations/0004_phase6.sql");
const VERSION_0004: &str = "0004_phase6";

const MIGRATION_0005: &str = include_str!("../../../migrations/0005_workspace_snapshot.sql");
const VERSION_0005: &str = "0005_workspace_snapshot";

const MIGRATION_0006: &str = include_str!("../../../migrations/0006_index_analysis_cache.sql");
const VERSION_0006: &str = "0006_index_analysis_cache";

const MIGRATION_0007: &str = include_str!("../../../migrations/0007_analysis_cache_versioning.sql");
const VERSION_0007: &str = "0007_analysis_cache_versioning";

/// Every migration version this binary knows how to apply/reason about, in
/// order. Used to detect a downgrade (an older binary opening a database a
/// newer binary already migrated further) — see `run_migrations`.
const KNOWN_VERSIONS: &[&str] = &[
    VERSION_0001,
    VERSION_0002,
    VERSION_0003,
    VERSION_0004,
    VERSION_0005,
    VERSION_0006,
    VERSION_0007,
];

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------

/// Apply all pending migrations to `conn`.
///
/// Safe to call on every startup — already-applied migrations are skipped.
pub fn run_migrations(conn: &Connection) -> Result<(), StorageError> {
    // Bootstrap the tracking table.  Uses `id` as the primary key column to
    // match the schema established by 0001_initial.sql.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS core_schema_migrations (
            id          TEXT    PRIMARY KEY NOT NULL,
            applied_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000000)
        );",
    )?;

    // Fail closed on downgrade: if the database already records a migration
    // this binary doesn't recognize, a newer binary has already advanced the
    // schema past what this build understands. Silently proceeding would
    // "successfully" run zero migrations and then serve against a schema
    // this code cannot reason about — refuse instead.
    let mut stmt = conn.prepare("SELECT id FROM core_schema_migrations")?;
    let applied_ids: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in &applied_ids {
        if !KNOWN_VERSIONS.contains(&id.as_str()) {
            return Err(StorageError::Migration {
                message: format!(
                    "database has migration '{id}' applied, which this binary does not \
                     recognize (known: {KNOWN_VERSIONS:?}) — this binary is older than \
                     the database schema; refusing to serve a schema state it cannot verify"
                ),
            });
        }
    }

    apply_migration(conn, VERSION_0001, MIGRATION_0001)?;
    apply_migration(conn, VERSION_0002, MIGRATION_0002)?;
    apply_migration(conn, VERSION_0003, MIGRATION_0003)?;
    apply_migration(conn, VERSION_0004, MIGRATION_0004)?;
    apply_migration(conn, VERSION_0005, MIGRATION_0005)?;
    apply_migration(conn, VERSION_0006, MIGRATION_0006)?;
    apply_migration(conn, VERSION_0007, MIGRATION_0007)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Apply a single migration if it has not already been recorded.
///
/// The migration SQL is executed inside `BEGIN IMMEDIATE … COMMIT`.  On
/// failure the transaction is rolled back and a [`StorageError::Migration`]
/// is returned.  Each migration SQL file contains its own
/// `INSERT OR IGNORE INTO core_schema_migrations` so there is no need to
/// insert here.
fn apply_migration(conn: &Connection, version: &str, sql: &str) -> Result<(), StorageError> {
    let already_applied: bool = conn.query_row(
        "SELECT COUNT(*) FROM core_schema_migrations WHERE id = ?1",
        rusqlite::params![version],
        |row| row.get::<_, i64>(0),
    )? > 0;

    if already_applied {
        info!("migration {version} already applied — skipping");
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE;")?;

    match conn.execute_batch(sql) {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            info!("migration {version} applied");
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(StorageError::Migration {
                message: format!("migration {version} failed and was rolled back: {e}"),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
            .query_row("SELECT COUNT(*) FROM core_schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            count as usize,
            KNOWN_VERSIONS.len(),
            "one row per known migration expected after first run"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = in_memory_conn();
        run_migrations(&conn).expect("first run");
        run_migrations(&conn).expect("second run should be a no-op");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM core_schema_migrations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            count as usize,
            KNOWN_VERSIONS.len(),
            "still exactly one row per known migration after second run"
        );
    }

    #[test]
    fn unrecognized_future_migration_is_rejected() {
        // Simulate a newer binary having already migrated this database
        // further than the current binary knows how to.
        let conn = in_memory_conn();
        run_migrations(&conn).expect("first run");
        conn.execute(
            "INSERT INTO core_schema_migrations (id) VALUES (?1)",
            rusqlite::params!["0099_future_migration"],
        )
        .unwrap();

        let err = run_migrations(&conn).expect_err("downgrade must be rejected");
        assert!(
            matches!(err, StorageError::Migration { .. }),
            "expected a Migration error, got {err:?}"
        );
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
            "core_retrieval_units",
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

    #[test]
    fn phase1d_columns_exist_after_migration() {
        let conn = in_memory_conn();
        run_migrations(&conn).unwrap();

        // Use PRAGMA table_info to check that the Phase 1D columns were added.
        let mut stmt = conn
            .prepare("PRAGMA table_info(core_retrieval_units)")
            .unwrap();

        let columns: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        for col in &[
            "analyzer_id",
            "analyzer_version",
            "start_line",
            "end_line",
            "is_redacted",
        ] {
            assert!(
                columns.contains(&col.to_string()),
                "column '{col}' should exist in core_retrieval_units after phase1d migration"
            );
        }
    }
}
