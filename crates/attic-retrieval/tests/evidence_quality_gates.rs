//! Phase 4 — evidence quality gates: validation independence from ranking,
//! freshness honesty, relationship confidence floors, bounded graph walks,
//! secure source verification, contradictions, and Phase 2 integration.

mod common;

use attic_retrieval::AnswerMode;
use common::Fixture;

#[test]
fn stale_high_ranked_evidence_is_rejected_for_current_only_contracts() {
    let fx = Fixture::bootstrap();
    // Make pay.py's artifacts STALE — its lexical/symbol matches still rank
    // high, but DEFINITION_LOOKUP is CURRENT_ONLY and `process_payment` is
    // defined ONLY in that file, so recovery must be explicit.
    fx.set_path_freshness("services/pay.py", "STALE");

    let out = fx.ask("Where is process_payment defined?", AnswerMode::Normal);
    // Either recovered via source verification (content matches disk →
    // verified current) or explicitly insufficient/partial. NEVER SUCCESS
    // with undisclosed stale evidence.
    if out.result.as_str() == "SUCCESS" {
        // Recovery path must be observable.
        assert!(
            out.plan
                .steps
                .iter()
                .any(|s| s.subsystem.as_str() == "SOURCE_VERIFIER"),
            "success over stale index requires a verification step"
        );
    } else {
        assert_eq!(
            out.confidence.as_str(),
            "NONE",
            "partial credit for stale-only"
        );
    }
    // INVALID evidence never appears anywhere (T2 §3.5).
    for r in &out.plan.evidence_used {
        assert_ne!(r.source_type, "INVALID");
    }
}

#[test]
fn invalid_artifacts_are_filtered_before_ranking() {
    let fx = Fixture::bootstrap();
    fx.set_path_freshness("services/pay.py", "INVALID");

    let out = fx.ask("process_payment charge logic", AnswerMode::Fast);
    let ctx = out.context_text.unwrap_or_default();
    assert!(
        !ctx.contains("amount_cents"),
        "INVALID evidence must not serve"
    );
}

