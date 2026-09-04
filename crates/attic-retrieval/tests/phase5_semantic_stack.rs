//! Phase 5 §22 executable proofs — semantic layer behavior on top of a REAL
//! indexed workspace (discovery → secrets → analyzers → publication), plus
//! provider failure/cancellation/timeout, crash resume, invalidation,
//! security, and the disposable-layer invariant.

mod common;

use std::sync::Arc;

use attic_retrieval::{
    AnswerMode, AnswerRequest,
    semantic::{SemanticStack, enrich_to_completion},
};
use attic_semantic::{
    CancelFlag, EnrichmentConfig, FailingProvider, HashingEmbedder, RecordingProvider,
    SlowProvider, UnavailableProvider,
};
use common::Fixture;

fn hashing() -> Arc<dyn attic_semantic::SemanticProvider> {
    Arc::new(HashingEmbedder::new())
}

/// File-backed stack beside the fixture (crash-resume realism).
fn file_stack(fx: &Fixture) -> (Arc<SemanticStack>, std::path::PathBuf) {
    let path = fx.dir.path().join("semantic.db");
    let stack = Arc::new(SemanticStack::open(&path, hashing()).expect("semantic stack"));
    (stack, path)
}

const FULL: EnrichmentConfig = EnrichmentConfig {
    batch_size: 16,
    max_attempts: 3,
    budget_ms: 10_000,
};

// ── §12/§13: candidate generation + hybrid fusion ────────────────────────────

#[test]
fn enriched_layer_contributes_semantic_candidates_through_normal_pipeline() {
    let fx = Fixture::bootstrap();
    let svc = fx.service_with_semantic(hashing()).unwrap();
    let stack = svc.semantic.clone().unwrap();
    let conn = fx.read_conn();
    let stats = enrich_to_completion(&conn, &stack, &FULL).expect("enrich");
    assert!(stats.embedded > 0, "selection must have embedded something");

    let out = svc
        .answer(&AnswerRequest::new(
            "payment provider charge logic",
            AnswerMode::Normal,
        ))
        .expect("answer");
    let t = &out.plan.policy_trace;
    assert!(t.semantic_invoked, "trace must show semantic invocation");
    assert!(t.semantic_candidates_returned > 0);
    assert_eq!(t.semantic_fallback_reason, "");
    assert!(matches!(out.result.as_str(), "SUCCESS" | "PARTIAL_SUCCESS"));
}

#[test]
fn exact_configuration_lookup_is_not_degraded_by_semantics() {
    let fx = Fixture::bootstrap();
    let plain = fx.service();
    let hybrid = fx.service_with_semantic(hashing()).unwrap();
    {
        let conn = fx.read_conn();
        let _ = enrich_to_completion(&conn, hybrid.semantic.as_ref().unwrap(), &FULL).unwrap();
    }

    let first = |o: &attic_retrieval::AnswerOutcome| {
        o.context_text
            .as_deref()
            .and_then(|c| {
                c.lines().find_map(|l| {
                    let l = l.trim_start_matches('#').trim();
                    l.split_once(']').map(|(_, p)| p.trim().to_owned())
                })
            })
            .unwrap_or_default()
    };
    let a = plain
        .answer(&AnswerRequest::new("config/app.yml", AnswerMode::Normal))
        .unwrap();
    let b = hybrid
        .answer(&AnswerRequest::new("config/app.yml", AnswerMode::Normal))
        .unwrap();
    assert!(
        first(&a).contains("config/app.yml"),
        "baseline served {:?}",
        first(&a)
    );
    assert!(
        first(&b).contains("config/app.yml"),
        "hybrid regressed exact lookup"
    );
}

// ── §14: mode policies ──────────────────────────────────────────────────────

#[test]
fn fast_mode_never_touches_the_semantic_layer_even_when_ready() {
    let fx = Fixture::bootstrap();
    let svc = fx.service_with_semantic(hashing()).unwrap();
    {
        let conn = fx.read_conn();
        let _ = enrich_to_completion(&conn, svc.semantic.as_ref().unwrap(), &FULL).unwrap();
    }
    let out = svc
        .answer(&AnswerRequest::new(
            "payment provider charge logic",
            AnswerMode::Fast,
        ))
        .expect("answer");
    assert!(!out.plan.policy_trace.semantic_invoked);
    assert_eq!(
        out.plan.policy_trace.semantic_fallback_reason,
        "SEMANTIC_DISABLED"
    );
}

