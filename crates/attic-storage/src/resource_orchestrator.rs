//! Global Resource Orchestrator (Final Master Plan V2 §2.1, §3, §12, §15).
//!
//! The single normal resource authority in Attic, proactively arbitrating capacity
//! between canonical indexing, semantic inference, and interactive MCP traffic based on
//! live machine state, workload queues, and user intent policies.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use attic_core::{
    MachineSnapshot, ModePolicy, PowerSource, ResourceAllocation, ResourceModeSetting,
    ResourcePressure, WorkloadSnapshot,
};

use crate::machine_telemetry::MachineTelemetry;

/// Minimum time Auto mode must dwell in a state before transitioning up, preventing flapping.
const AUTO_DWELL_MIN_DURATION: Duration = Duration::from_secs(10);

/// Internal state for the Auto mode dynamic state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoModeState {
    /// Conservative operation (matches Low policy).
    Conservative,
    /// Moderate operation (matches Balanced policy).
    Moderate,
    /// High-throughput operation (matches Performance policy).
    Aggressive,
}

impl AutoModeState {
    /// Resolve to the active ModePolicy for this auto state.
    pub fn to_policy(self) -> ModePolicy {
        match self {
            Self::Conservative => ModePolicy::low(),
            Self::Moderate => ModePolicy::balanced(),
            Self::Aggressive => ModePolicy::performance(),
        }
    }
}

/// Dynamic state machine managing Auto mode transitions with hysteresis.
pub struct AutoStateMachine {
    current_state: AutoModeState,
    state_entered_at: Instant,
}

impl AutoStateMachine {
    /// Create a new state machine starting in Moderate state.
    pub fn new() -> Self {
        Self {
            current_state: AutoModeState::Moderate,
            state_entered_at: Instant::now(),
        }
    }

    /// Update state machine based on live physical machine conditions and pressure.
    pub fn update(
        &mut self,
        machine: &MachineSnapshot,
        pressure: ResourcePressure,
    ) -> AutoModeState {
        let now = Instant::now();
        let dwell = now.duration_since(self.state_entered_at);

        // Immediate fast downscale conditions
        let should_conserve = machine.available_memory_mib < 1536
            || machine.available_cpu_fraction < 0.15
            || matches!(machine.power_source, Some(PowerSource::Battery))
            || matches!(
                pressure,
                ResourcePressure::Critical | ResourcePressure::Emergency
            );

        if should_conserve {
            if self.current_state != AutoModeState::Conservative {
                self.current_state = AutoModeState::Conservative;
                self.state_entered_at = now;
            }
            return self.current_state;
        }

        // Candidates for aggressive scale-up (must dwell before upgrading)
        let can_be_aggressive = machine.available_memory_mib >= 6144
            && machine.available_cpu_fraction >= 0.45
            && !matches!(machine.power_source, Some(PowerSource::Battery))
            && matches!(pressure, ResourcePressure::Normal);

        if can_be_aggressive {
            if self.current_state == AutoModeState::Conservative && dwell >= AUTO_DWELL_MIN_DURATION
            {
                self.current_state = AutoModeState::Moderate;
                self.state_entered_at = now;
            } else if self.current_state == AutoModeState::Moderate
                && dwell >= AUTO_DWELL_MIN_DURATION
            {
                self.current_state = AutoModeState::Aggressive;
                self.state_entered_at = now;
            }
        } else if self.current_state == AutoModeState::Aggressive {
            // Drop back to Moderate if aggressive conditions no longer met
            self.current_state = AutoModeState::Moderate;
            self.state_entered_at = now;
        }

        self.current_state
    }

    /// Return the current Auto state.
    pub fn current_state(&self) -> AutoModeState {
        self.current_state
    }
}

impl Default for AutoStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

/// Global resource orchestrator managing dynamic capacity across Attic.
pub struct ResourceOrchestrator {
    mode: ResourceModeSetting,
    telemetry: MachineTelemetry,
    auto_state: Mutex<AutoStateMachine>,
    shadow_mode: AtomicBool,
    last_allocation: Arc<RwLock<ResourceAllocation>>,
}