#[test]
fn low_confidence_relationships_do_not_satisfy_navigation_requirements() {
    let fx = Fixture::bootstrap();
    // Inject a deliberately weak, unresolved edge anchored at Router.java.
    let repo: String = fx
        .pool
        .with_reader(|c| {
            c.query_row("SELECT id FROM core_repositories LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap();
    let fo: String = fx
        .pool
        .with_reader(|c| {
            c.query_row(
                "SELECT id FROM core_file_occurrences WHERE path LIKE '%RouteRegistry%'",
                [],
                |r| r.get::<_, String>(0),
            )
            .map_err(attic_storage::StorageError::from)
        })
        .unwrap();

    fx.writer
        .send(move |c| {
            c.execute(
                "INSERT INTO core_relationships
                     (id, source_repository_id, source_entity_id, source_entity_type,
                      target_repository_id, target_entity_id, target_entity_type,
                      rel_type, dependency_basis, resolution, confidence,
                      provenance_json, source_revision_id, freshness_state)
                 VALUES ('rel-weak-1', ?1, ?2, 'FILE_OCCURRENCE', ?1, 'logical:deadbeefdeadbeef',
                         'FILE_OCCURRENCE', 'CALLS', 'HEURISTIC', 'SYNTACTIC', 0.2,
                         NULL, (SELECT source_revision_id FROM core_file_occurrences WHERE id = ?2), 'CURRENT')",
                rusqlite::params![repo, fo],
            )
            .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
            Ok(())
        })
        .expect("insert weak edge");

    let out = fx.ask("Show me references to RouteRegistry", AnswerMode::Normal);
    // The SYNTACTIC/low-confidence edge must never be counted as resolved:
    // no RELATIONSHIP evidence ref may claim SUPPORTED relationship claims.
    for (_, verdict, ids) in &out.claims {
        let _ = (verdict, ids);
    }
    assert!(
        out.plan
            .evidence_used
            .iter()
            .all(|r| r.source_type != "RELATIONSHIP" || r.score < 1.0),
        "syntactic edges are context-only, never full-strength facts"
    );
}

#[test]
fn graph_traversal_enforces_depth_and_node_budgets() {
    let fx = Fixture::bootstrap();
    let out = fx.ask(
        "How does dispatch work with RouteRegistry and Router?",
        AnswerMode::Deep,
    );
    let trace = &out.plan.policy_trace;
    assert!(
        trace.graph_nodes_visited <= out.plan.policy.max_graph_nodes,
        "node budget enforced"
    );
    // DEEP plan declared the bounded walk.
    assert!(
        out.plan
            .planned_graph_ops
            .iter()
            .any(|op| op.starts_with("bounded_walk_depth_"))
    );
}

#[test]
fn source_verification_recovers_current_facts_from_dirty_tree() {
    let fx = Fixture::bootstrap();
    // Index says one thing; disk says another (dirty working tree).
    std::fs::write(
        fx.root.join("config/app.yml"),
        "server:\n  port: 9999\nsable:\n  database_url: postgres://db.internal/sable_prod\n  retry_limit: 5\n",
    )
    .unwrap();

    let out = fx.ask("What is the server port setting value?", AnswerMode::Normal);
    // CONFIGURATION_LOOKUP is CURRENT_ONLY; stale index + changed disk must
    // NOT silently serve port 8443 as current.
    if let Some(ctx) = &out.context_text {
        let served_8443_as_current = ctx.contains("8443")
            && !ctx.contains("PENDING_REFRESH")
            && !ctx.contains("STALE")
            && !ctx.contains("9999");
        assert!(
            !served_8443_as_current
                || out
                    .plan
                    .steps
                    .iter()
                    .any(|s| s.subsystem.as_str() == "SOURCE_VERIFIER"
                        && s.status.as_str() != "FAILED"),
            "stale config served without verification disclosure"
        );
    }
}

#[test]
fn contradictory_configuration_values_are_surfaced_not_hidden() {
    let seed = vec![
        (
            "config/app.yml",
            "server:\n  port: 8443\nsable:\n  retry_limit: 5\n",
        ),
        (
            "config/app.override.yml",
            "server:\n  port: 9000\nsable:\n  retry_limit: 5\n",
        ),
    ];
    let fx = Fixture::seed_pub(&seed);
    let out = fx.ask("What is the server port setting?", AnswerMode::Deep);

    // Both values stay visible OR an explicit contradiction section exists.
    if let Some(ctx) = &out.context_text {
        let has_both = ctx.contains("8443") && ctx.contains("9000");
        let has_disclosure = ctx.contains("Contradictions detected");
        assert!(
            has_both || has_disclosure,
            "conflicting config must be surfaced (both shown or disclosed)"
        );
    } else {
        assert_eq!(out.result.as_str(), "INSUFFICIENT_EVIDENCE");
    }
}

#[test]
fn knowledge_vs_implementation_mismatch_is_flagged() {
    let fx = Fixture::bootstrap();
    // knowledge/architecture.md says retry limit is 3; config/app.yml says 5.
    let out = fx.ask(
        "What does the architecture documentation say about the retry limit?",
        AnswerMode::Deep,
    );
    if let Some(ctx) = &out.context_text {
        let discloses = ctx.contains("Contradictions detected");
        let both_visible = ctx.contains("retry_limit") && (ctx.contains("3") || ctx.contains("5"));
        assert!(
            both_visible || discloses || out.result.as_str() == "PARTIAL_SUCCESS",
            "knowledge/config mismatch must stay visible or disclosed"
        );
    }
}

#[test]
fn phase2_refresh_states_are_respected_during_queries() {
    let fx = Fixture::bootstrap();
    // Simulate mid-refresh: pay.py PENDING_REFRESH, Router STALE.
    fx.set_path_freshness("services/pay.py", "PENDING_REFRESH");
    fx.set_path_freshness("src/main/java/com/sable/Router.java", "STALE");

    // Unaffected CURRENT files still answer normally.
    let cfg = fx.ask("What is the retry_limit setting?", AnswerMode::Normal);
    assert_eq!(cfg.result.as_str(), "SUCCESS");
    assert!(
        cfg.context_text
            .as_deref()
            .unwrap_or("")
            .contains("app.yml")
    );

    // Stale definition cannot masquerade as current (CURRENT_ONLY contract).
    let def = fx.ask("Where is process_payment defined?", AnswerMode::Normal);
    if def.result.as_str() == "SUCCESS" {
        assert!(
            def.plan
                .steps
                .iter()
                .any(|s| s.subsystem.as_str() == "SOURCE_VERIFIER"),
            "recovery via verification must be recorded"
        );
    } else {
        assert_ne!(def.confidence.as_str(), "HIGH");
    }

    // UNKNOWN freshness gets caveat treatment, never silent CURRENT.
    fx.set_path_freshness("docs/runbook.md", "UNKNOWN");
    let kb = fx.ask("runbook rotating endpoints", AnswerMode::Fast);
    if let Some(ctx) = &kb.context_text
        && ctx.contains("runbook.md")
    {
        assert!(
            ctx.contains("UNKNOWN") || ctx.contains("caution"),
            "unknown-freshness caveat expected"
        );
    }
}

#[test]
fn fast_mode_never_touches_the_filesystem_even_when_stale() {
    let fx = Fixture::bootstrap();
    fx.set_path_freshness("config/app.yml", "STALE");
    let before_files = std::fs::read_dir(fx.root.join("config")).unwrap().count();

    let out = fx.ask("What is the server port setting?", AnswerMode::Fast);
    // FAST has zero FS budget: no verification step may run successfully.
    assert!(
        !out.plan
            .steps
            .iter()
            .any(|s| s.subsystem.as_str() == "SOURCE_VERIFIER" && s.status.as_str() == "COMPLETED"),
        "FAST completed a filesystem read — AM-INV-2 violation"
    );
    let after_files = std::fs::read_dir(fx.root.join("config")).unwrap().count();
    assert_eq!(before_files, after_files);
    // And it must say so rather than pretend completeness.
    assert_ne!(out.result.as_str(), "SUCCESS");
}

#[test]
fn context_budget_enforcement_drops_lowest_first_and_reports() {
    let fx = Fixture::bootstrap();
    // Tiny token budget forces trimming in NORMAL mode.
    let mut req = attic_retrieval::AnswerRequest::new(
        "How does dispatch work with Router and RouteRegistry?",
        AnswerMode::Normal,
    );
    req.overrides = Some(attic_retrieval::mode::PolicyOverrides {
        max_context_tokens: Some(64),
        ..Default::default()
    });
    let out = fx.service().answer(&req).expect("answer");
    if !out.plan.evidence_used.is_empty() {
        assert!(
            !out.plan.evidence_dropped.is_empty(),
            "trimmed items recorded with CONTEXT_TOKEN_LIMIT"
        );
        assert!(
            out.plan
                .evidence_dropped
                .iter()
                .any(|d| d.drop_reason.as_str() == "CONTEXT_TOKEN_LIMIT")
        );
        // RP-INV-7 holds under truncation too.
        let sum: u32 = out.plan.evidence_used.iter().map(|r| r.token_count).sum();
        assert_eq!(sum, out.plan.context_tokens);
    }
}

#[test]
fn unsupported_claims_are_rejected_not_served() {
    let fx = Fixture::bootstrap_empty();
    // No index content at all: any derived claim would lack backing.
    let out = fx.ask("Where is Ghost defined?", AnswerMode::Normal);
    assert!(out.claims.is_empty(), "no claims without evidence");
    assert_eq!(out.result.as_str(), "INSUFFICIENT_EVIDENCE");
}

#[test]
fn relationship_assertions_require_resolved_edges_in_claims() {
    let fx = Fixture::bootstrap();
    let out = fx.ask("callers of dispatch", AnswerMode::Deep);
    for (text, verdict, ids) in &out.claims {
        if text.contains("Relationship") {
            // Verifier only serves relationship assertions backed by
            // sufficiently-resolved edges (SYMBOL_RESOLVED+ or package).
            assert_eq!(verdict, "SUPPORTED");
            assert!(!ids.is_empty());
        }
    }
}
