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

/// Initialize a new canonical catalog or keep an exactly compatible schema.
///
/// Unlike [`run_migrations`], this never resets an existing database. Unknown
/// schemas (including older layouts) fail closed so callers can preserve the
/// original file and choose a fresh catalog path explicitly.
pub fn migrate_new_or_compatible_catalog(conn: &Connection) -> Result<(), StorageError> {
    let has_migrations: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
          WHERE type = 'table' AND name = 'core_schema_migrations')",
        [],
        |row| row.get(0),
    )?;
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master
          WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    if tables == 0 {
        return run_migrations(conn);
    }
    if has_migrations && is_current_schema(conn)? {
        return Ok(());
    }
    Err(StorageError::Migration {
        message: "existing database schema is incompatible; preserved unchanged. Choose a fresh catalog path and explicitly reindex".into(),
    })
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
    has_current_columns(conn)
}

/// Column/table probe distinguishing this baseline from earlier baselines
/// that recorded the same migration id.
fn has_current_columns(conn: &Connection) -> Result<bool, StorageError> {
    let probe: i64 = conn.query_row(
        "SELECT
             (SELECT COUNT(*) FROM pragma_table_info('core_retrieval_units')
               WHERE name = 'canonical_hash')
           + (SELECT COUNT(*) FROM pragma_table_info('core_file_occurrences')
               WHERE name IN ('mtime_ns', 'change_time_ns', 'file_id', 'captured_at_ns'))
           + (SELECT COUNT(*) FROM pragma_table_info('core_repositories')
               WHERE name = 'root_identity')
           + (SELECT COUNT(*) FROM sqlite_master
               WHERE type = 'table' AND name IN (
                   'ops_store_version', 'core_manifest_contents',
                   'core_crossrepo_diagnostics', 'core_import_targets'))",
        [],
        |row| row.get(0),
    )?;
    Ok(probe == 10)
}

/// `true` when `conn` holds exactly the current baseline schema (used by
/// repository-store verification so a store created by an older build is
/// re-initialized instead of failing at the first write).
pub fn schema_is_current(conn: &Connection) -> Result<bool, StorageError> {
    let has_migrations: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master
          WHERE type = 'table' AND name = 'core_schema_migrations')",
        [],
        |row| row.get(0),
    )?;
    Ok(has_migrations && is_current_schema(conn)?)
}

/// Upgrade reset: when the catalog database at `db_path` was created by a
/// different schema, delete it (with its `-wal`/`-shm` side files) and every
/// per-repository index under `<data dir>/repositories`, so the server starts
/// clean and re-derives all index data from the configured roots.
///
/// Must run before any connection to `db_path` is opened. Spec documents
/// live in a separate store and are untouched. Returns `true` when a reset
/// happened.
pub fn reset_outdated_catalog(db_path: &std::path::Path) -> Result<bool, StorageError> {
    if !db_path.is_file() {
        return Ok(false);
    }
    let outdated = {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
              WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        tables != 0 && !schema_is_current(&conn)?
    };
    if !outdated {
        return Ok(false);
    }
    warn!(
        path = %db_path.display(),
        "catalog schema is outdated; deleting all Attic index data for a full reindex"
    );
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        let path = std::path::PathBuf::from(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(parent) = db_path.parent() {
        remove_owned_store_dirs(&parent.join("repositories"))?;
        // Pinned-commit stores: <commits>/<repository uuid>/<sha>/...
        let commits = parent.join("commits");
        if let Ok(repos) = std::fs::read_dir(&commits) {
            for repo in repos.flatten() {
                if uuid::Uuid::parse_str(&repo.file_name().to_string_lossy()).is_ok() {
                    std::fs::remove_dir_all(repo.path())?;
                }
            }
        }
    }
    Ok(true)
}

/// Delete only Attic-owned repository store directories under `root`:
/// entries named by a UUID that contain nothing but `index.db*` files.
/// Anything else (a user folder that happens to be called `repositories`)
/// is left untouched.
fn remove_owned_store_dirs(root: &std::path::Path) -> Result<(), StorageError> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || uuid::Uuid::parse_str(&entry.file_name().to_string_lossy()).is_err() {
            continue;
        }
        let only_index_files = std::fs::read_dir(&path)?
            .flatten()
            .all(|f| f.path().is_file() && f.file_name().to_string_lossy().starts_with("index.db"));
        if only_index_files {
            std::fs::remove_dir_all(&path)?;
        }
    }
    // Remove the container only if it is now empty.
    let _ = std::fs::remove_dir(root);
    Ok(())
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
    fn outdated_catalog_and_repository_indexes_are_deleted_on_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("attic.db");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE core_schema_migrations (id TEXT PRIMARY KEY, applied_at INTEGER);
                 INSERT INTO core_schema_migrations VALUES ('0001_initial', 0);
                 CREATE TABLE core_retrieval_units (id TEXT, canonical_hash TEXT);",
            )
            .unwrap();
        }
        let repo_dir = dir
            .path()
            .join("repositories")
            .join("5a12aced-2c4f-81db-aea2-b8816b62551a");
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("index.db"), b"old").unwrap();
        // A user folder that merely shares the name must survive.
        let user_dir = dir.path().join("repositories").join("my-notes");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(user_dir.join("todo.txt"), b"keep").unwrap();
        std::fs::write(dir.path().join("attic.db-wal"), b"").unwrap();

        assert!(reset_outdated_catalog(&db).unwrap());
        assert!(!db.exists());
        assert!(!dir.path().join("attic.db-wal").exists());
        assert!(!repo_dir.exists());
        assert!(user_dir.join("todo.txt").exists());

        // A current catalog is left alone.
        {
            let conn = Connection::open(&db).unwrap();
            run_migrations(&conn).unwrap();
            assert!(schema_is_current(&conn).unwrap());
        }
        assert!(!reset_outdated_catalog(&db).unwrap());
        assert!(db.exists());
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
