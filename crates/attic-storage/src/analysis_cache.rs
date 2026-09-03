//! PR-7 — durable analysis-result cache backing full-index retry isolation.
//!
//! `attic-indexing` analyzes every discovered file in memory, then publishes
//! everything as ONE atomic writer-queue transaction at the very end (see
//! [`crate::submit_index_publication`]). If a full-index run aborts due to a
//! transient failure after most files already succeeded, this table lets the
//! next attempt skip re-analyzing any path whose content hash hasn't
//! changed, instead of redoing the entire repository's analysis.
//!
//! This module only stores/retrieves opaque JSON blobs — it has no knowledge
//! of `attic-indexing`'s internal types. Serializing/deserializing those
//! types is `attic-indexing`'s responsibility.

use std::collections::HashMap;

use rusqlite::Connection;

use attic_core::RepositoryId;

use crate::error::StorageError;

/// One cached analysis result for a single file. Cache reuse is permitted only
/// when the content hash, discovery-policy hash, security/analyzer versions,
/// and analysis options all match the current run.
#[derive(Debug, Clone)]
pub struct CachedFileAnalysis {
    /// Workspace-relative normalized path (forward slashes).
    pub repo_relative: String,
    /// BLAKE3 hex digest of the raw file bytes this result was computed from.
    pub content_hash: String,
    /// Cached `SecurityState` DB token (e.g. `"clean"`, `"flagged"`).
    pub security_state: String,
    /// Whether the cached analysis only covered a bounded partial sample.
    pub is_partial_scan: bool,
    /// Secret-detector pattern version active when this entry was computed.
    /// A mismatch against the current version at read time must be treated
    /// as a cache miss, never replayed — see module docs.
    pub secret_pattern_version: i64,
    /// Analyzer registry version active when this entry was computed. Same
    /// treatment as `secret_pattern_version`.
    pub analyzer_registry_version: String,
    /// JSON-serialized `Vec<PendingUnit>`-equivalent retrieval units.
    pub units_json: String,
    /// JSON-serialized structural capture; `None` when the file produced no
    /// structural intelligence.
    pub captured_json: Option<String>,
    /// Discovery-policy fingerprint used when this analysis was produced.
    pub discovery_policy_hash: String,

    /// Whether structural analysis was enabled for this cached result.
    pub structural: bool,

    /// Maximum retrieval units per file used when this analysis was produced.
    pub max_units_per_file: u64,
}

/// Bulk-load every cached analysis result for a repository in one query,
/// keyed by `repo_relative` path — the caller compares each entry's
/// `content_hash` against the current discovery hash to decide cache
/// hit/miss.
pub fn bulk_load_analysis_cache(
    conn: &Connection,
    repository_id: &RepositoryId,
) -> Result<HashMap<String, CachedFileAnalysis>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT repo_relative, content_hash, security_state, is_partial_scan,
                secret_pattern_version, analyzer_registry_version,
                discovery_policy_hash, structural, max_units_per_file,
                units_json, captured_json
           FROM index_analysis_cache
          WHERE repository_id = ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![repository_id.to_string_repr()], |r| {
        Ok(CachedFileAnalysis {
            repo_relative: r.get(0)?,
            content_hash: r.get(1)?,
            security_state: r.get(2)?,
            is_partial_scan: r.get::<_, i64>(3)? != 0,
            secret_pattern_version: r.get(4)?,
            analyzer_registry_version: r.get(5)?,
            discovery_policy_hash: r.get(6)?,
            structural: r.get::<_, i64>(7)? != 0,
            max_units_per_file: r.get::<_, i64>(8)? as u64,
            units_json: r.get(9)?,
            captured_json: r.get(10)?,
        })
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let entry = row?;
        out.insert(entry.repo_relative.clone(), entry);
    }
    Ok(out)
}

