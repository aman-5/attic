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

/// Hard bound on snippet length in characters. Retrieval units can be
/// megabyte-scale (e.g. minified JSON exports); returning full bodies per
/// hit made responses unusable. 240 chars is enough to identify the hit;
/// the `file` tool is the escape hatch for full content.
pub const MAX_SNIPPET_CHARS: usize = 240;

/// Head-truncate `text` to at most `MAX_SNIPPET_CHARS` characters on a
/// char boundary, appending `…` when truncated.
pub fn bound_snippet(text: &str) -> String {
    if text.chars().count() <= MAX_SNIPPET_CHARS {
        return text.to_string();
    }
    let truncated: String = text.chars().take(MAX_SNIPPET_CHARS).collect();
    format!("{truncated}…")
}

/// Default per-ranker candidate depth before fusion (provisional tuning).
const DEFAULT_CANDIDATE_DEPTH: usize = 100;
/// One search query's semantic budget. If the worker is still occupied past
/// this point, hybrid search degrades to lexical-only instead of waiting for a
/// whole background batch to finish.
const SEARCH_SEMANTIC_DEADLINE_MS: u64 = 1_500;

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
        // Widen the candidate depths to at least `result_limit` so a caller
        // requesting more results than `DEFAULT_CANDIDATE_DEPTH` doesn't get
        // silently truncated before `result_limit` is even applied (fusion
        // can only return as many results as entered it).
        let candidate_depth = result_limit.max(DEFAULT_CANDIDATE_DEPTH);
        Self {
            repository_id: None,
            file_type: None,
            language: None,
            fts_candidate_depth: candidate_depth,
            semantic_candidate_depth: candidate_depth,
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
    /// The semantic query timed out waiting for or running an embedding batch.
    QueryTimedOut,
    /// Query embedding failed.
    EmbeddingFailed,
    /// The disposable semantic store itself failed (poisoned/IO).
    StoreUnavailable,
}

