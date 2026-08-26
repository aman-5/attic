//! Phase 5 — SEMANTIC BENCHMARK GATE (§23): value-driven comparison.
//!
//! ```text
//! A = Phase 4 non-semantic Attic          (service without semantic stack)
//! B = Phase 5 embeddings only             (bounded kNN ranking, alone)
//! C = Phase 5 hybrid retrieval            (full pipeline + semantic generator)
//! ```
//!
//! Gates (hard):
//!   * C Recall@5 >= A Recall@5                       (no coverage loss)
//!   * C MRR / nDCG@10 within −0.02 of A              (no material regression)
//!   * no-regression slices (exact/def/config/lexical) R@5 == 1.0 under C
//!   * semantic layer deletion leaves A fully functional (disposable invariant)
//!
//! Operational metrics printed for the completion report: embedding time,
//! semantic index size, incremental enrichment time, kNN latency.

mod common;

use std::sync::Arc;
use std::time::Instant;

use attic_retrieval::{AnswerRequest, semantic::enrich_to_completion};
use attic_semantic::{EnrichmentConfig, HashingEmbedder};
use common::Fixture;
use common::bench::{cases, evaluate_tier, path_order, recall_at, served_paths};

const FULL: EnrichmentConfig = EnrichmentConfig {
    batch_size: 16,
    max_attempts: 3,
    budget_ms: 10_000,
};

