//! Phase 5 hardening round (post-review): panic-free store degradation,
//! enforceable kNN/scan budgets, provider deadline contract, and truthful
//! reranking observability.

mod common;

use std::sync::Arc;

use attic_retrieval::{
    AnswerMode, AnswerModePolicy, AnswerRequest, RetrievalService, semantic::SemanticStack,
};
use attic_semantic::{
    CancelFlag, EmbeddingInput, EnrichmentConfig, ResourceUsage, ScanBudget, SemanticProvider,
    testing::{HashingEmbedder, SlowProvider},
};
use common::Fixture;

// ── 1. Poisoned semantic-store mutex must degrade, never crash ─────────────

#[test]
fn poisoned_store_mutex_degrades_to_canonical_retrieval() {
    let fx = Fixture::bootstrap();
    let stack = SemanticStack::in_memory(Arc::new(HashingEmbedder::new()))
        .map(Arc::new)
        .unwrap();

    // Poison the mutex: panic while the lock is held, on a sacrificial
    // thread so THIS test survives; guard poisoning is what we are proving.
    let victim = stack.store.clone();
    let joined = std::thread::spawn(move || victim.debug_poison_mutex()).join();
    assert!(joined.is_err(), "poisoner must have panicked");

    // Every store operation now returns an error — no panics.
    let cancel = CancelFlag::new();
    let err = stack
        .store
        .count("hashing", "hashed-ngram-v1", None)
        .unwrap_err();
    assert!(err.to_string().contains("unavailable"), "{err}");
    let qerr = stack
        .store
        .knn_search_generation(
            1,
            &[1.0, 0.0],
            4,
            None,
            &ScanBudget::unbounded(&cancel),
        )
        .unwrap_err();
    assert!(qerr.to_string().contains("unavailable"), "{qerr}");

    // The PIPELINE degrades to canonical retrieval with the honest reason.
    let manual = RetrievalService {
        readers: fx.pool.clone(),
        writer: fx.writer.clone(),
        semantic: Some(stack.clone()),
        crossrepo_degraded: false,
    };
    let out = manual
        .answer(&AnswerRequest::new(
            "retry limit configuration",
            AnswerMode::Normal,
        ))
        .expect("canonical answer MUST still succeed with a poisoned store");
    assert!(
        matches!(out.result.as_str(), "SUCCESS" | "PARTIAL_SUCCESS"),
        "semantic failure corrupted the answer: {:?}",
        out.insufficient_reason
    );
    assert!(out.context_text.is_some());
    assert_eq!(
        out.plan.policy_trace.semantic_fallback_reason,
        "SEMANTIC_STORE_UNAVAILABLE"
    );
    assert!(!out.plan.policy_trace.semantic_invoked);
}



// ── 3. Provider deadline contract: slow backend cannot exceed budget ───────

#[test]
fn slow_provider_stops_within_query_deadline_and_pipeline_degrades() {
    // Provider-level conformance: 6 items × 80 ms vs a 200 ms deadline →
    // cooperative give-up well under any unbounded wait.
    let p = SlowProvider { delay_ms: 80 };
    let cancel = CancelFlag::new();
    let inputs: Vec<EmbeddingInput> = (0..6)
        .map(|i| EmbeddingInput {
            unit_key: format!("u{i}"),
            text: format!("text {i}"),
        })
        .collect();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let t0 = std::time::Instant::now();
    let r = p.embed_batch(
        &inputs,
        &cancel,
        &mut ResourceUsage::default(),
        Some(deadline),
    );
    let elapsed = t0.elapsed();
    assert!(matches!(
        r,
        Err(attic_semantic::SemanticError::Cancelled { .. })
    ));
    assert!(
        elapsed < std::time::Duration::from_millis(700),
        "provider ignored its deadline: {elapsed:?}"
    );

    // Pipeline-level: NORMAL's 250 ms semantic budget bounds the step; the
    // canonical answer still returns promptly with recorded degradation.
    let fx = Fixture::bootstrap();
    let stack = SemanticStack::in_memory(Arc::new(SlowProvider { delay_ms: 120 }))
        .map(Arc::new)
        .unwrap();
    {
        let conn = fx.read_conn();
        attic_semantic::reconcile(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &attic_semantic::SelectionConfig::default(),
        )
        .unwrap();
        attic_semantic::drive(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &EnrichmentConfig::standalone(4, 3, 300, 1),
            &CancelFlag::new(),
        )
        .unwrap();
    }
    let svc = fx.service_with_semantic(stack.provider.clone()).unwrap();
    let t1 = std::time::Instant::now();
    let out = svc
        .answer(&AnswerRequest::new(
            "payment charge logic",
            AnswerMode::Normal,
        ))
        .expect("answer under slow provider");
    let wall = t1.elapsed();
    assert!(out.context_text.is_some(), "canonical path must serve");
    assert!(
        wall < std::time::Duration::from_millis(2_000),
        "NORMAL answer waited too long on a slow provider: {wall:?}"
    );
}

// ── 4. reranking_invoked is TRUTHFUL ────────────────────────────────────────

#[test]
fn reranking_invoked_is_false_even_when_policy_permits_it() {
    let fx = Fixture::bootstrap();
    for mode in [AnswerMode::Normal, AnswerMode::Deep] {
        let out = fx.ask("retry_limit setting value", mode);
        assert!(
            !out.plan.policy_trace.reranking_invoked,
            "{mode}: permission is not execution — no reranker exists"
        );
        assert!(AnswerModePolicy::for_mode(mode).reranking_allowed);
    }
}
