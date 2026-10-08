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
// clippy 1.98 `missing_const_for_thread_local` misfires on the test-only
// ANALYZE_SINGLE_FILE_CALLS counter: its initializer already IS a const
// block (the form the lint asks for), and an allow attribute cannot attach
// to the thread_local! macro invocation — so it is scoped to test builds.
#![cfg_attr(test, allow(clippy::missing_const_for_thread_local))]

use std::collections::{HashMap, HashSet};
use std::path::Path;

use thiserror::Error;
use tracing::{debug, info, warn};

use attic_analyzers::{
    AnalyzerContent, AnalyzerInput, AnalyzerRegistry, ResourceBudget, diagnostic_codes,
};
use attic_core::{
    CancellationToken, DiscoveryClass, ExistenceState, FileIdentityId, FileOccurrenceId, FileType,
    IndexGenerationId, RepositoryId, RetrievalUnitId, SecurityState, SourceRevisionId,
    SubsystemVersions,
    constants::{CURRENT_SCHEMA_VERSION, SECRET_PATTERN_VERSION, subsystem_keys},
};
use attic_discovery::{
    DiscoveryPolicy, DownstreamClassification, EligibleEntry, SecretScanDecision,
};
use attic_storage::{
    DbPool, IndexPublication, IndexPublicationStats, PublicationFile, PublicationOccurrence,
    PublicationRetrievalUnit, StorageError, WriterQueueHandle, bulk_file_identities_for_repository,
    bulk_latest_occurrence_ids_for_repository, latest_active_paths_for_repository,
    lookup_occurrence_snapshot, lookup_repository_by_root_path, submit_index_publication,
};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("indexing cancelled")]
    Cancelled,
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
    /// The analyzer plugin selection names an unknown plugin.
    #[error("invalid analyzer configuration: {0}")]
    AnalyzerConfig(String),
    #[error("repository at {0} has not been bootstrapped; run a full index first")]
    RepositoryNotBootstrapped(String),
    /// Generation-completeness invariant (Phase 6.4): one or more discovered
    /// paths hit a transient/retryable failure and never reached a terminal
    /// state (indexed, intentionally skipped, or removed/excluded) this run.
    /// The caller MUST treat this exactly like any other failed indexing
    /// attempt — nothing was published, the previous generation (if any)
    /// remains untouched and current, and a retry is expected to resolve it.
    #[error(
        "indexing generation incomplete: {} file(s) failed with a transient/retryable error and never reached a terminal state; nothing was published, the previous generation remains current",
        paths.len()
    )]
    TransientFailures { paths: Vec<String> },
    /// Fail-closed completeness contract: the analyzer reported that it could
    /// not represent the whole file within its resource budget
    /// (`RESOURCE_EXHAUSTED` — e.g. retrieval-unit cap reached mid-document).
    /// Publishing those units would silently truncate indexed content, so the
    /// file is treated as a transient failure: the completeness gate aborts
    /// the run, nothing is published, and the previous generation remains
    /// current. Raising the budget or fixing the chunker is required before
    /// this content can index successfully — never publish partial units.
    #[error(
        "analysis incomplete for {path}: {reason}; nothing from this file will be published (fail-closed)"
    )]
    IncompleteAnalysis { path: String, reason: String },
    /// `discovery.downstream_classifications` is supposed to be positionally
    /// aligned with `discovery.entries` (one classification per entry, same
    /// order — see `attic_discovery::discover`). A length mismatch means
    /// that invariant was violated; failing loudly here is required so a
    /// future discovery-layer change can never silently misattribute one
    /// file's classification to another.
    #[error(
        "discovery invariant violated: {entries} discovered entries but {classifications} downstream classifications (must be equal and positionally aligned)"
    )]
    ClassificationCountMismatch {
        entries: usize,
        classifications: usize,
    },
    /// The lengths matched but the path at some position did not — the
    /// alignment invariant is violated even though the counts agree.
    #[error(
        "discovery invariant violated: entry '{expected}' at position {index} does not match its classification's recorded path '{found}'"
    )]
    ClassificationPathMismatch {
        index: usize,
        expected: String,
        found: String,
    },
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

/// Placeholder `IndexOptions::repository_name`. When a caller keeps it, the
/// repository is named after its root folder instead.
pub const DEFAULT_REPOSITORY_NAME: &str = "default";

/// Name stored for the repository at `root`: an explicitly configured
/// `repository_name`, else the root folder's own name.
pub fn repository_display_name(root: &Path, opts: &IndexOptions) -> String {
    if opts.repository_name != DEFAULT_REPOSITORY_NAME {
        return opts.repository_name.clone();
    }
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Display name for the repository; [`DEFAULT_REPOSITORY_NAME`] means
    /// "use the root folder's name".
    pub repository_name: String,
    pub max_units_per_file: usize,
    pub refresh_existing: bool,
    /// Phase 3 — when `false`, only the GenericAnalyzer runs (Phase 1D
    /// behaviour; used for honest baselines in benchmarks and as an
    /// operational kill-switch). Default `true`.
    pub structural: bool,
    /// Flush the in-memory PR-7 analysis-cache buffer to the database once it
    /// holds this many bytes of serialized analysis output.
    ///
    /// Without a bound, `cache_writes` accumulates a second, JSON-escaped copy
    /// of every analyzed file's units for the whole repository — measured at
    /// ~1.13× the size of the retrieval text the pipeline is already holding —
    /// so peak memory grew linearly with repository size with no ceiling.
    ///
    /// Flushing early is safe in a way that batching the *publication* is not:
    /// `index_analysis_cache` is purely a retry accelerator. It touches
    /// neither `core_file_occurrences` nor `core_index_generations`, never
    /// affects which generation is CURRENT, and is cleared wholesale after a
    /// successful publication. Flushing sooner also strictly *improves*
    /// recovery — analysis that completed before a crash or cancellation is
    /// already durable instead of being discarded with the process.
    ///
    /// `0` disables flushing (single end-of-run write, the pre-bound
    /// behaviour).
    pub analysis_cache_flush_bytes: u64,
    /// Structural analyzers to enable (plugin selection from `attic.toml
    /// [indexing] analyzers / disabled_analyzers`). Ignored when
    /// `structural` is `false`. Default: every built-in plugin.
    pub analyzers: attic_analyzers::AnalyzerSelection,
    /// Maximum worker threads used for per-file analysis.
    ///
    /// `0` means "derive from [`std::thread::available_parallelism`]".
    /// Analysis is the dominant stage on a cold index and
    /// [`analyze_single_file`] is pure — it reads one file, dispatches to a
    /// `Send + Sync` analyzer, and returns owned data — so it parallelises
    /// without touching the coordinated-writer contract. Results are always
    /// merged back in discovery order, so output is independent of thread
    /// count.
    pub analysis_threads: usize,
}

/// Default ceiling for the in-memory analysis-cache buffer (32 MiB).
///
/// Sized from the measured ~1.7 KB average serialized unit: 32 MiB is roughly
/// 19,000 units of buffered retry-cache, which amortises the writer-queue
/// round trip well while keeping the buffer a rounding error next to the model
/// activations it now has to share a machine with.
pub const DEFAULT_ANALYSIS_CACHE_FLUSH_BYTES: u64 = 32 * 1024 * 1024;

/// Files analyzed per thread per stripe (see the striped analysis pipeline in
/// `index_repository_with_cancellation`). Eight gives the parallel stage
/// enough work to amortise thread dispatch without letting one stripe's
/// unmerged output grow large.
const ANALYSIS_STRIPE_PER_THREAD: usize = 8;

/// Floor on the analysis stripe size, in files. The stripe bounds how many
/// files' analysis output (retrieval text plus structural captures) is live
/// and unmerged at once: 32 files × the measured ~1.8 KB average unit text is
/// tens of kilobytes, and even pathological files (5 MB of minified JSON) keep
/// a stripe in the tens of megabytes — versus the whole corpus at once before
/// striping.
const ANALYSIS_STRIPE_MIN_FILES: usize = 32;

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            repository_name: DEFAULT_REPOSITORY_NAME.to_owned(),
            // Fail-closed per-file unit ceiling (r01). Sized from the measured
            // worst case in the target corpora (~9,700 units for a 4.5 MiB
            // JSON export) with ~10x headroom. Exceeding it aborts the run
            // via IndexError::IncompleteAnalysis — never silently truncates.
            // r03 replaces this ceiling with bounded staging/spilling.
            max_units_per_file: 100_000,
            refresh_existing: true,
            structural: true,
            analysis_cache_flush_bytes: DEFAULT_ANALYSIS_CACHE_FLUSH_BYTES,
            analyzers: attic_analyzers::AnalyzerSelection::all(),
            analysis_threads: 0,
        }
    }
}

impl IndexOptions {
    /// Options derived from `attic.toml [indexing]`. Every field the file
    /// does not set keeps its library default.
    pub fn from_config(cfg: &attic_core::config::IndexingOverride) -> Self {
        let defaults = Self::default();
        Self {
            structural: cfg.structural,
            max_units_per_file: cfg
                .max_units_per_file
                .unwrap_or(defaults.max_units_per_file),
            analysis_threads: cfg.analysis_threads.unwrap_or(0),
            analyzers: attic_analyzers::AnalyzerSelection::new(
                &cfg.analyzers,
                &cfg.disabled_analyzers,
            ),
            ..defaults
        }
    }

    /// Fail-closed validation, for callers that want configuration errors at
    /// startup rather than on the first indexing run.
    pub fn validate(&self) -> Result<(), IndexError> {
        if self.max_units_per_file == 0 {
            return Err(IndexError::AnalyzerConfig(
                "max_units_per_file must be >= 1".into(),
            ));
        }
        attic_analyzers::PluginCatalog::builtin()
            .validate(&self.analyzers)
            .map_err(|e| IndexError::AnalyzerConfig(e.to_string()))
    }

    /// Version stamp for PR-7 analysis-cache rows: the registry version plus
    /// the effective analyzer plugin set, so a retry after the analyzer
    /// configuration changed never replays output produced by different
    /// analyzers.
    fn analysis_cache_version(&self) -> Result<String, IndexError> {
        let plugins = if self.structural {
            attic_analyzers::PluginCatalog::builtin()
                .fingerprint(&self.analyzers)
                .map_err(|e| IndexError::AnalyzerConfig(e.to_string()))?
        } else {
            "generic".to_owned()
        };
        Ok(format!(
            "{}|{plugins}",
            attic_core::constants::ANALYZER_REGISTRY_VERSION
        ))
    }
}

/// Resolve the effective analysis worker count.
///
/// Clamped to the number of files so a two-file repository does not spawn
/// sixteen threads, and to at least 1 so the pipeline always makes progress on
/// a platform that cannot report parallelism.
fn resolve_analysis_threads(requested: usize, file_count: usize) -> usize {
    let available = if requested > 0 {
        requested
    } else {
        // r08: reserve two logical processors for the developer's foreground
        // work — analysis must never claim the whole machine by default.
        std::thread::available_parallelism()
            .map_or(1, std::num::NonZeroUsize::get)
            .saturating_sub(2)
            .max(1)
    };
    available.clamp(1, file_count.max(1))
}

/// Per-stage wall-clock and memory profile for one indexing run (Phase 0
/// instrumentation).
///
/// Exists so "which stage is slow" and "how much RAM did this run actually
/// need" are answerable from an `IndexResult` instead of from server logs or
/// guesswork. Every field is a measurement, never a limit: nothing in the
/// pipeline reads these values back to make a decision.
///
/// The `*_peak_bytes` high-water marks are the live sizes of the whole-repo
/// accumulators. They are the numbers that must stay flat as repository size
/// grows once batched publication lands; a run whose peaks scale with repo
/// size is the defect these fields exist to expose.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct IndexStageTimings {
    /// Discovery walk: gitignore-aware traversal, manifest hashing, secret
    /// classification (`attic_discovery::discover_with_cancellation`).
    pub discovery_ms: u64,
    /// Building the eligible `FileRecord` set (identity lookup/allocation).
    pub collect_ms: u64,
    /// Per-file analysis: cache lookup, read, analyzer dispatch, chunking.
    pub analysis_ms: u64,
    /// Structural resolution (`StructuralPipeline::finish`) plus assembly of
    /// the publication payloads.
    pub resolve_ms: u64,
    /// The coordinated writer-queue publication itself.
    pub publish_ms: u64,
    /// Whole-run wall clock, including time not attributed to any stage above.
    pub total_ms: u64,
    /// Process RSS sampled before the run starts, in MiB. `None` when the
    /// platform does not expose process memory.
    pub rss_start_mib: Option<u64>,
    /// Highest process RSS observed during the run, in MiB. Sampled at stage
    /// boundaries and at each accumulator high-water mark, so it is a floor on
    /// true peak, never an overestimate.
    pub rss_peak_mib: Option<u64>,
    /// High-water mark of `pending_units` retrieval text held in memory.
    ///
    /// Monotonic within a run, and deliberately so: the whole generation's
    /// retrieval text must be resident when the single atomic publication
    /// transaction runs, so nothing drains `pending_units` before then.
    /// Measured against real repositories this stays in the single-digit
    /// megabytes; see [`PENDING_UNITS_ADVISORY_BYTES`] for the size at which
    /// splitting that transaction would start to pay for its risk.
    pub pending_units_peak_bytes: u64,
    /// High-water mark of the serialized `cache_writes` buffer — the PR-7
    /// analysis-cache copy, which holds a second, JSON-escaped copy of the
    /// same text that `pending_units` already holds.
    ///
    /// A true high-water mark, not a run total: the live counter behind it is
    /// reset on every flush (see `IndexOptions::analysis_cache_flush_bytes`),
    /// so this value is bounded by that threshold plus one file's output
    /// rather than by repository size.
    pub cache_writes_peak_bytes: u64,
    /// Number of retrieval units held at the `pending_units` high-water mark.
    pub pending_units_peak_count: usize,
    /// How many times the analysis-cache buffer was flushed mid-run because it
    /// reached `IndexOptions::analysis_cache_flush_bytes`. `0` means the whole
    /// run fit in one buffer — the bound never engaged.
    pub cache_writes_flushes: u64,
    /// Worker threads actually used for the per-file analysis stage. `1` means
    /// the run executed sequentially (single file, single core, or an explicit
    /// `analysis_threads = 1`).
    pub analysis_threads_used: usize,
    /// High-water mark of retrieval-text bytes held in *unmerged* analysis
    /// output. Analysis runs in stripes that are merged and dropped before the
    /// next stripe starts, so this is bounded by one stripe
    /// (`ANALYSIS_STRIPE_MIN_FILES` / `ANALYSIS_STRIPE_PER_THREAD`) rather than
    /// by repository size. Counts retrieval text only; structural captures,
    /// when enabled, are additional.
    pub analysis_inflight_peak_bytes: u64,
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
    /// Discovery-walk explainability counters (PR-3): directories visited,
    /// files seen/eligible, and why anything was excluded — see
    /// [`attic_discovery::WalkCounters`]. Surfaced through status/MCP so
    /// "why was X skipped" is answerable without reading server logs.
    pub discovery_counters: attic_discovery::WalkCounters,
    /// PR-8 measurement: bytes read while analyzing SMALL files this run
    /// (skips cache hits — see PR-7 — since those never re-read the file).
    /// Compare against `discovery_counters.small_file_bytes_read` to see
    /// the actual size of the discovery/analysis duplicate-read for this
    /// repository, before deciding whether eliminating it is worth the
    /// added complexity.
    pub analysis_small_file_bytes_read: u64,
    /// Number of SMALL files actually re-read during analysis this run
    /// (companion to `analysis_small_file_bytes_read`).
    pub analysis_small_file_reads: u64,
    /// Non-fatal discovery-walk diagnostics (submodule boundaries, symlink
    /// escapes/cycles, IO errors, exemption rejections) — the human-readable
    /// counterpart to `discovery_counters`, so "why was X skipped" doesn't
    /// require reading server logs. See [`attic_discovery::Diagnostic`].
    pub discovery_diagnostics: Vec<attic_discovery::Diagnostic>,
    /// Phase 0 instrumentation: per-stage wall clock, process RSS, and
    /// accumulator high-water marks for this run. Measurement only — no
    /// pipeline decision reads these back.
    pub stage_timings: IndexStageTimings,
}

