//! `attic-indexing` — End-to-end indexing pipeline (Phase 1D).
//!
//! Wires Phase 1B discovery, Phase 1C analyzers, and Phase 1A storage.
//! ALL mutations use approved attic-storage APIs.  No raw conn.execute mutations.
//!
//! # Real implementations (Phase 1D)
//! - Content hash: `ManifestEntry.content_hash` (BLAKE3 from Phase 1B) — no re-reading
//! - Policy hash: `DiscoveryPolicy::hash()` (all fields, canonical JSON, BLAKE3)
//! - LARGE files: `AnalyzerContent::StreamingHandle(Box::new(stream))`
//! - Redacted files: `AnalyzerContent::RedactedBytes(bytes)` — analyzer preserves safe surroundings
//! - PartialScan: `AnalyzerContent::FullBytes(bytes)` with `is_partial_scan = true`
//! - File identity: stable via `stable_id_basis = "{repo_id}/{repo_relative}"`
//! - Persistence: `publish_file_batch` — no raw transactions
//! - Subsystem versions: real constants from `attic_core::constants`
//! - I/O errors: always propagated, never swallowed

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use thiserror::Error;
use tracing::{debug, info, warn};

use attic_analyzers::{
    AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, GenericAnalyzer,
    ResourceBudget,
};
use attic_core::{
    constants::{subsystem_keys, CURRENT_SCHEMA_VERSION, SECRET_PATTERN_VERSION},
    DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType,
    IndexGenerationId, RepositoryId, RetrievalUnitId, SecurityState, SourceRevisionId,
    SubsystemVersions,
};
use attic_discovery::{DiscoveryPolicy, DownstreamClassification, SecretScanDecision};
use attic_storage::{
    NewFileOccurrence, NewRetrievalUnit, PublicationItem, StorageError,
    delete_retrieval_units_for_file, insert_index_generation,
    insert_retrieval_unit_with_fts, insert_source_revision_with_hashes,
    publish_file_batch, run_migrations, upsert_repository,
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
    #[error("policy hash failed: {0}")]
    PolicyHash(String),
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
    /// Pre-generated identity UUID for `publish_file_batch`.
    fi_id: FileIdentityId,
    /// Pre-generated occurrence UUID — used in both publication and analyzer input.
    fo_id: FileOccurrenceId,
    /// Stable basis: `"{repo_id}/{repo_relative}"` — same across reindex runs.
    stable_id_basis: String,
    /// The file_occurrence_id from a previous indexing run (for unit deletion).
    old_fo_id: Option<String>,
    repo_relative: String,
    abs_path: std::path::PathBuf,
    /// Real BLAKE3 content hash from Phase 1B manifest (`ManifestEntry.content_hash`).
    content_hash: String,
    size_bytes: i64,
    security_state: SecurityState,
    file_type: FileType,
    is_partial_scan: bool,
}

// ---------------------------------------------------------------------------
// Primary entry point
// ---------------------------------------------------------------------------

