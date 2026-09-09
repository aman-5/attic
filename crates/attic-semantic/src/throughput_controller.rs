//! Throughput Controller and Startup Warm-up (Final Master Plan V2 §22, §23, §24, CP16).
//!
//! Provides bounded throughput optimization for semantic embedding:
//! 1. Cold vs steady-state separation (startup warm-up, conservative initial concurrency).
//! 2. Hill-climbing candidate exploration: apply candidate -> stabilize -> measure -> retain or rollback.
//! 3. Thermal throttling and diminishing returns detection: rolling back when increased resource allocation
//!    fails to yield meaningful throughput gain (`min_meaningful_gain_ratio`).
//! 4. Interactive MCP latency guard: immediate rollback when interactive MCP latency exceeds threshold.
//! 5. Cooldown and bounded exploration frequency: prevents continuous experimentation.

use std::time::{Duration, Instant};
use crate::error::SemanticError;
use crate::provider::{EmbeddingExecutionBudget, EmbeddingProvider};

/// Configuration knobs for the bounded throughput controller.
#[derive(Debug, Clone)]
pub struct ThroughputControllerConfig {
    /// Minimum relative throughput gain required to retain an upscale (e.g. 0.05 = 5%).
    pub min_meaningful_gain_ratio: f64,
    /// Maximum acceptable MCP interactive query latency in milliseconds before triggering rollback.
    pub mcp_latency_guard_ms: f64,
    /// Mandatory cooldown period between exploration attempts.
    pub cooldown_duration: Duration,
    /// Number of batch completion samples to discard during stabilization after an allocation change.
    pub stabilization_samples: usize,
    /// Number of batch completion samples to measure before evaluating candidate throughput.
    pub evaluation_window_samples: usize,
}

impl Default for ThroughputControllerConfig {
    fn default() -> Self {
        Self {
            min_meaningful_gain_ratio: 0.05, // 5% minimum gain
            mcp_latency_guard_ms: 100.0,     // 100ms MCP latency threshold
            cooldown_duration: Duration::from_secs(30),
            stabilization_samples: 2,
            evaluation_window_samples: 5,
        }
    }
}

/// Resource allocation profile evaluated by the throughput controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateAllocation {
    pub batch_size: usize,
    pub inference_lanes: usize,
    pub cpu_threads: usize,
}

impl Default for CandidateAllocation {
    fn default() -> Self {
        Self {
            batch_size: 16,
            inference_lanes: 1,
            cpu_threads: 2,
        }
    }
}

/// Lifecycle phases of the throughput controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerPhase {
    /// Initial cold phase: model load and dummy warm-up pass.
    StartupWarmup,
    /// Running stably under verified baseline allocation.
    SteadyState,
    /// Candidate applied; ignoring initial transient samples for stabilization.
    Stabilizing,
    /// Accumulating throughput and latency metrics for the active candidate.
    Evaluating,
    /// Cooldown window where further exploration is paused to prevent continuous flapping.
    Cooldown,
}

/// Decisions emitted by the controller after observing batch metrics.
#[derive(Debug, Clone, PartialEq)]
pub enum ControllerAction {
    /// No change; continue current phase.
    None,
    /// Propose a new candidate allocation for exploration.
    ProposeCandidate(CandidateAllocation),
    /// Candidate demonstrated meaningful throughput gain without latency degradation; retain as new baseline.
    RetainCandidate {
        allocation: CandidateAllocation,
        chunks_per_sec: f64,
    },
    /// Candidate failed validation (diminishing return, thermal throttle, or MCP latency spike); rollback.
    Rollback {
        rollback_to: CandidateAllocation,
        reason: String,
    },
}

/// Bounded throughput controller managing semantic execution efficiency.
#[derive(Debug)]
pub struct ThroughputController {
    config: ThroughputControllerConfig,
    phase: ControllerPhase,
    baseline_allocation: CandidateAllocation,
    candidate_allocation: Option<CandidateAllocation>,
    baseline_throughput: f64,
    phase_entered_at: Instant,
    sample_count: usize,
    total_chunks: usize,
    total_duration: Duration,
}

