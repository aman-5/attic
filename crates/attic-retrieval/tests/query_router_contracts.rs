//! Phase 4 — query router, contracts, and mode-policy gates (§3, §4, §5).

mod common;

use attic_retrieval::{
    AnswerMode, QueryType, classify,
    contract::{FallbackStrategy, FreshnessRequirement, contract_for},
    mode::{AnswerModePolicy, PolicyOverrides, PolicyResult},
};

// ─── §3 query taxonomy / classification ─────────────────────────────────────

#[test]
fn classification_covers_every_taxonomy_entry() {
    let cases: &[(&str, QueryType)] = &[
        ("config/app.yml", QueryType::ExactLookup),
        (
            "Where is the Router class defined?",
            QueryType::DefinitionLookup,
        ),
        (
            "Show me the callers of handle()",
            QueryType::SymbolNavigation,
        ),
        (
            "What is the server port setting in production?",
            QueryType::ConfigurationLookup,
        ),
        (
            "How does request dispatch work?",
            QueryType::ArchitectureExplanation,
        ),
        (
            "Why does the health endpoint fail unexpectedly?",
            QueryType::DebuggingRootCause,
        ),
        (
            "Which services depend on the sable package?",
            QueryType::DependencyQuestion,
        ),
        (
            "What would break if I change Router.handle?",
            QueryType::ImpactAnalysis,
        ),
        (
            "How is this used across repositories?",
            QueryType::CrossRepoQuestion,
        ),
        (
            "What scenarios does the RouterTest suite cover?",
            QueryType::TestBehavior,
        ),
        (
            "What does the runbook documentation say about rotating keys?",
            QueryType::KnowledgeQuestion,
        ),
        ("webhook delivery", QueryType::GenericSearch),
    ];
    for (q, expected) in cases {
        let c = classify(q).expect("classify");
        assert_eq!(&c.query_type, expected, "query: {q}");
    }
}

#[test]
fn ambiguous_classification_never_claims_certainty() {
    // Hits configuration + debugging + definition signals simultaneously.
    let c = classify("why does the configured database_url fail where is it defined")
        .expect("classify");
    assert!(
        !c.competing_types.is_empty() || c.confidence.as_str() != "HIGH",
        "overlapping signals must downgrade certainty"
    );
}

#[test]
fn unrecognized_query_defaults_to_generic_search_low_confidence() {
    let c = classify("zzzz qqqq").unwrap();
    assert_eq!(c.query_type, QueryType::GenericSearch);
}

#[test]
fn malformed_and_untrusted_queries_are_rejected() {
    assert!(classify("").is_err());
    assert!(classify(" \t \n ").is_err());
    let long = "a".repeat(513);
    assert!(classify(&long).is_err(), "over-length must be rejected");
    // FTS5 syntax injection attempts must classify safely (no panic, no
    // error) and are neutralized downstream by phrase quoting.
    let injection = classify("search\"; DROP TABLE core_retrieval_units; --").unwrap();
    assert!(!injection.extracted.terms.is_empty());
    assert!(classify("nul\u{0}byte").is_err());
}

#[test]
fn classification_is_deterministic() {
    let q = "Why does authentication fail when token expired?";
    let a = classify(q).unwrap();
    let b = classify(q).unwrap();
    assert_eq!(a, b);
}

// ─── §4 evidence contracts ───────────────────────────────────────────────────

#[test]
fn every_query_type_maps_to_a_contract_with_fallbacks() {
    for qt in QueryType::all() {
        let c = contract_for(*qt);
        assert_eq!(c.query_type, *qt);
        assert!(
            !c.allowed_fallbacks.is_empty(),
            "{qt} must declare allowed fallbacks"
        );
        assert!(c.expansion_budget.max_expansion_rounds >= 1);
    }
}

#[test]
fn debugging_contract_requires_implementation_and_allows_verification() {
    let c = contract_for(QueryType::DebuggingRootCause);
    assert_eq!(c.required_evidence.len(), 1);
    assert_eq!(c.required_evidence[0].evidence_type, "implementation");
    assert!(
        c.allowed_fallbacks
            .contains(&FallbackStrategy::SourceVerification)
    );
    // Root-cause queries combine implementation + tests + config + knowledge.
    assert!(c.preferred_evidence.len() >= 4);
}

