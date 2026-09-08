//! `EmbeddingPolicy` — the sole owner of model-size recommendation (Phase 8).
//!
//! Kept fully independent of `attic_storage::resource_policy::ResourceMode`
//! so a hardware-tuning abstraction can never implicitly own vector-space
//! selection. V1 always recommends one fixed default (`bge-base-en-v1.5`,
//! 768-dim — a deliberate choice, not `small`/384-dim or `large`/1024-dim)
//! regardless of detected mode; a future hardware-tiered table
//! (Low→small, Balanced→base, Performance→large) is deliberately deferred
//! until benchmark data justifies it — this signature takes no `mode`
//! parameter today so that widening it later is a visible, deliberate
//! change, not a silently-ignored parameter sitting here now.

/// A cheap, UNRESOLVED suggestion — no network/hf-hub lookup. Distinct from
/// a claimed `EmbeddingProfile`, which requires resolved immutable model/
/// tokenizer revisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingRecommendation {
    /// Recommended provider id.
    pub provider: String,
    /// Recommended model name.
    pub model: String,
}

/// Model-selection policy.
pub struct EmbeddingPolicy;

impl EmbeddingPolicy {
    /// Attic's current recommended embedding provider/model.
    ///
    /// `BgeEmbedder` (Phase 9) is the constructible implementation of this
    /// recommendation. This method itself stays a cheap, unresolved
    /// suggestion — no network/hf-hub lookup — surfaced by `status` even
    /// before any provider has been constructed.
    pub fn recommend() -> EmbeddingRecommendation {
        EmbeddingRecommendation {
            provider: "bge".into(),
            model: "bge-base-en-v1.5".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommendation_is_stable() {
        assert_eq!(EmbeddingPolicy::recommend(), EmbeddingPolicy::recommend());
    }

    #[test]
    fn v1_recommends_bge_base() {
        let r = EmbeddingPolicy::recommend();
        assert_eq!(r.provider, "bge");
        assert_eq!(r.model, "bge-base-en-v1.5");
    }
}
