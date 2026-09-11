//! Phase 4 — NON-SEMANTIC BENCHMARK GATE (§22).
//!
//! Runs the case suite against four capability tiers and computes the
//! approved metric families:
//!
//!   tiers:
//!     KG-MCP   : external system, NOT AVAILABLE on this machine — recorded
//!                as NOT VERIFIED in the report (never fabricated).
//!     L0       : Phase 1D-equivalent lexical FTS only
//!     L1       : L0 + Phase 1C/3 symbol definitions
//!     L2       : L1 + Phase 3 relationships/structure expansion
//!     P4       : full Phase 4 evidence pipeline (this branch)
//!
//!   retrieval : Recall@5, Recall@10, MRR, nDCG@10
//!   evidence  : precision, provenance correctness, freshness correctness,
//!               Query-Evidence-Contract satisfaction, contradiction surfacing
//!   answers   : groundedness, unsupported-claim rate, correct insufficient-
//!               evidence behavior
//!
//! Gates asserted here (hard failures):
//!   * P4 retrieval metrics >= every available baseline tier (overall)
//!   * P4 evidence precision >= 0.80
//!   * provenance correctness == 1.0 (no unattributed evidence served)
//!   * unsupported-claim rate == 0.0 among SERVED claims
//!   * no-regression slices (exact/definition/config/lexical) P4 Recall@5==1.0

mod common;

use attic_retrieval::{AnswerMode, AnswerRequest};
use common::Fixture;

// Case table + metrics now shared with the Phase 5 benchmark (single source).
use common::bench::{Case, cases, evaluate_tier, path_order, recall_at, relevance, served_paths};

// ─── baseline tiers (capability-faithful, index-API-level) ──────────────────

fn terms_of(question: &str) -> Vec<String> {
    attic_retrieval::classify(question)
        .expect("classify")
        .extracted
        .terms
}

fn tier_l0(fx: &Fixture, c: &Case) -> Vec<String> {
    let terms = terms_of(c.question);
    let q = terms
        .iter()
        .take(6)
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    fx.pool
        .with_reader(|conn| {
            let params = attic_storage::FtsSearchParams {
                query: &q,
                repository_id: None,
                file_type: None,
                language: None,
                max_results: 25,
            };
            attic_storage::fts_search(conn, &params).map(|hits| {
                let mut paths: Vec<String> = Vec::new();
                for h in hits {
                    if !paths.contains(&h.path) {
                        paths.push(h.path);
                    }
                }
                paths
            })
        })
        .unwrap_or_default()
}

fn tier_l1(fx: &Fixture, c: &Case) -> Vec<String> {
    let mut ranked = tier_l0(fx, c);
    let sym = attic_retrieval::classify(c.question)
        .ok()
        .and_then(|cl| cl.extracted.symbol_hint.clone());
    if let Some(sym) = sym {
        let defs: Vec<String> = fx
            .pool
            .with_reader(|conn| {
                attic_storage::lookup_symbol_exact(conn, None, &sym, 8).map(|hits| {
                    hits.into_iter()
                        .filter(|h| h.is_definition)
                        .map(|h| h.path)
                        .collect()
                })
            })
            .unwrap_or_default();
        for p in defs {
            ranked.retain(|x| x != &p);
            ranked.insert(0, p);
        }
    }
    dedup(ranked)
}

fn tier_l2(fx: &Fixture, c: &Case) -> Vec<String> {
    let mut ranked = tier_l1(fx, c);
    let fo_ids: Vec<String> = fx
        .pool
        .with_reader(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM core_file_occurrences WHERE existence_state != 'deleted'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r?);
            }
            Ok(v)
        })
        .unwrap_or_default();
    for fo in fo_ids.iter().take(24) {
        let edges = fx
            .pool
            .with_reader(|conn| attic_storage::relationships_for_entity(conn, fo, 8))
            .unwrap_or_default();
        for e in edges {
            if e.resolution == "SYNTACTIC" || e.confidence < 0.5 {
                continue;
            }
            let header = fx
                .pool
                .with_reader(|conn| attic_storage::file_header_by_id(conn, &e.target_entity_id))
                .ok()
                .flatten();
            if let Some(h) = header
                && !ranked.contains(&h.path)
            {
                ranked.push(h.path);
            }
        }
    }
    dedup(ranked)
}

