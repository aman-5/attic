//! Canonical SQLite schema: one baseline, no migration chain.
//!
//! `attic.db` is derived entirely from source, so a database created by any
//! other schema — an older build, a newer build, or an unrelated file — is
//! wiped and rebuilt from the baseline; the next index pass repopulates it.

use rusqlite::Connection;
use tracing::{info, warn};

use crate::error::StorageError;

const BASELINE_SQL: &str = include_str!("../../../migrations/0001_initial.sql");
/// Id recorded in `core_schema_migrations` by the baseline.
pub const BASELINE_VERSION: &str = "0001_initial";

/// Bring `conn` to the current schema: apply the baseline to a fresh
/// database, keep a current one untouched, and rebuild anything else.
pub fn run_migrations(conn: &Connection) -> Result<(), StorageError> {
    ensure_migrations_table(conn)?;
    if !is_current_schema(conn)? {
        warn!(
            "attic.db was created by a different schema; rebuilding it (index data is re-derived from source)"
        );
        reset_schema(conn)?;
        ensure_migrations_table(conn)?;
    }
    let applied: i64 = conn.query_row(
        "SELECT COUNT(*) FROM core_schema_migrations WHERE id = ?1",
        rusqlite::params![BASELINE_VERSION],
        |row| row.get(0),
    )?;
    if applied > 0 {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    match conn.execute_batch(BASELINE_SQL) {
        Ok(()) => {
            conn.execute_batch("COMMIT;")?;
            info!("schema {BASELINE_VERSION} applied");
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK;");
            Err(StorageError::Migration {
                message: format!("schema {BASELINE_VERSION} failed and was rolled back: {e}"),
            })
        }
    }
}

fn ensure_migrations_table(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS core_schema_migrations (
            id          TEXT    PRIMARY KEY NOT NULL,
            applied_at  INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000000)
        );",
    )?;
    Ok(())
}

/// `true` for a fresh database or one created by exactly this baseline.
fn is_current_schema(conn: &Connection) -> Result<bool, StorageError> {
    let mut stmt = conn.prepare("SELECT id FROM core_schema_migrations")?;
    let applied: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    if applied.is_empty() {
        let other_tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                AND name != 'core_schema_migrations'",
            [],
            |row| row.get(0),
        )?;
        return Ok(other_tables == 0);
    }
    if applied != [BASELINE_VERSION] {
        return Ok(false);
    }
    // An earlier baseline carried the same id but not the canonical columns.
    let has_canonical_hash: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('core_retrieval_units')
          WHERE name = 'canonical_hash'",
        [],
        |row| row.get(0),
    )?;
    Ok(has_canonical_hash == 1)
}

/// Drop every table and view in `conn` (virtual tables first, which also
/// removes their shadow tables). Shared with the semantic store, whose
/// database is disposable in the same way.
pub fn reset_schema(conn: &Connection) -> Result<(), StorageError> {
    conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
    let dropped = drop_all_objects(conn);
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;
    dropped
}

fn drop_all_objects(conn: &Connection) -> Result<(), StorageError> {
    let objects = |sql: &str| -> Result<Vec<(String, String)>, StorageError> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    };
    let quote = |name: &str| format!("\"{}\"", name.replace('"', "\"\""));
    for (name, _) in objects("SELECT name, '' FROM sqlite_master WHERE type = 'view'")? {
        conn.execute_batch(&format!("DROP VIEW IF EXISTS {};", quote(&name)))?;
    }
    let tables_sql = "SELECT name, COALESCE(sql, '') FROM sqlite_master
                       WHERE type = 'table' AND name NOT LIKE 'sqlite_%'";
    for (name, sql) in objects(tables_sql)? {
        if sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("CREATE VIRTUAL TABLE")
        {
            conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", quote(&name)))?;
        }
    }
    for (name, _) in objects(tables_sql)? {
        conn.execute_batch(&format!("DROP TABLE IF EXISTS {};", quote(&name)))?;
    }
    Ok(())
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

    fn recorded_versions(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT id FROM core_schema_migrations ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn fresh_database_gets_the_single_baseline() {
        let conn = in_memory_conn();
        run_migrations(&conn).expect("first run should succeed");
        assert_eq!(recorded_versions(&conn), vec![BASELINE_VERSION]);
    }

    #[test]
    fn migration_is_idempotent_and_keeps_data() {
        let conn = in_memory_conn();
        run_migrations(&conn).expect("first run");
        conn.execute(
            "INSERT INTO ops_server_state (id, watcher_epoch, schema_version, server_version,
                                           last_startup_at, config_hash)
             VALUES ('singleton', 7, '1.0.0', 'test', 1, 'h')",
            [],
        )
        .unwrap();
        run_migrations(&conn).expect("second run should be a no-op");
        assert_eq!(recorded_versions(&conn), vec![BASELINE_VERSION]);
        let epoch: i64 = conn
            .query_row("SELECT watcher_epoch FROM ops_server_state", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(epoch, 7, "a current database must not be rebuilt");
    }

    #[test]
    fn database_from_another_schema_is_rebuilt() {
        let conn = in_memory_conn();
        conn.execute_batch(
            "CREATE TABLE core_schema_migrations (
                id TEXT PRIMARY KEY NOT NULL,
                applied_at INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO core_schema_migrations (id)
                  VALUES ('0001_initial'), ('0002_retrieval_occurrence');
             CREATE TABLE ops_migration_log (id TEXT PRIMARY KEY);
             CREATE TABLE core_evidence (id TEXT PRIMARY KEY);
             CREATE VIRTUAL TABLE fts_symbol_names USING fts5(qualified_name, kind);
             CREATE VIEW v_old AS SELECT id FROM core_evidence;",
        )
        .unwrap();

        run_migrations(&conn).expect("an old database must be rebuilt, not rejected");

        assert_eq!(recorded_versions(&conn), vec![BASELINE_VERSION]);
        for gone in [
            "ops_migration_log",
            "core_evidence",
            "fts_symbol_names",
            "v_old",
        ] {
            assert!(!object_exists(&conn, gone), "{gone} must be gone");
        }
        assert!(object_exists(&conn, "core_retrieval_units"));
    }

    #[test]
    fn earlier_baseline_with_the_same_id_is_rebuilt() {
        let conn = in_memory_conn();
        conn.execute_batch(
            "CREATE TABLE core_schema_migrations (
                id TEXT PRIMARY KEY NOT NULL,
                applied_at INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO core_schema_migrations (id) VALUES ('0001_initial');
             CREATE TABLE core_retrieval_units (id TEXT PRIMARY KEY, retrieval_text TEXT);",
        )
        .unwrap();

        run_migrations(&conn).unwrap();

        let has_canonical_hash: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('core_retrieval_units')
                  WHERE name = 'canonical_hash'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_canonical_hash, 1);
    }

    #[test]
    fn database_from_a_newer_build_is_rebuilt() {
        let conn = in_memory_conn();
        run_migrations(&conn).unwrap();
        conn.execute(
            "INSERT INTO core_schema_migrations (id) VALUES ('0099_future_migration')",
            [],
        )
        .unwrap();
        run_migrations(&conn).unwrap();
        assert_eq!(recorded_versions(&conn), vec![BASELINE_VERSION]);
    }

    fn object_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
            rusqlite::params![name],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
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
            "ops_tasks",
            "ops_server_state",
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
            "canonical_text",
            "canonical_hash",
            "occurrence_metadata",
            "coverage_state",
        ] {
            assert!(
                columns.contains(&col.to_string()),
                "column '{col}' should exist in core_retrieval_units in final baseline schema"
            );
        }
    }
}
