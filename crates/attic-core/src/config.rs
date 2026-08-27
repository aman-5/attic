//! Production configuration model for Attic MCP server.
//!
//! Reads defaults from `crates/attic-core/src/constants.rs::resources` and
//! allows override via environment variables.  All values are compile-time
//! constants by default; only the override mechanism is runtime.
//!
//! Environment variables (all prefix attic_):
//!   ATTIC_MAX_FOREGROUND_QUERIES
//!   ATTIC_MAX_INDEXING_WORKERS
//!   ATTIC_TOTAL_MEMORY_BUDGET_MIB
//!   ATTIC_PER_REPO_MEMORY_BUDGET_MIB
//!   ATTIC_MAX_IO_OPS_PER_SEC
//!   ATTIC_WRITER_QUEUE_CAPACITY
//!   ATTIC_WRITER_BATCH_SIZE
//!   ATTIC_WRITER_FLUSH_INTERVAL_MS
//!   ATTIC_INCREMENTAL_TASK_QUEUE_CAPACITY
//!   ATTIC_RECONCILIATION_TASK_QUEUE_CAPACITY
//!   ATTIC_MIN_FREE_MEMORY_MIB
//!   ATTIC_BACKUP_RELATIVE_DIR
//!   ATTIC_MAX_BACKUP_RETAIN
//!   ATTIC_CHECKPOINT_WAL_FRAMES
//!   ATTIC_CHECKPOINT_MINUTES
//!   ATTIC_WAL_AUTOCKPT_ENABLED
//!   ATTIC_GRACEFUL_SHUTDOWN_TIMEOUT_MS
//!   ATTIC_STARTUP_INTEGRITY_CHECK
//!   ATTIC_STARTUP_FOREIGN_KEY_CHECK

use crate::constants::resources;

/// production configuration model; zero-allocation accessors into the
/// `resources` constants with optional env-var overrides.
#[derive(Debug, Clone)]
pub struct ProductionConfig {
    /// Maximum concurrent foreground MCP queries.  Prevents query flooding.
    pub max_foreground_queries: usize,
    /// Maximum concurrent indexing workers.  Prevents indexing from starving
    /// foreground queries (see foreground_priority.md §2).
    pub max_indexing_workers: usize,
    /// Total memory budget for all in-index operations (MiB).
    pub total_memory_budget_mib: u64,
    /// Per-repository memory budget ceiling (MiB).
    pub per_repo_memory_budget_mib: u64,
    /// Maximum disk I/O operations per second across all workers.
    pub max_io_ops_per_sec: u64,
    /// Maximum queue depth for the writer queue (pending mutations).
    pub writer_queue_capacity: usize,
    /// Maximum batch size for writer commits (mutations per transaction).
    pub writer_batch_size: usize,
    /// Flush the writer batch at least this often (ms).
    pub writer_flush_interval_ms: u64,
    /// Maximum number of pending incremental tasks in the task queue.
    pub incremental_task_queue_capacity: usize,
    /// Maximum number of pending reconciliation tasks.
    pub reconciliation_task_queue_capacity: usize,
    /// Maximum depth of graph traversal for evidence expansion.
    pub max_graph_depth: usize,
    /// Maximum nodes traversed in a single graph walk.
    pub max_graph_nodes: usize,
    /// Maximum tokens consumed by context building for a single query.
    pub max_context_tokens: usize,
    /// Default timeout for background tasks (ms).  Tasks exceeding this are
    /// cancelled and rescheduled.
    pub default_task_timeout_ms: u64,
    /// Minimum free memory (MiB) that must be retained after foreground work.
    /// Background indexing pauses if falling below this threshold.
    pub min_free_memory_mib: u64,
    /// Backup directory relative to database path (for crash recovery backups).
    pub backup_relative_dir: String,
    /// Maximum number of backup checkpoints to retain (REC-B2).
    pub max_backup_retain: usize,
    /// Checkpoint interval: every N WAL frames OR every M minutes, whichever comes first.
    pub checkpoint_wal_frames: u64,
    /// Checkpoint interval: every M minutes (alternative to WAL frames threshold).
    pub checkpoint_minutes: u64,
    /// Whether WAL auto-checkpoint is enabled.
    pub wal_autocpt_enabled: bool,
    /// Graceful shutdown timeout (ms).  Server waits this long for in-flight
    /// tasks to complete before force-exiting.
    pub graceful_shutdown_timeout_ms: u64,
    /// Whether integrity check is performed at startup.
    pub startup_integrity_check: bool,
    /// Whether foreign key check is performed at startup.
    pub startup_foreign_key_check: bool,
}

