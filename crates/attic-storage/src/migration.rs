//! Canonical SQLite schema bootstrap.
//!
//! The pre-release development migration chain was squashed before QA. The
//! embedded baseline is the complete schema for a fresh `attic.db`; future
//! released schema changes must be added as new migrations.

use rusqlite::Connection;
use tracing::info;

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Embedded baseline schema
// ---------------------------------------------------------------------------

const MIGRATION_0001: &str = include_str!("../../../migrations/0001_initial.sql");
const VERSION_0001: &str = "0001_initial";
const KNOWN_VERSIONS: &[&str] = &[VERSION_0001];

/// Apply the pre-release QA baseline schema to `conn`.
///
/// Attic had not shipped when this baseline was frozen, so development-era
/// migrations were intentionally squashed. Fresh QA/release databases have a
/// single canonical schema version.
pub fn run_migrations(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS core_schema_migrations (
            id          TEXT    PRIMARY KEY NOT NULL,
            applied_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000000)
        );",
    )?;

    let mut stmt = conn.prepare("SELECT id FROM core_schema_migrations")?;
    let applied_ids: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    for id in &applied_ids {
        if !KNOWN_VERSIONS.contains(&id.as_str()) {
            return Err(StorageError::Migration {
                message: format!(
                    "database schema version '{id}' is not supported by this pre-release baseline; \
                     start QA with a fresh database (known: {KNOWN_VERSIONS:?})"
                ),
            });
        }
    }

    apply_migration(conn, VERSION_0001, MIGRATION_0001)?;
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
    fn pre_qa_development_database_is_rejected() {
        let conn = in_memory_conn();
        conn.execute_batch(
            "CREATE TABLE core_schema_migrations (
                id TEXT PRIMARY KEY NOT NULL,
                applied_at INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO core_schema_migrations (id) VALUES ('0007_analysis_cache_versioning');",
        )
        .unwrap();

        let err = run_migrations(&conn).expect_err("pre-QA development DB must require reset");
        assert!(matches!(err, StorageError::Migration { .. }));
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
            "core_identity_links",
            "core_workspace_catalog",
            "core_workspace_snapshots",
            "core_workspace_snapshot_revisions",
            "index_analysis_cache",
            "fts_retrieval_units",
            "fts_symbol_names",
            "ops_tasks",
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
    fn final_retrieval_unit_columns_exist_after_migration() {
        let conn = in_memory_conn();
        run_migrations(&conn).unwrap();

        // Verify analyzer/search provenance columns are part of the frozen baseline.
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
                "column '{col}' should exist in core_retrieval_units in final baseline schema"
            );
        }
    }
}
