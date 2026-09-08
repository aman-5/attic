//! Persisted embedding vector-space identity (Phase 8).
//!
//! `EmbeddingProfile` is claimed exactly once, at first real embedding work
//! — never merely on startup or `status`. The database persists both the
//! identity (`profile_id`, a BLAKE3 hash) and the means to reconstruct it
//! (`config`, the canonical `EmbeddingSpaceDescriptor`) — a hash alone would
//! be an opaque fingerprint with no way to rebuild a provider from a stored
//! record.
//!
//! `model_revision`/`tokenizer_revision` MUST be resolved, immutable commit
//! SHAs before hashing — never a mutable ref like `"main"`. If either drifts
//! while the label stays the same, the profile hash would stay identical
//! while the actual embedding space silently changed underneath it — exactly
//! the invisible-corpus-corruption failure mode this type exists to prevent.
//! Resolving refs to commit SHAs is the caller's job (a future
//! `BgeEmbedder`/model-loading concern); this module only hashes and
//! persists whatever resolved descriptor it is handed.

use serde::{Deserialize, Serialize};

/// Pooling strategy applied to the transformer's last hidden state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolingStrategy {
    /// Use the `[CLS]` token position (index 0).
    Cls,
    /// Mean-pool over all token positions.
    Mean,
}

/// How inputs longer than `max_tokens` are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TruncationPolicy {
    /// Truncate to `max_tokens`.
    Truncate,
    /// Reject the input outright.
    Reject,
}

/// The complete, hashable identity of an embedding vector space.
///
/// `schema_version` versions THIS canonical encoding (bumped only if the
/// encoding itself changes) — never bumped for model changes, which are
/// captured by `provider`/`model`/`model_revision` instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingSpaceDescriptor {
    /// Version of [`Self::canonical_bytes`]'s encoding.
    pub schema_version: u32,
    /// Stable provider id (e.g. `"bge"`).
    pub provider: String,
    /// Model name (e.g. `"bge-small-en-v1.5"`).
    pub model: String,
    /// Resolved, immutable model revision (a commit SHA, never `"main"`).
    pub model_revision: String,
    /// Resolved, immutable tokenizer revision (a commit SHA, never `"main"`;
    /// resolved independently — the tokenizer artifact is not assumed to
    /// share the model's revision).
    pub tokenizer_revision: String,
    /// Pooling strategy.
    pub pooling: PoolingStrategy,
    /// Whether output vectors are L2-normalized.
    pub normalize: bool,
    /// Truncation policy for over-length inputs.
    pub truncation: TruncationPolicy,
    /// Maximum input sequence length, in tokens.
    pub max_tokens: usize,
}

impl EmbeddingSpaceDescriptor {
    /// Current canonical encoding version.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Fixed field order, explicit length-prefixed encoding — NOT "derive
    /// `Serialize` and hope JSON key order is stable."
    pub fn canonical_bytes(&self) -> Vec<u8> {
        fn write_field(buf: &mut Vec<u8>, bytes: &[u8]) {
            buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            buf.extend_from_slice(bytes);
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.schema_version.to_le_bytes());
        write_field(&mut buf, self.provider.as_bytes());
        write_field(&mut buf, self.model.as_bytes());
        write_field(&mut buf, self.model_revision.as_bytes());
        write_field(&mut buf, self.tokenizer_revision.as_bytes());
        buf.push(match self.pooling {
            PoolingStrategy::Cls => 0,
            PoolingStrategy::Mean => 1,
        });
        buf.push(self.normalize as u8);
        buf.push(match self.truncation {
            TruncationPolicy::Truncate => 0,
            TruncationPolicy::Reject => 1,
        });
        buf.extend_from_slice(&(self.max_tokens as u64).to_le_bytes());
        buf
    }

    /// BLAKE3 hex hash of [`Self::canonical_bytes`] — "are two vector spaces
    /// identical?"
    pub fn profile_id(&self) -> String {
        blake3::hash(&self.canonical_bytes()).to_hex().to_string()
    }
}

/// A claimed, persisted embedding identity: the hash plus the config needed
/// to reconstruct the provider that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingProfile {
    /// `config.profile_id()`, stored alongside for cheap equality checks.
    pub id: String,
    /// The full descriptor ("what vector space is this?").
    pub config: EmbeddingSpaceDescriptor,
}

/// Where a requested [`EmbeddingSpaceDescriptor`] came from. Determines
/// whether losing a first-claim race is safe to silently adopt (a mere
/// recommendation) or must surface as a conflict (an explicit user request).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingIntentSource {
    /// `EmbeddingPolicy::recommend()`'s suggestion — no explicit user intent.
    Recommendation,
    /// Explicit `attic.toml` `[embedding]` override.
    TomlOverride,
    /// Explicit `ATTIC_EMBEDDING_*` env override.
    EnvOverride,
}