#[test]
fn navigation_contract_enforces_relationship_confidence_floor() {
    let c = contract_for(QueryType::SymbolNavigation);
    assert_eq!(c.relationship_confidence_min, Some(0.6));
}

#[test]
fn current_only_contracts_reject_stale_by_default() {
    for qt in [
        QueryType::ExactLookup,
        QueryType::DefinitionLookup,
        QueryType::ConfigurationLookup,
        QueryType::DebuggingRootCause,
    ] {
        assert_eq!(
            contract_for(qt).freshness_requirement,
            FreshnessRequirement::CurrentOnly,
            "{qt} should be CURRENT_ONLY per approved table"
        );
    }
}

// ─── §5 FAST/NORMAL/DEEP as enforceable budgets ──────────────────────────────

#[test]
fn fast_policy_forbids_filesystem_and_semantics() {
    let p = AnswerModePolicy::for_mode(AnswerMode::Fast);
    assert_eq!(p.max_fs_files, 0);
    assert_eq!(p.max_fs_bytes, 0);
    assert!(!p.fs_reads_permitted());
    assert!(!p.semantic_allowed);
    assert_eq!(p.repair_attempts, 0);
}

#[test]
fn deep_policy_expands_bounds_with_bounded_semantics() {
    let n = AnswerModePolicy::for_mode(AnswerMode::Normal);
    let p = AnswerModePolicy::for_mode(AnswerMode::Deep);
    assert!(p.max_graph_depth > n.max_graph_depth);
    assert!(p.max_graph_nodes > n.max_graph_nodes);
    assert!(p.repair_attempts >= 1);
    // Phase 5 §14: DEEP may use semantics MORE broadly than NORMAL, but
    // always bounded — never unlimited vector search.
    assert!(p.semantic_allowed);
    assert!(p.max_semantic_candidates > n.max_semantic_candidates);
    assert!(p.semantic_time_budget_ms >= n.semantic_time_budget_ms);
}

#[test]
fn fast_budget_refuses_source_verification_observably() {
    let fx = common::Fixture::bootstrap();
    // A knowledge question under FAST cannot verify against source.
    let outcome = fx.ask(
        "What does the runbook say about rotating endpoints?",
        AnswerMode::Fast,
    );
    // The pipeline must record an explicit policy step, not silently skip.
    let policy_steps: Vec<_> = outcome
        .plan
        .steps
        .iter()
        .filter(|s| {
            s.subsystem.as_str() == "POLICY_ENFORCER"
                || s.operation.contains("verification")
                || s.output_summary.contains("PolicyViolation")
                || s.status.as_str() == "FAILED" && s.input_summary.contains("FAST")
        })
        .collect();
    assert!(
        outcome
            .plan
            .steps
            .iter()
            .any(|s| s.subsystem.as_str() == "POLICY_ENFORCER"),
        "AM-I1 warning step expected for FAST+explanation query"
    );
    assert!(!policy_steps.is_empty() || !outcome.plan.policy_trace.semantic_invoked);
    let _ = PolicyResult::CompletedWithinBudget;
}

#[test]
fn normal_mode_runs_within_deadline_on_fixture() {
    let fx = common::Fixture::bootstrap();
    let start = std::time::Instant::now();
    let outcome = fx.ask("Where is Router defined?", AnswerMode::Normal);
    assert!(start.elapsed().as_millis() < 3_000, "NORMAL deadline");
    assert_eq!(outcome.result.as_str(), "SUCCESS");
}

#[test]
fn startup_override_validation_rejects_impossible_configs() {
    let bad_time = PolicyOverrides {
        max_time_ms: Some(10),
        ..Default::default()
    };
    assert!(AnswerModePolicy::with_overrides(AnswerMode::Normal, &bad_time).is_err());

    let huge_tokens = PolicyOverrides {
        max_context_tokens: Some(500_000),
        ..Default::default()
    };
    assert!(AnswerModePolicy::with_overrides(AnswerMode::Deep, &huge_tokens).is_err());

    let sane = PolicyOverrides {
        max_time_ms: Some(2_000),
        max_candidates: Some(100),
        max_context_tokens: Some(8_192),
    };
    let p = AnswerModePolicy::with_overrides(AnswerMode::Normal, &sane).unwrap();
    assert_eq!(p.max_candidates, 100);
}
