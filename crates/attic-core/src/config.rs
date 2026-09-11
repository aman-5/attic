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

/// Policy defining the aggressiveness of resource consumption under a specific mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModePolicy {
    /// Relative CPU aggressiveness (0.0 to 1.0).
    pub cpu_aggressiveness: f32,
    /// Relative memory aggressiveness (0.0 to 1.0).
    pub memory_aggressiveness: f32,
    /// Relative disk aggressiveness (0.0 to 1.0).
    pub disk_aggressiveness: f32,
    /// Fraction of machine resources strictly reserved for developer workflows (0.0 to 1.0).
    pub developer_headroom: f32,
    /// Speed/aggressiveness of upscale transitions (0.0 to 1.0).
    pub scale_up_aggressiveness: f32,
    /// Priority weight given to interactive MCP latency (0.0 to 1.0).
    pub interactive_latency_priority: f32,
    /// Multiplier when operating on battery power (0.0 to 1.0).
    pub battery_aggressiveness: f32,
}

impl ModePolicy {
    /// Conservative policy for Low mode.
    pub fn low() -> Self {
        Self {
            cpu_aggressiveness: 0.25,
            memory_aggressiveness: 0.30,
            disk_aggressiveness: 0.30,
            developer_headroom: 0.35,
            scale_up_aggressiveness: 0.20,
            interactive_latency_priority: 0.90,
            battery_aggressiveness: 0.40,
        }
    }

    /// Moderate policy for Balanced mode.
    pub fn balanced() -> Self {
        Self {
            cpu_aggressiveness: 0.55,
            memory_aggressiveness: 0.60,
            disk_aggressiveness: 0.60,
            developer_headroom: 0.20,
            scale_up_aggressiveness: 0.50,
            interactive_latency_priority: 0.80,
            battery_aggressiveness: 0.50,
        }
    }

    /// High-throughput policy for Performance mode.
    pub fn performance() -> Self {
        Self {
            cpu_aggressiveness: 0.90,
            memory_aggressiveness: 0.85,
            disk_aggressiveness: 0.85,
            developer_headroom: 0.10,
            scale_up_aggressiveness: 0.80,
            interactive_latency_priority: 0.70,
            battery_aggressiveness: 0.60,
        }
    }

    /// Resolve policy for a given mode setting.
    pub fn for_mode(setting: ResourceModeSetting) -> Self {
        match setting {
            ResourceModeSetting::Low => Self::low(),
            ResourceModeSetting::Balanced | ResourceModeSetting::Auto => Self::balanced(),
            ResourceModeSetting::Performance => Self::performance(),
        }
    }
}

impl Default for ModePolicy {
    fn default() -> Self {
        Self::balanced()
    }
}

/// Source of electrical power powering the current host machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerSource {
    /// Running on wall / AC power.
    Ac,
    /// Running on battery power.
    Battery,
    /// Power source unknown or unreadable.
    Unknown,
}

/// Real-time physical telemetry captured from the host system.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MachineSnapshot {
    /// Total system RAM in MiB.
    pub total_memory_mib: u64,
    /// Currently available (unallocated or reclaimable) system RAM in MiB.
    pub available_memory_mib: u64,
    /// Resident Set Size (RSS) of the Attic process in MiB.
    pub attic_rss_mib: u64,
    /// Number of logical CPU cores on the host.
    pub logical_cpus: usize,
    /// Host system-wide CPU utilization (0.0 to 100.0%).
    pub cpu_utilization: f32,
    /// Fraction of CPU capacity currently available for use (0.0 to 1.0).
    pub available_cpu_fraction: f32,
    /// Free disk space available on the volume holding the semantic store in MiB.
    pub semantic_disk_free_mib: u64,
    /// Machine power source, if detectable.
    pub power_source: Option<PowerSource>,
}

/// Real-time snapshot of current queue depths and processing throughput across Attic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadSnapshot {
    /// Number of files/items pending canonical indexing.
    pub indexing_pending: u64,
    /// Number of active canonical indexing worker threads.
    pub indexing_active: usize,
    /// Number of semantic units awaiting embedding.
    pub semantic_pending: u64,
    /// Number of semantic units currently in-flight across inference lanes.
    pub semantic_inflight: u64,
    /// Total semantic units successfully embedded.
    pub semantic_completed: u64,
    /// Number of interactive MCP embedding queries pending.
    pub interactive_embedding_pending: u64,
    /// Recent canonical indexing rate (files/sec).
    pub indexing_rate: f64,
    /// Recent semantic embedding rate (chunks/sec).
    pub embedding_rate: f64,
    /// Recent average semantic batch latency in milliseconds.
    pub embedding_batch_latency_ms: f64,
    /// Recent average interactive MCP request latency in milliseconds.
    pub mcp_interactive_latency_ms: f64,
}