impl ThroughputController {
    /// Create a new controller with conservative initial allocation.
    pub fn new(config: ThroughputControllerConfig, initial_allocation: CandidateAllocation) -> Self {
        Self {
            config,
            phase: ControllerPhase::StartupWarmup,
            baseline_allocation: initial_allocation,
            candidate_allocation: None,
            baseline_throughput: 0.0,
            phase_entered_at: Instant::now(),
            sample_count: 0,
            total_chunks: 0,
            total_duration: Duration::ZERO,
        }
    }

    /// Current controller phase.
    pub fn phase(&self) -> ControllerPhase {
        self.phase
    }

    /// Current accepted baseline allocation.
    pub fn baseline_allocation(&self) -> CandidateAllocation {
        self.baseline_allocation
    }

    /// Current baseline throughput in chunks per second.
    pub fn baseline_throughput(&self) -> f64 {
        self.baseline_throughput
    }

    /// Perform warm-up on the embedding provider under a conservative execution budget.
    pub fn warm_up_provider(
        &mut self,
        provider: &dyn EmbeddingProvider,
    ) -> Result<(), SemanticError> {
        let budget = EmbeddingExecutionBudget {
            cpu_threads: self.baseline_allocation.cpu_threads.min(2),
            max_batch_size: 1,
            deadline: Some(Instant::now() + Duration::from_secs(5)),
        };
        provider.warm_up(&budget)?;
        self.phase = ControllerPhase::SteadyState;
        self.phase_entered_at = Instant::now();
        Ok(())
    }

    /// Request exploration of a candidate upscale allocation.
    ///
    /// Rejected if currently in cooldown, stabilizing, or evaluating.
    pub fn try_explore(
        &mut self,
        candidate: CandidateAllocation,
    ) -> Result<ControllerAction, &'static str> {
        if self.phase == ControllerPhase::StartupWarmup {
            return Err("controller is still in startup warm-up");
        }
        if self.phase == ControllerPhase::Cooldown {
            if self.phase_entered_at.elapsed() < self.config.cooldown_duration {
                return Err("controller is in cooldown; exploration paused");
            }
            self.phase = ControllerPhase::SteadyState;
        }
        if self.phase != ControllerPhase::SteadyState {
            return Err("cannot explore while another candidate is active");
        }

        self.candidate_allocation = Some(candidate);
        self.phase = ControllerPhase::Stabilizing;
        self.phase_entered_at = Instant::now();
        self.sample_count = 0;
        self.total_chunks = 0;
        self.total_duration = Duration::ZERO;

