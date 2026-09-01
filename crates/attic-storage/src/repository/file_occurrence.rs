//! S4 — `core_file_identities` and `core_file_occurrences` CRUD.
//!
//! `core_file_identities` tracks the stable identity of a file across revisions.
//! `core_file_occurrences` tracks a file's presence and metadata within a specific
//! source revision.

use std::collections::HashMap;

use rusqlite::Connection;

use attic_core::{
    DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType, IndexGenerationId,
    RepositoryId, SecretScanState, SecurityState, SourceRevisionId,
};

use crate::error::StorageError;

/// Shared CTE resolving each path in a repository to its latest occurrence
/// row (by `rowid`), regardless of freshness/existence state. Every
/// "latest per path" query below joins against this so the dedup rule
/// lives in exactly one place.
const LATEST_OCCURRENCE_PER_PATH_CTE: &str = "WITH latest AS (
             SELECT fo.path AS p, MAX(fo.rowid) AS m
               FROM core_file_occurrences fo
               JOIN core_file_identities fi ON fo.file_identity_id = fi.id
              WHERE fi.repository_id = ?1
              GROUP BY fo.path
         )";

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
pub fn exists_file_identity(conn: &Connection, id: &FileIdentityId) -> Result<bool, StorageError> {
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

/// Insert an occurrence and explicitly set its freshness in the same ambient
/// transaction. Used by coordinated publication for tombstones, which must
/// be born `INVALID` rather than briefly becoming `CURRENT`.
pub fn insert_file_occurrence_with_freshness(
    conn: &Connection,
    rec: &NewFileOccurrence<'_>,
    freshness: attic_core::FreshnessState,
) -> Result<(), StorageError> {
    insert_file_occurrence(conn, rec)?;
    conn.execute(
        "UPDATE core_file_occurrences SET freshness_state = ?2 WHERE id = ?1",
        rusqlite::params![rec.id.to_string_repr(), freshness.as_str()],
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

/// Look up a file identity ID by its `stable_id_basis`, returning `None` if absent.
///
/// Used by `attic-indexing` to reuse the same UUID across reindex runs.
pub fn lookup_file_identity_by_basis(
    conn: &Connection,
    stable_id_basis: &str,
) -> Result<Option<FileIdentityId>, StorageError> {
    use rusqlite::OptionalExtension;
    let id_str: Option<String> = conn
        .query_row(
            "SELECT id FROM core_file_identities WHERE stable_id_basis = ?1 LIMIT 1",
            rusqlite::params![stable_id_basis],
            |r| r.get(0),
        )
        .optional()?;
    match id_str {
        Some(s) => {
            let id = s.parse::<FileIdentityId>().map_err(|e| {
                StorageError::Domain(attic_core::CoreError::UnknownVariant {
                    type_name: "FileIdentityId",
                    value: e.to_string(),
                })
            })?;
            Ok(Some(id))
        }
        None => Ok(None),
    }
}

/// Bulk-load `stable_id_basis -> file_identity_id` for every identity
/// already recorded in the given repository, in one query.
///
/// Used by the full-index loop to resolve identity reuse in memory instead
/// of one [`lookup_file_identity_by_basis`] round trip per discovered file
/// (PR-5: bounded DB-query behavior at scale).
pub fn bulk_file_identities_for_repository(
    conn: &Connection,
    repository_id: &RepositoryId,
) -> Result<HashMap<String, FileIdentityId>, StorageError> {
    let mut stmt = conn
        .prepare("SELECT stable_id_basis, id FROM core_file_identities WHERE repository_id = ?1")?;
    let rows = stmt.query_map(rusqlite::params![repository_id.to_string_repr()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (basis, id_str) = row?;
        let id = id_str.parse::<FileIdentityId>().map_err(|e| {
            StorageError::Domain(attic_core::CoreError::UnknownVariant {
                type_name: "FileIdentityId",
                value: e.to_string(),
            })
        })?;
        out.insert(basis, id);
    }
    Ok(out)
}

/// Bulk-load `repo_relative_path -> latest file_occurrence_id` for every
/// path in the given repository, in one query.
///
/// Mirrors [`lookup_latest_file_occurrence_for_path`]'s semantics (latest
/// occurrence by rowid, regardless of freshness/existence state) but
/// resolves every path at once instead of one round trip per discovered
/// file (PR-5: bounded DB-query behavior at scale).
pub fn bulk_latest_occurrence_ids_for_repository(
    conn: &Connection,
    repository_id: &RepositoryId,
) -> Result<HashMap<String, String>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "{LATEST_OCCURRENCE_PER_PATH_CTE}
         SELECT fo.path, fo.id
           FROM core_file_occurrences fo
           JOIN latest ON fo.path = latest.p AND fo.rowid = latest.m"
    ))?;
    let rows = stmt.query_map(rusqlite::params![repository_id.to_string_repr()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (path, id) = row?;
        out.insert(path, id);
    }
    Ok(out)
}

/// Look up the most recent file occurrence ID for a given repository and path.
///
/// Returns the `FileOccurrenceId` string of the latest occurrence (by rowid),
/// or `None` if this path has never been indexed in the given repository.
///
/// Used by `attic-indexing` to find the previous occurrence for unit deletion.
pub fn lookup_latest_file_occurrence_for_path(
    conn: &Connection,
    repository_id: &RepositoryId,
    path: &str,
) -> Result<Option<String>, StorageError> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT fo.id
           FROM core_file_occurrences fo
           JOIN core_file_identities  fi ON fo.file_identity_id = fi.id
          WHERE fi.repository_id = ?1 AND fo.path = ?2
          ORDER BY fo.rowid DESC
          LIMIT 1",
        rusqlite::params![repository_id.to_string_repr(), path],
        |r| r.get(0),
    )
    .optional()
    .map_err(StorageError::from)
}

