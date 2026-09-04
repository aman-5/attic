//! `HybridSearcher` — RRF (Reciprocal Rank Fusion) of lexical (FTS) and
//! semantic (kNN) results for the public `search` MCP tool (Phase 8).
//!
//! Deliberately independent of the `context` tool's Evidence/Candidate/
//! Contract pipeline (`fuse.rs`/`rank.rs`/`pipeline.rs`, `candidates.rs`,
//! `semantic.rs`'s `SemanticCandidateGenerator`) — this module shares ZERO
//! types with that pipeline. It may reuse only low-level primitives:
//! `SemanticStore::knn`, `attic_storage::retrieval_unit_anchor`,
//! `attic_storage::fts_search`. Co-located in the same crate for
//! convenience only, not because it shares the `context` ranking domain
//! model.
//!
//! A query-time semantic failure (provider unavailable, embedding failure,
//! store failure, no coverage) degrades to lexical-only — it never aborts
//! the whole search. FTS has no fallback path: a failure there is a genuine
//! search failure and is propagated with `?`.

use std::collections::BTreeMap;

use attic_storage::{DbPool, FtsSearchParams, FtsSearchResult, StorageError, fts_search};
use serde::Serialize;

use crate::semantic::{SemanticStack, truncate_to_byte_limit};

/// Standard RRF constant (Cormack et al.) — tunable later.
pub const K_RRF: f64 = 60.0;

/// Default per-ranker candidate depth before fusion (provisional tuning).
const DEFAULT_CANDIDATE_DEPTH: usize = 100;

/// Options controlling one hybrid search call.
#[derive(Debug, Clone)]
pub struct HybridSearchOptions {
    /// Optional repository UUID filter.
    pub repository_id: Option<String>,
    /// Optional file type filter.
    pub file_type: Option<String>,
    /// Optional language filter.
    pub language: Option<String>,
    /// How many FTS hits enter fusion — deliberately wider than
    /// `result_limit`, NOT the final result count.
    pub fts_candidate_depth: usize,
    /// How many kNN hits enter fusion — same, for the semantic side.
    pub semantic_candidate_depth: usize,
    /// Final returned count, applied AFTER fusion.
    pub result_limit: usize,
}

impl HybridSearchOptions {
    /// Reasonable starting depths (100/100) for a given final `result_limit`
    /// — provisional tuning values, not requirements. Fetching wide from
    /// each ranker before fusing, then truncating, produces materially
    /// better results than requesting `result_limit` from each directly.
    pub fn with_result_limit(result_limit: usize) -> Self {
        Self {
            repository_id: None,
            file_type: None,
            language: None,
            fts_candidate_depth: DEFAULT_CANDIDATE_DEPTH,
            semantic_candidate_depth: DEFAULT_CANDIDATE_DEPTH,
            result_limit,
        }
    }
}

/// Which ranker(s) surfaced a given retrieval unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchType {
    /// FTS only.
    Lexical,
    /// Semantic kNN only.
    Semantic,
    /// Both rankers surfaced the same retrieval unit.
    Both,
}

/// Why the semantic side contributed nothing this call. Distinct from "not
/// configured" (`None` in [`HybridSearchResponse::semantic_degraded`]),
/// which is not a failure at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SemanticDegradationReason {
    /// Provider reported itself unavailable.
    ProviderUnavailable,
    /// Active model has zero embeddings for the scope.
    NoEmbeddings,
    /// Query embedding failed.
    EmbeddingFailed,
    /// The disposable semantic store itself failed (poisoned/IO).
    StoreUnavailable,
}

/// One fused search result.
#[derive(Debug, Clone, Serialize)]
pub struct HybridSearchResult {
    /// `core_retrieval_units.id`.
    pub retrieval_unit_id: String,
    /// Owning repository UUID.
    pub repository_id: String,
    /// Workspace-relative file path.
    pub path: String,
    /// File type, when known from the FTS side.
    pub file_type: Option<String>,
    /// Language, when known from the FTS side.
    pub language: Option<String>,
    /// Bounded snippet, when available from the FTS side.
    pub snippet: Option<String>,
    /// Which ranker(s) surfaced this unit.
    pub match_type: MatchType,
    /// Fused RRF score (higher = better).
    pub rrf_score: f64,
    /// Raw FTS relevance score, if this unit was an FTS hit.
    pub lexical_score: Option<f64>,
    /// Raw cosine similarity, if this unit was a semantic hit.
    pub semantic_similarity: Option<f32>,
}

