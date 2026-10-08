//! User-Visible Semantic Progress, ETA, and "Why Slow" Diagnostics (Final Master Plan V2 §61, §62, CP18).
//!
//! Provides structured observability for semantic embedding and machine resource allocation:
//! - Detailed queue progress: pending, inflight, failed, and done counts.
//! - Throughput rates: chunks/sec, batches/sec, batch latency, and ETA.
//! - Resource mode and active generation state.
//! - Best-effort "why_slow" diagnostic explanation (§62).

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

/// Detailed snapshot of semantic background enrichment progress and throughput (§61).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticProgressSnapshot {
    /// Total pending items in the enrichment queue.
    pub queue_pending: u64,
    /// Currently in-flight items claimed by workers.
    pub queue_inflight: u64,
    /// Items successfully embedded and committed.
    pub queue_done: u64,
    /// Quarantined failed items.
    pub queue_failed: u64,
    /// Total active queue depth (`pending + inflight`).
    pub total_queue_depth: u64,
    /// Throughput rate in chunks per second.
    pub chunks_per_sec: f64,
    /// Throughput rate in batches per second.
    pub batches_per_sec: f64,
    /// Average batch latency in milliseconds.
    pub batch_latency_ms: f64,
    /// Estimated time to drain queue in seconds, if throughput > 0.
    pub eta_seconds: Option<u64>,
    /// Why `eta_seconds` is `None`, in plain language.
    ///
    /// A null ETA with no explanation is the least useful thing a progress
    /// API can report: during a long throttle the queue showed
    /// `eta_seconds: null` for twelve straight minutes, which is
    /// indistinguishable from "just started" or "hung". Whenever the ETA
    /// cannot be computed this states the observable reason instead, and it
    /// is `None` only when `eta_seconds` is actually populated.
    pub eta_unavailable_reason: Option<String>,
    /// Currently active generation ID serving queries.
    pub active_generation_id: Option<i64>,
    /// Generation ID currently building in background, if any.
    pub building_generation_id: Option<i64>,
    /// Local model cache availability status.
    pub model_cache_state: String,
}

impl SemanticProgressSnapshot {
    /// Compute progress snapshot from queue counts, throughput metrics, and generation IDs.
    #[allow(clippy::too_many_arguments)]
    pub fn compute(
        pending: u64,
        inflight: u64,
        done: u64,
        failed: u64,
        chunks_per_sec: f64,
        batch_latency_ms: f64,
        active_gen: Option<i64>,
        building_gen: Option<i64>,
        model_cache_state: impl Into<String>,
    ) -> Self {
        let total_depth = pending + inflight;
        let batches_per_sec = if batch_latency_ms > 0.0 {
            1000.0 / batch_latency_ms
        } else {
            0.0
        };

        let eta_seconds = if chunks_per_sec > 0.1 && total_depth > 0 {
            Some((total_depth as f64 / chunks_per_sec).ceil() as u64)
        } else if total_depth == 0 {
            Some(0)
        } else {
            None
        };

        // Never report an unexplained null ETA — say what is actually
        // observable instead.
        let eta_unavailable_reason = if eta_seconds.is_some() {
            None
        } else if inflight > 0 {
            Some(format!(
                "{total_depth} item(s) queued and {inflight} in flight, but no batch has \
                 completed yet, so there is no throughput to extrapolate from; \
                 see why_slow for the current bottleneck"
            ))
        } else {
            Some(format!(
                "{total_depth} item(s) queued but no worker has been dispatched yet, so \
                 throughput is zero; see why_slow for the current bottleneck"
            ))
        };

        Self {
            queue_pending: pending,
            queue_inflight: inflight,
            queue_done: done,
            queue_failed: failed,
            total_queue_depth: total_depth,
            chunks_per_sec,
            batches_per_sec,
            batch_latency_ms,
            eta_seconds,
            eta_unavailable_reason,
            active_generation_id: active_gen,
            building_generation_id: building_gen,
            model_cache_state: model_cache_state.into(),
        }
    }
}

