// crates/attic-server/src/main.rs
// Phase 1D – MCP server (rmcp-based), no raw rusqlite writes, DbPool readers +
// coordinated WriterQueueHandle writer.  Workspace indexing runs exclusively
// through the approved Phase 1A coordinated publication service; the `file`
// tool serves bounded regions with UTF-8-safe offsets, checked numeric
// arguments, and genuine bounded streaming for LARGE files.

mod daemon;
mod eviction;
mod handlers;
mod startup;
mod tools;
mod validate;
mod workspace_stores;

use attic_discovery::{
    DiscoveryPolicy, GlobRule, SecretScanDecision, canonicalize_within_root,
    preprocess_file_content, security::is_security_forbidden,
};
#[cfg(test)]
use attic_indexing::index_repository;
use attic_indexing::{IndexError, IndexOptions, IndexingStore};
use attic_storage::{
    DbPool, MAX_SEARCH_RESULTS, StorageError, WriterQueue, WriterQueueHandle,
    current_files_for_repo_map, get_db_stats, get_repository_path, get_repository_stats,
    lookup_repository_by_root_path, resource_manager::ResourceMonitor,
};
use handlers::context::*;
use handlers::file::*;
use handlers::repo_map::*;
// The standalone `search` tool was merged into `context` (mode="SEARCH");
// the handler remains for that delegation and for tests.
#[cfg(test)]
use handlers::search::*;
use handlers::status::*;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    },
    service::RequestContext,
};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    io,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use thiserror::Error;
use tools::make_tools;
use tracing::{error, info, warn};
use tracing_subscriber::{
    Layer, filter::LevelFilter, layer::SubscriberExt, reload, util::SubscriberInitExt,
};
use validate::{parse_path_scope, require_active_member, validate_filter, validate_repository_id};

/// Runtime kill switch for file logging (see `main`'s subscriber setup and
/// the `logging` MCP tool) — `None` until `main` initializes it, which
/// happens before the server ever accepts a tool call.
static LOG_RELOAD_HANDLE: OnceLock<reload::Handle<LevelFilter, tracing_subscriber::Registry>> =
    OnceLock::new();

#[derive(Clone)]
struct LazyFileLogWriter {
    log_dir: PathBuf,
    state: Arc<Mutex<LazyFileLogWriterState>>,
}

struct LazyFileLogWriterState {
    writer: Option<tracing_appender::non_blocking::NonBlocking>,
    _guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    error_reported: bool,
}

impl LazyFileLogWriter {
    fn new(log_dir: PathBuf) -> Self {
        Self {
            log_dir,
            state: Arc::new(Mutex::new(LazyFileLogWriterState {
                writer: None,
                _guard: None,
                error_reported: false,
            })),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LazyFileLogWriter {
    type Writer = Box<dyn io::Write + Send + 'static>;

    fn make_writer(&'a self) -> Self::Writer {
        let Ok(mut state) = self.state.lock() else {
            return Box::new(io::sink());
        };

        if let Some(writer) = &state.writer {
            return Box::new(writer.clone());
        }

        match tracing_appender::rolling::RollingFileAppender::builder()
            .rotation(tracing_appender::rolling::Rotation::DAILY)
            .filename_prefix("attic.log")
            .build(&self.log_dir)
        {
            Ok(file_appender) => {
                let (writer, guard) = tracing_appender::non_blocking(file_appender);
                state.writer = Some(writer.clone());
                state._guard = Some(guard);
                Box::new(writer)
            }
            Err(e) => {
                if !state.error_reported {
                    eprintln!(
                        "failed to initialize Attic file log in '{}': {e}",
                        self.log_dir.display()
                    );
                    state.error_reported = true;
                }
                Box::new(io::sink())
            }
        }
    }
}

/// Map a poisoned RwLock/Mutex to [`ServerError::Retrieval`] in handler
/// functions that return `Result<_, ServerError>`.
macro_rules! lock_or_server_err {
    ($expr:expr, $name:literal) => {
        $expr.map_err(|_| {
            ServerError::Retrieval(format!("server lock poisoned ({}); restart Attic", $name))
        })
    };
}

/// Acquire a lock inside an async `call_tool` handler, returning a structured
/// `CallToolResult::error` early if the lock is poisoned rather than panicking
/// the MCP process.  Only valid inside async move blocks returning
/// `Result<CallToolResponse, McpError>`.
macro_rules! lock_or_call_err {
    ($expr:expr, $name:literal) => {
        match $expr {
            Ok(g) => g,
            Err(_) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(
                    serde_json::json!({
                        "error": "internal_error",
                        "message": concat!(
                            "server lock poisoned (",
                            $name,
                            "); restart Attic"
                        )
                    })
                    .to_string(),
                )])
                .into())
            }
        }
    };
}

const SERVER_NAME: &str = "attic";
const SERVER_VERSION: &str = "0.1.0";

// ─── input / resource limits ───────────────────────────────────────────────────

/// Maximum accepted value for any single line/byte argument.  Anything above
/// this is rejected outright before any work happens (overflow guard).
pub(crate) const MAX_REGION_VALUE: u64 = 1 << 48;

/// Largest line-window a single request may cover.
pub(crate) const MAX_LINE_SPAN: u64 = 100_000;

/// Largest byte-window a single request may cover.
pub(crate) const MAX_BYTE_SPAN: u64 = 8 * 1024 * 1024;

/// Hard cap on the bytes returned in one tool response.  Applies to every
/// `file` response, streamed or not.
pub(crate) const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Absolute bound on how many bytes of a LARGE file's redacted stream are
/// scanned while assembling one response.  Prevents unbounded work even when
/// a caller supplies an open-ended window far beyond EOF.
pub(crate) const MAX_STREAM_SCAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ACTIVE_REPOSITORY_STORES: usize = attic_crossrepo::limits::MAX_REPOSITORIES_PER_RUN + 1;