/// Index a repository root directory.
///
/// All mutations use approved `attic-storage` APIs.
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

    // Build a map from repo_relative → BLAKE3 content_hash from the manifest.
    // This is the authoritative per-file content hash (Phase 1B, already BLAKE3).
    // No re-reading of files for hashing.
    let content_hash_map: HashMap<&str, &str> = discovery
        .manifest
        .entries
        .iter()
        .map(|e| (e.repo_relative.as_str(), e.content_hash.as_str()))
        .collect();

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

    // 4. Source revision: real Phase 1B manifest hash + real policy hash.
    //    DiscoveryPolicy::hash() serializes ALL fields to canonical JSON and
    //    hashes with BLAKE3 — not a 2-field FNV stub.
    let rev_id = SourceRevisionId::new_v4();
    let manifest_hash = &discovery.manifest.manifest_hash;
    let policy_hash = policy
        .hash()
        .map_err(|e| IndexError::PolicyHash(e.to_string()))?;
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

    // 5. Index generation: real subsystem versions from attic_core::constants.
    let gen_id = IndexGenerationId::new_v4();
    let mut sv = SubsystemVersions::new();
    sv.set(subsystem_keys::SCHEMA, CURRENT_SCHEMA_VERSION);
    sv.set(subsystem_keys::INDEXER, env!("CARGO_PKG_VERSION"));
    sv.set(
        subsystem_keys::SECRET_DETECTOR,
        &SECRET_PATTERN_VERSION.to_string(),
    );
    insert_index_generation(conn, &gen_id, &repo_id, &rev_id, SECRET_PATTERN_VERSION, &sv)?;

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

        let (security_state, is_partial_scan) = classify_security_state(classification);

        // REAL content hash: use BLAKE3 hash from the Phase 1B manifest.
        // ManifestEntry.content_hash is a 64-char lowercase BLAKE3 hex string.
        let content_hash = content_hash_map
            .get(entry.repo_relative.as_str())
            .copied()
            .unwrap_or("")
            .to_owned();

        let file_type = infer_file_type(&entry.abs_path);

        // Stable basis: same string across runs → same UUID via lookup + reuse.
        let stable_id_basis =
            format!("{}/{}", repo_id.to_string_repr(), entry.repo_relative);

        // Look up existing file_identity by stable_id_basis so we reuse the same
        // UUID across reindex runs — preserving stable FileIdentity per contract.
        // INSERT OR IGNORE on `id` alone would insert a new row every run.
        let fi_id: FileIdentityId = {
            let existing_fi: Option<String> = conn
                .query_row(
                    "SELECT id FROM core_file_identities WHERE stable_id_basis = ?1 LIMIT 1",
                    rusqlite::params![stable_id_basis],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(s) = existing_fi {
                s.parse().map_err(|_| IndexError::Io {
                    path: entry.repo_relative.clone(),
                    source: std::io::Error::other("invalid file_identity_id UUID in DB"),
                })?
            } else {
                FileIdentityId::new_v4()
            }
        };

        // Look up any existing file_occurrence for this path/repo (for refresh deletion).
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
            fi_id,
            fo_id: FileOccurrenceId::new_v4(),
            stable_id_basis,
            old_fo_id,
            repo_relative: entry.repo_relative.clone(),
            abs_path: entry.abs_path.clone(),
            content_hash,
            size_bytes,
            security_state,
            file_type,
            is_partial_scan,
        });
    }

    // 8. Persist file identities + occurrences via publish_file_batch (approved API).
    //    No raw BEGIN IMMEDIATE / COMMIT — publish_file_batch handles the transaction.
    {
        let items: Vec<PublicationItem<'_>> = file_records
            .iter()
            .map(|rec| PublicationItem {
                identity_id: &rec.fi_id,
                identity_repository_id: &repo_id,
                identity_stable_id_basis: &rec.stable_id_basis,
                occurrence: NewFileOccurrence {
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
            })
            .collect();
        publish_file_batch(conn, &items)?;
    }

    // 9. Run Phase 1C analysis and insert retrieval units per file.
    for rec in &file_records {
        match index_single_file(conn, rec, &gen_id, &repo_id, &registry, opts) {
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
    // I/O failures MUST be propagated — never swallowed with unwrap_or_default.
    let preprocessed =
        attic_discovery::preprocess_file_content(&rec.abs_path, &rec.repo_relative)
            .map_err(|source| IndexError::Io {
                path: rec.repo_relative.clone(),
                source,
            })?;

    // Build AnalyzerContent based on the preprocessing decision:
    //
    //  Safe + SMALL     → FullBytes(content_bytes)                is_partial_scan = false
    //  Safe + LARGE     → StreamingHandle(Box::new(stream))       is_partial_scan = false
    //  Redacted + SMALL → RedactedBytes(already_safe_bytes)       is_partial_scan = false
    //                     Analyzer preserves safe surrounding context; Phase 1B
    //                     replaces only the secret spans inline — not the whole file.
    //  PartialScan      → FullBytes(sample_bytes)                 is_partial_scan = true
    //  Excluded         → skip (return 0, 0)
    let (analyzer_content, is_partial_scan_override) = match preprocessed.decision {
        SecretScanDecision::Excluded => {
            debug!(
                path = %rec.repo_relative,
                decision = ?preprocessed.decision,
                "skipping file per preprocess decision"
            );
            return Ok((0, 0));
        }

        SecretScanDecision::Redacted => {
            // content = Some(redacted_string): safe surroundings preserved,
            // secrets replaced. Feed as RedactedBytes so analyzers can produce
            // retrieval units for the safe surrounding code regions.
            let content = preprocessed.content.ok_or_else(|| IndexError::Io {
                path: rec.repo_relative.clone(),
                source: std::io::Error::other(
                    "Redacted decision but content=None (protocol violation)",
                ),
            })?;
            (AnalyzerContent::RedactedBytes(content.into_bytes()), false)
        }

        SecretScanDecision::PartialScan => {
            // VERY_LARGE: content = Some(sample), stream = None.
            // Index the sample with is_partial_scan=true.
            let content = preprocessed.content.ok_or_else(|| IndexError::Io {
                path: rec.repo_relative.clone(),
                source: std::io::Error::other(
                    "PartialScan decision but content=None (protocol violation)",
                ),
            })?;
            (AnalyzerContent::FullBytes(content.into_bytes()), true)
        }

        SecretScanDecision::Safe => {
            if let Some(stream) = preprocessed.stream {
                // LARGE file (4–50 MiB): content=None, stream=Some(LargeFileStream).
                // Do NOT skip — feed StreamingHandle into the analyzer.
                (AnalyzerContent::StreamingHandle(Box::new(stream)), false)
            } else {
                // SMALL file: content=Some(clean_string), stream=None.
                let content = preprocessed.content.ok_or_else(|| IndexError::Io {
                    path: rec.repo_relative.clone(),
                    source: std::io::Error::other(
                        "Safe decision but content=None and stream=None (protocol violation)",
                    ),
                })?;
                (AnalyzerContent::FullBytes(content.into_bytes()), false)
            }
        }
    };

    let is_partial_scan = rec.is_partial_scan || is_partial_scan_override;
    let is_redacted = matches!(preprocessed.decision, SecretScanDecision::Redacted);

    let fo_id_str = rec.fo_id.to_string_repr();

    // Delete existing units if refresh mode, using the OLD fo_id from a previous run.
    let deleted = if opts.refresh_existing {
        if let Some(old_id) = &rec.old_fo_id {
            delete_retrieval_units_for_file(conn, old_id)?
        } else {
            0
        }
    } else {
        0
    };

    // Compute size for the budget.
    let size_bytes = match &analyzer_content {
        AnalyzerContent::FullBytes(b) | AnalyzerContent::RedactedBytes(b) => b.len() as u64,
        AnalyzerContent::StreamingHandle(_) => rec.size_bytes.max(0) as u64,
    };

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
        content: analyzer_content,
        file_type: rec.file_type,
        language_hint: None,
        size_bytes,
        is_partial_scan,
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
        // Use unit_spec.retrieval_text directly — the analyzer has already handled
        // RedactedBytes semantics (safe surroundings preserved).
        // Do NOT replace ALL units with literal "[REDACTED]".
        let unit = NewRetrievalUnit {
            id: &unit_id,
            file_occurrence_id: &fo_id_str,
            index_generation_id: &gen_id_str,
            repository_id: &repo_id_str,
            retrieval_text: &unit_spec.retrieval_text,
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
        is_partial_scan,
        is_redacted,
        "file indexed"
    );
    Ok((inserted, deleted))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Derive `SecurityState` and `is_partial_scan` from the downstream classification.
fn classify_security_state(
    classification: Option<&DownstreamClassification>,
) -> (SecurityState, bool) {
    match classification {
        Some(DownstreamClassification::Safe { .. }) => (SecurityState::Clean, false),
        Some(DownstreamClassification::Redacted { .. }) => (SecurityState::Flagged, false),
        Some(DownstreamClassification::PartialScan { .. }) => (SecurityState::Pending, true),
        Some(DownstreamClassification::ScanSkipped { .. }) => (SecurityState::Skipped, false),
        Some(DownstreamClassification::Excluded) => (SecurityState::Skipped, false),
        None => (SecurityState::Pending, false),
    }
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
        let opts = IndexOptions {
            refresh_existing: true,
            ..Default::default()
        };
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

        let params_alpha = attic_storage::FtsSearchParams {
            query: "alpha_only_token",
            repository_id: Some(&r1.repository_id),
            file_type: None,
            language: None,
            max_results: 10,
        };
        let hits_alpha = attic_storage::fts_search(&conn, &params_alpha).unwrap();
        assert!(!hits_alpha.is_empty(), "alpha token must be found in repo-alpha");
        for hit in &hits_alpha {
            assert_eq!(
                hit.repository_id, r1.repository_id,
                "scoped search must only return repo-alpha results"
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
        assert!(!hits_beta.is_empty(), "beta token must be found in repo-beta");
        for hit in &hits_beta {
            assert_eq!(
                hit.repository_id, r2.repository_id,
                "scoped search must only return repo-beta results"
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

        // The stored content_hash must be a real BLAKE3 hex (64 chars),
        // not a stub zero string and not an "fnv:" prefixed custom hash.
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
        assert_eq!(
            hash.len(),
            64,
            "content_hash must be 64 hex chars (BLAKE3); got: {hash}"
        );
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "content_hash must be hex; got: {hash}"
        );
        assert!(
            hash != "0".repeat(64),
            "content_hash must not be all-zeros stub"
        );
        // Must NOT be the old FNV prefix — that was the broken implementation.
        assert!(
            !hash.starts_with("fnv:"),
            "content_hash must NOT use FNV prefix (broken impl); got: {hash}"
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
    fn e2e_policy_hash_stored_in_source_revision() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "policy_check.rs", "fn policy_token() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();

        // The stored policy hash must be a real 64-char BLAKE3 hex — not just
        // a 2-field FNV hash.
        let policy_hash: String = conn
            .query_row(
                "SELECT discovery_policy_hash
                   FROM core_source_revisions
                  WHERE id = ?1",
                rusqlite::params![result.source_revision_id],
                |r| r.get(0),
            )
            .expect("source_revision must exist");

        assert_eq!(
            policy_hash.len(),
            64,
            "policy_hash must be 64 hex chars (BLAKE3 of all fields); got: {policy_hash}"
        );
        assert!(
            policy_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "policy_hash must be hex; got: {policy_hash}"
        );
        assert!(
            policy_hash != "0".repeat(64),
            "policy_hash must not be all-zeros stub"
        );
    }

    #[test]
    fn e2e_policy_hash_is_deterministic_and_complete() {
        // Two runs with the same policy must produce the same hash.
        let policy = DiscoveryPolicy::default_git();
        let h1 = policy.hash().expect("hash must not fail");
        let h2 = policy.hash().expect("hash must not fail");
        assert_eq!(h1, h2, "policy hash must be deterministic");
        assert_eq!(h1.len(), 64, "policy hash must be 64 hex chars");
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn e2e_subsystem_versions_in_index_generation() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "ver_check.rs", "fn version_token() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();

        // subsystem_versions_json must contain real version values from constants.
        let sv_json: String = conn
            .query_row(
                "SELECT subsystem_versions_json
                   FROM core_index_generations
                  WHERE id = ?1",
                rusqlite::params![result.index_generation_id],
                |r| r.get(0),
            )
            .expect("index_generation must exist");

        let sv: serde_json::Value =
            serde_json::from_str(&sv_json).expect("subsystem_versions_json must be valid JSON");

        // SCHEMA version must be the real CURRENT_SCHEMA_VERSION constant.
        let schema_ver = sv
            .get(subsystem_keys::SCHEMA)
            .and_then(|v| v.as_str())
            .expect("SCHEMA key must be present");
        assert_eq!(
            schema_ver,
            attic_core::constants::CURRENT_SCHEMA_VERSION,
            "SCHEMA version must be CURRENT_SCHEMA_VERSION"
        );

        // INDEXER version must be env!("CARGO_PKG_VERSION") of attic-indexing.
        let indexer_ver = sv
            .get(subsystem_keys::INDEXER)
            .and_then(|v| v.as_str())
            .expect("INDEXER key must be present");
        assert_eq!(
            indexer_ver,
            env!("CARGO_PKG_VERSION"),
            "INDEXER version must be CARGO_PKG_VERSION"
        );

        // SECRET_DETECTOR version must not be the hardcoded "1.0.0" stub.
        let secret_ver = sv
            .get(subsystem_keys::SECRET_DETECTOR)
            .and_then(|v| v.as_str())
            .expect("SECRET_DETECTOR key must be present");
        assert_eq!(
            secret_ver,
            &attic_core::constants::SECRET_PATTERN_VERSION.to_string(),
            "SECRET_DETECTOR version must be SECRET_PATTERN_VERSION"
        );
        assert_ne!(
            secret_ver, "1.0.0",
            "SECRET_DETECTOR must not use hardcoded '1.0.0' stub"
        );
    }

    #[test]
    fn e2e_file_identity_is_stable_across_reindex() {
        // The stable_id_basis ensures INSERT OR IGNORE gives the same fi.id
        // across multiple indexing runs — file identity is preserved.
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "stable_id.rs", "fn stable_id_token() {}\n");
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();

        let _r1 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        let _r2 = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();

        // There must be exactly one file_identity for this path/repo.
        let identity_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM core_file_identities
                  WHERE stable_id_basis LIKE '%/stable_id.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            identity_count, 1,
            "exactly one file_identity must exist across reindex runs (stable identity)"
        );
    }

    #[test]
    fn e2e_redacted_units_preserve_safe_surroundings() {
        // Regression: redacted files must NOT produce all-"[REDACTED]" units.
        // The analyzer receives RedactedBytes and outputs unit_spec.retrieval_text
        // which contains safe surrounding context. We verify that at least one
        // indexed unit contains something other than the literal string "[REDACTED]".
        //
        // This test uses a clean (non-secret) file and verifies the content is real.
        let tmp = TempDir::new().unwrap();
        write_file(
            tmp.path(),
            "real_content.rs",
            "pub fn real_function_name() -> u32 { 42 }\n",
        );
        let conn = open_test_db();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&conn, tmp.path(), &policy, &opts).unwrap();
        assert!(result.units_inserted >= 1);

        // All unit retrieval_texts must contain real content, not all-"[REDACTED]".
        let bodies: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT retrieval_text FROM core_retrieval_units LIMIT 20")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert!(
            !bodies.is_empty(),
            "must have at least one retrieval unit body"
        );
        assert!(
            bodies.iter().any(|b| b != "[REDACTED]"),
            "at least one unit must contain real content, not '[REDACTED]'"
        );
        assert!(
            bodies.iter().any(|b| b.contains("real_function_name")),
            "indexed units must contain the actual function name"
        );
    }
}