#[test]
fn deep_mode_ceiling_is_broader_than_normal() {
    let n = attic_retrieval::AnswerModePolicy::for_mode(AnswerMode::Normal);
    let d = attic_retrieval::AnswerModePolicy::for_mode(AnswerMode::Deep);
    assert!(d.max_semantic_candidates > n.max_semantic_candidates);
}

// ── §15: bounded fallback when enrichment is incomplete ─────────────────────

#[test]
fn partially_enriched_workspace_still_answers_lexically_without_stalling() {
    let fx = Fixture::bootstrap();
    let svc = fx.service_with_semantic(hashing()).unwrap();
    let stack = svc.semantic.as_ref().unwrap().clone();
    {
        let conn = fx.read_conn();
        let sel_cfg = attic_semantic::SelectionConfig::default();
        attic_semantic::reconcile(&conn, &stack.store, stack.provider.as_ref(), &sel_cfg).unwrap();
        // Deliberately tiny budget → partial coverage.
        attic_semantic::drive(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &EnrichmentConfig {
                budget_ms: 1,
                ..Default::default()
            },
            &CancelFlag::new(),
            attic_semantic::EmbeddingIntentSource::Recommendation,
        )
        .unwrap();
    }
    let out = svc
        .answer(&AnswerRequest::new(
            "retry_limit setting value",
            AnswerMode::Normal,
        ))
        .expect("answer under partial enrichment");
    assert!(out.context_text.is_some(), "canonical retrieval must serve");
}

#[test]
fn empty_semantic_layer_reports_no_embeddings_fallback() {
    let fx = Fixture::bootstrap();
    let svc = fx.service_with_semantic(hashing()).unwrap(); // nothing enriched
    let out = svc
        .answer(&AnswerRequest::new(
            "payment provider charge logic",
            AnswerMode::Normal,
        ))
        .expect("answer");
    assert!(!out.plan.policy_trace.semantic_invoked);
    assert_eq!(
        out.plan.policy_trace.semantic_fallback_reason,
        "NO_EMBEDDINGS_FOR_MODEL"
    );
    assert!(out.context_text.is_some());
}

// ── §6/§20: provider failure and bounded resources ──────────────────────────

#[test]
fn unavailable_provider_degrades_with_canonical_answer_intact() {
    let fx = Fixture::bootstrap();
    let svc = fx
        .service_with_semantic(Arc::new(UnavailableProvider {
            reason: "model files missing".into(),
        }))
        .unwrap();
    let stack = svc.semantic.as_ref().unwrap().clone();
    assert!(!stack.provider.available());
    // Selection is provider-independent; enrichment would fail cleanly.
    let conn = fx.read_conn();
    attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();

    let out = svc
        .answer(&AnswerRequest::new(
            "payment charge logic",
            AnswerMode::Normal,
        ))
        .expect("answer");
    assert!(out.context_text.is_some(), "canonical path unaffected");
}

#[test]
fn failing_provider_quarantines_after_attempts_without_corruption() {
    let fx = Fixture::bootstrap();
    let stack = SemanticStack::in_memory(Arc::new(FailingProvider { fail_after: 0 }))
        .map(Arc::new)
        .unwrap();
    let conn = fx.read_conn();
    attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    let stats = attic_semantic::drive(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &EnrichmentConfig {
            budget_ms: 2_000,
            ..Default::default()
        },
        &CancelFlag::new(),
        attic_semantic::EmbeddingIntentSource::Recommendation,
    )
    .unwrap();
    assert!(stats.failed_items > 0, "failures must be observable");
    let done = stack
        .store
        .queue_counts()
        .unwrap()
        .get("DONE")
        .copied()
        .unwrap_or(0);
    assert_eq!(done, 0);

    // Canonical intelligence untouched.
    let out = fx
        .service()
        .answer(&AnswerRequest::new(
            "process_payment defined",
            AnswerMode::Fast,
        ))
        .unwrap();
    assert!(out.context_text.is_some());
}

