//! Evidence ranking (Phase 4 §9): computes the observable signal vector and
//! a derived combined score from an explicit per-intent weight table.
//!
//! Ranking answers "likely useful?"; validation (see `validate`) separately
//! answers "can it support the requirement?". The two are never merged into
//! one opaque score.

use attic_evidence::EvidenceSourceType as ST;
use attic_evidence::signals::{freshness_score, normalize_lexical};
use attic_evidence::{AuthorityLevel, Evidence};

use crate::candidates::RetrieverKind;
use crate::query::QueryType;

/// Slot indexes into the weight-table rows (documented order).
#[allow(dead_code)]
mod w {
    pub const LEXICAL: usize = 0;
    pub const SYMBOL: usize = 1;
    pub const INTENT: usize = 2;
    pub const REPO: usize = 3;
    pub const FRESHNESS: usize = 4;
    pub const STRUCTURAL: usize = 5;
    pub const RELATIONSHIP: usize = 6;
    pub const KNOWLEDGE: usize = 7;
    pub const TEST: usize = 8;
    /// Phase 5: semantic similarity — deliberately ZERO for exact/definition/
    /// configuration lookups so those slices can never regress from vectors.
    pub const SEMANTIC: usize = 9;
}

/// Explicit, inspectable weights per query type (ADR-014). Widened to ten
/// slots in Phase 5; the first nine rows are byte-for-byte the Phase 4
/// table with `0.0` appended where semantics must not participate.
pub fn intent_weights(qt: QueryType) -> [f64; 10] {
    use QueryType as Q;
    match qt {
        // Definitions: exact symbol evidence dominates, freshness matters
        // because CURRENT_ONLY contracts reject stale definitions.
        Q::ExactLookup | Q::DefinitionLookup => [1.5, 3.0, 2.0, 0.5, 2.0, 0.5, 0.0, 0.0, 0.0, 0.0],
        Q::SymbolNavigation => [1.0, 2.5, 2.0, 0.5, 1.0, 0.5, 2.5, 0.0, 0.0, 0.0],
        Q::ConfigurationLookup => [1.5, 0.5, 2.0, 0.5, 2.0, 0.0, 0.0, 1.5, 0.0, 0.0],
        Q::ArchitectureExplanation => [1.5, 1.0, 2.0, 0.5, 1.0, 1.5, 1.0, 2.0, 1.0, 0.5],
        Q::DebuggingRootCause => [1.5, 1.0, 2.0, 0.5, 1.5, 1.0, 1.0, 1.5, 2.0, 0.5],
        Q::ImpactAnalysis => [1.0, 2.0, 2.0, 0.5, 1.0, 1.0, 2.5, 0.0, 1.5, 0.25],
        Q::DependencyQuestion | Q::CrossRepoQuestion => {
            [1.0, 1.0, 2.0, 1.0, 1.0, 0.5, 3.0, 0.5, 0.0, 0.25]
        }
        Q::TestBehavior => [1.5, 1.0, 2.0, 0.5, 1.0, 0.5, 0.5, 0.5, 3.0, 0.25],
        Q::KnowledgeQuestion => [1.5, 0.5, 2.0, 0.5, 1.0, 0.0, 0.0, 3.0, 0.0, 0.75],
        Q::GenericSearch => [2.5, 1.0, 1.0, 0.5, 1.0, 0.5, 0.5, 0.5, 0.5, 1.0],
    }
}