#[derive(Debug, Error)]
enum ServerError {
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    #[error("indexing error: {0}")]
    Indexing(#[from] IndexError),
    #[error("discovery I/O: {0}")]
    Discovery(#[from] io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid argument: {0}")]
    InvalidArg(String),
    #[error("retrieval error: {0}")]
    Retrieval(String),
}

struct BootstrapJob {
    root_key: String,
    cancellation: attic_core::CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
pub(crate) struct AtticServer {
    stores: Option<Arc<attic_storage::RepositoryStoreManager>>,
    repository_lease: Option<Arc<attic_storage::RepositoryStoreLease>>,
    repository_registration_gate: Arc<std::sync::Mutex<()>>,
    repository_runtimes: workspace_stores::RuntimeMap,
    catalog_generations: Arc<std::sync::RwLock<HashMap<String, u64>>>,
    pool: DbPool,
    writer: WriterQueueHandle,
    _queue: Arc<WriterQueue>,
    /// Phase 2 incremental service, keyed by `repository_id`. One entry per
    /// successfully bootstrapped configured root — a multi-root workspace
    /// runs one watcher per repository, sharing this process's single
    /// pool/writer/scheduler (see §10-12 of the multi-root design).
    /// `Arc<RwLock<...>>` so the runtime `workspace` tool can add/remove
    /// membership through `&self`.
    incremental:
        Arc<std::sync::RwLock<HashMap<String, Arc<attic_incremental::IncrementalService>>>>,
    /// Which change-detection mechanism is running per `repository_id`
    /// (absent key = incremental disabled/not yet started for that repo).
    watch_mode: Arc<std::sync::RwLock<HashMap<String, attic_incremental::WatchMode>>>,
    /// Live change-detection handles, keyed by `repository_id`. Owned here
    /// (not by `main`) so the runtime `workspace` MCP tool can start and stop
    /// watchers for roots that are added/removed while the process is up.
    watches: Arc<std::sync::Mutex<HashMap<String, attic_incremental::IncrementalWatch>>>,
    /// Server-owned bootstrap jobs. Never detached: shutdown cancels and joins all of them.
    bootstrap_jobs: Arc<std::sync::Mutex<Vec<BootstrapJob>>>,
    /// Shared scheduler is owned by the server so background startup can install it
    /// after MCP serving has already begun, and shutdown can stop it deterministically.
    scheduler: Arc<std::sync::Mutex<Option<attic_incremental::SchedulerHandle>>>,
    /// Whether the logical workspace is configured (any `ATTIC_CONFIG`, the
    /// persistent default config file, or `ATTIC_WORKSPACE_ROOT`). `false`
    /// (UNCONFIGURED first run) gates query tools and drives status.
    workspace_configured: Arc<std::sync::atomic::AtomicBool>,
    /// Current active (validated, canonicalized) roots, in config order.
    /// Updated on runtime `workspace` mutations; the authoritative set for
    /// membership-scoped outputs (status/query guards/WorkspaceSnapshot).
    active_roots: Arc<std::sync::RwLock<Vec<PathBuf>>>,
    /// Path of the persistent default workspace config file (where the MCP
    /// `workspace` tool writes runtime membership changes).
    default_config: PathBuf,
    /// Configured-but-unavailable roots this run (spec §17): preserved from
    /// configuration, reported by `status` as degraded, never active.
    unavailable_roots: Arc<std::sync::RwLock<Vec<(PathBuf, String)>>>,
    /// §23 degraded-add marker: roots whose `bootstrap_workspace` succeeded
    /// (config is authoritative) but whose post-config indexing task failed
    /// at runtime.  Reported by `status` as degraded/pending so the caller
    /// can see the failure without requiring a restart.  In-memory only —
    /// a restart will re-attempt indexing from the persisted config.
    /// [FIX] Value is the real error's `Display` text — previously just a
    /// `HashSet<PathBuf>`, so `status` could only ever report the hardcoded
    /// literal `"indexing_failed"` for every failure, never the actual
    /// reason, even though the real message was already being logged
    /// (`tracing::warn!("... bootstrap failed: {e}")`) and simply never
    /// carried through to the API.
    pending_index_failed: Arc<std::sync::Mutex<HashMap<PathBuf, String>>>,
    /// Phase 6 cross-repo subsystem health.  `true` = degraded: sync
    /// failed or has not yet completed.  Cross-repo-dependent answers are
    /// prevented until this clears.
    crossrepo_degraded: Arc<std::sync::atomic::AtomicBool>,
    /// Path to the Attic database file, needed for checkpoint+backup.
    db_path: std::path::PathBuf,
    /// Phase 7 resource monitor for foreground/background priority control.
    resource_monitor: Option<Arc<attic_storage::resource_manager::ResourceMonitor>>,
    /// Phase 8: the `ResourceMode` selected at startup (detected from
    /// hardware, or forced via `attic.toml`/`ATTIC_RESOURCE_MODE`).
    resource_mode: attic_storage::ResourceMode,
    /// Phase 8: where `resource_mode` came from — answers "did my
    /// `attic.toml`/env override actually take effect?" for `status`.
    resource_mode_source: attic_storage::ResourceModeSource,
    /// Phase 8: the fully-resolved, hardware-clamped resource values
    /// actually handed to the scheduler / SQLite / writer / `ResourceMonitor`.
    effective_resources: attic_storage::EffectiveResourceConfig,
    /// Phase 8: parsed `attic.toml` (defaults when the file is absent).
    attic_config: attic_core::AtticConfig,
    /// PR-3 discovery explainability: the most recent walk's counters per
    /// `repository_id`, populated after every `bootstrap_workspace` run.
    /// In-memory only — a fresh walk on the next index run replaces it, so
    /// this always reflects the last actually-observed traversal rather than
    /// a stale persisted value.
    last_discovery_counters: Arc<std::sync::RwLock<HashMap<String, attic_discovery::WalkCounters>>>,
    /// Human-readable counterpart to `last_discovery_counters`: the actual
    /// `Diagnostic` messages (submodule boundaries, symlink issues, etc.)
    /// from the most recent walk, per `repository_id`. Same in-memory-only,
    /// replaced-on-reindex lifecycle as `last_discovery_counters`.
    last_discovery_diagnostics:
        Arc<std::sync::RwLock<HashMap<String, Vec<attic_discovery::Diagnostic>>>>,
    /// Maps a configured root's identity key (`root_identity_key`) to the
    /// set of effective repository roots it fanned out into. Most entries
    /// are `[configured_root]` (the common 1:1 case); a container directory
    /// with no top-level `.git` but nested git repos fans out to N entries,
    /// one per nested root discovered by `discover_nested_git_roots`.
    container_repo_roots: Arc<std::sync::RwLock<HashMap<String, Vec<PathBuf>>>>,
    /// Repository ids whose `start_watcher` call failed, with the error
    /// message. Cleared on a subsequent successful `start_watcher` or on
    /// `stop_watcher`. Lets `status` report a real reason for `DISABLED`
    /// instead of silently absorbing the failure into a bare catch-all.
    watcher_start_failures: Arc<std::sync::RwLock<HashMap<String, String>>>,
    /// Central knowledge folder (`attic.toml [knowledge] dir`): indexed and
    /// watched as a hidden repository that is never a workspace member, and
    /// searched separately by every `context` answer.
    knowledge: Arc<std::sync::RwLock<KnowledgeState>>,
}

/// Lifecycle of the central knowledge folder, reported by `status`.
#[derive(Debug, Clone, Default)]
pub(crate) struct KnowledgeState {
    /// Canonical folder in use, when configured and valid.
    dir: Option<PathBuf>,
    /// Hidden repository id once the first index run finished.
    repository_id: Option<String>,
    /// `off` | `indexing` | `ready` | `failed`.
    state: &'static str,
    /// Why the feature is off or failed.
    reason: Option<String>,
}

impl KnowledgeState {
    fn to_json(&self) -> Value {
        json!({
            "state": if self.state.is_empty() { "off" } else { self.state },
            "dir": self.dir.as_ref().map(|d| d.display().to_string()),
            "repository_id": self.repository_id,
            "reason": self.reason,
        })
    }
}

/// Resolve the central knowledge folder to a canonical directory.
/// `Ok(None)` = turned off (`enabled = false`). With no `dir`, the default
/// `default_dir` (`<ATTIC_HOME>/knowledge`) is created if missing. A
/// configured `dir` is never created; a missing or non-directory path is an
/// `Err` with a reason for `status` and never stops the server.
fn resolve_knowledge_dir(
    cfg: &attic_core::KnowledgeConfig,
    default_dir: &Path,
) -> Result<Option<PathBuf>, String> {
    if !cfg.enabled {
        return Ok(None);
    }
    let path = match cfg.dir.as_deref() {
        Some(raw) => PathBuf::from(raw.trim()),
        None => {
            std::fs::create_dir_all(default_dir).map_err(|e| {
                format!(
                    "cannot create default knowledge folder {}: {e}",
                    default_dir.display()
                )
            })?;
            default_dir.to_path_buf()
        }
    };
    let canonical = std::fs::canonicalize(&path)
        .map_err(|e| format!("[knowledge] dir {} is not accessible: {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "[knowledge] dir {} is not a directory",
            path.display()
        ));
    }
    Ok(Some(canonical))
}

impl AtticServer {
    fn new(db_path: &Path) -> Result<Self, ServerError> {
        // `attic.toml` holds resource/indexing tunables, separate from
        // `config.toml` workspace membership.
        // Read BEFORE opening the database so SQLite pragmas can be tuned
        // from the very first connection. An invalid file fails closed with
        // a clear error rather than silently running on partial defaults.
        // An absent file is materialized on disk from ATTIC_TOML_TEMPLATE
        // (falling back to an in-memory AtticConfig::default() if the write
        // itself fails, e.g. a read-only directory) so every install ends up
        // with a real, editable attic.toml instead of an invisible default.
        let attic_toml_path = attic_core::sibling(db_path, "attic.toml");
        let attic_config = if attic_toml_path.exists() {
            let contents = std::fs::read_to_string(&attic_toml_path).map_err(|e| {
                ServerError::InvalidArg(format!(
                    "failed to read '{}': {e}",
                    attic_toml_path.display()
                ))
            })?;
            attic_core::AtticConfig::parse_str(&contents).map_err(|e| {
                ServerError::InvalidArg(format!("invalid '{}': {e}", attic_toml_path.display()))
            })?
        } else {
            if let Err(e) = std::fs::write(&attic_toml_path, attic_core::ATTIC_TOML_TEMPLATE) {
                warn!(
                    "failed to write default '{}': {e}; continuing with in-memory defaults",
                    attic_toml_path.display()
                );
            }
            attic_core::AtticConfig::default()
        };
        if attic_config.removed_embedding_settings_present {
            warn!(
                "legacy embedding settings ignored: embedding support has been removed; remove [semantic], [embedding] and embedding resource overrides from attic.toml"
            );
        }
        // Fail closed at startup on analyzer settings (unknown plugin ids,
        // zero unit ceiling) instead of on the first indexing run.
        IndexOptions::from_config(&attic_config.indexing)
            .validate()
            .map_err(|e| {
                ServerError::InvalidArg(format!("invalid '{}': {e}", attic_toml_path.display()))
            })?;

        // Hardware detection failure never cascades into a crash — it only
        // affects ResourceMode/ResourcePolicy (falls back to Low's
        // conservative settings via `apply_fallback_safety_limits`).
        let snapshot = attic_storage::HardwareSnapshot::capture();
        if let Err(e) = &snapshot {
            warn!(
                "hardware detection failed ({e}); falling back to a conservative resource baseline"
            );
        }
        let env_overrides = attic_storage::env_resource_overrides()
            .map_err(|e| ServerError::InvalidArg(format!("invalid resource configuration: {e}")))?;
        let monitor_overrides = attic_storage::env_monitor_overrides()
            .map_err(|e| ServerError::InvalidArg(format!("invalid resource configuration: {e}")))?;
        let resolution = attic_storage::resolve_effective_config(
            &attic_config.resources,
            &env_overrides,
            &snapshot,
        )
        .map_err(|e| ServerError::InvalidArg(format!("invalid resource configuration: {e}")))?;
        let effective = resolution.effective;
        info!(
            mode = resolution.mode.as_str(),
            mode_source = resolution.mode_source.as_str(),
            scheduler_workers = effective.scheduler_workers,
            memory_budget_mib = effective.memory_budget_mib,
            writer_batch_size = effective.writer_batch_size,
            "resolved Phase 8 resource configuration"
        );

        let (conn, pool) = attic_storage::open_db_with_pragmas(
            db_path,
            effective.sqlite_cache_pages,
            effective.sqlite_mmap_bytes,
        )
        .map_err(ServerError::Storage)?;
        attic_storage::migration::migrate_new_or_compatible_catalog(&conn)?;
        let queue = WriterQueue::new_with_config(
            conn,
            attic_storage::WriterConfig {
                queue_capacity: effective.writer_queue_capacity,
                batch_size: effective.writer_batch_size,
                flush_interval: Duration::from_millis(effective.writer_flush_interval_ms),
                max_io_ops_per_sec: effective.max_io_ops_per_sec,
            },
        )
        .map_err(ServerError::Storage)?;
        let writer = queue.handle();
        let _queue = Arc::new(queue);
        // The resource monitor is driven by the SAME hardware-aware
        // `EffectiveResourceConfig` resolved above (env > attic.toml >
        // detected mode > built-in default, then hardware-clamped).
        let resource_monitor =
            ResourceMonitor::from_config(&effective.as_resource_config(monitor_overrides));
        // Use the same hardware-clamped limit for bootstrap and incremental work.
        resource_monitor.apply_resource_policy(effective.scheduler_workers);
        Ok(AtticServer {
            stores: None,
            repository_lease: None,
            repository_registration_gate: Arc::new(std::sync::Mutex::new(())),
            repository_runtimes: Arc::new(std::sync::RwLock::new(HashMap::new())),
            catalog_generations: Arc::new(std::sync::RwLock::new(HashMap::new())),
            pool,
            writer,
            _queue,
            incremental: Arc::new(std::sync::RwLock::new(HashMap::new())),
            watch_mode: Arc::new(std::sync::RwLock::new(HashMap::new())),
            watches: Arc::new(std::sync::Mutex::new(HashMap::new())),
            bootstrap_jobs: Arc::new(std::sync::Mutex::new(Vec::new())),
            scheduler: Arc::new(std::sync::Mutex::new(None)),
            workspace_configured: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            active_roots: Arc::new(std::sync::RwLock::new(Vec::new())),
            default_config: attic_core::sibling(db_path, "config.toml"),
            unavailable_roots: Arc::new(std::sync::RwLock::new(Vec::new())),
            pending_index_failed: Arc::new(std::sync::Mutex::new(HashMap::new())),
            crossrepo_degraded: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            db_path: db_path.to_path_buf(),
            resource_monitor: Some(Arc::new(resource_monitor)),
            resource_mode: resolution.mode,
            resource_mode_source: resolution.mode_source,
            effective_resources: effective,
            attic_config,
            last_discovery_counters: Arc::new(std::sync::RwLock::new(HashMap::new())),
            last_discovery_diagnostics: Arc::new(std::sync::RwLock::new(HashMap::new())),
            container_repo_roots: Arc::new(std::sync::RwLock::new(HashMap::new())),
            watcher_start_failures: Arc::new(std::sync::RwLock::new(HashMap::new())),
            knowledge: Arc::new(std::sync::RwLock::new(KnowledgeState::default())),
        })
    }

    /// Indexing options for every run this server performs (bootstrap and
    /// incremental), derived once from `attic.toml [indexing]`.
    fn index_options(&self) -> IndexOptions {
        IndexOptions::from_config(&self.attic_config.indexing)
    }

    /// The ONE discovery policy every indexing path uses — bootstrap,
    /// file-watcher filtering, background reconciliation/recompute and the
    /// debug drain — so `attic.toml [indexing].exclude` is honoured
    /// identically everywhere (an excluded path can never re-enter the index
    /// through an edit or a reconciliation pass).
    fn discovery_policy(&self) -> DiscoveryPolicy {
        let mut policy = DiscoveryPolicy::default_git();
        policy.attic_exclude_rules = self
            .attic_config
            .indexing
            .exclude
            .iter()
            .map(|pattern| GlobRule::exclude(pattern.clone()))
            .collect();
        policy
    }

    /// Bootstrap (or reconcile) the repository at `root`.
    ///
    /// Always runs a full authoritative [`index_repository`] pass, even when
    /// a repository row already exists — a repository row proves nothing
    /// about whether its index converged with the current filesystem state
    /// (it may have been created by an interrupted/partial prior run, or the
    /// filesystem may have changed while the service was down). Because
    /// `index_repository` is itself the authoritative reconciliation
    /// (paths gone from disk or newly excluded/unsupported are tombstoned;
    /// unchanged content is simply reproduced), rerunning it is always safe
    /// and is the only way `Ok` here can mean "the index is complete," not
    /// merely "a row exists."
    #[cfg(test)]
    fn bootstrap_workspace(&self, root: &Path) -> Result<String, ServerError> {
        self.bootstrap_workspace_cancellable(root, &attic_core::CancellationToken::default())
    }
    fn bootstrap_workspace_cancellable(
        &self,
        root: &Path,
        cancellation: &attic_core::CancellationToken,
    ) -> Result<String, ServerError> {
        if let Some(manager) = &self.stores {
            let registration = {
                let roots = self
                    .active_roots
                    .read()
                    .map_err(|_| ServerError::Retrieval("membership lock poisoned".into()))?;
                let knowledge = self
                    .knowledge
                    .read()
                    .map_err(|_| ServerError::Retrieval("knowledge lock poisoned".into()))?;
                if !roots
                    .iter()
                    .any(|configured| root == configured || root.starts_with(configured))
                    && knowledge.dir.as_deref() != Some(root)
                {
                    return Err(ServerError::Indexing(IndexError::Cancelled));
                }
                let _registration = self.repository_registration_gate.lock().map_err(|_| {
                    ServerError::Retrieval("repository registration lock poisoned".into())
                })?;
                let canonical_root = root.canonicalize()?;
                let repositories = manager.list_repositories()?;
                let already_active = repositories.iter().any(|repository| {
                    repository.root_path == canonical_root
                        && repository.readiness != attic_storage::RepositoryReadiness::Inactive
                });
                let active_count = repositories
                    .iter()
                    .filter(|repository| {
                        repository.readiness != attic_storage::RepositoryReadiness::Inactive
                    })
                    .count();
                if !already_active && active_count >= MAX_ACTIVE_REPOSITORY_STORES {
                    return Err(ServerError::InvalidArg(format!(
                        "workspace supports at most {MAX_ACTIVE_REPOSITORY_STORES} active repository stores"
                    )));
                }
                manager.register_repository(root)?
            };
            loop {
                if cancellation.is_cancelled() {
                    return Err(ServerError::Indexing(IndexError::Cancelled));
                }
                let roots = self
                    .active_roots
                    .read()
                    .map_err(|_| ServerError::Retrieval("membership lock poisoned".into()))?;
                let knowledge = self
                    .knowledge
                    .read()
                    .map_err(|_| ServerError::Retrieval("knowledge lock poisoned".into()))?;
                if !roots
                    .iter()
                    .any(|configured| root == configured || root.starts_with(configured))
                    && knowledge.dir.as_deref() != Some(root)
                {
                    return Err(ServerError::Indexing(IndexError::Cancelled));
                }
                drop(knowledge);
                drop(roots);
                if manager.ensure_repository_store(&registration.stable_id)?
                    == attic_storage::RepositoryReadiness::Ready
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let local = self.repository_server(&registration.stable_id)?;
            let repository_id = registration
                .stable_id
                .parse::<attic_core::RepositoryId>()
                .map_err(|error| ServerError::Retrieval(error.to_string()))?;
            let stored_root = root.to_string_lossy().to_string();
            let display_name = root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            local.writer.send(move |conn| {
                attic_storage::upsert_repository(
                    conn,
                    &repository_id,
                    &stored_root,
                    &display_name,
                )?;
                Ok(())
            })?;
            if !self
                .repository_runtimes
                .read()
                .map_err(|_| ServerError::Retrieval("repository runtime lock poisoned".into()))?
                .contains_key(&registration.stable_id)
            {
                attic_incremental::run_startup_recovery(&local.pool, &local.writer).map_err(
                    |error| ServerError::Retrieval(format!("repository recovery failed: {error}")),
                )?;
            }
            let id = local.bootstrap_workspace_cancellable(root, cancellation)?;
            if id != registration.stable_id {
                return Err(ServerError::Retrieval(
                    "repository identity does not match store identity".into(),
                ));
            }
            if cancellation.is_cancelled() {
                return Err(ServerError::Indexing(IndexError::Cancelled));
            }
            let roots = self
                .active_roots
                .read()
                .map_err(|_| ServerError::Retrieval("membership lock poisoned".into()))?;
            let knowledge = self
                .knowledge
                .read()
                .map_err(|_| ServerError::Retrieval("knowledge lock poisoned".into()))?;
            if !roots
                .iter()
                .any(|configured| root == configured || root.starts_with(configured))
                && knowledge.dir.as_deref() != Some(root)
            {
                return Err(ServerError::Indexing(IndexError::Cancelled));
            }
            self.project_repository(&local, &id)?;
            return Ok(id);
        }
        let store = IndexingStore {
            readers: &self.pool,
            writer: &self.writer,
        };
        let policy = self.discovery_policy();
        let opts = self.index_options();
        let result = attic_indexing::index_repository_with_cancellation(
            &store,
            root,
            &policy,
            &opts,
            cancellation,
        )
        .map_err(ServerError::Indexing)?;
        // Best-effort: a poisoned lock here must never fail an otherwise
        // successful bootstrap — these counters are diagnostics, not the
        // authoritative index state.
        if let Ok(mut counters) = self.last_discovery_counters.write() {
            counters.insert(result.repository_id.clone(), result.discovery_counters);
        }
        if let Ok(mut diagnostics) = self.last_discovery_diagnostics.write() {
            diagnostics.insert(
                result.repository_id.clone(),
                result.discovery_diagnostics.clone(),
            );
        }
        Ok(result.repository_id)
    }

    /// Discover nested git roots below `configured_root` (or treat it as
    /// one root when it has no git boundaries anywhere) and bootstrap each
    /// effective root independently.
    ///
    /// A container directory with no top-level `.git` whose entire content
    /// lives inside child git repositories previously indexed as a single,
    /// always-empty repository (every file lives under a pruned submodule
    /// boundary — see `walk_pass`). This fans the container out into one
    /// repository per nested `.git` instead.
    ///
    /// [FIX] Parallel fan-out across independent nested repos — different
    /// repos share zero state with each other, making this the safest place
    /// to add concurrency (unlike parallelizing the per-file walk *within*
    /// one repo, which isn't attempted here). Worker count comes from the
    /// same hardware-aware `ResourcePolicy::scheduler_workers` used
    /// elsewhere, not a new hardcoded number. A shared work queue (not a
    /// fixed upfront split) means idle threads automatically pick up more
    /// work — real repos vary wildly in size (e.g. 21 files next to 653),
    /// so a fixed split would leave fast threads idle while one thread is
    /// stuck with all the large repos.
    ///
    /// Behavior change from the old strictly-sequential version, called out
    /// explicitly: previously "all-or-nothing" meant the first failure
    /// aborted every root *after* it in iteration order, even though
    /// sibling repos are fully independent — a failure in repo #3 of 19
    /// silently prevented #4–19 from ever being attempted. Now every root is
    /// attempted regardless of a sibling's failure (each bootstrap is
    /// independent and already idempotent — "rows already committed remain
    /// in the DB and are harmlessly re-indexed on retry" was already true),
    /// and every individual failure is still reported, not swallowed.
    fn bootstrap_workspace_roots_cancellable(
        &self,
        configured_root: &Path,
        cancellation: &attic_core::CancellationToken,
    ) -> Result<Vec<(PathBuf, String)>, ServerError> {
        let nested = attic_discovery::discover_nested_git_roots(configured_root, cancellation)?;
        let effective_roots: Vec<PathBuf> = if nested.is_empty() {
            vec![configured_root.to_path_buf()]
        } else {
            nested
        };

        let worker_count = self
            .effective_resources
            .scheduler_workers
            .clamp(1, effective_roots.len().max(1));
        let work_queue = std::sync::Mutex::new(std::collections::VecDeque::from(effective_roots));
        let results_mutex: std::sync::Mutex<Vec<Result<(PathBuf, String), ServerError>>> =
            std::sync::Mutex::new(Vec::new());

        // Phase 2B: each bootstrap worker must hold an indexing-heavy permit
        // (a background slot from the shared `ResourceMonitor`) for the
        // duration of its `bootstrap_workspace_cancellable` call.  Without
        // this, unbounded numbers of concurrent bootstrap threads could proceed
        // under `Pause`/`Emergency` pressure or beyond the configured
        // background-slot capacity, silently violating the resource model.
        //
        // The borrow is extracted here (before the scope) so the reference has
        // a lifetime that outlives the scope's thread closures — `Arc::as_ref`
        // gives a `&ResourceMonitor` that lives as long as `self`.
        let monitor_ref: Option<&attic_storage::resource_manager::ResourceMonitor> =
            self.resource_monitor.as_deref();

        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                scope.spawn(|| {
                    loop {
                        let root = {
                            let mut q = work_queue.lock().unwrap_or_else(|e| e.into_inner());
                            q.pop_front()
                        };
                        let Some(root) = root else { break };

                        // Acquire an indexing-heavy background permit before
                        // doing any expensive filesystem/DB work.  The blocking
                        // variant spins with sleep until:
                        //   (a) a slot opens AND pressure is below Pause/Emergency
                        //       → returns Some(permit), RAII-released on drop; or
                        //   (b) cancellation fires → returns None → break loop.
                        // When no ResourceMonitor is configured (unit tests)
                        // the permit is skipped entirely.
                        let _permit: Option<attic_storage::IndexingHeavyPermit<'_>> =
                            if let Some(monitor) = monitor_ref {
                                let permit = monitor.acquire_indexing_heavy_blocking(|| {
                                    cancellation.is_cancelled()
                                });
                                if permit.is_none() {
                                    // Cancellation fired while waiting for a slot.
                                    break;
                                }
                                permit
                            } else {
                                None
                            };

                        let outcome = self
                            .bootstrap_workspace_cancellable(&root, cancellation)
                            .map(|id| (root.clone(), id));
                        if matches!(outcome, Err(ServerError::Indexing(IndexError::Cancelled))) {
                            let configured_root_active =
                                self.active_roots.read().ok().is_some_and(|roots| {
                                    roots.iter().any(|r| {
                                        root_identity_key(r) == root_identity_key(configured_root)
                                    })
                                });
                            if !configured_root_active
                                && let Some(repo_id) = self
                                    .pool
                                    .with_reader(|c| {
                                        lookup_repository_by_root_path(c, &root.to_string_lossy())
                                    })
                                    .ok()
                                    .flatten()
                                    .map(|id| id.to_string())
                                && let Err(e) =
                                    eviction::enqueue_eviction(&self.writer, &repo_id, &root)
                            {
                                tracing::warn!(repository_id = %repo_id, root = %root.display(), "could not queue cleanup for cancelled bootstrap residue: {e}");
                            }
                        }
                        // Container roots: publish each nested repository the
                        // moment it is indexed, so it shows in `status` and is
                        // searchable while its siblings are still running
                        // (previously the whole container stayed invisible
                        // until its last repository finished).
                        if let Ok((ref done_root, ref repo_id)) = outcome
                            && done_root.as_path() != configured_root
                            && !cancellation.is_cancelled()
                        {
                            if let Ok(mut g) = self.container_repo_roots.write() {
                                let entry =
                                    g.entry(root_identity_key(configured_root)).or_default();
                                if !entry.contains(done_root) {
                                    entry.push(done_root.clone());
                                }
                            }
                            tracing::info!(
                                root = %done_root.display(),
                                container_root = %configured_root.display(),
                                repository_id = %repo_id,
                                "nested repository indexed"
                            );
                        }
                        results_mutex
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(outcome);
                        // `_permit` drops here, releasing the background slot
                        // before the next iteration's acquisition attempt.
                    }
                });
            }
        });

        let results = results_mutex
            .into_inner()
            .unwrap_or_else(|e| e.into_inner());
        let mut ok_results = Vec::with_capacity(results.len());
        let mut first_err = None;
        for r in results {
            match r {
                Ok(v) => ok_results.push(v),
                Err(e) => {
                    tracing::warn!("nested repo bootstrap failed: {e}");
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        // Every root is attempted regardless of a sibling's failure (see doc
        // comment above); a failure only propagates to the caller when NO
        // root succeeded, so partial successes aren't discarded alongside it.
        if ok_results.is_empty()
            && let Some(e) = first_err
        {
            return Err(e);
        }
        Ok(ok_results)
    }

    /// Runtime control of the persistent file log via `LOG_RELOAD_HANDLE`:
    /// `on` (optionally with `level`), `off`, `level` and `status` take effect
    /// in the already-running process, no restart. `stderr` output is a
    /// separate, always-on layer and is never affected by this.
    fn handle_logging(args: &HashMap<String, Value>) -> Result<CallToolResult, ServerError> {
        let action = args.get("action").and_then(|v| v.as_str()).ok_or_else(|| {
            ServerError::InvalidArg("missing 'action' (on|off|level|status)".into())
        })?;
        let handle = LOG_RELOAD_HANDLE
            .get()
            .ok_or_else(|| ServerError::InvalidArg("log reload handle not initialized".into()))?;
        let parse_level = |required: bool| -> Result<LevelFilter, ServerError> {
            match args.get("level").and_then(|v| v.as_str()) {
                None if !required => Ok(LevelFilter::INFO),
                None => Err(ServerError::InvalidArg(
                    "missing 'level' (error|warn|info|debug|trace)".into(),
                )),
                Some(s) => match level_filter_from_name(s) {
                    Some(l) if l != LevelFilter::OFF => Ok(l),
                    _ => Err(ServerError::InvalidArg(format!(
                        "unknown level '{s}' (expected error|warn|info|debug|trace)"
                    ))),
                },
            }
        };
        let body = match action {
            "on" | "level" => {
                let level = parse_level(action == "level")?;
                handle.modify(|filter| *filter = level).map_err(|e| {
                    ServerError::InvalidArg(format!("failed to set file logging level: {e}"))
                })?;
                format!("file logging: ON (level {level})")
            }
            "off" => {
                handle
                    .modify(|filter| *filter = LevelFilter::OFF)
                    .map_err(|e| {
                        ServerError::InvalidArg(format!("failed to disable file logging: {e}"))
                    })?;
                "file logging: OFF".to_string()
            }
            "status" => {
                let current = handle.with_current(|filter| *filter).map_err(|e| {
                    ServerError::InvalidArg(format!("failed to read file logging state: {e}"))
                })?;
                format!(
                    "file logging: {}",
                    if current == LevelFilter::OFF {
                        "OFF".to_string()
                    } else {
                        format!("ON (level {current})")
                    }
                )
            }
            other => {
                return Err(ServerError::InvalidArg(format!(
                    "unknown action '{other}' (expected on|off|level|status)"
                )));
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    /// Debug/admin tool: claim and execute exactly one pending incremental
    /// task synchronously, bypassing the background scheduler threads —
    /// for on-demand catch-up without waiting on the scheduler's poll
    /// interval. Shares the exact same atomic claim
    /// (`attic_storage::ops_tasks::claim_next_pending_task`) as the
    /// background scheduler, so there is no risk of double-processing a
    /// task racing the background threads.
    ///
    /// This call BLOCKS until the claimed task completes (or returns
    /// immediately with `drained: false` if the queue was empty) — it is
    /// not fire-and-forget. Never call this from a latency-sensitive path.
    fn handle_debug_drain_task(&self) -> Result<CallToolResult, ServerError> {
        let drained = attic_incremental::run_next_task_synchronously(
            &self.pool,
            &self.writer,
            &self.discovery_policy(),
            self.resource_monitor.as_deref(),
        )
        .map_err(|e| ServerError::InvalidArg(format!("task drain failed: {e}")))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::json!({ "drained": drained }).to_string(),
        )]))
    }

    /// Runtime logical-workspace membership management via the `workspace`
    /// MCP tool.
    ///
    /// `inspect` is a pure read. `add`/`remove`/`set` mutate membership,
    /// persist it atomically to the default `<home>/config/config.toml` (so it
    /// survives restarts), and reconcile LIVE watchers: newly added roots are
    /// bootstrapped/indexed and watched immediately, removed roots have their
    /// watcher stopped. This makes first-run, fully-runtime configuration
    /// possible on a pristine machine with no environment variables.
    async fn handle_workspace(
        &self,
        args: &HashMap<String, Value>,
    ) -> Result<CallToolResult, ServerError> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ServerError::InvalidArg("missing 'action' (inspect|add|remove|set)".into())
            })?
            .to_string();