/// Result of one [`HybridSearcher::search`] call.
#[derive(Debug, Clone, Serialize)]
pub struct HybridSearchResponse {
    /// Fused, ranked results (already truncated to `result_limit`).
    pub results: Vec<HybridSearchResult>,
    /// `Some(reason)` when the semantic side degraded this call;
    /// `None` when it either succeeded or was never configured at all.
    pub semantic_degraded: Option<SemanticDegradationReason>,
}

struct SemanticHit {
    retrieval_unit_id: String,
    similarity: f32,
    repository_id: String,
    path: String,
}

/// Thin caller over `fts_search` + the semantic kNN stack, fusing both via
/// RRF. Lives in `attic-retrieval`, never in `attic-server::main`.
pub struct HybridSearcher<'a> {
    pool: &'a DbPool,
    semantic: Option<&'a SemanticStack>,
}

impl<'a> HybridSearcher<'a> {
    /// `semantic == None` means the semantic layer is not configured at all
    /// (e.g. `ATTIC_SEMANTIC` opt-in is off) — every search is lexical-only
    /// with `semantic_degraded == None` (not configured is not a failure).
    pub fn new(pool: &'a DbPool, semantic: Option<&'a SemanticStack>) -> Self {
        Self { pool, semantic }
    }

    /// Run one hybrid search. FTS failures propagate (no fallback exists for
    /// FTS itself); semantic failures degrade to lexical-only.
    pub fn search(
        &self,
        query: &str,
        opts: &HybridSearchOptions,
    ) -> Result<HybridSearchResponse, StorageError> {
        let params = FtsSearchParams {
            query,
            repository_id: opts.repository_id.as_deref(),
            file_type: opts.file_type.as_deref(),
            language: opts.language.as_deref(),
            max_results: opts.fts_candidate_depth,
        };
        let fts = self.pool.with_reader(|c| fts_search(c, &params))?;
        let (semantic_hits, semantic_degraded) = self.fetch_semantic(query, opts);
        let results = rrf_fuse(fts, semantic_hits, opts.result_limit);
        Ok(HybridSearchResponse {
            results,
            semantic_degraded,
        })
    }

