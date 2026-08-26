//! AnswerModePolicy (`docs/contracts/answer_modes.md`, C12): explicit,
//! enforceable resource budgets for FAST / NORMAL / DEEP.
//!
//! Budgets are compile-time defaults overridable at startup; they are not
//! per-request user choices in V1. Exceeding a budget is a hard observable
//! event, never a silent degradation.

use serde::{Deserialize, Serialize};

use attic_evidence::str_enum;

str_enum! {
    /// Operating tier.
    AnswerMode {
        /// Sub-second, index-only, no semantic search.
        Fast => "FAST",
        /// Default; balanced latency/quality.
        Normal => "NORMAL",
        /// Highest quality; broader bounded expansion.
        Deep => "DEEP",
    }
}

str_enum! {
    /// How deeply evidence is verified against live source.
    VerificationLevel {
        /// Trust stored hash; no filesystem access.
        None => "NONE",
        /// Re-read file from disk and compare BLAKE3 hash.
        Checksum => "CHECKSUM",
        /// Re-read and re-verify span content fields.
        Full => "FULL",
    }
}

str_enum! {
    /// Final policy outcome recorded in the execution trace.
    PolicyResult {
        /// Completed without hitting any budget ceiling.
        CompletedWithinBudget => "COMPLETED_WITHIN_BUDGET",
        /// Deadline hit mid-pipeline.
        DegradedByTime => "DEGRADED_BY_TIME",
        /// Candidate cap reached; ranking ran on the collected set.
        DegradedByCandidates => "DEGRADED_BY_CANDIDATES",
        /// Filesystem verification budget exhausted.
        DegradedByFsBudget => "DEGRADED_BY_FS_BUDGET",
        /// Context token budget truncated items.
        DegradedByTokens => "DEGRADED_BY_TOKENS",
        /// Contract unsatisfied after all repair cycles.
        InsufficientEvidence => "INSUFFICIENT_EVIDENCE",
        /// Hard cancellation by deadline enforcement.
        HardCancelled => "HARD_CANCELLED",
    }
}

/// The enforceable budget object that travels with every RetrievalPlan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerModePolicy {
    /// Which policy tier.
    pub mode: AnswerMode,
    /// Wall-clock deadline for the entire pipeline (ms).
    pub max_time_ms: u64,
    /// Max retrieval units considered before ranking.
    pub max_candidates: u32,
    /// Max edge hops in graph traversal.
    pub max_graph_depth: u8,
    /// Max nodes visited in any graph walk.
    pub max_graph_nodes: u32,
    /// Max filesystem files read end-to-end (0 = NO filesystem reads).
    pub max_fs_files: u32,
    /// Max total bytes read from the filesystem (0 = none).
    pub max_fs_bytes: u64,
    /// Max tokens assembled into context (approx. bytes/4 accounting).
    pub max_context_tokens: u32,
    /// Semantic search permitted — Phase 5: FAST=false, NORMAL/DEEP=true.
    pub semantic_allowed: bool,
    /// Max candidates the semantic generator may return (0 = none).
    pub max_semantic_candidates: u32,
    /// Wall-clock budget for the semantic generator step (ms).
    pub semantic_time_budget_ms: u64,
    /// Re-ranking pass permitted (deterministic re-scoring only).
    pub reranking_allowed: bool,
    /// Source verification depth.
    pub source_verification_level: VerificationLevel,
    /// Max automatic repair cycles on INSUFFICIENT_EVIDENCE.
    pub repair_attempts: u8,
}

/// Startup-time overrides (all optional; validated before use).
#[derive(Debug, Clone, Copy, Default)]
pub struct PolicyOverrides {
    pub max_time_ms: Option<u64>,
    pub max_candidates: Option<u32>,
    pub max_context_tokens: Option<u32>,
}