        if action == "inspect" {
            let active = lock_or_server_err!(self.active_roots.read(), "active_roots")?.clone();
            let configured = self
                .workspace_configured
                .load(std::sync::atomic::Ordering::SeqCst);
            // Only surface roots that genuinely fanned out into more than
            // one repository (a container with nested git repos) — the
            // trivial 1:1 case stays implicit, matching every other
            // configured root.
            let root_expansions: HashMap<String, Vec<String>> =
                lock_or_server_err!(self.container_repo_roots.read(), "container_repo_roots")?
                    .iter()
                    .filter(|(_, v)| v.len() != 1)
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            v.iter().map(|p| p.display().to_string()).collect(),
                        )
                    })
                    .collect();
            let payload = json!({
                "configured": configured,
                "unconfigured": !configured,
                "config_file": self.default_config.display().to_string(),
                "membership_count": active.len(),
                "roots": active.iter().map(|r| r.display().to_string()).collect::<Vec<_>>(),
                "root_expansions": root_expansions,
            });
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&payload)?,
            )]));
        }

        // Membership changes do blocking work — lock waits, the atomic
        // config write, SQLite lookups, watcher shutdown and the eviction
        // enqueue (which waits for the single writer). Run all of it on the
        // blocking pool so a slow step can never stall the async runtime
        // that serves every other MCP request, and log each stage so a slow
        // request is diagnosable from the log alone.
        let started = std::time::Instant::now();
        tracing::info!(action = %action, path = ?args.get("path"), "workspace request started");
        let server = self.clone();
        let (args_owned, action_owned) = (args.clone(), action.clone());
        let WorkspaceChange {
            new_active,
            added,
            mut events,
        } = tokio::task::spawn_blocking(move || {
            server.apply_workspace_change(&action_owned, &args_owned)
        })
        .await
        .map_err(|e| ServerError::InvalidArg(format!("workspace update task failed: {e}")))??;
        tracing::info!(
            action = %action,
            elapsed_ms = started.elapsed().as_millis() as u64,
            added = added.len(),
            membership = new_active.len(),
            "workspace membership updated"
        );
        for root in &added {
            let server = self.clone();
            let root = root.clone();
            let root_for_event = root.clone();
            let cancellation = attic_core::CancellationToken::new();
            let worker_cancellation = cancellation.clone();
            let job_key = root_identity_key(&root);

            let handle = tokio::spawn(async move {
                let bootstrap_root = root.clone();
                let bootstrap_server = server.clone();
                let blocking_cancellation = worker_cancellation.clone();
                let job_key = root_identity_key(&root);

                let result = tokio::task::spawn_blocking(move || {
                    bootstrap_server.bootstrap_workspace_roots_cancellable(
                        &bootstrap_root,
                        &blocking_cancellation,
                    )
                })
                .await;

                match result {
                    Ok(Ok(repo_roots)) if !worker_cancellation.is_cancelled() => {
                        // Membership may have changed while indexing was running.
                        let still_active = server.active_roots.read().ok().is_some_and(|roots| {
                            roots
                                .iter()
                                .any(|r| root_identity_key(r) == root_identity_key(&root))
                        });
                        if !still_active {
                            for (effective_root, repo_id) in &repo_roots {
                                if let Err(e) = eviction::enqueue_eviction(
                                    &server.writer,
                                    repo_id,
                                    effective_root,
                                ) {
                                    tracing::warn!(repository_id = %repo_id, root = %effective_root.display(), "could not queue cleanup for obsolete bootstrap result: {e}");
                                }
                            }
                            tracing::info!(root = %root.display(), "bootstrap completed after removal; cleanup re-queued and watcher not started");
                            return;
                        }
                        if let Ok(mut g) = server.pending_index_failed.lock() {
                            g.remove(&root);
                        }
                        if let Ok(mut g) = server.container_repo_roots.write() {
                            g.insert(
                                job_key.clone(),
                                repo_roots.iter().map(|(p, _)| p.clone()).collect(),
                            );
                        }
                        for (effective_root, repo_id) in &repo_roots {
                            let started = server.start_watcher(effective_root, repo_id);
                            tracing::info!(root = %effective_root.display(), container_root = %root.display(), repository_id = %repo_id, watcher_started = started, "background workspace bootstrap completed");
                        }
                    }
                    Ok(Err(ServerError::Indexing(IndexError::Cancelled))) => {
                        tracing::info!(root = %root.display(), "background workspace bootstrap cancelled");
                    }
                    Ok(Err(e)) => {
                        if let Ok(mut g) = server.pending_index_failed.lock() {
                            g.insert(root.clone(), e.to_string());
                        }
                        tracing::warn!(root = %root.display(), "background workspace bootstrap failed: {e}");
                    }
                    Err(e) => {
                        if let Ok(mut g) = server.pending_index_failed.lock() {
                            g.insert(root.clone(), e.to_string());
                        }
                        tracing::warn!(root = %root.display(), "background workspace bootstrap task failed: {e}");
                    }
                    Ok(Ok(repo_roots)) => {
                        let still_active = server.active_roots.read().ok().is_some_and(|roots| {
                            roots
                                .iter()
                                .any(|r| root_identity_key(r) == root_identity_key(&root))
                        });
                        if !still_active {
                            for (effective_root, repo_id) in &repo_roots {
                                if let Err(e) = eviction::enqueue_eviction(
                                    &server.writer,
                                    repo_id,
                                    effective_root,
                                ) {
                                    tracing::warn!(repository_id = %repo_id, root = %effective_root.display(), "could not queue cleanup for obsolete cancelled bootstrap result: {e}");
                                }
                            }
                        }
                        tracing::info!(root = %root.display(), "background workspace bootstrap cancelled before watcher startup");
                    }
                }
            });

            if let Ok(mut jobs) = self.bootstrap_jobs.lock() {
                jobs.push(BootstrapJob {
                    root_key: job_key,
                    cancellation,
                    handle,
                });
            }

            events.push(format!(
                "added root; indexing scheduled in background: {}",
                root_for_event.display()
            ));
        }

        let payload = json!({
            "action": action,
            "configured": !new_active.is_empty(),
            "config_file": self.default_config.display().to_string(),
            "membership_count": new_active.len(),
            "roots": new_active.iter().map(|r| r.display().to_string()).collect::<Vec<_>>(),
            "events": events,
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&payload)?,
        )]))
    }

    /// Synchronous part of a `workspace` add/remove/set: validate, persist,
    /// update live membership, and tear down removed roots. Returns
    /// `(new_active, added, events)`; the caller bootstraps `added`.
    fn apply_workspace_change(
        &self,
        action: &str,
        args: &HashMap<String, Value>,
    ) -> Result<WorkspaceChange, ServerError> {
        let t0 = std::time::Instant::now();
        /// Validate + canonicalize a single root path for membership changes.
        fn validate_root(path: &str) -> Result<PathBuf, ServerError> {
            let p = PathBuf::from(path);
            if !p.exists() {
                return Err(ServerError::InvalidArg(format!(
                    "path does not exist: {path}"
                )));
            }
            if !p.is_dir() {
                return Err(ServerError::InvalidArg(format!(
                    "path is not a directory: {path}"
                )));
            }
            p.canonicalize()
                .map_err(|e| ServerError::InvalidArg(format!("cannot canonicalize '{path}': {e}")))
        }

        /// Deterministic canonical dedup preserving configuration order,
        /// comparing via [`root_identity_key`] so a root reached through a
        /// differently-produced `PathBuf` (see PR-6) is still recognized as
        /// the same root everywhere, not just at the `remove` call site.
        fn dedup_keep_order(roots: Vec<PathBuf>) -> Vec<PathBuf> {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for r in roots {
                if seen.insert(root_identity_key(&r)) {
                    out.push(r);
                }
            }
            out
        }

        // PR-9: serialize the whole compute → persist → commit sequence by
        // holding `active_roots`'s own write lock across it, rather than a
        // separate parallel lock — `active_roots` is already the single
        // source of truth for membership, so it's the natural single point
        // of mutual exclusion for mutating it too. Scoped in an explicit
        // block so the guard is structurally out of scope (not just
        // manually dropped) before the `.await` below — the async-fn Send
        // analysis needs that to prove the guard is never held across it.
        let (new_active, added, removed): (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) = {
            // Bounded wait: a request must fail with a clear, retriable error
            // rather than hang until the client's own timeout fires.
            let lock_started = std::time::Instant::now();
            let mut active_guard = loop {
                match self.active_roots.try_write() {
                    Ok(g) => break g,
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        return Err(ServerError::InvalidArg(
                            "internal lock poisoned: active_roots".into(),
                        ));
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        if lock_started.elapsed() > std::time::Duration::from_secs(10) {
                            return Err(ServerError::InvalidArg(
                                "workspace is busy (another membership change is in progress); retry shortly"
                                    .into(),
                            ));
                        }
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
            };
            tracing::debug!(
                wait_ms = lock_started.elapsed().as_millis() as u64,
                "workspace: membership lock acquired"
            );

            let new_active: Vec<PathBuf> = match action {
                "add" => {
                    let path = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| ServerError::InvalidArg("missing 'path' for add".into()))?;
                    let canon = validate_root(path)?;
                    let mut active = active_guard.clone();
                    if !active
                        .iter()
                        .any(|r| root_identity_key(r) == root_identity_key(&canon))
                    {
                        active.push(canon);
                    }
                    dedup_keep_order(active)
                }
                "remove" => {
                    let path = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
                        ServerError::InvalidArg("missing 'path' for remove".into())
                    })?;
                    let target = PathBuf::from(path);
                    // Two-path strategy (principal-architect audit A-06): a
                    // configured root that has been deleted or moved must still
                    // be removable. `canonicalize()` requires the path to exist,
                    // so fall back to a lexical (filesystem-free) normalization
                    // when it doesn't — comparison then goes through the shared
                    // `root_identity_key` so either form matches the persisted
                    // canonical root.
                    let normalized = if target.exists() {
                        target.canonicalize().map_err(|e| {
                            ServerError::InvalidArg(format!(
                                "cannot canonicalize removal path '{path}': {e}"
                            ))
                        })?
                    } else {
                        normalize_root_lexically(&target).map_err(|e| {
                            ServerError::InvalidArg(format!(
                                "cannot normalize removal path '{path}': {e}"
                            ))
                        })?
                    };
                    let target_key = root_identity_key(&normalized);
                    dedup_keep_order(
                        active_guard
                            .iter()
                            .filter(|r| root_identity_key(r) != target_key)
                            .cloned()
                            .collect(),
                    )
                }
                "set" => {
                    let paths = args
                        .get("paths")
                        .and_then(|v| v.as_array())
                        .ok_or_else(|| {
                            ServerError::InvalidArg("missing 'paths' (array) for set".into())
                        })?
                        .iter()
                        .map(|v| {
                            v.as_str().ok_or_else(|| {
                                ServerError::InvalidArg("paths must be strings".into())
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let mut validated = Vec::new();
                    for p in paths {
                        validated.push(validate_root(p)?);
                    }
                    dedup_keep_order(validated)
                }
                other => return Err(ServerError::InvalidArg(format!("unknown action '{other}'"))),
            };

            // Compute added/removed roots relative to the current live membership.
            let old_active = active_guard.clone();
            let added: Vec<PathBuf> = new_active
                .iter()
                .filter(|r| !old_active.contains(r))
                .cloned()
                .collect();
            let removed: Vec<PathBuf> = old_active
                .iter()
                .filter(|r| !new_active.contains(r))
                .cloned()
                .collect();

            // 1. Persist the new membership atomically BEFORE touching live state,
            //    so a crash still leaves a coherent durable config.
            if new_active.is_empty() {
                remove_workspace_config(&self.default_config).map_err(ServerError::InvalidArg)?;
            } else {
                persist_repositories_config(&self.default_config, &new_active)
                    .map_err(ServerError::InvalidArg)?;
            }

            tracing::debug!(
                elapsed_ms = t0.elapsed().as_millis() as u64,
                "workspace: membership persisted"
            );
            // 2. Update in-memory authoritative membership + configured flag.
            *active_guard = new_active.clone();
            self.workspace_configured
                .store(!new_active.is_empty(), std::sync::atomic::Ordering::SeqCst);
            let writer_roots = new_active.clone();
            self.writer.send(move |conn| {
                attic_storage::repo_eviction::sync_workspace_membership(conn, &writer_roots)
            })?;
            (new_active, added, removed)
            // `active_guard` drops here, going out of scope before the `.await`
            // points below.
        };

        // 3. Reconcile LIVE watchers: stop watchers for removed roots,
        //    start+bootstrap watchers for added roots. Each is isolated so a
        //    single failure never corrupts the rest of the reconciliation.
        let mut events = Vec::new();
        for root in &removed {
            let job_key = root_identity_key(root);
            if let Ok(jobs) = self.bootstrap_jobs.lock() {
                for job in jobs.iter().filter(|job| job.root_key == job_key) {
                    job.cancellation.cancel();
                }
            }
            // A configured root may have fanned out into N effective
            // repository roots (container with nested git repos); look up
            // and clear that mapping so every fanned-out repo's watcher and
            // diagnostics get cleaned up, not just a single lookup on the
            // configured root itself (which is never itself a repo row in
            // the fan-out case).
            let mut effective_roots = self
                .container_repo_roots
                .write()
                .ok()
                .and_then(|mut g| g.remove(&job_key))
                .unwrap_or_else(|| vec![root.clone()]);
            if let Some(manager) = &self.stores {
                for status in manager.list_repositories()? {
                    if status.root_path.starts_with(root)
                        && !effective_roots.contains(&status.root_path)
                    {
                        effective_roots.push(status.root_path);
                    }
                }
            }
            let mut stopped = 0usize;
            for effective_root in &effective_roots {
                if new_active.iter().any(|configured| {
                    effective_root == configured || effective_root.starts_with(configured)
                }) {
                    continue;
                }
                let repo_id = if let Some(manager) = &self.stores {
                    manager
                        .list_repositories()?
                        .into_iter()
                        .find(|status| status.root_path == *effective_root)
                        .map(|status| status.stable_id)
                } else {
                    self.pool
                        .with_reader(|c| {
                            lookup_repository_by_root_path(c, &effective_root.to_string_lossy())
                        })
                        .ok()
                        .flatten()
                        .map(|id| id.to_string())
                };
                if let Some(id) = repo_id {
                    if self.stores.is_some() {
                        self.remove_managed_repository(&id, effective_root)?;
                    }
                    self.stop_watcher(&id);
                    // Catalog provenance cleanup is separate from owned index-file reclamation.
                    if let Err(e) = eviction::enqueue_eviction(&self.writer, &id, effective_root) {
                        tracing::warn!(repository_id = %id, "could not queue data eviction: {e}");
                    }
                    // PR-3 counters are keyed by repository_id; a removed root's
                    // entry would otherwise never be cleaned up, growing this
                    // map unboundedly over a long-running process's lifetime.
                    if let Ok(mut counters) = self.last_discovery_counters.write() {
                        counters.remove(&id);
                    }
                    if let Ok(mut diagnostics) = self.last_discovery_diagnostics.write() {
                        diagnostics.remove(&id);
                    }
                    stopped += 1;
                }
            }
            if stopped > 0 {
                events.push(format!(
                    "stopped {stopped} watcher(s) for: {}",
                    root.display()
                ));
            } else {
                events.push(format!("removed (no registered repo): {}", root.display()));
            }
        }
        tracing::debug!(
            elapsed_ms = t0.elapsed().as_millis() as u64,
            "workspace: removed roots reconciled"
        );
        Ok(WorkspaceChange {
            new_active,
            added,
            events,
        })
    }
    /// Hidden repository id of the central knowledge folder, once indexed.
    fn knowledge_repository_id(&self) -> Option<String> {
        self.knowledge
            .read()
            .ok()
            .and_then(|k| k.repository_id.clone())
    }

    fn set_knowledge(&self, state: KnowledgeState) {
        if let Ok(mut k) = self.knowledge.write() {
            *k = state;
        }
    }

    /// Index and watch the central knowledge folder in the background. Never
    /// blocks or fails startup; the outcome is reported by `status`. The
    /// folder is NOT added to workspace membership.
    fn start_central_knowledge(&self) {
        let default_dir = attic_core::sibling(&self.db_path, "knowledge");
        let dir = match resolve_knowledge_dir(&self.attic_config.knowledge, &default_dir) {
            Ok(Some(dir)) => dir,
            Ok(None) => return,
            Err(reason) => {
                warn!(%reason, "central knowledge folder disabled");
                self.set_knowledge(KnowledgeState {
                    state: "failed",
                    reason: Some(reason),
                    ..KnowledgeState::default()
                });
                return;
            }
        };
        self.set_knowledge(KnowledgeState {
            dir: Some(dir.clone()),
            state: "indexing",
            ..KnowledgeState::default()
        });
        let srv = self.clone();
        let cancellation = attic_core::CancellationToken::new();
        let token = cancellation.clone();
        let handle = tokio::spawn(async move {
            let (job_srv, job_dir, job_token) = (srv.clone(), dir.clone(), token.clone());
            let outcome = tokio::task::spawn_blocking(move || {
                job_srv.bootstrap_workspace_cancellable(&job_dir, &job_token)
            })
            .await;
            if token.is_cancelled() {
                return;
            }
            match outcome {
                Ok(Ok(id)) => {
                    // A folder that is also a workspace root already has that
                    // root's watcher; a second one would replace it.
                    let is_workspace_root = srv.active_roots.read().is_ok_and(|roots| {
                        roots
                            .iter()
                            .any(|r| root_identity_key(r) == root_identity_key(&dir))
                    });
                    if !is_workspace_root {
                        srv.start_watcher(&dir, &id);
                    }
                    info!(repository_id = %id, dir = %dir.display(), "central knowledge folder indexed");
                    srv.set_knowledge(KnowledgeState {
                        dir: Some(dir),
                        repository_id: Some(id),
                        state: "ready",
                        reason: None,
                    });
                }
                Ok(Err(e)) => {
                    error!(dir = %dir.display(), "central knowledge folder indexing failed: {e}");
                    srv.set_knowledge(KnowledgeState {
                        dir: Some(dir),
                        state: "failed",
                        reason: Some(e.to_string()),
                        ..KnowledgeState::default()
                    });
                }
                Err(e) => {
                    error!(dir = %dir.display(), "central knowledge folder task failed: {e}");
                    srv.set_knowledge(KnowledgeState {
                        dir: Some(dir),
                        state: "failed",
                        reason: Some(e.to_string()),
                        ..KnowledgeState::default()
                    });
                }
            }
        });
        if let Ok(mut jobs) = self.bootstrap_jobs.lock() {
            jobs.push(BootstrapJob {
                root_key: "__knowledge__".to_string(),
                cancellation,
                handle,
            });
        }
    }

    /// Stop the live watcher for `repository_id`, if any, and drop its
    /// incremental/watch-mode bookkeeping. Idempotent.
    fn stop_watcher(&self, repository_id: &str) {
        match self.watches.lock() {
            Ok(mut g) => {
                if let Some(mut w) = g.remove(repository_id) {
                    w.stop();
                }
            }
            Err(_) => {
                error!(
                    repository_id,
                    "watches lock poisoned in stop_watcher; skipping watcher cleanup"
                );
            }
        }
        if let Ok(mut g) = self.incremental.write() {
            g.remove(repository_id);
        } else {
            error!(repository_id, "incremental lock poisoned in stop_watcher");
        }
        if let Ok(mut g) = self.watch_mode.write() {
            g.remove(repository_id);
        } else {
            error!(repository_id, "watch_mode lock poisoned in stop_watcher");
        }
        if let Ok(mut g) = self.watcher_start_failures.write() {
            g.remove(repository_id);
        }
        if let Ok(mut runtimes) = self.repository_runtimes.write()
            && let Some(runtime) = runtimes.remove(repository_id)
        {
            runtime.scheduler.shutdown();
        }
    }

    /// Start a watcher for `root` (already bootstrapped/registered as
    /// `repository_id`) and record it in live state. Returns true if a
    /// watcher is now running. Best-effort: a failed watcher start is logged
    /// and the root remains indexed but incrementally-disabled.
    fn start_watcher(&self, root: &Path, repository_id: &str) -> bool {
        if self.stores.is_some() {
            return self.start_repository_runtime(root, repository_id);
        }
        let service = Arc::new(
            attic_incremental::IncrementalService::new(root, self.discovery_policy())
                .with_quiet_period_ms(attic_incremental::DEFAULT_QUIET_MS),
        );
        match service.start_incremental_watch(self.pool.clone(), self.writer.clone()) {
            Ok(watch) => {
                let mode = watch.mode();
                match self.watches.lock() {
                    Ok(mut g) => {
                        g.insert(repository_id.to_string(), watch);
                    }
                    Err(_) => {
                        error!(
                            repository_id = %repository_id,
                            "watches lock poisoned in start_watcher; watcher created but not registered"
                        );
                        return false;
                    }
                }
                match self.watch_mode.write() {
                    Ok(mut g) => {
                        g.insert(repository_id.to_string(), mode);
                    }
                    Err(_) => {
                        error!(
                            repository_id = %repository_id,
                            "watch_mode lock poisoned in start_watcher; degrading"
                        );
                        return false;
                    }
                }
                match self.incremental.write() {
                    Ok(mut g) => {
                        g.insert(repository_id.to_string(), service);
                    }
                    Err(_) => {
                        error!(
                            repository_id = %repository_id,
                            "incremental lock poisoned in start_watcher; degrading"
                        );
                        return false;
                    }
                }
                info!(
                    repository_id = %repository_id,
                    root = %root.display(),
                    mode = mode.as_str(),
                    "runtime-added watcher started"
                );
                if let Ok(mut g) = self.watcher_start_failures.write() {
                    g.remove(repository_id);
                }
                true
            }
            Err(e) => {
                error!(
                    "change detection failed to start for {} ({e}) — incremental DISABLED for this repository",
                    root.display()
                );
                if let Ok(mut g) = self.watcher_start_failures.write() {
                    g.insert(repository_id.to_string(), e.to_string());
                }
                false
            }
        }
    }
}

// ─── input validation ──────────────────────────────────────────────────────────

/// Normalize a path for identity comparison when it (or a suffix of it) no
/// longer exists on disk, so `canonicalize()` cannot run directly.
///
/// Used as the removal-time fallback when the configured root has been
/// deleted or moved (principal-architect audit A-06): a stale membership
/// entry must still be removable by path. The common case is that only the
/// leaf (the removed root itself) is gone while its parent still exists —
/// walk up to the longest still-existing ancestor, canonicalize *that*
/// (recovering Windows short-name/case differences for the part that can
/// still be resolved), then lexically re-append the missing suffix and
/// resolve any remaining `.`/`..` components structurally.
fn normalize_root_lexically(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut missing_tail: Vec<std::ffi::OsString> = Vec::new();
    let mut existing_prefix = absolute.as_path();
    while !existing_prefix.exists() {
        let Some(name) = existing_prefix.file_name() else {
            break; // reached a filesystem root with no existing ancestor
        };
        missing_tail.push(name.to_os_string());
        match existing_prefix.parent() {
            Some(parent) => existing_prefix = parent,
            None => break,
        }
    }

    let mut candidate = if existing_prefix.exists() {
        existing_prefix.canonicalize()?
    } else {
        existing_prefix.to_path_buf()
    };
    for component in missing_tail.into_iter().rev() {
        candidate.push(component);
    }

    // Resolve any remaining `.`/`..` in the (still lexical) missing suffix —
    // `canonicalize()` already normalized the existing prefix above.
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

/// Comparison key for two workspace-root `PathBuf`s that may have been
/// produced differently (one via `canonicalize()`, which resolves the
/// on-disk casing and adds Windows' `\\?\` verbatim prefix; one via
/// [`normalize_root_lexically`], which can do neither without the path
/// existing). Strips the verbatim prefix and case-folds on Windows —
/// mirroring the case-insensitive filename semantics `canonicalize()`
/// already applies implicitly for existing paths — so a root added with one
/// casing/prefix can still be recognized as the same root when removed with
/// another.
fn root_identity_key(path: &Path) -> String {
    let s = path.to_string_lossy();
    let stripped = s.strip_prefix(r"\\?\").unwrap_or(&s);
    if cfg!(windows) {
        stripped.to_lowercase()
    } else {
        stripped.to_string()
    }
}

/// Expand configured roots into their fanned-out repository ids.
///
/// Most configured roots map to exactly one repository (the common 1:1
/// case, when `container_repo_roots` has no entry for the root's identity
/// key). A container root with nested git repositories fans out into
/// N repository ids, one per nested root discovered at `add` time.
///
/// Returns the flattened set of active repository ids, plus a reverse map
/// from each repository id to the identity key of the configured root that
/// owns it (used by `handle_status` to recognize "this repository's
/// configured root is still bootstrapping" instead of guessing).
fn expand_active_ids(
    pool: &DbPool,
    active_roots: &[PathBuf],
    container_repo_roots: &HashMap<String, Vec<PathBuf>>,
) -> (HashSet<String>, HashMap<String, String>) {
    let mut ids = HashSet::new();
    let mut owner = HashMap::new();
    for configured_root in active_roots {
        let key = root_identity_key(configured_root);
        let effective_roots = container_repo_roots
            .get(&key)
            .cloned()
            .unwrap_or_else(|| vec![configured_root.clone()]);
        for root in &effective_roots {
            if let Some(id) = pool
                .with_reader(|c| lookup_repository_by_root_path(c, &root.to_string_lossy()))
                .ok()
                .flatten()
                .map(|id| id.to_string())
            {
                ids.insert(id.clone());
                owner.insert(id, key.clone());
            }
        }
    }
    (ids, owner)
}

/// SCREAMING_SNAKE_CASE label for a discovery diagnostic kind, matching the
/// existing status-string convention (`"RECONCILIATION_REQUIRED"`, etc.).
fn diagnostic_kind_str(kind: &attic_discovery::DiagnosticKind) -> &'static str {
    use attic_discovery::DiagnosticKind::*;
    match kind {
        SymlinkEscape => "SYMLINK_ESCAPE",
        SymlinkCycle => "SYMLINK_CYCLE",
        UnstableCapture => "UNSTABLE_CAPTURE",
        IoError => "IO_ERROR",
        SubmoduleDetected => "SUBMODULE_DETECTED",
        ExemptionRejected => "EXEMPTION_REJECTED",
        InvalidPath => "INVALID_PATH",
    }
}

// ─── tool handlers ─────────────────────────────────────────────────────────────

/// Result of the synchronous part of a `workspace` membership change.
struct WorkspaceChange {
    /// Membership after the change.
    new_active: Vec<PathBuf>,
    /// Roots to bootstrap.
    added: Vec<PathBuf>,
    /// Human-readable events for the response.
    events: Vec<String>,
}
// ─── ServerHandler impl ────────────────────────────────────────────────────────

impl ServerHandler for AtticServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _cx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<ListToolsResult, McpError>> + Send {
        let tools = make_tools();
        async move {
            Ok(ListToolsResult {
                tools,
                ..Default::default()
            })
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _cx: RequestContext<RoleServer>,
    ) -> impl std::future::Future<Output = Result<CallToolResponse, McpError>> + Send {
        let pool = self.pool.clone();
        let writer = self.writer.clone();
        let incremental = self.incremental.clone();
        let watch_mode = self.watch_mode.clone();
        let workspace_configured = self
            .workspace_configured
            .load(std::sync::atomic::Ordering::SeqCst);
        // Clone the Arc so the lock can be acquired inside the async block
        // using lock_or_call_err! — returning a second async move block from
        // the synchronous preamble would create a type mismatch.
        let active_roots_arc = self.active_roots.clone();
        let crossrepo_degraded = self
            .crossrepo_degraded
            .load(std::sync::atomic::Ordering::SeqCst);
        let name = request.name.clone();
        let args: HashMap<String, Value> =
            request.arguments.unwrap_or_default().into_iter().collect();

        async move {
            // Acquire active_roots inside the async block so a poisoned lock
            // returns a structured error via lock_or_call_err! rather than
            // panicking the process or requiring a second async move return
            // type in the synchronous preamble.
            let active_roots = lock_or_call_err!(active_roots_arc.read(), "active_roots").clone();

            // Phase 7: memory-aware foreground admission.  Each MCP tool call
            // must acquire a foreground slot (hard capacity from configuration)
            // before any work happens; when the server is at capacity the
            // caller receives an explicit busy error instead of unbounded
            // queueing.  Real process RSS is refreshed by the admission call,
            // so degradation decisions reflect genuine memory usage.
            let admission = match self
                .resource_monitor
                .as_ref()
                .and_then(|m| m.try_foreground())
            {
                Some(guard) => guard,
                None => {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "server busy: foreground query capacity exhausted ({} concurrent); retry shortly",
                        self.resource_monitor.as_ref().map(|m| m.foreground_capacity()).unwrap_or(0)
                    ))])
                    .into());
                }
            };
            let advisory = admission.advisory();
            // Foreground MCP requests are never refused for memory pressure:
            // pressure throttles background indexing and downgrades
            // optional foreground depth (DEEP→NORMAL via `advisory`), never
            // availability. The only hard refusal above is the foreground
            // concurrency-slot limit — flood protection, not memory policy.
            // Membership-authoritative scope (§14/§16): the set of repository
            // IDs that belong to the CURRENT configured workspace. Query tools
            // use this so historical repositories still present in storage can
            // never leak into active retrieval.
            let active_ids: HashSet<String> = if workspace_configured {
                let container_repo_roots =
                    lock_or_call_err!(self.container_repo_roots.read(), "container_repo_roots");
                expand_active_ids(&pool, &active_roots, &container_repo_roots).0
            } else {
                HashSet::new()
            };
            let args = if workspace_configured
                && matches!(name.as_ref(), "file" | "repo_map" | "context")
            {
                match crate::validate::resolve_repository_selector(&pool, &args, &active_ids) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        return Ok(CallToolResult::error(vec![ContentBlock::text(
                            error.to_string(),
                        )])
                        .into());
                    }
                }
            } else {
                args
            };
            if self.stores.is_some()
                && (workspace_configured || matches!(name.as_ref(), "status" | "debug_drain_task"))
                && matches!(
                    name.as_ref(),
                    "status" | "file" | "repo_map" | "context" | "debug_drain_task"
                )
            {
                let server = self.clone();
                let task_name = name.to_string();
                let tool_args = args.clone();
                let tool_active_ids = active_ids.clone();
                let queued_at = std::time::Instant::now();
                let result = tokio::task::spawn_blocking(move || {
                    let queue_ms = queued_at.elapsed().as_secs_f64() * 1000.0;
                    let started = std::time::Instant::now();
                    let result =
                        server.managed_tool(&task_name, &tool_args, &tool_active_ids, advisory);
                    tracing::info!(
                        tool = %task_name,
                        queue_ms,
                        execution_ms = started.elapsed().as_secs_f64() * 1000.0,
                        success = result.is_ok(),
                        "managed tool timing"
                    );
                    result
                })
                .await;
                return Ok(match result {
                    Ok(Ok(result)) => result.into(),
                    Ok(Err(error)) => {
                        error!("managed tool {name} failed: {error}");
                        CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into()
                    }
                    Err(error) => {
                        error!("managed tool {name} task failed: {error}");
                        CallToolResult::error(vec![ContentBlock::text(format!(
                            "managed tool task failed: {error}"
                        ))])
                        .into()
                    }
                });
            }
            let result: Result<CallToolResult, ServerError> = match name.as_ref() {
                "logging" => Self::handle_logging(&args),
                "debug_drain_task" => self.handle_debug_drain_task(),
                "workspace" => self.handle_workspace(&args).await,
                "file" | "repo_map" | "context" if !workspace_configured => {
                    // UNCONFIGURED first run (§8/§30): query tools that depend on
                    // indexed workspace state must NOT fabricate results. They return
                    // a clear structured error identifying the missing configuration
                    // and the path to fix it (the `workspace` MCP tool). `status` and
                    // `workspace inspect` remain available.
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "workspace not configured: no repository roots are configured yet. \
                         Use the `workspace` tool (action=add or set) to configure the \
                         logical workspace, or start the server with ATTIC_CONFIG / \
                         ATTIC_WORKSPACE_ROOT / a persistent \
                         <ATTIC_HOME>/config/config.toml."
                            .to_string(),
                    )])
                    .into());
                }
                "file" => handle_file(&pool, &args, &active_ids),
                "repo_map" => {
                    let discovery_counters = lock_or_call_err!(
                        self.last_discovery_counters.read(),
                        "last_discovery_counters"
                    );
                    let discovery_diagnostics = lock_or_call_err!(
                        self.last_discovery_diagnostics.read(),
                        "last_discovery_diagnostics"
                    );
                    handle_repo_map(
                        &pool,
                        &args,
                        &active_ids,
                        &discovery_counters,
                        &discovery_diagnostics,
                    )
                }
                "status" => {
                    let inc = lock_or_call_err!(incremental.read(), "incremental");
                    let wm = lock_or_call_err!(watch_mode.read(), "watch_mode");
                    // §23: merge startup unavailable_roots with any in-flight
                    // pending_index_failed entries so status always reflects the
                    // true degraded set without requiring a restart.
                    let base_unavail =
                        lock_or_call_err!(self.unavailable_roots.read(), "unavailable_roots");
                    let failed_guard =
                        lock_or_call_err!(self.pending_index_failed.lock(), "pending_index_failed");
                    let mut combined_unavail: Vec<(PathBuf, String)> =
                        base_unavail.iter().cloned().collect();
                    for (p, reason) in failed_guard.iter() {
                        // Only add if not already present in the base list.
                        if !combined_unavail.iter().any(|(bp, _)| bp == p) {
                            combined_unavail.push((p.clone(), reason.clone()));
                        }
                    }
                    drop(failed_guard);
                    let pending_index_roots =
                        lock_or_call_err!(self.bootstrap_jobs.lock(), "bootstrap_jobs")
                            .iter()
                            .filter(|job| !job.handle.is_finished())
                            .map(|job| job.root_key.clone())
                            .collect::<Vec<_>>();
                    let container_repo_roots =
                        lock_or_call_err!(self.container_repo_roots.read(), "container_repo_roots");
                    let watcher_start_failures = lock_or_call_err!(
                        self.watcher_start_failures.read(),
                        "watcher_start_failures"
                    );
                    handle_status(
                        &pool,
                        &inc,
                        &wm,
                        self.resource_monitor.as_ref().map(|m| m.as_ref()),
                        workspace_configured,
                        &active_roots,
                        &combined_unavail,
                        &pending_index_roots,
                        &container_repo_roots,
                        &watcher_start_failures,
                        &ResourceStatus {
                            resource_mode: self.resource_mode,
                            resource_mode_source: self.resource_mode_source,
                            effective_resources: self.effective_resources,
                            knowledge: self
                                .knowledge
                                .read()
                                .map(|k| k.to_json())
                                .unwrap_or(Value::Null),
                        },
                    )
                }
                "context" => handle_context(
                    &pool,
                    &writer,
                    crossrepo_degraded,
                    &args,
                    &active_ids,
                    advisory,
                    self.knowledge_repository_id(),
                ),
                other => Err(ServerError::InvalidArg(format!("unknown tool: {other}"))),
            };
            drop(admission);
            match result {
                Ok(r) => Ok(r.into()),
                Err(e) => {
                    error!("tool {name} error: {e}");
                    Ok(CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into())
                }
            }
        }
    }
}