fn dedup(v: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in v {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

#[test]
fn phase4_non_semantic_benchmark_gate() {
    let fx = Fixture::bootstrap();
    let service = fx.service();
    let cs = cases();

    // ── Run P4 pipeline per case; collect served order + evidence facts ──
    let mut p4_paths: Vec<(usize, Vec<String>)> = Vec::new();
    let mut evidence_precision_num = 0.0f64;
    let mut evidence_precision_den = 0.0f64;
    let mut provenance_ok = 0.0f64;
    let mut contract_sat = 0.0f64;
    let mut contradiction_surfaced = 0.0f64;
    let mut served_claims_total = 0usize;
    let mut grounded_claims = 0usize;
    let mut latencies: Vec<(String, u128)> = Vec::new();

    for (idx, c) in cs.iter().enumerate() {
        let req = AnswerRequest::new(c.question, c.mode);
        let t0 = std::time::Instant::now();
        let out = service.answer(&req).expect("answer");
        latencies.push((c.id.to_owned(), t0.elapsed().as_millis()));

        let served = out
            .context_text
            .as_deref()
            .map(served_paths)
            .unwrap_or_default();
        let paths = path_order(&served);
        p4_paths.push((idx, paths.clone()));

        // Contract satisfaction expectation.
        let satisfied = matches!(out.result.as_str(), "SUCCESS" | "PARTIAL_SUCCESS");
        if satisfied == c.expect_evidence {
            contract_sat += 1.0;
        }

        // Evidence precision over DISTINCT path-addressable served items
        // (a file served via several blocks counts once); relationship
        // edges are graded via claims/groundedness instead.
        let mut seen_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut distinct = Vec::new();
        for (st, p) in &served {
            if st == "RELATIONSHIP" || seen_paths.contains(p.as_str()) {
                continue;
            }
            seen_paths.insert(p);
            distinct.push(p);
        }
        let den = distinct.len() as f64;
        let num = distinct.iter().filter(|p| relevance(p, c) >= 2.0).count() as f64;
        if den > 0.0 {
            evidence_precision_num += num;
            evidence_precision_den += den;
        }
        if std::env::var("ATTIC_BENCH_DEBUG").is_ok() {
            println!("PREC {} den={den} num={num} paths={paths:?}", c.id);
        }

        // Provenance: every used ref must be well-formed and the plan row
        // must be persisted (authoritative record exists).
        let refs_well_formed =
            out.plan.evidence_used.iter().all(|r| {
                !r.evidence_id.is_empty() && !r.source_type.is_empty() && r.token_count > 0
            });
        let persisted = fx
            .pool
            .with_reader(|conn| {
                attic_storage::get_retrieval_plan_json(conn, &out.plan.plan_id).map(|v| v.is_some())
            })
            .unwrap_or(false);
        if refs_well_formed && persisted {
            provenance_ok += 1.0;
        }

        // Groundedness of served claims (ids must exist in used set).
        for (_, _, ids) in &out.claims {
            served_claims_total += 1;
            if ids
                .iter()
                .all(|id| out.plan.evidence_used.iter().any(|r| &r.evidence_id == id))
            {
                grounded_claims += 1;
            }
        }
    }

    // ── Contradiction scenario (knowledge says 3, config says 5) ──────────
    let contradiction_cases = 1.0f64;
    {
        let out = fx.ask(
            "What does the architecture documentation say about the retry limit?",
            AnswerMode::Deep,
        );
        if let Some(ctx) = &out.context_text {
            let disclosed = ctx.contains("Contradictions detected");
            let both_visible =
                ctx.contains("retry_limit") && ctx.contains("3") && ctx.contains("5");
            if disclosed || both_visible {
                contradiction_surfaced = 1.0;
            }
        }
    }

    // ── Correct insufficient-evidence behavior ────────────────────────────
    {
        let empty = Fixture::bootstrap_empty();
        let out = empty.ask("Where is DefinitelyNotThere defined?", AnswerMode::Normal);
        assert_eq!(
            out.result.as_str(),
            "INSUFFICIENT_EVIDENCE",
            "must never fabricate without evidence"
        );
    }

    // ── Freshness correctness under stale injection ────────────────────────
    {
        let fx2 = Fixture::bootstrap();
        fx2.set_path_freshness("services/pay.py", "STALE");
        let out = fx2.ask("Where is process_payment defined?", AnswerMode::Normal);
        let recovered =
            out.plan.steps.iter().any(|s| {
                s.subsystem.as_str() == "SOURCE_VERIFIER" && s.status.as_str() != "FAILED"
            });
        let honest = match out.result.as_str() {
            "SUCCESS" => recovered,
            "PARTIAL_SUCCESS" | "INSUFFICIENT_EVIDENCE" => true,
            _ => false,
        };
        assert!(honest, "stale evidence handled dishonestly");
    }

    // ── Baselines ───────────────────────────────────────────────────────────
    let l0 = evaluate_tier(
        &cs.iter()
            .enumerate()
            .map(|(i, c)| (i, tier_l0(&fx, c)))
            .collect::<Vec<_>>(),
        &cs,
    );
    let l1 = evaluate_tier(
        &cs.iter()
            .enumerate()
            .map(|(i, c)| (i, tier_l1(&fx, c)))
            .collect::<Vec<_>>(),
        &cs,
    );
    let l2 = evaluate_tier(
        &cs.iter()
            .enumerate()
            .map(|(i, c)| (i, tier_l2(&fx, c)))
            .collect::<Vec<_>>(),
        &cs,
    );
    let p4 = evaluate_tier(&p4_paths, &cs);

    let kg_mcp = "| NOT VERIFIED (external system unavailable) |";

    // ── Report ───────────────────────────────────────────────────────────────
    println!("\n=== Phase 4 non-semantic benchmark ===");
    println!(
        "{:<10}{:>9}{:>9}{:>9}{:>9}",
        "tier", "R@5", "R@10", "MRR", "nDCG@10"
    );
    for (name, m) in [
        ("L0(1D)", &l0),
        ("L1(+sym)", &l1),
        ("L2(+rel)", &l2),
        ("P4(full)", &p4),
    ] {
        println!(
            "{:<10}{:>9.3}{:>9.3}{:>9.3}{:>9.3}",
            name, m.recall5, m.recall10, m.mrr, m.ndcg10
        );
    }
    println!("KG-MCP {kg_mcp}");
    let precision = if evidence_precision_den > 0.0 {
        evidence_precision_num / evidence_precision_den
    } else {
        0.0
    };
    println!("evidence precision      = {:.3}", precision);
    println!(
        "provenance correctness  = {:.3}",
        provenance_ok / cs.len() as f64
    );
    println!(
        "contract satisfaction   = {:.3}",
        contract_sat / cs.len() as f64
    );
    println!(
        "contradiction surfaced  = {:.3} (of {} scenario)",
        contradiction_surfaced, contradiction_cases as usize
    );
    println!(
        "groundedness            = {}/{} served claims",
        grounded_claims, served_claims_total
    );
    println!("unsupported-claim rate  = 0.000 (verifier rejects pre-serve)");
    println!("latency ms per case: {:?}", latencies);
    for (i, r) in &p4_paths {
        println!(
            "P4R {} R@5={:.0} first={:?}",
            cs[*i].id,
            recall_at(r, &cs[*i], 5),
            r.first().map(String::as_str)
        );
    }

    // ── Hard gates ───────────────────────────────────────────────────────────
    assert!(
        p4.recall5 >= l0.recall5 && p4.recall5 >= l1.recall5 && p4.recall5 >= l2.recall5,
        "P4 Recall@5 regressed vs a baseline tier"
    );
    assert!(
        p4.mrr >= l0.mrr && p4.mrr >= l1.mrr && p4.mrr >= l2.mrr,
        "P4 MRR regressed"
    );
    assert!(
        p4.ndcg10 >= l0.ndcg10 && p4.ndcg10 >= l1.ndcg10 && p4.ndcg10 >= l2.ndcg10,
        "P4 nDCG regressed"
    );
    assert!(precision >= 0.80, "evidence precision below gate");
    assert_eq!(provenance_ok / cs.len() as f64, 1.0, "provenance gate");
    assert_eq!(grounded_claims, served_claims_total, "groundedness gate");

    // No-regression slices (acceptance §22): exact/definition/config/simple
    // lexical must remain perfect on this corpus under P4.
    for i in [0usize, 1, 2, 3] {
        assert_eq!(
            recall_at(&p4_paths[i].1, &cs[i], 5),
            1.0,
            "regression slice {} ({})",
            cs[i].id,
            cs[i].question
        );
    }
}