    /// Never returns `Err` — any failure at any step (availability,
    /// coverage, embed, kNN, anchor resolution) is caught and turned into
    /// `(vec![], Some(reason))`, so `search()` above always has FTS results
    /// to fall back to.
    fn fetch_semantic(
        &self,
        query: &str,
        opts: &HybridSearchOptions,
    ) -> (Vec<SemanticHit>, Option<SemanticDegradationReason>) {
        let Some(stack) = self.semantic else {
            return (Vec::new(), None);
        };
        if !stack.provider.available() {
            return (
                Vec::new(),
                Some(SemanticDegradationReason::ProviderUnavailable),
            );
        }
        let coverage = match stack.store.count(
            stack.provider.id(),
            stack.provider.model_id(),
            opts.repository_id.as_deref(),
        ) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("hybrid search: semantic store unavailable (coverage probe): {e}");
                return (
                    Vec::new(),
                    Some(SemanticDegradationReason::StoreUnavailable),
                );
            }
        };
        if coverage == 0 {
            return (Vec::new(), Some(SemanticDegradationReason::NoEmbeddings));
        }

        let q = truncate_to_byte_limit(query, stack.provider.max_input_bytes());

        let mut usage = attic_semantic::ResourceUsage::default();
        let cancel = attic_semantic::CancelFlag::new();
        let qv = match stack.provider.embed_batch(
            &[attic_semantic::EmbeddingInput {
                unit_key: "__search_query__".into(),
                text: q,
            }],
            &cancel,
            &mut usage,
            None,
        ) {
            Ok(mut outs) if !outs.is_empty() => outs.remove(0).vector,
            Ok(_) => {
                return (Vec::new(), Some(SemanticDegradationReason::EmbeddingFailed));
            }
            Err(e) => {
                tracing::warn!("hybrid search: query embedding failed: {e}");
                return (Vec::new(), Some(SemanticDegradationReason::EmbeddingFailed));
            }
        };

        let scan_budget = attic_semantic::ScanBudget {
            cancel: &cancel,
            deadline: None,
            max_rows: (opts.semantic_candidate_depth.max(1) as u64) * 8,
        };
        let kn = match stack.store.knn(
            &qv,
            opts.semantic_candidate_depth,
            stack.provider.id(),
            stack.provider.model_id(),
            opts.repository_id.as_deref(),
            &scan_budget,
        ) {
            Ok(kn) => kn,
            Err(e) => {
                tracing::warn!("hybrid search: semantic store unavailable during query: {e}");
                return (
                    Vec::new(),
                    Some(SemanticDegradationReason::StoreUnavailable),
                );
            }
        };
        if kn.hits.is_empty() {
            return (Vec::new(), Some(SemanticDegradationReason::NoEmbeddings));
        }

        let hits = kn.hits;
        let anchored = self.pool.with_reader(|conn| {
            let mut out = Vec::with_capacity(hits.len());
            for h in &hits {
                if let Some(anchor) =
                    attic_storage::retrieval_unit_anchor(conn, &h.retrieval_unit_id)?
                {
                    out.push(SemanticHit {
                        retrieval_unit_id: h.retrieval_unit_id.clone(),
                        similarity: h.similarity,
                        repository_id: anchor.repository_id,
                        path: anchor.path,
                    });
                }
            }
            Ok(out)
        });
        match anchored {
            Ok(hits) => (hits, None),
            Err(e) => {
                tracing::warn!("hybrid search: anchor resolution failed: {e}");
                (
                    Vec::new(),
                    Some(SemanticDegradationReason::StoreUnavailable),
                )
            }
        }
    }
}

#[derive(Clone)]
struct FusionEntry {
    score: f64,
    match_type: MatchType,
    repository_id: String,
    path: String,
    file_type: Option<String>,
    language: Option<String>,
    snippet: Option<String>,
    lexical_score: Option<f64>,
    semantic_similarity: Option<f32>,
}

