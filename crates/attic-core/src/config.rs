//! `attic.toml` configuration model (Phase 8) — resource/embedding tunables
//! only. Replaces the dead `ProductionConfig` (zero production consumers).
//!
//! Pure parsing: this module never touches the filesystem. `AtticConfig`
//! deserializes TOML text that the caller (`attic-server`) reads from disk.
//! It is a second, new file living alongside — never merged with — the
//! existing `<ATTIC_HOME>/config.toml`, which keeps its own hand-rolled
//! `[[repositories]]` workspace-membership grammar untouched.

use serde::{Deserialize, Serialize};

/// Error parsing or validating `attic.toml`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConfigError {
    /// The TOML text could not be deserialized into [`AtticConfig`].
    #[error("invalid attic.toml: {0}")]
    Parse(String),
    /// The parsed configuration failed a range/relational validation check.
    #[error("invalid attic.toml configuration: {0}")]
    Invalid(String),
}

/// User-selectable resource mode, or `auto` to detect from hardware.
///
/// An enum (not `Option<String>`) so an invalid value like `"performnace"`
/// fails at deserialization time, not silently later when
/// `detect_resource_mode` never runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceModeSetting {
    /// Detect from `HardwareSnapshot` at every launch (the shipped default).
    #[default]
    Auto,
    /// Force the conservative baseline regardless of detected hardware.
    Low,
    /// Force the mid-tier baseline regardless of detected hardware.
    Balanced,
    /// Force the high-tier baseline regardless of detected hardware.
    Performance,
}

/// Helper returning true as default for semantic.enabled.
fn default_semantic_enabled() -> bool {
    true
}

/// Helper returning true as default for indexing.structural.
fn default_structural_indexing() -> bool {
    true
}

/// Helper returning default model name for semantic.model.
fn default_semantic_model() -> String {
    "qwen3-embedding-0.6b".to_string()
}

