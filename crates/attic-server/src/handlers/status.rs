use crate::*;

/// `status` reports the WHOLE workspace, not one repository (§20): a
/// multi-root workspace with one healthy repository and two degraded ones
/// must never be reported as uniformly "ok". `incremental`/`watch_mode` are
/// keyed by `repository_id`, one entry per repository this process is
/// actively watching. A repository known to storage but absent from these
/// maps is reported as `DISABLED` UNLESS its configured root is still
/// bootstrapping (`pending_index_roots`, via `container_repo_roots`'s
/// root→repository-id ownership), in which case it is `INDEXING`. When it
/// genuinely is disabled, the reason is `watcher_start_failures`'s recorded
/// error when available, else an honest `"watcher_not_registered"` label —
/// never a bare unreasoned catch-all. A configured root may also fan out
/// into more than one repository id (a container with nested git repos —
/// see `container_repo_roots`), so this is a container→N-repository
/// relationship, not 1:1.
/// Phase 8 additions bundled into one struct rather than further extending
/// `handle_status`'s already-long parameter list.
pub(crate) struct ResourceStatus {
    pub(crate) resource_mode: attic_storage::ResourceMode,
    pub(crate) resource_mode_source: attic_storage::ResourceModeSource,
    pub(crate) effective_resources: attic_storage::EffectiveResourceConfig,
    /// Central knowledge folder state (`status.knowledge`).
    pub(crate) knowledge: Value,
}

/// `off|error|warn|info|debug|trace` (any case) as a tracing level filter.
pub(crate) fn level_filter_from_name(name: &str) -> Option<LevelFilter> {
    match name.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::OFF),
        "error" => Some(LevelFilter::ERROR),
        "warn" => Some(LevelFilter::WARN),
        "info" => Some(LevelFilter::INFO),
        "debug" => Some(LevelFilter::DEBUG),
        "trace" => Some(LevelFilter::TRACE),
        _ => None,
    }
}