/// Verified state snapshot of the latest occurrence for one repo+path.
///
/// Phase 2 change detection compares this against actual filesystem state;
/// `content_hash` must never be inferred from timestamps alone.
#[derive(Debug, Clone, PartialEq)]
pub struct OccurrenceSnapshot {
    /// `core_file_occurrences.id` (UUID string) of the latest row.
    pub id: String,
    /// Owning identity (UUID string).
    pub file_identity_id: String,
    /// BLAKE3 hex of raw bytes at capture time.
    pub content_hash: String,
    /// Freshness value (`CURRENT | STALE | UNKNOWN | INVALID | PENDING_REFRESH`).
    pub freshness_state: String,
    /// Existence value (`PRESENT | DELETED`).
    pub existence_state: String,
}

/// Read the [`OccurrenceSnapshot`] for the latest occurrence at repo+path.
pub fn lookup_occurrence_snapshot(
    conn: &Connection,
    repository_id: &RepositoryId,
    path: &str,
) -> Result<Option<OccurrenceSnapshot>, StorageError> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT fo.id, fo.file_identity_id, fo.content_hash,
                fo.freshness_state, fo.existence_state
           FROM core_file_occurrences fo
           JOIN core_file_identities  fi ON fo.file_identity_id = fi.id
          WHERE fi.repository_id = ?1 AND fo.path = ?2
          ORDER BY fo.rowid DESC
          LIMIT 1",
        rusqlite::params![repository_id.to_string_repr(), path],
        |r| {
            Ok(OccurrenceSnapshot {
                id: r.get(0)?,
                file_identity_id: r.get(1)?,
                content_hash: r.get(2)?,
                freshness_state: r.get(3)?,
                existence_state: r.get(4)?,
            })
        },
    )
    .optional()
    .map_err(StorageError::from)
}