/// Tracks when semantic work most recently became available in this process.
///
/// The stall clock must reset when a previously idle queue receives new work,
/// even if the last committed batch in this process happened hours ago.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAvailabilityClock {
    /// When the queue most recently transitioned from idle to active.
    pub active_since_secs: Option<u64>,
}

/// Advance the queue-activity clock from one status observation.
pub fn observe_work_availability(
    clock: WorkAvailabilityClock,
    now_secs: u64,
    pending: u64,
    inflight: u64,
) -> WorkAvailabilityClock {
    if pending + inflight == 0 {
        WorkAvailabilityClock {
            active_since_secs: None,
        }
    } else if clock.active_since_secs.is_some() {
        clock
    } else {
        WorkAvailabilityClock {
            active_since_secs: Some(now_secs),
        }
    }
}

/// Seconds since meaningful semantic progress for the CURRENT queue.
///
/// "Meaningful progress" is the later of:
/// 1. the last committed batch recorded by the enricher itself; or
/// 2. when the queue most recently became non-empty in this process.
///
/// This prevents a long-idle server from treating fresh work as already
/// stalled before a single batch has had a chance to run.
pub fn secs_since_meaningful_progress(
    clock: WorkAvailabilityClock,
    now_secs: u64,
    secs_since_last_commit: Option<u64>,
) -> Option<u64> {
    let active_since = clock.active_since_secs?;
    let last_progress_at = secs_since_last_commit
        .map(|age| now_secs.saturating_sub(age))
        .map_or(active_since, |commit_at| commit_at.max(active_since));
    Some(now_secs.saturating_sub(last_progress_at))
}

/// Most recent provider-load/backoff state, surfaced in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderBackoffStatus {
    /// Consecutive transient provider failures driving the current backoff.
    pub consecutive_failures: u32,
    /// Human-readable last error from the provider or worker load path.
    pub last_error: String,
    /// Unix time in milliseconds when the next retry is scheduled.
    pub next_retry_unix_ms: u64,
}

static PROVIDER_BACKOFF_STATUS: OnceLock<Mutex<Option<ProviderBackoffStatus>>> = OnceLock::new();

fn provider_backoff_cell() -> &'static Mutex<Option<ProviderBackoffStatus>> {
    PROVIDER_BACKOFF_STATUS.get_or_init(|| Mutex::new(None))
}

fn unix_ms_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record that semantic work is paused behind a provider/load backoff.
pub fn note_provider_backoff(
    consecutive_failures: u32,
    delay_ms: u64,
    last_error: impl Into<String>,
) {
    if let Ok(mut slot) = provider_backoff_cell().lock() {
        *slot = Some(ProviderBackoffStatus {
            consecutive_failures,
            last_error: last_error.into(),
            next_retry_unix_ms: unix_ms_now().saturating_add(delay_ms),
        });
    }
}

/// Clear any recorded provider backoff once work resumes normally.
pub fn clear_provider_backoff() {
    if let Ok(mut slot) = provider_backoff_cell().lock() {
        *slot = None;
    }
}

/// Current provider backoff snapshot, if the queue is intentionally paused.
pub fn provider_backoff_snapshot() -> Option<ProviderBackoffStatus> {
    provider_backoff_cell()
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
}

/// Stall detection (Phase 5): flags the exact failure mode observed in
/// production — a batch claimed INFLIGHT but producing zero completions for
/// longer than the threshold. Pure function over observable counters so it
/// is testable without a live queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallAssessment {
    pub stalled: bool,
    /// Human-readable verdict for status output.
    pub verdict: String,
    /// `progress_watchdog` for heartbeat-backed detection, `deadline_only`
    /// for backends that cannot report in-batch progress.
    pub detection_mode: &'static str,
}