// ─── multi-root workspace configuration ───────────────────────────────────────
//
// One Attic process serves ONE logical workspace made of one or more
// independent repository roots. Roots may live anywhere on disk — they are
// never required to share a filesystem parent, be symlinked together, or be
// git submodules. There is intentionally NO symlink-workspace requirement,
// no common-parent requirement, and no per-repo process:
//
// The logical workspace is configured in ONE of these ways (deterministic
// precedence, never silently combined):
//
//   1. `ATTIC_CONFIG=<path>`            explicit multi-root config file
//   2. `<ATTIC_HOME>/config/config.toml` persistent default workspace config
//      (under the resolved application home)
//   3. `ATTIC_WORKSPACE_ROOT=<path>`    single-repository shortcut (not persisted)
//   4. (none of the above)              UNCONFIGURED first run
//
// The persistent default config file (source 2) is what makes the workspace
// durable across restarts and configurable at RUNTIME through the MCP
// `workspace` tool: on a pristine machine, the server starts UNCONFIGURED,
// the operator calls `workspace` to add roots, and the resulting membership
// is written back to `<ATTIC_HOME>/config/config.toml` so it survives the
// next start without any environment variables.
//
// Config-file grammar (shared by `ATTIC_CONFIG` and the default config.toml):
// a flat list of `[[repositories]]` blocks each holding one `path = "..."`.
// Deliberately NOT a general TOML parser (no heavyweight config framework
// dependency for what is, structurally, a list of paths) — see
// [`parse_repositories_config`].
//
// Ambiguity policy: `ATTIC_CONFIG` and `ATTIC_WORKSPACE_ROOT` set together
// are rejected as ambiguous rather than silently preferring one. A
// persistent default config file takes precedence over
// `ATTIC_WORKSPACE_ROOT` (a configured workspace always outranks a mere
// environment hint). `ATTIC_CONFIG` always wins over the default config
// file, since it is the most explicit source.

