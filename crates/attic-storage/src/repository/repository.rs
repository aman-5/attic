//! S3 — `core_repositories` CRUD operations.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use attic_core::RepositoryId;

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// Stats types
// ---------------------------------------------------------------------------

/// Per-repository statistics returned by [`get_repository_stats`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryStats {
    /// Repository UUID string.
    pub id: String,
    /// Human-readable display name.
    pub display_name: String,
    /// Number of distinct file occurrences.
    pub file_count: i64,
    /// Number of retrieval units indexed.
    pub unit_count: i64,
}

/// Database-level statistics returned by [`get_db_stats`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbStats {
    /// Number of applied schema migrations.
    pub migration_count: i64,
    /// Number of indexed repositories.
    pub repository_count: i64,
    /// Total number of retrieval units across all repositories.
    pub unit_count: i64,
}

/// Insert or update a repository record.
///
/// Uses `ON CONFLICT(id) DO UPDATE` so this is safe to call on every startup
/// even if the repository was previously registered.
pub fn upsert_repository(
    conn: &Connection,
    id: &RepositoryId,
    root_path: &str,
    display_name: &str,
) -> Result<(), StorageError> {
    let now_us: i64 = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
    };
    conn.execute(
        "INSERT INTO core_repositories
             (id, root_path, display_name, is_git, case_sensitive, created_at, updated_at)
         VALUES (?1, ?2, ?3, 1, 1, ?4, ?4)
         ON CONFLICT(id) DO UPDATE SET
             root_path    = excluded.root_path,
             display_name = excluded.display_name,
             updated_at   = excluded.updated_at",
        rusqlite::params![id.to_string_repr(), root_path, display_name, now_us],
    )?;
    Ok(())
}

/// Return per-repository file and unit counts for all indexed repositories.
///
/// Counts only the **latest, still-`present`** occurrence per path — not
/// every historical occurrence row (rows are never physically deleted on
/// reindex, so an unfiltered count grows every time any file is reindexed
/// and never reflects the current repository state).
///
/// Results are ordered by `display_name` ascending.
/// The query never returns `root_path` — absolute paths are kept server-side.
pub fn get_repository_stats(conn: &Connection) -> Result<Vec<RepositoryStats>, StorageError> {
    // The latest-row-per-path determination MUST happen before filtering by
    // existence_state: a path's newest row (by stable occurrence sequence) may be a `deleted`
    // tombstone superseding an older `present` row for the same path, and
    // that tombstone must win — filtering `present` rows first (before
    // computing MAX(occurrence_seq)) would incorrectly resurrect the stale row.
    let sql = "
        WITH latest_row AS (
             SELECT fi.repository_id AS repository_id,
                    fo.path AS path,
                    MAX(fo.occurrence_seq) AS max_seq
               FROM core_file_occurrences fo
               JOIN core_file_identities fi ON fo.file_identity_id = fi.id
              GROUP BY fi.repository_id, fo.path
        ), latest AS (
             SELECT lr.repository_id, fo.id AS fo_id, fo.existence_state
               FROM latest_row lr
               JOIN core_file_occurrences fo ON fo.occurrence_seq = lr.max_seq
        )
        SELECT r.id, r.display_name,
               COUNT(DISTINCT CASE WHEN latest.existence_state = 'present' THEN latest.fo_id END) AS files,
               COUNT(CASE WHEN latest.existence_state = 'present' THEN ru.id END) AS units
          FROM core_repositories r
          LEFT JOIN latest ON latest.repository_id = r.id
          LEFT JOIN core_retrieval_units ru ON ru.file_occurrence_id = latest.fo_id
         GROUP BY r.id
         ORDER BY r.display_name
    ";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(RepositoryStats {
            id: row.get(0)?,
            display_name: row.get(1)?,
            file_count: row.get(2)?,
            unit_count: row.get(3)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Return database-level statistics (migration count, repository count, unit count).
pub fn get_db_stats(conn: &Connection) -> Result<DbStats, StorageError> {
    let migration_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM core_schema_migrations", [], |r| {
            r.get(0)
        })?;
    let repository_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM core_repositories", [], |r| r.get(0))?;
    let unit_count: i64 = conn.query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
        r.get(0)
    })?;
    Ok(DbStats {
        migration_count,
        repository_count,
        unit_count,
    })
}

/// Return the `root_path` of a repository by ID, or `None` if not found.
pub fn get_repository_path(
    conn: &Connection,
    id: &RepositoryId,
) -> Result<Option<String>, StorageError> {
    let mut stmt = conn.prepare("SELECT root_path FROM core_repositories WHERE id = ?1")?;
    let mut rows = stmt.query(rusqlite::params![id.to_string_repr()])?;
    match rows.next()? {
        Some(row) => Ok(Some(row.get(0)?)),
        None => Ok(None),
    }
}

/// Look up a repository by its `root_path`, returning its ID or `None`.
///
/// Used by `attic-indexing` to avoid ad-hoc SQL for repository bootstrapping.
pub fn lookup_repository_by_root_path(
    conn: &Connection,
    root_path: &str,
) -> Result<Option<RepositoryId>, StorageError> {
    use rusqlite::OptionalExtension;
    let id_str: Option<String> = conn
        .query_row(
            "SELECT id FROM core_repositories WHERE root_path = ?1 LIMIT 1",
            rusqlite::params![root_path],
            |r| r.get(0),
        )
        .optional()?;
    match id_str {
        Some(s) => {
            let id = s.parse::<RepositoryId>().map_err(|e| {
                StorageError::Domain(attic_core::CoreError::UnknownVariant {
                    type_name: "RepositoryId",
                    value: e.to_string(),
                })
            })?;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn upsert_and_get_repository() {
        let conn = migrated_conn();
        let id = RepositoryId::new_v4();

        upsert_repository(&conn, &id, "/home/user/project", "my-project").unwrap();

        let path = get_repository_path(&conn, &id).unwrap();
        assert_eq!(path, Some("/home/user/project".to_owned()));
    }

    #[test]
    fn upsert_updates_existing_record() {
        let conn = migrated_conn();
        let id = RepositoryId::new_v4();

        upsert_repository(&conn, &id, "/old/path", "old-name").unwrap();
        upsert_repository(&conn, &id, "/new/path", "new-name").unwrap();

        let path = get_repository_path(&conn, &id).unwrap();
        assert_eq!(path, Some("/new/path".to_owned()));
    }

    #[test]
    fn get_repository_path_returns_none_for_unknown_id() {
        let conn = migrated_conn();
        let id = RepositoryId::new_v4();
        assert_eq!(get_repository_path(&conn, &id).unwrap(), None);
    }
}