impl ResourceOrchestrator {
    /// Create a new orchestrator with the specified mode setting and telemetry source.
    pub fn new(mode: ResourceModeSetting, telemetry: MachineTelemetry, shadow_mode: bool) -> Self {
        let initial_alloc = ResourceAllocation::default();
        Self {
            mode,
            telemetry,
            auto_state: Mutex::new(AutoStateMachine::new()),
            shadow_mode: AtomicBool::new(shadow_mode),
            last_allocation: Arc::new(RwLock::new(initial_alloc)),
        }
    }

    /// Return true if running in shadow mode (recommendations only).
    pub fn is_shadow_mode(&self) -> bool {
        self.shadow_mode.load(Ordering::Relaxed)
    }

    /// Enable or disable shadow mode at runtime.
    pub fn set_shadow_mode(&self, shadow: bool) {
        self.shadow_mode.store(shadow, Ordering::Release);
    }

    /// Current mode setting.
    pub fn mode(&self) -> ResourceModeSetting {
        self.mode
    }

    /// Set mode setting at runtime.
    pub fn set_mode(&mut self, mode: ResourceModeSetting) {
        self.mode = mode;
    }

    /// Recompute global resource allocation given current workload and emergency pressure.
    pub fn recompute_allocation(
        &self,
        workload: &WorkloadSnapshot,
        pressure: ResourcePressure,
    ) -> ResourceAllocation {
        let machine = self.telemetry.current_snapshot();

        // 1. Determine active intent policy
        let policy = match self.mode {
            ResourceModeSetting::Low => ModePolicy::low(),
            ResourceModeSetting::Balanced => ModePolicy::balanced(),
            ResourceModeSetting::Performance => ModePolicy::performance(),
            ResourceModeSetting::Auto => {
                let mut guard = self.auto_state.lock().unwrap_or_else(|e| e.into_inner());
                guard.update(&machine, pressure).to_policy()
            }
        };

        // 2. Compute total usable CPU core pool
        let total_cpus = machine.logical_cpus;
        let available_cpu = (total_cpus as f32 * machine.available_cpu_fraction).max(1.0);
        let usable_cores =
            ((available_cpu * policy.cpu_aggressiveness).round() as usize).clamp(1, total_cpus);

        // 3. Bounded MCP reserve (Master Plan §14)
        let mcp_reserved = (usable_cores / 4).clamp(1, 4);
        let remaining_workers = usable_cores.saturating_sub(mcp_reserved).max(1);

        // 4. Resource redistribution between canonical and semantic (Master Plan §13, §15)
        let has_indexing = workload.indexing_pending > 0;
        let has_semantic = workload.semantic_pending > 0;

        let (indexing_workers, semantic_cpu_threads) = if has_indexing && !has_semantic {
            // Only indexing has backlog
            (remaining_workers, 1)
        } else if !has_indexing && has_semantic {
            // Only semantic has backlog: reserve minimal 1 canonical worker, rest to semantic
            (1, remaining_workers)
        } else if has_indexing && has_semantic {
            // Both active: enforce minimum canonical share, divide remainder
            let min_canonical = 1;
            let split = (remaining_workers.saturating_sub(min_canonical)) / 2;
            (
                min_canonical + split,
                (remaining_workers - (min_canonical + split)).max(1),
            )
        } else {
            // Idle state: conservative baseline
            (1, 1)
        };

        // 5. Semantic inference tuning based on policy and available memory
        let semantic_inference_lanes = (semantic_cpu_threads / 2).clamp(1, 4);

        let base_batch =
            if policy.memory_aggressiveness > 0.70 && machine.available_memory_mib > 4096 {
                32
            } else if policy.memory_aggressiveness > 0.40 && machine.available_memory_mib > 2048 {
                16
            } else {
                8
            };

        let mut allocation = ResourceAllocation {
            indexing_workers,
            semantic_cpu_threads,
            semantic_inference_lanes,
            semantic_batch_size: base_batch,
            semantic_prefetch_limit: base_batch * 2,
            mcp_reserved_capacity: mcp_reserved,
        };

        // 6. Reactive emergency safety override (Master Plan §2.2)
        match pressure {
            ResourcePressure::Normal => {}
            ResourcePressure::Warning => {
                allocation.semantic_batch_size = (allocation.semantic_batch_size / 2).max(1);
                allocation.semantic_prefetch_limit = allocation.semantic_batch_size * 2;
            }
            ResourcePressure::Critical => {
                allocation.indexing_workers = 1;
                allocation.semantic_inference_lanes = 1;
                allocation.semantic_batch_size = (allocation.semantic_batch_size / 4).max(1);
                allocation.semantic_prefetch_limit = allocation.semantic_batch_size;
            }
            ResourcePressure::Emergency => {
                allocation.indexing_workers = 0;
                allocation.semantic_inference_lanes = 0;
                allocation.semantic_batch_size = 0;
                allocation.semantic_prefetch_limit = 0;
            }
        }

        if self.is_shadow_mode() {
            tracing::info!(
                ?allocation,
                mode = ?self.mode,
                ?pressure,
                "ResourceOrchestrator [Shadow Mode]: recommendation computed"
            );
        }

        let mut current = self
            .last_allocation
            .write()
            .unwrap_or_else(|e| e.into_inner());
        *current = allocation;
        allocation
    }

