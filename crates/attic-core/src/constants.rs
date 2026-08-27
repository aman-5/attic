//! Global compile-time constants for schema versioning and secret scanning.

/// Current schema version string, embedded in every index generation.
pub const CURRENT_SCHEMA_VERSION: &str = "1.0.0";

/// Analyzer registry implementation version (compatibility contract:
/// recorded per index generation under `analyzer_registry`; bumped when the
/// bundled analyzer set changes so operators can detect stale generations).
/// 0.1.x = Phase 1C generic-only; 0.2.0 = Phase 3 structural languages.
pub const ANALYZER_REGISTRY_VERSION: &str = "0.2.0";

/// Version of the secret-pattern ruleset used during scanning.
/// Increment this whenever the ruleset changes to trigger re-scanning.
pub const SECRET_PATTERN_VERSION: i64 = 1;

/// Well-known keys used in the `subsystem_versions_json` map stored in
/// `core_index_generations`. Keep in sync with the migration SQL.
pub mod subsystem_keys {
    /// Schema migration version.
    pub const SCHEMA: &str = "schema";
    /// Analyzer registry version.
    pub const ANALYZER_REGISTRY: &str = "analyzer_registry";
    /// Segmentation algorithm version.
    pub const SEGMENTATION: &str = "segmentation";
    /// Indexer pipeline version.
    pub const INDEXER: &str = "indexer";
    /// Ranking algorithm version.
    pub const RANKING: &str = "ranking";
    /// Embedding model identifier.
    pub const EMBEDDING_MODEL: &str = "embedding_model";
    /// Secret-detector ruleset version (mirrors `SECRET_PATTERN_VERSION`).
    pub const SECRET_DETECTOR: &str = "secret_detector";
    /// General configuration version.
    pub const CONFIGURATION: &str = "configuration";
}

/// Resource management constants for Phase 7 production hardening.
///
/// These are compile-time defaults that the server may override from
/// configuration at startup.  They must not be changed between restarts
/// without a migration.
pub mod resources {
    /// Maximum concurrent foreground MCP queries.  Prevents query flooding.
    pub const MAX_FOREGROUND_QUERIES: usize = 64;

    /// Maximum concurrent indexing workers.  Prevents indexing from starving
    /// foreground queries (see foreground_priority.md §2).
    pub const MAX_INDEXING_WORKERS: usize = 8;

    /// Maximum concurrent semantic enrichment workers.  Only active when
    /// ATTIC_SEMANTIC=1; baseline hashing embedder is single-threaded.
    pub const MAX_SEMANTIC_WORKERS: usize = 4;

    /// Total memory budget for all in-index operations (MiB).  When approached,
    /// the system degrades by pausing semantic enrichment, reducing indexing
    /// concurrency, and rejecting expensive tasks.
    pub const TOTAL_MEMORY_BUDGET_MIB: u64 = 1024;

    /// Per-repository memory budget ceiling (MiB).  No single repository may
    /// consume more than this during indexing.
    pub const PER_REPO_MEMORY_BUDGET_MIB: u64 = 128;

    /// Maximum disk I/O operations per second across all workers.  When
    /// exceeded, the system backs off expensive fs operations.
    pub const MAX_IO_OPS_PER_SEC: u64 = 200;

    /// Maximum queue depth for the writer queue (pending mutations).  Beyond
    /// this, `WriterQueueHandle::send` returns `QueueFull`.
    pub const WRITER_QUEUE_CAPACITY: usize = 512;

    /// Maximum batch size for writer commits (mutations per transaction).
    pub const WRITER_BATCH_SIZE: usize = 256;

    /// Flush the writer batch at least this often (ms).
    pub const WRITER_FLUSH_INTERVAL_MS: u64 = 50;

    /// Maximum pending incremental tasks in the queue.
    pub const INCREMENTAL_TASK_QUEUE_CAPACITY: usize = 1024;

    /// Maximum number of pending reconciliation tasks.
    pub const RECONCILIATION_TASK_QUEUE_CAPACITY: usize = 256;

    /// Maximum depth of graph traversal for evidence expansion.
    pub const MAX_GRAPH_DEPTH: usize = 5;

    /// Maximum nodes traversed in a single graph walk.
    pub const MAX_GRAPH_NODES: usize = 500;

    /// Maximum tokens consumed by context building for a single query.
    pub const MAX_CONTEXT_TOKENS: usize = 8192;

    /// Default timeout for background tasks (ms).  Tasks exceeding this are
    /// cancelled and rescheduled.
    pub const DEFAULT_TASK_TIMEOUT_MS: u64 = 300_000;

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
    pub const MIN_FREE_MEMORY_MIB: u64 = 100;

    /// Backup directory relative to database path (for crash recovery backups).
    pub const BACKUP_RELATIVE_DIR: &str = "backups";

    /// Maximum number of backup checkpoints to retain (REC-B2).
    pub const MAX_BACKUP_RETAIN: usize = 3;

    /// Checkpoint interval: every N WAL frames OR every M minutes, whichever comes first.
    pub const CHECKPOINT_WAL_FRAMES: u64 = 1000;

    /// Checkpoint interval: every N minutes (alternative to WAL frames threshold).
    pub const CHECKPOINT_MINUTES: u64 = 5;

    /// Whether WAL auto-checkpoint is enabled.
    pub const WAL_AUTOCKPT_ENABLED: bool = true;

    /// Graceful shutdown timeout (ms).  Server waits this long for in-flight
    /// tasks to complete before force-exiting.
    pub const GRACEFUL_SHUTDOWN_TIMEOUT_MS: u64 = 30_000;

    /// Whether integrity check is performed at startup.
    pub const STARTUP_INTEGRITY_CHECK: bool = true;

    /// Whether foreign key check is performed at startup.
    pub const STARTUP_FOREIGN_KEY_CHECK: bool = true;
}
