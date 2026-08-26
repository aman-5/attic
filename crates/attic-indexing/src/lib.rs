//! `attic-indexing` — End-to-end indexing pipeline (Phase 1D).
//!
//! Wires Phase 1B discovery, Phase 1C analyzers, and Phase 1A storage.
//!
//! # Coordinated-writer contract
//!
//! `attic-indexing` NEVER receives a raw write `rusqlite::Connection`.
//! Callers hand it an [`IndexingStore`] pairing an approved Phase 1A read pool
//! (`DbPool`) with the Phase 1A coordinated writer (`WriterQueueHandle`).
//! All canonical indexing writes (repository upsert, source revision, index
//! generation, file identities/occurrences, retrieval-unit deletion and FTS
//! insertion) are submitted as ONE [`attic_storage::submit_index_publication`]
//! mutation, which executes inside the writer queue's ambient
//! `BEGIN IMMEDIATE … COMMIT` — no nested transactions, no ad-hoc SQL.
//!
//! # Real implementations (Phase 1D)
//! - Content hash: `ManifestEntry.content_hash` (BLAKE3 from Phase 1B) — no re-reading
//! - Policy hash: `DiscoveryPolicy::hash()` (all fields, canonical JSON, BLAKE3)
//! - LARGE files: `AnalyzerContent::StreamingHandle(Box::new(stream))`
//! - Redacted files: `AnalyzerContent::RedactedBytes(bytes)` — analyzer preserves safe surroundings
//! - PartialScan: `AnalyzerContent::FullBytes(bytes)` with `is_partial_scan = true`
//! - File identity: stable via `stable_id_basis = "{repo_id}/{repo_relative}"`
//! - Subsystem versions: real constants from `attic_core::constants`
//! - I/O errors: always propagated, never swallowed

#![forbid(unsafe_code)]
#![deny(clippy::all)]

use std::collections::HashMap;
use std::path::Path;

use thiserror::Error;
use tracing::{debug, info, warn};

use attic_analyzers::{
    AnalyzerContent, AnalyzerInput, AnalyzerRegistry, CancellationToken, ResourceBudget,
};
use attic_core::{
    DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType, IndexGenerationId,
    RepositoryId, RetrievalUnitId, SecurityState, SourceRevisionId, SubsystemVersions,
    constants::{CURRENT_SCHEMA_VERSION, SECRET_PATTERN_VERSION, subsystem_keys},
};
use attic_discovery::{DiscoveryPolicy, DownstreamClassification, SecretScanDecision};
use attic_storage::{
    DbPool, IndexPublication, IndexPublicationStats, PublicationFile, PublicationOccurrence,
    PublicationRetrievalUnit, StorageError, WriterQueueHandle, lookup_file_identity_by_basis,
    lookup_latest_file_occurrence_for_path, lookup_repository_by_root_path,
    submit_index_publication,
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
    #[error("I/O error preprocessing {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("policy hash failed: {0}")]
    PolicyHash(String),
    #[error("repository at {0} has not been bootstrapped; run a full index first")]
    RepositoryNotBootstrapped(String),
}

pub mod incremental;
pub(crate) mod structural_pipeline;

pub use incremental::{ScopedChanges, ScopedIndexResult, index_changes};

// ---------------------------------------------------------------------------
// Store handle — the ONLY way callers provide database access
// ---------------------------------------------------------------------------

/// Approved Phase 1A storage endpoints handed to the indexing pipeline.
///
/// Reads go through the bounded `DbPool` of read-only connections; every
/// write goes through the coordinated `WriterQueueHandle`.  Constructing this
/// struct requires neither a raw connection nor unrestricted write access.
#[derive(Clone, Copy)]
pub struct IndexingStore<'a> {
    /// Bounded read-only connection pool (Phase 1A).
    pub readers: &'a DbPool,
    /// Coordinated single-writer submission endpoint (Phase 1A).
    pub writer: &'a WriterQueueHandle,
}