/// Assess whether the enrichment pipeline is stalled.
///
/// `inflight` items with `chunks_per_sec` at ~0 for longer than
/// `STALL_THRESHOLD_SECS` since the last completed batch means the worker is
/// hung (e.g. a blocked GPU kernel call), not merely slow — a healthy slow
/// pipeline still completes batches. The 2026-09 incident: 16 chunks
/// in-flight 20+ min, 0 completed — this detector flags exactly that.
pub const STALL_THRESHOLD_SECS: u64 = 120;

/// Wall-clock budget the supervisor gives a single embedding batch before it
/// kills the worker process and restarts it.
///
/// This lives here, and `worker_supervisor::EMBED_DEADLINE` is derived from
/// it, so the number quoted in operator-facing diagnostics is provably the
/// same number the supervisor enforces.
pub const EMBED_DEADLINE_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct StallContext {
    /// Total queue depth (`pending + inflight`).
    pub queue_depth: u64,
    /// Claimed items currently in-flight.
    pub inflight: u64,
    /// Completed items for the active vector space.
    pub done: u64,
    /// Measured committed throughput.
    pub chunks_per_sec: f64,
    /// Seconds since the later of "queue became active" and "last commit".
    pub secs_since_last_progress: u64,
    /// The provider is still loading or warming up and has not produced any
    /// throughput yet.
    pub loading_or_warmup: bool,
    /// Current provider-load backoff, if the queue is intentionally paused.
    pub provider_backoff: Option<ProviderBackoffStatus>,
    /// Whether the active backend emits progress heartbeats inside a batch.
    pub progress_heartbeat_supported: bool,
}

pub fn assess_stall(ctx: &StallContext) -> StallAssessment {
    let detection_mode = if ctx.progress_heartbeat_supported {
        "progress_watchdog"
    } else {
        "deadline_only"
    };
    if let Some(backoff) = ctx.provider_backoff.as_ref() {
        return StallAssessment {
            stalled: false,
            verdict: format!(
                "BACKING OFF: provider load failed {} consecutive time(s); queue is held \
                 PENDING until the next retry. Last error: {}",
                backoff.consecutive_failures, backoff.last_error
            ),
            detection_mode,
        };
    }
    if ctx.loading_or_warmup && ctx.queue_depth > 0 && ctx.chunks_per_sec < 0.01 {
        return StallAssessment {
            stalled: false,
            verdict: if ctx.progress_heartbeat_supported {
                format!(
                    "LOADING/WARMING: {0} queued item(s), no completed batch yet; the provider \
                     is still loading or warming up, so stall timing starts only after it begins \
                     serving work",
                    ctx.queue_depth
                )
            } else {
                format!(
                    "LOADING/WARMING: {0} queued item(s), no completed batch yet; this backend \
                     does not emit progress heartbeats, so hang detection is deadline-based at \
                     {EMBED_DEADLINE_SECS}s",
                    ctx.queue_depth
                )
            },
            detection_mode,
        };
    }

    let stall_threshold = if ctx.progress_heartbeat_supported {
        STALL_THRESHOLD_SECS
    } else {
        EMBED_DEADLINE_SECS
    };
    let stalled = ctx.inflight > 0
        && ctx.chunks_per_sec < 0.01
        && ctx.secs_since_last_progress > stall_threshold;
    let verdict = if stalled {
        if ctx.progress_heartbeat_supported {
            // Past the detection threshold but still inside the supervisor's kill
            // deadline, recovery is already scheduled and automatic. Telling an
            // operator to "restart the embedding worker" here is wrong advice: it
            // reads as "this is dead", when in fact the worker gets killed and
            // restarted without intervention. Only once the deadline has passed
            // without a restart is something genuinely wedged.
            if ctx.secs_since_last_progress <= EMBED_DEADLINE_SECS {
                let secs_to_recovery = EMBED_DEADLINE_SECS - ctx.secs_since_last_progress;
                format!(
                    "STALLED: {0} items in-flight, 0 completed batches for {1}s (threshold \
                     {STALL_THRESHOLD_SECS}s) — the supervisor kills and restarts a hung worker \
                     at {EMBED_DEADLINE_SECS}s, so recovery is automatic in ~{2}s; no action \
                     needed yet",
                    ctx.inflight, ctx.secs_since_last_progress, secs_to_recovery
                )
            } else {
                format!(
                    "STALLED: {0} items in-flight, 0 completed batches for {1}s — past the \
                     {EMBED_DEADLINE_SECS}s supervisor deadline without a restart, so the worker \
                     is genuinely wedged; restart the embedding worker",
                    ctx.inflight, ctx.secs_since_last_progress
                )
            }
        } else {
            format!(
                "STALLED: {0} items in-flight, 0 completed batches for {1}s — this backend does \
                 not emit progress heartbeats, so hang detection is deadline-based; the batch is \
                 past the {EMBED_DEADLINE_SECS}s hard deadline and should already have restarted",
                ctx.inflight, ctx.secs_since_last_progress
            )
        }
    } else if ctx.inflight > 0 && ctx.chunks_per_sec < 0.01 {
        if ctx.progress_heartbeat_supported {
            format!(
                "SLOW: {0} items in-flight, no batch completed yet ({1}s) — within tolerance, \
                 first batch may still be running",
                ctx.inflight, ctx.secs_since_last_progress
            )
        } else {
            format!(
                "SLOW: {0} items in-flight, no batch completed yet ({1}s) — this backend does \
                 not emit progress heartbeats, so hang detection is deadline-based at \
                 {EMBED_DEADLINE_SECS}s",
                ctx.inflight, ctx.secs_since_last_progress
            )
        }
    } else if ctx.inflight == 0 && ctx.done > 0 {
        "IDLE: queue drained".to_string()
    } else {
        "HEALTHY".to_string()
    };
    StallAssessment {
        stalled,
        verdict,
        detection_mode,
    }
}