/// Explicit resource allocation computed by the Global Resource Orchestrator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceAllocation {
    /// Number of worker threads granted to canonical indexing.
    pub indexing_workers: usize,
    /// Dedicated CPU thread count allocated to the semantic inference runtime.
    pub semantic_cpu_threads: usize,
    /// Number of parallel inference lanes for embedding generation.
    pub semantic_inference_lanes: usize,
    /// Batch size allocated per inference pass.
    pub semantic_batch_size: usize,
    /// Maximum number of items pre-fetched from the durable semantic queue.
    pub semantic_prefetch_limit: usize,
    /// Dedicated concurrency slots reserved for interactive MCP requests.
    pub mcp_reserved_capacity: usize,
}

impl Default for ResourceAllocation {
    fn default() -> Self {
        Self {
            indexing_workers: 2,
            semantic_cpu_threads: 2,
            semantic_inference_lanes: 1,
            semantic_batch_size: 16,
            semantic_prefetch_limit: 32,
            mcp_reserved_capacity: 4,
        }
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}

impl Default for SemanticConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: default_semantic_model(),
            dimension: None,
        }
    }
}

/// User-tunable resource overrides (`[resources]` in `attic.toml`).
///
/// Intentionally exposes only 7 of `ResourcePolicy`'s 12 controlled values.
/// `scheduler_workers`, `sqlite_cache_pages`, `sqlite_mmap_bytes`,
/// `embedding_batch_size`, and `embedding_worker_count` remain
/// mode-derived/automatic in V1 by design — not parsed from this struct at
/// all, so there is no parsed-and-ignored field for them.
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
        }
    }
}

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
}

impl Default for IndexingOverride {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            structural: true,
        }
    }
}

/// Parsed `attic.toml` — resource/semantic/indexing tunables only.
///
/// Never contains workspace-membership (`[[repositories]]`); that stays on
/// the existing, separate `<ATTIC_HOME>/config.toml` and its hand-rolled
/// parser, completely untouched by this type.
#[derive(Debug, Clone, Default, Deserialize)]
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
    /// Parse `attic.toml` contents. Pure — does no I/O; the caller
    /// (`attic-server`) reads the file and hands the contents here.
    pub fn parse_str(contents: &str) -> Result<Self, ConfigError> {
        if contents.contains("[embedding]") {
            return Err(ConfigError::Parse(
                "the [embedding] table and generic provider selection are removed; configure the semantic engine using [semantic] (e.g. model = \"qwen3-embedding-0.6b\")".into(),
            ));
        }
        let cfg: Self = toml::from_str(contents).map_err(|e| ConfigError::Parse(e.to_string()))?;
        Ok(cfg)
    }
}

/// The `attic.toml` template written for a fresh install. Only `mode =
/// "auto"` ships active; every concrete `[resources]` field ships commented
/// out — shipping concrete numbers uncommented would make every install
/// silently override auto-tuning, defeating `ResourcePolicy` entirely.
pub const ATTIC_TOML_TEMPLATE: &str = r#"# attic.toml — resource/embedding/indexing tunables.
# Separate from <ATTIC_HOME>/config.toml, which continues to hold
# [[repositories]] workspace membership exactly as it does today, untouched.

[resources]
# Automatically selects low/balanced/performance from available RAM and CPU.
mode = "auto"

# Optional overrides — uncomment to override automatic tuning.
# total_memory_budget_mib = 4096
# min_free_memory_mib = 400
# max_foreground_queries = 64
# writer_batch_size = 256
# writer_flush_interval_ms = 50
# writer_queue_capacity = 512
# max_io_ops_per_sec = 200

[semantic]
# Production neural semantic model (Qwen3-Embedding-0.6B).
enabled = true
model = "qwen3-embedding-0.6b"

[indexing]
# Additional glob patterns to exclude from indexing, beyond .gitignore and
# Attic's built-in defaults (node_modules/, target/, build output, etc.).
# exclude = ["**/pom.xml"]

# Kill-switch: set to false to fall back to lexical-only indexing (no
# structural/AST analysis) if structural analysis misbehaves on this codebase.
# structural = true
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
    fn invalid_mode_value_fails_to_parse() {
        let result = AtticConfig::parse_str("[resources]\nmode = \"performnace\"\n");
        assert!(result.is_err());
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
