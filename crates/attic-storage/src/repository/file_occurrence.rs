//! S4 — `core_file_identities` and `core_file_occurrences` CRUD.
//!
//! `core_file_identities` tracks the stable identity of a file across revisions.
//! `core_file_occurrences` tracks a file's presence and metadata within a specific
//! source revision.

use rusqlite::Connection;

use attic_core::{
    DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType,
    IndexGenerationId, RepositoryId, SecretScanState, SecurityState, SourceRevisionId,
};

use crate::error::StorageError;

// ---------------------------------------------------------------------------
// core_file_identities
// ---------------------------------------------------------------------------

/// Insert or ignore a file identity record.
///
/// `stable_id_basis` is the canonical basis for cross-revision identity
/// (e.g., Git blob SHA or a path-derived hash for non-Git repos).
/// This is **idempotent** — re-inserting the same `id` is a no-op.
pub fn upsert_file_identity(
    conn: &Connection,
    id: &FileIdentityId,
    repository_id: &RepositoryId,
    stable_id_basis: &str,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR IGNORE INTO core_file_identities
             (id, repository_id, stable_id_basis)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![
            id.to_string_repr(),
            repository_id.to_string_repr(),
            stable_id_basis,
        ],
    )?;
    Ok(())
}

/// Return `true` if a file identity with the given `id` exists.
pub fn exists_file_identity(
    conn: &Connection,
    id: &FileIdentityId,
) -> Result<bool, StorageError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM core_file_identities WHERE id = ?1",
        rusqlite::params![id.to_string_repr()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

// ---------------------------------------------------------------------------
// core_file_occurrences
// ---------------------------------------------------------------------------

/// All fields required to create a new file occurrence record.
///
/// Fields that have database-level defaults (`freshness_state`,
/// `secret_scan_state`, `secret_pattern_version`) are omitted and will be
/// set to their defaults (`CURRENT`, `PENDING`, `1`).
pub struct NewFileOccurrence<'a> {
    /// Primary key UUID for this occurrence row.
    pub id: &'a FileOccurrenceId,
    /// Foreign key to `core_file_identities.id`.
    pub file_identity_id: &'a FileIdentityId,
    /// Foreign key to `core_source_revisions.id`.
    pub source_revision_id: &'a SourceRevisionId,
    /// Foreign key to `core_index_generations.id`; `None` if not yet indexed.
    pub index_generation_id: Option<&'a IndexGenerationId>,
    /// Workspace-relative normalized path (forward slashes).
    pub path: &'a str,
    /// BLAKE3 hex digest of the raw file bytes.
    pub content_hash: &'a str,
    /// File size in bytes.
    pub size_bytes: i64,
    /// Detected language; `None` for binary or language-unknown files.
    pub language: Option<&'a str>,
    /// Broad file-type classification.
    pub file_type: FileType,
    /// How this file was discovered.
    pub discovery_class: DiscoveryClass,
    /// Security classification derived from secret scanning.
    pub security_state: SecurityState,
    /// Whether the file is present or deleted in this revision.
    pub existence_state: ExistenceState,
}

/// Insert a new file occurrence record.
///
/// Returns `Err(StorageError::Sqlite(_))` with `SQLITE_CONSTRAINT_PRIMARYKEY`
/// if the `id` already exists.
pub fn insert_file_occurrence(
    conn: &Connection,
    rec: &NewFileOccurrence<'_>,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO core_file_occurrences
             (id, file_identity_id, source_revision_id, index_generation_id,
              path, content_hash, size_bytes, language,
              file_type, discovery_class, security_state, existence_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            rec.id.to_string_repr(),
            rec.file_identity_id.to_string_repr(),
            rec.source_revision_id.to_string_repr(),
            rec.index_generation_id.map(|id| id.to_string_repr()),
            rec.path,
            rec.content_hash,
            rec.size_bytes,
            rec.language,
            rec.file_type.as_str(),
            rec.discovery_class.as_str(),
            rec.security_state.as_str(),
            rec.existence_state.as_str(),
        ],
    )?;
    Ok(())
}

/// Return `true` if a file occurrence with the given `id` exists.
pub fn exists_file_occurrence(
    conn: &Connection,
    id: &FileOccurrenceId,
) -> Result<bool, StorageError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM core_file_occurrences WHERE id = ?1",
        rusqlite::params![id.to_string_repr()],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Mark a file occurrence as indexed by recording the generation and timestamp.
///
/// Also updates `freshness_state` to `'CURRENT'`.
pub fn set_file_occurrence_indexed(
    conn: &Connection,
    id: &FileOccurrenceId,
    index_generation_id: &IndexGenerationId,
    last_indexed_at_us: i64,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE core_file_occurrences
         SET index_generation_id = ?2,
             last_indexed_at     = ?3,
             freshness_state     = 'CURRENT'
         WHERE id = ?1",
        rusqlite::params![
            id.to_string_repr(),
            index_generation_id.to_string_repr(),
            last_indexed_at_us,
        ],
    )?;
    Ok(())
}