/// Modern semantic engine configuration (`[semantic]` in `attic.toml`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticConfig {
    /// Whether semantic embedding and search are enabled.
    #[serde(default = "default_semantic_enabled")]
    pub enabled: bool,
    /// Primary model identifier (default: "qwen3-embedding-0.6b").
    #[serde(default = "default_semantic_model")]
    pub model: String,
    /// Optional output dimension override (e.g. 512, 768, 1024).
    #[serde(default)]
    pub dimension: Option<usize>,
    /// Path globs whose units are never selected for embedding
    /// (case-insensitive; `*` stays within a path segment, `**` crosses
    /// segments, trailing `/` matches a directory anywhere). Excluded content
    /// stays fully lexical-searchable — this only keeps it out of the
    /// embedding queue.
    #[serde(default)]
    pub exclude_globs: Vec<String>,
    /// Files larger than this many bytes are never selected for embedding
    /// (generated exports/dumps). `None` uses the backend default: 8 MiB on a
    /// GPU, 256 KiB on CPU.
    #[serde(default)]
    pub max_file_bytes: Option<u64>,
    /// Minimum selection score (0.0–1.0) a unit needs to be embedded.
    /// Lower = more coverage, more GPU/CPU work. `None` uses the backend
    /// default: 0.0 on a GPU, 0.30 on CPU.
    #[serde(default)]
    pub min_score: Option<f64>,
    /// Maximum units embedded per repository. `None` uses the backend
    /// default: 100000 on a GPU, 2560 on CPU.
    #[serde(default)]
    pub max_units_per_repo: Option<usize>,
    /// Maximum units embedded across the whole workspace (bounds queue and
    /// vector storage). `None` uses the built-in default (100000).
    #[serde(default)]
    pub max_units_total: Option<usize>,
    /// Wall-clock budget for ONE background enrichment drive slice (ms).
    ///
    /// Bounds how long a slice keeps starting new batches; it is NOT a
    /// per-batch inference deadline. `None` uses the built-in default.
    #[serde(default)]
    pub drive_budget_ms: Option<u64>,
    /// Directory containing a local ONNX export (`model_fp16.onnx` +
    /// `tokenizer.json`) used by the GPU execution backend.
    ///
    /// GPU acceleration requires BOTH of the following, and silently stays
    /// on CPU if either is missing:
    ///  1. a Windows MSVC build (DirectML is compiled in automatically), and
    ///  2. this directory (or the `ATTIC_ONNX_MODEL_DIR` environment
    ///     variable, which this setting takes precedence over).
    ///
    /// This was previously env-var-only and completely undocumented, so a
    /// machine with a perfectly good GPU reported `backend = "candle-cpu"`
    /// with no indication of what was missing. `semantic_identity` now
    /// always explains which of the two conditions failed.
    #[serde(default)]
    pub onnx_model_dir: Option<String>,

    /// Fixed padded sequence length for the ONNX/DirectML provider.
    ///
    /// This is the model's read window, and it anchors the whole size chain:
    /// the selection gate admits at most `onnx_seq_len * MIN_BYTES_PER_TOKEN`
    /// bytes, so a unit that passes selection is always one the provider can
    /// actually read.
    ///
    /// Defaults to 1024 so the ONNX window matches the Candle window. It was
    /// previously pinned to 512 for throughput (padding waste grows with the
    /// window), but a 512-token window accepts only 1024 bytes while selection
    /// admitted 2048 — so every unit in between passed the gate and then failed
    /// permanently with "input too large". Lowering this trades coverage for
    /// speed and re-opens that gap unless the selection gate moves with it,
    /// which `SelectionConfig::for_provider_capacity` now guarantees.
    #[serde(default)]
    pub onnx_seq_len: Option<usize>,

    /// Padded tokens per GPU forward pass (ONNX/DirectML). Inputs are grouped
    /// into length buckets (32/48/64/96/…) and each pass carries
    /// `gpu_batch_tokens / bucket` items. Default 4096 — measured best on a
    /// 4 GB card (8192 was 3% faster but left < 0.2 GiB free). Raise it on
    /// cards with more VRAM.
    #[serde(default)]
    pub gpu_batch_tokens: Option<usize>,

    /// GPU core temperature (°C) at which embedding pauses. Default 90.
    /// Read via `nvidia-smi` (NVIDIA, Windows/Linux) or hwmon (Linux
    /// AMD/Intel); where no sensor exists (macOS, other Windows adapters)
    /// the guard is inactive and the OS's own thermal management applies.
    #[serde(default)]
    pub gpu_temp_pause_c: Option<u32>,

    /// GPU core temperature (°C) at or below which paused embedding
    /// resumes. Default 85; always clamped below `gpu_temp_pause_c`.
    #[serde(default)]
    pub gpu_temp_resume_c: Option<u32>,

    /// Seconds without pending chunks or semantic queries after which the
    /// model worker process is stopped, returning all of its memory (GPU
    /// VRAM, including the driver's pool, or CPU RAM) to the OS. The next
    /// chunk or query restarts it. Default 900 (15 minutes); 0 keeps the
    /// model resident for the life of the server.
    #[serde(default)]
    pub gpu_idle_unload_secs: Option<u64>,

    /// Minimum dedicated VRAM (MiB) for the GPU to be used at all. Below it
    /// embedding runs on CPU from startup, with the reason in `status`.
    /// Default 4096 (a nominal 4 GB card qualifies — drivers report ~3.9 GB,
    /// so a 1/16 slack is applied). 0 = always try the GPU.
    #[serde(default)]
    pub gpu_min_vram_mb: Option<u64>,

    /// Allow an integrated GPU (shared-memory carve-out). Default false:
    /// it competes with the desktop for the same RAM and is usually slower
    /// than the CPU path for this model.
    #[serde(default)]
    pub allow_integrated_gpu: Option<bool>,

    /// Which device to run embedding inference on.
    ///
    /// Accepted values: `"auto"` (default), `"cpu"`, `"cuda"`, `"metal"`.
    /// An unrecognized value logs a warning and falls back to `"auto"`
    /// rather than blocking startup.
    ///
    /// | Value    | Effect                                                    |
    /// |----------|-----------------------------------------------------------|
    /// | `auto`   | Metal on Apple Silicon, CUDA on Linux/Windows, else CPU   |
    /// | `cpu`    | Force CPU even when a GPU is present                      |
    /// | `cuda`   | Require NVIDIA; falls back to CPU with a logged reason    |
    /// | `metal`  | Require Apple Silicon; falls back to CPU with a reason    |
    ///
    /// GPU backends must also be compiled in (`candle-cuda` / `candle-metal`
    /// cargo features). When they are not, the selected device reports
    /// exactly that instead of silently using CPU.
    ///
    /// Note: AMD GPUs are not supported by the Candle backend on Linux —
    /// Candle has no ROCm device. AMD hosts run on CPU, and say so.
    #[serde(default)]
    pub device: Option<String>,
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_semantic_model(),
            dimension: None,
            exclude_globs: Vec::new(),
            max_file_bytes: None,
            min_score: None,
            max_units_per_repo: None,
            max_units_total: None,
            drive_budget_ms: None,
            onnx_model_dir: None,
            onnx_seq_len: None,
            gpu_batch_tokens: None,
            gpu_temp_pause_c: None,
            gpu_temp_resume_c: None,
            gpu_idle_unload_secs: None,
            gpu_min_vram_mb: None,
            allow_integrated_gpu: None,
            device: None,
        }
    }
}

/// Default for `[semantic] gpu_idle_unload_secs`.
pub const DEFAULT_GPU_IDLE_UNLOAD_SECS: u64 = 900;

/// Upper bound for `[semantic] gpu_idle_unload_secs` (one week).
pub const MAX_GPU_IDLE_UNLOAD_SECS: u64 = 7 * 24 * 3600;