/// Result of resolving where the workspace configuration comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigSource {
    /// `ATTIC_CONFIG=<path>` — most explicit.
    Explicit(String),
    /// Default persistent `<home>/config/config.toml`.
    Persistent,
    /// `ATTIC_WORKSPACE_ROOT=<path>` single-repository shortcut — not persisted.
    EnvRoot(String),
    /// No configuration present anywhere — UNCONFIGURED first run.
    Unconfigured,
}

/// Read every configured repository root, in precedence order.
///
/// Returns `(source, raw_roots)`. `raw_roots` is the RAW (unvalidated,
/// uncanonicalized) list in configuration order; existence/directory/
/// canonicalization checks and dedup happen later per root in
/// [`validate_configured_roots`], so one bad entry never prevents the others
/// from being reported. `source == Unconfigured` means the workspace is not
/// configured yet — the MCP `workspace` tool remains the entry point.
fn load_workspace_roots(default_config: &Path) -> anyhow::Result<(ConfigSource, Vec<PathBuf>)> {
    let explicit = std::env::var("ATTIC_CONFIG").ok();
    let env_root = std::env::var("ATTIC_WORKSPACE_ROOT").ok();
    if explicit.is_some() && env_root.is_some() {
        anyhow::bail!(
            "ATTIC_CONFIG and ATTIC_WORKSPACE_ROOT are mutually exclusive — set only one \
             (ATTIC_CONFIG for multi-root workspaces, ATTIC_WORKSPACE_ROOT for a single repository)"
        );
    }
    if let Some(path) = explicit {
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("failed to read ATTIC_CONFIG file '{path}': {e}"))?;
        let roots = parse_repositories_config(&contents)
            .map_err(|e| anyhow::anyhow!("invalid ATTIC_CONFIG ('{path}'): {e}"))?;
        return Ok((ConfigSource::Explicit(path), roots));
    }
    if default_config.exists() {
        let contents = std::fs::read_to_string(default_config).map_err(|e| {
            anyhow::anyhow!(
                "failed to read workspace config '{}': {e}",
                default_config.display()
            )
        })?;
        let roots = parse_repositories_config(&contents).map_err(|e| {
            anyhow::anyhow!(
                "invalid workspace config ('{}'): {e}",
                default_config.display()
            )
        })?;
        return Ok((ConfigSource::Persistent, roots));
    }
    if let Some(root) = env_root {
        return Ok((
            ConfigSource::EnvRoot(root.clone()),
            vec![PathBuf::from(root)],
        ));
    }
    Ok((ConfigSource::Unconfigured, Vec::new()))
}