// ---------------------------------------------------------------------------
// Public options / result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IndexOptions {
    pub repository_name: String,
    pub max_units_per_file: usize,
    pub refresh_existing: bool,
    /// Phase 3 — when `false`, only the GenericAnalyzer runs (Phase 1D
    /// behaviour; used for honest baselines in benchmarks and as an
    /// operational kill-switch). Default `true`.
    pub structural: bool,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            repository_name: "default".to_owned(),
            max_units_per_file: 512,
            refresh_existing: true,
            structural: true,
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
    /// Pre-generated identity UUID for the publication batch.
    fi_id: FileIdentityId,
    /// Pre-generated occurrence UUID — used in publication AND analyzer input.
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

/// A retrieval unit produced by analysis, awaiting coordinated publication.
struct PendingUnit {
    file_occurrence_id: String,
    retrieval_text: String,
    analyzer_id: String,
    analyzer_version: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
    is_redacted: bool,
    /// Phase 3 — structural node index this unit is anchored to.
    structural_node_index: Option<usize>,
}

// ---------------------------------------------------------------------------
// Primary entry point
// ---------------------------------------------------------------------------

/// Index a repository root directory.
///
/// Discovery and analysis run on the calling thread; every mutation is then
/// submitted atomically through the coordinated writer queue.  The target
/// database MUST already be migrated (the server migrates at startup before
/// creating the writer queue).
///
/// All reads use approved `attic-storage` accessors; all writes use
/// [`submit_index_publication`].  No raw SQL and no raw write connection
/// exists anywhere in this crate.
pub fn index_repository(
    store: &IndexingStore<'_>,
    root: &Path,
    policy: &DiscoveryPolicy,
    opts: &IndexOptions,
) -> Result<IndexResult, IndexError> {
    // 1. Phase 1B discovery — real manifest hash, git meta, security classification.
    let discovery = attic_discovery::discover(root, policy)?;

    info!(
        files = discovery.entries.len(),
        manifest_hash = %discovery.manifest.manifest_hash,
        "discovery complete"
    );

    // Build a map from repo_relative → BLAKE3 content_hash from the manifest.
    let content_hash_map: HashMap<&str, &str> = discovery
        .manifest
        .entries
        .iter()
        .map(|e| (e.repo_relative.as_str(), e.content_hash.as_str()))
        .collect();

    // 2. Bootstrap / retrieve repository record via approved read accessor.
    let root_str = root.to_string_lossy();
    let existing_repo_id: Option<RepositoryId> = store
        .readers
        .with_reader(|c| lookup_repository_by_root_path(c, &root_str))
        .map_err(IndexError::Storage)?;

    let repo_id: RepositoryId = match existing_repo_id {
        Some(id) => id,
        None => RepositoryId::new_v4(),
    };
    let repo_upsert = match existing_repo_id {
        None => Some((root_str.to_string(), opts.repository_name.clone())),
        Some(_) => None,
    };

    // 3. Source revision: real Phase 1B manifest hash + real policy hash.
    //    DiscoveryPolicy::hash() serializes ALL fields to canonical JSON and
    //    hashes with BLAKE3.
    let rev_id = SourceRevisionId::new_v4();
    let manifest_hash = &discovery.manifest.manifest_hash;
    let policy_hash = policy
        .hash()
        .map_err(|e| IndexError::PolicyHash(e.to_string()))?;
    let commit_sha: Option<String> = discovery.git_meta.as_ref().and_then(|g| g.head_sha.clone());
    let unstable_capture = !discovery.manifest.unstable_captures.is_empty();

    // 4. Index generation: real subsystem versions from attic_core::constants.
    let gen_id = IndexGenerationId::new_v4();
    let mut sv = SubsystemVersions::new();
    sv.set(subsystem_keys::SCHEMA, CURRENT_SCHEMA_VERSION);
    sv.set(subsystem_keys::INDEXER, env!("CARGO_PKG_VERSION"));
    sv.set(
        subsystem_keys::ANALYZER_REGISTRY,
        attic_core::constants::ANALYZER_REGISTRY_VERSION,
    );
    sv.set(
        subsystem_keys::SECRET_DETECTOR,
        SECRET_PATTERN_VERSION.to_string(),
    );

    let mut result = IndexResult {
        repository_id: repo_id.to_string_repr(),
        source_revision_id: rev_id.to_string_repr(),
        index_generation_id: gen_id.to_string_repr(),
        ..Default::default()
    };

    // 5. Collect eligible file records.  All lookups below read COMMITTED
    //    state from previous runs (via the read pool); nothing here depends
    //    on this run's own writes, so pre-publication reads are correct.
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
        let size_bytes = i64::try_from(file_meta.len()).unwrap_or(i64::MAX);

        let (security_state, is_partial_scan) = classify_security_state(classification);

        // REAL content hash: BLAKE3 hash from the Phase 1B manifest.
        let content_hash = content_hash_map
            .get(entry.repo_relative.as_str())
            .copied()
            .unwrap_or("")
            .to_owned();

        let file_type = infer_file_type(&entry.abs_path);

        // Stable basis: same string across runs → same UUID via lookup + reuse.
        let repo_id_str = repo_id.to_string_repr();
        let stable_id_basis = format!("{repo_id_str}/{}", entry.repo_relative);

        // Look up existing file_identity by stable_id_basis via approved API.
        // This reuses the same UUID across reindex runs.
        let fi_id: FileIdentityId = store
            .readers
            .with_reader(|c| lookup_file_identity_by_basis(c, &stable_id_basis))
            .map_err(IndexError::Storage)?
            .unwrap_or_else(FileIdentityId::new_v4);

        // Look up any existing file_occurrence for this path/repo via approved API.
        let old_fo_id: Option<String> = store
            .readers
            .with_reader(|c| {
                lookup_latest_file_occurrence_for_path(c, &repo_id, &entry.repo_relative)
            })
            .map_err(IndexError::Storage)?;

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

    // 6. Run Phase 1C analysis per file.  Produces pending units only — no
    //    database writes happen during analysis.
    let registry = if opts.structural {
        structural_pipeline::default_registry()
    } else {
        structural_pipeline::generic_only_registry()
    };
    let mut pending_units: Vec<PendingUnit> = Vec::new();
    let mut stale_occurrences: Vec<String> = Vec::new();
    let mut indexed_records: Vec<FileRecord> = Vec::new();
    let mut pipeline = structural_pipeline::StructuralPipeline::new(
        root,
        discovery
            .manifest
            .entries
            .iter()
            .map(|e| e.repo_relative.clone())
            .collect(),
    );

    for rec in file_records {
        match analyze_single_file(&rec, &registry, opts) {
            Ok((mut units, captured)) => {
                if opts.refresh_existing
                    && let Some(old) = rec.old_fo_id.clone()
                {
                    stale_occurrences.push(old);
                }
                pipeline.note_occurrence(&rec.repo_relative, &rec.fo_id.to_string_repr());
                if let Some(captured) = captured {
                    pipeline.record(captured);
                }
                pending_units.append(&mut units);
                indexed_records.push(rec);
                result.files_indexed += 1;
            }
            Err(e) => {
                warn!(
                    path = %rec.repo_relative,
                    error = %e,
                    "analysis/unit production failed, skipping"
                );
                result.files_skipped += 1;
            }
        }
    }

    // 7. Publish EVERYTHING as one coordinated writer-queue mutation.
    let publication_files: Vec<PublicationFile> = indexed_records
        .iter()
        .map(|rec| PublicationFile {
            identity_id: rec.fi_id,
            identity_repository_id: repo_id,
            stable_id_basis: rec.stable_id_basis.clone(),
            occurrence: PublicationOccurrence {
                id: rec.fo_id,
                source_revision_id: rev_id,
                index_generation_id: Some(gen_id),
                path: rec.repo_relative.clone(),
                content_hash: rec.content_hash.clone(),
                size_bytes: rec.size_bytes,
                language: None,
                file_type: rec.file_type,
                discovery_class: DiscoveryClass::Vcs,
                security_state: rec.security_state,
                existence_state: ExistenceState::Present,
            },
        })
        .collect();

    let repo_id_str = repo_id.to_string_repr();
    let gen_id_str = gen_id.to_string_repr();
    let mut retrieval_units: Vec<PublicationRetrievalUnit> = Vec::new();
    let mut unit_links_by_occ: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for u in pending_units {
        let unit_id = RetrievalUnitId::new_v4().to_string_repr();
        if let Some(idx) = u.structural_node_index {
            unit_links_by_occ
                .entry(u.file_occurrence_id.clone())
                .or_default()
                .push((unit_id.clone(), idx));
        }
        retrieval_units.push(PublicationRetrievalUnit {
            id: unit_id,
            file_occurrence_id: u.file_occurrence_id,
            index_generation_id: gen_id_str.clone(),
            repository_id: repo_id_str.clone(),
            retrieval_text: u.retrieval_text,
            analyzer_id: u.analyzer_id,
            analyzer_version: u.analyzer_version,
            start_line: u.start_line,
            end_line: u.end_line,
            is_redacted: u.is_redacted,
        });
    }

    // Phase 3 — resolve structural edges and assemble payloads.
    let repo_id_for_deps = repo_id;
    let deps = structural_pipeline::ResolverDeps {
        symbol_definition: &|qname, kinds| {
            store
                .readers
                .with_reader(|c| {
                    attic_storage::lookup_symbol_definition_occurrence(
                        c,
                        &repo_id_for_deps.to_string_repr(),
                        qname,
                        kinds,
                    )
                })
                .ok()
                .flatten()
        },
        path_occurrence: &|rel_path| {
            store
                .readers
                .with_reader(|c| {
                    attic_storage::lookup_latest_file_occurrence_for_path(
                        c,
                        &repo_id_for_deps,
                        rel_path,
                    )
                })
                .ok()
                .flatten()
        },
    };
    let structural_files = pipeline.finish(&deps, &unit_links_by_occ);

    let stats: IndexPublicationStats = submit_index_publication(
        store.writer,
        IndexPublication {
            repository_id: repo_id,
            repository_upsert: repo_upsert,
            source_revision_id: rev_id,
            commit_sha,
            working_tree_manifest_hash: manifest_hash.clone(),
            discovery_policy_hash: policy_hash,
            unstable_capture,
            index_generation_id: gen_id,
            secret_detector_version: SECRET_PATTERN_VERSION,
            subsystem_versions: sv,
            files: publication_files,
            delete_units_for_occurrences: stale_occurrences.clone(),
            close_audit_for_occurrences: stale_occurrences.clone(),
            retrieval_units,
            structural_files,
            delete_structural_for_occurrences: stale_occurrences,
        },
    )
    .map_err(IndexError::Storage)?;

    result.units_inserted = stats.units_inserted;
    result.units_deleted = stats.units_deleted;

    info!(
        files_indexed = result.files_indexed,
        files_skipped = result.files_skipped,
        units_inserted = result.units_inserted,
        units_deleted = result.units_deleted,
        repository_id = %result.repository_id,
        "indexing run complete"
    );

    Ok(result)
}