/// Read `(path, content_hash)` for every non-deleted occurrence whose
/// freshness is trusted (`CURRENT`) in the given repository, deduplicated to
/// the latest row per path.
///
/// This is the incremental-manifest basis: verified hashes are reused without
/// re-reading unchanged files; anything not CURRENT is re-verified separately.
pub fn current_path_hashes_for_repository(
    conn: &Connection,
    repository_id: &RepositoryId,
) -> Result<Vec<(String, String)>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "{LATEST_OCCURRENCE_PER_PATH_CTE}
         SELECT fo.path, fo.content_hash
           FROM core_file_occurrences fo
           JOIN latest ON fo.path = latest.p AND fo.rowid = latest.m
          WHERE fo.freshness_state = 'CURRENT'
            AND fo.existence_state != 'deleted'"
    ))?;
    let rows = stmt.query_map(rusqlite::params![repository_id.to_string_repr()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Latest-per-path occurrence paths that are still `present` (not tombstoned)
/// in the given repository, regardless of `freshness_state`.
///
/// Used by full/authoritative indexing to diff "previously active paths"
/// against the current discovery run so paths that disappeared (deleted from
/// disk, or newly excluded/unsupported) can be tombstoned instead of being
/// left as stale searchable content forever. `STALE`/`PENDING_REFRESH` paths
/// are intentionally included (unlike [`current_path_hashes_for_repository`],
/// which only trusts `CURRENT` rows as a hashing baseline) — they are still
/// "active" from a tombstone-diff point of view.
pub fn latest_active_paths_for_repository(
    conn: &Connection,
    repository_id: &RepositoryId,
) -> Result<Vec<String>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "{LATEST_OCCURRENCE_PER_PATH_CTE}
         SELECT fo.path
           FROM core_file_occurrences fo
           JOIN latest ON fo.path = latest.p AND fo.rowid = latest.m
          WHERE fo.existence_state != 'deleted'"
    ))?;
    let rows = stmt.query_map(rusqlite::params![repository_id.to_string_repr()], |r| {
        r.get::<_, String>(0)
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Latest-per-path `(path, file_type)` for every occurrence that is both
/// `present` and `CURRENT` in the given repository — i.e. the file set of the
/// current generation, never a superseded one. Optionally filtered to a
/// single `file_type`.
///
/// This is the read backing the MCP `repo_map` tool: directories are never
/// persisted as their own entity, so a directory tree is derived at read
/// time from these current-generation active file paths.
pub fn current_files_for_repo_map(
    conn: &Connection,
    repository_id: &RepositoryId,
    file_type: Option<&str>,
) -> Result<Vec<(String, String)>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "{LATEST_OCCURRENCE_PER_PATH_CTE}
         SELECT fo.path, fo.file_type
           FROM core_file_occurrences fo
           JOIN latest ON fo.path = latest.p AND fo.rowid = latest.m
          WHERE fo.freshness_state = 'CURRENT'
            AND fo.existence_state != 'deleted'
            AND (?2 IS NULL OR fo.file_type = ?2)"
    ))?;
    let rows = stmt.query_map(
        rusqlite::params![repository_id.to_string_repr(), file_type],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
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
        rusqlite::params![id.to_string_repr(), state.as_str(), pattern_version,],
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

    // -----------------------------------------------------------------------
    // Bulk preload functions (PR-5) — must agree exactly with the equivalent
    // per-path lookups they replace in the full-index hot loop.
    // -----------------------------------------------------------------------

    #[test]
    fn bulk_file_identities_matches_single_lookup_for_every_basis() {
        let conn = migrated_conn();
        let (repo_id, _) = seed_repo_and_revision(&conn);

        let fid_a = FileIdentityId::new_v4();
        let fid_b = FileIdentityId::new_v4();
        upsert_file_identity(&conn, &fid_a, &repo_id, "repo/a.rs").unwrap();
        upsert_file_identity(&conn, &fid_b, &repo_id, "repo/b.rs").unwrap();

        let bulk = bulk_file_identities_for_repository(&conn, &repo_id).unwrap();
        assert_eq!(bulk.len(), 2);

        for basis in ["repo/a.rs", "repo/b.rs"] {
            let single = lookup_file_identity_by_basis(&conn, basis).unwrap();
            assert_eq!(
                bulk.get(basis).copied(),
                single,
                "bulk and single-lookup identity must agree for '{basis}'"
            );
        }
    }

    #[test]
    fn bulk_file_identities_is_scoped_to_its_repository() {
        let conn = migrated_conn();
        let (repo_a, _) = seed_repo_and_revision(&conn);
        let repo_b = RepositoryId::new_v4();
        upsert_repository(&conn, &repo_b, "/repo-b", "test-b").unwrap();

        upsert_file_identity(&conn, &FileIdentityId::new_v4(), &repo_a, "shared/path.rs").unwrap();
        upsert_file_identity(&conn, &FileIdentityId::new_v4(), &repo_b, "shared/path.rs").unwrap();

        let bulk_a = bulk_file_identities_for_repository(&conn, &repo_a).unwrap();
        let bulk_b = bulk_file_identities_for_repository(&conn, &repo_b).unwrap();
        assert_eq!(bulk_a.len(), 1);
        assert_eq!(bulk_b.len(), 1);
        assert_ne!(
            bulk_a.get("shared/path.rs"),
            bulk_b.get("shared/path.rs"),
            "identical stable_id_basis in different repositories must resolve to different identities"
        );
    }

    #[test]
    fn bulk_latest_occurrence_ids_matches_single_lookup_and_prefers_latest_rowid() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);
        let fid = FileIdentityId::new_v4();
        upsert_file_identity(&conn, &fid, &repo_id, "blob:main").unwrap();

        // Two occurrences for the same path (simulating a reindex run) — the
        // bulk map must resolve to the SAME latest-by-rowid row as the
        // single lookup, not the first one inserted.
        let occ_old = FileOccurrenceId::new_v4();
        insert_file_occurrence(
            &conn,
            &NewFileOccurrence {
                id: &occ_old,
                file_identity_id: &fid,
                source_revision_id: &rev_id,
                index_generation_id: None,
                path: "src/main.rs",
                content_hash: "blake3:old",
                size_bytes: 10,
                language: Some("rust"),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Pending,
                existence_state: ExistenceState::Present,
            },
        )
        .unwrap();
        let occ_new = FileOccurrenceId::new_v4();
        insert_file_occurrence(
            &conn,
            &NewFileOccurrence {
                id: &occ_new,
                file_identity_id: &fid,
                source_revision_id: &rev_id,
                index_generation_id: None,
                path: "src/main.rs",
                content_hash: "blake3:new",
                size_bytes: 20,
                language: Some("rust"),
                file_type: FileType::Rust,
                discovery_class: DiscoveryClass::Vcs,
                security_state: SecurityState::Pending,
                existence_state: ExistenceState::Present,
            },
        )
        .unwrap();

        let bulk = bulk_latest_occurrence_ids_for_repository(&conn, &repo_id).unwrap();
        let single = lookup_latest_file_occurrence_for_path(&conn, &repo_id, "src/main.rs")
            .unwrap()
            .unwrap();

        assert_eq!(bulk.get("src/main.rs").cloned(), Some(single.clone()));
        assert_eq!(
            single,
            occ_new.to_string_repr(),
            "both bulk and single lookup must resolve to the most recently inserted occurrence"
        );
    }

    #[test]
    fn bulk_latest_occurrence_ids_empty_for_repository_with_no_occurrences() {
        let conn = migrated_conn();
        let (repo_id, _) = seed_repo_and_revision(&conn);
        let bulk = bulk_latest_occurrence_ids_for_repository(&conn, &repo_id).unwrap();
        assert!(bulk.is_empty());
    }

    #[test]
    fn repo_map_latest_path_never_resurrects_deleted_history() {
        let conn = migrated_conn();
        let (repo_id, rev_id) = seed_repo_and_revision(&conn);
        let fid = FileIdentityId::new_v4();
        upsert_file_identity(&conn, &fid, &repo_id, "repo/foo.rs").unwrap();

        let insert = |id: &FileOccurrenceId,
                      hash: &str,
                      existence: ExistenceState,
                      freshness: attic_core::FreshnessState| {
            insert_file_occurrence_with_freshness(
                &conn,
                &NewFileOccurrence {
                    id,
                    file_identity_id: &fid,
                    source_revision_id: &rev_id,
                    index_generation_id: None,
                    path: "foo.rs",
                    content_hash: hash,
                    size_bytes: 1,
                    language: Some("rust"),
                    file_type: FileType::Rust,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: SecurityState::Clean,
                    existence_state: existence,
                },
                freshness,
            )
            .unwrap();
        };

        let a = FileOccurrenceId::new_v4();
        insert(
            &a,
            "a",
            ExistenceState::Present,
            attic_core::FreshnessState::Current,
        );
        assert_eq!(
            current_files_for_repo_map(&conn, &repo_id, None)
                .unwrap()
                .len(),
            1
        );

        let b = FileOccurrenceId::new_v4();
        insert(
            &b,
            "b",
            ExistenceState::Present,
            attic_core::FreshnessState::Current,
        );
        let current = current_files_for_repo_map(&conn, &repo_id, None).unwrap();
        assert_eq!(
            current,
            vec![("foo.rs".to_owned(), FileType::Rust.as_str().to_owned())]
        );

        let deleted = FileOccurrenceId::new_v4();
        insert(
            &deleted,
            "b",
            ExistenceState::Deleted,
            attic_core::FreshnessState::Invalid,
        );
        assert!(
            current_files_for_repo_map(&conn, &repo_id, None)
                .unwrap()
                .is_empty(),
            "latest deleted occurrence must suppress, never resurrect, older CURRENT rows"
        );

        let recreated = FileOccurrenceId::new_v4();
        insert(
            &recreated,
            "c",
            ExistenceState::Present,
            attic_core::FreshnessState::Current,
        );
        assert_eq!(
            current_files_for_repo_map(&conn, &repo_id, None)
                .unwrap()
                .len(),
            1
        );
    }
}