/// Best-effort explanation of why Attic operations may be currently throttled or degraded (§62).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhySlowDiagnostic {
    /// Short machine-readable code for the primary bottleneck.
    pub code: &'static str,
    /// Human-readable explanation suitable for developer status output.
    pub explanation: String,
}

/// Inputs evaluated to produce a deterministic "why_slow" diagnostic.
#[derive(Debug, Clone)]
pub struct DiagnosticContext {
    pub disk_emergency: bool,
    pub disk_warning: bool,
    pub resource_pressure_restricted: bool,
    pub available_ram_mib: u64,
    pub queue_depth: u64,
    /// The queue is large AND growing faster than it drains (not merely
    /// large: a 30k queue on a healthy GPU is normal work, not backpressure).
    pub queue_backpressure_active: bool,
    pub canonical_indexing_active: bool,
    pub semantic_inference_active: bool,
    pub model_loading_or_warmup: bool,
    pub mcp_high_latency: bool,
    pub user_caps_active: bool,
    /// Embedding is running on a GPU (dedicated or unified-memory), which is
    /// gated by its own host-RAM floor rather than the pressure tiers.
    pub gpu_embedding: bool,
    /// Available host RAM is below that GPU's floor, so GPU embedding is
    /// paused.
    pub host_ram_floor_breached: bool,
}