/// Update the `secret_scan_state` and `secret_pattern_version` for a file occurrence.
pub fn set_secret_scan_state(
    conn: &Connection,
    id: &FileOccurrenceId,
    state: SecretScanState,
    pattern_version: i64,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE core_file_occurrences
         SET secret_scan_state      = ?2,
             secret_pattern_version = ?3
         WHERE id = ?1",
        rusqlite::params![
            id.to_string_repr(),
            state.as_str(),
            pattern_version,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use crate::repository::repository::upsert_repository;
    use crate::repository::source_revision::insert_source_revision;
    use attic_core::SourceType;
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
            "deadbeef",
            "2026-01-01T00:00:00Z",
            SourceType::Git,
        )
        .unwrap();
        (repo_id, rev_id)
    }

    #[test]
    fn upsert_file_identity_is_idempotent() {
        let conn = migrated_conn();
        let (repo_id, _) = seed_repo_and_revision(&conn);

        let fid = FileIdentityId::new_v4();
        assert!(!exists_file_identity(&conn, &fid).unwrap());

        upsert_file_identity(&conn, &fid, &repo_id, "blob:abc123").unwrap();
        assert!(exists_file_identity(&conn, &fid).unwrap());

        // Second upsert must be a no-op (INSERT OR IGNORE).
        upsert_file_identity(&conn, &fid, &repo_id, "blob:abc123").unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_file_identities WHERE id = ?1",
                rusqlite::params![fid.to_string_repr()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_file_occurrence_and_exists() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let fid = FileIdentityId::new_v4();
        upsert_file_identity(&conn, &fid, &repo_id, "blob:deadbeef").unwrap();

        let occ_id = FileOccurrenceId::new_v4();
        assert!(!exists_file_occurrence(&conn, &occ_id).unwrap());

        insert_file_occurrence(
            &conn,
            &NewFileOccurrence {
                id: &occ_id,
                file_identity_id: &fid,
                source_revision_id: &rev_id,
                index_generation_id: None,
                path: "src/main.rs",
                content_hash: "blake3:aabbcc",
                size_bytes: 1024,
                language: Some("rust"),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Pending,
                existence_state: ExistenceState::Present,
            },
        )
        .unwrap();

        assert!(exists_file_occurrence(&conn, &occ_id).unwrap());
    }

    #[test]
    fn duplicate_file_occurrence_insert_fails() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let fid = FileIdentityId::new_v4();
        upsert_file_identity(&conn, &fid, &repo_id, "blob:ff").unwrap();

        let occ_id = FileOccurrenceId::new_v4();
        let rec = NewFileOccurrence {
            id: &occ_id,
            file_identity_id: &fid,
            source_revision_id: &rev_id,
            index_generation_id: None,
            path: "lib.rs",
            content_hash: "blake3:ff",
            size_bytes: 512,
            language: Some("rust"),
            file_type: FileType::Rust,
            discovery_class: DiscoveryClass::Vcs,
            security_state: SecurityState::Pending,
            existence_state: ExistenceState::Present,
        };
        insert_file_occurrence(&conn, &rec).unwrap();

        let result = insert_file_occurrence(&conn, &rec);
        assert!(result.is_err(), "duplicate insert must fail");
    }

    #[test]
    fn set_secret_scan_state_updates_row() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);

        let fid = FileIdentityId::new_v4();
        upsert_file_identity(&conn, &fid, &repo_id, "blob:ee").unwrap();

        let occ_id = FileOccurrenceId::new_v4();
        insert_file_occurrence(
            &conn,
            &NewFileOccurrence {
                id: &occ_id,
                file_identity_id: &fid,
                source_revision_id: &rev_id,
                index_generation_id: None,
                path: "config.toml",
                content_hash: "blake3:ee",
                size_bytes: 256,
                language: None,
                file_type: FileType::Toml,
                discovery_class: DiscoveryClass::Filesystem,
                security_state: SecurityState::Pending,
                existence_state: ExistenceState::Present,
            },
        )
        .unwrap();

        set_secret_scan_state(&conn, &occ_id, SecretScanState::Clean, 2).unwrap();

        let (state, version): (String, i64) = conn
            .query_row(
                "SELECT secret_scan_state, secret_pattern_version
                 FROM core_file_occurrences WHERE id = ?1",
                rusqlite::params![occ_id.to_string_repr()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, SecretScanState::Clean.as_str());
        assert_eq!(version, 2);
    }
}