/// Serialize a list of canonicalized repository roots to the shared
/// `[[repositories]] / path = "..."` config-file grammar.
///
/// Round-trips exactly with [`parse_repositories_config`]. Each root is
/// written inside double quotes verbatim (single-backslash Windows paths
/// survive as-is, matching the reader's literal quote handling).
fn serialize_repositories_config(roots: &[PathBuf]) -> String {
    let mut out =
        String::from("# Attic workspace configuration (generated by the `workspace` MCP tool)\n");
    for root in roots {
        out.push_str("[[repositories]]\n");
        out.push_str(&format!("path = \"{}\"\n", root.display()));
    }
    out
}

/// Atomically persist workspace membership to `path` (write temp + rename),
/// so a configured workspace survives process restarts without corruption.
fn persist_repositories_config(path: &Path, roots: &[PathBuf]) -> Result<(), String> {
    let contents = serialize_repositories_config(roots);

    // PR-9 durability hardening: a unique temp filename (PID + monotonic
    // counter) so two overlapping writers (e.g. a crashed prior process
    // whose temp file was never cleaned up) can never collide on the same
    // path; explicit flush + fsync of the temp file's contents before the
    // atomic rename, so a crash right after this call can never observe a
    // renamed-but-not-yet-durable file; best-effort fsync of the parent
    // directory afterward, since on some platforms/filesystems the rename
    // itself is not guaranteed durable until the containing directory is
    // flushed too.
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("config.toml.tmp.{}.{unique}", std::process::id()));

    // Any failure from here on must not leave the temp file behind —
    // repeated fsync/write failures (disk full, AV lock, restricted
    // filesystem) would otherwise accumulate orphaned
    // `config.toml.tmp.<pid>.<n>` files in the config directory forever.
    let write_result: Result<(), String> = (|| {
        let file = std::fs::File::create(&tmp).map_err(|e| {
            format!(
                "failed to create workspace config temp file '{}': {e}",
                tmp.display()
            )
        })?;
        let mut writer = std::io::BufWriter::new(file);
        writer
            .write_all(contents.as_bytes())
            .map_err(|e| format!("failed to write workspace config '{}': {e}", tmp.display()))?;
        writer
            .flush()
            .map_err(|e| format!("failed to flush workspace config '{}': {e}", tmp.display()))?;
        writer
            .into_inner()
            .map_err(|e| format!("failed to flush workspace config '{}': {e}", tmp.display()))?
            .sync_all()
            .map_err(|e| format!("failed to fsync workspace config '{}': {e}", tmp.display()))
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write_result?;

    let rename_result = std::fs::rename(&tmp, path).map_err(|e| {
        format!(
            "failed to finalize workspace config '{}': {e}",
            path.display()
        )
    });
    if rename_result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    rename_result?;

    // Best-effort: not every platform/filesystem supports fsync on a
    // directory handle (notably plain FAT-family filesystems). A failure
    // here must never turn an otherwise-successful, already-durable-file
    // config write into a reported error.
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }

    Ok(())
}

/// Remove an empty workspace config so a workspace reported as configured for
/// zero roots never lingers as an invisible, confusing file.
fn remove_workspace_config(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "failed to remove workspace config '{}': {e}",
            path.display()
        )),
    }
}

/// Parse the minimal `[[repositories]] / path = "..."` configuration
/// grammar. Deliberately NOT a general TOML parser (no heavyweight config
/// framework dependency for what is, structurally, a flat list of paths):
/// blank lines and `#` comments are ignored, `[[repositories]]` opens a
/// block, any other `[...]` line closes it, and each block must contain
/// exactly one `path = "..."` entry. Quoted text is taken literally (no
/// escape processing) so ordinary single-backslash Windows paths work
/// as-is.
fn parse_repositories_config(contents: &str) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();
    let mut in_block = false;
    for (idx, raw_line) in contents.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[repositories]]" {
            in_block = true;
            continue;
        }
        if line.starts_with('[') {
            in_block = false;
            continue;
        }
        if !in_block {
            return Err(format!(
                "line {lineno}: expected a `[[repositories]]` block before `{line}`"
            ));
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!(
                "line {lineno}: expected `path = \"...\"`, got: {line}"
            ));
        };
        if key.trim() != "path" {
            return Err(format!(
                "line {lineno}: unknown key '{}' inside [[repositories]] (only `path` is supported)",
                key.trim()
            ));
        }
        let value = value.trim();
        let unquoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .ok_or_else(|| format!("line {lineno}: path value must be a quoted string"))?;
        if unquoted.is_empty() {
            return Err(format!("line {lineno}: empty path"));
        }
        roots.push(PathBuf::from(unquoted));
        in_block = false; // one `path` per `[[repositories]]` block
    }
    if roots.is_empty() {
        return Err("no [[repositories]] entries with a `path` found".to_string());
    }
    Ok(roots)
}

/// Validate every configured root INDEPENDENTLY (existence, directory-ness,
/// canonicalization) and deterministically drop exact canonical duplicates.
///
/// A root failing validation is skipped (logged) rather than failing the
/// whole workspace — startup configuration for repo B being broken must
/// never prevent repos A and C from being registered, indexed, and served
/// (failure isolation).  Order is preserved so unrelated configuration
/// reordering does not change which duplicate survives.
/// Structured outcome of workspace-root validation (spec §17): valid roots
/// become active membership; configured-but-unavailable roots are PRESERVED
/// (never silently discarded) so status can report them as degraded, and
/// duplicates are reported distinctly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RootValidation {
    /// Canonical roots that are usable now — the active membership set.
    valid: Vec<PathBuf>,
    /// Configured roots that could not be used this run, with the reason.
    unavailable: Vec<(PathBuf, String)>,
    /// Canonical duplicate entries (same canonical path as an earlier one).
    duplicates: Vec<PathBuf>,
}

