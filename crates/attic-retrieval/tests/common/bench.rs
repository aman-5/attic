//! Shared benchmark case table + metric helpers (Phase 4 §22 / Phase 5 §23).

/// One benchmark case: question, mode, graded expectations.
pub struct Case {
    pub id: &'static str,
    pub question: &'static str,
    pub mode: attic_retrieval::AnswerMode,
    /// Paths that fully answer the question (relevance 3).
    pub expected: &'static [&'static str],
    /// Supporting paths (relevance 2).
    pub related: &'static [&'static str],
    /// Whether the contract is satisfiable on this corpus.
    pub expect_evidence: bool,
}

pub fn cases() -> Vec<Case> {
    vec![
        Case {
            id: "B01",
            question: "Where is the Router class defined?",
            mode: attic_retrieval::AnswerMode::Fast,
            expected: &["src/main/java/com/sable/Router.java"],
            related: &["src/test/java/com/sable/RouterTest.java"],
            expect_evidence: true,
        },
        Case {
            id: "B02",
            question: "Where is process_payment defined?",
            mode: attic_retrieval::AnswerMode::Fast,
            expected: &["services/pay.py"],
            related: &[],
            expect_evidence: true,
        },
        Case {
            id: "B03",
            question: "config/app.yml",
            mode: attic_retrieval::AnswerMode::Fast,
            expected: &["config/app.yml"],
            related: &[],
            expect_evidence: true,
        },
        Case {
            id: "B04",
            question: "What is the server port setting?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["config/app.yml"],
            related: &["docs/runbook.md"],
            expect_evidence: true,
        },
        Case {
            id: "B05",
            question: "What is the retry_limit setting value?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["config/app.yml"],
            related: &["knowledge/architecture.md"],
            expect_evidence: true,
        },
        Case {
            id: "B06",
            question: "What does the architecture documentation say about dispatch?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["knowledge/architecture.md"],
            related: &["src/main/java/com/sable/RouteRegistry.java"],
            expect_evidence: true,
        },
        Case {
            id: "B07",
            question: "What does the runbook say about rotating endpoints?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["docs/runbook.md"],
            related: &[],
            expect_evidence: true,
        },
        Case {
            id: "B08",
            question: "What scenarios does RouterTest cover?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["src/test/java/com/sable/RouterTest.java"],
            related: &["src/main/java/com/sable/Router.java"],
            expect_evidence: true,
        },
        Case {
            id: "B09",
            question: "Show me references to RouteRegistry",
            mode: attic_retrieval::AnswerMode::Deep,
            expected: &[
                "src/main/java/com/sable/RouteRegistry.java",
                "src/main/java/com/sable/Router.java",
            ],
            related: &[],
            expect_evidence: true,
        },
        Case {
            id: "B10",
            question: "What would break if I change Router.handle?",
            mode: attic_retrieval::AnswerMode::Deep,
            expected: &["src/main/java/com/sable/Router.java"],
            related: &["src/main/java/com/sable/RouteRegistry.java"],
            expect_evidence: true,
        },
        Case {
            id: "B11",
            question: "Which services depend on the sable package?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["config/app.yml"],
            related: &["src/main/java/com/sable/RouteRegistry.java"],
            expect_evidence: true,
        },
        Case {
            id: "B12",
            question: "Why does request handling fail for unknown paths?",
            mode: attic_retrieval::AnswerMode::Deep,
            expected: &["src/main/java/com/sable/Router.java"],
            related: &[
                "src/test/java/com/sable/RouterTest.java",
                "knowledge/architecture.md",
            ],
            expect_evidence: true,
        },
        Case {
            id: "B13",
            question: "payment provider charge logic",
            mode: attic_retrieval::AnswerMode::Fast,
            expected: &["services/pay.py"],
            related: &[],
            expect_evidence: true,
        },
    ]
}

pub fn relevance(path: &str, c: &Case) -> f64 {
    if c.expected.iter().any(|e| path.contains(e)) {
        3.0
    } else if c.related.iter().any(|r| path.contains(r)) {
        2.0
    } else {
        0.0
    }
}

