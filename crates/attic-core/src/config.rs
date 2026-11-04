//! `attic.toml` configuration model - resource/indexing tunables
//! only. Replaces the dead `ProductionConfig` (zero production consumers).
//!
//! Pure parsing: this module never touches the filesystem. `AtticConfig`
//! deserializes TOML text that the caller (`attic-server`) reads from disk.
//! It is a second file living alongside — never merged with — the
//! `<ATTIC_HOME>/config/config.toml` workspace-membership file, which keeps
//! its own hand-rolled `[[repositories]]` grammar untouched.

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

fn default_structural_indexing() -> bool {
    true
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

/// `[knowledge]` table: one central folder of Markdown notes that Attic
/// serves as project knowledge for every repository in the workspace.
///
/// ON by default: with no `[knowledge]` table the folder is
/// `<ATTIC_HOME>/knowledge`, created on first start.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeConfig {
    /// `false` turns the central knowledge folder off entirely.
    #[serde(default = "default_knowledge_enabled")]
    pub enabled: bool,
    /// Use this folder instead of the default `<ATTIC_HOME>/knowledge`.
    /// It is never created for you; a path that does not exist or is not a
    /// directory turns the feature off with the reason shown in `status`,
    /// and never fails startup.
    #[serde(default)]
    pub dir: Option<String>,
}

fn default_knowledge_enabled() -> bool {
    true
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: None,
        }
    }
}

/// Parsed `attic.toml` — resource/indexing tunables only.
///
/// Never contains workspace-membership (`[[repositories]]`); that stays in
/// the separate `<ATTIC_HOME>/config/config.toml` and its hand-rolled parser,
/// completely untouched by this type. Unknown top-level tables are
/// rejected, so a misspelled table (`[indexng]`) fails loudly instead of
/// being silently ignored.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AtticConfig {
    /// `[resources]` table.
    #[serde(default)]
    pub resources: ResourceOverrides,
    /// Old embedding settings were recognized and removed during parsing.
    #[serde(skip)]
    pub removed_embedding_settings_present: bool,
    /// `[indexing]` table. Unconditionally present, empty `exclude` by
    /// default (no extra exclusions beyond the built-in ones).
    #[serde(default)]
    pub indexing: IndexingOverride,
    /// `[knowledge]` table. Absent = default folder `<ATTIC_HOME>/knowledge`.
    #[serde(default)]
    pub knowledge: KnowledgeConfig,
    /// `[logging]` table. Absent = file logging off until the `logging`
    /// tool turns it on.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// `[logging]` — persistent file-log level applied at every start. The
/// `logging` MCP tool still changes it at runtime (not written back).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// `off` | `error` | `warn` | `info` | `debug` | `trace`. `None` = off.
    #[serde(default)]
    pub file_level: Option<String>,
}

/// Accepted `[logging] file_level` values.
pub const FILE_LOG_LEVELS: &[&str] = &["off", "error", "warn", "info", "debug", "trace"];

impl LoggingConfig {
    /// The configured level, lowercased; `None` when unset.
    pub fn file_level(&self) -> Option<String> {
        self.file_level
            .as_ref()
            .map(|l| l.trim().to_ascii_lowercase())
    }
}

impl AtticConfig {
    /// Parse and validate `attic.toml` contents. Pure — does no I/O; the
    /// caller (`attic-server`) reads the file and hands the contents here.
    pub fn parse_str(contents: &str) -> Result<Self, ConfigError> {
        let mut document: toml::Value =
            toml::from_str(contents).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let mut removed = false;
        if let Some(table) = document.as_table_mut() {
            removed |= table.remove("semantic").is_some();
            removed |= table.remove("embedding").is_some();
            if let Some(resources) = table
                .get_mut("resources")
                .and_then(toml::Value::as_table_mut)
            {
                removed |= resources.remove("embedding_batch_size").is_some();
                removed |= resources.remove("embedding_worker_count").is_some();
            }
        }
        let mut cfg: Self = document
            .try_into()
            .map_err(|e: toml::de::Error| ConfigError::Parse(e.to_string()))?;
        cfg.removed_embedding_settings_present = removed;
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
        if let Some(level) = self.logging.file_level()
            && !FILE_LOG_LEVELS.contains(&level.as_str())
        {
            return Err(ConfigError::Invalid(format!(
                "[logging] file_level must be one of {} (got '{level}')",
                FILE_LOG_LEVELS.join("|")
            )));
        }
        if self
            .knowledge
            .dir
            .as_deref()
            .is_some_and(|d| d.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "[knowledge] dir must not be empty; remove the key to turn the feature off".into(),
            ));
        }
        Ok(())
    }
}

