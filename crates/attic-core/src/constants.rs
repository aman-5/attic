//! Global compile-time constants for schema versioning and secret scanning.

/// Current schema version string, embedded in every index generation.
pub const CURRENT_SCHEMA_VERSION: &str = "1.0.0";

/// Analyzer registry implementation version (compatibility contract:
/// recorded per index generation under `analyzer_registry`; bumped when the
/// bundled analyzer set changes so operators can detect stale generations).
/// 0.1.x = Phase 1C generic-only; 0.2.0 = Phase 3 structural languages;
/// 0.3.0 = Tier-1 analyzers for Kotlin, Scala, Lua, Ruby, PHP, Swift, C, C++,
/// C#, Rust and Dockerfile (existing analyses of those languages are redone).
pub const ANALYZER_REGISTRY_VERSION: &str = "0.3.0";

/// Version of the secret-pattern ruleset used during scanning.
/// Increment this whenever the ruleset changes to trigger re-scanning.
pub const SECRET_PATTERN_VERSION: i64 = 1;

/// Chunking/segmentation strategy version — a REAL, live value (Phase: identity
/// split). MUST be bumped whenever chunk boundary logic changes (e.g. the
/// generic analyzer's TARGET_CHUNK_CHARS or a content-class router), so stale
/// generations are detectable instead of silently mixing chunk shapes.
/// 1.x = legacy line-count cap; 2.0.0 = character-target (TARGET_CHUNK_CHARS).
pub const CHUNKING_VERSION: &str = "2.0.0";

/// Well-known keys used in the `subsystem_versions_json` map stored in
/// `core_index_generations`.
pub mod subsystem_keys {
    /// Schema migration version.
    pub const SCHEMA: &str = "schema";
    /// Analyzer registry version.
    pub const ANALYZER_REGISTRY: &str = "analyzer_registry";
    /// Indexer pipeline version.
    pub const INDEXER: &str = "indexer";
    /// Secret-detector ruleset version (mirrors `SECRET_PATTERN_VERSION`).
    pub const SECRET_DETECTOR: &str = "secret_detector";
}

/// Built-in resource defaults, used when neither `attic.toml [resources]`
/// nor the resource policy (hardware/mode sizing) supplies a value; see
/// `attic_storage::resource_policy` and `resource_manager::ResourceConfig`.
pub mod resources {
    /// Maximum concurrent foreground MCP queries.  Prevents query flooding.
    pub const MAX_FOREGROUND_QUERIES: usize = 64;

    /// Maximum concurrent indexing workers.  Prevents indexing from starving
    /// foreground queries.
    pub const MAX_INDEXING_WORKERS: usize = 8;

    /// Maximum concurrent semantic enrichment workers.
    pub const MAX_SEMANTIC_WORKERS: usize = 4;

    /// Total memory budget for all in-index operations (MiB).  When approached,
    /// the system degrades by pausing semantic enrichment, reducing indexing
    /// concurrency, and rejecting expensive tasks.
    pub const TOTAL_MEMORY_BUDGET_MIB: u64 = 4096;

    /// Per-repository memory budget ceiling (MiB).  No single repository may
    /// consume more than this during indexing.
    pub const PER_REPO_MEMORY_BUDGET_MIB: u64 = 512;

    /// Minimum free memory (MiB) that must be retained after foreground work.
    /// Background indexing pauses if falling below this threshold.
    ///
    /// MUST stay below 15% of `TOTAL_MEMORY_BUDGET_MIB` (i.e. below
    /// `100 - resource_manager::PRESSURE_CRITICAL_PCT`). The Emergency tier
    /// triggers when free memory drops below this value; if it implies an
    /// Emergency floor at or below the Critical percentage (85%), Critical
    /// becomes unreachable (Emergency always preempts it first). See
    /// `ResourceConfig::validate` / `resource_manager::safe_min_free_mib`,
    /// which reject or clamp configurations that violate this invariant.
    pub const MIN_FREE_MEMORY_MIB: u64 = 400;

    /// Backup directory, relative to the Attic home (crash-recovery backups).
    pub const BACKUP_RELATIVE_DIR: &str = "backups";

    /// Graceful shutdown timeout (ms).  The server waits this long for
    /// in-flight tasks to complete before force-exiting.
    pub const GRACEFUL_SHUTDOWN_TIMEOUT_MS: u64 = 30_000;
}