impl AnswerModePolicy {
    /// V1 default policy table (answer_modes.md §3).
    pub fn for_mode(mode: AnswerMode) -> Self {
        match mode {
            AnswerMode::Fast => Self {
                mode,
                max_time_ms: 300,
                max_candidates: 50,
                max_graph_depth: 1,
                max_graph_nodes: 25,
                max_fs_files: 0,
                max_fs_bytes: 0,
                max_context_tokens: 4096,
                semantic_allowed: false,
                max_semantic_candidates: 0,
                semantic_time_budget_ms: 0,
                reranking_allowed: false,
                source_verification_level: VerificationLevel::None,
                repair_attempts: 0,
            },
            AnswerMode::Normal => Self {
                mode,
                max_time_ms: 3_000,
                max_candidates: 500,
                max_graph_depth: 3,
                max_graph_nodes: 200,
                max_fs_files: 20,
                max_fs_bytes: 4 * 1024 * 1024,
                max_context_tokens: 16_384,
                semantic_allowed: true, // Phase 5: selective use (§14)
                max_semantic_candidates: 48,
                semantic_time_budget_ms: 250,
                reranking_allowed: true,
                source_verification_level: VerificationLevel::Checksum,
                repair_attempts: 1,
            },
            AnswerMode::Deep => Self {
                mode,
                max_time_ms: 30_000,
                max_candidates: 5_000,
                max_graph_depth: 8,
                max_graph_nodes: 2_000,
                max_fs_files: 200,
                max_fs_bytes: 100 * 1024 * 1024,
                max_context_tokens: 65_536,
                semantic_allowed: true, // Phase 5: broader bounded use (§14)
                max_semantic_candidates: 120,
                semantic_time_budget_ms: 2_000,
                reranking_allowed: true,
                source_verification_level: VerificationLevel::Full,
                repair_attempts: 3,
            },
        }
    }

    /// Apply startup overrides with validation (AM-INV-3/4/5 + §5.2).
    pub fn with_overrides(
        mode: AnswerMode,
        o: &PolicyOverrides,
    ) -> Result<Self, crate::error::RetrievalError> {
        let mut p = Self::for_mode(mode);
        if let Some(t) = o.max_time_ms {
            if t < 50 {
                return Err(crate::error::RetrievalError::InvalidQuery(
                    "max_time_ms must be >= 50".into(),
                ));
            }
            p.max_time_ms = t;
        }
        if let Some(c) = o.max_candidates {
            p.max_candidates = c;
        }
        if let Some(t) = o.max_context_tokens {
            if t > 131_072 {
                return Err(crate::error::RetrievalError::InvalidQuery(
                    "max_context_tokens exceeds the 128K hard ceiling".into(),
                ));
            }
            p.max_context_tokens = t;
        }
        p.validate()?;
        Ok(p)
    }

    /// Invariant checks (AM-INV-2..5). Called after any override merge.
    pub fn validate(&self) -> Result<(), crate::error::RetrievalError> {
        if self.max_time_ms == 0 {
            return Err(crate::error::RetrievalError::InvalidQuery(
                "max_time_ms must be > 0".into(),
            ));
        }
        if self.mode == AnswerMode::Fast
            && (self.repair_attempts != 0
                || self.max_fs_files != 0
                || self.max_fs_bytes != 0
                || self.semantic_allowed
                || self.max_semantic_candidates != 0)
        {
            return Err(crate::error::RetrievalError::InvalidQuery(
                "FAST must have zero repair/fs/semantic budget".into(),
            ));
        }
        if self.max_context_tokens > 131_072 {
            return Err(crate::error::RetrievalError::InvalidQuery(
                "max_context_tokens above hard ceiling".into(),
            ));
        }
        Ok(())
    }

    /// Whether this plan may touch the live filesystem at all.
    pub fn fs_reads_permitted(&self) -> bool {
        self.source_verification_level != VerificationLevel::None
            && self.max_fs_files > 0
            && self.max_fs_bytes > 0
    }
}