/// Persist (upsert) cache entries for every file successfully analyzed so
/// far in a run that is about to abort due to a transient failure — one
/// writer-queue submission, one transaction, many statements inside it
/// (mirrors the same "O(1) round trips, O(N) statements" shape as
/// [`crate::submit_index_publication`]).
///
/// A pre-existing row for the same `(repository_id, repo_relative)` is
/// overwritten unconditionally: the newest analysis result is always the
/// one worth keeping, regardless of what content hash it carries.
pub fn upsert_analysis_cache_entries(
    conn: &Connection,
    repository_id: &RepositoryId,
    entries: &[CachedFileAnalysis],
    now_us: i64,
) -> Result<(), StorageError> {
    let mut stmt = conn.prepare(
        "INSERT INTO index_analysis_cache
             (repository_id, repo_relative, content_hash, security_state,
              is_partial_scan, secret_pattern_version, analyzer_registry_version,
              discovery_policy_hash, structural, max_units_per_file,
              units_json, captured_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(repository_id, repo_relative) DO UPDATE SET
             content_hash              = excluded.content_hash,
             security_state            = excluded.security_state,
             is_partial_scan           = excluded.is_partial_scan,
             secret_pattern_version    = excluded.secret_pattern_version,
             analyzer_registry_version = excluded.analyzer_registry_version,
             discovery_policy_hash     = excluded.discovery_policy_hash,
             structural                = excluded.structural,
             max_units_per_file        = excluded.max_units_per_file,
             units_json                = excluded.units_json,
             captured_json             = excluded.captured_json,
             created_at                = excluded.created_at",
    )?;
    let repo_id_str = repository_id.to_string_repr();
    for entry in entries {
        stmt.execute(rusqlite::params![
            repo_id_str,
            entry.repo_relative,
            entry.content_hash,
            entry.security_state,
            entry.is_partial_scan as i64,
            entry.secret_pattern_version,
            entry.analyzer_registry_version,
            entry.discovery_policy_hash,
            entry.structural as i64,
            entry.max_units_per_file as i64,
            entry.units_json,
            entry.captured_json,
            now_us,
        ])?;
    }
    Ok(())
}

/// Delete every cached entry for a repository — called once a full-index
/// run publishes successfully, since the cache is no longer needed and
/// would otherwise grow unbounded.
pub fn clear_analysis_cache(
    conn: &Connection,
    repository_id: &RepositoryId,
) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM index_analysis_cache WHERE repository_id = ?1",
        rusqlite::params![repository_id.to_string_repr()],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::configure_connection;
    use crate::migration::run_migrations;
    use crate::repository::repository::upsert_repository;

    fn migrated_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    fn seed_repo(conn: &Connection) -> RepositoryId {
        let repo_id = RepositoryId::new_v4();
        upsert_repository(conn, &repo_id, "/repo", "test").unwrap();
        repo_id
    }

    fn entry(repo_relative: &str, content_hash: &str) -> CachedFileAnalysis {
        CachedFileAnalysis {
            repo_relative: repo_relative.to_string(),
            content_hash: content_hash.to_string(),
            security_state: "clean".to_string(),
            is_partial_scan: false,
            secret_pattern_version: 1,
            analyzer_registry_version: "0.0.0-test".to_string(),
            discovery_policy_hash: "policy-test".to_string(),
            structural: true,
            max_units_per_file: 512,
            units_json: "[]".to_string(),
            captured_json: None,
        }
    }

    #[test]
    fn upsert_then_bulk_load_round_trips() {
        let conn = migrated_conn();
        let repo_id = seed_repo(&conn);

        upsert_analysis_cache_entries(
            &conn,
            &repo_id,
            &[entry("a.rs", "hash-a"), entry("b.rs", "hash-b")],
            1000,
        )
        .unwrap();

        let loaded = bulk_load_analysis_cache(&conn, &repo_id).unwrap();
        assert_eq!(loaded.len(), 2);
        let a = loaded.get("a.rs").unwrap();
        assert_eq!(a.content_hash, "hash-a");
        assert_eq!(a.discovery_policy_hash, "policy-test");
        assert!(a.structural);
        assert_eq!(a.max_units_per_file, 512);
        let b = loaded.get("b.rs").unwrap();
        assert_eq!(b.content_hash, "hash-b");
    }

    #[test]
    fn upsert_overwrites_stale_entry_for_same_path() {
        let conn = migrated_conn();
        let repo_id = seed_repo(&conn);

        upsert_analysis_cache_entries(&conn, &repo_id, &[entry("a.rs", "old-hash")], 1000).unwrap();
        upsert_analysis_cache_entries(&conn, &repo_id, &[entry("a.rs", "new-hash")], 2000).unwrap();

        let loaded = bulk_load_analysis_cache(&conn, &repo_id).unwrap();
        assert_eq!(loaded.len(), 1, "must overwrite, not duplicate");
        assert_eq!(loaded.get("a.rs").unwrap().content_hash, "new-hash");
    }

    #[test]
    fn clear_removes_only_the_target_repository() {
        let conn = migrated_conn();
        let repo_a = seed_repo(&conn);
        let repo_b = RepositoryId::new_v4();
        upsert_repository(&conn, &repo_b, "/repo-b", "test-b").unwrap();

        upsert_analysis_cache_entries(&conn, &repo_a, &[entry("a.rs", "h")], 1000).unwrap();
        upsert_analysis_cache_entries(&conn, &repo_b, &[entry("b.rs", "h")], 1000).unwrap();

        clear_analysis_cache(&conn, &repo_a).unwrap();

        assert!(bulk_load_analysis_cache(&conn, &repo_a).unwrap().is_empty());
        assert_eq!(bulk_load_analysis_cache(&conn, &repo_b).unwrap().len(), 1);
    }

    #[test]
    fn bulk_load_empty_for_repository_with_no_cache_entries() {
        let conn = migrated_conn();
        let repo_id = seed_repo(&conn);
        assert!(
            bulk_load_analysis_cache(&conn, &repo_id)
                .unwrap()
                .is_empty()
        );
    }
}
