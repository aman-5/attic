//! S4 — Atomic batch publication of file identities and occurrences.
//!
//! A **publication** is a transactional unit of work that persists all
//! `core_file_identities` + `core_file_occurrences` belonging to a single
//! discovery pass over a source revision.  Every record in the batch is written
//! inside one `BEGIN IMMEDIATE` / `COMMIT` so the revision's file set is always
//! visible atomically.
//!
//! This module intentionally has no schema table of its own.  It composes the
//! primitives from [`super::file_occurrence`] and enforces the transactional
//! invariant that callers must not be responsible for managing.

use rusqlite::Connection;

use crate::error::StorageError;
use crate::repository::file_occurrence::{
    upsert_file_identity, insert_file_occurrence, NewFileOccurrence,
};

use attic_core::{FileIdentityId, RepositoryId};

/// A single item in a publication batch.
///
/// Each item pairs a file identity (idempotently upserted) with exactly one
/// file occurrence (inserted once per revision pass).
pub struct PublicationItem<'a> {
    /// Stable identity key for the file (upserted with `INSERT OR IGNORE`).
    pub identity_id: &'a FileIdentityId,
    /// Foreign key to `core_repositories.id` for the file identity row.
    pub identity_repository_id: &'a RepositoryId,
    /// Canonical basis string used to establish cross-revision file identity
    /// (e.g., Git blob SHA or path-derived hash).
    pub identity_stable_id_basis: &'a str,
    /// Occurrence record to insert for this revision.
    pub occurrence: NewFileOccurrence<'a>,
}