impl Default for ProductionConfig {
    fn default() -> Self {
        Self {
            max_foreground_queries: resources::MAX_FOREGROUND_QUERIES,
            max_indexing_workers: resources::MAX_INDEXING_WORKERS,
            total_memory_budget_mib: resources::TOTAL_MEMORY_BUDGET_MIB,
            per_repo_memory_budget_mib: resources::PER_REPO_MEMORY_BUDGET_MIB,
            max_io_ops_per_sec: resources::MAX_IO_OPS_PER_SEC,
            writer_queue_capacity: resources::WRITER_QUEUE_CAPACITY,
            writer_batch_size: resources::WRITER_BATCH_SIZE,
            writer_flush_interval_ms: resources::WRITER_FLUSH_INTERVAL_MS,
            incremental_task_queue_capacity: resources::INCREMENTAL_TASK_QUEUE_CAPACITY,
            reconciliation_task_queue_capacity: resources::RECONCILIATION_TASK_QUEUE_CAPACITY,
            max_graph_depth: resources::MAX_GRAPH_DEPTH,
            max_graph_nodes: resources::MAX_GRAPH_NODES,
            max_context_tokens: resources::MAX_CONTEXT_TOKENS,
            default_task_timeout_ms: resources::DEFAULT_TASK_TIMEOUT_MS,
            min_free_memory_mib: resources::MIN_FREE_MEMORY_MIB,
            backup_relative_dir: resources::BACKUP_RELATIVE_DIR.to_owned(),
            max_backup_retain: resources::MAX_BACKUP_RETAIN,
            checkpoint_wal_frames: resources::CHECKPOINT_WAL_FRAMES,
            checkpoint_minutes: resources::CHECKPOINT_MINUTES,
            wal_autocpt_enabled: resources::WAL_AUTOCKPT_ENABLED,
            graceful_shutdown_timeout_ms: resources::GRACEFUL_SHUTDOWN_TIMEOUT_MS,
            startup_integrity_check: resources::STARTUP_INTEGRITY_CHECK,
            startup_foreign_key_check: resources::STARTUP_FOREIGN_KEY_CHECK,
        }
    }
}

impl ProductionConfig {
    /// Apply environment-variable overrides to the default configuration.
    ///
    /// Environment variables use the `ATTIC_` prefix followed by the constant
    /// name in UPPER_SNAKE_CASE.  For example:
    ///   `ATTIC_MAX_FOREGROUND_QUERIES=128`
    ///   `ATTIC_TOTAL_MEMORY_BUDGET_MIB=2048`
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        // Helper to read env var or keep default
        macro_rules! env_or {
            ($env:ident, $default:expr) => {{
                std::env::var(stringify!($env))
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or($default)
            }};
        }

