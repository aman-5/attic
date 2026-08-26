//! Signal computation helpers shared by ranking and validation.

use attic_core::FreshnessState;

/// Freshness score mapping (evidence.md): 1.0 = CURRENT, 0.0 = INVALID.
pub fn freshness_score(f: FreshnessState) -> f64 {
    match f {
        FreshnessState::Current => 1.0,
        FreshnessState::PendingRefresh => 0.75,
        FreshnessState::Stale => 0.4,
        FreshnessState::Unknown => 0.3,
        FreshnessState::Invalid => 0.0,
    }
}

/// Normalize a bm25-style relevance score (higher = better) into [0, 1)
/// with a fixed saturation constant so scores stay comparable across
/// corpora sizes.
pub fn normalize_lexical(raw: f64) -> f64 {
    if raw <= 0.0 {
        return 0.0;
    }
    const SATURATION: f64 = 3.0;
    raw / (raw + SATURATION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attic_core::FreshnessState;

    #[test]
    fn freshness_scores_ordered() {
        assert!(freshness_score(FreshnessState::Current) > freshness_score(FreshnessState::Stale));
        assert_eq!(freshness_score(FreshnessState::Invalid), 0.0);
    }

    #[test]
    fn lexical_normalizer_bounded() {
        assert_eq!(normalize_lexical(0.0), 0.0);
        let s = normalize_lexical(100.0);
        assert!((0.9..1.0).contains(&s));
    }
}