/// Tracks the highest process RSS seen across a run.
///
/// Sampling is explicit and cheap-ish (`sample_process_rss_mib` refreshes one
/// process), so it happens at stage boundaries rather than per file. That
/// makes the recorded peak a floor on the true peak, which is the safe
/// direction for a budget check: it can under-report, never over-report.
#[derive(Debug, Default)]
struct RssTracker {
    start_mib: Option<u64>,
    peak_mib: Option<u64>,
}

impl RssTracker {
    fn start() -> Self {
        let now = attic_storage::sample_process_rss_mib();
        Self {
            start_mib: now,
            peak_mib: now,
        }
    }

    /// Sample RSS now and retain it if it is a new high-water mark.
    fn sample(&mut self) {
        if let Some(now) = attic_storage::sample_process_rss_mib() {
            self.peak_mib = Some(self.peak_mib.map_or(now, |p| p.max(now)));
        }
    }
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
///
/// PR-7: also the unit cached by `index_analysis_cache` so a full-index
/// retry can reuse a file's units instead of re-analyzing it. `Serialize`/
/// `Deserialize` support that cache only.
#[derive(serde::Serialize, serde::Deserialize)]
struct PendingUnit {
    file_occurrence_id: String,
    retrieval_text: String,
    /// Canonical body for semantic hashing/embedding (r03); `None` means
    /// canonical == retrieval_text. `serde(default)` keeps cache rows written
    /// before this field existed deserializable.
    #[serde(default)]
    canonical_text: Option<String>,
    /// Per-occurrence provenance JSON (JSON pointer, environment); never
    /// hashed or embedded.
    #[serde(default)]
    occurrence_metadata: Option<String>,
    analyzer_id: String,
    analyzer_version: String,
    start_line: Option<u32>,
    end_line: Option<u32>,
    is_redacted: bool,
    /// Phase 3 — structural node index this unit is anchored to.
    structural_node_index: Option<usize>,
}

/// Outcome of preparing/analyzing one file — the single contract shared by
/// full and incremental indexing so the two pipelines cannot diverge on
/// Redacted/ScanSkipped handling or security-metadata propagation again.
///
/// `Err(IndexError)` from [`analyze_single_file`] is reserved for genuinely
/// transient conditions (the file may be indexable on a later retry);
/// permanent, deterministic outcomes are always `Ok(FilePrep::Skip)`.
pub(crate) enum FilePrep {
    /// The file was analyzed and produced indexable content (possibly zero
    /// retrieval units, e.g. an empty file).
    Indexable {
        units: Vec<PendingUnit>,
        // Boxed: `CapturedFile` is large enough (structural nodes/symbols/
        // rels/imports) that an unboxed `Option` here would make every
        // `FilePrep` at least as big as the largest variant, even for the
        // common `Skip` case which carries no data at all.
        captured: Option<Box<structural_pipeline::CapturedFile>>,
        security_state: SecurityState,
        is_partial_scan: bool,
    },
    /// The file is permanently ineligible for indexing this run (excluded,
    /// or content that cannot be read as text — binary/invalid UTF-8, e.g.
    /// PDF/DOCX). Terminal for this content: callers must never retry it,
    /// and must retire (tombstone) any prior occurrence for the same path
    /// rather than leaving it stale. Always corresponds to
    /// `SecurityState::Skipped`.
    Skip,
}

/// Map a resolved secrets-layer decision to the persisted security state.
fn security_state_for_decision(decision: &SecretScanDecision) -> SecurityState {
    match decision {
        SecretScanDecision::Safe => SecurityState::Clean,
        SecretScanDecision::Redacted => SecurityState::Flagged,
        SecretScanDecision::PartialScan => SecurityState::Pending,
        SecretScanDecision::Excluded => SecurityState::Skipped,
    }
}

/// Build a `Deleted`/`Skipped` tombstone occurrence for `repo_relative`.
///
/// The single shared tombstone contract for full and incremental indexing —
/// every call site must go through this so the shape (and fields like
/// `content_hash`) cannot silently drift between the two pipelines.
/// `prior_content_hash` should be the best hash available for this path
/// (the file's current on-disk hash if it still exists but is being retired
/// as permanently unsupported/excluded, or the last-known hash if the path
/// is gone entirely); pass `String::new()` only when truly unknown.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_tombstone(
    repo_id: RepositoryId,
    rev_id: SourceRevisionId,
    gen_id: IndexGenerationId,
    identity_id: FileIdentityId,
    stable_id_basis: String,
    repo_relative: String,
    prior_content_hash: String,
) -> PublicationFile {
    PublicationFile {
        identity_id,
        identity_repository_id: repo_id,
        stable_id_basis,
        occurrence: PublicationOccurrence {
            id: FileOccurrenceId::new_v4(),
            source_revision_id: rev_id,
            index_generation_id: Some(gen_id),
            path: repo_relative,
            content_hash: prior_content_hash,
            size_bytes: 0,
            language: None,
            file_type: FileType::Other,
            discovery_class: DiscoveryClass::Vcs,
            security_state: SecurityState::Skipped,
            existence_state: ExistenceState::Deleted,
        },
    }
}

/// Retire the current occurrence of `path` (deleted from disk, newly
/// excluded, or newly permanently unsupported): look up its prior
/// identity/occurrence/content-hash and, if one exists, schedule the old
/// occurrence's units/structural rows for deletion and publish a `Deleted`
/// tombstone occurrence in its place. A genuine no-op (nothing pushed) if
/// the path has no prior occurrence — there is nothing to retire.
fn retire_path(
    store: &IndexingStore<'_>,
    repo_id: RepositoryId,
    rev_id: SourceRevisionId,
    gen_id: IndexGenerationId,
    repo_relative: &str,
    stale_occurrences: &mut Vec<String>,
    tombstones: &mut Vec<PublicationFile>,
) -> Result<(), IndexError> {
    let Some(snap) = store
        .readers
        .with_reader(|c| lookup_occurrence_snapshot(c, &repo_id, repo_relative))
        .map_err(IndexError::Storage)?
    else {
        // No prior occurrence: genuinely nothing to retire.
        return Ok(());
    };
    stale_occurrences.push(snap.id.clone());
    let identity_id: FileIdentityId = snap
        .file_identity_id
        .parse()
        .unwrap_or_else(|_| FileIdentityId::new_v4());
    let repo_id_str = repo_id.to_string_repr();
    tombstones.push(build_tombstone(
        repo_id,
        rev_id,
        gen_id,
        identity_id,
        format!("{repo_id_str}/{repo_relative}"),
        repo_relative.to_owned(),
        snap.content_hash,
    ));
    Ok(())
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
    index_repository_with_cancellation(store, root, policy, opts, &CancellationToken::default())
}

