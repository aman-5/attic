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

/// Insert a source revision record with real manifest and policy hashes.
///
/// Accepts the actual `working_tree_manifest_hash` (64-char BLAKE3 hex from
/// `SourceManifest::manifest_hash`) and `discovery_policy_hash` (64-char
/// BLAKE3 hex of the serialised `DiscoveryPolicy`) so the revision record is
/// genuinely reproducible.
///
/// `commit_sha` is `None` for non-Git repositories.
/// `unstable_capture` should be `true` when the manifest contains any
/// unstable captures (e.g. working-tree modifications on top of a VCS commit).
pub fn insert_source_revision_with_hashes(
    conn: &Connection,
    id: &SourceRevisionId,
    repository_id: &RepositoryId,
    commit_sha: Option<&str>,
    working_tree_manifest_hash: &str,
    discovery_policy_hash: &str,
    unstable_capture: bool,
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
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            id.to_string_repr(),
            repository_id.to_string_repr(),
            commit_sha,
            working_tree_manifest_hash,
            discovery_policy_hash,
            if unstable_capture { 1i64 } else { 0i64 },
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

    #[test]
    fn insert_with_hashes_stores_real_values() {
        let conn = migrated_conn();
        let repo_id = RepositoryId::new_v4();
        upsert_repository(&conn, &repo_id, "/repo", "test").unwrap();
        let rev_id = SourceRevisionId::new_v4();

        let manifest_hash = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let policy_hash = "9988776655443322110099887766554433221100998877665544332211009988";

        insert_source_revision_with_hashes(
            &conn,
            &rev_id,
            &repo_id,
            Some("deadbeef1234"),
            manifest_hash,
            policy_hash,
            false,
        )
        .unwrap();

        let (mhash, phash, unstable): (String, String, i64) = conn
            .query_row(
                "SELECT working_tree_manifest_hash, discovery_policy_hash, unstable_capture
                   FROM core_source_revisions WHERE id = ?1",
                rusqlite::params![rev_id.to_string_repr()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();

        assert_eq!(mhash, manifest_hash);
        assert_eq!(phash, policy_hash);
        assert_eq!(unstable, 0);
    }

    #[test]
    fn insert_with_hashes_none_commit_sha() {
        let conn = migrated_conn();
        let repo_id = RepositoryId::new_v4();
        upsert_repository(&conn, &repo_id, "/repo", "test").unwrap();
        let rev_id = SourceRevisionId::new_v4();

        insert_source_revision_with_hashes(
            &conn,
            &rev_id,
            &repo_id,
            None,
            "0000000000000000000000000000000000000000000000000000000000000001",
            "0000000000000000000000000000000000000000000000000000000000000002",
            true,
        )
        .unwrap();

        let (sha, unstable): (Option<String>, i64) = conn
            .query_row(
                "SELECT commit_sha, unstable_capture FROM core_source_revisions WHERE id = ?1",
                rusqlite::params![rev_id.to_string_repr()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert!(sha.is_none());
        assert_eq!(unstable, 1);
    }
}
