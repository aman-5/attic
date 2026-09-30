//! Deferred semantic provider (Phase 2 — canonical/semantic separation).
//!
//! The server must never block startup on model download. This provider starts
//! as `UnavailableProvider` with a model-lifecycle state, a background task
//! downloads + verifies the weights, and on success the inner provider is
//! atomically swapped to the real embedder. All trait calls delegate to the
//! current inner provider, so callers see `Unavailable` until the swap.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, ResourceUsage,
    SemanticProvider, UnavailableProvider,
};

/// Model download/activation lifecycle (Final Plan: explicit states).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelLifecycle {
    /// Weights not present locally.
    Missing,
    /// Background download in progress (attempt number, 1-based).
    Downloading { attempt: u32 },
    /// Download complete, verifying checksums/probing.
    Verifying,
    /// Provider live and serving embeddings.
    Ready,
    /// Download/verification failed; will retry after backoff.
    Backoff { attempt: u32, reason: String },
    /// Permanently failed after all retries.
    Failed { reason: String },
    /// Semantic disabled or no download configured.
    Disabled,
}

impl ModelLifecycle {
    pub fn as_str(&self) -> String {
        match self {
            Self::Missing => "missing".into(),
            Self::Downloading { attempt } => format!("downloading(attempt={attempt})"),
            Self::Verifying => "verifying".into(),
            Self::Ready => "ready".into(),
            Self::Backoff { attempt, .. } => format!("backoff(attempt={attempt})"),
            Self::Failed { .. } => "failed".into(),
            Self::Disabled => "disabled".into(),
        }
    }
}

/// A provider whose inner implementation can be swapped at runtime.
/// Starts unavailable; the background model task calls [`Self::swap_in`]
/// once the real provider is constructed from downloaded weights.
pub struct DeferredProvider {
    inner: RwLock<Arc<dyn SemanticProvider>>,
    lifecycle: RwLock<ModelLifecycle>,
}

impl DeferredProvider {
    pub fn new(reason: &str) -> Self {
        Self {
            inner: RwLock::new(Arc::new(UnavailableProvider {
                reason: reason.to_string(),
            })),
            lifecycle: RwLock::new(ModelLifecycle::Missing),
        }
    }

    /// Current model lifecycle state (for status reporting).
    pub fn lifecycle(&self) -> ModelLifecycle {
        self.lifecycle
            .read()
            .map(|g| g.clone())
            .unwrap_or(ModelLifecycle::Failed {
                reason: "lifecycle lock poisoned".into(),
            })
    }

    pub fn set_lifecycle(&self, state: ModelLifecycle) {
        if let Ok(mut g) = self.lifecycle.write() {
            *g = state;
        }
    }

    /// Atomically swap in the real provider and mark lifecycle Ready.
    pub fn swap_in(&self, provider: Arc<dyn SemanticProvider>) {
        if let Ok(mut g) = self.inner.write() {
            *g = provider;
        }
        self.set_lifecycle(ModelLifecycle::Ready);
    }

    fn current(&self) -> Arc<dyn SemanticProvider> {
        self.inner.read().map(|g| g.clone()).unwrap_or_else(|_| {
            Arc::new(UnavailableProvider {
                reason: "provider lock poisoned".into(),
            })
        })
    }
}

impl SemanticProvider for DeferredProvider {
    fn id(&self) -> &'static str {
        // Delegate so callers see "unavailable" before swap and the real
        // provider id (e.g. "qwen3") after — identity flows through.
        // `current()` returns Arc<dyn ...>; id() is 'static so no borrow issue.
        self.current().id()
    }
    fn model_id(&self) -> &str {
        // Cannot borrow out of the lock; report a stable placeholder. Callers
        // needing the real model id read it via fingerprint() after swap.
        "deferred"
    }
    fn dimensions(&self) -> usize {
        self.current().dimensions()
    }
    fn max_input_bytes(&self) -> usize {
        self.current().max_input_bytes()
    }
    fn available(&self) -> bool {
        self.current().available()
    }
    fn model_lifecycle(&self) -> Option<String> {
        Some(self.lifecycle().as_str())
    }
    fn concurrency_contract(&self) -> crate::provider::ProviderConcurrencyContract {
        self.current().concurrency_contract()
    }
    fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
        self.current().fingerprint()
    }
    fn preferred_claim_items(&self) -> Option<usize> {
        self.current().preferred_claim_items()
    }
    fn worker_status(&self) -> Option<crate::provider::WorkerStatus> {
        self.current().worker_status()
    }
    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        self.current().embed_batch(inputs, cancel, usage, deadline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_unavailable_and_swaps_to_ready() {
        let d = DeferredProvider::new("weights missing");
        assert_eq!(d.lifecycle(), ModelLifecycle::Missing);
        assert!(!d.available());
        let mut usage = ResourceUsage::default();
        let err = d
            .embed_batch(&[], &CancelFlag::new(), &mut usage, None)
            .unwrap_err();
        assert!(matches!(err, SemanticError::ProviderUnavailable { .. }));

        d.swap_in(Arc::new(crate::testing::HashingEmbedder::new()));
        assert_eq!(d.lifecycle(), ModelLifecycle::Ready);
        assert!(d.available());
    }

    #[test]
    fn lifecycle_states_report_distinctly() {
        assert_eq!(ModelLifecycle::Missing.as_str(), "missing");
        assert!(
            ModelLifecycle::Downloading { attempt: 2 }
                .as_str()
                .contains("attempt=2")
        );
        assert!(
            ModelLifecycle::Backoff {
                attempt: 1,
                reason: "x".into()
            }
            .as_str()
            .contains("backoff")
        );
    }
}
