//! S3 — `core_source_revisions` write-once insert and existence check.

use rusqlite::Connection;

use attic_core::{RepositoryId, SourceRevisionId, SourceType};

use crate::error::StorageError;

/// Insert a source revision record.
///
/// This is **write-once** — revisions are immutable snapshots and must not be
/// updated after creation.  Returns `Err(StorageError::Sqlite(_))` with
/// `SQLITE_CONSTRAINT_PRIMARYKEY` if the ID already exists.
///
/// `commit_sha` maps to the `commit_sha` column (nullable for non-Git repos).
/// `_committed_at` and `_source_type` are accepted for call-site compatibility
/// but the schema does not have corresponding columns; `captured_at` is set to
/// the current wall-clock time.
pub fn insert_source_revision(
    conn: &Connection,
    id: &SourceRevisionId,
    repository_id: &RepositoryId,
    commit_sha: &str,
    _committed_at: &str,
    _source_type: SourceType,
) -> Result<(), StorageError> {
    let now_us: i64 = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros() as i64)
            .unwrap_or(0)
    };
    conn.execute(
        "INSERT INTO core_source_revisions
             (id, repository_id, commit_sha,
              working_tree_manifest_hash, discovery_policy_hash,
              unstable_capture, captured_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
        rusqlite::params![
            id.to_string_repr(),
            repository_id.to_string_repr(),
            commit_sha,
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            now_us,
        ],
    )?;
    Ok(())
}

/// Return `true` if a source revision with the given ID exists in the database.
pub fn exists_source_revision(
    conn: &Connection,
    id: &SourceRevisionId,
) -> Result<bool, StorageError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM core_source_revisions WHERE id = ?1",
        rusqlite::params![id.to_string_repr()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use crate::repository::repository::upsert_repository;
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    #[test]
    fn insert_and_exists() {
        let conn = migrated_conn();
        let repo_id = RepositoryId::new_v4();
        upsert_repository(&conn, &repo_id, "/repo", "test").unwrap();
        let rev_id = SourceRevisionId::new_v4();
        assert!(!exists_source_revision(&conn, &rev_id).unwrap());
        insert_source_revision(
            &conn,
            &rev_id,
            &repo_id,
            "abc123def456",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        assert!(exists_source_revision(&conn, &rev_id).unwrap());
    }

    #[test]
    fn duplicate_insert_fails() {
        let conn = migrated_conn();
        let repo_id = RepositoryId::new_v4();
        upsert_repository(&conn, &repo_id, "/repo", "test").unwrap();
        let rev_id = SourceRevisionId::new_v4();
        insert_source_revision(
            &conn,
            &rev_id,
            &repo_id,
            "abc123",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        let result = insert_source_revision(
            &conn,
            &rev_id,
            &repo_id,
            "abc123",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        );
        assert!(result.is_err(), "duplicate insert must fail");
    }
}