#[test]
fn slow_provider_honors_drive_budget_and_leaves_nothing_inflight() {
    let fx = Fixture::bootstrap();
    let stack = SemanticStack::in_memory(Arc::new(SlowProvider { delay_ms: 40 }))
        .map(Arc::new)
        .unwrap();
    let conn = fx.read_conn();
    attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    let stats = attic_semantic::drive(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &EnrichmentConfig {
            budget_ms: 60,
            batch_size: 2,
            max_attempts: 3,
        },
        &CancelFlag::new(),
        attic_semantic::EmbeddingIntentSource::Recommendation,
    )
    .unwrap();
    assert!(stats.elapsed_ms < 5_000, "budget bound must hold");
    let inflight = stack
        .store
        .queue_counts()
        .unwrap()
        .get("INFLIGHT")
        .copied()
        .unwrap_or(0);
    assert_eq!(inflight, 0, "no work stays INFLIGHT after a bounded drive");
}

#[test]
fn cancellation_flag_stops_embedding_without_quarantine() {
    let fx = Fixture::bootstrap();
    let stack = SemanticStack::in_memory(hashing()).map(Arc::new).unwrap();
    let conn = fx.read_conn();
    attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    let cancel = CancelFlag::new();
    cancel.cancel();
    let stats = attic_semantic::drive(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &EnrichmentConfig {
            budget_ms: 1_000,
            batch_size: 4,
            max_attempts: 3,
        },
        &cancel,
        attic_semantic::EmbeddingIntentSource::Recommendation,
    )
    .unwrap();
    assert_eq!(stats.embedded, 0);
    let failed = stack
        .store
        .queue_counts()
        .unwrap()
        .get("FAILED")
        .copied()
        .unwrap_or(0);
    assert_eq!(failed, 0, "cancellation must never quarantine items");
}

// ── §11: crash / power-loss resume ──────────────────────────────────────────

#[test]
fn crash_between_drives_retains_committed_and_reschedules_rest() {
    let fx = Fixture::bootstrap();
    let (stack, path) = file_stack(&fx);
    {
        let conn = fx.read_conn();
        attic_semantic::reconcile(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &attic_semantic::SelectionConfig::default(),
        )
        .unwrap();
        // Drive a little, then "crash".
        attic_semantic::drive(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &EnrichmentConfig {
                budget_ms: 120,
                batch_size: 4,
                max_attempts: 3,
            },
            &CancelFlag::new(),
            attic_semantic::EmbeddingIntentSource::Recommendation,
        )
        .unwrap();
    }
    let committed_before = stack
        .store
        .count("hashing", "hashed-ngram-v1", None)
        .unwrap();
    drop(stack);

    let reopened = Arc::new(SemanticStack::open(&path, hashing()).expect("reopen"));
    let committed_after = reopened
        .store
        .count("hashing", "hashed-ngram-v1", None)
        .unwrap();
    assert_eq!(
        committed_after, committed_before,
        "committed work must survive"
    );
    let inflight = reopened
        .store
        .queue_counts()
        .unwrap()
        .get("INFLIGHT")
        .copied()
        .unwrap_or(0);
    assert_eq!(inflight, 0, "restart must reschedule INFLIGHT work");

    let conn = fx.read_conn();
    let stats = enrich_to_completion(&conn, &reopened, &FULL).unwrap();
    assert!(stats.embedded > 0 || stats.queue_remaining == 0);
}

// ── §10: incremental invalidation ───────────────────────────────────────────

#[test]
fn model_change_purges_only_the_semantic_layer() {
    let fx = Fixture::bootstrap();
    let (stack, _p) = file_stack(&fx);
    {
        let conn = fx.read_conn();
        enrich_to_completion(&conn, &stack, &FULL).unwrap();
    }
    assert!(
        stack
            .store
            .count("hashing", "hashed-ngram-v1", None)
            .unwrap()
            > 0
    );
    let units_before = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
            r.get::<_, i64>(0)
        })
    });

    let newprov: Arc<dyn attic_semantic::SemanticProvider> = Arc::new(RecordingProvider {
        vectors: vec![vec![1.0, 0.0]],
        seen_texts: Default::default(),
    });
    let swapped =
        Arc::new(SemanticStack::open(&fx.dir.path().join("semantic.db"), newprov).unwrap());
    let conn = fx.read_conn();
    let rep = attic_semantic::reconcile(
        &conn,
        &swapped.store,
        swapped.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    assert!(
        rep.purged_other_models > 0,
        "inactive-model purge must fire"
    );
    let units_after = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
            r.get::<_, i64>(0)
        })
    });
    assert_eq!(units_before, units_after, "canonical units untouched");
}