/// Parse served-context order (`## [TYPE] path:span` headers).
pub fn served_paths(context: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in context.lines() {
        let line = line.trim_start_matches('#').trim();
        if let Some(rest) = line.strip_prefix('[')
            && let Some(close) = rest.find(']')
        {
            let stype = rest[..close].to_owned();
            let after = rest[close + 1..].trim();
            let path = after.split_whitespace().next().unwrap_or("");
            let path = path.split(':').next().unwrap_or(path);
            if !path.is_empty() {
                out.push((stype, path.to_owned()));
            }
        }
    }
    out
}

pub fn path_order(served: &[(String, String)]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (_, p) in served {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

pub fn recall_at(ranked: &[String], c: &Case, k: usize) -> f64 {
    if ranked.iter().take(k).any(|p| relevance(p, c) >= 3.0) {
        1.0
    } else {
        0.0
    }
}

pub fn mrr(ranked: &[String], c: &Case) -> f64 {
    for (i, p) in ranked.iter().enumerate() {
        if relevance(p, c) >= 3.0 {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

pub fn ndcg_at(ranked: &[String], c: &Case, k: usize) -> f64 {
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, p)| relevance(p, c) / (i as f64 + 2.0).log2())
        .sum();
    let mut ideal_rels: Vec<f64> = c.expected.iter().map(|_| 3.0).collect();
    ideal_rels.extend(c.related.iter().map(|_| 2.0));
    ideal_rels.truncate(k);
    let idcg: f64 = ideal_rels
        .iter()
        .enumerate()
        .map(|(i, rel)| rel / (i as f64 + 2.0).log2())
        .sum();
    if idcg <= 0.0 { 0.0 } else { dcg / idcg }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TierMetrics {
    pub recall5: f64,
    pub recall10: f64,
    pub mrr: f64,
    pub ndcg10: f64,
}

pub fn evaluate_tier(per_case: &[(usize, Vec<String>)], cs: &[Case]) -> TierMetrics {
    let n = cs.len() as f64;
    TierMetrics {
        recall5: per_case
            .iter()
            .map(|(i, r)| recall_at(r, &cs[*i], 5))
            .sum::<f64>()
            / n,
        recall10: per_case
            .iter()
            .map(|(i, r)| recall_at(r, &cs[*i], 10))
            .sum::<f64>()
            / n,
        mrr: per_case.iter().map(|(i, r)| mrr(r, &cs[*i])).sum::<f64>() / n,
        ndcg10: per_case
            .iter()
            .map(|(i, r)| ndcg_at(r, &cs[*i], 10))
            .sum::<f64>()
            / n,
    }
}

/// Semantic-TARGET cases (Phase 5 value gate): each question is written so
/// its content words do NOT appear verbatim in the target unit (FTS5
/// unicode61 has no stemming) — pure paraphrase / synonym / conceptual /
/// weak-overlap queries where embeddings are the ONLY plausible advantage.
/// S06 is a strong-lexical CONTROL that must not regress.
pub fn semantic_cases() -> Vec<Case> {
    vec![
        Case {
            id: "S01",
            question: "How are incoming HTTP requests routed to their handlers?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["src/main/java/com/sable/Router.java"],
            related: &["src/main/java/com/sable/RouteRegistry.java"],
            expect_evidence: true,
        },
        Case {
            id: "S02",
            question: "Which component dispatches URLs to controller classes?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["src/main/java/com/sable/RouteRegistry.java"],
            related: &["src/main/java/com/sable/Router.java"],
            expect_evidence: true,
        },
        Case {
            id: "S03",
            question: "What happens when someone visits an undefined endpoint?",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["src/main/java/com/sable/Router.java"],
            related: &[],
            expect_evidence: true,
        },
        Case {
            id: "S04",
            question: "transaction processing implementation",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["services/pay.py"],
            related: &[],
            expect_evidence: true,
        },
        Case {
            id: "S05",
            question: "verification suite for the routing behavior",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["src/test/java/com/sable/RouterTest.java"],
            related: &["src/main/java/com/sable/Router.java"],
            expect_evidence: true,
        },
        Case {
            id: "S06",
            question: "healthEndpointReturnsOk test assertion",
            mode: attic_retrieval::AnswerMode::Normal,
            expected: &["src/test/java/com/sable/RouterTest.java"],
            related: &[],
            expect_evidence: true,
        },
    ]
}
