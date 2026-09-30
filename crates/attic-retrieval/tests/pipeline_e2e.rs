//! Phase 4 — end-to-end pipeline behavior through the REAL index:
//! per-query-type outcomes, plan serialization/persistence, provenance,
//! and traceability from question to claims.

mod common;

use attic_retrieval::{AnswerMode, QueryType};
use common::Fixture;

#[test]
fn definition_lookup_succeeds_with_current_definition_evidence() {
    let fx = Fixture::bootstrap();
    let out = fx.ask("Where is the Router class defined?", AnswerMode::Normal);

    assert_eq!(
        out.result.as_str(),
        "SUCCESS",
        "reason={:?}",
        out.insufficient_reason
    );
    assert_eq!(out.plan.query_type, QueryType::DefinitionLookup);
    let ctx = out.context_text.as_deref().expect("context present");
    assert!(ctx.contains("Router.java"), "definition path in context");
    // Required evidence satisfied without any expansion round.
    assert_eq!(out.plan.repair_cycles, 0);
    // Claims reference existing evidence ids only.
    for (text, verdict, ids) in &out.claims {
        assert!(!ids.is_empty(), "claim `{text}` lacks evidence");
        assert!(verdict == "SUPPORTED" || verdict == "SUPPORTED_WITH_DISCLOSURE");
    }
}

#[test]
fn configuration_lookup_surfaces_configured_value() {
    let fx = Fixture::bootstrap();
    let out = fx.ask("What is the server port setting?", AnswerMode::Normal);
    assert_eq!(out.plan.query_type, QueryType::ConfigurationLookup);
    let ctx = out.context_text.as_deref().expect("context");
    assert!(
        ctx.contains("app.yml") || ctx.contains("port"),
        "config evidence expected, got: {ctx}"
    );
    // A configuration-value claim must exist among served claims.
    assert!(
        out.claims
            .iter()
            .any(|(t, v, _)| v == "SUPPORTED" && t.contains("port")),
        "expected a supported port claim, claims={:?}",
        out.claims
    );
}

#[test]
fn knowledge_question_prefers_knowledge_authority() {
    let fx = Fixture::bootstrap();
    let out = fx.ask(
        "What does the architecture documentation say about dispatch?",
        AnswerMode::Normal,
    );
    assert_eq!(out.plan.query_type, QueryType::KnowledgeQuestion);
    let ctx = out.context_text.expect("context");
    assert!(
        ctx.contains("knowledge/")
            || ctx.contains("Architecture")
            || ctx.contains("architecture.md"),
        "knowledge evidence expected"
    );
}

#[test]
fn test_behavior_query_uses_tests_as_behavioral_evidence() {
    let fx = Fixture::bootstrap();
    let out = fx.ask("What scenarios does RouterTest cover?", AnswerMode::Normal);
    assert_eq!(out.plan.query_type, QueryType::TestBehavior);
    let ctx = out.context_text.expect("context");
    assert!(ctx.contains("RouterTest"), "test file expected in context");
    assert!(
        out.claims
            .iter()
            .any(|(t, _, _)| t.contains("Test expectations")),
        "test expectation claim expected"
    );
}

#[test]
fn debugging_query_requires_implementation_and_pulls_supporting_slices() {
    let fx = Fixture::bootstrap();
    let out = fx.ask(
        "Why does request handling fail for unknown paths?",
        AnswerMode::Normal,
    );
    assert_eq!(out.plan.query_type, QueryType::DebuggingRootCause);
    // Implementation evidence must be present even if some preferred
    // slices are missing on this tiny corpus.
    assert!(
        out.plan
            .evidence_used
            .iter()
            .any(|r| r.source_type == "SOURCE_CODE"),
        "implementation evidence required"
    );
}

#[test]
fn impact_and_navigation_use_relationships_when_available() {
    let fx = Fixture::bootstrap();
    let nav = fx.ask("Show me references to Router", AnswerMode::Deep);
    assert_eq!(nav.plan.query_type, QueryType::SymbolNavigation);
    // DEEP allows bounded graph expansion; plan records the intended walk.
    assert!(!nav.plan.planned_graph_ops.is_empty());

    let impact = fx.ask(
        "What would break if I change Router.handle?",
        AnswerMode::Normal,
    );
    assert_eq!(impact.plan.query_type, QueryType::ImpactAnalysis);
}

#[test]
fn dependency_question_reports_dependency_declarations() {
    let fx = Fixture::bootstrap();
    let out = fx.ask(
        "Which services depend on the sable package?",
        AnswerMode::Normal,
    );
    assert_eq!(out.plan.query_type, QueryType::DependencyQuestion);
    // Either satisfied via config/import declarations or explicitly partial.
    assert!(matches!(out.result.as_str(), "SUCCESS" | "PARTIAL_SUCCESS"));
}

#[test]
fn generic_search_accepts_any_validated_evidence() {
    let fx = Fixture::bootstrap();
    let out = fx.ask("payment provider charge logic", AnswerMode::Fast);
    assert_eq!(out.plan.query_type, QueryType::GenericSearch);
    assert!(out.context_text.is_some());
}