        cfg.max_foreground_queries = env_or!(ATTIC_MAX_FOREGROUND_QUERIES, cfg.max_foreground_queries);
        cfg.max_indexing_workers = env_or!(ATTIC_MAX_INDEXING_WORKERS, cfg.max_indexing_workers);
        cfg.total_memory_budget_mib = env_or!(ATTIC_TOTAL_MEMORY_BUDGET_MIB, cfg.total_memory_budget_mib);
        cfg.per_repo_memory_budget_mib = env_or!(ATTIC_PER_REPO_MEMORY_BUDGET_MIB, cfg.per_repo_memory_budget_mib);
        cfg.max_io_ops_per_sec = env_or!(ATTIC_MAX_IO_OPS_PER_SEC, cfg.max_io_ops_per_sec);
        cfg.writer_queue_capacity = env_or!(ATTIC_WRITER_QUEUE_CAPACITY, cfg.writer_queue_capacity);
        cfg.writer_batch_size = env_or!(ATTIC_WRITER_BATCH_SIZE, cfg.writer_batch_size);
        cfg.writer_flush_interval_ms = env_or!(ATTIC_WRITER_FLUSH_INTERVAL_MS, cfg.writer_flush_interval_ms);
        cfg.incremental_task_queue_capacity = env_or!(ATTIC_INCREMENTAL_TASK_QUEUE_CAPACITY, cfg.incremental_task_queue_capacity);
        cfg.reconciliation_task_queue_capacity = env_or!(ATTIC_RECONCILIATION_TASK_QUEUE_CAPACITY, cfg.reconciliation_task_queue_capacity);
        cfg.max_graph_depth = env_or!(ATTIC_MAX_GRAPH_DEPTH, cfg.max_graph_depth);
        cfg.max_graph_nodes = env_or!(ATTIC_MAX_GRAPH_NODES, cfg.max_graph_nodes);
        cfg.max_context_tokens = env_or!(ATTIC_MAX_CONTEXT_TOKENS, cfg.max_context_tokens);
        cfg.default_task_timeout_ms = env_or!(ATTIC_DEFAULT_TASK_TIMEOUT_MS, cfg.default_task_timeout_ms);
        cfg.min_free_memory_mib = env_or!(ATTIC_MIN_FREE_MEMORY_MIB, cfg.min_free_memory_mib);
        cfg.backup_relative_dir = env_or!(ATTIC_BACKUP_RELATIVE_DIR, cfg.backup_relative_dir);
        cfg.max_backup_retain = env_or!(ATTIC_MAX_BACKUP_RETAIN, cfg.max_backup_retain);
        cfg.checkpoint_wal_frames = env_or!(ATTIC_CHECKPOINT_WAL_FRAMES, cfg.checkpoint_wal_frames);
        cfg.checkpoint_minutes = env_or!(ATTIC_CHECKPOINT_MINUTES, cfg.checkpoint_minutes);
        cfg.wal_autocpt_enabled = env_or!(ATTIC_WAL_AUTOCKPT_ENABLED, cfg.wal_autocpt_enabled).unwrap_or(cfg.wal_autocpt_enabled);
        cfg.graceful_shutdown_timeout_ms = env_or!(ATTIC_GRACEFUL_SHUTDOWN_TIMEOUT_MS, cfg.graceful_shutdown_timeout_ms);
        cfg.startup_integrity_check = env_or!(ATTIC_STARTUP_INTEGRITY_CHECK, cfg.startup_integrity_check).unwrap_or(cfg.startup_integrity_check);
        cfg.startup_foreign_key_check = env_or!(ATTIC_STARTUP_FOREIGN_KEY_CHECK, cfg.startup_foreign_key_check).unwrap_or(cfg.startup_foreign_key_check);

        cfg
    }

    /// Return the backup directory path relative to the database file.
    pub fn backup_dir(&self, db_path: &std::path::Path) -> std::path::PathBuf {
        db_path.parent()
            .unwrap_or(std::path::Path::new("."))
            .join(&self.backup_relative_dir)
    }

    /// Validate that all configuration values are within acceptable bounds.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_foreground_queries == 0 {
            return Err("max_foreground_queries must be > 0".into());
        }
        if self.max_indexing_workers == 0 {
            return Err("max_indexing_workers must be > 0".into());
        }
        if self.total_memory_budget_mib == 0 {
            return Err("total_memory_budget_mib must be > 0".into());
        }
        if self.per_repo_memory_budget_mib == 0 {
            return Err("per_repo_memory_budget_mib must be > 0".into());
        }
        if self.max_io_ops_per_sec == 0 {
            return Err("max_io_ops_per_sec must be > 0".into());
        }
        if self.writer_queue_capacity == 0 {
            return Err("writer_queue_capacity must be > 0".into());
        }
        if self.writer_batch_size == 0 {
            return Err("writer_batch_size must be > 0".into());
        }
        if self.writer_flush_interval_ms == 0 {
            return Err("writer_flush_interval_ms must be > 0".into());
        }
        if self.incremental_task_queue_capacity == 0 {
            return Err("incremental_task_queue_capacity must be > 0".into());
        }
        if self.reconciliation_task_queue_capacity == 0 {
            return Err("reconciliation_task_queue_capacity must be > 0".into());
        }
        if self.max_graph_depth == 0 {
            return Err("max_graph_depth must be > 0".into());
        }
        if self.max_graph_nodes == 0 {
            return Err("max_graph_nodes must be > 0".into());
        }
        if self.max_context_tokens == 0 {
            return Err("max_context_tokens must be > 0".into());
        }
        if self.default_task_timeout_ms == 0 {
            return Err("default_task_timeout_ms must be > 0".into());
        }
        if self.min_free_memory_mib == 0 {
            return Err("min_free_memory_mib must be > 0".into());
        }
        if self.max_backup_retain < 1 {
            return Err("max_backup_retain must be >= 1".into());
        }
        if self.checkpoint_wal_frames == 0 {
            return Err("checkpoint_wal_frames must be > 0".into());
        }
        if self.checkpoint_minutes == 0 {
            return Err("checkpoint_minutes must be > 0".into());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Export production config from attic-core lib for use by attic-server.
// ---------------------------------------------------------------------------

pub use ProductionConfig;