impl EmbeddingIntentSource {
    /// True for anything the user explicitly asked for (never silently
    /// overridden by a race loss).
    pub fn is_explicit(self) -> bool {
        !matches!(self, Self::Recommendation)
    }
}

/// Outcome of [`crate::store::SemanticStore::claim_embedding_profile_if_absent`].
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimOutcome {
    /// This process's own request won the race (or claimed the empty slot).
    Claimed(EmbeddingProfile),
    /// A profile already existed and happens to equal this process's request.
    ExistingMatched(EmbeddingProfile),
    /// Race lost, but the source was `Recommendation` only — safe to
    /// silently converge on the winner (no explicit intent violated).
    AdoptedRace {
        /// The profile that won the race and was adopted.
        adopted: EmbeddingProfile,
    },
    /// Race lost AND the source was explicit — the caller's request must
    /// NOT be silently discarded; surface "re-index required" and keep the
    /// existing (`adopted`) profile serving.
    Conflict {
        /// What this process asked for.
        requested: EmbeddingSpaceDescriptor,
        /// The profile that is actually persisted and stays active.
        adopted: EmbeddingProfile,
    },
}

/// Outcome of comparing an already-persisted profile against a fresh
/// request (the "profile already exists" path, as opposed to the
/// first-claim race in [`ClaimOutcome`]).
#[derive(Debug, Clone, PartialEq)]
pub enum ProfileCheck {
    /// The request matches what's already persisted.
    Matches,
    /// The request differs — surface "re-index required", keep `existing`
    /// active. Never true for a downgrade path (see
    /// `attic_semantic::embedding_policy`); this function performs the raw
    /// comparison only, the caller decides whether to surface it.
    Conflict {
        /// The persisted profile that remains active.
        existing: Box<EmbeddingProfile>,
        /// What this process asked for.
        requested: EmbeddingSpaceDescriptor,
    },
}

/// Compare an already-persisted profile against a fresh request.
///
/// `source` is accepted for symmetry with [`ClaimOutcome`] and future
/// intent-aware policy (e.g. distinguishing an explicit downgrade request
/// from a policy-driven one); the raw comparison itself does not depend on
/// it today.
pub fn check_requested_profile(
    existing: &EmbeddingProfile,
    requested: &EmbeddingSpaceDescriptor,
    _source: EmbeddingIntentSource,
) -> ProfileCheck {
    if existing.config == *requested {
        ProfileCheck::Matches
    } else {
        ProfileCheck::Conflict {
            existing: Box::new(existing.clone()),
            requested: requested.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(model_revision: &str) -> EmbeddingSpaceDescriptor {
        EmbeddingSpaceDescriptor {
            schema_version: EmbeddingSpaceDescriptor::SCHEMA_VERSION,
            provider: "bge".into(),
            model: "bge-small-en-v1.5".into(),
            model_revision: model_revision.into(),
            tokenizer_revision: "tok-abc123".into(),
            pooling: PoolingStrategy::Cls,
            normalize: true,
            truncation: TruncationPolicy::Truncate,
            max_tokens: 512,
        }
    }

    #[test]
    fn identical_descriptors_hash_identically() {
        assert_eq!(
            descriptor("rev1").profile_id(),
            descriptor("rev1").profile_id()
        );
    }

    #[test]
    fn differing_model_revision_changes_the_hash() {
        assert_ne!(
            descriptor("rev1").profile_id(),
            descriptor("rev2").profile_id()
        );
    }

    #[test]
    fn differing_pooling_changes_the_hash() {
        let mut a = descriptor("rev1");
        let mut b = a.clone();
        a.pooling = PoolingStrategy::Cls;
        b.pooling = PoolingStrategy::Mean;
        assert_ne!(a.profile_id(), b.profile_id());
    }

    #[test]
    fn check_requested_profile_matches_identical_config() {
        let d = descriptor("rev1");
        let profile = EmbeddingProfile {
            id: d.profile_id(),
            config: d.clone(),
        };
        assert_eq!(
            check_requested_profile(&profile, &d, EmbeddingIntentSource::TomlOverride),
            ProfileCheck::Matches
        );
    }

    #[test]
    fn check_requested_profile_flags_conflict_on_difference() {
        let existing_desc = descriptor("rev1");
        let profile = EmbeddingProfile {
            id: existing_desc.profile_id(),
            config: existing_desc,
        };
        let requested = descriptor("rev2");
        let outcome =
            check_requested_profile(&profile, &requested, EmbeddingIntentSource::TomlOverride);
        assert!(matches!(outcome, ProfileCheck::Conflict { .. }));
    }

    #[test]
    fn intent_source_explicitness() {
        assert!(!EmbeddingIntentSource::Recommendation.is_explicit());
        assert!(EmbeddingIntentSource::TomlOverride.is_explicit());
        assert!(EmbeddingIntentSource::EnvOverride.is_explicit());
    }
}