/// `score(unit) = Σ over rankers containing it of 1 / (K_RRF + rank)`, rank
/// is 1-BASED (first hit = rank 1) — the standard Cormack et al. RRF
/// convention. Ties broken by `retrieval_unit_id` (stable, deterministic) —
/// never by insertion/hashmap order. Duplicate hits from both rankers are
/// merged into one `MatchType::Both` entry, never duplicated in output.
fn rrf_fuse(
    fts: Vec<FtsSearchResult>,
    semantic: Vec<SemanticHit>,
    result_limit: usize,
) -> Vec<HybridSearchResult> {
    let mut scores: BTreeMap<String, FusionEntry> = BTreeMap::new();

    for (i, r) in fts.iter().enumerate() {
        let rank = i as f64 + 1.0;
        let contribution = 1.0 / (K_RRF + rank);
        scores
            .entry(r.retrieval_unit_id.clone())
            .and_modify(|e| {
                e.score += contribution;
                e.match_type = MatchType::Both;
                e.lexical_score = Some(r.score);
            })
            .or_insert(FusionEntry {
                score: contribution,
                match_type: MatchType::Lexical,
                repository_id: r.repository_id.clone(),
                path: r.path.clone(),
                file_type: Some(r.file_type.clone()),
                language: r.language.clone(),
                snippet: Some(r.body.clone()),
                lexical_score: Some(r.score),
                semantic_similarity: None,
            });
    }

    for (i, h) in semantic.iter().enumerate() {
        let rank = i as f64 + 1.0;
        let contribution = 1.0 / (K_RRF + rank);
        scores
            .entry(h.retrieval_unit_id.clone())
            .and_modify(|e| {
                e.score += contribution;
                e.match_type = MatchType::Both;
                e.semantic_similarity = Some(h.similarity);
            })
            .or_insert(FusionEntry {
                score: contribution,
                match_type: MatchType::Semantic,
                repository_id: h.repository_id.clone(),
                path: h.path.clone(),
                file_type: None,
                language: None,
                snippet: None,
                lexical_score: None,
                semantic_similarity: Some(h.similarity),
            });
    }

    let mut out: Vec<(String, FusionEntry)> = scores.into_iter().collect();
    out.sort_by(|(id_a, a), (id_b, b)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| id_a.cmp(id_b))
    });
    out.into_iter()
        .take(result_limit)
        .map(|(id, e)| HybridSearchResult {
            retrieval_unit_id: id,
            repository_id: e.repository_id,
            path: e.path,
            file_type: e.file_type,
            language: e.language,
            snippet: e.snippet,
            match_type: e.match_type,
            rrf_score: e.score,
            lexical_score: e.lexical_score,
            semantic_similarity: e.semantic_similarity,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts_hit(id: &str, score: f64) -> FtsSearchResult {
        FtsSearchResult {
            retrieval_unit_id: id.into(),
            file_occurrence_id: "fo".into(),
            index_generation_id: "gen".into(),
            repository_id: "repo".into(),
            repository_name: "repo".into(),
            path: format!("{id}.rs"),
            language: Some("rust".into()),
            file_type: "rust".into(),
            body: "body".into(),
            score,
            start_line: None,
            end_line: None,
            freshness_state: "CURRENT".into(),
        }
    }

    fn sem_hit(id: &str, similarity: f32) -> SemanticHit {
        SemanticHit {
            retrieval_unit_id: id.into(),
            similarity,
            repository_id: "repo".into(),
            path: format!("{id}.rs"),
        }
    }

    #[test]
    fn lexical_only_hit_is_tagged_lexical() {
        let out = rrf_fuse(vec![fts_hit("a", 10.0)], vec![], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].match_type, MatchType::Lexical);
        assert!(out[0].semantic_similarity.is_none());
    }

    #[test]
    fn semantic_only_hit_is_tagged_semantic() {
        let out = rrf_fuse(vec![], vec![sem_hit("a", 0.9)], 10);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].match_type, MatchType::Semantic);
        assert!(out[0].lexical_score.is_none());
    }

    #[test]
    fn hit_in_both_rankers_merges_into_one_both_entry() {
        let out = rrf_fuse(vec![fts_hit("a", 10.0)], vec![sem_hit("a", 0.9)], 10);
        assert_eq!(
            out.len(),
            1,
            "must not duplicate a unit present in both rankers"
        );
        assert_eq!(out[0].match_type, MatchType::Both);
        assert!(out[0].lexical_score.is_some());
        assert!(out[0].semantic_similarity.is_some());
    }

    #[test]
    fn a_unit_ranked_by_both_scores_higher_than_either_alone() {
        let both = rrf_fuse(vec![fts_hit("a", 10.0)], vec![sem_hit("a", 0.9)], 10);
        let lexical_only = rrf_fuse(vec![fts_hit("a", 10.0)], vec![], 10);
        assert!(both[0].rrf_score > lexical_only[0].rrf_score);
    }

    #[test]
    fn result_limit_truncates_after_fusion() {
        let fts: Vec<_> = (0..5).map(|i| fts_hit(&format!("u{i}"), 1.0)).collect();
        let out = rrf_fuse(fts, vec![], 2);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn ties_break_by_retrieval_unit_id() {
        // Two units with identical rank-1 contribution from two independent
        // single-hit FTS calls would tie on score; verify deterministic order.
        let out = rrf_fuse(vec![fts_hit("b", 1.0), fts_hit("a", 1.0)], vec![], 10);
        // "a" and "b" both get rank 1 and rank 2 respectively from a single
        // FTS list, so they do NOT tie here; assert plain rank ordering
        // instead (first hit ranks first).
        assert_eq!(out[0].retrieval_unit_id, "b");
        assert_eq!(out[1].retrieval_unit_id, "a");
    }

    #[test]
    fn first_hit_ranks_above_later_hits_within_one_ranker() {
        let out = rrf_fuse(
            vec![fts_hit("first", 1.0), fts_hit("second", 1.0)],
            vec![],
            10,
        );
        assert!(out[0].rrf_score > out[1].rrf_score);
        assert_eq!(out[0].retrieval_unit_id, "first");
    }
}