/// Default for `[semantic] gpu_min_vram_mb`.
pub const DEFAULT_GPU_MIN_VRAM_MB: u64 = 4096;

/// Upper bound for `[semantic] gpu_min_vram_mb` (1 TiB).
pub const MAX_GPU_MIN_VRAM_MB: u64 = 1 << 20;

/// Why the GPU was (not) chosen at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpuEligibility {
    /// The GPU may be used.
    Eligible,
    /// Dedicated VRAM is below `gpu_min_vram_mb` (after slack).
    TooLittleVram {
        /// Reported dedicated VRAM.
        have_mb: u64,
        /// Configured floor.
        min_mb: u64,
    },
    /// Integrated GPU and `allow_integrated_gpu` is false.
    Integrated,
}

impl SemanticConfig {
    /// Effective idle-unload delay in seconds; 0 means never unload.
    pub fn idle_unload_secs(&self) -> u64 {
        self.gpu_idle_unload_secs
            .unwrap_or(DEFAULT_GPU_IDLE_UNLOAD_SECS)
    }

    /// Effective minimum dedicated VRAM in MiB; 0 means always try the GPU.
    pub fn min_vram_mb(&self) -> u64 {
        self.gpu_min_vram_mb.unwrap_or(DEFAULT_GPU_MIN_VRAM_MB)
    }

    /// Decide whether an adapter with `dedicated_mb` of VRAM may be used.
    /// Cards are sold by nominal size but drivers reserve a slice (a 4 GB
    /// A500 reports 3965 MiB), so the floor has a 1/16 slack.
    pub fn gpu_eligibility(&self, dedicated_mb: u64, integrated: bool) -> GpuEligibility {
        if integrated && !self.allow_integrated_gpu.unwrap_or(false) {
            return GpuEligibility::Integrated;
        }
        let min_mb = self.min_vram_mb();
        if dedicated_mb < min_mb - min_mb / 16 {
            return GpuEligibility::TooLittleVram {
                have_mb: dedicated_mb,
                min_mb,
            };
        }
        GpuEligibility::Eligible
    }
}

/// User-tunable resource overrides (`[resources]` in `attic.toml`).
///
/// Every field is optional: `None` keeps the hardware-detected mode's
/// baseline. Overrides are validated (`ResourcePolicy::validate`) and then
/// hardware-clamped as the final step, so an override can never exceed what
/// the machine can support. `sqlite_cache_pages`/`sqlite_mmap_bytes` remain
/// mode-derived by design.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceOverrides {
    /// `None` when not set at all (falls through to the next layer in
    /// `resolve_effective_config`'s precedence chain); `Some(Auto)` when
    /// EXPLICITLY set to `"auto"`/`ATTIC_RESOURCE_MODE=auto`. These two must
    /// stay distinguishable: an explicit env override of `auto` must still
    /// win over a toml `mode = "performance"` per the documented
    /// `env > toml` precedence, which a bare (non-`Option`) `Auto` value
    /// could not express (it would be indistinguishable from "unset").
    #[serde(default)]
    pub mode: Option<ResourceModeSetting>,
    /// Override for `ResourcePolicy::memory_budget_mib`.
    pub total_memory_budget_mib: Option<u64>,
    /// Override for `ResourcePolicy::min_free_memory_mib`.
    pub min_free_memory_mib: Option<u64>,
    /// Override for `ResourcePolicy::max_foreground_queries`.
    pub max_foreground_queries: Option<usize>,
    /// Override for `ResourcePolicy::writer_batch_size`.
    pub writer_batch_size: Option<usize>,
    /// Override for `ResourcePolicy::writer_flush_interval_ms`.
    pub writer_flush_interval_ms: Option<u64>,
    /// Override for `ResourcePolicy::writer_queue_capacity`.
    pub writer_queue_capacity: Option<usize>,
    /// Override for `ResourcePolicy::max_io_ops_per_sec`.
    pub max_io_ops_per_sec: Option<u32>,
    /// Override for `ResourcePolicy::scheduler_workers` (concurrent
    /// incremental reindex tasks and bootstrap repositories). Clamped to the
    /// physical core count.
    pub scheduler_workers: Option<usize>,
    /// Override for `ResourcePolicy::embedding_batch_size` (items per
    /// provider call). An explicit value bypasses the conservative default
    /// cap applied to the neural provider, but is still bounded by the
    /// provider's token budget.
    pub embedding_batch_size: Option<usize>,
    /// Override for `ResourcePolicy::embedding_worker_count`. Providers that
    /// serialize inference still run one effective lane.
    pub embedding_worker_count: Option<usize>,
}