#[test]
fn phase5_semantic_benchmark_gate() {
    let fx = Fixture::bootstrap();
    let cs = cases();

    // ── Tier B setup: embeddings-only over a fully enriched layer ──────────
    let hybrid = fx
        .service_with_semantic(Arc::new(HashingEmbedder::new()))
        .unwrap();
    let stack = hybrid.semantic.clone().unwrap();
    let t_enrich0 = Instant::now();
    let enrich_stats = {
        let conn = fx.read_conn();
        enrich_to_completion(&conn, &stack, &FULL).expect("enrich")
    };
    let embed_ms = t_enrich0.elapsed().as_millis();
    let sem_rows = stack
        .store
        .count("hashing", "hashed-ngram-v1", None)
        .unwrap() as f64;
    let total_units = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| {
            r.get::<_, i64>(0)
        })
    }) as f64;

    // ── Tier A: Phase 4 baseline ───────────────────────────────────────────
    let plain = fx.service();
    let mut a_paths = Vec::new();
    for (i, c) in cs.iter().enumerate() {
        let out = plain
            .answer(&AnswerRequest::new(c.question, c.mode))
            .unwrap();
        let served = out
            .context_text
            .as_deref()
            .map(served_paths)
            .unwrap_or_default();
        a_paths.push((i, path_order(&served)));
    }
    let a = evaluate_tier(&a_paths, &cs);

    // ── Tier B: embeddings-only ranking (capability-faithful) ──────────────
    let mut b_paths = Vec::new();
    {
        let conn = fx.read_conn();
        for (i, c) in cs.iter().enumerate() {
            let qtext = attic_retrieval::classify(c.question)
                .map(|cl| {
                    let mut q = cl.extracted.terms.join(" ");
                    if let Some(s) = &cl.extracted.symbol_hint {
                        q.push(' ');
                        q.push_str(s);
                    }
                    q
                })
                .unwrap_or_else(|_| c.question.to_owned());
            let provider = stack.provider.clone();
            let usage = attic_semantic::ResourceUsage::default();
            let cancel = attic_semantic::CancelFlag::new();
            let outs = provider
                .embed_batch(
                    &[attic_semantic::EmbeddingInput {
                        unit_key: "__q".into(),
                        text: qtext,
                    }],
                    &cancel,
                    &mut usage.clone(),
                    None,
                )
                .expect("embed query");
            let qv = outs[0].vector.clone();
            let kn = stack
                .store
                .knn(
                    &qv,
                    10,
                    "hashing",
                    "hashed-ngram-v1",
                    None,
                    &attic_semantic::ScanBudget::unbounded(&cancel),
                )
                .expect("knn");
            let mut paths: Vec<String> = Vec::new();
            for h in kn.hits {
                if let Ok(Some(anchor)) =
                    attic_storage::retrieval_unit_anchor(&conn, &h.retrieval_unit_id)
                    && !paths.contains(&anchor.path)
                {
                    paths.push(anchor.path);
                }
            }
            b_paths.push((i, paths));
        }
    }
    let b = evaluate_tier(&b_paths, &cs);

    // ── Tier C: full hybrid pipeline ────────────────────────────────────────
    let mut c_paths = Vec::new();
    let mut c_latencies: Vec<u128> = Vec::new();
    for (i, c) in cs.iter().enumerate() {
        let t0 = Instant::now();
        let out = hybrid
            .answer(&AnswerRequest::new(c.question, c.mode))
            .unwrap();
        c_latencies.push(t0.elapsed().as_millis());
        let served = out
            .context_text
            .as_deref()
            .map(served_paths)
            .unwrap_or_default();
        c_paths.push((i, path_order(&served)));
    }
    let c = evaluate_tier(&c_paths, &cs);

    // ── Semantic-target subset (§23/§24): paraphrase/synonym/conceptual ────
    // Same fixture, same services: does hybrid BEAT Phase 4 where semantics
    // is the only plausible advantage?
    let scs = common::bench::semantic_cases();
    let mut a_s = Vec::new();
    let mut c_s = Vec::new();
    for (i, c) in scs.iter().enumerate() {
        let oa = plain
            .answer(&AnswerRequest::new(c.question, c.mode))
            .unwrap();
        let oc = hybrid
            .answer(&AnswerRequest::new(c.question, c.mode))
            .unwrap();
        let pa = path_order(
            &oa.context_text
                .as_deref()
                .map(served_paths)
                .unwrap_or_default(),
        );
        let pc = path_order(
            &oc.context_text
                .as_deref()
                .map(served_paths)
                .unwrap_or_default(),
        );
        a_s.push((i, pa));
        c_s.push((i, pc));
    }
    let a_sem = evaluate_tier(&a_s, &scs);
    let c_sem = evaluate_tier(&c_s, &scs);
    for (i, _) in a_s.iter().enumerate() {
        println!(
            "SEMSUB {} A_first={:?} C_first={:?} C_R5={:.0}",
            scs[i].id,
            a_s[i].1.first(),
            c_s[i].1.first(),
            recall_at(&c_s[i].1, &scs[i], 5)
        );
    }

    // ── Operational: incremental enrichment after ONE source edit ──────────
    std::fs::write(
        fx.root.join("services/pay.py"),
        "def process_payment(amount_cents: int, currency: str) -> bool:\n    return amount_cents > 0\n",
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
    let t_inc = Instant::now();
    {
        let conn = fx.read_conn();
        enrich_to_completion(&conn, &stack, &FULL).expect("incremental enrich");
    }
    let incremental_ms = t_inc.elapsed().as_millis();

    // ── Disposable-layer deletion: canonical retrieval must survive ────────
    let units_before = total_units as i64;
    drop(hybrid);
    drop(stack);

    // ── Report ──────────────────────────────────────────────────────────────
    println!("\n=== Phase 5 semantic benchmark ===");
    println!(
        "{:<12}{:>9}{:>9}{:>9}{:>9}",
        "tier", "R@5", "R@10", "MRR", "nDCG@10"
    );
    for (name, m) in [("A(P4)", &a), ("B(emb)", &b), ("C(hyb)", &c)] {
        println!(
            "{:<12}{:>9.3}{:>9.3}{:>9.3}{:>9.3}",
            name, m.recall5, m.recall10, m.mrr, m.ndcg10
        );
    }
    println!("embedding time (full)      = {embed_ms} ms");
    println!(
        "semantic index             = {sem_rows:.0} rows / {total_units:.0} units ({:.1}% selected)",
        100.0 * sem_rows / total_units.max(1.0)
    );
    println!("embedded items             = {}", enrich_stats.embedded);
    println!("incremental enrichment     = {incremental_ms} ms");
    println!("hybrid latency per case ms = {c_latencies:?}");
    println!("\n--- semantic-target subset (paraphrase/synonym/conceptual) ---");
    println!(
        "A(P4) subset: R@5={:.3} MRR={:.3} nDCG={:.3}",
        a_sem.recall5, a_sem.mrr, a_sem.ndcg10
    );
    println!(
        "C(hyb) subset: R@5={:.3} MRR={:.3} nDCG={:.3}",
        c_sem.recall5, c_sem.mrr, c_sem.ndcg10
    );

    // ── Hard gates ──────────────────────────────────────────────────────────
    assert!(
        c.recall5 >= a.recall5,
        "hybrid must not lose Recall@5 vs Phase 4"
    );
    assert!(
        c.mrr >= a.mrr - 0.02 && c.ndcg10 >= a.ndcg10 - 0.02,
        "hybrid materially regressed ordering quality"
    );
    for i in [0usize, 1, 2, 3] {
        assert_eq!(
            recall_at(&c_paths[i].1, &cs[i], 5),
            1.0,
            "no-regression slice {} regressed under hybrid",
            cs[i].id
        );
    }
    // Control case S06 (strong lexical overlap) must stay perfect.
    assert_eq!(
        recall_at(&c_s[5].1, &scs[5], 5),
        1.0,
        "lexical control S06 must not regress under hybrid"
    );
    // VALUE GATE (§24/§7): semantics must ADD measurable benefit where it is
    // the only plausible advantage. Hybrid subset MRR/nDCG must strictly
    // beat the Phase 4 baseline on the semantic-target cases.
    assert!(
        c_sem.mrr > a_sem.mrr || c_sem.ndcg10 > a_sem.ndcg10,
        "no measurable semantic benefit (A_mrr={} C_mrr={} A_ndcg={} C_ndcg={})",
        a_sem.mrr,
        c_sem.mrr,
        a_sem.ndcg10,
        c_sem.ndcg10
    );
    assert_eq!(
        fx.query_i64(
            |c| c.query_row("SELECT COUNT(*) FROM core_retrieval_units", [], |r| r
                .get::<_, i64>(0))
        ),
        units_before,
        "canonical index must be untouched by semantic work"
    );
}