/// Persist a batch of file identities + occurrences inside a single
/// `BEGIN IMMEDIATE` transaction.
///
/// On the first constraint violation or SQLite error the transaction is rolled
/// back and the error is returned to the caller.  No partial writes survive.
///
/// # Errors
///
/// - `StorageError::Sqlite(_)` — any rusqlite error including constraint
///   violations (e.g., duplicate occurrence `id`).
/// - `StorageError::MutexPoisoned(_)` — should never occur here since this
///   function takes a direct `&Connection` reference.
pub fn publish_file_batch(
    conn: &Connection,
    items: &[PublicationItem<'_>],
) -> Result<(), StorageError> {
    if items.is_empty() {
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;

    let result: Result<(), StorageError> = (|| {
        for item in items {
            upsert_file_identity(
                conn,
                item.identity_id,
                item.identity_repository_id,
                item.identity_stable_id_basis,
            )?;
            insert_file_occurrence(conn, &item.occurrence)?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            // Best-effort rollback; ignore secondary error.
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use crate::repository::file_occurrence::{
        exists_file_identity, exists_file_occurrence, NewFileOccurrence,
    };
    use crate::repository::repository::upsert_repository;
    use crate::repository::source_revision::insert_source_revision;
    use attic_core::{
        DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType,
        RepositoryId, SecretScanState, SecurityState, SourceRevisionId, SourceType,
    };
    use rusqlite::Connection;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    fn seed_repo_and_revision(conn: &Connection) -> (RepositoryId, SourceRevisionId) {
        let repo_id = RepositoryId::new_v4();
        upsert_repository(conn, &repo_id, "/repo", "test").unwrap();
        let rev_id = SourceRevisionId::new_v4();
        insert_source_revision(
            conn,
            &rev_id,
            &repo_id,
            "cafebabe",
            "2026-06-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        (repo_id, rev_id)
    }

    #[test]
    fn empty_batch_is_a_noop() {
        let conn = migrated_conn();
        publish_file_batch(&conn, &[]).unwrap();
    }

    #[test]
    fn single_item_batch_persists_identity_and_occurrence() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let fid = FileIdentityId::new_v4();
        let occ_id = FileOccurrenceId::new_v4();

        let items = vec![PublicationItem {
            identity_id: &fid,
            identity_repository_id: &repo_id,
            identity_stable_id_basis: "blob:aabbcc",
            occurrence: NewFileOccurrence {
                id: &occ_id,
                file_identity_id: &fid,
                source_revision_id: &rev_id,
                index_generation_id: None,
                path: "src/lib.rs",
                content_hash: "blake3:aabbcc",
                size_bytes: 2048,
                language: Some("rust"),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Pending,
                existence_state: ExistenceState::Present,
            },
        }];

        publish_file_batch(&conn, &items).unwrap();

        assert!(exists_file_identity(&conn, &fid).unwrap());
        assert!(exists_file_occurrence(&conn, &occ_id).unwrap());
    }

    #[test]
    fn multi_item_batch_all_persisted() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let fid1 = FileIdentityId::new_v4();
        let fid2 = FileIdentityId::new_v4();
        let occ1 = FileOccurrenceId::new_v4();
        let occ2 = FileOccurrenceId::new_v4();

        let items = vec![
            PublicationItem {
                identity_id: &fid1,
                identity_repository_id: &repo_id,
                identity_stable_id_basis: "blob:111",
                occurrence: NewFileOccurrence {
                    id: &occ1,
                    file_identity_id: &fid1,
                    source_revision_id: &rev_id,
                    index_generation_id: None,
                    path: "Cargo.toml",
                    content_hash: "blake3:111",
                    size_bytes: 512,
                    language: None,
                    file_type: FileType::Toml,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: SecurityState::Pending,
                    existence_state: ExistenceState::Present,
                },
            },
            PublicationItem {
                identity_id: &fid2,
                identity_repository_id: &repo_id,
                identity_stable_id_basis: "blob:222",
                occurrence: NewFileOccurrence {
                    id: &occ2,
                    file_identity_id: &fid2,
                    source_revision_id: &rev_id,
                    index_generation_id: None,
                    path: "src/main.rs",
                    content_hash: "blake3:222",
                    size_bytes: 1024,
                    language: Some("rust"),
                    file_type: FileType::Rust,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: SecurityState::Pending,
                    existence_state: ExistenceState::Present,
                },
            },
        ];

        publish_file_batch(&conn, &items).unwrap();

        assert!(exists_file_identity(&conn, &fid1).unwrap());
        assert!(exists_file_identity(&conn, &fid2).unwrap());
        assert!(exists_file_occurrence(&conn, &occ1).unwrap());
        assert!(exists_file_occurrence(&conn, &occ2).unwrap());
    }

    #[test]
    fn batch_rolls_back_on_duplicate_occurrence_id() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let fid1 = FileIdentityId::new_v4();
        let fid2 = FileIdentityId::new_v4();
        // Both occurrences share the same ID — second insert must violate PK.
        let shared_occ_id = FileOccurrenceId::new_v4();

        let items = vec![
            PublicationItem {
                identity_id: &fid1,
                identity_repository_id: &repo_id,
                identity_stable_id_basis: "blob:aaa",
                occurrence: NewFileOccurrence {
                    id: &shared_occ_id,
                    file_identity_id: &fid1,
                    source_revision_id: &rev_id,
                    index_generation_id: None,
                    path: "a.rs",
                    content_hash: "blake3:aaa",
                    size_bytes: 100,
                    language: Some("rust"),
                    file_type: FileType::Rust,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: SecurityState::Pending,
                    existence_state: ExistenceState::Present,
                },
            },
            PublicationItem {
                identity_id: &fid2,
                identity_repository_id: &repo_id,
                identity_stable_id_basis: "blob:bbb",
                occurrence: NewFileOccurrence {
                    id: &shared_occ_id,  // duplicate — must trigger rollback
                    file_identity_id: &fid2,
                    source_revision_id: &rev_id,
                    index_generation_id: None,
                    path: "b.rs",
                    content_hash: "blake3:bbb",
                    size_bytes: 200,
                    language: Some("rust"),
                    file_type: FileType::Rust,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: SecurityState::Pending,
                    existence_state: ExistenceState::Present,
                },
            },
        ];

        let result = publish_file_batch(&conn, &items);
        assert!(result.is_err(), "batch with duplicate pk must fail");

        // Neither occurrence nor identity must have been persisted (rollback).
        assert!(
            !exists_file_occurrence(&conn, &shared_occ_id).unwrap(),
            "rollback must prevent occurrence from persisting"
        );
        assert!(
            !exists_file_identity(&conn, &fid1).unwrap(),
            "rollback must prevent first identity from persisting"
        );
        assert!(
            !exists_file_identity(&conn, &fid2).unwrap(),
            "rollback must prevent second identity from persisting"
        );
    }

    #[test]
    fn identity_upsert_is_idempotent_across_batches() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let fid = FileIdentityId::new_v4();
        let occ1 = FileOccurrenceId::new_v4();
        let occ2 = FileOccurrenceId::new_v4();

        // First batch: creates identity + occurrence 1.
        publish_file_batch(
            &conn,
            &[PublicationItem {
                identity_id: &fid,
                identity_repository_id: &repo_id,
                identity_stable_id_basis: "blob:stable",
                occurrence: NewFileOccurrence {
                    id: &occ1,
                    file_identity_id: &fid,
                    source_revision_id: &rev_id,
                    index_generation_id: None,
                    path: "stable.rs",
                    content_hash: "blake3:v1",
                    size_bytes: 300,
                    language: Some("rust"),
                    file_type: FileType::Rust,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: SecurityState::Pending,
                    existence_state: ExistenceState::Present,
                },
            }],
        )
        .unwrap();

        // Second batch: re-uses the same identity (INSERT OR IGNORE) + new occurrence.
        publish_file_batch(
            &conn,
            &[PublicationItem {
                identity_id: &fid,
                identity_repository_id: &repo_id,
                identity_stable_id_basis: "blob:stable",
                occurrence: NewFileOccurrence {
                    id: &occ2,
                    file_identity_id: &fid,
                    source_revision_id: &rev_id,
                    index_generation_id: None,
                    path: "stable_v2.rs",
                    content_hash: "blake3:v2",
                    size_bytes: 400,
                    language: Some("rust"),
                    file_type: FileType::Rust,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: SecurityState::Pending,
                    existence_state: ExistenceState::Present,
                },
            }],
        )
        .unwrap();

        // Exactly one identity row.
        let identity_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_file_identities WHERE id = ?1",
                rusqlite::params![fid.to_string_repr()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(identity_count, 1, "identity must not be duplicated");

        // Two distinct occurrence rows.
        assert!(exists_file_occurrence(&conn, &occ1).unwrap());
        assert!(exists_file_occurrence(&conn, &occ2).unwrap());
    }
}
