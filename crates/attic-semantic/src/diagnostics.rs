//! User-Visible Semantic Progress, ETA, and "Why Slow" Diagnostics (Final Master Plan V2 §61, §62, CP18).
//!
//! Provides structured observability for semantic embedding and machine resource allocation:
//! - Detailed queue progress: pending, inflight, failed, and done counts.
//! - Throughput rates: chunks/sec, batches/sec, batch latency, and ETA.
//! - Resource mode and active generation state.
//! - Best-effort "why_slow" diagnostic explanation (§62).

use serde::{Deserialize, Serialize};

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

/// Stall detection (Phase 5): flags the exact failure mode observed in
/// production — a batch claimed INFLIGHT but producing zero completions for
/// longer than the threshold. Pure function over observable counters so it
/// is testable without a live queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StallAssessment {
    pub stalled: bool,
    /// Human-readable verdict for status output.
    pub verdict: String,
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

pub fn assess_stall(
    inflight: u64,
    done: u64,
    chunks_per_sec: f64,
    secs_since_last_completed_batch: u64,
) -> StallAssessment {
    let stalled = inflight > 0
        && chunks_per_sec < 0.01
        && secs_since_last_completed_batch > STALL_THRESHOLD_SECS;
    let verdict = if stalled {
        // Past the detection threshold but still inside the supervisor's kill
        // deadline, recovery is already scheduled and automatic. Telling an
        // operator to "restart the embedding worker" here is wrong advice: it
        // reads as "this is dead", when in fact the worker gets killed and
        // restarted without intervention. Only once the deadline has passed
        // without a restart is something genuinely wedged.
        if secs_since_last_completed_batch <= EMBED_DEADLINE_SECS {
            let secs_to_recovery = EMBED_DEADLINE_SECS - secs_since_last_completed_batch;
            format!(
                "STALLED: {inflight} items in-flight, 0 completed batches for \
                 {secs_since_last_completed_batch}s (threshold {STALL_THRESHOLD_SECS}s) — the \
                 supervisor kills and restarts a hung worker at {EMBED_DEADLINE_SECS}s, so \
                 recovery is automatic in ~{secs_to_recovery}s; no action needed yet"
            )
        } else {
            format!(
                "STALLED: {inflight} items in-flight, 0 completed batches for \
                 {secs_since_last_completed_batch}s — past the {EMBED_DEADLINE_SECS}s supervisor \
                 deadline without a restart, so the worker is genuinely wedged; restart the \
                 embedding worker"
            )
        }
    } else if inflight > 0 && chunks_per_sec < 0.01 {
        format!(
            "SLOW: {inflight} items in-flight, no batch completed yet ({secs_since_last_completed_batch}s) — within tolerance, first batch may still be running"
        )
    } else if inflight == 0 && done > 0 {
        "IDLE: queue drained".to_string()
    } else {
        "HEALTHY".to_string()
    };
    StallAssessment { stalled, verdict }
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
    pub queue_backpressure_active: bool,
    pub canonical_indexing_active: bool,
    pub semantic_inference_active: bool,
    pub model_loading_or_warmup: bool,
    pub mcp_high_latency: bool,
    pub user_caps_active: bool,
}

/// Determine the most critical reason why Attic is slow or throttled (§62).
pub fn diagnose_why_slow(ctx: &DiagnosticContext) -> WhySlowDiagnostic {
    if ctx.disk_emergency {
        WhySlowDiagnostic {
            code: "disk_pressure",
            explanation: "free disk space is below emergency reserve; semantic indexing is halted"
                .to_string(),
        }
    } else if ctx.resource_pressure_restricted {
        WhySlowDiagnostic {
            code: "resource_monitor_pressure",
            explanation: "high host memory or CPU pressure; background work is throttled"
                .to_string(),
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
                "enrichment queue depth ({}) exceeded high watermark",
                ctx.queue_depth
            ),
        }
    } else if ctx.canonical_indexing_active && ctx.semantic_inference_active {
        WhySlowDiagnostic {
            code: "canonical_indexing",
            explanation: "canonical file parsing and semantic embedding are actively sharing system resources".to_string(),
        }
    } else if ctx.semantic_inference_active {
        WhySlowDiagnostic {
            code: "semantic_cpu_inference",
            explanation:
                "neural embedding inference is actively executing on allocated CPU threads"
                    .to_string(),
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
        };
        assert_eq!(diagnose_why_slow(&ctx).code, "disk_pressure");

        // Without disk emergency, resource pressure dominates
        let mut ctx2 = ctx.clone();
        ctx2.disk_emergency = false;
        assert_eq!(diagnose_why_slow(&ctx2).code, "resource_monitor_pressure");

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
        };
        assert_eq!(diagnose_why_slow(&nominal_ctx).code, "nominal");
    }
}

#[cfg(test)]
mod stall_tests {
    use super::*;

    #[test]
    fn inflight_with_zero_throughput_past_threshold_is_stalled() {
        // The exact 2026-09 incident signature: 16 in-flight, 0 done, 20 min.
        let a = assess_stall(16, 0, 0.0, 1209);
        assert!(a.stalled);
        assert!(a.verdict.contains("STALLED"));
    }

    #[test]
    fn stall_inside_the_supervisor_deadline_does_not_demand_a_manual_restart() {
        // Regression: an operator polling at 245s was told "inference worker
        // is hung; restart the embedding worker" while the supervisor was
        // still 55s away from killing and restarting it automatically. The
        // stall is real, but the prescribed action was wrong.
        let a = assess_stall(8, 0, 0.0, 245);
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
        let a = assess_stall(8, 0, 0.0, EMBED_DEADLINE_SECS + 1);
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
        let a = assess_stall(16, 0, 0.0, 45);
        assert!(!a.stalled);
        assert!(a.verdict.contains("SLOW"));
    }

    #[test]
    fn completing_batches_is_healthy() {
        let a = assess_stall(16, 500, 12.5, 3);
        assert!(!a.stalled);
        assert_eq!(a.verdict, "HEALTHY");
    }

    #[test]
    fn drained_queue_is_idle() {
        let a = assess_stall(0, 200, 0.0, 9999);
        assert!(!a.stalled);
        assert!(a.verdict.contains("IDLE"));
    }
}
