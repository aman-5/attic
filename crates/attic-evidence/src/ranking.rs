//! Observable per-dimension ranking signals (`docs/contracts/evidence.md`
//! RankingSignals). The combined score is derived, never the only signal
//! used; every component signal stays inspectable for debugging and
//! benchmarking.

use serde::{Deserialize, Serialize};

/// Per-dimension ranking signals. `None` means "signal not applicable for
/// this candidate"; a value of 0.0 is a real zero.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RankingSignals {
    /// Normalized lexical (bm25) relevance.
    #[serde(default)]
    pub lexical_score: Option<f64>,
    /// Exactness of a symbol-name match (1.0 exact qname, 0.7 suffix).
    #[serde(default)]
    pub symbol_match_score: Option<f64>,
    /// How well the candidate origin matches the classified query intent.
    #[serde(default)]
    pub query_intent_match: Option<f64>,
    /// Repository relevance for the query scope.
    #[serde(default)]
    pub repository_relevance: Option<f64>,
    /// 1.0 = CURRENT … 0.0 = INVALID.
    #[serde(default)]
    pub freshness_score: Option<f64>,
    /// Structural proximity to intent anchors (same file / anchored node).
    #[serde(default)]
    pub structural_proximity: Option<f64>,
    /// Relationship confidence when graph-derived.
    #[serde(default)]
    pub relationship_confidence: Option<f64>,
    /// Authority fit of knowledge items for this intent.
    #[serde(default)]
    pub knowledge_authority: Option<f64>,
    /// Relevance of test evidence for this intent.
    #[serde(default)]
    pub test_relevance: Option<f64>,
    /// Semantic score — ALWAYS `None` in Phase 4 (Phase 5 capability).
    #[serde(default)]
    pub semantic_score: Option<f64>,
    /// Derived weighted score; operational only.
    #[serde(default)]
    pub combined_score: Option<f64>,
}

impl RankingSignals {
    /// Elementwise max merge used by candidate fusion: when two candidates
    /// fuse, each signal keeps its best observed value.
    pub fn merge_max(&mut self, other: &RankingSignals) {
        fn mx(slot: &mut Option<f64>, incoming: Option<f64>) {
            if let Some(v) = incoming
                && slot.is_none_or(|cur| v > cur)
            {
                *slot = Some(v);
            }
        }
        mx(&mut self.lexical_score, other.lexical_score);
        mx(&mut self.symbol_match_score, other.symbol_match_score);
        mx(&mut self.query_intent_match, other.query_intent_match);
        mx(&mut self.repository_relevance, other.repository_relevance);
        mx(&mut self.freshness_score, other.freshness_score);
        mx(&mut self.structural_proximity, other.structural_proximity);
        mx(
            &mut self.relationship_confidence,
            other.relationship_confidence,
        );
        mx(&mut self.knowledge_authority, other.knowledge_authority);
        mx(&mut self.test_relevance, other.test_relevance);
        mx(&mut self.semantic_score, other.semantic_score);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_max_keeps_best_per_dimension() {
        let mut a = RankingSignals {
            lexical_score: Some(0.2),
            freshness_score: Some(1.0),
            ..Default::default()
        };
        let b = RankingSignals {
            lexical_score: Some(0.9),
            symbol_match_score: Some(1.0),
            ..Default::default()
        };
        a.merge_max(&b);
        assert_eq!(a.lexical_score, Some(0.9));
        assert_eq!(a.freshness_score, Some(1.0));
        assert_eq!(a.symbol_match_score, Some(1.0));
    }

    #[test]
    fn signals_round_trip_json() {
        let s = RankingSignals {
            lexical_score: Some(0.5),
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: RankingSignals = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        assert!(json.contains("lexical_score"));
        assert!(!back.semantic_score.is_some());
    }
}