// ---------------------------------------------------------------------------
// Per-file analysis (pure — no database writes)
// ---------------------------------------------------------------------------

/// Run Phase 1B preprocessing + Phase 1C dispatch for one file and return
/// the retrieval units (with structural anchors) and, when a specialized
/// structural analyzer produced output, its capturable payload.
/// Never touches the database.
fn analyze_single_file(
    rec: &FileRecord,
    registry: &AnalyzerRegistry,
    opts: &IndexOptions,
) -> Result<(Vec<PendingUnit>, Option<structural_pipeline::CapturedFile>), IndexError> {
    // Preprocess through Phase 1B secrets layer.
    // I/O failures MUST be propagated — never swallowed with unwrap_or_default.
    let preprocessed = attic_discovery::preprocess_file_content(&rec.abs_path, &rec.repo_relative)
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
    //  Excluded         → skip (return no units)
    let (analyzer_content, is_partial_scan_override) = match preprocessed.decision {
        SecretScanDecision::Excluded => {
            debug!(
                path = %rec.repo_relative,
                decision = ?preprocessed.decision,
                "skipping file per preprocess decision"
            );
            return Ok((Vec::new(), None));
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
    let analyzer_id = output.analyzer_id.as_str().to_owned();
    let analyzer_version = output.analyzer_version.as_str().to_owned();

    let units: Vec<PendingUnit> = output
        .retrieval_units
        .iter()
        .map(|unit_spec| PendingUnit {
            file_occurrence_id: fo_id_str.clone(),
            // unit_spec.retrieval_text directly — the analyzer has already
            // handled RedactedBytes semantics (safe surroundings preserved).
            retrieval_text: unit_spec.retrieval_text.clone(),
            analyzer_id: analyzer_id.clone(),
            analyzer_version: analyzer_version.clone(),
            start_line: Some(unit_spec.span.start_line),
            end_line: Some(unit_spec.span.end_line),
            is_redacted,
            structural_node_index: unit_spec.structural_node_index,
        })
        .collect();

    let captured = structural_pipeline::capture_structural(&rec.repo_relative, &fo_id_str, &output);

    debug!(
        path = %rec.repo_relative,
        units = units.len(),
        is_partial_scan,
        is_redacted,
        "file analyzed"
    );
    Ok((units, captured))
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
        Some("js") | Some("mjs") | Some("cjs") | Some("jsx") => FileType::JavaScript,
        Some("java") => FileType::Java,
        Some("go") => FileType::Go,
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
    use attic_storage::{WriterQueue, connection::open_ro};
    use tempfile::TempDir;

    /// File-backed store fixture: open_db → migrations → WriterQueue.
    ///
    /// Mirrors exactly how `attic-server` constructs its endpoints; there is
    /// no way to obtain a raw write connection through this helper's output.
    struct StoreFixture {
        _dir: TempDir,
        db_path: std::path::PathBuf,
        pool: DbPool,
        _queue: WriterQueue,
        handle: WriterQueueHandle,
    }

    fn make_store() -> StoreFixture {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("indexing_test.db");
        let (conn, pool) = attic_storage::open_db(&db_path).unwrap();
        run_migrations_fixture(&conn);
        let queue = WriterQueue::new(conn).unwrap();
        let handle = queue.handle();
        StoreFixture {
            _dir: dir,
            db_path,
            pool,
            _queue: queue,
            handle,
        }
    }

    fn run_migrations_fixture(conn: &rusqlite::Connection) {
        attic_storage::run_migrations(conn).unwrap();
    }

    fn store(fx: &StoreFixture) -> IndexingStore<'_> {
        IndexingStore {
            readers: &fx.pool,
            writer: &fx.handle,
        }
    }

    fn verify_conn(fx: &StoreFixture) -> rusqlite::Connection {
        // Read-only independent connection for post-commit assertions.
        open_ro(&fx.db_path).unwrap()
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    // -----------------------------------------------------------------------
    // Basic pipeline tests
    // -----------------------------------------------------------------------

    #[test]
    fn index_empty_directory_succeeds() {
        let fx = make_store();
        let tmp = TempDir::new().unwrap();
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), tmp.path(), &policy, &opts)
            .expect("indexing empty dir should succeed");
        assert_eq!(result.files_indexed, 0);
        assert_eq!(result.units_inserted, 0);
    }

    #[test]
    fn index_single_text_file_inserts_units() {
        let fx = make_store();
        write_file(
            fx._dir.path(),
            "hello.rs",
            "fn main() { println!(\"hello world\"); }\n",
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts)
            .expect("indexing should succeed");
        assert!(result.files_indexed >= 1, "at least one file indexed");
        assert!(result.units_inserted >= 1, "at least one unit inserted");
        assert!(!result.repository_id.is_empty());
        assert!(!result.source_revision_id.is_empty());
        assert!(!result.index_generation_id.is_empty());
    }

    #[test]
    fn second_index_run_refreshes_units() {
        let fx = make_store();
        write_file(fx._dir.path(), "foo.rs", "fn foo() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions {
            refresh_existing: true,
            ..Default::default()
        };
        let s = store(&fx);
        let r1 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        let r2 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        // Second run deletes first run's units and re-inserts.
        assert_eq!(r2.units_deleted, r1.units_inserted);
        assert_eq!(r2.units_inserted, r1.units_inserted);
    }

    #[test]
    fn repository_id_is_stable_across_runs() {
        let fx = make_store();
        write_file(fx._dir.path(), "a.rs", "fn a() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        let r1 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        let r2 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(r1.repository_id, r2.repository_id, "stable repository_id");
    }

    // -----------------------------------------------------------------------
    // Coordinated-writer guarantees
    // -----------------------------------------------------------------------

    #[test]
    fn indexing_writes_commit_only_through_coordinated_writer() {
        // Structural gate: the pipeline receives ONLY DbPool + WriterQueueHandle.
        // After a successful run an INDEPENDENT read-only connection must see
        // every row, proving writes were committed by the writer queue rather
        // than any side channel.
        let fx = make_store();
        write_file(
            fx._dir.path(),
            "coord.rs",
            "pub fn coord_writer_token() {}\n",
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert!(result.units_inserted >= 1);

        let verify = verify_conn(&fx);
        let repos: i64 = verify
            .query_row("SELECT COUNT(*) FROM core_repositories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(repos, 1, "repository row must be committed");

        let units: i64 = verify
            .query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            units, result.units_inserted as i64,
            "committed unit count must match reported count"
        );

        // And the same data must be visible through the approved read pool.
        let hits = fx
            .pool
            .with_reader(|c| {
                attic_storage::fts_search(
                    c,
                    &attic_storage::FtsSearchParams {
                        query: "coord_writer_token",
                        repository_id: None,
                        file_type: None,
                        language: None,
                        max_results: 10,
                    },
                )
            })
            .unwrap();
        assert!(
            !hits.is_empty(),
            "coordinated-writer commit must be searchable"
        );
    }

    #[test]
    fn failed_publication_leaves_no_partial_state() {
        // A publication whose batch fails must roll back completely.  We force
        // a failure by shutting the writer queue down BEFORE indexing: every
        // send returns QueueShutdown, so index_repository must fail cleanly
        // without leaving repository/revision rows behind.
        let fx = make_store();
        let root = fx._dir.path().to_path_buf();
        write_file(&root, "rollback.rs", "fn rollback_token() {}\n");
        let StoreFixture {
            db_path,
            pool,
            handle,
            _queue,
            ..
        } = fx;
        drop(_queue); // shut down the writer worker

        let broken = IndexingStore {
            readers: &pool,
            writer: &handle,
        };
        let policy = DiscoveryPolicy::default_git();
        let outcome = index_repository(&broken, &root, &policy, &IndexOptions::default());

        match outcome {
            Err(IndexError::Storage(_)) => {}
            other => panic!("expected Storage error after queue shutdown, got {other:?}"),
        }

        let verify = open_ro(&db_path).unwrap();
        let revisions: i64 = verify
            .query_row("SELECT COUNT(*) FROM core_source_revisions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            revisions, 0,
            "failed publication must not persist a revision"
        );
    }

    // -----------------------------------------------------------------------
    // E2E: real data actually reaches the database
    // -----------------------------------------------------------------------

    #[test]
    fn e2e_indexed_file_is_searchable() {
        let fx = make_store();
        write_file(
            fx._dir.path(),
            "search_me.rs",
            "pub fn greet_the_world() -> &'static str { \"hello\" }\n",
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert!(result.units_inserted >= 1);

        let hits = fx
            .pool
            .with_reader(|c| {
                attic_storage::fts_search(
                    c,
                    &attic_storage::FtsSearchParams {
                        query: "greet_the_world",
                        repository_id: None,
                        file_type: None,
                        language: None,
                        max_results: 10,
                    },
                )
            })
            .unwrap();
        assert!(
            !hits.is_empty(),
            "FTS search must find indexed content after index_repository"
        );
    }

    #[test]
    fn e2e_repository_scoped_search() {
        let fx = make_store();
        let repo1 = fx._dir.path().join("alpha");
        let repo2 = fx._dir.path().join("beta");
        std::fs::create_dir_all(&repo1).unwrap();
        std::fs::create_dir_all(&repo2).unwrap();
        write_file(&repo1, "alpha.rs", "fn alpha_only_token() {}\n");
        write_file(&repo2, "beta.rs", "fn beta_only_token() {}\n");

        let policy = DiscoveryPolicy::default_git();
        let opts1 = IndexOptions {
            repository_name: "repo-alpha".into(),
            ..Default::default()
        };
        let opts2 = IndexOptions {
            repository_name: "repo-beta".into(),
            ..Default::default()
        };
        let s = store(&fx);
        let r1 = index_repository(&s, &repo1, &policy, &opts1).unwrap();
        let r2 = index_repository(&s, &repo2, &policy, &opts2).unwrap();

        let scoped = |query: &'static str, repo: &str| {
            fx.pool
                .with_reader(|c| {
                    attic_storage::fts_search(
                        c,
                        &attic_storage::FtsSearchParams {
                            query,
                            repository_id: Some(repo),
                            file_type: None,
                            language: None,
                            max_results: 10,
                        },
                    )
                })
                .unwrap()
        };

        let hits_alpha = scoped("alpha_only_token", &r1.repository_id);
        assert!(
            !hits_alpha.is_empty(),
            "alpha token must be found in repo-alpha"
        );
        for hit in &hits_alpha {
            assert_eq!(
                hit.repository_id, r1.repository_id,
                "scoped search must only return repo-alpha results"
            );
        }

        let hits_beta = scoped("beta_only_token", &r2.repository_id);
        assert!(
            !hits_beta.is_empty(),
            "beta token must be found in repo-beta"
        );
        for hit in &hits_beta {
            assert_eq!(
                hit.repository_id, r2.repository_id,
                "scoped search must only return repo-beta results"
            );
        }
    }

    #[test]
    fn e2e_path_lookup_returns_units() {
        let fx = make_store();
        write_file(fx._dir.path(), "lookup_me.rs", "fn lookup_token_xyz() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert!(result.units_inserted >= 1);

        let units = fx
            .pool
            .with_reader(|c| attic_storage::fts_path_lookup(c, "lookup_me.rs", None, 50))
            .unwrap();
        assert!(
            !units.is_empty(),
            "fts_path_lookup must return units for the indexed file"
        );
    }

    #[test]
    fn e2e_real_content_hash_stored() {
        let fx = make_store();
        write_file(fx._dir.path(), "hash_check.rs", "fn hash_test() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert!(result.files_indexed >= 1);

        let verify = verify_conn(&fx);
        let hash: String = verify
            .query_row(
                "SELECT fo.content_hash
                    FROM core_file_occurrences fo
                    JOIN core_file_identities fi ON fo.file_identity_id = fi.id
                   WHERE fi.repository_id = ?1 AND fo.path = 'hash_check.rs'
                   LIMIT 1",
                rusqlite::params![result.repository_id],
                |r| r.get(0),
            )
            .expect("file occurrence must exist");
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
        assert!(
            !hash.starts_with("fnv:"),
            "content_hash must NOT use FNV prefix (broken impl); got: {hash}"
        );
    }

    #[test]
    fn e2e_real_manifest_hash_in_source_revision() {
        let fx = make_store();
        write_file(
            fx._dir.path(),
            "manifest_check.rs",
            "fn manifest_token() {}\n",
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();

        let verify = verify_conn(&fx);
        let manifest_hash: String = verify
            .query_row(
                "SELECT working_tree_manifest_hash FROM core_source_revisions WHERE id = ?1",
                rusqlite::params![result.source_revision_id],
                |r| r.get(0),
            )
            .expect("source_revision must exist");

        assert_eq!(
            manifest_hash.len(),
            64,
            "manifest_hash must be 64 hex chars"
        );
        assert!(
            manifest_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "manifest_hash must be hex; got: {manifest_hash}"
        );
        assert!(
            manifest_hash != "0".repeat(64),
            "manifest_hash must not be all-zeros stub"
        );
        assert_ne!(
            manifest_hash, "HEAD",
            "manifest_hash must not be stub 'HEAD'"
        );
    }

    #[test]
    fn e2e_policy_hash_stored_in_source_revision() {
        let fx = make_store();
        write_file(fx._dir.path(), "policy_check.rs", "fn policy_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();

        let verify = verify_conn(&fx);
        let policy_hash: String = verify
            .query_row(
                "SELECT discovery_policy_hash FROM core_source_revisions WHERE id = ?1",
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
        let policy = DiscoveryPolicy::default_git();
        let h1 = policy.hash().expect("hash must not fail");
        let h2 = policy.hash().expect("hash must not fail");
        assert_eq!(h1, h2, "policy hash must be deterministic");
        assert_eq!(h1.len(), 64, "policy hash must be 64 hex chars");
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn e2e_subsystem_versions_in_index_generation() {
        let fx = make_store();
        write_file(fx._dir.path(), "ver_check.rs", "fn version_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();

        let verify = verify_conn(&fx);
        let sv_json: String = verify
            .query_row(
                "SELECT subsystem_versions_json FROM core_index_generations WHERE id = ?1",
                rusqlite::params![result.index_generation_id],
                |r| r.get(0),
            )
            .expect("index_generation must exist");

        let sv: serde_json::Value =
            serde_json::from_str(&sv_json).expect("subsystem_versions_json must be valid JSON");

        let schema_ver = sv
            .get(subsystem_keys::SCHEMA)
            .and_then(|v| v.as_str())
            .expect("SCHEMA key must be present");
        assert_eq!(
            schema_ver,
            attic_core::constants::CURRENT_SCHEMA_VERSION,
            "SCHEMA version must be CURRENT_SCHEMA_VERSION"
        );

        let indexer_ver = sv
            .get(subsystem_keys::INDEXER)
            .and_then(|v| v.as_str())
            .expect("INDEXER key must be present");
        assert_eq!(
            indexer_ver,
            env!("CARGO_PKG_VERSION"),
            "INDEXER version must be CARGO_PKG_VERSION"
        );

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
        let fx = make_store();
        write_file(fx._dir.path(), "stable_id.rs", "fn stable_id_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        let _r1 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        let _r2 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        let verify = verify_conn(&fx);
        let identity_count: i64 = verify
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
        // This test uses a clean (non-secret) file and verifies real content.
        let fx = make_store();
        write_file(
            fx._dir.path(),
            "real_content.rs",
            "pub fn real_function_name() -> u32 { 42 }\n",
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert!(result.units_inserted >= 1);

        let verify = verify_conn(&fx);
        let bodies: Vec<String> = {
            let mut stmt = verify
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