/// Determine the most critical reason why Attic is slow or throttled (§62).
pub fn diagnose_why_slow(ctx: &DiagnosticContext) -> WhySlowDiagnostic {
    if ctx.disk_emergency {
        WhySlowDiagnostic {
            code: "disk_pressure",
            explanation: "free disk space is below emergency reserve; semantic indexing is halted"
                .to_string(),
        }
    } else if ctx.gpu_embedding && ctx.host_ram_floor_breached {
        WhySlowDiagnostic {
            code: "host_ram_floor",
            explanation: "available host RAM is below the GPU embedding safety floor; GPU \
                          embedding is paused until memory is freed"
                .to_string(),
        }
    } else if ctx.resource_pressure_restricted {
        WhySlowDiagnostic {
            code: "resource_monitor_pressure",
            explanation: if ctx.gpu_embedding {
                "high host memory pressure; background indexing is throttled, while GPU \
                 embedding is exempt from the pressure tiers and continues (it pauses only \
                 below its host-RAM floor)"
                    .to_string()
            } else {
                "high host memory pressure; background indexing and embedding are throttled"
                    .to_string()
            },
        }
    } else if ctx.mcp_high_latency {
        WhySlowDiagnostic {
            code: "mcp_priority",
            explanation: "MCP interactive latency exceeds guard; prioritizing foreground responses"
                .to_string(),
        }
    } else if ctx.model_loading_or_warmup {
        WhySlowDiagnostic {
            code: "model_warmup",
            explanation: "model assets are loading and running initial cold warm-up pass"
                .to_string(),
        }
    } else if ctx.queue_backpressure_active {
        WhySlowDiagnostic {
            code: "semantic_queue_backpressure",
            explanation: format!(
                "the embedding queue ({} chunks) is growing faster than it drains",
                ctx.queue_depth
            ),
        }
    } else if ctx.canonical_indexing_active && ctx.semantic_inference_active {
        WhySlowDiagnostic {
            code: "canonical_indexing",
            explanation: "canonical file parsing and semantic embedding are actively sharing system resources".to_string(),
        }
    } else if ctx.semantic_inference_active {
        if ctx.gpu_embedding {
            WhySlowDiagnostic {
                code: "semantic_inference",
                explanation: format!(
                    "embedding {} queued chunks on the GPU; nothing is wrong, this is normal \
                     background work",
                    ctx.queue_depth
                ),
            }
        } else {
            WhySlowDiagnostic {
                code: "semantic_cpu_inference",
                explanation: format!(
                    "embedding {} queued chunks on allocated CPU threads",
                    ctx.queue_depth
                ),
            }
        }
    } else if ctx.available_ram_mib < 2048 {
        WhySlowDiagnostic {
            code: "memory_headroom",
            explanation: format!(
                "available host RAM is constrained ({} MiB available)",
                ctx.available_ram_mib
            ),
        }
    } else if ctx.user_caps_active {
        WhySlowDiagnostic {
            code: "advanced_user_cap",
            explanation: "concurrency or memory is bounded by explicit attic.toml user caps"
                .to_string(),
        }
    } else {
        WhySlowDiagnostic {
            code: "nominal",
            explanation: "system is operating within nominal resource parameters".to_string(),
        }
    }
}

/// Authoritative semantic query latency breakdown (Final Master Plan V2 §5.9, Checkpoint P9).
///
/// Encapsulates the entire production latency pipeline:
/// query prep + tokenization + Qwen embedding + vector search + filtering/ranking + handler overhead = total.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticLatencyBreakdown {
    pub query_prepare_ms: f64,
    pub tokenization_ms: f64,
    pub query_embedding_ms: f64,
    pub vector_search_ms: f64,
    pub filtering_ranking_ms: f64,
    pub handler_overhead_ms: f64,
    pub total_ms: f64,
}