impl ResourceOverrides {
    /// Layer `other` on top of `self`: every `Some` field in `other` wins,
    /// every `None` field falls back to `self`'s value. Used to apply env
    /// var overrides on top of `attic.toml` overrides (see resolution order
    /// in `attic_storage::resource_policy::resolve_effective_config`).
    pub fn layer(self, other: &ResourceOverrides) -> Self {
        Self {
            mode: other.mode.or(self.mode),
            total_memory_budget_mib: other
                .total_memory_budget_mib
                .or(self.total_memory_budget_mib),
            min_free_memory_mib: other.min_free_memory_mib.or(self.min_free_memory_mib),
            max_foreground_queries: other.max_foreground_queries.or(self.max_foreground_queries),
            writer_batch_size: other.writer_batch_size.or(self.writer_batch_size),
            writer_flush_interval_ms: other
                .writer_flush_interval_ms
                .or(self.writer_flush_interval_ms),
            writer_queue_capacity: other.writer_queue_capacity.or(self.writer_queue_capacity),
            max_io_ops_per_sec: other.max_io_ops_per_sec.or(self.max_io_ops_per_sec),
            scheduler_workers: other.scheduler_workers.or(self.scheduler_workers),
            embedding_batch_size: other.embedding_batch_size.or(self.embedding_batch_size),
            embedding_worker_count: other.embedding_worker_count.or(self.embedding_worker_count),
        }
    }
}

/// Upper bound accepted for `[indexing] analysis_threads`. Larger values are
/// almost certainly typos and would only oversubscribe the machine.
pub const MAX_ANALYSIS_THREADS: usize = 256;

/// User-tunable indexing/discovery overrides (`[indexing]` in `attic.toml`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexingOverride {
    /// Additional repo-relative glob patterns to exclude from indexing,
    /// beyond `.gitignore` and Attic's built-in defaults (`node_modules/`,
    /// `target/`, build output, etc. — see
    /// `attic_discovery::classification::DEFAULT_IGNORED_PATTERNS`).
    /// Converted 1:1 into `DiscoveryPolicy::attic_exclude_rules`
    /// (`attic_discovery::GlobRule::exclude`) at bootstrap time.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Operational kill-switch mirroring `attic_indexing::IndexOptions::structural`
    /// (previously hardcoded `true` with no way to flip it in production):
    /// when `false`, only the GenericAnalyzer runs (lexical-only indexing,
    /// no structural/AST analysis) for both bootstrap and incremental
    /// reindexing. Default `true` — current behavior unchanged unless
    /// explicitly set.
    #[serde(default = "default_structural_indexing")]
    pub structural: bool,
    /// Hard ceiling on retrieval units produced per file (maps to
    /// `attic_indexing::IndexOptions::max_units_per_file`). Since r01 this is
    /// FAIL-CLOSED: a file that exceeds the budget aborts the whole indexing
    /// generation rather than publishing silently truncated units. The
    /// default (100,000) is sized from the measured worst case in the target
    /// corpora (~9,700 units for a 4.5 MiB JSON export) with ~10x headroom;
    /// `None` = use the built-in default. Lower it only to make exhaustion
    /// failures surface earlier, never as a coverage fix.
    #[serde(default)]
    pub max_units_per_file: Option<usize>,
    /// Worker threads for per-file analysis (maps to
    /// `attic_indexing::IndexOptions::analysis_threads`). `None` or `0` =
    /// automatic (logical processors minus two, reserved for the developer's
    /// foreground work).
    #[serde(default)]
    pub analysis_threads: Option<usize>,
    /// Analyzer plugins to enable, by plugin id (e.g. `"java"`, `"swift"`,
    /// `"aem"`). Empty = every built-in plugin. Unknown ids fail startup.
    #[serde(default)]
    pub analyzers: Vec<String>,
    /// Analyzer plugins to disable, applied after `analyzers`. Files those
    /// plugins would handle stay fully searchable through the generic
    /// lexical analyzer.
    #[serde(default)]
    pub disabled_analyzers: Vec<String>,
}

impl Default for IndexingOverride {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            structural: true,
            max_units_per_file: None,
            analysis_threads: None,
            analyzers: Vec::new(),
            disabled_analyzers: Vec::new(),
        }
    }
}

impl IndexingOverride {
    /// Range and consistency checks that do not need to know which analyzer
    /// plugins exist (plugin ids are resolved by `attic-analyzers`).
    fn validate(&self) -> Result<(), ConfigError> {
        if self.exclude.iter().any(|p| p.trim().is_empty()) {
            return Err(ConfigError::Invalid(
                "[indexing] exclude must not contain empty patterns".into(),
            ));
        }
        if self.max_units_per_file == Some(0) {
            return Err(ConfigError::Invalid(
                "[indexing] max_units_per_file must be >= 1".into(),
            ));
        }
        if let Some(threads) = self.analysis_threads
            && threads > MAX_ANALYSIS_THREADS
        {
            return Err(ConfigError::Invalid(format!(
                "[indexing] analysis_threads must be <= {MAX_ANALYSIS_THREADS} (got {threads}); \
                 use 0 for automatic"
            )));
        }
        for (key, ids) in [
            ("analyzers", &self.analyzers),
            ("disabled_analyzers", &self.disabled_analyzers),
        ] {
            if ids.iter().any(|id| id.trim().is_empty()) {
                return Err(ConfigError::Invalid(format!(
                    "[indexing] {key} must not contain empty ids"
                )));
            }
        }
        if let Some(id) = self
            .analyzers
            .iter()
            .find(|id| self.disabled_analyzers.contains(id))
        {
            return Err(ConfigError::Invalid(format!(
                "[indexing] analyzer '{id}' is listed in both analyzers and disabled_analyzers"
            )));
        }
        Ok(())
    }
}