/// Full authoritative indexing with cooperative cancellation. Cancellation is
/// checked throughout discovery/analysis and immediately before publication,
/// so a cancelled run can never publish a new CURRENT generation.
pub fn index_repository_with_cancellation(
    store: &IndexingStore<'_>,
    root: &Path,
    policy: &DiscoveryPolicy,
    opts: &IndexOptions,
    cancellation: &CancellationToken,
) -> Result<IndexResult, IndexError> {
    if cancellation.is_cancelled() {
        return Err(IndexError::Cancelled);
    }
    // Phase 0 instrumentation. `run_start` anchors `total_ms`; `stage_start`
    // is reset at each stage boundary. Both are measurement-only.
    let run_start = std::time::Instant::now();
    let mut timings = IndexStageTimings::default();
    let mut rss = RssTracker::start();
    let mut stage_start = run_start;

    // 1. Phase 1B discovery — real manifest hash, git meta, security classification.
    let discovery = attic_discovery::discover_with_cancellation(root, policy, cancellation)?;
    timings.discovery_ms = stage_start.elapsed().as_millis() as u64;
    rss.sample();
    stage_start = std::time::Instant::now();

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
    // Display name: an explicit `repository_name`, else the root folder's
    // name. Every server-indexed repository used to be stored as "default",
    // so the status of a 21-repository workspace was unreadable. Existing
    // rows still carrying a different name are renamed on their next run.
    let display_name = repository_display_name(root, opts);
    let stored_name: Option<String> = match existing_repo_id {
        Some(id) => store
            .readers
            .with_reader(|c| attic_storage::get_repository_display_name(c, &id))
            .map_err(IndexError::Storage)?,
        None => None,
    };
    let repo_upsert = match existing_repo_id {
        Some(_) if stored_name.as_deref() == Some(display_name.as_str()) => None,
        _ => Some((root_str.to_string(), display_name)),
    };

    // PR-7: persist the repository row now, before analysis, rather than
    // waiting for the end-of-run `submit_index_publication`. Two reasons:
    // (1) `index_analysis_cache` has a FK on `repository_id`, so a cache
    //     write for a brand-new repository's first attempt would otherwise
    //     fail outright; (2) without a persisted row, a brand-new
    //     repository's `repo_id` is a fresh random UUID every attempt
    //     (nothing to look up by root path yet), so a retry could never
    //     find the previous attempt's cache at all. `upsert_repository` is
    //     idempotent — `submit_index_publication`'s own upsert later is a
    //     harmless no-op re-write of the same row.
    if let Some((ref root, ref display_name)) = repo_upsert {
        #[cfg(test)]
        fire_indexing_test_hook(IndexingTestHookPoint::BeforeRepositoryUpsert);
        if cancellation.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
        let root = root.clone();
        let display_name = display_name.clone();
        let cancellation = cancellation.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        store
            .writer
            .send(move |conn| {
                if cancellation.is_cancelled() {
                    let _ = tx.send(false);
                    return Ok(());
                }
                attic_storage::upsert_repository(conn, &repo_id, &root, &display_name)?;
                let _ = tx.send(true);
                Ok(())
            })
            .map_err(IndexError::Storage)?;
        let wrote_row = rx.try_recv().unwrap_or(false);
        if !wrote_row {
            return Err(IndexError::Cancelled);
        }
    }

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
        discovery_counters: discovery.counters,
        discovery_diagnostics: discovery.diagnostics.clone(),
        ..Default::default()
    };

    // 5. Collect eligible file records.  All lookups below read COMMITTED
    //    state from previous runs (via the read pool); nothing here depends
    //    on this run's own writes, so pre-publication reads are correct.
    let mut file_records: Vec<FileRecord> = Vec::new();

    // Authoritative reconciliation bookkeeping (P0-4): a full index run must
    // retire any previously-active path that is no longer part of current
    // indexable truth (deleted from disk, newly excluded, or newly
    // permanently unsupported) rather than silently leaving its old
    // occurrence/units/structural rows as searchable "current" content.
    // `stale_occurrences` therefore accumulates ids to retire from THREE
    // sources: (a) a path successfully reindexed (below), (b) a path
    // permanently skipped this run that had prior content, and (c) a path
    // that vanished from discovery entirely (handled after this loop).
    //
    // Generation-completeness invariant (Phase 6.4): every discovered path
    // must reach an explicit terminal state — INDEXED, INTENTIONALLY_SKIPPED,
    // or REMOVED/EXCLUDED — before this run's generation may be published as
    // authoritative/current. A transient/retryable failure (stat race, I/O
    // hiccup, unstable read) is none of those; `transient_failed_paths`
    // collects every such path, and if it is non-empty when the analysis
    // loop finishes, the ENTIRE run aborts with `IndexError::TransientFailures`
    // BEFORE `submit_index_publication` is ever called — the previous
    // generation (if any) is left completely untouched and stays current.
    // This mirrors the incremental pipeline's existing "no silent fallback,
    // whole batch aborts for scheduler retry" contract instead of publishing
    // a generation that mixes verified-current content with content nobody
    // actually verified this run.
    let previous_active: HashSet<String> = store
        .readers
        .with_reader(|c| latest_active_paths_for_repository(c, &repo_id))
        .map_err(IndexError::Storage)?
        .into_iter()
        .collect();
    let mut current_entry_paths: HashSet<String> = HashSet::new();
    let mut stale_occurrences: Vec<String> = Vec::new();
    let mut tombstones: Vec<PublicationFile> = Vec::new();
    let mut transient_failed_paths: Vec<String> = Vec::new();

    // PR-5: preload existing index state for the whole repository in two
    // bulk queries, instead of two `with_reader` round trips per discovered
    // file below. Identity/occurrence semantics are unchanged — this only
    // moves the same lookups from per-file to per-run.
    let existing_identities: HashMap<String, FileIdentityId> = store
        .readers
        .with_reader(|c| bulk_file_identities_for_repository(c, &repo_id))
        .map_err(IndexError::Storage)?;
    let existing_occurrences: HashMap<String, String> = store
        .readers
        .with_reader(|c| bulk_latest_occurrence_ids_for_repository(c, &repo_id))
        .map_err(IndexError::Storage)?;

    let aligned_classifications =
        align_classifications(&discovery.entries, &discovery.downstream_classifications)?;

    for (entry, classification) in discovery.entries.iter().zip(aligned_classifications) {
        if cancellation.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
        result.files_visited += 1;
        current_entry_paths.insert(entry.repo_relative.clone());

        let classification = Some(classification);

        if matches!(
            classification,
            Some(DownstreamClassification::Excluded)
                | Some(DownstreamClassification::ScanSkipped { .. })
        ) {
            debug!(path = %entry.repo_relative, classification = ?classification, "skipping excluded/scan-skipped file");
            result.files_skipped += 1;
            if previous_active.contains(&entry.repo_relative) {
                retire_path(
                    store,
                    repo_id,
                    rev_id,
                    gen_id,
                    &entry.repo_relative,
                    &mut stale_occurrences,
                    &mut tombstones,
                )?;
            }
            continue;
        }

        if let Some(DownstreamClassification::ScanTransientError { reason }) = classification {
            // NOT a content verdict — never tombstone, never treat as a
            // permanent skip. Record for the completeness gate below.
            warn!(
                path = %entry.repo_relative,
                reason = %reason,
                "discovery classification transient error; this run cannot become current until resolved"
            );
            transient_failed_paths.push(entry.repo_relative.clone());
            continue;
        }

        let file_meta = match std::fs::metadata(&entry.abs_path) {
            Ok(m) => m,
            Err(e) => {
                // Transient (the path was just discovered on disk moments
                // ago; a stat failure now is a race, not a permanent
                // condition). Never falsely tombstone/delete on this — record
                // it and let the generation-completeness gate below decide.
                warn!(
                    path = %entry.repo_relative,
                    error = %e,
                    "stat failed (transient); this run cannot become current until resolved"
                );
                transient_failed_paths.push(entry.repo_relative.clone());
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

        // Reuse the same UUID across reindex runs — resolved in memory from
        // the bulk-preloaded map instead of a per-file DB round trip.
        let fi_id: FileIdentityId = existing_identities
            .get(&stable_id_basis)
            .copied()
            .unwrap_or_else(FileIdentityId::new_v4);

        // Any existing file_occurrence for this path/repo — also resolved
        // in memory from the bulk-preloaded map.
        let old_fo_id: Option<String> = existing_occurrences.get(&entry.repo_relative).cloned();

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

    // 5b. Paths previously active but absent from this run's discovery
    // entirely (deleted from disk, or newly outside the walk/policy) —
    // retire them too. This is the other half of P0-4: the loop above only
    // ever sees paths still present on disk.
    for path in previous_active.difference(&current_entry_paths) {
        if cancellation.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
        retire_path(
            store,
            repo_id,
            rev_id,
            gen_id,
            path,
            &mut stale_occurrences,
            &mut tombstones,
        )?;
    }

    // 6. Run Phase 1C analysis per file.  Produces pending units only — no
    //    database writes happen during analysis.
    //
    // Phase 0: close the "collect eligible records" stage here — everything
    // above this point is discovery-derived bookkeeping, everything below is
    // per-file analysis.
    timings.collect_ms = stage_start.elapsed().as_millis() as u64;
    rss.sample();
    stage_start = std::time::Instant::now();
    //
    // PR-7: bulk-load the analysis cache from any prior attempt at this
    // repository (one query, same bulk-preload shape as PR-5). A cache hit
    // (same path, same content hash) skips re-running the analyzer entirely
    // — this is what makes a retry after a transient failure cheap instead
    // of re-analyzing every file in the repository again.
    let analysis_cache: HashMap<String, attic_storage::CachedFileAnalysis> = store
        .readers
        .with_reader(|c| attic_storage::bulk_load_analysis_cache(c, &repo_id))
        .map_err(IndexError::Storage)?;
    let mut cache_writes: Vec<attic_storage::CachedFileAnalysis> = Vec::new();

    let registry = structural_pipeline::shared_registry(opts)?;
    let cache_version = opts.analysis_cache_version()?;
    let mut pending_units: Vec<PendingUnit> = Vec::new();
    let mut indexed_records: Vec<FileRecord> = Vec::new();
    // Phase 0: live byte counts for the two whole-repo text accumulators.
    // These only feed `timings`; nothing reads them to make a decision.
    let mut pending_units_live_bytes: u64 = 0;
    let mut cache_writes_live_bytes: u64 = 0;
    let mut pipeline = structural_pipeline::StructuralPipeline::new(
        root,
        discovery
            .manifest
            .entries
            .iter()
            .map(|e| e.repo_relative.clone())
            .collect(),
    );

    // ── Analysis: striped analyze → merge pipeline ───────────────────────
    //
    // `analyze_single_file` is pure — it reads one path, dispatches to a
    // `Send + Sync` analyzer, and returns owned data. It performs no database
    // access, so parallelising it cannot interact with the coordinated-writer
    // contract. The shared inputs (`analysis_cache`, `registry`, `opts`,
    // `policy_hash`) are all read-only here.
    //
    // Analysis runs in stripes: one stripe is analyzed in parallel, merged
    // below, and dropped before the next stripe is analyzed. Analyzing the
    // whole corpus before merging anything held every file's `FilePrep`
    // (retrieval text plus structural captures) resident simultaneously — a
    // peak that grew with repository size, in a pipeline whose purpose is the
    // opposite. The stripe bounds live preps to `stripe_size` files; stripes
    // are contiguous slices of the discovery order merged in sequence, so the
    // merge observes exactly the order a single pass would produce.
    let analysis_threads = resolve_analysis_threads(opts.analysis_threads, file_records.len());
    timings.analysis_threads_used = analysis_threads;

    let stripe_size = analysis_threads
        .saturating_mul(ANALYSIS_STRIPE_PER_THREAD)
        .max(ANALYSIS_STRIPE_MIN_FILES);
    let mut stripes: Vec<Vec<FileRecord>> =
        Vec::with_capacity(file_records.len().div_ceil(stripe_size));
    let mut rest = file_records;
    while !rest.is_empty() {
        let take = stripe_size.min(rest.len());
        let tail = rest.split_off(take);
        stripes.push(rest);
        rest = tail;
    }

    for stripe in stripes {
        if cancellation.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
        let prepped = analyze_files(
            stripe,
            &AnalysisCacheLookup {
                entries: &analysis_cache,
                policy_hash: &policy_hash,
                version: &cache_version,
            },
            &registry,
            opts,
            cancellation,
            analysis_threads,
        )?;

        // High-water mark of unmerged analysis output: bounded by one stripe,
        // not by repository size. Retrieval text only; structural captures
        // (when `opts.structural` is on) are additional.
        let stripe_prep_bytes: u64 = prepped
            .iter()
            .map(|f| match &f.prep {
                Ok(FilePrep::Indexable { units, .. }) => units
                    .iter()
                    .map(|u| u.retrieval_text.len() as u64)
                    .sum::<u64>(),
                _ => 0,
            })
            .sum();
        timings.analysis_inflight_peak_bytes =
            timings.analysis_inflight_peak_bytes.max(stripe_prep_bytes);
        rss.sample();

        // ── Merge this stripe, in discovery order ────────────────────────────
        for prepped_file in prepped {
            if cancellation.is_cancelled() {
                return Err(IndexError::Cancelled);
            }
            let AnalyzedFile {
                mut rec,
                prep,
                was_cache_hit,
                small_file_bytes_read,
            } = prepped_file;

            // PR-8 measurement: a fresh analysis of a SMALL file re-reads content
            // discovery already read once (see
            // `discovery_counters.small_file_bytes_read`). Cache hits never
            // re-read anything, so they report `None` here.
            if let Some(bytes) = small_file_bytes_read {
                result.analysis_small_file_bytes_read += bytes;
                result.analysis_small_file_reads += 1;
            }

            match prep {
                Ok(FilePrep::Indexable {
                    mut units,
                    captured,
                    security_state,
                    is_partial_scan,
                }) => {
                    if opts.refresh_existing
                        && let Some(old) = rec.old_fo_id.clone()
                    {
                        stale_occurrences.push(old);
                    }
                    rec.security_state = security_state;
                    rec.is_partial_scan = is_partial_scan;
                    pipeline.note_occurrence(&rec.repo_relative, &rec.fo_id.to_string_repr());

                    // Stash this file's result for potential cache persistence
                    // BEFORE `units`/`captured` are consumed below — serializing
                    // by reference here needs no clone of the analysis output.
                    // If this run later hits a transient failure elsewhere,
                    // whatever succeeded is written in one batch by the
                    // completeness gate below. Skipped for cache hits: the
                    // existing `index_analysis_cache` row already has this exact
                    // content_hash, so rewriting it would be a no-op.
                    if !was_cache_hit && let Ok(units_json) = serde_json::to_string(&units) {
                        let captured_json = captured
                            .as_ref()
                            .and_then(|c| serde_json::to_string(c).ok());
                        // Phase 0: this is the second in-memory copy of the same
                        // text `pending_units` already holds, JSON-escaped. Track
                        // it so its true cost is visible rather than inferred.
                        cache_writes_live_bytes += (units_json.len()
                            + captured_json.as_ref().map_or(0, |c| c.len()))
                            as u64;
                        timings.cache_writes_peak_bytes =
                            timings.cache_writes_peak_bytes.max(cache_writes_live_bytes);
                        cache_writes.push(attic_storage::CachedFileAnalysis {
                            repo_relative: rec.repo_relative.clone(),
                            content_hash: rec.content_hash.clone(),
                            security_state: security_state.as_str().to_owned(),
                            is_partial_scan,
                            secret_pattern_version: SECRET_PATTERN_VERSION,
                            analyzer_registry_version: cache_version.clone(),
                            discovery_policy_hash: policy_hash.clone(),
                            structural: opts.structural,
                            max_units_per_file: opts.max_units_per_file as u64,
                            units_json,
                            captured_json,
                        });

                        // Bound the buffer. See
                        // `IndexOptions::analysis_cache_flush_bytes` for why
                        // flushing this particular accumulator early is safe
                        // (pure retry cache, no bearing on which generation is
                        // CURRENT) while batching the publication is not.
                        if opts.analysis_cache_flush_bytes > 0
                            && cache_writes_live_bytes >= opts.analysis_cache_flush_bytes
                        {
                            flush_analysis_cache(store, repo_id, &mut cache_writes)?;
                            cache_writes_live_bytes = 0;
                            timings.cache_writes_flushes += 1;
                            rss.sample();
                        }
                    }

                    if let Some(captured) = captured {
                        pipeline.record(*captured);
                    }
                    // Phase 0: accumulate BEFORE `append` moves `units` away.
                    pending_units_live_bytes += units
                        .iter()
                        .map(|u| u.retrieval_text.len() as u64)
                        .sum::<u64>();
                    pending_units.append(&mut units);
                    if pending_units_live_bytes > timings.pending_units_peak_bytes {
                        timings.pending_units_peak_bytes = pending_units_live_bytes;
                        timings.pending_units_peak_count = pending_units.len();
                    }
                    indexed_records.push(rec);
                    result.files_indexed += 1;
                }
                Ok(FilePrep::Skip) => {
                    // Permanent, deterministic skip discovered only during
                    // analysis (e.g. content changed to binary/invalid-UTF-8
                    // since discovery ran). Retire any prior occurrence — never
                    // leave it advertised as current truth (P0-3/P0-5).
                    debug!(path = %rec.repo_relative, "file permanently skipped during analysis");
                    result.files_skipped += 1;
                    if let Some(old) = rec.old_fo_id.clone() {
                        stale_occurrences.push(old);
                        tombstones.push(build_tombstone(
                            repo_id,
                            rev_id,
                            gen_id,
                            rec.fi_id,
                            rec.stable_id_basis.clone(),
                            rec.repo_relative.clone(),
                            rec.content_hash.clone(),
                        ));
                    }
                }
                Err(e) => {
                    // Transient/retryable failure: this path has not reached any
                    // terminal state this run. Do not falsely delete or publish
                    // anything for it — record it for the completeness gate
                    // below, which aborts the whole run rather than publish a
                    // generation that mixes verified content with content nobody
                    // actually verified.
                    warn!(
                        path = %rec.repo_relative,
                        error = %e,
                        "analysis failed (transient); this run cannot become current until resolved"
                    );
                    transient_failed_paths.push(rec.repo_relative.clone());
                }
            }
        }
    }

    // Phase 0: analysis stage ends here — the gate below and the payload
    // assembly after it are resolution/publication work, not analysis.
    timings.analysis_ms = stage_start.elapsed().as_millis() as u64;
    rss.sample();
    stage_start = std::time::Instant::now();

    // Generation-completeness gate (Phase 6.4): every discovered path must
    // have reached INDEXED, INTENTIONALLY_SKIPPED, or REMOVED/EXCLUDED above.
    // Any transient/retryable failure means this generation is not
    // authoritative — abort before publication so the previous generation
    // (if any) remains completely untouched and current; the scheduler is
    // expected to retry the full index later.
    if !transient_failed_paths.is_empty() {
        // PR-7: persist every successfully-analyzed file's result before
        // aborting, in ONE writer-queue submission (many statements, one
        // transaction — same shape as `submit_index_publication`), so a
        // retry does not have to re-analyze the files that already
        // succeeded. Anything already flushed mid-run by the
        // `analysis_cache_flush_bytes` bound is durable; this writes only the
        // tail that is still buffered. Purely a cache write: it does not touch
        // `core_file_occurrences`/`core_index_generations` and has no
        // effect on which generation is CURRENT.
        flush_analysis_cache(store, repo_id, &mut cache_writes)?;
        return Err(IndexError::TransientFailures {
            paths: transient_failed_paths,
        });
    }

    // 7. Publish EVERYTHING as one coordinated writer-queue mutation.
    let mut publication_files: Vec<PublicationFile> = indexed_records
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
    publication_files.extend(tombstones);

    let repo_id_str = repo_id.to_string_repr();
    let gen_id_str = gen_id.to_string_repr();
    let mut retrieval_units: Vec<PublicationRetrievalUnit> = Vec::new();
    let mut unit_links_by_occ: HashMap<String, Vec<(String, usize)>> = HashMap::new();
    for u in pending_units {
        if cancellation.is_cancelled() {
            return Err(IndexError::Cancelled);
        }
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
            canonical_text: u.canonical_text,
            occurrence_metadata: u.occurrence_metadata,
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
    if cancellation.is_cancelled() {
        return Err(IndexError::Cancelled);
    }
    let structural_files = pipeline.finish(&deps, &unit_links_by_occ);
    if cancellation.is_cancelled() {
        return Err(IndexError::Cancelled);
    }

    // Phase 0: payload assembly + structural resolution are done; what
    // follows is the writer-queue publication itself.
    timings.resolve_ms = stage_start.elapsed().as_millis() as u64;
    rss.sample();
    stage_start = std::time::Instant::now();
    #[cfg(test)]
    fire_indexing_test_hook(IndexingTestHookPoint::BeforePublication);
    if cancellation.is_cancelled() {
        return Err(IndexError::Cancelled);
    }

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

    // PR-7: this generation published successfully, so any analysis cache
    // left over from an earlier failed attempt at this repository is no
    // longer needed — clear it so the table doesn't grow unbounded. Purely
    // a cache eviction: harmless if it's already empty, and has no bearing
    // on what just became CURRENT above.
    if let Err(e) = store
        .writer
        .send(move |conn| attic_storage::clear_analysis_cache(conn, &repo_id))
    {
        warn!(
            repository_id = %repo_id,
            error = %e,
            "analysis-cache cleanup failed after successful publication; canonical index remains valid"
        );
    }

    result.units_inserted = stats.units_inserted;
    result.units_deleted = stats.units_deleted;

    // Phase 0: close the publication stage and the whole-run clock, then take
    // a final RSS sample so the peak covers the largest commit.
    timings.publish_ms = stage_start.elapsed().as_millis() as u64;
    rss.sample();
    timings.total_ms = run_start.elapsed().as_millis() as u64;
    timings.rss_start_mib = rss.start_mib;
    timings.rss_peak_mib = rss.peak_mib;
    result.stage_timings = timings;

    info!(
        files_indexed = result.files_indexed,
        files_skipped = result.files_skipped,
        units_inserted = result.units_inserted,
        units_deleted = result.units_deleted,
        repository_id = %result.repository_id,
        discovery_ms = result.stage_timings.discovery_ms,
        collect_ms = result.stage_timings.collect_ms,
        analysis_ms = result.stage_timings.analysis_ms,
        resolve_ms = result.stage_timings.resolve_ms,
        publish_ms = result.stage_timings.publish_ms,
        total_ms = result.stage_timings.total_ms,
        rss_start_mib = ?result.stage_timings.rss_start_mib,
        rss_peak_mib = ?result.stage_timings.rss_peak_mib,
        pending_units_peak_bytes = result.stage_timings.pending_units_peak_bytes,
        cache_writes_peak_bytes = result.stage_timings.cache_writes_peak_bytes,
        cache_writes_flushes = result.stage_timings.cache_writes_flushes,
        analysis_threads_used = result.stage_timings.analysis_threads_used,
        analysis_inflight_peak_bytes = result.stage_timings.analysis_inflight_peak_bytes,
        "indexing run complete"
    );

    // `pending_units` is the one accumulator still proportional to repository
    // size: the whole generation's retrieval text must be resident when the
    // single atomic publication transaction runs. Bounding it would mean
    // splitting that transaction, which trades a real corruption risk for a
    // saving that is negligible at normal repository sizes.
    //
    // Rather than guess where "normal" ends, say so out loud. Crossing this
    // threshold is the signal that staged publication has become worth its
    // risk — and until it fires, the current design is the right one.
    if result.stage_timings.pending_units_peak_bytes > PENDING_UNITS_ADVISORY_BYTES {
        warn!(
            repository_id = %result.repository_id,
            pending_units_peak_bytes = result.stage_timings.pending_units_peak_bytes,
            pending_units_peak_count = result.stage_timings.pending_units_peak_count,
            advisory_bytes = PENDING_UNITS_ADVISORY_BYTES,
            "retrieval text held for atomic publication exceeded the advisory bound; \
             this repository is large enough that staged publication would now pay for itself"
        );
    }

    Ok(result)
}

/// Point at which the whole-generation retrieval-text buffer stops being
/// negligible and starts being worth a design change (512 MiB).
///
/// Chosen from measurement, not intuition: the peak tracks the repository's
/// total indexable text, which for ordinary source trees is single-digit
/// megabytes. Reaching half a gigabyte means the repository is two orders of
/// magnitude larger than that, and the atomic-publication trade-off should be
/// revisited.
pub const PENDING_UNITS_ADVISORY_BYTES: u64 = 512 * 1024 * 1024;

/// Drain the buffered PR-7 analysis-cache entries into the database.
///
/// One writer-queue submission per call (many statements, one transaction —
/// the same shape as `submit_index_publication`). `entries` is left empty so
/// the caller's live-byte counter can be reset to zero alongside it.
///
/// A no-op when there is nothing buffered, so callers can invoke it
/// unconditionally at a flush point or on the abort path.
fn flush_analysis_cache(
    store: &IndexingStore<'_>,
    repo_id: RepositoryId,
    entries: &mut Vec<attic_storage::CachedFileAnalysis>,
) -> Result<(), IndexError> {
    if entries.is_empty() {
        return Ok(());
    }
    let batch = std::mem::take(entries);
    let now_us = incremental::now_micros();
    store
        .writer
        .send(move |conn| {
            attic_storage::upsert_analysis_cache_entries(conn, &repo_id, &batch, now_us)
        })
        .map_err(IndexError::Storage)?;
    Ok(())
}

/// One file's analysis outcome, carried from the parallel analyze stage to
/// the sequential merge stage in discovery order.
struct AnalyzedFile {
    rec: FileRecord,
    prep: Result<FilePrep, IndexError>,
    /// `true` when the PR-7 analysis cache satisfied this file and the
    /// analyzer never ran — the merge stage uses it to skip rewriting an
    /// identical cache row.
    was_cache_hit: bool,
    /// `Some(bytes)` when this file was a freshly-analyzed SMALL file, whose
    /// content analysis re-read after discovery had already read it (the PR-8
    /// duplicate-read measurement). `None` for cache hits and larger tiers.
    small_file_bytes_read: Option<u64>,
}

/// The PR-7 analysis cache as seen by one run: prior entries plus the keys an
/// entry must match to be replayed.
struct AnalysisCacheLookup<'a> {
    entries: &'a HashMap<String, attic_storage::CachedFileAnalysis>,
    policy_hash: &'a str,
    /// See [`IndexOptions::analysis_cache_version`].
    version: &'a str,
}

/// Analyze every file record, using `threads` workers, and return the results
/// in the original (discovery) order.
///
/// # Why this is safe to parallelise
///
/// [`analyze_single_file`] performs no database access whatsoever: it reads
/// one path from disk, dispatches to an analyzer (`Analyzer: Send + Sync`,
/// documented read-only during `analyze`), and returns owned data. Nothing
/// here goes near the coordinated writer, so the single-writer contract is
/// untouched.
///
/// # Scheduling
///
/// Workers pull files from one shared atomic cursor (dynamic scheduling)
/// rather than owning a fixed contiguous slice. File cost is heavily skewed
/// in real corpora — one multi-megabyte JSON export costs as much as
/// thousands of small sources — so static partitioning left most workers
/// idle while one ground through the slice that happened to hold the big
/// files. The cursor walks a largest-first permutation (longest-processing-
/// time-first), so the expensive files start immediately and the small ones
/// fill in behind them.
///
/// # Why output is deterministic
///
/// Each result is tagged with its record's original index and the results
/// are reassembled in that order, so the sequence the caller observes is
/// exactly the sequence a single-threaded run would produce. Thread count and
/// scheduling affect timing only — never ordering, never content.
///
/// Cancellation is checked per file inside each worker, so a cancelled run
/// stops promptly rather than after the whole stripe. A worker panic is
/// propagated to the caller unchanged.
fn analyze_files(
    file_records: Vec<FileRecord>,
    cache: &AnalysisCacheLookup<'_>,
    registry: &AnalyzerRegistry,
    opts: &IndexOptions,
    cancellation: &CancellationToken,
    threads: usize,
) -> Result<Vec<AnalyzedFile>, IndexError> {
    if file_records.is_empty() {
        return Ok(Vec::new());
    }

    let analyze_one = |rec: &FileRecord| -> FileAnalysis {
        // A cache hit requires the content hash AND the secret-detector /
        // analyzer-registry versions to match what's current: a retry that
        // spans a ruleset upgrade must never replay a verdict computed under
        // the old rules for unchanged content (e.g. a secret the upgraded
        // detector would now catch).
        let cache_hit = cache
            .entries
            .get(&rec.repo_relative)
            .filter(|cached| {
                cached.content_hash == rec.content_hash
                    && cached.secret_pattern_version == SECRET_PATTERN_VERSION
                    && cached.analyzer_registry_version == cache.version
                    && cached.discovery_policy_hash == cache.policy_hash
                    && cached.structural == opts.structural
                    && cached.max_units_per_file == opts.max_units_per_file as u64
            })
            .and_then(|cached| reconstruct_file_prep_from_cache(cached, rec));

        match cache_hit {
            Some(prep) => FileAnalysis {
                prep: Ok(prep),
                was_cache_hit: true,
                small_file_bytes_read: None,
            },
            None => {
                let small_file_bytes_read = (rec.size_bytes >= 0
                    && (rec.size_bytes as u64) <= attic_discovery::MAX_FULL_LOAD_BYTES)
                    .then_some(rec.size_bytes as u64);
                let prep = analyze_single_file(rec, registry, opts, cancellation);
                FileAnalysis {
                    prep,
                    was_cache_hit: false,
                    small_file_bytes_read,
                }
            }
        }
    };

    let assemble = |records: Vec<FileRecord>, results: Vec<FileAnalysis>| -> Vec<AnalyzedFile> {
        records
            .into_iter()
            .zip(results)
            .map(|(rec, a)| AnalyzedFile {
                rec,
                prep: a.prep,
                was_cache_hit: a.was_cache_hit,
                small_file_bytes_read: a.small_file_bytes_read,
            })
            .collect()
    };

    let threads = threads.clamp(1, file_records.len());
    if threads == 1 {
        let mut results = Vec::with_capacity(file_records.len());
        for rec in &file_records {
            if cancellation.is_cancelled() {
                return Err(IndexError::Cancelled);
            }
            results.push(analyze_one(rec));
        }
        return Ok(assemble(file_records, results));
    }

    // Largest-first permutation; the index tie-break keeps it deterministic.
    let mut work_order: Vec<usize> = (0..file_records.len()).collect();
    work_order.sort_by(|&a, &b| {
        file_records[b]
            .size_bytes
            .cmp(&file_records[a].size_bytes)
            .then(a.cmp(&b))
    });

    let cursor = std::sync::atomic::AtomicUsize::new(0);
    let records = &file_records;
    let work_order = &work_order;
    let cursor = &cursor;
    let analyze_one = &analyze_one;
    let tagged: Vec<Vec<(usize, FileAnalysis)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(move || {
                    let mut local = Vec::new();
                    loop {
                        if cancellation.is_cancelled() {
                            break;
                        }
                        let next = cursor.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(&idx) = work_order.get(next) else {
                            break;
                        };
                        local.push((idx, analyze_one(&records[idx])));
                    }
                    // Hand the worker's analysis tally back with its results so
                    // the spawning thread can attribute it (test-only; see
                    // ANALYZE_SINGLE_FILE_CALLS).
                    #[cfg(test)]
                    let local = (local, take_analyze_single_file_calls());
                    local
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                // A worker panic is a bug, not a transient condition: analysis
                // is pure and every expected failure is already modelled as
                // `FilePrep::Skip` or `IndexError`. Propagating restores the
                // single-threaded behaviour of unwinding out of the run.
                let joined = h
                    .join()
                    .unwrap_or_else(|payload| std::panic::resume_unwind(payload));
                #[cfg(test)]
                let joined = {
                    let (results, calls) = joined;
                    add_analyze_single_file_calls(calls);
                    results
                };
                joined
            })
            .collect()
    });

    // A worker that observed cancellation stops early, so some slots stay
    // empty. Surface it as the cancellation it is rather than publishing an
    // incomplete generation.
    if cancellation.is_cancelled() {
        return Err(IndexError::Cancelled);
    }

    let mut slots: Vec<Option<FileAnalysis>> = Vec::with_capacity(file_records.len());
    slots.resize_with(file_records.len(), || None);
    for (idx, analysis) in tagged.into_iter().flatten() {
        slots[idx] = Some(analysis);
    }
    let results: Option<Vec<FileAnalysis>> = slots.into_iter().collect();
    match results {
        Some(results) => Ok(assemble(file_records, results)),
        // Unreachable without cancellation (every index below `len` is
        // claimed exactly once), but never publish a partial stripe.
        None => Err(IndexError::Cancelled),
    }
}