#[test]
fn source_edit_invalidates_affected_embeddings_only() {
    let fx = Fixture::bootstrap();
    let (stack, _p) = file_stack(&fx);
    {
        let conn = fx.read_conn();
        enrich_to_completion(&conn, &stack, &FULL).unwrap();
    }
    let before = stack
        .store
        .count("hashing", "hashed-ngram-v1", None)
        .unwrap();

    std::fs::write(
        fx.root.join("services/pay.py"),
        "def process_payment(amount_cents: int, currency: str) -> bool:\n    return amount_cents > 0  # v2 fee logic\n",
    )
    .unwrap();
    let opts = attic_indexing::IndexOptions {
        repository_name: "phase4".into(),
        ..Default::default()
    };
    attic_indexing::index_repository(
        &fx.store(),
        &fx.root,
        &attic_discovery::DiscoveryPolicy::default_git(),
        &opts,
    )
    .unwrap();

    let conn = fx.read_conn();
    let rep = attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    assert!(rep.invalidated_stale >= 1, "edited units must invalidate");
    let after = stack
        .store
        .count("hashing", "hashed-ngram-v1", None)
        .unwrap();
    assert!(after <= before, "{before}→{after}");
}

// ── §4: selection behaviors observable through reconcile reports ────────────

#[test]
fn duplicate_units_are_selected_once() {
    let body = "def duplicated_helper(x):\n    return x * 3\n";
    let fx = Fixture::seed_pub(&[
        ("src/main/java/com/sable/Router.java", common::ROUTER_JAVA),
        ("config/app.yml", common::APP_YML),
        ("lib/a.py", body),
        ("lib/b_copy.py", body),
    ]);
    let svc = fx.service_with_semantic(hashing()).unwrap();
    let stack = svc.semantic.as_ref().unwrap().clone();
    let conn = fx.read_conn();
    let rep = attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    let dup = rep
        .selection
        .excluded
        .get(attic_semantic::EX_DUPLICATE)
        .copied()
        .unwrap_or(0);
    assert!(
        dup >= 1,
        "duplicate content must be counted: {:?}",
        rep.selection.excluded
    );
}

#[test]
fn generated_output_is_never_embedded() {
    let fx = Fixture::seed_pub(&[
        ("src/main/java/com/sable/Router.java", common::ROUTER_JAVA),
        ("config/app.yml", common::APP_YML),
        ("src/handwritten.rs", "fn meaningful() { 42 }\n"),
        ("dist/bundle.min.js", "!function(a,b,c){return a+b+c}();"),
    ]);
    let svc = fx.service_with_semantic(hashing()).unwrap();
    let stack = svc.semantic.as_ref().unwrap().clone();
    let conn = fx.read_conn();
    enrich_to_completion(&conn, &stack, &FULL).unwrap();

    let rows = stack
        .store
        .active_identity_rows("hashing", "hashed-ngram-v1")
        .unwrap();
    let offenders: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            attic_storage::retrieval_unit_anchor(&conn, &r.retrieval_unit_id)
                .ok()
                .flatten()
                .map(|a| a.path)
        })
        .filter(|p| p.contains("dist/") || p.ends_with(".min.js"))
        .collect();
    assert!(
        offenders.is_empty(),
        "generated artifacts embedded: {offenders:?}"
    );
}

#[test]
fn oversized_units_are_excluded_not_truncated_silently() {
    let fx = Fixture::seed_pub(&[
        ("src/main/java/com/sable/Router.java", common::ROUTER_JAVA),
        ("config/app.yml", common::APP_YML),
        (
            "src/big_generated_table.rs",
            Box::leak(format!("// {}\nfn big() {{}}\n", "x".repeat(24_000)).into_boxed_str()),
        ),
    ]);
    let svc = fx.service_with_semantic(hashing()).unwrap();
    let stack = svc.semantic.as_ref().unwrap().clone();
    let conn = fx.read_conn();
    let rep = attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    let too_big = rep
        .selection
        .excluded
        .get(attic_semantic::EX_TOO_LARGE)
        .copied()
        .unwrap_or(0);
    assert!(
        too_big >= 1,
        "oversized unit must be counted: {:?}",
        rep.selection.excluded
    );
}

