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

    /// Apply environment constraints to prevent unconstrained internal threading
    /// in BLAS/Rayon/OpenMP libraries.
    pub fn apply_environment_hints(&self) {
        let threads_str = self.threads_per_lane.to_string();
        // Safe hints for math and tokenization runtime libraries
        unsafe {
            std::env::set_var("RAYON_NUM_THREADS", &threads_str);
            std::env::set_var("OMP_NUM_THREADS", &threads_str);
            std::env::set_var("MKL_NUM_THREADS", &threads_str);
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
    fn dynamic_scaling_8_4_2_6_lifecycle_enforcement() {
        use crate::error::SemanticError;
        use crate::model_lifecycle::SharedModelHandle;
        use crate::provider::{
            EmbeddingExecutionBudget, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput,
            EmbeddingProvider,
        };
        use std::sync::Arc;

        struct TestMockProvider {
            fp: EmbeddingFingerprint,
        }
        impl EmbeddingProvider for TestMockProvider {
            fn model_fingerprint(&self) -> EmbeddingFingerprint {
                self.fp.clone()
            }
            fn dimension(&self) -> usize {
                self.fp.dimension
            }
            fn warm_up(&self, _: &EmbeddingExecutionBudget) -> Result<(), SemanticError> {
                Ok(())
            }
            fn embed_documents(
                &self,
                inputs: &[EmbeddingInput],
                _: &EmbeddingExecutionBudget,
            ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
                Ok(inputs
                    .iter()
                    .map(|i| EmbeddingOutput {
                        unit_key: i.unit_key.clone(),
                        vector: vec![0.1; 128],
                    })
                    .collect())
            }
            fn embed_query(
                &self,
                _: &str,
                _: &EmbeddingExecutionBudget,
            ) -> Result<Vec<f32>, SemanticError> {
                Ok(vec![0.1; 128])
            }
        }

        let fp = EmbeddingFingerprint {
            provider: "qwen3".to_string(),
            model_id: "qwen3-embedding-0.6b".to_string(),
            model_revision: "rev_test".to_string(),
            dimension: 128,
            pooling_version: "last_token_v1".to_string(),
            normalization_version: "l2_unit_v1".to_string(),
            tokenizer_version: "tok_v1".to_string(),
            chunking_version: "ast_v1".to_string(),
            query_instruction_version: "code_v1".to_string(),
        };

        // Shared loaded model instance (single authority)
        let handle = Arc::new(SharedModelHandle::new(
            Arc::new(TestMockProvider { fp }),
            500,
            8,
        ));

        // Test progression: 8 -> 4 -> 2 -> 6 granted threads with 4 requested worker lanes
        let grant_sequence = vec![8, 4, 2, 6];
        let requested_lanes = 4;

        for granted_threads in grant_sequence {
            let plan = CpuIsolationPlan::compute(granted_threads, requested_lanes);
            assert!(
                !plan.is_oversubscribed(),
                "must never oversubscribe granted threads"
            );
            assert!(plan.total_allocated_threads <= granted_threads);

            // Dynamically scale handle concurrency to isolation plan
            handle.update_max_concurrency(plan.inference_lanes);
            assert_eq!(handle.max_concurrency(), plan.inference_lanes);

            // Apply runtime hints
            plan.apply_environment_hints();

            // Verify permit acquisition strictly enforces the scaled lane limit
            let mut permits = Vec::new();
            for _ in 0..plan.inference_lanes {
                let permit = handle.acquire_inference_permit();
                assert!(permit.is_ok(), "should permit up to allocated lanes");
                permits.push(permit.unwrap());
            }
            assert_eq!(handle.active_inferences(), plan.inference_lanes);

            // Exceeding lane limit must be rejected
            let overflow = handle.acquire_inference_permit();
            assert!(
                overflow.is_err(),
                "cannot exceed dynamic lane concurrency limit"
            );

            // Dropping permits drains active inferences safely
            drop(permits);
            assert_eq!(handle.active_inferences(), 0);
        }
    }
}
