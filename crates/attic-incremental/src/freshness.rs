//! Freshness state machine (invalidation contract §State Transitions).
//!
//! ```text
//! CURRENT → STALE → INVALID → PENDING_REFRESH → CURRENT
//!    │        │        ↑   ↕          │
//!    │        ↓        └───┘          └── (refresh fails) → INVALID
//!    └──────→ UNKNOWN ─→ {STALE | INVALID | CURRENT(verified)}
//!
//! Direct CURRENT → INVALID is legal for: deletion, incompatible migration,
//! explicit rebuild.
//! ```

use attic_core::FreshnessState;

/// A transition the state machine rejects.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("illegal freshness transition: {from:?} → {to:?}")]
pub struct FreshnessTransitionError {
    /// Source state.
    pub from: FreshnessState,
    /// Rejected target state.
    pub to: FreshnessState,
}

/// Legal-transition table (invalidation contract; Phase 2 enforcement).
///
/// Self-transitions are no-ops and always allowed (idempotent writes).
pub fn can_transition(from: FreshnessState, to: FreshnessState) -> bool {
    use FreshnessState as F;
    if from == to {
        return true;
    }
    matches!(
        (from, to),
        // Forward degradation chain + shortcuts.
        (F::Current, F::Stale)
            | (F::Current, F::Invalid)
            | (F::Current, F::Unknown)
            | (F::Current, F::PendingRefresh)
            // STALE may be confirmed invalid, refreshed, or re-verified.
            | (F::Stale, F::Invalid)
            | (F::Stale, F::Unknown)
            | (F::Stale, F::PendingRefresh)
            | (F::Stale, F::Current)
            // UNKNOWN resolves only through verification/reconciliation.
            | (F::Unknown, F::Stale)
            | (F::Unknown, F::Invalid)
            | (F::Unknown, F::PendingRefresh)
            | (F::Unknown, F::Current)
            // INVALID must pass through PENDING_REFRESH before CURRENT.
            | (F::Invalid, F::PendingRefresh)
            | (F::Invalid, F::Unknown)
            // Refresh completes or fails.
            | (F::PendingRefresh, F::Current)
            | (F::PendingRefresh, F::Invalid)
    )
}

/// Validate a transition; returns `Err` on an illegal move so callers never
/// silently promote uncertain state to CURRENT.
pub fn assert_transition(
    from: FreshnessState,
    to: FreshnessState,
) -> Result<(), FreshnessTransitionError> {
    if can_transition(from, to) {
        Ok(())
    } else {
        Err(FreshnessTransitionError { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use attic_core::FreshnessState::*;

    #[test]
    fn forward_chain_is_legal() {
        assert!(can_transition(Current, Stale));
        assert!(can_transition(Current, Invalid));
        assert!(can_transition(Stale, Invalid));
        assert!(can_transition(Invalid, PendingRefresh));
        assert!(can_transition(PendingRefresh, Current));
    }

    #[test]
    fn invalid_never_directly_current() {
        assert!(!can_transition(Invalid, Current));
        let err = assert_transition(Invalid, Current).unwrap_err();
        assert_eq!(err.from, Invalid);
        assert_eq!(err.to, Current);
    }

    #[test]
    fn unknown_resolves_through_verified_paths() {
        // Verification may confirm UNKNOWN state as CURRENT ...
        assert!(can_transition(Unknown, Current));
        // ... or degrade it; it can never be *skipped* over.
        assert!(can_transition(Unknown, Stale));
        assert!(can_transition(Unknown, Invalid));
    }

    #[test]
    fn self_transitions_are_idempotent() {
        for s in [Current, Stale, Unknown, Invalid, PendingRefresh] {
            assert!(can_transition(s, s), "{s} self-transition must be a no-op");
        }
    }
}