/// Intent match of a retriever origin for a query type — explicit table,
/// inspectable in benchmarks and debugging. The semantic column (last) is
/// the Phase 5 addition.
pub fn origin_intent_match(qt: QueryType, kind: RetrieverKind) -> f64 {
    use QueryType as Q;
    let (fts, path, sym, strc, rel, kno) = match qt {
        Q::ExactLookup | Q::DefinitionLookup => (0.5, 1.0, 1.0, 0.7, 0.3, 0.1),
        Q::SymbolNavigation => (0.5, 0.4, 1.0, 0.6, 0.9, 0.1),
        Q::ConfigurationLookup => (0.8, 1.0, 0.3, 0.3, 0.2, 0.5),
        Q::ArchitectureExplanation => (0.7, 0.4, 0.6, 0.9, 0.7, 0.9),
        Q::DebuggingRootCause => (0.8, 0.4, 0.6, 0.7, 0.6, 0.6),
        Q::ImpactAnalysis => (0.5, 0.4, 0.9, 0.7, 1.0, 0.2),
        Q::DependencyQuestion | Q::CrossRepoQuestion => (0.6, 0.4, 0.6, 0.5, 1.0, 0.4),
        Q::TestBehavior => (0.8, 0.5, 0.6, 0.5, 0.4, 0.5),
        Q::KnowledgeQuestion => (0.7, 0.4, 0.3, 0.2, 0.2, 1.0),
        Q::GenericSearch => (1.0, 0.8, 0.8, 0.6, 0.5, 0.6),
    };
    // Semantic-origin intent fit (independent of the other six columns).
    let sem = match qt {
        Q::ExactLookup | Q::DefinitionLookup | Q::ConfigurationLookup => 0.2,
        Q::GenericSearch | Q::KnowledgeQuestion => 0.85,
        Q::DebuggingRootCause | Q::ArchitectureExplanation => 0.75,
        _ => 0.55,
    };
    match kind {
        RetrieverKind::Fts => fts,
        RetrieverKind::Path => path,
        RetrieverKind::Symbol => sym,
        RetrieverKind::Structural => strc,
        RetrieverKind::Relationship => rel,
        RetrieverKind::Knowledge => kno,
        RetrieverKind::Semantic => sem,
    }
}

/// Fill any still-missing signals from candidate metadata, then compute the
/// combined score using the explicit weight table. Individual signals stay
/// observable on the returned evidence.
pub fn apply_signals_and_rank(
    mut ev: Evidence,
    qt: QueryType,
    kind: RetrieverKind,
    single_repo_scope: bool,
) -> Evidence {
    if ev.signals.lexical_score.is_none()
        && ev
            .retrieval_sources
            .iter()
            .any(|s| s.retriever_type == "FTS")
    {
        ev.signals.lexical_score = Some(normalize_lexical(ev.confidence.max(0.0)));
    }
    if ev.signals.query_intent_match.is_none() {
        ev.signals.query_intent_match = Some(origin_intent_match(qt, kind));
    }
    if ev.signals.repository_relevance.is_none() {
        ev.signals.repository_relevance = Some(if single_repo_scope { 1.0 } else { 0.8 });
    }
    if ev.signals.freshness_score.is_none() {
        ev.signals.freshness_score = Some(freshness_score(ev.freshness_state));
    }
    let w = intent_weights(qt);
    if ev.signals.knowledge_authority.is_none()
        && intent_weights(qt)[w::KNOWLEDGE] > 0.0
        && matches!(
            ev.authority,
            AuthorityLevel::ProjectKnowledge | AuthorityLevel::Doc
        )
    {
        ev.signals.knowledge_authority = Some(match ev.authority {
            AuthorityLevel::ProjectKnowledge => 1.0,
            _ => 0.7,
        });
    }
    if ev.signals.test_relevance.is_none()
        && intent_weights(qt)[w::TEST] > 0.0
        && ev.source_type == ST::Test
    {
        ev.signals.test_relevance = Some(1.0);
    }

    let vals = [
        ev.signals.lexical_score,
        ev.signals.symbol_match_score,
        ev.signals.query_intent_match,
        ev.signals.repository_relevance,
        ev.signals.freshness_score,
        ev.signals.structural_proximity,
        ev.signals
            .relationship_confidence
            .or(ev.relationship.as_ref().map(|r| r.confidence)),
        ev.signals.knowledge_authority,
        ev.signals.test_relevance,
        // Phase 5: set ONLY by the semantic generator; absence competes as
        // zero contribution but the weight still enters the denominator for
        // every item, so hybrid ordering stays explainable.
        ev.signals.semantic_score,
    ];
    let mut num = 0.0;
    let mut den = 0.0;
    for (i, v) in vals.iter().enumerate() {
        if w[i] > 0.0 {
            den += w[i];
            if let Some(v) = v {
                num += w[i] * v.clamp(0.0, 1.0);
            }
        }
    }
    ev.signals.combined_score = Some(if den > 0.0 { num / den } else { 0.0 });
    ev.confidence = ev.signals.combined_score.unwrap_or(0.0);
    ev
}