/// Parsed `attic.toml` — resource/semantic/indexing tunables only.
///
/// Never contains workspace-membership (`[[repositories]]`); that stays on
/// the existing, separate `<ATTIC_HOME>/config.toml` and its hand-rolled
/// parser, completely untouched by this type. Unknown top-level tables are
/// rejected, so a misspelled table (`[indexng]`) fails loudly instead of
/// being silently ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtticConfig {
    /// `[resources]` table.
    #[serde(default)]
    pub resources: ResourceOverrides,
    /// `[semantic]` table. Controls modern semantic engine settings (Master Plan V2 §26).
    #[serde(default)]
    pub semantic: SemanticConfig,
    /// `[indexing]` table. Unconditionally present, empty `exclude` by
    /// default (no extra exclusions beyond the built-in ones).
    #[serde(default)]
    pub indexing: IndexingOverride,
}

impl AtticConfig {
    /// Parse and validate `attic.toml` contents. Pure — does no I/O; the
    /// caller (`attic-server`) reads the file and hands the contents here.
    pub fn parse_str(contents: &str) -> Result<Self, ConfigError> {
        if contents.contains("[embedding]") {
            return Err(ConfigError::Parse(
                "the [embedding] table and generic provider selection are removed; configure the semantic engine using [semantic] (e.g. model = \"qwen3-embedding-0.6b\")".into(),
            ));
        }
        let cfg: Self = toml::from_str(contents).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Range and consistency checks for values that deserialize fine but are
    /// meaningless (zero sizes, empty patterns, contradictory analyzer
    /// lists). Resource values are validated separately after mode
    /// resolution (`ResourcePolicy::validate`), because their valid range
    /// depends on the other layers they are merged with.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.indexing.validate()?;
        if self.semantic.model.trim().is_empty() {
            return Err(ConfigError::Invalid(
                "[semantic] model must not be empty".into(),
            ));
        }
        if self.semantic.dimension == Some(0) {
            return Err(ConfigError::Invalid(
                "[semantic] dimension must be >= 1".into(),
            ));
        }
        if self.semantic.max_file_bytes == Some(0) {
            return Err(ConfigError::Invalid(
                "[semantic] max_file_bytes must be >= 1".into(),
            ));
        }
        if self.semantic.drive_budget_ms == Some(0) {
            return Err(ConfigError::Invalid(
                "[semantic] drive_budget_ms must be >= 1".into(),
            ));
        }
        if let Some(s) = self.semantic.min_score
            && !(0.0..=1.0).contains(&s)
        {
            return Err(ConfigError::Invalid(
                "[semantic] min_score must be within 0.0..=1.0".into(),
            ));
        }
        if self.semantic.max_units_per_repo == Some(0) || self.semantic.max_units_total == Some(0) {
            return Err(ConfigError::Invalid(
                "[semantic] max_units_per_repo and max_units_total must be >= 1".into(),
            ));
        }
        if self.semantic.gpu_batch_tokens == Some(0) {
            return Err(ConfigError::Invalid(
                "[semantic] gpu_batch_tokens must be >= 1".into(),
            ));
        }
        if let Some(p) = self.semantic.gpu_temp_pause_c
            && !(40..=110).contains(&p)
        {
            return Err(ConfigError::Invalid(
                "[semantic] gpu_temp_pause_c must be within 40..=110".into(),
            ));
        }
        if let Some(s) = self.semantic.gpu_idle_unload_secs
            && s > MAX_GPU_IDLE_UNLOAD_SECS
        {
            return Err(ConfigError::Invalid(format!(
                "[semantic] gpu_idle_unload_secs must be within 0..={MAX_GPU_IDLE_UNLOAD_SECS} (0 = never unload)"
            )));
        }
        if let Some(v) = self.semantic.gpu_min_vram_mb
            && v > MAX_GPU_MIN_VRAM_MB
        {
            return Err(ConfigError::Invalid(format!(
                "[semantic] gpu_min_vram_mb must be within 0..={MAX_GPU_MIN_VRAM_MB} (0 = always try the GPU)"
            )));
        }
        if self
            .semantic
            .exclude_globs
            .iter()
            .any(|g| g.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "[semantic] exclude_globs must not contain empty patterns".into(),
            ));
        }
        Ok(())
    }
}

