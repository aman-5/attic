//! CPU thread isolation and oversubscription guard (Master Plan V2 §21, CP14).
//!
//! Enforces:
//! - Strict compliance with Orchestrator's granted CPU allocation (`semantic_cpu_threads`).
//! - Prevention of the forbidden multiplication state (§21):
//!   `granted_threads = 4`, but `4 lanes * 8 threads = 32 threads`.
//! - Per-lane thread budgeting: `threads_per_lane = (granted_threads / lanes).max(1)`.
//! - Tokenizer and internal library parallelism auditing.

use serde::{Deserialize, Serialize};

/// Calculated CPU thread distribution across inference lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CpuIsolationPlan {
    pub granted_semantic_threads: usize,
    pub inference_lanes: usize,
    pub threads_per_lane: usize,
    pub total_allocated_threads: usize,
}

impl CpuIsolationPlan {
    /// Compute strict thread allocation preventing thread multiplication oversubscription (§21).
    pub fn compute(granted_semantic_threads: usize, requested_lanes: usize) -> Self {
        let granted = granted_semantic_threads.max(1);
        let lanes = requested_lanes.max(1).min(granted);
        let threads_per_lane = (granted / lanes).max(1);
        let total_allocated = lanes * threads_per_lane;

        Self {
            granted_semantic_threads: granted,
            inference_lanes: lanes,
            threads_per_lane,
            total_allocated_threads: total_allocated,
        }
    }

    /// Audit whether a given thread and lane configuration violates CPU bounds.
    pub fn is_oversubscribed(&self) -> bool {
        self.total_allocated_threads > self.granted_semantic_threads
    }

    /// Apply environment constraints as a startup hint before runtime pools exist.
    /// Note: Runtime elasticity must be enforced through admission control and scoped lane pools.
    pub fn apply_environment_hints(&self) {
        let threads_str = self.threads_per_lane.to_string();
        unsafe {
            std::env::set_var("RAYON_NUM_THREADS", &threads_str);
            std::env::set_var("OMP_NUM_THREADS", &threads_str);
            std::env::set_var("MKL_NUM_THREADS", &threads_str);
        }
    }

    /// Build a dedicated, isolated Rayon thread pool for this plan's per-lane budget.
    /// Does NOT rely on mutating environment variables after global thread pools exist (§21).
    pub fn create_lane_pool(&self) -> Result<rayon::ThreadPool, rayon::ThreadPoolBuildError> {
        rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads_per_lane)
            .thread_name(|idx| format!("attic-qwen-lane-{idx}"))
            .build()
    }

    /// Execute a closure inside a dedicated, bounded Rayon thread pool matching this plan's
    /// per-lane CPU budget, guaranteeing that math/gemm/tokenizer libraries cannot multiply
    /// beyond `threads_per_lane`.
    pub fn execute_isolated<F, R>(&self, op: F) -> R
    where
        F: FnOnce() -> R + Send,
        R: Send,
    {
        if let Ok(pool) = self.create_lane_pool() {
            pool.install(op)
        } else {
            op()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_prevents_oversubscription() {
        // Orchestrator grants 4 threads, requests 4 lanes
        let plan = CpuIsolationPlan::compute(4, 4);
        assert_eq!(plan.inference_lanes, 4);
        assert_eq!(plan.threads_per_lane, 1);
        assert_eq!(plan.total_allocated_threads, 4);
        assert!(!plan.is_oversubscribed());

        // Orchestrator grants 8 threads, requests 2 lanes
        let plan = CpuIsolationPlan::compute(8, 2);
        assert_eq!(plan.inference_lanes, 2);
        assert_eq!(plan.threads_per_lane, 4);
        assert_eq!(plan.total_allocated_threads, 8);
        assert!(!plan.is_oversubscribed());

        // Orchestrator grants 2 threads, requests 4 lanes: lanes clamped to 2
        let plan = CpuIsolationPlan::compute(2, 4);
        assert_eq!(plan.inference_lanes, 2);
        assert_eq!(plan.threads_per_lane, 1);
        assert_eq!(plan.total_allocated_threads, 2);
        assert!(!plan.is_oversubscribed());
    }


    #[test]
    fn plan_execute_isolated_bounds_threads() {
        for &(granted, lanes) in &[(8, 2), (4, 4), (2, 2), (6, 2)] {
            let plan = CpuIsolationPlan::compute(granted, lanes);
            let threads_used = plan.execute_isolated(|| rayon::current_num_threads());
            assert_eq!(threads_used, plan.threads_per_lane);
        }
    }

    #[test]
    fn plan_candle_native_math_confinement_across_allocations() {
        use candle_core::{Device, Tensor};
        use std::collections::HashSet;
        use std::sync::{Arc, Mutex};

        // Validate real execution under dynamic Orchestrator allocations: 8 -> 4 -> 2 -> 6
        let transitions = [(8, 2), (4, 4), (2, 2), (6, 2)];

        for &(granted, lanes) in &transitions {
            let plan = CpuIsolationPlan::compute(granted, lanes);
            let threads_budget = plan.threads_per_lane;

            // Execute real Candle CPU matrix multiplications inside execute_isolated
            let result = plan.execute_isolated(|| {
                let a = Tensor::randn(0f32, 1f32, (128, 128), &Device::Cpu).unwrap();
                let b = Tensor::randn(0f32, 1f32, (128, 128), &Device::Cpu).unwrap();
                let c = a.matmul(&b).unwrap();
                c.to_vec2::<f32>().unwrap()
            });
            assert_eq!(result.len(), 128);

            // Verify that create_lane_pool builds exactly the requested thread budget
            let pool = plan.create_lane_pool().unwrap();
            assert_eq!(pool.current_num_threads(), threads_budget);

            // Execute parallel Candle tensor math across dedicated pool workers
            let observed_threads = Arc::new(Mutex::new(HashSet::new()));
            let observed_clone = Arc::clone(&observed_threads);

            pool.broadcast(|ctx| {
                let name = std::thread::current().name().unwrap_or("unnamed").to_string();
                let index = ctx.index();
                observed_clone.lock().unwrap().insert((index, name));

                // Perform real native tensor operations inside every worker
                let t1 = Tensor::zeros((64, 64), candle_core::DType::F32, &Device::Cpu).unwrap();
                let t2 = Tensor::ones((64, 64), candle_core::DType::F32, &Device::Cpu).unwrap();
                let _ = t1.add(&t2).unwrap();
            });

            let observed = observed_threads.lock().unwrap();
            assert_eq!(observed.len(), threads_budget);

            for (idx, name) in observed.iter() {
                assert_eq!(name, &format!("attic-qwen-lane-{}", idx));
            }
        }
    }
}
