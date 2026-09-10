//! Shared model lifecycle, concurrency management, and memory accounting (Master Plan V2 §19, §20, §50, CP9).
//!
//! Enforces:
//! - Single shared model instance across all repos, workspaces, and workers (§19).
//! - Explicit concurrency and inference lane limits.
//! - Bounded baseline memory accounting reported to the resource orchestrator.
//! - Orderly lifecycle state transitions (Unloaded -> Loading -> Ready -> Draining -> Cancelled) with safe drain/unload (§50).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingExecutionBudget, EmbeddingFingerprint, EmbeddingInput,
    EmbeddingOutput, EmbeddingProvider, ProviderConcurrencyContract,
};

/// Lifecycle state of a shared neural embedding model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelLifecycleState {
    /// Model weights are not present in memory.
    Unloaded,
    /// Model tensors are being read from disk or initialized.
    Loading,
    /// Model is fully loaded and ready to serve inference requests.
    Ready,
    /// Model is draining in-flight requests; no new requests are accepted.
    Draining,
    /// Model execution was cooperatively cancelled.
    Cancelled,
}

/// Thread-safe shared handle managing one loaded embedding model instance across all repos (§19).
pub struct SharedModelHandle {
    provider: Arc<dyn EmbeddingProvider>,
    state: Arc<RwLock<ModelLifecycleState>>,
    active_inferences: Arc<AtomicUsize>,
    cancel_flag: Arc<CancelFlag>,
    baseline_memory_mib: u64,
    max_concurrency: AtomicUsize,
}

impl SharedModelHandle {
    /// Wrap an initialized `EmbeddingProvider` into a shared lifecycle manager.
    pub fn new(
        provider: Arc<dyn EmbeddingProvider>,
        baseline_memory_mib: u64,
        max_concurrency: usize,
    ) -> Self {
        Self {
            provider,
            state: Arc::new(RwLock::new(ModelLifecycleState::Ready)),
            active_inferences: Arc::new(AtomicUsize::new(0)),
            cancel_flag: Arc::new(CancelFlag::new()),
            baseline_memory_mib,
            max_concurrency: AtomicUsize::new(max_concurrency.max(1)),
        }
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ModelLifecycleState {
        *self.state.read().unwrap()
    }

    /// Baseline model memory requirement in MiB.
    pub fn baseline_memory_mib(&self) -> u64 {
        self.baseline_memory_mib
    }

    /// Estimated current memory footprint including active inference lanes.
    pub fn current_memory_mib(&self) -> u64 {
        if self.state() == ModelLifecycleState::Unloaded {
            0
        } else {
            let active = self.active_inferences.load(Ordering::Relaxed);
            // ~32 MiB per active lane buffer
            self.baseline_memory_mib + (active as u64 * 32)
        }
    }

    /// Number of inference passes currently executing.
    pub fn active_inferences(&self) -> usize {
        self.active_inferences.load(Ordering::Relaxed)
    }

    /// Maximum allowed concurrent inference operations.
    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency.load(Ordering::Relaxed)
    }

    /// Dynamically adjust maximum allowed concurrent inference operations (§21).
    pub fn update_max_concurrency(&self, new_max: usize) {
        self.max_concurrency.store(new_max.max(1), Ordering::SeqCst);
    }

    /// Cooperative cancellation flag for this model instance.
    pub fn cancel_flag(&self) -> Arc<CancelFlag> {
        self.cancel_flag.clone()
    }

    /// Architectural fingerprint of the underlying vector space.
    pub fn fingerprint(&self) -> EmbeddingFingerprint {
        self.provider.model_fingerprint()
    }

    /// Dimensionality of vectors produced by this model.
    pub fn dimension(&self) -> usize {
        self.provider.dimension()
    }

