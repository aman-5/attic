//! Lexical file search for the public `search` API.

use std::collections::BTreeMap;
use std::time::Duration;

use attic_storage::{
    DbPool, FtsSearchParams, FtsSearchResult, StorageError, fts_search_with_timeout,
};
use serde::Serialize;

/// Hard bound on snippet length in characters.
pub const MAX_SNIPPET_CHARS: usize = 240;
const DEFAULT_CANDIDATE_DEPTH: usize = 100;
const SEARCH_QUERY_TIMEOUT: Duration = Duration::from_secs(3);
pub const K_RRF: f64 = 60.0;

/// Head-truncate a snippet on a character boundary.
pub fn bound_snippet(text: &str) -> String {
    if text.chars().count() <= MAX_SNIPPET_CHARS {
        return text.to_owned();
    }
    format!(
        "{}…",
        text.chars().take(MAX_SNIPPET_CHARS).collect::<String>()
    )
}

/// Options controlling one lexical search call.
#[derive(Debug, Clone)]
pub struct HybridSearchOptions {
    pub repository_id: Option<String>,
    pub file_type: Option<String>,
    pub language: Option<String>,
    pub fts_candidate_depth: usize,
    pub result_limit: usize,
}

impl HybridSearchOptions {
    pub fn with_result_limit(result_limit: usize) -> Self {
        Self {
            repository_id: None,
            file_type: None,
            language: None,
            fts_candidate_depth: result_limit.max(DEFAULT_CANDIDATE_DEPTH),
            result_limit,
        }
    }
}

/// Search match source. Search results are lexical-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchType {
    Lexical,
}

/// One lexical search result.
#[derive(Debug, Clone, Serialize)]
pub struct HybridSearchResult {
    pub retrieval_unit_id: String,
    pub repository_id: String,
    pub path: String,
    pub file_type: Option<String>,
    pub language: Option<String>,
    pub snippet: Option<String>,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub match_type: MatchType,
    pub rrf_score: f64,
    pub lexical_score: Option<f64>,
}

/// Results from one lexical search.
#[derive(Debug, Clone, Serialize)]
pub struct HybridSearchResponse {
    pub results: Vec<HybridSearchResult>,
}

/// Bounded lexical caller over the indexed FTS projection.
pub struct HybridSearcher<'a> {
    pool: &'a DbPool,
}

impl<'a> HybridSearcher<'a> {
    pub fn new(pool: &'a DbPool) -> Self {
        Self { pool }
    }

    pub fn search(
        &self,
        query: &str,
        opts: &HybridSearchOptions,
    ) -> Result<HybridSearchResponse, StorageError> {
        let fts_query = literal_fts_query(query);
        let params = FtsSearchParams {
            query: &fts_query,
            repository_id: opts.repository_id.as_deref(),
            file_type: opts.file_type.as_deref(),
            language: opts.language.as_deref(),
            max_results: opts.fts_candidate_depth,
        };
        let hits = self
            .pool
            .with_reader(|conn| fts_search_with_timeout(conn, &params, SEARCH_QUERY_TIMEOUT))?;
        Ok(HybridSearchResponse {
            results: lexical_results(hits, opts.result_limit),
        })
    }
}

fn literal_fts_query(query: &str) -> String {
    let terms = query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() {
        "\"\"".to_owned()
    } else {
        terms.join(" ")
    }
}

fn lexical_results(fts: Vec<FtsSearchResult>, result_limit: usize) -> Vec<HybridSearchResult> {
    let mut by_unit = BTreeMap::new();
    for (index, hit) in fts.into_iter().enumerate() {
        let rank = index as f64 + 1.0;
        by_unit
            .entry(hit.retrieval_unit_id.clone())
            .or_insert_with(|| HybridSearchResult {
                retrieval_unit_id: hit.retrieval_unit_id,
                repository_id: hit.repository_id,
                path: hit.path,
                file_type: Some(hit.file_type),
                language: hit.language,
                snippet: Some(bound_snippet(&hit.body)),
                start_line: hit.start_line,
                end_line: hit.end_line,
                match_type: MatchType::Lexical,
                rrf_score: 1.0 / (K_RRF + rank),
                lexical_score: Some(hit.score),
            });
    }
    let mut results: Vec<_> = by_unit.into_values().collect();
    results.sort_by(|a, b| {
        b.rrf_score
            .total_cmp(&a.rrf_score)
            .then_with(|| a.retrieval_unit_id.cmp(&b.retrieval_unit_id))
    });
    results.truncate(result_limit);
    results
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

    #[test]
    fn bound_snippet_truncates_long_bodies_on_char_boundary() {
        let long = "界".repeat(MAX_SNIPPET_CHARS * 2);
        let bounded = bound_snippet(&long);
        assert_eq!(bounded.chars().count(), MAX_SNIPPET_CHARS + 1);
        assert!(bounded.ends_with('…'));
        assert_eq!(bound_snippet("short"), "short");
    }

    #[test]
    fn lexical_hits_keep_provenance_fields_and_are_bounded() {
        let mut hit = fts_hit("a", 10.0);
        hit.body = "x".repeat(MAX_SNIPPET_CHARS * 3);
        hit.start_line = Some(7);
        hit.end_line = Some(9);
        let results = lexical_results(vec![hit], 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].match_type, MatchType::Lexical);
        assert_eq!(results[0].start_line, Some(7));
        assert_eq!(results[0].end_line, Some(9));
        assert_eq!(
            results[0].snippet.as_deref().unwrap().chars().count(),
            MAX_SNIPPET_CHARS + 1
        );
        assert_eq!(results[0].lexical_score, Some(10.0));
    }

    #[test]
    fn result_limit_widens_only_lexical_candidate_depth() {
        let opts = HybridSearchOptions::with_result_limit(250);
        assert!(opts.fts_candidate_depth >= 250);
        assert_eq!(opts.result_limit, 250);
        assert_eq!(
            HybridSearchOptions::with_result_limit(5).fts_candidate_depth,
            DEFAULT_CANDIDATE_DEPTH
        );
    }

    #[test]
    fn search_query_treats_fts_operators_and_punctuation_as_text() {
        assert_eq!(
            literal_fts_query("attic.toml OR value"),
            "\"attic.toml\" \"OR\" \"value\""
        );
        assert_eq!(
            literal_fts_query("name\"with\"quotes"),
            "\"name\"\"with\"\"quotes\""
        );
    }
}