/// Sort evidence by combined score with deterministic tie-breaks.
pub fn sort_ranked(items: &mut [Evidence]) {
    items.sort_by(|a, b| {
        let sa = a.signals.combined_score.unwrap_or(0.0);
        let sb = b.signals.combined_score.unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| {
                a.source_span
                    .map(|s| s.start_line)
                    .cmp(&b.source_span.map(|s| s.start_line))
            })
            .then_with(|| a.id.cmp(&b.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::classify;

    fn ranked_for(
        query: &'static str,
        st: ST,
        kind: RetrieverKind,
        fresh: attic_core::FreshnessState,
    ) -> Evidence {
        let qt = classify(query).unwrap().query_type;
        let mut ev = Evidence::new("e", "repo");
        ev.source_type = st;
        ev.freshness_state = fresh;
        apply_signals_and_rank(ev, qt, kind, false)
    }

    #[test]
    fn definition_query_prefers_symbol_origin() {
        let sym = ranked_for(
            "Where is Router defined?",
            ST::SourceCode,
            RetrieverKind::Symbol,
            attic_core::FreshnessState::Current,
        );
        let lex = ranked_for(
            "Where is Router defined?",
            ST::SourceCode,
            RetrieverKind::Fts,
            attic_core::FreshnessState::Current,
        );
        assert!(sym.signals.combined_score.unwrap() > lex.signals.combined_score.unwrap());
    }

    #[test]
    fn current_beats_stale_all_else_equal() {
        let cur = ranked_for(
            "What port is configured?",
            ST::Configuration,
            RetrieverKind::Fts,
            attic_core::FreshnessState::Current,
        );
        let stale = ranked_for(
            "What port is configured?",
            ST::Configuration,
            RetrieverKind::Fts,
            attic_core::FreshnessState::Stale,
        );
        assert!(cur.signals.combined_score.unwrap() > stale.signals.combined_score.unwrap());
    }

    #[test]
    fn test_evidence_ranks_high_for_test_queries_only() {
        let for_test_q = ranked_for(
            "What does the auth test suite cover?",
            ST::Test,
            RetrieverKind::Fts,
            attic_core::FreshnessState::Current,
        );
        let for_def_q = ranked_for(
            "Where is AuthService defined?",
            ST::Test,
            RetrieverKind::Fts,
            attic_core::FreshnessState::Current,
        );
        assert!(for_test_q.signals.test_relevance.is_some());
        assert!(for_def_q.signals.test_relevance.is_none());
    }

    #[test]
    fn semantic_signal_never_set_in_phase4() {
        let e = ranked_for(
            "find anything",
            ST::SourceCode,
            RetrieverKind::Fts,
            attic_core::FreshnessState::Current,
        );
        assert!(e.signals.semantic_score.is_none());
    }

    #[test]
    fn combined_score_is_deterministic_and_bounded() {
        let a = ranked_for(
            "impact of changing UserService",
            ST::Relationship,
            RetrieverKind::Relationship,
            attic_core::FreshnessState::Current,
        );
        let b = ranked_for(
            "impact of changing UserService",
            ST::Relationship,
            RetrieverKind::Relationship,
            attic_core::FreshnessState::Current,
        );
        assert_eq!(a.signals.combined_score, b.signals.combined_score);
        assert!(a.signals.combined_score.unwrap() <= 1.0);
    }
}