fn validate_configured_roots(raw_roots: Vec<PathBuf>) -> RootValidation {
    let mut seen = HashSet::new();
    let mut out = RootValidation::default();
    for raw_root in raw_roots {
        if !raw_root.exists() {
            error!(
                "configured repository root does not exist, keeping as UNAVAILABLE: {}",
                raw_root.display()
            );
            out.unavailable
                .push((raw_root, "path does not exist".to_string()));
            continue;
        }
        if !raw_root.is_dir() {
            error!(
                "configured repository root is not a directory, keeping as UNAVAILABLE: {}",
                raw_root.display()
            );
            out.unavailable
                .push((raw_root, "path is not a directory".to_string()));
            continue;
        }
        let canonical = match raw_root.canonicalize() {
            Ok(c) => c,
            Err(e) => {
                error!(
                    "failed to canonicalize configured repository root {}: {e}, keeping as UNAVAILABLE",
                    raw_root.display()
                );
                out.unavailable
                    .push((raw_root, format!("canonicalization failed: {e}")));
                continue;
            }
        };
        if !seen.insert(canonical.clone()) {
            warn!(
                "duplicate configured repository root (same canonical path as an earlier entry), skipping: {}",
                canonical.display()
            );
            out.duplicates.push(canonical);
            continue;
        }
        out.valid.push(canonical);
    }
    out
}

// ─── main ──────────────────────────────────────────────────────────────────────

/// Which role this process holds for the resolved database (see `daemon.rs`
/// for the full election design). Each variant carries whatever guard must
/// be kept alive for the remainder of `main`'s lifetime — the `attic.lock`
/// advisory lock.
#[allow(clippy::large_enum_variant)]
enum Ownership {
    /// This process won the daemon election; `daemon::run_daemon_accept_loop`
    /// serves its own client and any relays.
    Daemon(daemon::DaemonHandle),
    /// This relay won the daemon election during recovery. The process must
    /// start the replacement daemon **and** keep the existing relay alive so
    /// the MCP client's stdin/stdout session is preserved.
    Promoted {
        daemon_handle: daemon::DaemonHandle,
        recovery_state: daemon::RelayRecoveryState,
    },
}

/// Format one panic report exactly once for tracing/stderr.
pub(crate) fn render_panic_diagnostic(
    message: &str,
    location: Option<(&str, u32, u32)>,
    backtrace: Option<&str>,
) -> String {
    let where_ = location
        .map(|(file, line, column)| format!("{file}:{line}:{column}"))
        .unwrap_or_else(|| "unknown location".to_string());
    let mut rendered = format!("panic at {where_}: {message}");
    if let Some(bt) = backtrace.map(str::trim).filter(|bt| !bt.is_empty()) {
        rendered.push_str("\nbacktrace:\n");
        rendered.push_str(bt);
    }
    rendered
}

pub(crate) fn emit_panic_diagnostic_with<E, C>(
    message: &str,
    location: Option<(&str, u32, u32)>,
    backtrace: Option<&str>,
    emit: E,
    chain: C,
) where
    E: FnOnce(&str),
    C: FnOnce(),
{
    let rendered = render_panic_diagnostic(message, location, backtrace);
    emit(&rendered);
    chain();
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic payload is not a string".to_string());
        let location = info
            .location()
            .map(|loc| (loc.file(), loc.line(), loc.column()));
        let backtrace = std::backtrace::Backtrace::capture();
        let backtrace_text = matches!(
            backtrace.status(),
            std::backtrace::BacktraceStatus::Captured
        )
        .then(|| backtrace.to_string());
        emit_panic_diagnostic_with(
            &message,
            location,
            backtrace_text.as_deref(),
            |rendered| {
                if tracing::dispatcher::has_been_set() {
                    tracing::error!(target: "attic::panic", "{rendered}");
                } else {
                    eprintln!("{rendered}");
                }
            },
            || default_hook(info),
        );
    }));
}

/// Real process entry point. Deliberately NOT `#[tokio::main] async fn main()`:
/// that expansion builds a `Runtime`, runs `run()` on it, then drops the
/// `Runtime` before the process exits — and dropping a multi-threaded tokio
/// runtime BLOCKS until every outstanding task finishes, including tasks
/// parked in tokio's blocking-I/O thread pool (e.g. an in-flight
/// `tokio::io::stdin()` read whose Rust-level future was cancelled but whose
/// underlying OS read syscall is still parked waiting for input that will
/// never arrive because the writer end is still open). A relay that hits its
/// bounded recovery timeout and returns `Err` from `run()` would log the
/// error correctly and then hang indefinitely in that runtime-drop, because
/// nothing ever forces the process to actually exit — confirmed directly via
/// `recovery_is_bounded_when_daemon_cannot_start` (§17 item 10): the fatal
/// error logged at ~40s, but the OS process was still alive at 90s.
/// `std::process::exit()` sidesteps this entirely: it terminates the process
/// immediately, running no destructors at all, so the runtime's blocking drop
/// (and whatever it might be stuck waiting on) never gets a chance to run.
fn main() {
    install_panic_hook();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build the tokio runtime");
    let result = rt.block_on(run());
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("Error: {e:#}");
            std::process::exit(1);
        }
    }
}

async fn run() -> anyhow::Result<()> {
    // Phase 7: platform-appropriate data/cache/temp policy (see
    // attic_core::paths).  The data root is user-global (OS application-data
    // directory); workspaces are never written to.
    let paths = attic_core::AtticPaths::resolve()?;
    let db_path = paths.db_path();

    // Verbosity: `ATTIC_LOG`, else `RUST_LOG` (EnvFilter syntax), else `info`.
    let env_filter = tracing_subscriber::EnvFilter::try_from_env("ATTIC_LOG")
        .or_else(|_| tracing_subscriber::EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // Persistent file log, colocated with the database through the resolved
    // Attic paths, never through an independently inferred home directory. Kept
    // entirely separate from `stderr` (which stays on unconditionally) so
    // this is purely additive — nothing that already works changes.
    //
    // Wrapped in a `reload` handle so file logging can be switched on/off at
    // runtime via the `logging` MCP tool, without restarting the process —
    // an env var would only take effect on the next restart, which isn't a
    // real "instant kill switch." The writer initializes the rolling file
    // appender lazily, so the `logs/` directory is not created while file
    // logging is OFF.
    let file_log_writer = LazyFileLogWriter::new(attic_core::sibling(db_path, "logs"));
    // `[logging] file_level` in attic.toml makes the level survive restarts
    // (previously every launch started with the file log OFF, so a session
    // could not be diagnosed afterwards unless someone re-enabled it first).
    let (file_level, log_reload_handle) =
        tracing_subscriber::reload::Layer::new(configured_file_log_level(db_path));
    LOG_RELOAD_HANDLE
        .set(log_reload_handle)
        .map_err(|_| anyhow::anyhow!("log reload handle already initialized"))?;

    // The reload-wrapped file layer MUST be the first `.with()` call: its
    // type is pinned to `Layer<Registry>` by `LOG_RELOAD_HANDLE`'s static
    // type (`reload::Handle<LevelFilter, Registry>`), which only lines up
    // when it sits directly on bare `Registry`, not on an already-`Layered`
    // stack. The stderr layer goes second, where ordinary type inference
    // (not pinned to any static) adapts to whatever subscriber it's
    // actually joining.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_log_writer)
                .with_ansi(false)
                .with_filter(file_level),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(env_filter),
        )
        .init();

    // Multi-window concurrency: only the first `attic` launch for a given
    // database becomes the daemon — it alone owns the SQLite writer, the
    // filesystem watchers, and startup recovery. Every later launch for the
    // same database becomes a thin relay that splices its own stdin/stdout to
    // the daemon's local socket, so multiple windows on the same project run
    // concurrently against one shared live state. See `daemon.rs` for the
    // election/relay/accept-loop implementation.
    //
    // Looped inside `run_relay_supervised`: a relay whose daemon disappears
    // (crash, restart, config-change relaunch) retries election instead of
    // exiting, and if it wins it falls through to the same daemon path as a
    // launch that won on its first try.
    let ownership: Ownership = match daemon::elect(db_path).await? {
        daemon::ElectionResult::Relay(relay) => {
            info!(
                "attic relay: another instance already owns database '{}'; \
                 splicing stdio to its daemon (supervised recovery enabled)",
                db_path.display()
            );
            let db_path_buf = db_path.to_path_buf();
            let paths_clone = paths.clone();
            let daemon_starter: daemon::DaemonStarter = Arc::new(move |daemon_handle, ready_tx| {
                let srv = startup::StartupServer::start(&db_path_buf, &paths_clone);
                Ok(daemon::spawn_daemon(srv, daemon_handle, ready_tx))
            });

            match daemon::run_relay_supervised(relay, db_path, Some(daemon_starter)).await {
                daemon::RelaySupervisionOutcome::ClientClosed => {
                    return Ok(());
                }
                daemon::RelaySupervisionOutcome::PromoteToDaemon {
                    daemon_handle,
                    recovery_state,
                } => Ownership::Promoted {
                    daemon_handle,
                    recovery_state,
                },
                daemon::RelaySupervisionOutcome::Fatal { error } => {
                    error!("relay supervision failed: {error:#}");
                    return Err(error);
                }
            }
        }
        daemon::ElectionResult::Daemon(handle) => Ownership::Daemon(handle),
    };

    info!(
        "attic starting, db={} (home {})",
        db_path.display(),
        paths.home.display()
    );
    if db_path.parent() != Some(paths.home.as_path())
        && db_path.parent() != Some(paths.home.join("data").as_path())
    {
        tracing::warn!(
            "ATTIC_HOME ({}) and ATTIC_DB_PATH's directory ({}) disagree; config and runtime files \
             will be colocated with the database at {}, not under ATTIC_HOME",
            paths.home.display(),
            db_path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            db_path
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );
    }

    match ownership {
        Ownership::Daemon(handle) => {
            let server = startup::StartupServer::start(db_path, &paths);
            daemon::run_daemon_accept_loop(server, handle, true).await
        }
        Ownership::Promoted {
            daemon_handle,
            recovery_state,
        } => {
            let server = startup::StartupServer::start(db_path, &paths);
            let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

            info!(
                "relay promotion: spawning replacement daemon + resuming relay \
                 (existing MCP client stays connected)"
            );

            let daemon_task = daemon::spawn_daemon(server, daemon_handle, ready_tx);

            if ready_rx.await.is_err() {
                error!("relay promotion: daemon task failed before signaling readiness");
                match daemon_task.await {
                    Ok(Err(e)) => return Err(e.context("daemon failed during promotion")),
                    Err(e) => return Err(anyhow::anyhow!("daemon task panicked: {e}")),
                    Ok(Ok(())) => {
                        return Err(anyhow::anyhow!("daemon exited without signaling readiness"));
                    }
                }
            }

            let db_path_buf = db_path.to_path_buf();
            let paths_clone = paths.clone();
            let daemon_starter: daemon::DaemonStarter = Arc::new(move |daemon_handle, ready_tx| {
                let srv = startup::StartupServer::start(&db_path_buf, &paths_clone);
                Ok(daemon::spawn_daemon(srv, daemon_handle, ready_tx))
            });

            let relay_result = daemon::resume_relay_after_promotion(
                db_path,
                recovery_state,
                Some(daemon_starter),
                Some(daemon::OwnedDaemon { task: daemon_task }),
            )
            .await;

            if let Err(ref e) = relay_result {
                warn!("relay promotion: relay ended with error: {e:#}");
            }

            relay_result
        }
    }
}