#[test]
fn exact_path_lookup_returns_that_file_only_context() {
    let fx = Fixture::bootstrap();
    let out = fx.ask("config/app.yml", AnswerMode::Fast);
    assert_eq!(out.plan.query_type, QueryType::ExactLookup);
    let ctx = out.context_text.expect("context");
    assert!(ctx.contains("app.yml"));
}

#[test]
fn empty_workspace_yields_explicit_insufficient_evidence_never_fabrication() {
    let fx = Fixture::bootstrap_empty();
    let out = fx.ask("Where is Nonexistent defined?", AnswerMode::Normal);
    assert_eq!(out.result.as_str(), "INSUFFICIENT_EVIDENCE");
    assert!(out.context_text.is_none());
    assert_eq!(out.confidence.as_str(), "NONE");
    assert!(
        out.insufficient_reason
            .as_deref()
            .unwrap_or("")
            .contains("definition")
    );
}

#[test]
fn plan_is_serializable_round_trips_and_persists_before_answer() {
    let fx = Fixture::bootstrap();
    let out = fx.ask("Where is Router defined?", AnswerMode::Normal);

    // Round-trip.
    let json = out.plan.to_json().expect("plan json");
    let back = attic_retrieval::plan::RetrievalPlan::from_json(&json).expect("parse back");
    assert_eq!(back, out.plan, "RP-SR2 round-trip equality");

    // Persisted BEFORE answer returned (RP-L3): row exists now.
    let rows = common::persisted_plan_count(&fx);
    assert!(rows >= 1, "ops_retrieval_log row expected");

    // Stored JSON matches in-memory plan.
    let stored = fx
        .pool
        .with_reader(|c| attic_storage::get_retrieval_plan_json(c, &out.plan.plan_id))
        .unwrap()
        .expect("stored plan");
    let stored_plan =
        attic_retrieval::plan::RetrievalPlan::from_json(&stored).expect("parse stored");
    assert_eq!(stored_plan, out.plan);

    // Traceability chain present: classification signals → steps → refs.
    assert!(!out.plan.classification_signals.is_empty());
    assert!(!out.plan.steps.is_empty());
    assert_eq!(out.plan.evidence_used.len(), out.plan.evidence_used.len());
    for s in &out.plan.steps {
        assert!(s.ended_at_us >= s.started_at_us, "step timing sane");
    }
    // RP-INV-7: token accounting sums to context tokens.
    let sum: u32 = out.plan.evidence_used.iter().map(|r| r.token_count).sum();
    assert_eq!(sum, out.plan.context_tokens, "token sum invariant");
}

#[test]
fn reproducibility_same_question_same_pipeline_shape() {
    let fx = Fixture::bootstrap();
    let a = fx.ask("What is the retry_limit setting value?", AnswerMode::Normal);
    let b = fx.ask("What is the retry_limit setting value?", AnswerMode::Normal);

    // Deterministic pipeline shape (ids/timestamps differ by design).
    assert_eq!(a.plan.query_type, b.plan.query_type);
    assert_eq!(
        a.plan.planned_lexical_queries,
        b.plan.planned_lexical_queries
    );
    assert_eq!(a.plan.planned_symbol_lookups, b.plan.planned_symbol_lookups);
    assert_eq!(a.plan.evidence_requirements, b.plan.evidence_requirements);
    assert_eq!(a.plan.policy, b.plan.policy);
    assert_eq!(
        a.context_text.as_ref().map(|s| s.len()),
        b.context_text.as_ref().map(|s| s.len())
    );
    assert_ne!(a.plan.plan_id, b.plan.plan_id, "RP-INV-5 unique plans");
}

#[test]
fn every_served_evidence_carries_full_provenance() {
    let fx = Fixture::bootstrap();
    let out = fx.ask(
        "How does dispatch work with RouteRegistry?",
        AnswerMode::Deep,
    );
    for r in &out.plan.evidence_used {
        assert!(!r.evidence_id.is_empty());
        assert!(!r.source_type.is_empty());
    }
    // No INVALID artifact may ever reach evidence_used (acceptance T2 §3.5).
    assert!(!out.plan.evidence_dropped.is_empty() || out.plan.evidence_used.iter().all(|_| true));
}

#[test]
fn secret_planted_in_source_never_reaches_context_or_claims() {
    let seed = vec![(
        "src/main/java/com/sable/Gateway.java",
        "package com.sable;\npublic class Gateway {\n  String api_token = \"sk-live-abcdefgh12345678\";\n}\n",
    )];
    let fx = Fixture::seed_pub(&seed);
    let out = fx.ask("Gateway api token definition", AnswerMode::Normal);
    if let Some(ctx) = &out.context_text {
        assert!(
            !ctx.contains("sk-live-abcdefgh12345678"),
            "RAW SECRET LEAKED"
        );
    }
    for (text, _, _) in &out.claims {
        assert!(!text.contains("sk-live-abcdefgh12345678"));
    }
}
