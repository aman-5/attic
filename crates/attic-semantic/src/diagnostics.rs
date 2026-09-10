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
            active_generation_id: active_gen,
            building_generation_id: building_gen,
            model_cache_state: model_cache_state.into(),
        }
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

    /// Evaluates if total end-to-end latency meets SLA (never derived only from vector_search_ms).
    pub fn is_within_sla(&self, sla_ms: f64) -> bool {
        self.total_ms <= sla_ms
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
        assert_eq!(snapshot.active_generation_id, Some(1));
        assert_eq!(snapshot.building_generation_id, Some(2));
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
