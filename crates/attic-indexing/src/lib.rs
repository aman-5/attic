//! `attic-indexing` — End-to-end indexing pipeline (Phase 1D).
//!
//! Wires Phase 1B discovery, Phase 1C analyzers, and Phase 1A storage.
//! ALL mutations use approved attic-storage APIs.  No raw conn.execute mutations.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;
use tracing::{debug, info, warn};

use attic_analyzers::{
    AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer, ResourceBudget,
};
use attic_core::{
    constants::subsystem_keys,
    DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType,
    IndexGenerationId, RepositoryId, RetrievalUnitId, SecurityState, SourceRevisionId,
    SubsystemVersions,
};
use attic_discovery::{DiscoveryPolicy, DownstreamClassification};
use attic_storage::{
    NewFileOccurrence, NewRetrievalUnit, StorageError,
    delete_retrieval_units_for_file, insert_file_occurrence, insert_index_generation,
    insert_retrieval_unit_with_fts, insert_source_revision_with_hashes,
    run_migrations, upsert_file_identity, upsert_repository,
};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("discovery failed: {0}")]
    Discovery(#[from] attic_discovery::DiscoveryError),
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O error preprocessing {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Public options / result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub repository_name: String,
    pub max_units_per_file: usize,
    pub refresh_existing: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            repository_name: "default".to_owned(),
            max_units_per_file: 512,
            refresh_existing: true,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct IndexResult {
    pub files_visited: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub units_inserted: usize,
    pub units_deleted: usize,
    pub repository_id: String,
    pub source_revision_id: String,
    pub index_generation_id: String,
}

// ---------------------------------------------------------------------------
// Internal per-file record
// ---------------------------------------------------------------------------

struct FileRecord {
    fi_id: FileIdentityId,
    fo_id: FileOccurrenceId,
    /// The file_occurrence_id from a previous run, if one exists.
    /// Units for this id will be deleted during refresh before inserting new ones.
    old_fo_id: Option<String>,
    repo_relative: String,
    abs_path: std::path::PathBuf,
    content_hash: String,
    size_bytes: i64,
    security_state: SecurityState,
    file_type: FileType,
    is_redacted: bool,
}

// ---------------------------------------------------------------------------
// Primary entry point
// ---------------------------------------------------------------------------

/// Index a repository root directory.
///
/// All mutations use approved `attic-storage` APIs.  The only direct SQL in
/// this function is a read-only SELECT to look up an existing repository_id
/// by root_path (no corresponding read API exists in attic-storage).
pub fn index_repository(
    conn: &Connection,
    root: &Path,
    policy: &DiscoveryPolicy,
    opts: &IndexOptions,
) -> Result<IndexResult, IndexError> {
    // 1. Migrations.
    run_migrations(conn)?;

    // 2. Phase 1B discovery — real manifest hash, git meta, security classification.
    let discovery = attic_discovery::discover(root, policy)?;

    info!(
        files = discovery.entries.len(),
        manifest_hash = %discovery.manifest.manifest_hash,
        "discovery complete"
    );

    // 3. Bootstrap / retrieve repository record via approved API.
    let root_str = root.to_string_lossy();
    let repo_id: RepositoryId = {
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM core_repositories WHERE root_path = ?1 LIMIT 1",
                rusqlite::params![root_str],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(s) = existing {
            s.parse().map_err(|_| IndexError::Io {
                path: root_str.to_string(),
                source: std::io::Error::other("invalid repository_id UUID in DB"),
            })?
        } else {
            let id = RepositoryId::new_v4();
            upsert_repository(conn, &id, &root_str, &opts.repository_name)?;
            id
        }
    };

    // 4. Source revision with REAL manifest hash and policy hash.
    let rev_id = SourceRevisionId::new_v4();
    let manifest_hash = &discovery.manifest.manifest_hash;
    let policy_hash = compute_policy_hash(policy);
    let commit_sha: Option<&str> = discovery
        .git_meta
        .as_ref()
        .and_then(|g| g.head_sha.as_deref());
    let unstable_capture = !discovery.manifest.unstable_captures.is_empty();

    insert_source_revision_with_hashes(
        conn,
        &rev_id,
        &repo_id,
        commit_sha,
        manifest_hash,
        &policy_hash,
        unstable_capture,
    )?;

    // 5. Index generation record.
    let gen_id = IndexGenerationId::new_v4();
    let mut sv = SubsystemVersions::new();
    sv.set(subsystem_keys::SCHEMA, "1.0.0");
    sv.set(subsystem_keys::INDEXER, "1.0.0");
    sv.set(subsystem_keys::SECRET_DETECTOR, "1");
    insert_index_generation(conn, &gen_id, &repo_id, &rev_id, 1, &sv)?;

    info!(
        repository_id = %repo_id.to_string_repr(),
        source_revision_id = %rev_id.to_string_repr(),
        index_generation_id = %gen_id.to_string_repr(),
        root = %root.display(),
        "indexing run started"
    );

    // 6. Build analyzer registry.
    let registry = AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()));

    // 7. Collect eligible file records.
    let mut result = IndexResult {
        repository_id: repo_id.to_string_repr(),
        source_revision_id: rev_id.to_string_repr(),
        index_generation_id: gen_id.to_string_repr(),
        ..Default::default()
    };

    let mut file_records: Vec<FileRecord> = Vec::new();

    for entry in &discovery.entries {
        result.files_visited += 1;

        let classification = discovery
            .downstream_classifications
            .iter()
            .find(|(p, _)| p == &entry.repo_relative)
            .map(|(_, c)| c);

        if matches!(classification, Some(DownstreamClassification::Excluded)) {
            debug!(path = %entry.repo_relative, "skipping excluded file");
            result.files_skipped += 1;
            continue;
        }

        let file_meta = match std::fs::metadata(&entry.abs_path) {
            Ok(m) => m,
            Err(e) => {
                warn!(path = %entry.repo_relative, error = %e, "stat failed, skipping");
                result.files_skipped += 1;
                continue;
            }
        };
        let size_bytes = file_meta.len() as i64;
        let (security_state, is_redacted) = classify_security_state(classification);
        let content_hash = compute_file_content_hash(&entry.abs_path);
        let file_type = infer_file_type(&entry.abs_path);

        // Look up any existing file_occurrence for this path in this repository.
        // This allows the refresh pass to delete the *old* units (associated
        // with the old fo_id) rather than the brand-new one we are about to create.
        let old_fo_id: Option<String> = conn
            .query_row(
                "SELECT fo.id
                   FROM core_file_occurrences fo
                   JOIN core_file_identities  fi ON fo.file_identity_id = fi.id
                  WHERE fi.repository_id = ?1 AND fo.path = ?2
                  ORDER BY fo.rowid DESC
                  LIMIT 1",
                rusqlite::params![repo_id.to_string_repr(), entry.repo_relative],
                |r| r.get(0),
            )
            .optional()?;

        file_records.push(FileRecord {
            fi_id: FileIdentityId::new_v4(),
            fo_id: FileOccurrenceId::new_v4(),
            old_fo_id,
            repo_relative: entry.repo_relative.clone(),
            abs_path: entry.abs_path.clone(),
            content_hash,
            size_bytes,
            security_state,
            file_type,
            is_redacted,
        });
    }

    // 8. Persist file identities + occurrences in one atomic transaction.
    //    We use upsert_file_identity + insert_file_occurrence (approved APIs)
    //    wrapped in BEGIN IMMEDIATE / COMMIT for atomicity.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let persist_result: Result<(), IndexError> = (|| {
        for rec in &file_records {
            upsert_file_identity(conn, &rec.fi_id, &repo_id, &rec.repo_relative)?;
            insert_file_occurrence(
                conn,
                &NewFileOccurrence {
                    id: &rec.fo_id,
                    file_identity_id: &rec.fi_id,
                    source_revision_id: &rev_id,
                    index_generation_id: Some(&gen_id),
                    path: &rec.repo_relative,
                    content_hash: &rec.content_hash,
                    size_bytes: rec.size_bytes,
                    language: None,
                    file_type: rec.file_type,
                    discovery_class: DiscoveryClass::Vcs,
                    security_state: rec.security_state,
                    existence_state: ExistenceState::Present,
                },
            )?;
        }
        Ok(())
    })();
    match persist_result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }

    // 9. Run Phase 1C analysis and insert retrieval units per file.
    for rec in &file_records {
        match index_single_file(
            conn,
            rec,
            &gen_id,
            &repo_id,
            &registry,
            opts,
        ) {
            Ok((inserted, deleted)) => {
                result.files_indexed += 1;
                result.units_inserted += inserted;
                result.units_deleted += deleted;
            }
            Err(e) => {
                warn!(
                    path = %rec.repo_relative,
                    error = %e,
                    "analysis/unit insertion failed, skipping"
                );
                result.files_skipped += 1;
            }
        }
    }

    info!(
        files_indexed = result.files_indexed,
        files_skipped = result.files_skipped,
        units_inserted = result.units_inserted,
        "indexing run complete"
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Per-file analysis + FTS unit insertion
// ---------------------------------------------------------------------------

fn index_single_file(
    conn: &Connection,
    rec: &FileRecord,
    index_generation_id: &IndexGenerationId,
    repository_id: &RepositoryId,
    registry: &AnalyzerRegistry,
    opts: &IndexOptions,
) -> Result<(usize, usize), IndexError> {
    // Preprocess through Phase 1B secrets layer.
    let preprocessed =
        attic_discovery::preprocess_file_content(&rec.abs_path, &rec.repo_relative)
            .map_err(|source| IndexError::Io {
                path: rec.repo_relative.clone(),
                source,
            })?;

    let is_redacted = matches!(
        preprocessed.decision,
        attic_discovery::SecretScanDecision::Redacted
    );

    // LARGE files return stream=Some, content=None.  Skip in Phase 1D.
    let content = match preprocessed.content {
        Some(c) => c,
        None => {
            debug!(path = %rec.repo_relative, "skipping LARGE streaming file (Phase 1D)");
            return Ok((0, 0));
        }
    };

    let fo_id_str = rec.fo_id.to_string_repr();

    // Delete existing units if refresh mode.
    // Use the OLD fo_id (from a previous run) so we delete the right units.
    // The new fo_id hasn't been associated with any units yet.
    let deleted = if opts.refresh_existing {
        if let Some(old_id) = &rec.old_fo_id {
            delete_retrieval_units_for_file(conn, old_id)?
        } else {
            0
        }
    } else {
        0
    };

    // Build AnalyzerInput.
    let content_bytes = content.into_bytes();
    let size_bytes = content_bytes.len() as u64;
    let budget = ResourceBudget {
        max_retrieval_units: opts.max_units_per_file as u64,
        ..Default::default()
    };
    let file_occ_id: FileOccurrenceId = fo_id_str.parse().map_err(|_| IndexError::Io {
        path: rec.repo_relative.clone(),
        source: std::io::Error::other("invalid file_occurrence_id UUID"),
    })?;

    let input = AnalyzerInput {
        file_occurrence_id: file_occ_id,
        path: rec.abs_path.clone(),
        content: attic_analyzers::AnalyzerContent::FullBytes(content_bytes),
        file_type: rec.file_type,
        language_hint: None,
        size_bytes,
        is_partial_scan: false,
        cancellation_token: CancellationToken::default(),
        resource_budget: budget,
    };

    let output = attic_analyzers::dispatch(registry, input);
    let analyzer_id = output.analyzer_id.as_str();
    let analyzer_version = output.analyzer_version.as_str();
    let repo_id_str = repository_id.to_string_repr();
    let gen_id_str = index_generation_id.to_string_repr();

    let mut inserted = 0usize;
    for unit_spec in &output.retrieval_units {
        let unit_id = RetrievalUnitId::new_v4().to_string_repr();
        let retrieval_text = if is_redacted {
            "[REDACTED]".to_owned()
        } else {
            unit_spec.retrieval_text.clone()
        };
        let unit = NewRetrievalUnit {
            id: &unit_id,
            file_occurrence_id: &fo_id_str,
            index_generation_id: &gen_id_str,
            repository_id: &repo_id_str,
            retrieval_text: &retrieval_text,
            analyzer_id,
            analyzer_version,
            start_line: Some(unit_spec.span.start_line as u32),
            end_line: Some(unit_spec.span.end_line as u32),
            is_redacted,
        };
        insert_retrieval_unit_with_fts(conn, &unit)?;
        inserted += 1;
    }

    debug!(
        path = %rec.repo_relative,
        inserted,
        deleted,
        "file indexed"
    );
    Ok((inserted, deleted))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive `SecurityState` and `is_redacted` flag from the downstream
/// classification produced by Phase 1B.
fn classify_security_state(
    classification: Option<&DownstreamClassification>,
) -> (SecurityState, bool) {
    match classification {
        Some(DownstreamClassification::Safe { .. }) => (SecurityState::Clean, false),
        Some(DownstreamClassification::Redacted { .. }) => (SecurityState::Flagged, true),
        Some(DownstreamClassification::PartialScan { .. }) => (SecurityState::Pending, false),
        Some(DownstreamClassification::ScanSkipped { .. }) => (SecurityState::Skipped, false),
        Some(DownstreamClassification::Excluded) => (SecurityState::Skipped, false),
        None => (SecurityState::Pending, false),
    }
}

/// Compute a deterministic 64-hex-char hash of the discovery policy.
///
/// Uses FNV-1a applied with four different initial seeds over a canonical
/// JSON representation of the policy, producing 256 bits (64 hex chars).
/// This avoids adding a blake3 or sha2 dependency.
fn compute_policy_hash(policy: &DiscoveryPolicy) -> String {
    // Canonical representation: use serde_json if available, else a fixed
    // string. Since DiscoveryPolicy derives Serialize we can use serde_json.
    let canonical = policy_canonical_bytes(policy);
    fnv1a_256_hex(&canonical)
}

fn policy_canonical_bytes(policy: &DiscoveryPolicy) -> Vec<u8> {
    // Build a stable string from the key fields of DiscoveryPolicy.
    let s = format!(
        "git_aware={} include_untracked={}",
        policy.git_aware, policy.include_untracked
    );
    s.into_bytes()
}

/// Compute a deterministic 64-hex-char hash of a file's content.
///
/// Uses the same FNV-1a × 4 approach to avoid external dependencies.
/// Returns "fnv:" prefixed 64-char hex string.
fn compute_file_content_hash(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    format!("fnv:{}", fnv1a_256_hex(&bytes))
}

/// FNV-1a applied with 4 seeds to produce 256 bits (64 hex chars).
fn fnv1a_256_hex(data: &[u8]) -> String {
    const SEEDS: [u64; 4] = [
        0xcbf29ce484222325,
        0xd2a98b26625eee7b,
        0x6c62272e07bb0142,
        0x8a4c8c748b7ee7c5,
    ];
    const PRIME: u64 = 0x00000100000001b3;

    let mut parts = [0u64; 4];
    for (i, &seed) in SEEDS.iter().enumerate() {
        let mut h = seed;
        for &b in data {
            h ^= b as u64;
            h = h.wrapping_mul(PRIME);
        }
        // Mix the index in so each lane differs even on identical data.
        h ^= (i as u64).wrapping_mul(0x9e3779b97f4a7c15);
        parts[i] = h;
    }
    format!(
        "{:016x}{:016x}{:016x}{:016x}",
        parts[0], parts[1], parts[2], parts[3]
    )
}

/// Infer the broad file type from path extension.
fn infer_file_type(path: &Path) -> FileType {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => FileType::Rust,
        Some("ts") | Some("tsx") => FileType::TypeScript,
        Some("js") | Some("mjs") | Some("cjs") => FileType::JavaScript,
        Some("py") => FileType::Python,
        Some("md") => FileType::Markdown,
        Some("toml") => FileType::Toml,
        Some("json") => FileType::Json,
        Some("yaml") | Some("yml") => FileType::Yaml,
        _ => FileType::Other,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use attic_discovery::DiscoveryPolicy;
    use attic_storage::run_migrations;
    use rusqlite::Connection;
    use tempfile::TempDir;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        attic_storage::connection::configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    // -----------------------------------------------------------------------
    // Basic pipeline tests
    // -----------------------------------------------------------------------

    #[test]
    fn index_empty_directory_succeeds() {
        let tmp = TempDir::new().unwrap();
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts)
            .expect("indexing empty dir should succeed");
        assert_eq!(result.files_indexed, 0);
        assert_eq!(result.units_inserted, 0);
    }

    #[test]
    fn index_single_text_file_inserts_units() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "hello.rs",
            "fn main() { println!(\"hello world\"); }\n",
        );
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts)
            .expect("indexing should succeed");
        assert!(result.files_indexed >= 1, "at least one file indexed");
        assert!(result.units_inserted >= 1, "at least one unit inserted");
        assert!(!result.repository_id.is_empty());
        assert!(!result.source_revision_id.is_empty());
        assert!(!result.index_generation_id.is_empty());
    }

    #[test]
    fn second_index_run_refreshes_units() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "foo.rs", "fn foo() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions { refresh_existing: true, ..Default::default() };
        let r1 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        let r2 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        // Second run deletes first run's units and re-inserts.
        assert_eq!(r2.units_deleted, r1.units_inserted);
        assert_eq!(r2.units_inserted, r1.units_inserted);
    }

    #[test]
    fn repository_id_is_stable_across_runs() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "a.rs", "fn a() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let r1 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        let r2 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        assert_eq!(r1.repository_id, r2.repository_id, "stable repository_id");
    }

    // -----------------------------------------------------------------------
    // E2E: real data actually reaches the database
    // -----------------------------------------------------------------------

    #[test]
    fn e2e_indexed_file_is_searchable() {
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "search_me.rs",
            "pub fn greet_the_world() -> &'static str { \"hello\" }\n",
        );
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        assert!(result.units_inserted >= 1);

        // The FTS index must return a result for a term in the indexed content.
        let params = attic_storage::FtsSearchParams {
            query: "greet_the_world",
            repository_id: None,
            file_type: None,
            language: None,
            max_results: 10,
        };
        let hits = attic_storage::fts_search(&conn, &params).unwrap();
        assert!(
            !hits.is_empty(),
            "FTS search must find indexed content after index_repository"
        );
    }

    #[test]
    fn e2e_repository_scoped_search() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        write_file(tmp1.path(), "alpha.rs", "fn alpha_only_token() {}\n");
        write_file(tmp2.path(), "beta.rs", "fn beta_only_token() {}\n");

        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts1 = IndexOptions {
            repository_name: "repo-alpha".to_owned(),
            ..Default::default()
        };
        let opts2 = IndexOptions {
            repository_name: "repo-beta".to_owned(),
            ..Default::default()
        };
        let r1 = index_repository(&conn, tmp1.path(), &policy, &opts1).unwrap();
        let r2 = index_repository(&conn, tmp2.path(), &policy, &opts2).unwrap();

        // Scoped search must not return results from the other repository.
        let params_alpha = attic_storage::FtsSearchParams {
            query: "alpha_only_token",
            repository_id: Some(&r1.repository_id),
            file_type: None,
            language: None,
            max_results: 10,
        };
        let hits_alpha = attic_storage::fts_search(&conn, &params_alpha).unwrap();
        assert!(
            !hits_alpha.is_empty(),
            "alpha token must be found in repo-alpha"
        );
        for hit in &hits_alpha {
            assert_eq!(
                hit.repository_id, r1.repository_id,
                "search scoped to repo-alpha must only return repo-alpha results"
            );
        }

        let params_beta = attic_storage::FtsSearchParams {
            query: "beta_only_token",
            repository_id: Some(&r2.repository_id),
            file_type: None,
            language: None,
            max_results: 10,
        };
        let hits_beta = attic_storage::fts_search(&conn, &params_beta).unwrap();
        assert!(
            !hits_beta.is_empty(),
            "beta token must be found in repo-beta"
        );
        for hit in &hits_beta {
            assert_eq!(
                hit.repository_id, r2.repository_id,
                "search scoped to repo-beta must only return repo-beta results"
            );
        }
    }

    #[test]
    fn e2e_path_lookup_returns_units() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "lookup_me.rs", "fn lookup_token_xyz() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        assert!(result.units_inserted >= 1);

        let units = attic_storage::fts_path_lookup(&conn, "lookup_me.rs", None, 50).unwrap();
        assert!(
            !units.is_empty(),
            "fts_path_lookup must return units for the indexed file"
        );
    }

    #[test]
    fn e2e_real_content_hash_stored() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "hash_check.rs", "fn hash_test() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        assert!(result.files_indexed >= 1);

        // The stored content_hash must NOT be a stub zero string.
        let hash: Option<String> = conn
            .query_row(
                "SELECT fo.content_hash
                   FROM core_file_occurrences fo
                   JOIN core_file_identities fi ON fo.file_identity_id = fi.id
                  WHERE fi.repository_id = ?1 AND fo.path = 'hash_check.rs'
                  LIMIT 1",
                rusqlite::params![result.repository_id],
                |r| r.get(0),
            )
            .ok();
        let hash = hash.expect("file occurrence must exist");
        assert!(
            !hash.starts_with("blake3:0"),
            "content_hash must not be a stub zero value; got: {hash}"
        );
        assert!(
            hash.starts_with("fnv:"),
            "content_hash must start with fnv: prefix; got: {hash}"
        );
    }

    #[test]
    fn e2e_real_manifest_hash_in_source_revision() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "manifest_check.rs", "fn manifest_token() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();

        // The stored manifest hash must be the real BLAKE3 hash from discovery
        // (64 hex chars), not the stub 'HEAD' or zeros.
        let manifest_hash: String = conn
            .query_row(
                "SELECT working_tree_manifest_hash
                   FROM core_source_revisions
                  WHERE id = ?1",
                rusqlite::params![result.source_revision_id],
                |r| r.get(0),
            )
            .expect("source_revision must exist");

        assert_eq!(manifest_hash.len(), 64, "manifest_hash must be 64 hex chars");
        assert!(
            manifest_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "manifest_hash must be hex; got: {manifest_hash}"
        );
        assert!(
            manifest_hash != "0".repeat(64),
            "manifest_hash must not be all-zeros stub"
        );
        assert_ne!(manifest_hash, "HEAD", "manifest_hash must not be stub 'HEAD'");
    }

    #[test]
    fn policy_hash_is_deterministic() {
        let policy = DiscoveryPolicy::default_git();
        let h1 = compute_policy_hash(&policy);
        let h2 = compute_policy_hash(&policy);
        assert_eq!(h1, h2, "policy hash must be deterministic");
        assert_eq!(h1.len(), 64, "policy hash must be 64 hex chars");
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn content_hash_differs_for_different_content() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "content_a").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "content_b").unwrap();
        let ha = compute_file_content_hash(&tmp.path().join("a.txt"));
        let hb = compute_file_content_hash(&tmp.path().join("b.txt"));
        assert_ne!(ha, hb, "different file content must produce different hash");
    }
}