    /// Underlying provider's concurrency contract.
    pub fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        self.provider.concurrency_contract()
    }

    /// Embed documents with in-flight tracking, concurrency gating, and drain protection.
    pub fn embed_documents(
        &self,
        inputs: &[EmbeddingInput],
        budget: &EmbeddingExecutionBudget,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let _guard = self.acquire_inference_permit()?;
        self.provider.embed_documents(inputs, budget)
    }

    /// Embed query with in-flight tracking, concurrency gating, and drain protection.
    pub fn embed_query(
        &self,
        query: &str,
        budget: &EmbeddingExecutionBudget,
    ) -> Result<Vec<f32>, SemanticError> {
        let _guard = self.acquire_inference_permit()?;
        self.provider.embed_query(query, budget)
    }

    /// Request model unload: transition to Draining, signal cancellation, and drain.
    pub fn unload(&self, timeout: Duration) -> Result<(), SemanticError> {
        {
            let mut state = self.state.write().unwrap();
            if *state == ModelLifecycleState::Unloaded {
                return Ok(());
            }
            *state = ModelLifecycleState::Draining;
        }

        self.cancel_flag.cancel();

        // Drain in-flight operations up to timeout
        let start = Instant::now();
        while self.active_inferences.load(Ordering::SeqCst) > 0 {
            if start.elapsed() >= timeout {
                let mut state = self.state.write().unwrap();
                *state = ModelLifecycleState::Cancelled;
                return Err(SemanticError::Cancelled {
                    completed: 0,
                    total: self.active_inferences.load(Ordering::SeqCst),
                });
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut state = self.state.write().unwrap();
        *state = ModelLifecycleState::Unloaded;
        Ok(())
    }

    pub(crate) fn acquire_inference_permit(&self) -> Result<InferencePermit, SemanticError> {
        let state = *self.state.read().unwrap();
        if state != ModelLifecycleState::Ready {
            return Err(SemanticError::ProviderUnavailable {
                provider: self.provider.model_fingerprint().provider,
                reason: format!("model is in {state:?} state and cannot accept new inference work"),
            });
        }

        let max_concurrency = self.max_concurrency.load(Ordering::SeqCst);
        let current = self.active_inferences.fetch_add(1, Ordering::SeqCst);
        if current >= max_concurrency {
            self.active_inferences.fetch_sub(1, Ordering::SeqCst);
            return Err(SemanticError::ProviderUnavailable {
                provider: self.provider.model_fingerprint().provider,
                reason: format!(
                    "maximum inference lane concurrency ({}) reached",
                    max_concurrency
                ),
            });
        }

        Ok(InferencePermit {
            active_inferences: self.active_inferences.clone(),
        })
    }
}

/// RAII guard releasing an active inference counter when dropped.
pub(crate) struct InferencePermit {
    active_inferences: Arc<AtomicUsize>,
}

impl Drop for InferencePermit {
    fn drop(&mut self) {
        self.active_inferences.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::EmbeddingFingerprint;

    struct MockProvider {
        fp: EmbeddingFingerprint,
    }

    impl EmbeddingProvider for MockProvider {
        fn model_fingerprint(&self) -> EmbeddingFingerprint {
            self.fp.clone()
        }
        fn dimension(&self) -> usize {
            self.fp.dimension
        }
        fn warm_up(&self, _budget: &EmbeddingExecutionBudget) -> Result<(), SemanticError> {
            Ok(())
        }
        fn embed_documents(
            &self,
            inputs: &[EmbeddingInput],
            _budget: &EmbeddingExecutionBudget,
        ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
            Ok(inputs
                .iter()
                .map(|i| EmbeddingOutput {
                    unit_key: i.unit_key.clone(),
                    vector: vec![0.5; self.fp.dimension],
                })
                .collect())
        }
        fn embed_query(
            &self,
            _query: &str,
            _budget: &EmbeddingExecutionBudget,
        ) -> Result<Vec<f32>, SemanticError> {
            Ok(vec![0.5; self.fp.dimension])
        }
    }

    fn test_mock_handle(max_concurrency: usize) -> SharedModelHandle {
        let fp = EmbeddingFingerprint {
            provider: "mock".to_string(),
            model_id: "mock-model".to_string(),
            model_revision: "rev1".to_string(),
            dimension: 128,
            pooling_version: "last_token_v1".to_string(),
            normalization_version: "l2_unit_v1".to_string(),
            tokenizer_version: "tok_v1".to_string(),
            chunking_version: "ast_v1".to_string(),
            query_instruction_version: "code_v1".to_string(),
        };
        SharedModelHandle::new(Arc::new(MockProvider { fp }), 500, max_concurrency)
    }

    #[test]
    fn shared_handle_tracks_state_and_concurrency() {
        let handle = test_mock_handle(2);
        assert_eq!(handle.state(), ModelLifecycleState::Ready);
        assert_eq!(handle.baseline_memory_mib(), 500);
        assert_eq!(handle.current_memory_mib(), 500);

        let input = EmbeddingInput {
            unit_key: "u1".to_string(),
            text: "hello".to_string(),
        };
        let budget = EmbeddingExecutionBudget::default();

        let res = handle.embed_documents(&[input], &budget).unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(handle.active_inferences(), 0);
    }

    #[test]
    fn concurrency_limit_rejects_excess_lanes() {
        let handle = test_mock_handle(1);

        // Manually acquire a permit to saturate concurrency
        let permit = handle.acquire_inference_permit().unwrap();
        assert_eq!(handle.active_inferences(), 1);

        // Next acquire must fail
        assert!(handle.acquire_inference_permit().is_err());

        drop(permit);
        assert_eq!(handle.active_inferences(), 0);
        assert!(handle.acquire_inference_permit().is_ok());
    }

    #[test]
    fn unload_transitions_state_and_rejects_new_work() {
        let handle = test_mock_handle(2);
        handle.unload(Duration::from_millis(50)).unwrap();
        assert_eq!(handle.state(), ModelLifecycleState::Unloaded);

        let input = EmbeddingInput {
            unit_key: "u1".to_string(),
            text: "hello".to_string(),
        };
        let budget = EmbeddingExecutionBudget::default();
        let res = handle.embed_documents(&[input], &budget);
        assert!(res.is_err());
    }
}