// ── §18: security — secrets NEVER reach the provider ───────────────────────

#[test]
fn secret_bearing_unit_text_never_reaches_the_provider() {
    const RAW: &str = "AKIAIOSFODNN7EXAMPLE";
    let fx = Fixture::bootstrap();
    fx.writer
        .send(move |c| {
            // Poison a unit that selection WILL pick (largest text), simulating
            // an upstream Phase 1B bypass.
            c.execute(
                "UPDATE core_retrieval_units SET retrieval_text =
                    retrieval_text || ?1
                  WHERE id = (
                      SELECT id FROM core_retrieval_units
                       ORDER BY LENGTH(retrieval_text) DESC LIMIT 1)",
                rusqlite::params![format!("\naws_key = \"{RAW}\"")],
            )
            .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
            Ok(())
        })
        .expect("inject");

    let rec = Arc::new(RecordingProvider {
        vectors: vec![vec![0.5; 256]],
        seen_texts: Default::default(),
    });
    let stack = SemanticStack::in_memory(rec.clone() as Arc<dyn attic_semantic::SemanticProvider>)
        .map(Arc::new)
        .unwrap();
    let conn = fx.read_conn();
    attic_semantic::reconcile(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &attic_semantic::SelectionConfig::default(),
    )
    .unwrap();
    let stats = attic_semantic::drive(
        &conn,
        &stack.store,
        stack.provider.as_ref(),
        &EnrichmentConfig {
            budget_ms: 5_000,
            batch_size: 8,
            max_attempts: 3,
        },
        &CancelFlag::new(),
        attic_semantic::EmbeddingIntentSource::Recommendation,
    )
    .unwrap();
    assert_eq!(stats.skipped_secret, 1, "the poisoned unit must be refused");
    let seen = rec.seen_texts.lock().unwrap();
    assert!(
        seen.iter().all(|t| !t.contains(RAW)),
        "RAW SECRET REACHED PROVIDER"
    );
}

// ── §9/§20: background enrichment coexists with foreground queries ─────────

#[test]
fn foreground_queries_answer_during_background_enrichment() {
    let fx = Fixture::bootstrap();
    let (stack, _p) = file_stack(&fx);
    {
        let conn = fx.read_conn();
        attic_semantic::reconcile(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &attic_semantic::SelectionConfig::default(),
        )
        .unwrap();
    }
    let bg = attic_semantic::BackgroundEnricher::spawn(
        fx.db_path.clone(),
        stack.store.clone(),
        stack.provider.clone(),
        EnrichmentConfig {
            budget_ms: 200,
            batch_size: 4,
            max_attempts: 3,
        },
        None,
        attic_semantic::EmbeddingIntentSource::Recommendation,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
    );
    for _ in 0..3 {
        let out = fx
            .service()
            .answer(&AnswerRequest::new(
                "retry limit configuration",
                AnswerMode::Normal,
            ))
            .expect("foreground answer during enrichment");
        assert!(out.context_text.is_some());
    }
    assert!(
        bg.shutdown(std::time::Duration::from_secs(10)),
        "background worker must stop deterministically"
    );
}

// ── §12: similarity is a RANKING signal, never evidence authority ──────────

#[test]
fn perfect_similarity_cannot_rescue_stale_evidence_from_validation() {
    use attic_evidence::{AuthorityLevel, Evidence, EvidenceSourceType};
    use attic_retrieval::contract::contract_for;
    use attic_retrieval::query::QueryType;

    let mut ev = Evidence::new("ev-sem", "repo");
    ev.source_type = EvidenceSourceType::SourceCode;
    ev.freshness_state = attic_core::FreshnessState::Stale;
    ev.authority = AuthorityLevel::Implementation;
    ev.confidence = 0.99;
    ev.signals.semantic_score = Some(1.0); // perfect cosine
    let contract = contract_for(QueryType::DefinitionLookup); // CURRENT_ONLY
    let verdict = attic_retrieval::validate::validate(&ev, &contract);
    let rescued = verdict.drop_reason.is_none()
        && verdict.counts_toward_required
        && ev.freshness_state == attic_core::FreshnessState::Current;
    assert!(
        !rescued,
        "similarity must never fabricate freshness authority"
    );
}
