//! `attic.toml` configuration model (Phase 8) — resource/embedding tunables
//! only. Replaces the dead `ProductionConfig` (zero production consumers).
//!
//! Pure parsing: this module never touches the filesystem. `AtticConfig`
//! deserializes TOML text that the caller (`attic-server`) reads from disk.
//! It is a second, new file living alongside — never merged with — the
//! existing `<ATTIC_HOME>/config.toml`, which keeps its own hand-rolled
//! `[[repositories]]` workspace-membership grammar untouched.

use serde::Deserialize;

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
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

/// User-tunable resource overrides (`[resources]` in `attic.toml`).
///
/// Intentionally exposes only 7 of `ResourcePolicy`'s 11 controlled values.
/// `scheduler_workers`, `sqlite_cache_pages`, `sqlite_mmap_bytes`, and
/// `embedding_batch_size` remain mode-derived/automatic in V1 by design —
/// not parsed from this struct at all, so there is no parsed-and-ignored
/// field for them.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourceOverrides {
    /// `"auto"` (default), or an explicit forced mode.
    #[serde(default)]
    pub mode: ResourceModeSetting,
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
            mode: if matches!(other.mode, ResourceModeSetting::Auto) {
                self.mode
            } else {
                other.mode
            },
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

/// Explicit embedding provider override (`[embedding]` in `attic.toml`).
///
/// [FIX] `model` was deliberately removed, not just left unused: V1 has
/// exactly one loadable model (`BgeEmbedder` is hardcoded to
/// `bge-base-en-v1.5`, with no parameter to select another), so a `model`
/// override could never actually change anything — it silently accepted any
/// value while doing nothing, and worse, would make `re_index_recommended`
/// report `true` forever with no way to ever satisfy it (comparing a
/// configured-but-unreachable model name against the one real persisted
/// model). Rather than fix a knob to nowhere, dropping it entirely is more
/// honest: `provider` (`"bge"` vs `"hashing"`) is the only real, working
/// choice, so it's the only one exposed. An entirely absent or empty
/// `[embedding]` table deserializes to `{ provider: None }`, which
/// [`AtticConfig::has_explicit_embedding_override`] correctly reads as "no
/// override" (a value-level check, never a TOML-section-presence check).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingOverride {
    /// Explicit provider id (`"bge"` or `"hashing"`).
    pub provider: Option<String>,
}

/// User-tunable indexing/discovery overrides (`[indexing]` in `attic.toml`).
#[derive(Debug, Clone, Default, Deserialize)]
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
}

/// Parsed `attic.toml` — resource/embedding/indexing tunables only.
///
/// Never contains workspace-membership (`[[repositories]]`); that stays on
/// the existing, separate `<ATTIC_HOME>/config.toml` and its hand-rolled
/// parser, completely untouched by this type.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AtticConfig {
    /// `[resources]` table.
    #[serde(default)]
    pub resources: ResourceOverrides,
    /// `[embedding]` table. Unconditionally present (itself `Default`, both
    /// fields `None`) rather than `Option<EmbeddingOverride>` — explicit
    /// intent is a value-level check, never a presence-level one.
    #[serde(default)]
    pub embedding: EmbeddingOverride,
    /// `[indexing]` table. Unconditionally present, empty `exclude` by
    /// default (no extra exclusions beyond the built-in ones).
    #[serde(default)]
    pub indexing: IndexingOverride,
}

impl AtticConfig {
    /// Parse `attic.toml` contents. Pure — does no I/O; the caller
    /// (`attic-server`) reads the file and hands the contents here.
    pub fn parse_str(contents: &str) -> Result<Self, ConfigError> {
        toml::from_str(contents).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// True only when the user explicitly named a provider in `[embedding]`
    /// — never inferred from whether the TOML table exists.
    pub fn has_explicit_embedding_override(&self) -> bool {
        self.embedding.provider.is_some()
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

[embedding]
# Default: Attic's recommended embedding provider ("bge", a real neural
# embedder). Uncomment to force the deterministic offline baseline instead.
# provider = "hashing"

[indexing]
# Additional glob patterns to exclude from indexing, beyond .gitignore and
# Attic's built-in defaults (node_modules/, target/, build output, etc.).
# exclude = ["**/pom.xml"]
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_explicit_embedding_override() {
        let cfg = AtticConfig::default();
        assert!(!cfg.has_explicit_embedding_override());
        assert!(matches!(cfg.resources.mode, ResourceModeSetting::Auto));
    }

    #[test]
    fn empty_embedding_table_is_not_an_override() {
        let cfg = AtticConfig::parse_str("[resources]\nmode = \"auto\"\n\n[embedding]\n").unwrap();
        assert!(!cfg.has_explicit_embedding_override());
    }

    #[test]
    fn explicit_provider_is_an_override() {
        let cfg = AtticConfig::parse_str("[embedding]\nprovider = \"hashing\"\n").unwrap();
        assert!(cfg.has_explicit_embedding_override());
        assert_eq!(cfg.embedding.provider.as_deref(), Some("hashing"));
    }

    #[test]
    fn unknown_embedding_key_fails_to_parse() {
        // `model` no longer exists — an attic.toml left over from before this
        // change (or hand-written against stale docs) must fail loudly, not
        // silently parse and do nothing, per invariant #5.
        let result = AtticConfig::parse_str("[embedding]\nmodel = \"bge-large-en-v1.5\"\n");
        assert!(
            result.is_err(),
            "unknown [embedding] keys must be rejected, not silently ignored"
        );
    }

    #[test]
    fn invalid_mode_value_fails_to_parse() {
        let result = AtticConfig::parse_str("[resources]\nmode = \"performnace\"\n");
        assert!(result.is_err());
    }

    #[test]
    fn shipped_template_parses_and_has_no_overrides() {
        let cfg = AtticConfig::parse_str(ATTIC_TOML_TEMPLATE).unwrap();
        assert!(matches!(cfg.resources.mode, ResourceModeSetting::Auto));
        assert!(cfg.resources.total_memory_budget_mib.is_none());
        assert!(!cfg.has_explicit_embedding_override());
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
    fn resource_overrides_layer_keeps_base_mode_when_other_is_auto() {
        let base = ResourceOverrides {
            mode: ResourceModeSetting::Performance,
            ..Default::default()
        };
        let env = ResourceOverrides::default();
        let merged = base.layer(&env);
        assert!(matches!(merged.mode, ResourceModeSetting::Performance));
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
            ResourceModeSetting::Performance
        ));
        assert_eq!(cfg.resources.total_memory_budget_mib, Some(8192));
        assert_eq!(cfg.resources.max_io_ops_per_sec, Some(400));
    }
}