/// The `attic.toml` template written for a fresh install. Only `mode =
/// "auto"` ships active; every concrete `[resources]` field ships commented
/// out — shipping concrete numbers uncommented would make every install
/// silently override auto-tuning, defeating `ResourcePolicy` entirely.
pub const ATTIC_TOML_TEMPLATE: &str = r#"# attic.toml — resource and indexing tunables.
# Workspace membership lives separately in <ATTIC_HOME>/config/config.toml
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

[knowledge]
# Markdown notes served as project knowledge to EVERY repository (architecture,
# conventions, runbooks...). ON by default: the folder is <ATTIC_HOME>/knowledge
# (~/.attic/knowledge), created on first start. Drop .md files in and go.
# The folder's own README.md is skipped.
#
# Use your own folder instead (must already exist):
# dir = "C:\\path\\to\\attic\\knowledge"
#
# Turn the feature off:
# enabled = false

[logging]
# Level of the file log in <ATTIC_HOME>/logs, applied at every start
# (default: off). off | error | warn | info | debug | trace.
# The `logging` MCP tool can still change it until the next restart.
# file_level = "info"
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_model_settings_are_recognized_without_enabling_models() {
        let cfg = AtticConfig::parse_str(
            "[resources]\nscheduler_workers = 3\nembedding_batch_size = 32\n\
             [semantic]\nenabled = true\nmodel = \"old-model\"\n\
             [embedding]\nprovider = \"hashing\"\n",
        )
        .unwrap();
        assert!(cfg.removed_embedding_settings_present);
        assert_eq!(cfg.resources.scheduler_workers, Some(3));
        assert!(!AtticConfig::default().removed_embedding_settings_present);
    }

    #[test]
    fn invalid_indexing_values_fail_closed() {
        for (text, needle) in [
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
        ] {
            let error = AtticConfig::parse_str(text).unwrap_err();
            assert!(error.to_string().contains(needle), "{text}: {error}");
        }
    }

    #[test]
    fn shipped_template_enables_no_obsolete_controls() {
        let cfg = AtticConfig::parse_str(ATTIC_TOML_TEMPLATE).unwrap();
        assert_eq!(cfg.resources.mode, Some(ResourceModeSetting::Auto));
        assert!(cfg.resources.total_memory_budget_mib.is_none());
        assert!(!cfg.removed_embedding_settings_present);
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
    fn knowledge_table_is_optional_and_parses_a_dir() {
        let default = AtticConfig::default().knowledge;
        assert!(default.enabled, "knowledge is on by default");
        assert_eq!(default.dir, None);
        let cfg =
            AtticConfig::parse_str("[knowledge]\ndir = \"C:\\\\notes\\\\knowledge\"\n").unwrap();
        assert_eq!(cfg.knowledge.dir.as_deref(), Some("C:\\notes\\knowledge"));
        assert!(cfg.knowledge.enabled, "setting dir keeps it enabled");
        let off = AtticConfig::parse_str("[knowledge]\nenabled = false\n").unwrap();
        assert!(!off.knowledge.enabled);
    }

    #[test]
    fn knowledge_rejects_blank_dir_and_unknown_keys() {
        let err = AtticConfig::parse_str("[knowledge]\ndir = \"  \"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)), "got {err:?}");
        let err = AtticConfig::parse_str("[knowledge]\nfolder = \"x\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
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
    fn logging_file_level_parses_and_rejects_unknown_levels() {
        let cfg = AtticConfig::parse_str("[logging]\nfile_level = \"Debug\"\n").unwrap();
        assert_eq!(cfg.logging.file_level().as_deref(), Some("debug"));
        assert_eq!(AtticConfig::default().logging.file_level(), None);
        let err = AtticConfig::parse_str("[logging]\nfile_level = \"verbose\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("file_level must be one of"), "{err}");
        assert!(AtticConfig::parse_str(ATTIC_TOML_TEMPLATE).is_ok());
    }

    #[test]
    fn analysis_threads_zero_means_automatic_and_is_accepted() {
        let cfg = AtticConfig::parse_str("[indexing]\nanalysis_threads = 0\n").unwrap();
        assert_eq!(cfg.indexing.analysis_threads, Some(0));
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