impl SemanticLatencyBreakdown {
    pub fn new(
        query_prepare_ms: f64,
        tokenization_ms: f64,
        query_embedding_ms: f64,
        vector_search_ms: f64,
        filtering_ranking_ms: f64,
        handler_overhead_ms: f64,
    ) -> Self {
        let total_ms = query_prepare_ms
            + tokenization_ms
            + query_embedding_ms
            + vector_search_ms
            + filtering_ranking_ms
            + handler_overhead_ms;
        Self {
            query_prepare_ms,
            tokenization_ms,
            query_embedding_ms,
            vector_search_ms,
            filtering_ranking_ms,
            handler_overhead_ms,
            total_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_snapshot_computes_eta_and_rates() {
        let snapshot = SemanticProgressSnapshot::compute(
            500,
            20,
            1000,
            5,
            50.0,  // 50 chunks / sec
            200.0, // 200 ms batch latency
            Some(1),
            Some(2),
            "Present",
        );

        assert_eq!(snapshot.total_queue_depth, 520);
        assert!((snapshot.batches_per_sec - 5.0).abs() < 1e-3);
        // ETA: 520 / 50 = 10.4 -> ceil = 11 seconds
        assert_eq!(snapshot.eta_seconds, Some(11));
        // A computable ETA needs no excuse.
        assert_eq!(snapshot.eta_unavailable_reason, None);
        assert_eq!(snapshot.active_generation_id, Some(1));
        assert_eq!(snapshot.building_generation_id, Some(2));
    }

    /// Regression: a blocked queue must explain itself.
    ///
    /// During a long throttle the status output reported
    /// `eta_seconds: null` for twelve consecutive minutes with nothing
    /// beside it, which is indistinguishable from "just started" or "hung".
    #[test]
    fn blocked_queue_explains_why_it_has_no_eta() {
        // 20 chunks queued, nothing dispatched, zero throughput — exactly
        // the observed stall shape.
        let stalled =
            SemanticProgressSnapshot::compute(20, 0, 0, 0, 0.0, 0.0, Some(1), None, "hot");
        assert_eq!(stalled.eta_seconds, None);
        let reason = stalled
            .eta_unavailable_reason
            .expect("a null ETA must always carry a reason");
        assert!(
            reason.contains("20"),
            "reason should cite queue depth: {reason}"
        );
        assert!(
            reason.contains("why_slow"),
            "reason should point at the bottleneck diagnostic: {reason}"
        );

        // Same, but with work claimed and no completions yet.
        let inflight =
            SemanticProgressSnapshot::compute(12, 8, 0, 0, 0.0, 0.0, Some(1), None, "loading");
        assert_eq!(inflight.eta_seconds, None);
        assert!(
            inflight
                .eta_unavailable_reason
                .as_deref()
                .is_some_and(|r| r.contains("in flight")),
            "an inflight stall must be described differently from an undispatched one"
        );

        // A drained queue is a real answer, not a missing one.
        let drained =
            SemanticProgressSnapshot::compute(0, 0, 45, 0, 0.0, 0.0, Some(1), None, "hot");
        assert_eq!(drained.eta_seconds, Some(0));
        assert_eq!(drained.eta_unavailable_reason, None);
    }

    #[test]
    fn diagnose_why_slow_prioritizes_safety_and_pressure() {
        // Disk emergency is highest priority
        let ctx = DiagnosticContext {
            disk_emergency: true,
            disk_warning: true,
            resource_pressure_restricted: true,
            available_ram_mib: 1024,
            queue_depth: 6000,
            queue_backpressure_active: true,
            canonical_indexing_active: true,
            semantic_inference_active: true,
            model_loading_or_warmup: false,
            mcp_high_latency: true,
            user_caps_active: false,
            gpu_embedding: false,
            host_ram_floor_breached: false,
        };
        assert_eq!(diagnose_why_slow(&ctx).code, "disk_pressure");

        // Without disk emergency, resource pressure dominates
        let mut ctx2 = ctx.clone();
        ctx2.disk_emergency = false;
        assert_eq!(diagnose_why_slow(&ctx2).code, "resource_monitor_pressure");
        assert!(
            diagnose_why_slow(&ctx2)
                .explanation
                .contains("embedding are throttled")
        );

        // A GPU is exempt from the tiers, and the report says so.
        let mut gpu = ctx2.clone();
        gpu.gpu_embedding = true;
        let d = diagnose_why_slow(&gpu);
        assert_eq!(d.code, "resource_monitor_pressure");
        assert!(d.explanation.contains("exempt"), "{}", d.explanation);

        // Only the hard host floor pauses dedicated-GPU embedding.
        gpu.host_ram_floor_breached = true;
        assert_eq!(diagnose_why_slow(&gpu).code, "host_ram_floor");

        // A large queue being worked through on a healthy GPU is normal
        // work, reported as such — not as an error-sounding backpressure.
        let working = DiagnosticContext {
            disk_emergency: false,
            disk_warning: false,
            resource_pressure_restricted: false,
            available_ram_mib: 8192,
            queue_depth: 23_055,
            queue_backpressure_active: false,
            canonical_indexing_active: false,
            semantic_inference_active: true,
            model_loading_or_warmup: false,
            mcp_high_latency: false,
            user_caps_active: false,
            gpu_embedding: true,
            host_ram_floor_breached: false,
        };
        let d = diagnose_why_slow(&working);
        assert_eq!(d.code, "semantic_inference");
        assert!(
            d.explanation.contains("23055 queued chunks on the GPU"),
            "{}",
            d.explanation
        );
        let cpu = DiagnosticContext {
            gpu_embedding: false,
            ..working.clone()
        };
        assert_eq!(diagnose_why_slow(&cpu).code, "semantic_cpu_inference");

        // Normal state
        let nominal_ctx = DiagnosticContext {
            disk_emergency: false,
            disk_warning: false,
            resource_pressure_restricted: false,
            available_ram_mib: 8192,
            queue_depth: 0,
            queue_backpressure_active: false,
            canonical_indexing_active: false,
            semantic_inference_active: false,
            model_loading_or_warmup: false,
            mcp_high_latency: false,
            user_caps_active: false,
            gpu_embedding: false,
            host_ram_floor_breached: false,
        };
        assert_eq!(diagnose_why_slow(&nominal_ctx).code, "nominal");
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;

    fn stall_ctx(
        inflight: u64,
        done: u64,
        chunks_per_sec: f64,
        secs_since_last_progress: u64,
    ) -> StallContext {
        StallContext {
            queue_depth: inflight,
            inflight,
            done,
            chunks_per_sec,
            secs_since_last_progress,
            loading_or_warmup: false,
            provider_backoff: None,
            progress_heartbeat_supported: true,
        }
    }

    #[test]
    fn inflight_with_zero_throughput_past_threshold_is_stalled() {
        // The exact 2026-09 incident signature: 16 in-flight, 0 done, 20 min.
        let a = assess_stall(&stall_ctx(16, 0, 0.0, 1209));
        assert!(a.stalled);
        assert!(a.verdict.contains("STALLED"));
        assert_eq!(a.detection_mode, "progress_watchdog");
    }

    #[test]
    fn stall_inside_the_supervisor_deadline_does_not_demand_a_manual_restart() {
        // Regression: an operator polling at 245s was told "inference worker
        // is hung; restart the embedding worker" while the supervisor was
        // still 55s away from killing and restarting it automatically. The
        // stall is real, but the prescribed action was wrong.
        let a = assess_stall(&stall_ctx(8, 0, 0.0, 245));
        assert!(a.stalled, "245s with 8 in-flight is still a stall");
        assert!(
            !a.verdict.contains("restart the embedding worker"),
            "must not demand manual intervention while auto-recovery is pending: {}",
            a.verdict
        );
        assert!(
            a.verdict.contains("automatic"),
            "must say recovery is automatic: {}",
            a.verdict
        );
        assert!(
            a.verdict.contains("55s"),
            "must state the remaining time to recovery: {}",
            a.verdict
        );
    }

    #[test]
    fn stall_past_the_supervisor_deadline_does_demand_a_manual_restart() {
        // Past the kill deadline with no restart, the supervisor itself has
        // failed — this is the only case where manual action is correct.
        let a = assess_stall(&stall_ctx(8, 0, 0.0, EMBED_DEADLINE_SECS + 1));
        assert!(a.stalled);
        assert!(
            a.verdict.contains("restart the embedding worker"),
            "a genuinely wedged worker must ask for a restart: {}",
            a.verdict
        );
        assert!(a.verdict.contains("genuinely wedged"));
    }

    #[test]
    fn stall_diagnostics_quote_the_deadline_the_supervisor_enforces() {
        // The verdict cites EMBED_DEADLINE_SECS; worker_supervisor derives its
        // kill deadline from the same constant. If someone re-hardcodes one of
        // them, the advice silently becomes wrong again.
        assert_eq!(
            crate::worker_supervisor::embed_deadline().as_secs(),
            EMBED_DEADLINE_SECS,
            "supervisor kill deadline must match the one reported to operators"
        );
    }

    #[test]
    fn first_batch_within_threshold_is_slow_not_stalled() {
        let a = assess_stall(&stall_ctx(16, 0, 0.0, 45));
        assert!(!a.stalled);
        assert!(a.verdict.contains("SLOW"));
    }

    #[test]
    fn completing_batches_is_healthy() {
        let a = assess_stall(&stall_ctx(16, 500, 12.5, 3));
        assert!(!a.stalled);
        assert_eq!(a.verdict, "HEALTHY");
    }

    #[test]
    fn drained_queue_is_idle() {
        let a = assess_stall(&stall_ctx(0, 200, 0.0, 9999));
        assert!(!a.stalled);
        assert!(a.verdict.contains("IDLE"));
    }

    #[test]
    fn deadline_only_backends_do_not_call_a_long_first_batch_stalled_early() {
        let mut ctx = stall_ctx(8, 0, 0.0, 245);
        ctx.progress_heartbeat_supported = false;
        let a = assess_stall(&ctx);
        assert!(
            !a.stalled,
            "deadline-only backends wait for the hard deadline"
        );
        assert!(
            a.verdict.contains("deadline-based"),
            "deadline-only wording must be explicit: {}",
            a.verdict
        );
        assert_eq!(a.detection_mode, "deadline_only");
    }

    #[test]
    fn loading_and_backoff_are_reported_distinctly_from_stalls() {
        let mut loading = stall_ctx(0, 0, 0.0, 400);
        loading.queue_depth = 12;
        loading.loading_or_warmup = true;
        let a = assess_stall(&loading);
        assert!(!a.stalled);
        assert!(a.verdict.contains("LOADING/WARMING"), "{}", a.verdict);

        let mut backoff = stall_ctx(0, 0, 0.0, 400);
        backoff.queue_depth = 12;
        backoff.provider_backoff = Some(ProviderBackoffStatus {
            consecutive_failures: 3,
            last_error: "failed to load weights".into(),
            next_retry_unix_ms: 1234,
        });
        let a = assess_stall(&backoff);
        assert!(!a.stalled);
        assert!(a.verdict.contains("BACKING OFF"), "{}", a.verdict);
        assert!(
            a.verdict.contains("failed to load weights"),
            "{}",
            a.verdict
        );
    }

    #[test]
    fn fresh_queue_work_resets_the_stall_clock_after_a_long_idle() {
        let idle_clock = observe_work_availability(WorkAvailabilityClock::default(), 1_000, 0, 0);
        assert_eq!(idle_clock.active_since_secs, None);

        let active = observe_work_availability(idle_clock, 1_360, 9, 0);
        assert_eq!(active.active_since_secs, Some(1_360));
        assert_eq!(
            secs_since_meaningful_progress(active, 1_360, Some(360)),
            Some(0),
            "the first status sample after new work arrives must not inherit an old commit clock"
        );
    }

    #[test]
    fn provider_backoff_snapshot_round_trips() {
        clear_provider_backoff();
        note_provider_backoff(2, 1_500, "provider unavailable");
        let snap = provider_backoff_snapshot().expect("snapshot");
        assert_eq!(snap.consecutive_failures, 2);
        assert_eq!(snap.last_error, "provider unavailable");
        assert!(snap.next_retry_unix_ms >= unix_ms_now());
        clear_provider_backoff();
        assert!(provider_backoff_snapshot().is_none());
    }
}