/// The `attic.toml` template written for a fresh install. Only `mode =
/// "auto"` ships active; every concrete `[resources]` field ships commented
/// out — shipping concrete numbers uncommented would make every install
/// silently override auto-tuning, defeating `ResourcePolicy` entirely.
pub const ATTIC_TOML_TEMPLATE: &str = r#"# attic.toml — resource, semantic and indexing tunables.
# Workspace membership lives separately in <ATTIC_HOME>/config.toml
# ([[repositories]]), managed by the `workspace` MCP tool.
# Unknown tables or keys are rejected at startup. Environment variables
# (ATTIC_*) override values set here. Restart Attic to apply changes.

[resources]
# Automatically selects low/balanced/performance from available RAM and CPU.
mode = "auto"

# Optional overrides — uncomment to override automatic tuning. Every value is
# validated and then clamped to what this machine can support.
# total_memory_budget_mib = 4096
# min_free_memory_mib = 400
# max_foreground_queries = 64
# writer_batch_size = 256
# writer_flush_interval_ms = 50
# writer_queue_capacity = 512
# max_io_ops_per_sec = 200
# Concurrent incremental reindex tasks / bootstrapped repositories.
# scheduler_workers = 4
# Items per embedding call and concurrent embedding workers.
# embedding_batch_size = 16
# embedding_worker_count = 1

[semantic]
# Production neural semantic model (Qwen3-Embedding-0.6B).
enabled = true
model = "qwen3-embedding-0.6b"

# Admission policy for the embedding queue. Excluded content stays fully
# lexical-searchable; it is simply never embedded.
# Selection defaults depend on the embedding backend: full coverage on a
# GPU (min_score 0.0, max_units_per_repo 100000, max_file_bytes 8 MiB), and
# conservative on CPU (0.30 / 2560 / 256 KiB), where full coverage takes
# about an hour per repository. Values set here override both.
# Files larger than this are never embedded.
# max_file_bytes = 262144
# Additional paths to keep out of the embedding queue.
# exclude_globs = ["**/*.min.js", "testdata/"]

# How long one background enrichment slice keeps starting new batches
# (default: 60000 = 60s). This is NOT a per-batch inference deadline; a
# claimed batch always runs to completion under its own hang timeout.
# drive_budget_ms = 60000
# Minimum selection score a unit needs to be embedded (GPU 0.0, CPU 0.30).
# Lower it for more semantic coverage at the cost of more embedding work.
# min_score = 0.30
# Embedding caps: per repository (GPU 100000, CPU 2560) and workspace-wide
# (default 100000).
# max_units_per_repo = 2560
# max_units_total = 100000

# GPU (ONNX/DirectML) tunables. Inputs are grouped into length buckets
# (64/128/256/…) and each forward pass carries gpu_batch_tokens / bucket
# items. Default 4096 fits a 4 GB card; raise it on larger cards.
# gpu_batch_tokens = 4096
# Thermal guard: throttle at pause-1, pause at pause_c, resume at resume_c.
# Inactive where no sensor exists (macOS, non-NVIDIA Windows adapters).
# gpu_temp_pause_c = 90
# gpu_temp_resume_c = 85
# The model worker starts only when chunks are pending or a semantic query
# arrives. After this many idle seconds it is stopped and all of its GPU
# (or CPU) memory is released; the next chunk or query reloads it.
# 0 keeps the model resident.
# gpu_idle_unload_secs = 900
# GPU eligibility, decided once at startup (see `status` for the reason).
# A GPU with less dedicated VRAM than this runs embedding on CPU instead;
# a nominal 4 GB card qualifies. 0 = always try the GPU.
# gpu_min_vram_mb = 4096
# Integrated GPUs share system RAM with the desktop and are skipped unless
# allowed here.
# allow_integrated_gpu = false

[indexing]
# Additional glob patterns to exclude from indexing, beyond .gitignore and
# Attic's built-in defaults (node_modules/, target/, build/, dist/, .venv/, …).
# exclude = ["**/*.generated.ts", "docs/archive/**"]

# Kill-switch: set to false to fall back to lexical-only indexing (no
# structural/AST analysis) if structural analysis misbehaves on this codebase.
# structural = true

# Hard per-file retrieval-unit ceiling. FAIL-CLOSED: exceeding it aborts the
# indexing run instead of silently dropping content. Default 100000.
# max_units_per_file = 100000

# Per-file analysis worker threads. 0 = automatic (logical CPUs minus two).
# analysis_threads = 0