        Ok(ControllerAction::ProposeCandidate(candidate))
    }

    /// Record metrics from a completed inference batch.
    pub fn record_batch(
        &mut self,
        chunks: usize,
        duration: Duration,
        mcp_latency_ms: Option<f64>,
    ) -> ControllerAction {
        // 1. Guard against MCP interactive latency degradation (§24)
        if let Some(mcp_lat) = mcp_latency_ms
            && mcp_lat > self.config.mcp_latency_guard_ms
            && let Some(candidate) = self.candidate_allocation.take()
        {
            let rollback_alloc = self.baseline_allocation;
            self.enter_cooldown();
            return ControllerAction::Rollback {
                rollback_to: rollback_alloc,
                reason: format!(
                    "MCP latency guard breached: {mcp_lat:.1}ms > {:.1}ms with allocation {:?}",
                    self.config.mcp_latency_guard_ms, candidate
                ),
            };
        }

        match self.phase {
            ControllerPhase::StartupWarmup => {
                // Ignore samples during initial warmup
                ControllerAction::None
            }
            ControllerPhase::Cooldown => {
                if self.phase_entered_at.elapsed() >= self.config.cooldown_duration {
                    self.phase = ControllerPhase::SteadyState;
                }
                ControllerAction::None
            }
            ControllerPhase::SteadyState => {
                // Track baseline throughput continuously
                if duration.as_secs_f64() > 0.0 {
                    let rate = chunks as f64 / duration.as_secs_f64();
                    if self.baseline_throughput == 0.0 {
                        self.baseline_throughput = rate;
                    } else {
                        // Exponential moving average for baseline
                        self.baseline_throughput = (self.baseline_throughput * 0.8) + (rate * 0.2);
                    }
                }
                ControllerAction::None
            }
            ControllerPhase::Stabilizing => {
                self.sample_count += 1;
                if self.sample_count >= self.config.stabilization_samples {
                    self.phase = ControllerPhase::Evaluating;
                    self.sample_count = 0;
                    self.total_chunks = 0;
                    self.total_duration = Duration::ZERO;
                }
                ControllerAction::None
            }
            ControllerPhase::Evaluating => {
                self.sample_count += 1;
                self.total_chunks += chunks;
                self.total_duration += duration;

                if self.sample_count >= self.config.evaluation_window_samples {
                    let measured_tput = if self.total_duration.as_secs_f64() > 0.0 {
                        self.total_chunks as f64 / self.total_duration.as_secs_f64()
                    } else {
                        0.0
                    };

                    let candidate = self.candidate_allocation.take().unwrap_or(self.baseline_allocation);

                    // Evaluate gain relative to baseline (§22, §24)
                    let min_required = self.baseline_throughput * (1.0 + self.config.min_meaningful_gain_ratio);

                    if measured_tput >= min_required || self.baseline_throughput == 0.0 {
                        // Meaningful gain achieved; retain candidate!
                        self.baseline_allocation = candidate;
                        self.baseline_throughput = measured_tput;
                        self.enter_cooldown();
                        ControllerAction::RetainCandidate {
                            allocation: candidate,
                            chunks_per_sec: measured_tput,
                        }
                    } else {
                        // Diminishing returns, oversubscription, or thermal throttling (§22)
                        let rollback_alloc = self.baseline_allocation;
                        self.enter_cooldown();
                        ControllerAction::Rollback {
                            rollback_to: rollback_alloc,
                            reason: format!(
                                "diminishing throughput: measured {measured_tput:.1} chunks/sec < required {min_required:.1} chunks/sec (baseline: {:.1})",
                                self.baseline_throughput
                            ),
                        }
                    }
                } else {
                    ControllerAction::None
                }
            }
        }
    }

    fn enter_cooldown(&mut self) {
        self.phase = ControllerPhase::Cooldown;
        self.phase_entered_at = Instant::now();
        self.candidate_allocation = None;
        self.sample_count = 0;
        self.total_chunks = 0;
        self.total_duration = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_warmup_transition_to_steady_state() {
        let mut ctrl = ThroughputController::new(
            ThroughputControllerConfig::default(),
            CandidateAllocation {
                batch_size: 16,
                inference_lanes: 1,
                cpu_threads: 2,
            },
        );
        assert_eq!(ctrl.phase(), ControllerPhase::StartupWarmup);

        // Dummy provider simulating warm_up
        struct MockProvider;
        impl EmbeddingProvider for MockProvider {
            fn model_fingerprint(&self) -> crate::provider::EmbeddingFingerprint {
                unimplemented!()
            }
            fn dimension(&self) -> usize {
                384
            }
            fn warm_up(&self, _budget: &EmbeddingExecutionBudget) -> Result<(), SemanticError> {
                Ok(())
            }
            fn embed_documents(
                &self,
                _inputs: &[crate::provider::EmbeddingInput],
                _budget: &EmbeddingExecutionBudget,
            ) -> Result<Vec<crate::provider::EmbeddingOutput>, SemanticError> {
                Ok(Vec::new())
            }
            fn embed_query(
                &self,
                _query: &str,
                _budget: &EmbeddingExecutionBudget,
            ) -> Result<Vec<f32>, SemanticError> {
                Ok(Vec::new())
            }
        }

        let mock = MockProvider;
        ctrl.warm_up_provider(&mock).unwrap();
        assert_eq!(ctrl.phase(), ControllerPhase::SteadyState);
    }

    #[test]
    fn hill_climb_retains_candidate_on_meaningful_gain() {
        let mut ctrl = ThroughputController::new(
            ThroughputControllerConfig {
                min_meaningful_gain_ratio: 0.10, // 10% gain required
                cooldown_duration: Duration::from_millis(50),
                stabilization_samples: 1,
                evaluation_window_samples: 2,
                ..Default::default()
            },
            CandidateAllocation {
                batch_size: 16,
                inference_lanes: 1,
                cpu_threads: 2,
            },
        );
        ctrl.phase = ControllerPhase::SteadyState;
        // Establish baseline: 100 chunks / sec
        ctrl.record_batch(100, Duration::from_secs(1), None);
        assert!((ctrl.baseline_throughput() - 100.0).abs() < 1e-3);

        // Try exploration to batch 32
        let candidate = CandidateAllocation {
            batch_size: 32,
            inference_lanes: 2,
            cpu_threads: 4,
        };
        let action = ctrl.try_explore(candidate).unwrap();
        assert_eq!(action, ControllerAction::ProposeCandidate(candidate));
        assert_eq!(ctrl.phase(), ControllerPhase::Stabilizing);

        // 1 stabilization batch (discarded)
        assert_eq!(ctrl.record_batch(32, Duration::from_millis(200), None), ControllerAction::None);
        assert_eq!(ctrl.phase(), ControllerPhase::Evaluating);

        // 2 evaluation batches with superior throughput (e.g. 150 chunks/sec, > 110 required)
        assert_eq!(ctrl.record_batch(30, Duration::from_millis(200), None), ControllerAction::None);
        let final_action = ctrl.record_batch(30, Duration::from_millis(200), None);

        match final_action {
            ControllerAction::RetainCandidate { allocation, chunks_per_sec } => {
                assert_eq!(allocation, candidate);
                assert!(chunks_per_sec >= 140.0);
            }
            other => panic!("expected RetainCandidate, got {other:?}"),
        }
        assert_eq!(ctrl.baseline_allocation(), candidate);
        assert_eq!(ctrl.phase(), ControllerPhase::Cooldown);
    }

    #[test]
    fn diminishing_returns_triggers_rollback() {
        let mut ctrl = ThroughputController::new(
            ThroughputControllerConfig {
                min_meaningful_gain_ratio: 0.10, // 10% gain required
                cooldown_duration: Duration::from_millis(50),
                stabilization_samples: 1,
                evaluation_window_samples: 2,
                ..Default::default()
            },
            CandidateAllocation {
                batch_size: 16,
                inference_lanes: 1,
                cpu_threads: 2,
            },
        );
        ctrl.phase = ControllerPhase::SteadyState;
        // Baseline: 100 chunks / sec
        ctrl.record_batch(100, Duration::from_secs(1), None);

        // Explore candidate with higher threads/lanes
        let candidate = CandidateAllocation {
            batch_size: 32,
            inference_lanes: 4,
            cpu_threads: 8,
        };
        ctrl.try_explore(candidate).unwrap();

        // Stabilization sample
        ctrl.record_batch(32, Duration::from_millis(500), None);

        // 2 evaluation samples showing flat or lower throughput (e.g. 90 chunks/sec due to contention)
        ctrl.record_batch(45, Duration::from_millis(500), None);
        let final_action = ctrl.record_batch(45, Duration::from_millis(500), None);

        match final_action {
            ControllerAction::Rollback { rollback_to, reason } => {
                assert_eq!(rollback_to, CandidateAllocation {
                    batch_size: 16,
                    inference_lanes: 1,
                    cpu_threads: 2,
                });
                assert!(reason.contains("diminishing throughput"));
            }
            other => panic!("expected Rollback, got {other:?}"),
        }
        assert_eq!(ctrl.phase(), ControllerPhase::Cooldown);
    }

    #[test]
    fn mcp_latency_spike_triggers_immediate_rollback() {
        let mut ctrl = ThroughputController::new(
            ThroughputControllerConfig {
                mcp_latency_guard_ms: 50.0,
                ..Default::default()
            },
            CandidateAllocation {
                batch_size: 16,
                inference_lanes: 1,
                cpu_threads: 2,
            },
        );
        ctrl.phase = ControllerPhase::SteadyState;
        let candidate = CandidateAllocation {
            batch_size: 64,
            inference_lanes: 4,
            cpu_threads: 8,
        };
        ctrl.try_explore(candidate).unwrap();

        // High MCP latency observed (120ms > 50ms)
        let action = ctrl.record_batch(64, Duration::from_millis(200), Some(120.0));
        match action {
            ControllerAction::Rollback { rollback_to, reason } => {
                assert_eq!(rollback_to.batch_size, 16);
                assert!(reason.contains("MCP latency guard breached"));
            }
            other => panic!("expected immediate Rollback, got {other:?}"),
        }
    }
}