/// One file's analysis result before it is re-paired with its record.
struct FileAnalysis {
    prep: Result<FilePrep, IndexError>,
    was_cache_hit: bool,
    small_file_bytes_read: Option<u64>,
}

/// Reconstruct a cached analysis result for reuse (PR-7 cache hit).
///
/// Returns `None` if the cached entry can't be read back — a corrupt or
/// unexpectedly-shaped cache row must never become a hard indexing failure;
/// the caller falls back to normal analysis exactly as if there had been no
/// cache entry at all.
///
/// `units`/`captured` are retargeted to this run's freshly generated
/// `file_occurrence_id`: the cached value was serialized against a
/// *previous*, never-published attempt's occurrence id, which must not leak
/// into this run's publication.
fn reconstruct_file_prep_from_cache(
    cached: &attic_storage::CachedFileAnalysis,
    rec: &FileRecord,
) -> Option<FilePrep> {
    let mut units: Vec<PendingUnit> = serde_json::from_str(&cached.units_json).ok()?;
    let fo_id_str = rec.fo_id.to_string_repr();
    for unit in &mut units {
        unit.file_occurrence_id = fo_id_str.clone();
    }

    let captured = match &cached.captured_json {
        Some(json) => {
            let mut c: structural_pipeline::CapturedFile = serde_json::from_str(json).ok()?;
            c.retarget_file_occurrence_id(fo_id_str);
            Some(Box::new(c))
        }
        None => None,
    };

    let security_state = SecurityState::from_db_str(&cached.security_state).ok()?;

    Some(FilePrep::Indexable {
        units,
        captured,
        security_state,
        is_partial_scan: cached.is_partial_scan,
    })
}