/// Per-query policy execution trace (answer_modes.md §7); embedded in the
/// RetrievalPlan so observability is a first-class artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyExecutionTrace {
    pub time_elapsed_ms: u64,
    pub candidates_examined: u32,
    pub graph_nodes_visited: u32,
    pub fs_files_read: u32,
    pub fs_bytes_read: u64,
    pub context_tokens_used: u32,
    /// Always false in Phase 4 — recorded so absence of semantic work is
    /// itself observable. Phase 5 sets it when the semantic generator runs.
    pub semantic_invoked: bool,
    pub reranking_invoked: bool,
    pub repair_cycles: u8,
    /// Semantic generator outcome details (§21 observability).
    pub semantic_candidates_returned: u32,
    /// Why semantic retrieval did not contribute (empty = it did or N/A).
    pub semantic_fallback_reason: String,
    /// Which limits were reached (field names, deterministic order).
    pub budget_fields_hit: Vec<String>,
    pub final_result: PolicyResult,
}

impl PolicyExecutionTrace {
    pub fn new() -> Self {
        Self {
            time_elapsed_ms: 0,
            candidates_examined: 0,
            graph_nodes_visited: 0,
            fs_files_read: 0,
            fs_bytes_read: 0,
            context_tokens_used: 0,
            semantic_invoked: false,
            reranking_invoked: false,
            repair_cycles: 0,
            semantic_candidates_returned: 0,
            semantic_fallback_reason: String::new(),
            budget_fields_hit: Vec::new(),
            final_result: PolicyResult::CompletedWithinBudget,
        }
    }
}

impl Default for PolicyExecutionTrace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_never_reads_filesystem_or_repairs() {
        let p = AnswerModePolicy::for_mode(AnswerMode::Fast);
        assert!(!p.fs_reads_permitted());
        assert_eq!(p.repair_attempts, 0);
        assert_eq!(p.source_verification_level, VerificationLevel::None);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn fast_configured_with_repair_is_rejected() {
        let mut p = AnswerModePolicy::for_mode(AnswerMode::Fast);
        p.repair_attempts = 1;
        assert!(p.validate().is_err());
    }

    #[test]
    fn tiny_deadline_override_rejected() {
        let o = PolicyOverrides {
            max_time_ms: Some(30),
            ..Default::default()
        };
        assert!(AnswerModePolicy::with_overrides(AnswerMode::Normal, &o).is_err());
    }

    #[test]
    fn token_ceiling_enforced() {
        let o = PolicyOverrides {
            max_context_tokens: Some(200_000),
            ..Default::default()
        };
        assert!(AnswerModePolicy::with_overrides(AnswerMode::Deep, &o).is_err());
    }

    #[test]
    fn policy_json_round_trips_without_loss() {
        for m in [AnswerMode::Fast, AnswerMode::Normal, AnswerMode::Deep] {
            let p = AnswerModePolicy::for_mode(m);
            let json = serde_json::to_string(&p).unwrap();
            let back: AnswerModePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(p, back);
        }
    }

    #[test]
    fn phase5_semantic_mode_table() {
        let fast = AnswerModePolicy::for_mode(AnswerMode::Fast);
        assert!(!fast.semantic_allowed);
        assert_eq!(fast.max_semantic_candidates, 0);
        assert!(fast.validate().is_ok());

        let normal = AnswerModePolicy::for_mode(AnswerMode::Normal);
        assert!(normal.semantic_allowed);
        assert!(normal.max_semantic_candidates > 0);
        assert!(normal.semantic_time_budget_ms > 0);

        let deep = AnswerModePolicy::for_mode(AnswerMode::Deep);
        assert!(deep.semantic_allowed);
        assert!(
            deep.max_semantic_candidates > normal.max_semantic_candidates,
            "DEEP must be broader than NORMAL (§14)"
        );
    }

    #[test]
    fn fast_configured_with_semantic_budget_is_rejected() {
        let mut p = AnswerModePolicy::for_mode(AnswerMode::Fast);
        p.max_semantic_candidates = 10;
        assert!(p.validate().is_err());
    }
}