    /// Read the most recent computed allocation.
    pub fn current_allocation(&self) -> ResourceAllocation {
        let guard = self
            .last_allocation
            .read()
            .unwrap_or_else(|e| e.into_inner());
        *guard
    }

    /// Return a clone of the shared allocation handle for downstream subscribers (§15).
    pub fn shared_allocation(&self) -> Arc<RwLock<ResourceAllocation>> {
        self.last_allocation.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_telemetry(_mem_mib: u64, _cpus: usize) -> MachineTelemetry {
        MachineTelemetry::new(None)
    }

    #[test]
    fn auto_state_machine_transitions_on_pressure() {
        let mut auto = AutoStateMachine::new();
        let machine = MachineSnapshot {
            total_memory_mib: 16384,
            available_memory_mib: 500, // Very low available RAM
            attic_rss_mib: 1000,
            logical_cpus: 8,
            cpu_utilization: 20.0,
            available_cpu_fraction: 0.80,
            semantic_disk_free_mib: 50000,
            power_source: Some(PowerSource::Ac),
        };

        let state = auto.update(&machine, ResourcePressure::Normal);
        assert_eq!(state, AutoModeState::Conservative);
    }

    #[test]
    fn orchestrator_allocates_under_normal_conditions() {
        let tel = dummy_telemetry(16384, 8);
        let orchestrator = ResourceOrchestrator::new(ResourceModeSetting::Balanced, tel, false);

        let workload = WorkloadSnapshot {
            indexing_pending: 100,
            indexing_active: 0,
            semantic_pending: 500,
            semantic_inflight: 0,
            semantic_completed: 0,
            interactive_embedding_pending: 0,
            indexing_rate: 10.0,
            embedding_rate: 20.0,
            embedding_batch_latency_ms: 50.0,
            mcp_interactive_latency_ms: 10.0,
        };

        let alloc = orchestrator.recompute_allocation(&workload, ResourcePressure::Normal);
        assert!(alloc.indexing_workers >= 1);
        assert!(alloc.semantic_cpu_threads >= 1);
        assert!(alloc.mcp_reserved_capacity >= 1);
    }

    #[test]
    fn emergency_pressure_forces_concurrency_to_zero() {
        let tel = dummy_telemetry(16384, 8);
        let orchestrator = ResourceOrchestrator::new(ResourceModeSetting::Performance, tel, false);

        let workload = WorkloadSnapshot {
            indexing_pending: 100,
            indexing_active: 0,
            semantic_pending: 500,
            semantic_inflight: 0,
            semantic_completed: 0,
            interactive_embedding_pending: 0,
            indexing_rate: 0.0,
            embedding_rate: 0.0,
            embedding_batch_latency_ms: 0.0,
            mcp_interactive_latency_ms: 0.0,
        };

        let alloc = orchestrator.recompute_allocation(&workload, ResourcePressure::Emergency);
        assert_eq!(alloc.indexing_workers, 0);
        assert_eq!(alloc.semantic_inference_lanes, 0);
        assert_eq!(alloc.semantic_batch_size, 0);
    }
}