/// Initializes the verified catalog and launches repository-local bootstrap.
/// Protocol handling remains available through `StartupServer` during this work.
pub(crate) fn build_server_and_enricher(
    db_path: &std::path::Path,
    paths: &attic_core::AtticPaths,
) -> anyhow::Result<AtticServer> {
    let mut server = AtticServer::new(db_path)?;
    let has_legacy_payload = server.pool.with_reader(|conn| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM core_retrieval_units LIMIT 1)",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(Into::into)
    })?;
    if has_legacy_payload {
        anyhow::bail!(
            "configured catalog contains a legacy monolithic index; preserved unchanged. Choose a fresh catalog path and explicitly reindex repositories"
        );
    }

    // Verify database integrity and foreign key consistency
    let (verify_conn, _verify_pool) = attic_storage::open_db(db_path)
        .map_err(|e| anyhow::anyhow!("failed to open verification connection: {e}"))?;
    let integrity_violations = attic_storage::connection::verify_connection(&verify_conn)?;
    drop(verify_conn);
    if !integrity_violations.is_empty() {
        for v in &integrity_violations {
            error!("database integrity violation during startup: {v}");
        }
        return Err(anyhow::anyhow!(
            "database integrity check failed during startup"
        ));
    }
    attic_incremental::run_startup_recovery(&server.pool, &server.writer)
        .map_err(|error| anyhow::anyhow!("catalog recovery failed: {error}"))?;
    server.stores = Some(Arc::new(
        attic_storage::RepositoryStoreManager::open_with_catalog_writer(
            db_path,
            db_path.parent().unwrap_or(Path::new(".")),
            MAX_ACTIVE_REPOSITORY_STORES,
            server.writer.clone(),
        )?,
    ));

    // Multi-root workspace bootstrap
    let default_config = paths.config_file.clone();
    let (config_source, raw_roots) = load_workspace_roots(&default_config)?;
    let validation = validate_configured_roots(raw_roots);
    let roots = validation.valid.clone();
    match server.unavailable_roots.write() {
        Ok(mut g) => *g = validation.unavailable.clone(),
        Err(_) => return Err(anyhow::anyhow!("startup lock poisoned: unavailable_roots")),
    }
    match server.active_roots.write() {
        Ok(mut g) => *g = roots.clone(),
        Err(_) => return Err(anyhow::anyhow!("startup lock poisoned: active_roots")),
    }
    server.workspace_configured.store(
        config_source != ConfigSource::Unconfigured,
        std::sync::atomic::Ordering::SeqCst,
    );
    let startup_writer_roots = roots.clone();
    server
        .writer
        .send(move |conn| {
            attic_storage::repo_eviction::sync_workspace_membership(conn, &startup_writer_roots)
        })
        .map_err(|e| anyhow::anyhow!("startup workspace membership sync failed: {e}"))?;
    info!(
        configured = config_source != ConfigSource::Unconfigured,
        source = ?config_source,
        root_count = roots.len(),
        "workspace configuration resolved"
    );
    for root in &roots {
        info!(root = %root.display(), source = ?config_source, "startup workspace root");
    }

    // Repository-removal data eviction (STALE_EVICTION tasks). Started only
    // now — after recovery, the integrity check and with `active_roots`
    // populated — so a leftover task for a root that is configured again is
    // recognised as re-added and cancelled instead of deleting live data.
    let (cancellation, handle) = eviction::spawn_evictor(
        server.pool.clone(),
        server.writer.clone(),
        server.active_roots.clone(),
    )?;
    server
        .bootstrap_jobs
        .lock()
        .map_err(|_| anyhow::anyhow!("bootstrap job lock poisoned"))?
        .push(BootstrapJob {
            root_key: "__evictor__".into(),
            cancellation,
            handle,
        });

    // Central knowledge folder: independent of workspace roots, so it is
    // indexed even when no repository is configured yet.
    server.start_central_knowledge();
    server.start_catalog_refresh();

    if !roots.is_empty() {
        let startup_server = server.clone();
        let startup_roots = roots.clone();
        let cancellation = attic_core::CancellationToken::new();
        let worker_cancellation = cancellation.clone();

        let handle = tokio::spawn(async move {
            let mut bootstrapped: Vec<(PathBuf, String)> = Vec::new();

            for root in startup_roots {
                if worker_cancellation.is_cancelled() {
                    return;
                }
                let srv = startup_server.clone();
                let root_for_bootstrap = root.clone();
                let token = worker_cancellation.clone();
                let job_key = root_identity_key(&root);
                match tokio::task::spawn_blocking(move || {
                    srv.bootstrap_workspace_roots_cancellable(&root_for_bootstrap, &token)
                })
                .await
                {
                    Ok(Ok(repo_roots)) if !worker_cancellation.is_cancelled() => {
                        if let Ok(mut g) = startup_server.container_repo_roots.write() {
                            g.insert(job_key, repo_roots.iter().map(|(p, _)| p.clone()).collect());
                        }
                        for (effective_root, repository_id) in &repo_roots {
                            info!(repository_id = %repository_id, root = %effective_root.display(), container_root = %root.display(), "background startup repository bootstrap complete");
                            startup_server.start_watcher(effective_root, repository_id);
                        }
                        bootstrapped.extend(repo_roots);
                    }
                    Ok(Err(ServerError::Indexing(IndexError::Cancelled))) => return,
                    Ok(Err(e)) => {
                        error!(root = %root.display(), "background startup repository bootstrap failed: {e}");
                        if let Ok(mut failed) = startup_server.pending_index_failed.lock() {
                            failed.insert(root, e.to_string());
                        }
                    }
                    Err(e) => {
                        error!(root = %root.display(), "background startup repository task failed: {e}");
                        if let Ok(mut failed) = startup_server.pending_index_failed.lock() {
                            failed.insert(root, e.to_string());
                        }
                    }
                    Ok(Ok(_)) => return,
                }
            }

            if worker_cancellation.is_cancelled() || bootstrapped.is_empty() {
                return;
            }
            if startup_server.stores.is_some() {
                return;
            }

            // Cross-repository sync only after all successful bootstraps,
            // scoped to the configured membership: repositories left in
            // storage by an earlier configuration never contribute edges.
            let writer = startup_server.writer.clone();
            let pool = startup_server.pool.clone();
            let active_repository_ids = Some(
                bootstrapped
                    .iter()
                    .map(|(_, id)| id.clone())
                    .collect::<Vec<_>>(),
            );
            match tokio::task::spawn_blocking(move || {
                let opts = attic_crossrepo::maintenance::WorkspaceSyncOptions {
                    active_repository_ids,
                    ..Default::default()
                };
                pool.with_reader(|conn| {
                    attic_crossrepo::maintenance::sync_workspace(conn, &writer, &opts)
                        .map_err(|e| StorageError::Worker(e.to_string()))
                })
                .map_err(|e| StorageError::Worker(e.to_string()))
            })
            .await
            {
                Ok(Ok(result)) => {
                    info!(
                        repos = result.repository_reports.len(),
                        edges = result.edges_emitted,
                        "background cross-repo workspace sync complete"
                    );
                    if !result.diagnostics.is_empty() {
                        warn!(
                            missing = result.diagnostics.missing_targets.len(),
                            ambiguous = result.diagnostics.ambiguous_targets.len(),
                            skipped_repositories = result.diagnostics.skipped_repositories,
                            missing_targets = ?result.diagnostics.missing_targets,
                            ambiguous_targets = ?result.diagnostics.ambiguous_targets,
                            "background cross-repo workspace sync had unresolved targets"
                        );
                    }
                    startup_server
                        .crossrepo_degraded
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(Err(e)) => warn!("background cross-repo workspace sync failed: {e}"),
                Err(e) => warn!("background cross-repo workspace task failed: {e}"),
            }

            if worker_cancellation.is_cancelled() {
                return;
            }

            if startup_server.stores.is_some() {
                return;
            }
            // Schedule offline refresh after authoritative startup bootstrap.
            match attic_incremental::plan_offline_refresh(&startup_server.pool) {
                Ok(batch) => {
                    for refresh in batch {
                        if worker_cancellation.is_cancelled() {
                            return;
                        }
                        let payload = attic_storage::IncrementalTaskPayload {
                            dedup_key: format!(
                                "offline-{}",
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_micros())
                                    .unwrap_or_default()
                            ),
                            upserts: refresh.upsert_paths,
                            deletes: vec![],
                            renames: vec![],
                            from_reconciliation: true,
                        };
                        if let Err(e) = attic_incremental::scheduler::schedule_incremental(
                            &startup_server.writer,
                            &refresh.repository_id,
                            &payload,
                            attic_incremental::scheduler::PRIORITY_RECONCILE,
                            4096,
                            startup_server.resource_monitor.as_ref().map(|m| m.as_ref()),
                        ) {
                            warn!("offline refresh scheduling failed: {e}");
                        }
                    }
                }
                Err(e) => warn!("offline refresh planning failed: {e}"),
            }

            if worker_cancellation.is_cancelled() {
                return;
            }

            // Start the one shared scheduler after bootstrap; store it on the
            // server so shutdown can always stop it even though startup is async.
            // Every task carries its own repository, so the scheduler resolves
            // each task's root from storage.
            match attic_incremental::spawn_scheduler(
                attic_incremental::SchedulerConfig {
                    workers: startup_server.effective_resources.scheduler_workers,
                    index_options: startup_server.index_options(),
                    ..attic_incremental::SchedulerConfig::default()
                },
                startup_server.pool.clone(),
                startup_server.writer.clone(),
                startup_server.discovery_policy(),
                startup_server.resource_monitor.clone(),
            ) {
                Ok(sched) => {
                    if let Ok(mut slot) = startup_server.scheduler.lock() {
                        *slot = Some(sched);
                    }
                }
                Err(e) => error!("scheduler startup failed - incremental scheduler disabled: {e}"),
            }
        });

        if let Ok(mut jobs) = server.bootstrap_jobs.lock() {
            jobs.push(BootstrapJob {
                root_key: "__startup__".to_string(),
                cancellation,
                handle,
            });
        }
    }
    Ok(server)
}

/// Handles needed to run the teardown sequence ([`run_shutdown_sequence`])
/// once the daemon stops accepting new MCP work (idle timeout or SIGINT).
/// Captured from an `AtticServer` before it is consumed by `.serve(...)`,
/// since `serve` takes the transport-bound clone by value.
pub(crate) struct ShutdownHandles {
    repository_runtimes: workspace_stores::RuntimeMap,
    writer: WriterQueueHandle,
    db_path: PathBuf,
    watches: Arc<std::sync::Mutex<HashMap<String, attic_incremental::IncrementalWatch>>>,
    bootstrap_jobs: Arc<std::sync::Mutex<Vec<BootstrapJob>>>,
    scheduler: Arc<std::sync::Mutex<Option<attic_incremental::SchedulerHandle>>>,
    /// Phase 3: periodic RSS sampler cancellation + join handle.
    /// `None` when `resource_monitor` is absent (tests).
    rss_sampler: Option<(attic_core::CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl ShutdownHandles {
    pub(crate) fn capture(server: &AtticServer) -> Self {
        Self {
            repository_runtimes: server.repository_runtimes.clone(),
            writer: server.writer.clone(),
            db_path: server.db_path.clone(),
            watches: server.watches.clone(),
            bootstrap_jobs: server.bootstrap_jobs.clone(),
            scheduler: server.scheduler.clone(),
            rss_sampler: None,
        }
    }
}

/// Deterministic teardown, run exactly once per process lifetime by the
/// daemon accept loop (`daemon::run_daemon_accept_loop`) after it stops
/// accepting MCP work (idle timeout or Ctrl+C/SIGINT):
///
///   1. Cancel and join bootstrap jobs.
///   2. Watcher shutdown, then scheduler shutdown (bounded joins).
///   3. Record per-repository and catalog clean-shutdown markers.
///   4. Checkpoint the catalog WAL and prune expired operational records.
///   5. Release writer handles.
pub(crate) async fn run_shutdown_sequence(handles: ShutdownHandles, reason: &str) {
    let ShutdownHandles {
        repository_runtimes,
        writer,
        db_path,
        watches,
        bootstrap_jobs,
        scheduler,
        rss_sampler,
    } = handles;

    // Phase 3: cancel and join the periodic RSS sampler before bootstrap
    // cancel — ensures the hysteresis tier reflects current pressure right
    // up to the start of teardown, then stops updating it.
    if let Some((cancel, handle)) = rss_sampler {
        cancel.cancel();
        let _ = handle.await;
    }

    // 2. Cancel and JOIN every server-owned bootstrap before touching watchers,
    // scheduler, WAL or DB resources. spawn_blocking cannot be force-aborted once
    // running, so the indexing pipeline cooperatively observes these tokens.
    let jobs = {
        let mut guard = bootstrap_jobs.lock().unwrap_or_else(|e| e.into_inner());
        for job in guard.iter() {
            job.cancellation.cancel();
        }
        std::mem::take(&mut *guard)
    };
    for job in jobs {
        if let Err(e) = job.handle.await {
            warn!("background bootstrap join failed during shutdown: {e}");
        }
    }

    // 3. Watcher shutdown, then scheduler shutdown.  The watcher only
    //    detects changes; the scheduler drains work derived from those
    //    changes, so stopping detection first bounds how much new work the
    //    scheduler can still be asked to do. Both joins are bounded (worker
    //    threads poll a stop flag / condvar, not indefinite blocking I/O).
    {
        let mut watches_guard = watches.lock().unwrap_or_else(|e| e.into_inner());

        for watch in watches_guard.values_mut() {
            watch.stop();
        }
    } // MutexGuard is definitely dropped here

    let sched = scheduler.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(sched) = sched {
        sched.shutdown();
    }
    let runtimes = {
        let mut runtimes = repository_runtimes
            .write()
            .unwrap_or_else(|error| error.into_inner());
        std::mem::take(&mut *runtimes)
    };
    for (id, runtime) in runtimes {
        runtime.scheduler.shutdown();
        if let Err(error) =
            attic_incremental::record_clean_shutdown_marker(&runtime.lease.writer_handle())
        {
            warn!(
                repository_id = id,
                "failed to record repository shutdown: {error}"
            );
        }
    }

    // 4. Record clean shutdown marker (durable task state, REC-INV-1).
    if let Err(error) = attic_incremental::record_clean_shutdown_marker(&writer) {
        warn!("catalog clean-shutdown marker failed: {error}");
    }

    // Checkpointing failures retain recoverable WAL content.
    {
        let db_path = db_path.clone();
        let maintenance = tokio::task::spawn_blocking(move || {
            let (conn, _pool) = match attic_storage::open_db(&db_path) {
                Ok(x) => x,
                Err(e) => {
                    warn!("shutdown maintenance open failed: {e}");
                    return;
                }
            };
            match attic_storage::connection::checkpoint_wal(&conn) {
                Ok((busy, log, ckpt)) => {
                    info!(
                        "shutdown WAL checkpoint: busy={busy} log_pages={log} checkpointed={ckpt}"
                    );
                }
                Err(e) => warn!("shutdown WAL checkpoint failed: {e}"),
            }

            // 5b. Retention pruning (best-effort): drop old file-occurrence
            // tombstones, old invalidation audit records, and old terminal
            // task rows before vacuuming, so VACUUM has real space to
            // reclaim. Never fails shutdown — each is independently logged.
            match attic_storage::invalidation_ops::prune_old_tombstones(
                &conn,
                attic_storage::invalidation_ops::DEFAULT_TOMBSTONE_RETENTION_DAYS,
            ) {
                Ok(n) => {
                    if n > 0 {
                        info!("shutdown maintenance: pruned {n} old file-occurrence tombstones");
                    }
                }
                Err(e) => warn!("shutdown tombstone pruning failed (best-effort): {e}"),
            }
            match attic_storage::invalidation_ops::prune_old_invalidation_records(
                &conn,
                attic_storage::invalidation_ops::DEFAULT_INVALIDATION_RECORD_RETENTION_DAYS,
            ) {
                Ok(n) => {
                    if n > 0 {
                        info!("shutdown maintenance: pruned {n} old invalidation records");
                    }
                }
                Err(e) => warn!("shutdown invalidation-record pruning failed (best-effort): {e}"),
            }
            match attic_storage::ops_tasks::prune_terminal_tasks(
                &conn,
                attic_storage::ops_tasks::DEFAULT_TERMINAL_TASK_RETENTION_DAYS,
            ) {
                Ok(n) => {
                    if n > 0 {
                        info!("shutdown maintenance: pruned {n} old terminal task rows");
                    }
                }
                Err(e) => warn!("shutdown terminal-task pruning failed (best-effort): {e}"),
            }
        })
        .await;
        if let Err(e) = maintenance {
            warn!("shutdown maintenance task failed: {e}");
        }
    }

    // 6. Stop workers.  Drop the WriterQueue - this signals the worker thread
    //    to shut down and joins it deterministically. By this point `server`
    //    (and its `Arc<WriterQueue>`) has already been fully dropped (either
    //    inside `running.waiting()` for the stdio path, or once every
    //    accepted connection's `RunningService` finished for the daemon
    //    path), so this drops the last outstanding handle clone.
    drop(writer);

    // 7. Close DB resources (pool + writer connection) via Drop.
    // 8. Exit (return to the caller, which returns `Ok(())` to the runtime).

    info!("attic server shut down cleanly: {reason}");
}

// ─── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests;