// ---------------------------------------------------------------------------
// Per-file analysis (pure — no database writes)
// ---------------------------------------------------------------------------

// Test-only counter of `analyze_single_file` invocations (PR-7): proves a
// cache hit genuinely skips the analyzer rather than merely producing the
// same output by coincidence. Compiled out entirely in non-test builds.
//
// Thread-local, because libtest runs tests concurrently in one process and a
// process-global counter would let any other running test's analyses inflate
// this one's count. Analysis now runs on a worker pool, so workers report
// their own tallies back to the thread that spawned them (see
// `take_analyze_single_file_calls` and its use in `analyze_files`), keeping
// every count attributed to the test that caused it.
#[cfg(test)]
thread_local! {
    static ANALYZE_SINGLE_FILE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_analyze_single_file_calls() {
    ANALYZE_SINGLE_FILE_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
fn analyze_single_file_calls() -> usize {
    ANALYZE_SINGLE_FILE_CALLS.with(std::cell::Cell::get)
}

/// Read and clear the calling thread's tally. Used by an analysis worker to
/// hand its count back to the thread that spawned it.
#[cfg(test)]
fn take_analyze_single_file_calls() -> usize {
    ANALYZE_SINGLE_FILE_CALLS.with(|c| c.replace(0))
}

/// Add a worker's tally to the calling thread's.
#[cfg(test)]
fn add_analyze_single_file_calls(n: usize) {
    ANALYZE_SINGLE_FILE_CALLS.with(|c| c.set(c.get() + n));
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexingTestHookPoint {
    BeforeRepositoryUpsert,
    BeforePublication,
}

#[cfg(test)]
type IndexingTestHook = std::sync::Arc<dyn Fn(IndexingTestHookPoint) + Send + Sync>;

#[cfg(test)]
static INDEXING_TEST_HOOK: std::sync::OnceLock<std::sync::Mutex<Option<IndexingTestHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
struct IndexingTestHookGuard;

#[cfg(test)]
impl Drop for IndexingTestHookGuard {
    fn drop(&mut self) {
        if let Some(slot) = INDEXING_TEST_HOOK.get() {
            *slot.lock().unwrap() = None;
        }
    }
}

#[cfg(test)]
fn install_indexing_test_hook(hook: IndexingTestHook) -> IndexingTestHookGuard {
    let slot = INDEXING_TEST_HOOK.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().unwrap() = Some(hook);
    IndexingTestHookGuard
}

#[cfg(test)]
fn fire_indexing_test_hook(point: IndexingTestHookPoint) {
    let hook = INDEXING_TEST_HOOK
        .get()
        .and_then(|slot| slot.lock().ok().and_then(|guard| guard.clone()));
    if let Some(hook) = hook {
        hook(point);
    }
}

/// Run Phase 1B preprocessing + Phase 1C dispatch for one file and return
/// the retrieval units (with structural anchors) and, when a specialized
/// structural analyzer produced output, its capturable payload.
/// Never touches the database.
fn analyze_single_file(
    rec: &FileRecord,
    registry: &AnalyzerRegistry,
    opts: &IndexOptions,
    cancellation: &CancellationToken,
) -> Result<FilePrep, IndexError> {
    #[cfg(test)]
    ANALYZE_SINGLE_FILE_CALLS.with(|c| c.set(c.get() + 1));

    // Preprocess through Phase 1B secrets layer.
    // Transient I/O failures MUST be propagated — never swallowed. Content
    // that cannot be decoded as UTF-8 (binary, e.g. PDF/DOCX) is, by
    // contrast, a permanent and deterministic condition — it is reported as
    // a terminal Skip, not an error the caller might retry forever.
    let preprocessed =
        match attic_discovery::preprocess_file_content(&rec.abs_path, &rec.repo_relative) {
            Ok(p) => p,
            Err(source) if source.kind() == std::io::ErrorKind::InvalidData => {
                debug!(
                    path = %rec.repo_relative,
                    error = %source,
                    "skipping: content is not valid UTF-8 / unsupported binary"
                );
                return Ok(FilePrep::Skip);
            }
            Err(source) => {
                return Err(IndexError::Io {
                    path: rec.repo_relative.clone(),
                    source,
                });
            }
        };

    // Build AnalyzerContent based on the preprocessing decision:
    //
    //  Safe + SMALL     → FullBytes(content_bytes)                is_partial_scan = false
    //  Safe + LARGE     → StreamingHandle(Box::new(stream))       is_partial_scan = false
    //  Redacted + SMALL → RedactedBytes(already_safe_bytes)       is_partial_scan = false
    //                     Analyzer preserves safe surrounding context; Phase 1B
    //                     replaces only the secret spans inline — not the whole file.
    //  Redacted + LARGE → StreamingHandle(Box::new(stream))       is_partial_scan = false
    //                     Same large-file contract as Safe+LARGE: the stream
    //                     already has secrets redacted inline; the original
    //                     file must never be reopened downstream.
    //  PartialScan      → FullBytes(sample_bytes)                 is_partial_scan = true
    //  Excluded         → skip (terminal, no units)
    let (analyzer_content, is_partial_scan_override) = match preprocessed.decision {
        SecretScanDecision::Excluded => {
            debug!(
                path = %rec.repo_relative,
                decision = ?preprocessed.decision,
                "skipping file per preprocess decision"
            );
            return Ok(FilePrep::Skip);
        }

        SecretScanDecision::Redacted => {
            if let Some(stream) = preprocessed.stream {
                // LARGE file (4–50 MiB) with secrets: content=None,
                // stream=Some(LargeFileStream). Do NOT require content — the
                // stream already carries the redacted bytes; consume it
                // exclusively through StreamingHandle, same as Safe+LARGE.
                (AnalyzerContent::StreamingHandle(Box::new(stream)), false)
            } else {
                // SMALL file: content = Some(redacted_string): safe
                // surroundings preserved, secrets replaced. Feed as
                // RedactedBytes so analyzers can produce retrieval units for
                // the safe surrounding code regions.
                let content = preprocessed.content.ok_or_else(|| IndexError::Io {
                    path: rec.repo_relative.clone(),
                    source: std::io::Error::other(
                        "Redacted decision but content=None and stream=None (protocol violation)",
                    ),
                })?;
                (AnalyzerContent::RedactedBytes(content.into_bytes()), false)
            }
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
    let security_state = security_state_for_decision(&preprocessed.decision);

    let fo_id_str = rec.fo_id.to_string_repr();

    // Compute size for the budget.
    let size_bytes = match &analyzer_content {
        AnalyzerContent::FullBytes(b) | AnalyzerContent::RedactedBytes(b) => b.len() as u64,
        AnalyzerContent::StreamingHandle(_) => rec.size_bytes.max(0) as u64,
    };

    let mut budget = ResourceBudget {
        max_retrieval_units: opts.max_units_per_file as u64,
        ..Default::default()
    };
    budget.max_ast_nodes = budget
        .max_ast_nodes
        .max((opts.max_units_per_file as u64).saturating_mul(8));

    let file_occ_id: FileOccurrenceId = fo_id_str.parse().map_err(|_| IndexError::Io {
        path: rec.repo_relative.clone(),
        source: std::io::Error::other("invalid file_occurrence_id UUID"),
    })?;

    let input = AnalyzerInput {
        file_occurrence_id: file_occ_id,
        path: rec.abs_path.clone(),
        content: analyzer_content,
        file_type: rec.file_type,
        language_hint: infer_language_hint(&rec.repo_relative).map(str::to_string),
        size_bytes,
        is_partial_scan,
        cancellation_token: cancellation.clone(),
        resource_budget: budget,
    };

    let output = attic_analyzers::dispatch(registry, input);

    // Fail-closed completeness gate (per-file): if the analyzer reported that
    // a resource budget stopped it before the whole file was represented
    // (RESOURCE_EXHAUSTED — e.g. the retrieval-unit cap was reached mid-way
    // through a large JSON document), publishing these units would silently
    // truncate indexed content. Treat the file as a transient failure so the
    // generation-completeness gate aborts the run and the previous generation
    // stays current. This deliberately also covers GenericAnalyzer fallback
    // output: fallback preserves *text* coverage, but if the fallback itself
    // hit the unit budget the file is still incomplete.
    if let Some(diag) = output
        .diagnostics
        .iter()
        .find(|d| d.code == diagnostic_codes::RESOURCE_EXHAUSTED)
    {
        return Err(IndexError::IncompleteAnalysis {
            path: rec.repo_relative.clone(),
            reason: diag.message.clone(),
        });
    }

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
            canonical_text: unit_spec.canonical_text.clone(),
            occurrence_metadata: unit_spec.occurrence_metadata.clone(),
            analyzer_id: analyzer_id.clone(),
            analyzer_version: analyzer_version.clone(),
            start_line: Some(unit_spec.span.start_line),
            end_line: Some(unit_spec.span.end_line),
            is_redacted,
            structural_node_index: unit_spec.structural_node_index,
        })
        .collect();

    let captured = structural_pipeline::capture_structural(&rec.repo_relative, &fo_id_str, &output)
        .map(Box::new);

    debug!(
        path = %rec.repo_relative,
        units = units.len(),
        is_partial_scan,
        is_redacted,
        "file analyzed"
    );
    Ok(FilePrep::Indexable {
        units,
        captured,
        security_state,
        is_partial_scan,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Pair each discovered entry with its downstream classification in O(N).
///
/// `discovery.downstream_classifications` is built by `attic_discovery::discover`
/// iterating `discovery.entries` in the same order, pushing exactly one
/// classification per entry — so a positional zip is equivalent to a
/// per-path lookup, without the O(N²) linear scan a `.find()` inside the
/// entry loop would cost at scale. The alignment is verified rather than
/// assumed: a length or path mismatch at any position means the invariant
/// was violated (e.g. by a future change to the discovery layer) and must
/// fail loudly rather than silently misattributing one file's
/// classification to another.
fn align_classifications<'a>(
    entries: &[EligibleEntry],
    classifications: &'a [(String, DownstreamClassification)],
) -> Result<Vec<&'a DownstreamClassification>, IndexError> {
    if entries.len() != classifications.len() {
        return Err(IndexError::ClassificationCountMismatch {
            entries: entries.len(),
            classifications: classifications.len(),
        });
    }
    entries
        .iter()
        .zip(classifications.iter())
        .enumerate()
        .map(|(index, (entry, (path, classification)))| {
            if *path != entry.repo_relative {
                return Err(IndexError::ClassificationPathMismatch {
                    index,
                    expected: entry.repo_relative.clone(),
                    found: path.clone(),
                });
            }
            Ok(classification)
        })
        .collect()
}

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
        // Never actually reached: the caller's entry loop routes
        // `ScanTransientError` into `transient_failed_paths` and `continue`s
        // before ever calling this function. Handled defensively so the
        // match stays exhaustive.
        Some(DownstreamClassification::ScanTransientError { .. }) => {
            (SecurityState::Pending, false)
        }
        None => (SecurityState::Pending, false),
    }
}

/// Infer the broad file type from path extension (ASCII case-insensitive, so
/// `Main.JAVA` on a case-insensitive filesystem is classified like `Main.java`).
fn infer_file_type(path: &Path) -> FileType {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
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

/// Infer the fine-grained language tag used by `attic_analyzers::AnalyzerRegistry`'s
/// language-hint lookup (`AnalyzerInput::language_hint`), additive to — and
/// finer-grained than — `infer_file_type`'s broad `FileType` classification.
///
/// Delegates to the analyzer plugin catalog (`attic_analyzers::language_hint`),
/// the single source of truth for which plugin claims which path. That is
/// what lets `.tsx` route to the JSX-aware grammar, tier-2 languages with no
/// `FileType` variant (Swift, Ruby, …) be selected, and path-based platform
/// plugins (AEM: `jcr_root/…/*.html`, `*.cfg.json`) claim files whose
/// extension alone is ambiguous. `repo_relative` must be the
/// repository-relative path so path-based rules never match directories
/// outside the repository.
fn infer_language_hint(repo_relative: &str) -> Option<&'static str> {
    attic_analyzers::language_hint(repo_relative)
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

    /// Bug fix regression: nothing previously cross-checked that every tag
    /// string `infer_language_hint` can produce is actually a tag the
    /// analyzer registry recognizes — only a doc comment on
    /// `infer_language_hint` asserted "MUST match exactly" between this crate
    /// and `attic-analyzers`'s tier-2 table / `default_registry`. This test
    /// fails loudly the moment the two crates' tag strings drift.
    ///
    /// One sample path per `infer_language_hint` match arm (kept in sync with
    /// that function manually, since its match arms aren't otherwise
    /// enumerable from the outside).
    #[test]
    fn infer_language_hint_tags_are_registered_in_analyzer_registry() {
        let samples: &[&str] = &[
            "Dockerfile",
            "x.tsx",
            "x.ts",
            "x.rs",
            "x.c",
            "x.h",
            "x.cpp",
            "x.cc",
            "x.cxx",
            "x.hpp",
            "x.hh",
            "x.h++",
            "x.hxx",
            "x.rb",
            "x.cs",
            "x.scala",
            "x.sc",
            "x.php",
            "x.swift",
            "x.lua",
            "x.dockerfile",
            "ui.apps/src/main/content/jcr_root/apps/site/components/b/.content.xml",
            "ui.apps/src/main/content/jcr_root/apps/site/components/b/b.html",
            "ui.config/src/main/content/jcr_root/apps/site/osgiconfig/config/com.a.B.cfg.json",
            "ui.apps/src/main/content/jcr_root/apps/site/clientlibs/base/css.txt",
        ];
        let produced: HashSet<&'static str> = samples
            .iter()
            .filter_map(|s| infer_language_hint(s))
            .collect();
        assert!(
            !produced.is_empty(),
            "sample path list above must actually exercise infer_language_hint's match arms"
        );

        let registry = attic_analyzers::structural::default_registry();
        let known = registry.known_language_tags();

        for tag in &produced {
            // `"typescript"` is a documented exception: plain `.ts` sources
            // are dispatched via the `FileType::TypeScript` map (see
            // `TypeScriptSpec` in attic-analyzers), not via
            // `register_for_language`, so `"typescript"` never appears in
            // `known_language_tags()` even though the hint is correct —
            // `AnalyzerRegistry::select` falls through to the FileType-keyed
            // entry whenever a language-hint lookup misses, so this is safe
            // rather than a real drift.
            if *tag == "typescript" {
                continue;
            }
            assert!(
                known.contains(tag),
                "infer_language_hint can produce tag {tag:?}, but default_registry() has no \
                 analyzer registered under that language tag — attic-analyzers's tier-2 table \
                 (or the tsx registration) in structural/mod.rs / tags_generic.rs has drifted \
                 from infer_language_hint's match arms"
            );
        }
    }

    /// File-backed store fixture: open_db → migrations → WriterQueue.
    ///
    /// Mirrors exactly how `attic-server` constructs its endpoints; there is
    /// no way to obtain a raw write connection through this helper's output.
    struct StoreFixture {
        db_path: std::path::PathBuf,
        pool: DbPool,
        _queue: WriterQueue,
        handle: WriterQueueHandle,
        /// Declared LAST: fields drop in order, so the directory is removed only
        /// after every SQLite handle above is closed (Windows cannot delete open
        /// files, which silently leaked one temp dir per test).
        _dir: TempDir,
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
    // align_classifications (PR-4: O(N^2) -> O(N) classification lookup)
    // -----------------------------------------------------------------------

    fn entry(repo_relative: &str) -> EligibleEntry {
        EligibleEntry {
            abs_path: std::path::PathBuf::from(repo_relative),
            repo_relative: repo_relative.to_string(),
            priority: attic_discovery::DiscoveryPriority::Normal,
        }
    }

    #[test]
    fn align_classifications_pairs_by_position_in_order() {
        let entries = vec![entry("a.rs"), entry("b.rs"), entry("c.rs")];
        let classifications = vec![
            ("a.rs".to_string(), DownstreamClassification::Excluded),
            (
                "b.rs".to_string(),
                DownstreamClassification::Safe {
                    size_tier: attic_discovery::FileSizeTier::Small,
                },
            ),
            ("c.rs".to_string(), DownstreamClassification::Excluded),
        ];

        let aligned = align_classifications(&entries, &classifications).unwrap();

        assert_eq!(aligned.len(), 3);
        assert!(matches!(aligned[0], DownstreamClassification::Excluded));
        assert!(matches!(aligned[1], DownstreamClassification::Safe { .. }));
        assert!(matches!(aligned[2], DownstreamClassification::Excluded));
    }

    #[test]
    fn align_classifications_detects_count_mismatch() {
        let entries = vec![entry("a.rs"), entry("b.rs")];
        let classifications = vec![("a.rs".to_string(), DownstreamClassification::Excluded)];

        let err = align_classifications(&entries, &classifications).unwrap_err();
        assert!(matches!(
            err,
            IndexError::ClassificationCountMismatch {
                entries: 2,
                classifications: 1
            }
        ));
    }

    #[test]
    fn align_classifications_detects_wrong_path_association() {
        let entries = vec![entry("a.rs"), entry("b.rs")];
        // Swapped order relative to `entries`: position 0 claims to be for
        // "b.rs", not "a.rs" — the alignment invariant is violated even
        // though the counts match.
        let classifications = vec![
            ("b.rs".to_string(), DownstreamClassification::Excluded),
            ("a.rs".to_string(), DownstreamClassification::Excluded),
        ];

        let err = align_classifications(&entries, &classifications).unwrap_err();
        match err {
            IndexError::ClassificationPathMismatch {
                index,
                expected,
                found,
            } => {
                assert_eq!(index, 0);
                assert_eq!(expected, "a.rs");
                assert_eq!(found, "b.rs");
            }
            other => panic!("expected ClassificationPathMismatch, got {other:?}"),
        }
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
    fn index_repository_surfaces_submodule_diagnostic() {
        let fx = make_store();
        write_file(fx._dir.path(), "root_file.rs", "fn root() {}\n");
        let sub = fx._dir.path().join("sub");
        std::fs::create_dir_all(sub.join(".git")).unwrap();
        std::fs::write(sub.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
        write_file(&sub, "sub_file.rs", "fn sub() {}\n");

        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts)
            .expect("indexing should succeed even with a nested git boundary");

        let has_diag = result
            .discovery_diagnostics
            .iter()
            .any(|d| d.kind == attic_discovery::DiagnosticKind::SubmoduleDetected);
        assert!(
            has_diag,
            "expected SubmoduleDetected diagnostic to survive into IndexResult; got {:?}",
            result.discovery_diagnostics
        );
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

    #[test]
    fn cancelled_bootstrap_after_eviction_does_not_recreate_repository_row() {
        let fx = make_store();
        write_file(fx._dir.path(), "late.rs", "fn late() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let first = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();

        let evicted_repo = first.repository_id.clone();
        fx.handle
            .send(move |conn| {
                loop {
                    let step =
                        attic_storage::repo_eviction::evict_repository_step(conn, &evicted_repo)?;
                    if step.complete {
                        break;
                    }
                }
                Ok(())
            })
            .unwrap();

        let root_path = fx._dir.path().to_string_lossy().to_string();
        assert!(
            !verify_conn(&fx)
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM core_repositories WHERE root_path = ?1)",
                    rusqlite::params![root_path],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap(),
            "precondition: eviction must remove the repository row before the late bootstrap"
        );

        let cancellation = CancellationToken::new();
        let hook_cancellation = cancellation.clone();
        let _hook = install_indexing_test_hook(std::sync::Arc::new(move |point| {
            if point == IndexingTestHookPoint::BeforeRepositoryUpsert {
                hook_cancellation.cancel();
            }
        }));

        let err = index_repository_with_cancellation(
            &store(&fx),
            fx._dir.path(),
            &policy,
            &opts,
            &cancellation,
        )
        .unwrap_err();
        assert!(matches!(err, IndexError::Cancelled));
        assert!(
            !verify_conn(&fx)
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM core_repositories WHERE root_path = ?1)",
                    rusqlite::params![fx._dir.path().to_string_lossy().to_string()],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap(),
            "a cancelled late bootstrap must not recreate the evicted repository row"
        );
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
    fn repositories_are_named_after_their_folder_and_renamed_from_default() {
        let fx = make_store();
        let root = fx._dir.path().join("HDFC_FormsCommon");
        std::fs::create_dir_all(&root).unwrap();
        write_file(&root, "a.rs", "fn named_repo_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let s = store(&fx);
        let name_of = |id: &str| -> String {
            fx.pool
                .with_reader(|c| {
                    Ok(c.query_row(
                        "SELECT display_name FROM core_repositories WHERE id = ?1",
                        [id],
                        |r| r.get(0),
                    )?)
                })
                .unwrap()
        };

        let r = index_repository(&s, &root, &policy, &IndexOptions::default()).unwrap();
        assert_eq!(name_of(&r.repository_id), "HDFC_FormsCommon");

        // A row left over as "default" by an older build is renamed on the
        // next run; an explicit name still wins.
        fx.handle
            .send(|c| {
                c.execute("UPDATE core_repositories SET display_name = 'default'", [])?;
                Ok(())
            })
            .unwrap();
        index_repository(&s, &root, &policy, &IndexOptions::default()).unwrap();
        assert_eq!(name_of(&r.repository_id), "HDFC_FormsCommon");
        let named = IndexOptions {
            repository_name: "forms-common".into(),
            ..Default::default()
        };
        index_repository(&s, &root, &policy, &named).unwrap();
        assert_eq!(name_of(&r.repository_id), "forms-common");
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

    // ── Phase 0 instrumentation ───────────────────────────────────────────

    #[test]
    fn stage_timings_are_populated_and_accumulators_are_measured() {
        // The instrumentation must report a real profile, not zeros, and the
        // two text accumulators must be observed separately so the cost of
        // the PR-7 cache copy is visible rather than inferred.
        let fx = make_store();
        // Enough distinct content that at least one retrieval unit is emitted
        // per file and the peak counters are non-trivial.
        for i in 0..8 {
            write_file(
                fx._dir.path(),
                &format!("measured_{i}.rs"),
                &format!("pub fn measured_token_{i}() -> u32 {{ {i} }}\n").repeat(40),
            );
        }
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();

        let t = &result.stage_timings;
        assert!(
            t.total_ms >= t.discovery_ms,
            "total wall clock must cover the discovery stage: total={} discovery={}",
            t.total_ms,
            t.discovery_ms
        );
        assert!(
            t.pending_units_peak_bytes > 0,
            "pending_units high-water mark must be measured when units were emitted"
        );
        assert!(
            t.pending_units_peak_count > 0,
            "unit count at the high-water mark must be recorded alongside the byte figure"
        );
        // Fresh repository: every file misses the analysis cache, so the PR-7
        // JSON copy of the very same text is also resident. This is the
        // second whole-repo text buffer the RAM plan targets; assert it is
        // actually observed, because "it is only one copy" was the wrong
        // assumption that motivated this instrumentation.
        assert!(
            t.cache_writes_peak_bytes > 0,
            "cache_writes high-water mark must be measured on a cold (cache-miss) run"
        );
    }

    #[test]
    fn stage_timings_survive_a_reindex_and_report_cache_hits() {
        // On a second run every file hits `index_analysis_cache`, so the
        // JSON copy is skipped entirely. Guards the claim that the second
        // buffer is a cold-run cost, not a permanent one.
        let fx = make_store();
        write_file(
            fx._dir.path(),
            "cached_once.rs",
            "pub fn cached_once_token() {}\n",
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        let first = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        assert!(
            first.stage_timings.pending_units_peak_bytes > 0,
            "first run must emit units"
        );

        let second = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        assert!(
            second.stage_timings.total_ms >= second.stage_timings.publish_ms,
            "total wall clock must cover the publication stage on every run"
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

    // -----------------------------------------------------------------------
    // Phase 6.3 — full indexing regression/dry-run matrix
    // -----------------------------------------------------------------------

    fn search_hits(fx: &StoreFixture, query: &str) -> Vec<attic_storage::FtsSearchResult> {
        fx.pool
            .with_reader(|c| {
                attic_storage::fts_search(
                    c,
                    &attic_storage::FtsSearchParams {
                        query,
                        repository_id: None,
                        file_type: None,
                        language: None,
                        max_results: 50,
                    },
                )
            })
            .unwrap()
    }

    /// `(existence_state, freshness_state, security_state)` of the latest
    /// occurrence row for `path`.
    fn latest_occurrence_state(fx: &StoreFixture, path: &str) -> (String, String, String) {
        let verify = verify_conn(fx);
        verify
            .query_row(
                "SELECT existence_state, freshness_state, security_state
                   FROM core_file_occurrences
                  WHERE path = ?1
                  ORDER BY rowid DESC
                  LIMIT 1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
    }

    fn repo_stats(fx: &StoreFixture, repository_id: &str) -> attic_storage::RepositoryStats {
        fx.pool
            .with_reader(attic_storage::get_repository_stats)
            .unwrap()
            .into_iter()
            .find(|s| s.id == repository_id)
            .expect("repository stats row must exist")
    }

    #[test]
    fn new_clean_repository_all_eligible_files_represented() {
        let fx = make_store();
        write_file(fx._dir.path(), "a.rs", "fn a_token() {}\n");
        write_file(fx._dir.path(), "b.py", "def b_token(): pass\n");
        write_file(fx._dir.path(), "c.md", "# c_token heading\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(
            result.files_indexed, 3,
            "all three eligible files must be indexed"
        );
        for token in ["a_token", "b_token", "c_token"] {
            assert!(
                !search_hits(&fx, token).is_empty(),
                "{token} must be searchable"
            );
        }
    }

    /// PR-8: measurement-only counters must accurately report the
    /// duplicate discovery/analysis read of a SMALL file — the size of the
    /// cost the audit flagged, kept as an observable metric rather than a
    /// speculative fix (see module docs on `analysis_small_file_bytes_read`).
    #[test]
    fn small_file_io_counters_report_the_duplicate_read() {
        let fx = make_store();
        // A dedicated subdirectory, distinct from `fx._dir.path()` (which
        // also holds the SQLite db/wal/shm files) — otherwise those binary
        // files would be discovered and counted too, muddying the exact
        // byte-count assertions this test makes.
        let root = fx._dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let content = "fn io_counter_token() {}\n";
        write_file(&root, "a.rs", content);
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);

        let r1 = index_repository(&s, &root, &policy, &opts).unwrap();
        assert_eq!(r1.discovery_counters.small_file_reads, 1);
        assert_eq!(
            r1.discovery_counters.small_file_bytes_read,
            content.len() as u64
        );
        assert_eq!(
            r1.analysis_small_file_reads, 1,
            "no cache entry exists yet — analysis must re-read the file discovery already read"
        );
        assert_eq!(
            r1.analysis_small_file_bytes_read,
            content.len() as u64,
            "measured duplicate-read size must match the file's actual size"
        );
    }

    #[test]
    fn existing_repo_row_emptied_disk_converges_to_zero_files() {
        // P0-1/P0-4: a repository row already existing must never itself mean
        // "bootstrap complete" — reconciling against an emptied disk must
        // retire all previously-active content.
        let fx = make_store();
        write_file(fx._dir.path(), "only.rs", "fn only_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        let r1 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        assert!(!search_hits(&fx, "only_token").is_empty());

        std::fs::remove_file(fx._dir.path().join("only.rs")).unwrap();
        let r2 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(
            r2.repository_id, r1.repository_id,
            "the existing repository row is reconciled, not recreated"
        );
        assert_eq!(r2.files_indexed, 0);
        assert!(
            search_hits(&fx, "only_token").is_empty(),
            "deleted file's content must not remain searchable"
        );
        let stats = repo_stats(&fx, &r1.repository_id);
        assert_eq!(
            stats.file_count, 0,
            "get_repository_stats must reflect zero current files"
        );
    }

    #[test]
    fn existing_partial_repository_converges_to_disk() {
        let fx = make_store();
        write_file(fx._dir.path(), "keep.rs", "fn keep_token() {}\n");
        write_file(fx._dir.path(), "gone.rs", "fn gone_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        std::fs::remove_file(fx._dir.path().join("gone.rs")).unwrap();
        write_file(fx._dir.path(), "new.rs", "fn new_token() {}\n");
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        assert!(
            !search_hits(&fx, "keep_token").is_empty(),
            "unchanged file stays searchable"
        );
        assert!(
            search_hits(&fx, "gone_token").is_empty(),
            "deleted file's content must be gone"
        );
        assert!(
            !search_hits(&fx, "new_token").is_empty(),
            "newly added file must be searchable"
        );
    }

    #[test]
    fn repeated_unchanged_full_index_has_no_duplicate_truth() {
        let fx = make_store();
        write_file(fx._dir.path(), "stable.rs", "fn stable_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        let r1 = index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        let stats = repo_stats(&fx, &r1.repository_id);
        assert_eq!(
            stats.file_count, 1,
            "repeated unchanged indexing must not inflate file_count (P1-1)"
        );
        let hits = search_hits(&fx, "stable_token");
        assert_eq!(
            hits.len(),
            1,
            "must be exactly one current searchable hit, not one per run"
        );
    }

    #[test]
    fn modify_file_new_content_searchable_old_absent() {
        let fx = make_store();
        write_file(fx._dir.path(), "m.rs", "fn before_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        assert!(!search_hits(&fx, "before_token").is_empty());

        write_file(fx._dir.path(), "m.rs", "fn after_token() {}\n");
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        assert!(
            search_hits(&fx, "before_token").is_empty(),
            "old content must be gone after modify"
        );
        assert!(!search_hits(&fx, "after_token").is_empty());
    }

    #[test]
    fn delete_file_content_absent_from_search_and_db() {
        let fx = make_store();
        write_file(fx._dir.path(), "d.rs", "fn delete_me_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        std::fs::remove_file(fx._dir.path().join("d.rs")).unwrap();
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        assert!(search_hits(&fx, "delete_me_token").is_empty());
        let (existence, freshness, security) = latest_occurrence_state(&fx, "d.rs");
        assert_eq!(existence, "deleted");
        assert_eq!(freshness, "INVALID");
        assert_eq!(security, "skipped");

        let verify = verify_conn(&fx);
        let unit_count: i64 = verify
            .query_row(
                "SELECT COUNT(*) FROM core_retrieval_units ru
                   JOIN core_file_occurrences fo ON ru.file_occurrence_id = fo.id
                  WHERE fo.path = 'd.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            unit_count, 0,
            "no retrieval units may remain pointing at a deleted path"
        );
    }

    #[test]
    fn rename_same_content_file_new_path_correct_old_path_absent() {
        let fx = make_store();
        write_file(fx._dir.path(), "old_name.rs", "fn renamed_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        std::fs::rename(
            fx._dir.path().join("old_name.rs"),
            fx._dir.path().join("new_name.rs"),
        )
        .unwrap();
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();

        let hits = search_hits(&fx, "renamed_token");
        assert_eq!(
            hits.len(),
            1,
            "content must be searchable exactly once, under its new path"
        );
        assert_eq!(hits[0].path, "new_name.rs");
        let (existence, _, _) = latest_occurrence_state(&fx, "old_name.rs");
        assert_eq!(existence, "deleted", "old path must be retired");
    }

    #[test]
    fn file_becomes_excluded_old_content_retired() {
        let fx = make_store();
        write_file(fx._dir.path(), "excl.rs", "fn excl_token() {}\n");
        let base_policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        index_repository(&s, fx._dir.path(), &base_policy, &opts).unwrap();
        assert!(!search_hits(&fx, "excl_token").is_empty());

        let mut excluding_policy = DiscoveryPolicy::default_git();
        excluding_policy
            .attic_exclude_rules
            .push(attic_discovery::GlobRule::exclude("excl.rs"));
        index_repository(&s, fx._dir.path(), &excluding_policy, &opts).unwrap();

        assert!(
            search_hits(&fx, "excl_token").is_empty(),
            "newly-excluded content must not remain searchable (P0-4)"
        );
        let (existence, _, _) = latest_occurrence_state(&fx, "excl.rs");
        assert_eq!(existence, "deleted");
    }

    #[test]
    fn known_secret_filename_is_excluded() {
        let fx = make_store();
        write_file(
            fx._dir.path(),
            ".env",
            "SECRET_TOKEN_VALUE=doNotIndexThisEnvValue\n",
        );
        write_file(fx._dir.path(), "app.rs", "fn app_marker_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(
            result.files_indexed, 1,
            "only app.rs should be indexed; .env is a known-secrets filename"
        );
        assert!(search_hits(&fx, "doNotIndexThisEnvValue").is_empty());
        assert!(!search_hits(&fx, "app_marker_token").is_empty());
    }

    #[test]
    fn empty_file_is_deterministic_non_error() {
        let fx = make_store();
        write_file(fx._dir.path(), "empty.rs", "");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(
            result.files_indexed, 1,
            "an empty file is a valid, indexed (zero-unit) occurrence"
        );
        let (existence, _, security) = latest_occurrence_state(&fx, "empty.rs");
        assert_eq!(existence, "present");
        assert_eq!(security, "clean");
    }

    #[test]
    fn unicode_content_preserved_correctly() {
        let fx = make_store();
        write_file(
            fx._dir.path(),
            "unicode.rs",
            "// café 日本語コメント 😀\nfn unicode_token() {}\n",
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert!(!search_hits(&fx, "unicode_token").is_empty());

        let verify = verify_conn(&fx);
        let bodies: Vec<String> = {
            let mut stmt = verify
                .prepare("SELECT retrieval_text FROM core_retrieval_units")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert!(
            bodies
                .iter()
                .any(|b| b.contains("café") && b.contains('😀')),
            "unicode content must round-trip exactly, not be mangled"
        );
    }

    #[test]
    fn invalid_utf8_small_binary_is_a_clean_skip() {
        // P0-3: discovery classifies this ScanSkipped; full indexing must
        // treat it as a terminal clean skip, never re-attempt analysis and
        // surface "stream did not contain valid UTF-8".
        let fx = make_store();
        std::fs::write(
            fx._dir.path().join("binary.dat"),
            [0xFF, 0xFE, 0x00, 0xD8, 0x00, 0x01],
        )
        .unwrap();
        write_file(
            fx._dir.path(),
            "text.rs",
            "fn alongside_binary_token() {}\n",
        );

        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts)
            .expect("a binary file must never abort the whole indexing run");
        assert_eq!(result.files_indexed, 1, "only the text file is indexed");
        assert!(
            result.files_skipped >= 1,
            "the binary file must be counted as a clean skip"
        );
        assert!(!search_hits(&fx, "alongside_binary_token").is_empty());
    }

    #[test]
    fn pdf_like_binary_clean_skip_no_repeated_failure_across_runs() {
        let fx = make_store();
        let mut pdf_bytes = b"%PDF-1.4\n".to_vec();
        pdf_bytes.extend_from_slice(&[0x00, 0xFF, 0xC3, 0x28, 0x99, 0x00]);
        std::fs::write(fx._dir.path().join("doc.pdf"), &pdf_bytes).unwrap();

        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        let r1 = index_repository(&s, fx._dir.path(), &policy, &opts)
            .expect("PDF content must never surface as 'stream did not contain valid UTF-8'");
        let r2 = index_repository(&s, fx._dir.path(), &policy, &opts).expect(
            "repeated indexing of the same unsupported binary must stay a clean skip, not fail",
        );
        assert_eq!(r1.files_indexed, 0);
        assert_eq!(r2.files_indexed, 0);
        assert!(search_hits(&fx, "PDF").is_empty());
    }

    #[test]
    fn small_safe_file_has_correct_security_state() {
        let fx = make_store();
        write_file(fx._dir.path(), "safe.rs", "fn safe_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        let (existence, _, security) = latest_occurrence_state(&fx, "safe.rs");
        assert_eq!(existence, "present");
        assert_eq!(security, "clean");
    }

    #[test]
    fn small_redacted_secret_absent_safe_context_searchable() {
        let fx = make_store();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        write_file(
            fx._dir.path(),
            "small_redacted.rs",
            &format!("fn small_redacted_safe_token() {{}}\n// key: {secret}\n"),
        );
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert!(!search_hits(&fx, "small_redacted_safe_token").is_empty());
        assert!(
            search_hits(&fx, secret).is_empty(),
            "the raw secret must never be searchable"
        );
        let (_, _, security) = latest_occurrence_state(&fx, "small_redacted.rs");
        assert_eq!(security, "flagged");
    }

    #[test]
    fn large_safe_file_uses_streaming_path() {
        let fx = make_store();
        let filler = "fn filler_line() { let _ = 1; }\n".repeat(200_000);
        let content = format!("fn large_safe_token() {{}}\n{filler}");
        assert!(content.len() as u64 > attic_discovery::secrets::SMALL_FILE_THRESHOLD);
        std::fs::write(fx._dir.path().join("large_safe.rs"), &content).unwrap();

        let policy = DiscoveryPolicy::default_git();
        // This test verifies the LARGE-file streaming protocol, not budget
        // behavior. Since r01, RESOURCE_EXHAUSTED fails closed, so a
        // 200k-line file under the default 512-unit budget would correctly
        // abort the generation; give it a budget that fits the content.
        let opts = IndexOptions {
            max_units_per_file: 1_000_000,
            ..IndexOptions::default()
        };
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(result.files_indexed, 1);
        assert!(!search_hits(&fx, "large_safe_token").is_empty());
        let (_, _, security) = latest_occurrence_state(&fx, "large_safe.rs");
        assert_eq!(security, "clean");
    }

    #[test]
    fn large_redacted_file_streams_without_protocol_violation() {
        // P0-2 core regression: a LARGE file with a detected secret has
        // content=None/stream=Some — analyze_single_file must consume the
        // stream, never require `content` and error with "Redacted decision
        // but content=None". The secret is placed straddling a 64 KiB stream
        // chunk boundary so this also covers boundary-spanning redaction.
        let fx = make_store();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let chunk = 64 * 1024usize;
        let mut content = String::new();
        content.push_str("fn large_redacted_safe_token() {}\n");
        while content.len() < chunk - secret.len() / 2 {
            content.push_str("// filler line padding the file out\n");
        }
        content.push_str(secret);
        content.push('\n');
        while content.len() < 5 * 1024 * 1024 {
            content.push_str("// more filler padding past the LARGE threshold\n");
        }
        std::fs::write(fx._dir.path().join("large_redacted.rs"), &content).unwrap();

        let policy = DiscoveryPolicy::default_git();
        // Streaming/redaction protocol test — needs a unit budget that fits
        // the content now that RESOURCE_EXHAUSTED fails closed (r01).
        let opts = IndexOptions {
            max_units_per_file: 1_000_000,
            ..IndexOptions::default()
        };
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts)
            .expect("LARGE+Redacted must stream, not error with content=None (P0-2)");
        assert_eq!(result.files_indexed, 1);
        assert!(!search_hits(&fx, "large_redacted_safe_token").is_empty());
        assert!(
            search_hits(&fx, secret).is_empty(),
            "the raw secret must never be searchable"
        );
        let (_, _, security) = latest_occurrence_state(&fx, "large_redacted.rs");
        assert_eq!(security, "flagged");
    }

    #[test]
    fn very_large_file_partial_scan_only_safe_sample_indexed() {
        let fx = make_store();
        let head = b"fn head_sample_token() {}\n";
        let mid_marker = b"fn midbody_only_token() {}\n";
        let total_size = 50 * 1024 * 1024 + 4096;
        let mut body = vec![b'x'; total_size];
        body[0..head.len()].copy_from_slice(head);
        let mid_start = total_size / 2;
        body[mid_start..mid_start + mid_marker.len()].copy_from_slice(mid_marker);
        std::fs::write(fx._dir.path().join("huge.rs"), &body).unwrap();
        drop(body);

        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(result.files_indexed, 1);

        assert!(
            !search_hits(&fx, "head_sample_token").is_empty(),
            "the head sample must be indexed"
        );
        assert!(
            search_hits(&fx, "midbody_only_token").is_empty(),
            "the mid-body of a VERY_LARGE file must never be scanned/indexed"
        );

        let (_, _, security) = latest_occurrence_state(&fx, "huge.rs");
        assert_eq!(
            security, "pending",
            "VERY_LARGE partial scan must report an explicit partial state, never silently Clean"
        );
    }

    #[test]
    fn analyze_single_file_invalid_utf8_is_permanent_skip_not_error() {
        let fx = make_store();
        let path = fx._dir.path().join("bad.bin");
        std::fs::write(&path, [0xFF, 0xFE, 0xFD]).unwrap();
        let rec = FileRecord {
            fi_id: FileIdentityId::new_v4(),
            fo_id: FileOccurrenceId::new_v4(),
            stable_id_basis: "test/bad.bin".to_string(),
            old_fo_id: None,
            repo_relative: "bad.bin".to_string(),
            abs_path: path,
            content_hash: String::new(),
            size_bytes: 3,
            security_state: SecurityState::Pending,
            file_type: FileType::Other,
            is_partial_scan: false,
        };
        let registry = structural_pipeline::generic_only_registry();
        let opts = IndexOptions::default();
        let outcome = analyze_single_file(&rec, &registry, &opts, &CancellationToken::default())
            .expect("invalid UTF-8 must be reported as Ok(Skip), never Err");
        assert!(
            matches!(outcome, FilePrep::Skip),
            "invalid UTF-8 content must be a permanent Skip, not indexed"
        );
    }

    #[test]
    fn analyze_single_file_missing_path_is_transient_not_skip() {
        let fx = make_store();
        let path = fx._dir.path().join("does_not_exist.rs");
        let rec = FileRecord {
            fi_id: FileIdentityId::new_v4(),
            fo_id: FileOccurrenceId::new_v4(),
            stable_id_basis: "test/missing".to_string(),
            old_fo_id: None,
            repo_relative: "does_not_exist.rs".to_string(),
            abs_path: path,
            content_hash: String::new(),
            size_bytes: 0,
            security_state: SecurityState::Pending,
            file_type: FileType::Rust,
            is_partial_scan: false,
        };
        let registry = structural_pipeline::generic_only_registry();
        let opts = IndexOptions::default();
        match analyze_single_file(&rec, &registry, &opts, &CancellationToken::default()) {
            Err(IndexError::Io { .. }) => {}
            Ok(_) => panic!(
                "a missing file must be a transient error, never a silent Skip/Indexable (P0-5/P0-7)"
            ),
            Err(other) => {
                panic!("expected IndexError::Io for a transient I/O failure, got {other}")
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 6.4 — generation-completeness invariant
    // -----------------------------------------------------------------------

    /// A transient/retryable failure on ANY discovered file must abort the
    /// entire generation — including the files that WOULD have succeeded —
    /// rather than publish a generation that mixes verified-current content
    /// with content nobody actually verified this run. The previous
    /// generation must remain completely untouched and current.
    ///
    /// Windows-only: uses `share_mode(0)` (deny all sharing) to force a
    /// genuine transient I/O error deterministically and portably, without
    /// mocking. This exercises `index_repository`'s aggregate behavior; the
    /// underlying fix (bail before publication on any transient failure) is
    /// itself platform-agnostic and is also covered indirectly by the
    /// platform-independent `analyze_single_file_missing_path_is_transient_not_skip`
    /// unit test above.
    #[cfg(windows)]
    #[test]
    fn transient_failure_aborts_whole_generation_old_state_preserved() {
        use std::os::windows::fs::OpenOptionsExt;

        let fx = make_store();
        write_file(fx._dir.path(), "good.rs", "fn good_token() {}\n");
        write_file(fx._dir.path(), "locked.rs", "fn locked_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);
        index_repository(&s, fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(search_hits(&fx, "good_token").len(), 1);
        assert_eq!(search_hits(&fx, "locked_token").len(), 1);

        // Modify both files, then exclusively lock one so this run's read of
        // it fails with a genuine transient I/O error (never InvalidData).
        write_file(fx._dir.path(), "good.rs", "fn good_token_v2() {}\n");
        write_file(fx._dir.path(), "locked.rs", "fn locked_token_v2() {}\n");
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(fx._dir.path().join("locked.rs"))
            .expect("open with exclusive sharing to simulate a transient lock conflict");

        let result = index_repository(&s, fx._dir.path(), &policy, &opts);
        drop(_lock);

        match result {
            Err(IndexError::TransientFailures { paths }) => {
                assert!(paths.iter().any(|p| p == "locked.rs"));
            }
            other => panic!(
                "a transient read failure must abort the whole generation with \
                 TransientFailures, not publish a mixed one; got {other:?}"
            ),
        }

        // Old generation must remain entirely intact — including good.rs,
        // which WOULD have analyzed successfully this run.
        assert_eq!(
            search_hits(&fx, "good_token").len(),
            1,
            "good.rs's OLD content must still be current"
        );
        assert!(
            search_hits(&fx, "good_token_v2").is_empty(),
            "new content must never be published while the generation is incomplete"
        );
        assert_eq!(search_hits(&fx, "locked_token").len(), 1);
    }

    /// Fail-closed completeness: when an analyzer reports RESOURCE_EXHAUSTED
    /// (its unit budget stopped it before the whole file was represented),
    /// indexing must NOT publish the truncated units. The file is recorded
    /// as a transient failure, the whole generation aborts, and the previous
    /// generation (if any) remains current. A retry with an adequate budget
    /// must then index the same content fully.
    #[test]
    fn resource_exhausted_analysis_aborts_generation_previous_state_preserved() {
        let fx = make_store();
        // 8 top-level keys, each with a ~600-char value: the JsonAnalyzer
        // decomposes the oversized root into one unit per child, so the file
        // needs 8 units — far above the tight budget below.
        let mut obj = serde_json::Map::new();
        for i in 0..8 {
            obj.insert(
                format!("k{i}"),
                serde_json::Value::String(format!("json_budget_token_{i} {}", "x".repeat(600))),
            );
        }
        let json_text = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap();
        write_file(fx._dir.path(), "big.json", &json_text);
        write_file(fx._dir.path(), "ok.rs", "fn budget_ok_token() {}\n");

        let policy = DiscoveryPolicy::default_git();
        let s = store(&fx);

        let tight = IndexOptions {
            max_units_per_file: 2,
            ..IndexOptions::default()
        };
        let result = index_repository(&s, fx._dir.path(), &policy, &tight);
        match result {
            Err(IndexError::TransientFailures { paths }) => {
                assert!(
                    paths.iter().any(|p| p == "big.json"),
                    "big.json must be recorded as a failed path: {paths:?}"
                );
            }
            other => panic!(
                "resource exhaustion must abort the whole generation with \
                 TransientFailures, never publish truncated units; got {other:?}"
            ),
        }
        assert!(
            search_hits(&fx, "budget_ok_token").is_empty(),
            "nothing may be published from an incomplete generation"
        );
        assert!(
            search_hits(&fx, "json_budget_token_0").is_empty(),
            "truncated JSON units must never become searchable"
        );

        // Retry with an adequate budget: the same content now indexes fully.
        let wide = IndexOptions {
            max_units_per_file: 10_000,
            ..IndexOptions::default()
        };
        let result = index_repository(&s, fx._dir.path(), &policy, &wide).unwrap();
        assert!(result.files_indexed >= 2);
        for i in 0..8 {
            assert!(
                !search_hits(&fx, &format!("json_budget_token_{i}")).is_empty(),
                "json_budget_token_{i} must be searchable after a complete run"
            );
        }
        assert!(!search_hits(&fx, "budget_ok_token").is_empty());
    }

    /// r03 gate: identical JSON logical content in DEV/PROD exports must
    /// produce the SAME canonical_hash (one embedding candidate downstream),
    /// while each occurrence keeps its own environment/pointer provenance.
    #[test]
    fn json_cross_environment_units_share_canonical_hash() {
        let fx = make_store();
        let dev = r#"{ "service": "payment", "retry": 3 }"#;
        let prod = r#"{"retry":3,"service":"payment"}"#; // reordered keys
        write_file(fx._dir.path(), "DEV-Env.json", dev);
        write_file(fx._dir.path(), "PROD-Env.json", prod);

        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts).unwrap();
        assert_eq!(result.files_indexed, 2);

        let verify = verify_conn(&fx);
        let rows: Vec<(String, String, String)> = {
            let mut stmt = verify
                .prepare(
                    "SELECT o.path, u.canonical_hash, u.occurrence_metadata
                       FROM core_retrieval_units u
                       JOIN core_file_occurrences o ON o.id = u.file_occurrence_id
                      ORDER BY o.path",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect()
        };
        assert_eq!(rows.len(), 2, "one canonical unit per file");
        assert_eq!(
            rows[0].1, rows[1].1,
            "DEV and PROD units must share one canonical hash: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|(p, _, m)| p == "DEV-Env.json" && m.contains("\"DEV\"")),
            "DEV occurrence metadata: {rows:?}"
        );
        assert!(
            rows.iter()
                .any(|(p, _, m)| p == "PROD-Env.json" && m.contains("\"PROD\"")),
            "PROD occurrence metadata: {rows:?}"
        );
        // Both occurrences remain searchable via their decorated text.
        assert!(!search_hits(&fx, "payment").is_empty());
    }

    /// r03 gate: a file producing far more than the old 512-unit cap must
    /// index completely under the fail-closed default budget.
    #[test]
    fn json_file_with_over_10k_units_indexes_completely() {
        let fx = make_store();
        let mut obj = serde_json::Map::new();
        for i in 0..11_000 {
            obj.insert(
                format!("key_{i:05}"),
                serde_json::Value::String(format!("value_{i}")),
            );
        }
        let text = serde_json::to_string(&serde_json::Value::Object(obj)).unwrap();
        write_file(fx._dir.path(), "huge.json", &text);

        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let result = index_repository(&store(&fx), fx._dir.path(), &policy, &opts)
            .expect("11k-unit file must index completely under the 100k default budget");
        assert_eq!(result.units_inserted, 11_000);
        assert!(!search_hits(&fx, "value_0").is_empty());
        assert!(!search_hits(&fx, "value_10999").is_empty());
    }

    /// PR-7 acceptance test: N files, one transiently fails. The first
    /// attempt must analyze every file and persist a cache entry for each
    /// success; the previous generation stays untouched (already covered
    /// by the test above). Once the lock is released, a retry must reuse
    /// the cached results for the files that already succeeded — proven by
    /// the analyzer invocation count, not just by the final output — and
    /// publish successfully, after which the cache is cleared.
    #[cfg(windows)]
    #[test]
    fn retry_after_transient_failure_reuses_cached_analysis_and_eventually_publishes() {
        use std::os::windows::fs::OpenOptionsExt;

        let fx = make_store();
        const N: usize = 12;
        for i in 0..N {
            write_file(
                fx._dir.path(),
                &format!("f{i}.rs"),
                &format!("fn retry_cache_token_{i}() {{}}\n"),
            );
        }
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);

        let locked_path = fx._dir.path().join("f0.rs");
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&locked_path)
            .expect("open with exclusive sharing to simulate a transient lock conflict");

        reset_analyze_single_file_calls();
        let attempt1 = index_repository(&s, fx._dir.path(), &policy, &opts);
        assert!(
            matches!(attempt1, Err(IndexError::TransientFailures { .. })),
            "expected a transient-failure abort, got {attempt1:?}"
        );
        // f0.rs (locked) fails its `std::fs::metadata` stat before it ever
        // becomes a `FileRecord`, so `analyze_single_file` is only reached
        // for the other N-1 files — all of which have no cache yet.
        assert_eq!(
            analyze_single_file_calls(),
            N - 1,
            "every file that reached analysis (all but the locked one) must be analyzed fresh"
        );

        let verify = verify_conn(&fx);
        let cached_after_failure: i64 = verify
            .query_row("SELECT COUNT(*) FROM index_analysis_cache", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            cached_after_failure,
            (N - 1) as i64,
            "every successfully-analyzed file must be cached before the abort"
        );

        drop(_lock);

        reset_analyze_single_file_calls();
        let attempt2 = index_repository(&s, fx._dir.path(), &policy, &opts)
            .expect("retry must succeed once the lock is released");
        assert_eq!(
            analyze_single_file_calls(),
            1,
            "the retry must reuse cached results for the {} already-succeeded files \
             and analyze only the previously-locked one",
            N - 1
        );
        assert_eq!(attempt2.files_indexed, N);

        for i in 0..N {
            assert_eq!(
                search_hits(&fx, &format!("retry_cache_token_{i}")).len(),
                1,
                "file f{i}.rs must be searchable after the successful retry"
            );
        }

        let cached_after_success: i64 = verify
            .query_row("SELECT COUNT(*) FROM index_analysis_cache", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            cached_after_success, 0,
            "the cache must be cleared once the generation successfully publishes"
        );
    }

    #[test]
    fn analysis_cache_hit_requires_matching_policy_and_options() {
        let fx = make_store();
        let root = fx._dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write_file(&root, "a.rs", "fn cache_policy_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);

        let r1 = index_repository(&s, &root, &policy, &opts).unwrap();
        let repo_id: RepositoryId = r1.repository_id.parse().unwrap();
        let content_hash = fx
            .pool
            .with_reader(|c| attic_storage::current_path_hashes_for_repository(c, &repo_id))
            .unwrap()
            .into_iter()
            .find(|(p, _)| p == "a.rs")
            .map(|(_, h)| h)
            .expect("a.rs must have a content hash after indexing");

        let changed_policy = {
            let mut p = policy.clone();
            p.scan_exempt_paths.push("examples/**".to_string());
            p
        };

        let wrong_policy_hash = policy.hash().unwrap();
        let wrong_options = IndexOptions {
            structural: false,
            max_units_per_file: opts.max_units_per_file + 1,
            ..opts.clone()
        };
        assert_ne!(changed_policy.hash().unwrap(), wrong_policy_hash);

        fx.handle
            .send({
                let cached_policy_hash = wrong_policy_hash;
                move |conn| {
                    attic_storage::upsert_analysis_cache_entries(
                        conn,
                        &repo_id,
                        &[attic_storage::CachedFileAnalysis {
                            repo_relative: "a.rs".to_string(),
                            content_hash,
                            security_state: "clean".to_string(),
                            is_partial_scan: false,
                            secret_pattern_version: SECRET_PATTERN_VERSION,
                            analyzer_registry_version:
                                attic_core::constants::ANALYZER_REGISTRY_VERSION.to_owned(),
                            discovery_policy_hash: cached_policy_hash,
                            structural: wrong_options.structural,
                            max_units_per_file: wrong_options.max_units_per_file as u64,
                            units_json: "[]".to_string(),
                            captured_json: None,
                        }],
                        0,
                    )
                }
            })
            .unwrap();

        reset_analyze_single_file_calls();
        index_repository(&s, &root, &policy, &opts).unwrap();
        assert_eq!(
            analyze_single_file_calls(),
            1,
            "cache entry from a different policy/options must be ignored"
        );
    }

    /// Code-review finding: a cache entry stamped with a stale
    /// secret-pattern/analyzer-registry version must never be replayed for
    /// unchanged content — a version mismatch is a cache miss, forcing
    /// fresh analysis under the current ruleset.
    #[test]
    fn analysis_cache_hit_requires_matching_secret_pattern_version() {
        let fx = make_store();
        let root = fx._dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write_file(&root, "a.rs", "fn cache_version_token() {}\n");
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        let s = store(&fx);

        let r1 = index_repository(&s, &root, &policy, &opts).unwrap();
        let repo_id: RepositoryId = r1.repository_id.parse().unwrap();

        // Manually plant a cache row for "a.rs" with the CORRECT current
        // content_hash but a stale secret_pattern_version, simulating a
        // retry-recovery cache entry left over from before a ruleset
        // upgrade.
        let content_hash = fx
            .pool
            .with_reader(|c| attic_storage::current_path_hashes_for_repository(c, &repo_id))
            .unwrap()
            .into_iter()
            .find(|(p, _)| p == "a.rs")
            .map(|(_, h)| h)
            .expect("a.rs must have a content hash after indexing");
        let policy_for_cache = policy.clone();
        fx.handle
            .send(move |conn| {
                attic_storage::upsert_analysis_cache_entries(
                    conn,
                    &repo_id,
                    &[attic_storage::CachedFileAnalysis {
                        repo_relative: "a.rs".to_string(),
                        content_hash,
                        security_state: "clean".to_string(),
                        is_partial_scan: false,
                        secret_pattern_version: SECRET_PATTERN_VERSION - 1,
                        analyzer_registry_version: attic_core::constants::ANALYZER_REGISTRY_VERSION
                            .to_owned(),
                        discovery_policy_hash: policy_for_cache.hash().unwrap(),
                        structural: true,
                        max_units_per_file: 512,
                        units_json: "[]".to_string(),
                        captured_json: None,
                    }],
                    0,
                )
            })
            .unwrap();

        reset_analyze_single_file_calls();
        let r2 = index_repository(&s, &root, &policy, &opts).unwrap();
        assert_eq!(
            analyze_single_file_calls(),
            1,
            "a version-mismatched cache entry must be a miss, forcing fresh analysis"
        );
        assert_eq!(r2.files_indexed, 1);
        assert_eq!(search_hits(&fx, "cache_version_token").len(), 1);
    }
}