/// File-log level at startup from `[logging] file_level` in attic.toml;
/// OFF when unset or unreadable (an invalid attic.toml is reported loudly
/// by the server's own config load moments later).
pub(crate) fn configured_file_log_level(db_path: &Path) -> LevelFilter {
    std::fs::read_to_string(attic_core::sibling(db_path, "attic.toml"))
        .ok()
        .and_then(|s| attic_core::AtticConfig::parse_str(&s).ok())
        .and_then(|c| c.logging.file_level())
        .and_then(|l| level_filter_from_name(&l))
        .unwrap_or(LevelFilter::OFF)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_status(
    pool: &DbPool,
    incremental: &HashMap<String, Arc<attic_incremental::IncrementalService>>,
    watch_mode: &HashMap<String, attic_incremental::WatchMode>,
    resource_monitor: Option<&attic_storage::resource_manager::ResourceMonitor>,
    configured: bool,
    active_roots: &[PathBuf],
    unavailable_roots: &[(PathBuf, String)],
    pending_index_roots: &[String],
    container_repo_roots: &HashMap<String, Vec<PathBuf>>,
    watcher_start_failures: &HashMap<String, String>,
    phase8: &ResourceStatus,
) -> Result<CallToolResult, ServerError> {
    let stats = pool.with_reader(get_db_stats)?;
    let mut payload = json!({ "status": "ok", "db": stats });

    // Retrieval telemetry: aggregated ops_retrieval_log outcomes over the
    // newest 1000 completed plans — which modes run, how they end, and why
    // semantic fell back. Data for budget/default tuning instead of guesses.
    // Best-effort: telemetry failure never breaks status.
    if let Ok(t) = pool.with_reader(|c| attic_storage::retrieval_plan_stats(c, 1000)) {
        payload["retrieval_telemetry"] = json!(t);
    }

    append_resource_status(&mut payload, resource_monitor, phase8);

    // Membership-authoritative scoping: only configured repositories participate.
    let (active_ids, id_owner_root_key): (HashSet<String>, HashMap<String, String>) = if configured
    {
        expand_active_ids(pool, active_roots, container_repo_roots)
    } else {
        (HashSet::new(), HashMap::new())
    };
    build_workspace_status(
        payload,
        pool,
        incremental,
        watch_mode,
        configured,
        active_roots,
        unavailable_roots,
        pending_index_roots,
        watcher_start_failures,
        active_ids,
        id_owner_root_key,
    )
}

pub(crate) fn append_resource_status(
    payload: &mut Value,
    resource_monitor: Option<&attic_storage::resource_manager::ResourceMonitor>,
    phase8: &ResourceStatus,
) {
    if let Some(monitor) = resource_monitor {
        payload["resource_pressure"] = json!({
            "level": monitor.pressure().to_string().to_lowercase(),
            // Phase 1 extension: hysteresis-smoothed tier (stable_tier_pressure)
            // and slot/RSS/uptime observability fields.
            "stable_tier": format!("{:?}", monitor.stable_tier_pressure()).to_lowercase(),
            "memory_used_mib": monitor.memory_used_mib(),
            "peak_memory_used_mib": monitor.peak_memory_used_mib(),
            "process_rss_mib": monitor.process_rss_mib(),
            "min_free_memory_mib": monitor.min_free_memory_mib(),
            "max_memory_mib": monitor.max_memory_mib(),
            "foreground_slots_in_use": monitor.foreground_slots_in_use(),
            "foreground_capacity": monitor.foreground_capacity(),
            "background_slots_in_use": monitor.background_slots_in_use(),
            "background_capacity": monitor.background_capacity(),
            "uptime_secs": monitor.uptime_secs(),
            // Phase 93+: adaptive-limit observability fields.
            "recovery_stage": format!("{:?}", monitor.recovery_stage()),
            "effective_indexing_heavy_limit": monitor.effective_indexing_heavy_limit(),
            "max_indexing_heavy": monitor.max_indexing_heavy(),
            "active_indexing_heavy": monitor.indexing_heavy_active(),
            "system_available_mib": monitor.system_available_mib(),
            "mcp_pressure_rejections": monitor.mcp_pressure_rejections(),
            "daemon_reconnect_count": monitor.daemon_reconnect_count.load(
                std::sync::atomic::Ordering::Relaxed
            ),
        });
        payload["resource_advisory"] = json!({
            "advisory": match attic_storage::resource_manager::current_advisory(monitor) {
                attic_storage::resource_manager::ResourceAdvisory::Ok => "ok",
                attic_storage::resource_manager::ResourceAdvisory::Degraded => "degraded",
                attic_storage::resource_manager::ResourceAdvisory::Restricted => "restricted",
            }
        });
    }

    // Phase 8: hardware-aware resource tuning + semantic identity. Answers
    // "I changed attic.toml, did Attic actually use it?" — `effective_resources`
    // is the ACTUAL EffectiveResourceConfig the running components received,
    // not just which mode was selected.
    payload["knowledge"] = phase8.knowledge.clone();
    payload["resource_mode"] = json!(phase8.resource_mode.as_str());
    payload["resource_mode_source"] = json!(phase8.resource_mode_source.as_str());
    payload["effective_resources"] = json!({
        "scheduler_workers": phase8.effective_resources.scheduler_workers,
        "sqlite_cache_pages": phase8.effective_resources.sqlite_cache_pages,
        "sqlite_mmap_bytes": phase8.effective_resources.sqlite_mmap_bytes,
        "memory_budget_mib": phase8.effective_resources.memory_budget_mib,
        "min_free_memory_mib": phase8.effective_resources.min_free_memory_mib,
        "max_foreground_queries": phase8.effective_resources.max_foreground_queries,
        "writer_batch_size": phase8.effective_resources.writer_batch_size,
        "writer_flush_interval_ms": phase8.effective_resources.writer_flush_interval_ms,
        "writer_queue_capacity": phase8.effective_resources.writer_queue_capacity,
        "max_io_ops_per_sec": phase8.effective_resources.max_io_ops_per_sec,
    });
}

#[allow(clippy::too_many_arguments)]
fn build_workspace_status(
    mut payload: Value,
    pool: &DbPool,
    incremental: &HashMap<String, Arc<attic_incremental::IncrementalService>>,
    watch_mode: &HashMap<String, attic_incremental::WatchMode>,
    configured: bool,
    active_roots: &[PathBuf],
    unavailable_roots: &[(PathBuf, String)],
    pending_index_roots: &[String],
    watcher_start_failures: &HashMap<String, String>,
    active_ids: HashSet<String>,
    id_owner_root_key: HashMap<String, String>,
) -> Result<CallToolResult, ServerError> {
    if !configured {
        payload["status"] = json!("unconfigured");
        payload["workspace"] = json!({
            "configured": false,
            "unconfigured": true,
            "configured_repository_count": 0,
            "active_repositories": [],
            "note": "no workspace configured yet — use the `workspace` MCP tool to add repository roots"
        });
        return Ok(CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&payload)?,
        )]));
    }

    // Per-repository watcher/incremental state — one entry per repository
    // known to storage, independent of every other repository's health.
    let repo_stats = pool.with_reader(get_repository_stats)?;
    let active_stats: Vec<&attic_storage::RepositoryStats> = repo_stats
        .iter()
        .filter(|rs| active_ids.contains(&rs.id))
        .collect();
    let mut repositories = Vec::with_capacity(active_stats.len());
    let mut current = 0u64;
    let mut indexing = 0u64;
    let mut reconciliation_required = 0u64;
    let mut disabled = 0u64;
    let mut stuck_tasks = Vec::new();
    for rs in &active_stats {
        let (state, watcher_json) = match (incremental.get(&rs.id), watch_mode.get(&rs.id)) {
            (Some(svc), Some(mode)) => match svc.status_snapshot(pool, &rs.id) {
                Ok(snap) => {
                    let state = if snap.reconciliation_required {
                        "RECONCILIATION_REQUIRED"
                    } else if snap.tasks.pending > 0 || snap.tasks.running > 0 {
                        "INDEXING"
                    } else {
                        "CURRENT"
                    };
                    if !snap.tasks.stuck_tasks.is_empty() {
                        for task in &snap.tasks.stuck_tasks {
                            stuck_tasks.push(json!({
                                "repository_id": rs.id,
                                "repository": &rs.display_name,
                                "task_id": &task.task_id,
                                "task_type": &task.task_type,
                                "task_repository": &task.repository,
                                "age_seconds": task.age_seconds,
                            }));
                        }
                    }
                    (
                        state,
                        json!({
                            "mode": mode.as_str(),
                            "active": matches!(mode, attic_incremental::WatchMode::NativeWatcher),
                            "periodic_reconciliation": matches!(
                                mode,
                                attic_incremental::WatchMode::PeriodicReconciliation
                            ),
                            "events_ingested": snap.events_ingested,
                            "hints_dropped": snap.hints_dropped,
                            "watcher_errors": snap.watcher_errors,
                            "raw_batches_dropped": snap.raw_batches_dropped,
                            "reconciliation_required": snap.reconciliation_required,
                            "freshness": snap.freshness,
                            "tasks": snap.tasks,
                        }),
                    )
                }
                Err(e) => (
                    "UNKNOWN",
                    json!({ "mode": mode.as_str(), "error": e.to_string() }),
                ),
            },
            _ => {
                let bootstrap_in_progress = id_owner_root_key.get(&rs.id).is_some_and(|key| {
                    pending_index_roots
                        .iter()
                        .any(|p| p == key || p == "__startup__")
                });
                if bootstrap_in_progress {
                    (
                        "INDEXING",
                        json!({ "mode": "pending", "active": false, "reason": "bootstrap_in_progress" }),
                    )
                } else if let Some(err) = watcher_start_failures.get(&rs.id) {
                    (
                        "DISABLED",
                        json!({ "mode": "disabled", "active": false, "error": err }),
                    )
                } else {
                    (
                        "DISABLED",
                        json!({ "mode": "disabled", "active": false, "reason": "watcher_not_registered" }),
                    )
                }
            }
        };
        match state {
            "CURRENT" => current += 1,
            "INDEXING" => indexing += 1,
            "RECONCILIATION_REQUIRED" => reconciliation_required += 1,
            _ => disabled += 1,
        }
        repositories.push(json!({
            "repository_id": rs.id,
            "display_name": rs.display_name,
            "file_count": rs.file_count,
            "unit_count": rs.unit_count,
            "state": state,
            "watcher": watcher_json,
        }));
    }
    payload["incremental_stuck_tasks"] = json!(stuck_tasks);
    // Configured roots can be indexing before their repository row exists.
    // Surface them explicitly instead of reporting configured_repository_count=0.
    for root in active_roots {
        let key = root_identity_key(root);
        let has_repo = pool
            .with_reader(|c| lookup_repository_by_root_path(c, &root.to_string_lossy()))
            .ok()
            .flatten()
            .is_some();
        if !has_repo
            && pending_index_roots
                .iter()
                .any(|p| p == &key || p == "__startup__")
        {
            indexing += 1;
            repositories.push(json!({
                "repository_id": Value::Null,
                "display_name": root.display().to_string(),
                "file_count": 0,
                "unit_count": 0,
                "state": "INDEXING",
                "watcher": { "mode": "pending", "active": false }
            }));
        }
    }

    // §17: configured-but-unavailable roots are reported explicitly so the
    // caller can see the workspace is DEGRADED, never silently dropped from
    // membership or hidden behind an otherwise-current summary.
    let unavailable: Vec<Value> = unavailable_roots
        .iter()
        .map(|(p, reason)| json!({ "path": p.display().to_string(), "reason": reason }))
        .collect();
    payload["workspace"] = json!({
        "configured": true,
        "unconfigured": false,
        "configured_repository_count": active_roots.len(),
        "current_repository_count": current,
        "indexing_repository_count": indexing,
        "reconciliation_required_repository_count": reconciliation_required,
        "disabled_repository_count": disabled,
        "unavailable_repository_count": unavailable.len(),
        "degraded": !unavailable.is_empty(),
        "unavailable_repositories": unavailable,
        "repositories": repositories,
    });

    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&payload)?,
    )]))
}
