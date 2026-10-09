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
pub(crate) struct ResourceStatus<'a> {
    pub(crate) resource_mode: attic_storage::ResourceMode,
    pub(crate) resource_mode_source: attic_storage::ResourceModeSource,
    pub(crate) effective_resources: attic_storage::EffectiveResourceConfig,
    pub(crate) semantic: Option<&'a attic_retrieval::semantic::SemanticStack>,
    pub(crate) attic_config: &'a attic_core::AtticConfig,
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

/// Queue depth above which a GROWING queue is reported as backpressure.
pub(crate) const QUEUE_BACKPRESSURE_DEPTH: u64 = 5_000;

/// True when the embedding queue is large and has grown since a sample at
/// least 30 s old. A large queue that is draining is normal work: reporting it
/// as "backpressure … exceeded high watermark" made a healthy 80-minute GPU
/// run look like a fault.
pub(crate) fn queue_is_growing(depth: u64) -> bool {
    static SAMPLE: std::sync::Mutex<Option<(std::time::Instant, u64)>> =
        std::sync::Mutex::new(None);
    queue_is_growing_at(&SAMPLE, std::time::Instant::now(), depth)
}

pub(crate) fn queue_is_growing_at(
    sample: &std::sync::Mutex<Option<(std::time::Instant, u64)>>,
    now: std::time::Instant,
    depth: u64,
) -> bool {
    const MIN_SPAN: std::time::Duration = std::time::Duration::from_secs(30);
    let mut g = sample.lock().unwrap_or_else(|e| e.into_inner());
    let growing = match *g {
        Some((t, prev)) if now.duration_since(t) >= MIN_SPAN => {
            *g = Some((now, depth));
            depth > prev
        }
        Some((_, prev)) => depth > prev,
        None => {
            *g = Some((now, depth));
            false
        }
    };
    growing && depth >= QUEUE_BACKPRESSURE_DEPTH
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
    phase8: &ResourceStatus<'_>,
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

    // Same live decision the enricher makes each iteration: how host-RAM
    // pressure gates embedding on the backend that is actually serving.
    let embedding_class = phase8.semantic.map(|s| {
        attic_semantic::embedding_memory_class(
            s.provider.as_ref(),
            GPU_UNIFIED_MEMORY.get().copied().unwrap_or(false),
        )
    });
    let gpu_embedding = embedding_class.is_some_and(|c| c.host_floor_mib().is_some());

    // Resource pressure state — Phase 7 foreground/background priority.
    if let Some(monitor) = resource_monitor {
        let class = embedding_class
            .unwrap_or(attic_storage::resource_manager::EmbeddingMemoryClass::HostRam);
        let embedding_gate = match (class.host_floor_mib(), monitor.embedding_parked(class)) {
            (None, _) => "host_ram_tiers".to_string(),
            (Some(_), true) => format!("{}_parked_host_ram_floor", class.as_str()),
            (Some(_), false) => format!("{}_exempt_from_tiers", class.as_str()),
        };
        let embedding_limit = monitor.embedding_limit_for(class);
        let embedding_batch = monitor.embedding_batch_for(class);
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
            "effective_embedding_limit": embedding_limit,
            "active_embedding_heavy": monitor.embedding_heavy_active(),
            "effective_embedding_batch": embedding_batch,
            // Which admission rule governs embedding right now, and the
            // inputs to the dedicated-GPU host floor.
            "embedding_gate": embedding_gate,
            "embedding_memory_class": class.as_str(),
            "system_available_mib": monitor.system_available_mib(),
            "embedding_host_floor_mib": class.host_floor_mib(),
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
        "embedding_batch_size": phase8.effective_resources.embedding_batch_size,
        "embedding_worker_count": phase8.effective_resources.embedding_worker_count,
        "semantic_cpu_threads": semantic_cpu_thread_budget(
            std::thread::available_parallelism().map_or(1, usize::from)
        ),
        "writer_batch_size": phase8.effective_resources.writer_batch_size,
        "writer_flush_interval_ms": phase8.effective_resources.writer_flush_interval_ms,
        "writer_queue_capacity": phase8.effective_resources.writer_queue_capacity,
        "max_io_ops_per_sec": phase8.effective_resources.max_io_ops_per_sec,
    });
    payload["embedding_recommendation"] = json!({
        "provider": attic_semantic::QWEN_PROVIDER_ID,
        "model": attic_semantic::QWEN_MODEL_ID,
    });
    // In Phase 103, Qwen3 is the sole production provider and no provider overrides exist.
    payload["embedding_override_configured"] = json!(false);
    payload["semantic_configured_enabled"] = json!(phase8.attic_config.semantic.enabled);
    payload["semantic_configured_model"] = json!(phase8.attic_config.semantic.model);
    let semantic_health = match phase8.semantic {
        None => "disabled",
        Some(stack) => {
            if stack.provider.available() {
                "active"
            } else {
                "degraded"
            }
        }
    };
    payload["semantic_health"] = json!(semantic_health);
    payload["semantic_availability"] = match phase8.semantic {
        Some(stack) => {
            let class = embedding_class
                .unwrap_or(attic_storage::resource_manager::EmbeddingMemoryClass::HostRam);
            let embedding_parked = resource_monitor.map(|m| m.embedding_parked(class));
            let embedding_parked_reason = resource_monitor.and_then(|monitor| {
                if !monitor.embedding_parked(class) {
                    return None;
                }
                Some(match class.host_floor_mib() {
                    Some(floor) => format!(
                        "available system memory is below the {floor} MiB {} host floor",
                        class.as_str()
                    ),
                    None => {
                        "host-RAM embedding is parked because the stable pressure tier is emergency"
                            .to_string()
                    }
                })
            });
            let coverage = stack.store.queue_counts().ok();
            let embedded_units = stack
                .store
                .count(stack.provider.id(), stack.provider.model_id(), None)
                .ok();
            let search_reason = if !stack.provider.available() {
                Some(attic_retrieval::SemanticDegradationReason::ProviderUnavailable)
            } else {
                match stack.store.get_active_generation() {
                    Err(_) => Some(attic_retrieval::SemanticDegradationReason::StoreUnavailable),
                    Ok(None) => Some(attic_retrieval::SemanticDegradationReason::NoEmbeddings),
                    Ok(Some(_)) => match embedded_units {
                        None => Some(attic_retrieval::SemanticDegradationReason::StoreUnavailable),
                        Some(0) => Some(attic_retrieval::SemanticDegradationReason::NoEmbeddings),
                        Some(_) => None,
                    },
                }
            };
            let search_reason_code = search_reason.map(|reason| match reason {
                attic_retrieval::SemanticDegradationReason::ProviderUnavailable => {
                    "PROVIDER_UNAVAILABLE"
                }
                attic_retrieval::SemanticDegradationReason::NoEmbeddings => "NO_EMBEDDINGS",
                attic_retrieval::SemanticDegradationReason::QueryTimedOut => "QUERY_TIMED_OUT",
                attic_retrieval::SemanticDegradationReason::EmbeddingFailed => "EMBEDDING_FAILED",
                attic_retrieval::SemanticDegradationReason::StoreUnavailable => "STORE_UNAVAILABLE",
            });
            json!({
                "pressure_tier": resource_monitor
                    .map(|m| format!("{:?}", m.stable_tier_pressure()).to_lowercase()),
                "system_available_mib": resource_monitor.and_then(|m| m.system_available_mib()),
                "embedding_memory_class": class.as_str(),
                "embedding_parked": embedding_parked,
                "embedding_parked_reason": embedding_parked_reason,
                "coverage": {
                    "embedded_units": embedded_units,
                    "eligible_units": coverage
                        .as_ref()
                        .map(|q| q.pending + q.inflight + q.done + q.failed),
                    "pending_units": coverage.as_ref().map(|q| q.pending),
                    "inflight_units": coverage.as_ref().map(|q| q.inflight),
                    "failed_units": coverage.as_ref().map(|q| q.failed),
                },
                "search_uses_semantic": search_reason.is_none(),
                "search_semantic_reason": search_reason_code,
                "search_semantic_reason_text": search_reason.map(|reason| reason.description()),
            })
        }
        None => json!({
            "pressure_tier": resource_monitor
                .map(|m| format!("{:?}", m.stable_tier_pressure()).to_lowercase()),
            "system_available_mib": resource_monitor.and_then(|m| m.system_available_mib()),
            "embedding_memory_class": serde_json::Value::Null,
            "embedding_parked": serde_json::Value::Null,
            "embedding_parked_reason": serde_json::Value::Null,
            "coverage": {
                "embedded_units": serde_json::Value::Null,
                "eligible_units": serde_json::Value::Null,
                "pending_units": serde_json::Value::Null,
                "inflight_units": serde_json::Value::Null,
                "failed_units": serde_json::Value::Null,
            },
            "search_uses_semantic": false,
            "search_semantic_reason": "SEMANTIC_DISABLED",
            "search_semantic_reason_text": "semantic search is disabled because the semantic layer is not configured",
        }),
    };
    if let Some(stack) = phase8.semantic {
        if let Some(lifecycle) = stack.provider.model_lifecycle() {
            payload["model_lifecycle"] = json!(lifecycle);
        }
        // r13: identity truth — which backend/quantization/vector space is
        // actually serving, plus worker supervision state. Answers "am I on
        // GPU or CPU, fp16 or fp32, and is inference isolated?" without
        // reading logs.
        let fp = stack.provider.fingerprint();
        let gpu_report = gpu_capability_report(phase8.attic_config);
        let fallback_reason = stack.provider.fallback_reason();
        payload["semantic_identity"] = json!({
            "provider_id": stack.provider.id(),
            // One line: which device embeds and why.
            "device": device_line(fallback_reason.clone(), &gpu_report),
            "backend": fp
                .as_ref()
                .map(|f| f.execution_backend.as_str())
                .unwrap_or("unknown"),
            "quantization": fp
                .as_ref()
                .map(|f| f.quantization.as_str())
                .unwrap_or("unknown"),
            "vector_space_id": fp.as_ref().map(|f| f.vector_space_id()),
            "dimension": fp.as_ref().map(|f| f.dimension),
            "worker_isolated": stack.provider.id() == "qwen3-supervised",
            // GPU->CPU escalation state, when the active provider is a
            // `FallbackCoordinator` (or any provider that overrides
            // `fallback_reason`). `None` (never fabricated) when this
            // provider never fell back — see `attic_semantic::fallback`.
            "fallback_reason": fallback_reason,
            // Why this process can or cannot use a GPU. Always populated,
            // so "backend": "candle-cpu" is never ambiguous about whether
            // CPU was a deliberate choice or an unreported capability gap.
            "gpu": gpu_report,
            // Model worker lifecycle: not_loaded / loading / loaded /
            // unloaded (idle unload), last use, and last load time.
            "worker": stack.provider.worker_status(),
        });
    }

    // Phase V2 CP18: Semantic progress, ETA, and "why slow" diagnostics (§61, §62).
    if let Some(stack) = phase8.semantic {
        let qcounts = stack.store.queue_counts().unwrap_or_default();
        let (pending, inflight, done, failed) = (
            qcounts.pending,
            qcounts.inflight,
            qcounts.done,
            qcounts.failed,
        );
        let active_gen = stack
            .store
            .get_active_generation()
            .ok()
            .flatten()
            .map(|g| g.generation_id);
        let building_gen = stack
            .store
            .get_building_generation()
            .ok()
            .flatten()
            .map(|g| g.generation_id);
        let cache_state = if stack.provider.available() {
            "ready"
        } else {
            "loading"
        };

        // Throughput measured by the enricher on every committed batch over
        // a 5-minute window (idle gaps included), so the rate and ETA do not
        // depend on how often a client polls `status`. `batch_latency_ms` is
        // the measured mean claim-to-commit time of one batch.
        let measured = attic_semantic::throughput::snapshot();
        let chunks_per_sec = measured.chunks_per_sec;
        let batch_latency_ms = measured.batch_latency_ms;
        let progress = attic_semantic::SemanticProgressSnapshot::compute(
            pending,
            inflight,
            done,
            failed,
            chunks_per_sec,
            batch_latency_ms,
            active_gen,
            building_gen,
            cache_state,
        );
        payload["semantic_progress"] = json!(progress);

        // Phase 5 stall detection: flag a hung inference worker (the
        // 2026-09 incident: 16 in-flight, 0 done for 20+ min reported as
        // merely "slow"). The clock resets when new work arrives after an
        // idle spell, and the verdict distinguishes load/backoff from an
        // actual hung batch.
        {
            static WORK_AVAILABLE_CLOCK: std::sync::Mutex<
                attic_semantic::diagnostics::WorkAvailabilityClock,
            > = std::sync::Mutex::new(attic_semantic::diagnostics::WorkAvailabilityClock {
                active_since_secs: None,
            });
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let work_clock = {
                let mut clock = WORK_AVAILABLE_CLOCK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *clock = attic_semantic::diagnostics::observe_work_availability(
                    *clock, now_secs, pending, inflight,
                );
                *clock
            };
            let secs_since_advance = attic_semantic::diagnostics::secs_since_meaningful_progress(
                work_clock,
                now_secs,
                measured.secs_since_last_commit,
            )
            .unwrap_or(0);
            let provider_backoff = attic_semantic::diagnostics::provider_backoff_snapshot();
            let progress_heartbeat_supported =
                stack.provider.fingerprint().is_some_and(|fingerprint| {
                    matches!(
                        fingerprint.execution_backend,
                        attic_semantic::ExecutionBackend::OrtDirectMl
                    )
                });
            let stall = attic_semantic::diagnostics::assess_stall(
                &attic_semantic::diagnostics::StallContext {
                    queue_depth: pending + inflight,
                    inflight,
                    done,
                    chunks_per_sec,
                    secs_since_last_progress: secs_since_advance,
                    loading_or_warmup: !stack.provider.available(),
                    provider_backoff: provider_backoff.clone(),
                    progress_heartbeat_supported,
                },
            );
            payload["semantic_stall"] = json!({
                "stalled": stall.stalled,
                "verdict": stall.verdict,
                "secs_since_last_completed_batch": secs_since_advance,
                "secs_since_last_meaningful_progress": secs_since_advance,
                "detection_mode": stall.detection_mode,
                "heartbeat_watchdog_supported": progress_heartbeat_supported,
                "deadline_secs": attic_semantic::diagnostics::EMBED_DEADLINE_SECS,
            });
            if let Some(backoff) = provider_backoff {
                payload["semantic_provider_backoff"] = json!({
                    "last_error": backoff.last_error,
                    "consecutive_failures": backoff.consecutive_failures,
                    "next_retry_unix_ms": backoff.next_retry_unix_ms,
                });
            }

            // Why the embedded count is what it is. Selection can reject the
            // vast majority of an index for entirely legitimate reasons
            // (duplicates, low signal, caps) — without the breakdown an
            // operator cannot distinguish that from a misconfiguration.
            if let Some(sel) = attic_semantic::last_selection_report() {
                let mut excluded: Vec<(&str, usize)> =
                    sel.excluded.iter().map(|(k, v)| (*k, *v)).collect();
                excluded.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                let breakdown: serde_json::Map<String, serde_json::Value> = excluded
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), json!(v)))
                    .collect();
                let repo_cap_excluded = sel
                    .excluded
                    .get(attic_semantic::EX_CAP_REPO)
                    .copied()
                    .unwrap_or(0);
                let total_cap_excluded = sel
                    .excluded
                    .get(attic_semantic::EX_CAP_TOTAL)
                    .copied()
                    .unwrap_or(0);
                let eligible_before_caps = sel.selected + repo_cap_excluded + total_cap_excluded;
                let cap_reason = |limit: usize, scope: &str, source_key: &str| {
                    if let Some(eff) = SELECTION_EFFECTIVE.get() {
                        let source = eff
                            .get("source")
                            .and_then(|s| s.get(source_key))
                            .and_then(|v| v.as_str());
                        let profile = eff.get("profile").and_then(|v| v.as_str());
                        match source {
                            Some("attic.toml") => {
                                format!("cap reached (attic.toml {limit}/{scope})")
                            }
                            _ => match profile {
                                Some("gpu_defaults") => {
                                    format!("cap reached (GPU default {limit}/{scope})")
                                }
                                Some("cpu_defaults") => {
                                    format!("cap reached (CPU default {limit}/{scope})")
                                }
                                _ => format!("cap reached ({limit}/{scope})"),
                            },
                        }
                    } else {
                        format!("cap reached ({limit}/{scope})")
                    }
                };
                let configured_total_cap = SELECTION_EFFECTIVE
                    .get()
                    .and_then(|eff| eff.get("max_units_total"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(
                        attic_semantic::SelectionConfig::for_backend(gpu_embedding).max_units_total
                            as u64,
                    ) as usize;
                let configured_repo_cap = SELECTION_EFFECTIVE
                    .get()
                    .and_then(|eff| eff.get("max_units_per_repo"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(
                        attic_semantic::SelectionConfig::for_backend(gpu_embedding)
                            .max_units_per_repo as u64,
                    ) as usize;
                let coverage_reason = if total_cap_excluded > 0 {
                    Some(cap_reason(
                        configured_total_cap,
                        "workspace",
                        "max_units_total",
                    ))
                } else if repo_cap_excluded > 0 {
                    Some(cap_reason(
                        configured_repo_cap,
                        "repo",
                        "max_units_per_repo",
                    ))
                } else {
                    None
                };
                payload["semantic_selection"] = json!({
                    "scanned": sel.scanned,
                    "scan_truncated": sel.scan_truncated,
                    "selected": sel.selected,
                    "excluded": breakdown,
                    "top_exclusion_reason": excluded.first().map(|(k, _)| *k),
                });
                payload["semantic_selection_coverage"] = json!({
                    "eligible_before_caps": eligible_before_caps,
                    "selected": sel.selected,
                    "per_repository_cap_excluded": repo_cap_excluded,
                    "global_cap_excluded": total_cap_excluded,
                    "cap_reached": coverage_reason.is_some(),
                    "reason": coverage_reason,
                });
            }
            if let Some(eff) = SELECTION_EFFECTIVE.get() {
                payload["semantic_selection_effective"] = eff.clone();
            }
        }

        let diag_ctx = attic_semantic::DiagnosticContext {
            disk_emergency: false,
            disk_warning: false,
            resource_pressure_restricted: resource_monitor.is_some_and(|m| {
                matches!(
                    attic_storage::resource_manager::current_advisory(m),
                    attic_storage::resource_manager::ResourceAdvisory::Restricted
                )
            }),
            // Real sampled availability; `min_free_memory_mib` is a configured
            // reserve (e.g. 400), which made this read as "constrained" always.
            available_ram_mib: resource_monitor
                .and_then(|m| m.system_available_mib())
                .unwrap_or(4096),
            queue_depth: pending + inflight,
            queue_backpressure_active: queue_is_growing(pending + inflight),
            canonical_indexing_active: resource_monitor
                .is_some_and(|m| m.indexing_heavy_active() > 0),
            // Work is queued and the model is ready: embedding is running (the
            // permit is only held inside a drive slice, so sampling it made
            // the verdict flicker to "nominal" between slices).
            semantic_inference_active: (pending + inflight) > 0,
            model_loading_or_warmup: !stack.provider.available(),
            mcp_high_latency: false,
            user_caps_active: false,
            gpu_embedding,
            host_ram_floor_breached: gpu_embedding
                && resource_monitor
                    .zip(embedding_class)
                    .is_some_and(|(m, c)| m.embedding_parked(c)),
        };
        let why_slow = attic_semantic::diagnose_why_slow(&diag_ctx);
        payload["diagnostics"] = json!({
            "why_slow": why_slow.explanation,
            "bottleneck_code": why_slow.code,
        });
    }

    // Membership-authoritative scoping: ONLY repositories that belong to the
    // configured logical workspace are reported as current/active. Historical
    // repositories still present in the DB but no longer configured must not
    // masquerade as active (spec §14-16). When UNCONFIGURED, the active set
    // is empty and status reports "unconfigured" — stale DB repos never leak
    // into the response.
    let (active_ids, id_owner_root_key): (HashSet<String>, HashMap<String, String>) = if configured
    {
        expand_active_ids(pool, active_roots, container_repo_roots)
    } else {
        (HashSet::new(), HashMap::new())
    };
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