# Analyzer plugins. Empty = every built-in plugin. Files a disabled plugin
# would handle remain fully searchable through the generic lexical analyzer.
# Built-in ids: aem, java, python, go, javascript, typescript, json, c, cpp,
# ruby, csharp, scala, php, swift, lua, rust, kotlin, dockerfile.
# analyzers = ["java", "typescript", "aem"]
# disabled_analyzers = ["php"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_default_semantic() {
        let cfg = AtticConfig::default();
        assert!(cfg.semantic.enabled);
        assert_eq!(cfg.semantic.model, "qwen3-embedding-0.6b");
        assert!(cfg.resources.mode.is_none());
    }

    #[test]
    fn production_config_rejects_legacy_embedding_and_hashing() {
        let result = AtticConfig::parse_str("[embedding]\nprovider = \"hashing\"\n");
        assert!(
            result.is_err(),
            "production config must reject [embedding] / hashing"
        );
        let result2 = AtticConfig::parse_str("[embedding]\nprovider = \"qwen3\"\n");
        assert!(
            result2.is_err(),
            "production config must reject legacy [embedding] table entirely"
        );
    }

    #[test]
    fn indexing_config_accepts_max_units_per_file() {
        let cfg = AtticConfig::parse_str("[indexing]\nmax_units_per_file = 2048\n").unwrap();
        assert_eq!(cfg.indexing.max_units_per_file, Some(2048));
        let default = AtticConfig::parse_str("").unwrap();
        assert_eq!(default.indexing.max_units_per_file, None);
        // The shipped template must parse and leave the override absent.
        let template = AtticConfig::parse_str(ATTIC_TOML_TEMPLATE).unwrap();
        assert_eq!(template.indexing.max_units_per_file, None);
    }

    #[test]
    fn production_config_accepts_semantic_table() {
        let cfg = AtticConfig::parse_str(
            "[semantic]\nenabled = true\nmodel = \"qwen3-embedding-0.6b\"\ndimension = 512\n",
        )
        .unwrap();
        assert!(cfg.semantic.enabled);
        assert_eq!(cfg.semantic.model, "qwen3-embedding-0.6b");
        assert_eq!(cfg.semantic.dimension, Some(512));
    }

    #[test]
    fn semantic_disabled_config_parses() {
        let cfg = AtticConfig::parse_str("[semantic]\nenabled = false\n").unwrap();
        assert!(!cfg.semantic.enabled);
    }

    #[test]
    fn semantic_admission_keys_parse() {
        let cfg = AtticConfig::parse_str(
            "[semantic]\nexclude_globs = [\"*-Code.json\", \"fixtures/\"]\nmax_file_bytes = 131072\n",
        )
        .unwrap();
        assert_eq!(
            cfg.semantic.exclude_globs,
            vec!["*-Code.json".to_string(), "fixtures/".to_string()]
        );
        assert_eq!(cfg.semantic.max_file_bytes, Some(131_072));
    }

    #[test]
    fn invalid_mode_value_fails_to_parse() {
        let result = AtticConfig::parse_str("[resources]\nmode = \"performnace\"\n");
        assert!(result.is_err());
    }

    #[test]
    fn unknown_top_level_table_is_rejected() {
        let err = AtticConfig::parse_str("[indexng]\nstructural = false\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    #[test]
    fn worker_and_embedding_tunables_parse() {
        let cfg = AtticConfig::parse_str(
            "[resources]\nscheduler_workers = 6\nembedding_batch_size = 32\nembedding_worker_count = 2\n",
        )
        .unwrap();
        assert_eq!(cfg.resources.scheduler_workers, Some(6));
        assert_eq!(cfg.resources.embedding_batch_size, Some(32));
        assert_eq!(cfg.resources.embedding_worker_count, Some(2));
    }

    #[test]
    fn indexing_analysis_and_analyzer_keys_parse() {
        let cfg = AtticConfig::parse_str(
            "[indexing]\nanalysis_threads = 4\nanalyzers = [\"swift\", \"aem\"]\ndisabled_analyzers = [\"php\"]\n",
        )
        .unwrap();
        assert_eq!(cfg.indexing.analysis_threads, Some(4));
        assert_eq!(cfg.indexing.analyzers, ["swift", "aem"]);
        assert_eq!(cfg.indexing.disabled_analyzers, ["php"]);
    }

    #[test]
    fn invalid_indexing_values_fail_closed() {
        for (toml, needle) in [
            ("[indexing]\nmax_units_per_file = 0\n", "max_units_per_file"),
            (
                "[indexing]\nanalysis_threads = 100000\n",
                "analysis_threads",
            ),
            ("[indexing]\nexclude = [\"\"]\n", "exclude"),
            ("[indexing]\nanalyzers = [\" \"]\n", "analyzers"),
            (
                "[indexing]\nanalyzers = [\"swift\"]\ndisabled_analyzers = [\"swift\"]\n",
                "both",
            ),
            ("[semantic]\ndimension = 0\n", "dimension"),
            ("[semantic]\nmax_file_bytes = 0\n", "max_file_bytes"),
            ("[semantic]\nmin_score = 1.5\n", "min_score"),
            ("[semantic]\nmax_units_per_repo = 0\n", "max_units_per_repo"),
            ("[semantic]\ngpu_batch_tokens = 0\n", "gpu_batch_tokens"),
            ("[semantic]\ngpu_temp_pause_c = 200\n", "gpu_temp_pause_c"),
            (
                "[semantic]\ngpu_idle_unload_secs = 99999999\n",
                "gpu_idle_unload_secs",
            ),
            (
                "[semantic]\ngpu_min_vram_mb = 99999999999\n",
                "gpu_min_vram_mb",
            ),
            ("[semantic]\nmodel = \"\"\n", "model"),
        ] {
            match AtticConfig::parse_str(toml) {
                Err(ConfigError::Invalid(msg)) => {
                    assert!(msg.contains(needle), "{toml:?} → {msg}")
                }
                other => panic!("{toml:?} must be rejected as invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn gpu_eligibility_gate() {
        let cfg = SemanticConfig::default();
        // A nominal 4 GB card reports ~3965 MiB and must qualify.
        assert_eq!(cfg.gpu_eligibility(3965, false), GpuEligibility::Eligible);
        assert_eq!(
            cfg.gpu_eligibility(2048, false),
            GpuEligibility::TooLittleVram {
                have_mb: 2048,
                min_mb: 4096
            }
        );
        assert_eq!(cfg.gpu_eligibility(8192, true), GpuEligibility::Integrated);
        let open = SemanticConfig {
            gpu_min_vram_mb: Some(0),
            allow_integrated_gpu: Some(true),
            ..SemanticConfig::default()
        };
        assert_eq!(open.gpu_eligibility(0, true), GpuEligibility::Eligible);
    }

    #[test]
    fn analysis_threads_zero_means_automatic_and_is_accepted() {
        let cfg = AtticConfig::parse_str("[indexing]\nanalysis_threads = 0\n").unwrap();
        assert_eq!(cfg.indexing.analysis_threads, Some(0));
    }

    #[test]
    fn shipped_template_parses_and_has_no_overrides() {
        let cfg = AtticConfig::parse_str(ATTIC_TOML_TEMPLATE).unwrap();
        // The shipped template explicitly writes `mode = "auto"`, so this is
        // `Some(Auto)` (an explicit choice), not `None` (unset) — see
        // `default_config_has_no_explicit_embedding_override` for the
        // actually-unset case.
        assert!(matches!(
            cfg.resources.mode,
            Some(ResourceModeSetting::Auto)
        ));
        assert!(cfg.resources.total_memory_budget_mib.is_none());
        assert!(cfg.semantic.enabled);
        assert_eq!(cfg.semantic.model, "qwen3-embedding-0.6b");
    }

    #[test]
    fn resource_overrides_layer_prefers_other_when_set() {
        let base = ResourceOverrides {
            total_memory_budget_mib: Some(1000),
            max_foreground_queries: Some(10),
            ..Default::default()
        };
        let env = ResourceOverrides {
            total_memory_budget_mib: Some(2000),
            ..Default::default()
        };
        let merged = base.layer(&env);
        assert_eq!(merged.total_memory_budget_mib, Some(2000));
        assert_eq!(merged.max_foreground_queries, Some(10));
    }

    #[test]
    fn resource_overrides_layer_keeps_base_mode_when_other_is_unset() {
        let base = ResourceOverrides {
            mode: Some(ResourceModeSetting::Performance),
            ..Default::default()
        };
        let env = ResourceOverrides::default();
        let merged = base.layer(&env);
        assert!(matches!(
            merged.mode,
            Some(ResourceModeSetting::Performance)
        ));
    }

    #[test]
    fn resource_overrides_layer_prefers_other_explicit_auto_over_base_mode() {
        // The precedence-breaking case this type exists to prevent: an
        // explicit `Some(Auto)` in `other` (e.g. `ATTIC_RESOURCE_MODE=auto`)
        // must still win over a set `self.mode`, exactly like any other
        // explicit `other` value would — it must NOT be treated as if `other`
        // left mode unset.
        let base = ResourceOverrides {
            mode: Some(ResourceModeSetting::Performance),
            ..Default::default()
        };
        let env = ResourceOverrides {
            mode: Some(ResourceModeSetting::Auto),
            ..Default::default()
        };
        let merged = base.layer(&env);
        assert!(matches!(merged.mode, Some(ResourceModeSetting::Auto)));
    }

    #[test]
    fn full_resources_table_parses() {
        let toml = r#"
            [resources]
            mode = "performance"
            total_memory_budget_mib = 8192
            min_free_memory_mib = 512
            max_foreground_queries = 128
            writer_batch_size = 512
            writer_flush_interval_ms = 25
            writer_queue_capacity = 1024
            max_io_ops_per_sec = 400
        "#;
        let cfg = AtticConfig::parse_str(toml).unwrap();
        assert!(matches!(
            cfg.resources.mode,
            Some(ResourceModeSetting::Performance)
        ));
        assert_eq!(cfg.resources.total_memory_budget_mib, Some(8192));
        assert_eq!(cfg.resources.max_io_ops_per_sec, Some(400));
    }
}