impl SemanticDegradationReason {
    pub fn description(self) -> &'static str {
        match self {
            Self::ProviderUnavailable => {
                "the embedding provider is unavailable, so search fell back to lexical results"
            }
            Self::NoEmbeddings => {
                "no embeddings exist for the active model in this search scope, so search fell back to lexical results"
            }
            Self::QueryTimedOut => {
                "the semantic query timed out waiting for or running an embedding batch, so search fell back to lexical results"
            }
            Self::EmbeddingFailed => {
                "the semantic query embedding failed, so search fell back to lexical results"
            }
            Self::StoreUnavailable => {
                "the semantic store is unavailable, so search fell back to lexical results"
            }
        }
    }
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
    /// Bounded snippet (see [`bound_snippet`]). Present for every result
    /// whose unit text is available — lexical hits directly, semantic-only
    /// hits via post-fusion enrichment.
    pub snippet: Option<String>,
    /// Start line (0-based) of the retrieval unit span, when recorded.
    pub start_line: Option<u32>,
    /// End line (0-based, inclusive) of the retrieval unit span, when recorded.
    pub end_line: Option<u32>,
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
    /// Line window from the unit anchor (structural-node span), when known.
    start_line: Option<u32>,
    /// Line window end (0-based, inclusive), when known.
    end_line: Option<u32>,
    /// 1-based rank in the original kNN order, BEFORE anchor-resolution
    /// filtering drops any hits. RRF must score by this, not by position in
    /// the (possibly shorter) filtered `Vec` — otherwise a hit whose
    /// predecessor lost its anchor gets scored as if it ranked higher than
    /// it truly did.
    rank: usize,
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
        // FTS and semantic search are fully independent (neither depends on
        // the other's result) but were previously run back-to-back, paying
        // the sum of both latencies instead of the max. Run them on separate
        // threads so a slow semantic embed/kNN pass overlaps the FTS query
        // instead of queuing behind it.
        let (fts, (semantic_hits, semantic_degraded)) = std::thread::scope(|scope| {
            let semantic_handle = scope.spawn(|| self.fetch_semantic(query, opts));
            let fts = self.pool.with_reader(|c| fts_search(c, &params));
            let semantic = semantic_handle.join().unwrap_or_else(|_| {
                (
                    Vec::new(),
                    Some(SemanticDegradationReason::StoreUnavailable),
                )
            });
            (fts, semantic)
        });
        let fts = fts?;
        let mut results = rrf_fuse(fts, semantic_hits, opts.result_limit);
        self.enrich_semantic_only(&mut results);
        Ok(HybridSearchResponse {
            results,
            semantic_degraded,
        })
    }

    /// Fill `snippet`/`language`/`file_type` for semantic-only finalists in
    /// one batched read (they arrive from the kNN side with neither text nor
    /// classification). Best-effort: on failure the results are returned
    /// as-is — enrichment must never turn a healthy search into a failed one.
    fn enrich_semantic_only(&self, results: &mut [HybridSearchResult]) {
        let ids: Vec<String> = results
            .iter()
            .filter(|r| r.match_type == MatchType::Semantic && r.snippet.is_none())
            .map(|r| r.retrieval_unit_id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        let enriched = self
            .pool
            .with_reader(|conn| attic_storage::retrieval_unit_texts(conn, &ids));
        let Ok(map) = enriched else {
            tracing::warn!("hybrid search: semantic-only enrichment failed; returning as-is");
            return;
        };
        for r in results.iter_mut() {
            if r.match_type == MatchType::Semantic
                && r.snippet.is_none()
                && let Some(e) = map.get(&r.retrieval_unit_id)
            {
                r.snippet = Some(bound_snippet(&e.retrieval_text));
                if r.language.is_none() {
                    r.language = e.language.clone();
                }
                if r.file_type.is_none() {
                    r.file_type = e.file_type.clone();
                }
            }
        }
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
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(SEARCH_SEMANTIC_DEADLINE_MS);
        let qv = match stack.provider.embed_batch(
            &[attic_semantic::EmbeddingInput {
                unit_key: "__search_query__".into(),
                text: q,
            }],
            &cancel,
            &mut usage,
            Some(deadline),
        ) {
            Ok(mut outs) if !outs.is_empty() => outs.remove(0).vector,
            Ok(_) => {
                return (Vec::new(), Some(SemanticDegradationReason::EmbeddingFailed));
            }
            Err(attic_semantic::SemanticError::Cancelled { .. }) => {
                return (Vec::new(), Some(SemanticDegradationReason::QueryTimedOut));
            }
            Err(e) => {
                tracing::warn!("hybrid search: query embedding failed: {e}");
                return (Vec::new(), Some(SemanticDegradationReason::EmbeddingFailed));
            }
        };

        // ── Active Generation Check ───
        let active_gen = match stack.store.get_active_generation() {
            Ok(Some(g)) => g,
            Ok(None) => return (Vec::new(), Some(SemanticDegradationReason::NoEmbeddings)),
            Err(e) => {
                tracing::warn!(
                    "hybrid search: semantic store unavailable (get_active_generation): {e}"
                );
                return (
                    Vec::new(),
                    Some(SemanticDegradationReason::StoreUnavailable),
                );
            }
        };

        let scan_budget = attic_semantic::ScanBudget {
            cancel: &cancel,
            deadline: None,
            max_rows: 0, // Unused by HNSW
        };
        let kn = match stack.store.knn_search_generation(
            active_gen.generation_id,
            &qv,
            opts.semantic_candidate_depth,
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
            // Batched: one query per 64-hit chunk instead of 2-3 round
            // trips per individual hit (a single search can have 100-250+
            // hits, so this replaces hundreds of sequential round trips).
            let ids: Vec<String> = hits.iter().map(|h| h.retrieval_unit_id.clone()).collect();
            let anchors = attic_storage::retrieval_unit_anchors(conn, &ids)?;
            let mut out = Vec::with_capacity(hits.len());
            for (i, h) in hits.iter().enumerate() {
                if let Some(anchor) = anchors.get(&h.retrieval_unit_id) {
                    out.push(SemanticHit {
                        retrieval_unit_id: h.retrieval_unit_id.clone(),
                        similarity: h.similarity,
                        repository_id: anchor.repository_id.clone(),
                        path: anchor.path.clone(),
                        start_line: anchor.start_line,
                        end_line: anchor.end_line,
                        rank: i + 1,
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
    start_line: Option<u32>,
    end_line: Option<u32>,
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
                if e.start_line.is_none() {
                    e.start_line = r.start_line;
                }
                if e.end_line.is_none() {
                    e.end_line = r.end_line;
                }
            })
            .or_insert(FusionEntry {
                score: contribution,
                match_type: MatchType::Lexical,
                repository_id: r.repository_id.clone(),
                path: r.path.clone(),
                file_type: Some(r.file_type.clone()),
                language: r.language.clone(),
                snippet: Some(bound_snippet(&r.body)),
                start_line: r.start_line,
                end_line: r.end_line,
                lexical_score: Some(r.score),
                semantic_similarity: None,
            });
    }

    for h in semantic.iter() {
        let rank = h.rank as f64;
        let contribution = 1.0 / (K_RRF + rank);
        scores
            .entry(h.retrieval_unit_id.clone())
            .and_modify(|e| {
                e.score += contribution;
                e.match_type = MatchType::Both;
                e.semantic_similarity = Some(h.similarity);
                if e.start_line.is_none() {
                    e.start_line = h.start_line;
                }
                if e.end_line.is_none() {
                    e.end_line = h.end_line;
                }
            })
            .or_insert(FusionEntry {
                score: contribution,
                match_type: MatchType::Semantic,
                repository_id: h.repository_id.clone(),
                path: h.path.clone(),
                file_type: None,
                language: None,
                snippet: None,
                start_line: h.start_line,
                end_line: h.end_line,
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
            start_line: e.start_line,
            end_line: e.end_line,
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
        // All current call sites pass a single-element Vec, so rank 1
        // matches every one of them; add a `rank` parameter if a test ever
        // needs a multi-hit Vec with a non-trivial rank ordering.
        SemanticHit {
            retrieval_unit_id: id.into(),
            similarity,
            repository_id: "repo".into(),
            path: format!("{id}.rs"),
            start_line: None,
            end_line: None,
            rank: 1,
        }
    }

    #[test]
    fn bound_snippet_truncates_long_bodies_on_char_boundary() {
        let long: String = "a".repeat(MAX_SNIPPET_CHARS * 4);
        let bounded = bound_snippet(&long);
        assert!(bounded.chars().count() == MAX_SNIPPET_CHARS + 1);
        assert!(bounded.ends_with('…'));

        // Multi-byte chars must never be split mid-scalar.
        let wide: String = "界".repeat(MAX_SNIPPET_CHARS * 2);
        let bounded_wide = bound_snippet(&wide);
        assert_eq!(bounded_wide.chars().count(), MAX_SNIPPET_CHARS + 1);

        // Short text passes through unchanged.
        assert_eq!(bound_snippet("short"), "short");
    }

    #[test]
    fn lexical_hits_carry_bounded_snippet_and_line_span() {
        let mut hit = fts_hit("a", 10.0);
        hit.body = "x".repeat(MAX_SNIPPET_CHARS * 3);
        hit.start_line = Some(7);
        hit.end_line = Some(9);
        let out = rrf_fuse(vec![hit], vec![], 10);
        assert_eq!(out[0].start_line, Some(7));
        assert_eq!(out[0].end_line, Some(9));
        let snippet = out[0].snippet.as_deref().unwrap();
        assert_eq!(snippet.chars().count(), MAX_SNIPPET_CHARS + 1);
        assert!(snippet.ends_with('…'));
    }

    #[test]
    fn both_merge_keeps_lines_from_whichever_side_has_them() {
        let mut hit = fts_hit("a", 10.0);
        hit.start_line = Some(3);
        hit.end_line = Some(5);
        let mut sem = sem_hit("a", 0.9);
        sem.start_line = Some(11);
        sem.end_line = Some(12);
        let out = rrf_fuse(vec![hit], vec![sem], 10);
        assert_eq!(out[0].match_type, MatchType::Both);
        assert_eq!(out[0].start_line, Some(3));
        assert_eq!(out[0].end_line, Some(5));
    }

    #[test]
    fn semantic_lines_survive_when_lexical_side_lacks_them() {
        let mut sem = sem_hit("a", 0.9);
        sem.start_line = Some(11);
        sem.end_line = Some(12);
        let out = rrf_fuse(vec![fts_hit("a", 10.0)], vec![sem], 10);
        assert_eq!(out[0].start_line, Some(11));
        assert_eq!(out[0].end_line, Some(12));
    }

    #[test]
    fn with_result_limit_widens_candidate_depths_above_default() {
        // Regression test for Bug 18: requesting more results than
        // `DEFAULT_CANDIDATE_DEPTH` (100) must widen both candidate depths
        // to at least `result_limit`, instead of silently capping the fused
        // candidate pool below the requested output size.
        let opts = HybridSearchOptions::with_result_limit(250);
        assert!(opts.fts_candidate_depth >= 250);
        assert!(opts.semantic_candidate_depth >= 250);
        assert_eq!(opts.result_limit, 250);
    }

    #[test]
    fn with_result_limit_keeps_default_depth_for_small_limits() {
        // A small result_limit must not shrink the candidate depth below the
        // default (100) — fetching wide before fusing is still desirable.
        let opts = HybridSearchOptions::with_result_limit(5);
        assert_eq!(opts.fts_candidate_depth, DEFAULT_CANDIDATE_DEPTH);
        assert_eq!(opts.semantic_candidate_depth, DEFAULT_CANDIDATE_DEPTH);
        assert_eq!(opts.result_limit, 5);
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
}
