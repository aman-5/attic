//! Lexical file search for the public `search` API.

use std::collections::BTreeMap;
use std::time::Duration;

use attic_storage::{
    DbPool, FtsSearchParams, FtsSearchResult, PathScope, fts_search_with_timeout_scoped_allowed,
};
use serde::Serialize;

use crate::error::RetrievalError;

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
    pub path_scope: Option<PathScope>,
    pub allowed_repositories: Option<Vec<String>>,
    pub knowledge_paths_only: bool,
    /// Exclude central knowledge's root README before candidate limiting.
    pub exclude_root_readme: bool,
    pub timeout: Duration,
    pub match_mode: SearchMatchMode,
    pub fts_candidate_depth: usize,
    pub result_limit: usize,
}

/// How adjacent search terms are combined when no operator is supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMatchMode {
    /// Return matches for any term, ranked by FTS relevance.
    Any,
    /// Require every adjacent term, preserving the previous strict behavior.
    All,
}

impl HybridSearchOptions {
    pub fn with_result_limit(result_limit: usize) -> Self {
        Self {
            repository_id: None,
            file_type: None,
            language: None,
            path_scope: None,
            allowed_repositories: None,
            knowledge_paths_only: false,
            exclude_root_readme: false,
            timeout: SEARCH_QUERY_TIMEOUT,
            match_mode: SearchMatchMode::Any,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_start_byte: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_end_byte: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_basis: Option<String>,
    pub location_status: String,
    pub freshness_state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_identity: Option<String>,
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
    ) -> Result<HybridSearchResponse, RetrievalError> {
        let fts_query = build_fts_query(query, opts.match_mode)?;
        let params = FtsSearchParams {
            query: &fts_query,
            repository_id: opts.repository_id.as_deref(),
            file_type: opts.file_type.as_deref(),
            language: opts.language.as_deref(),
            max_results: opts.fts_candidate_depth,
        };
        let hits = self.pool.with_reader(|conn| {
            fts_search_with_timeout_scoped_allowed(
                conn,
                &params,
                opts.timeout,
                opts.path_scope.as_ref(),
                opts.allowed_repositories.as_deref(),
                opts.knowledge_paths_only,
                opts.exclude_root_readme,
            )
        })?;
        Ok(HybridSearchResponse {
            results: lexical_results(hits, opts.result_limit),
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum QueryToken {
    Term(String),
    And,
    Or,
    Not,
    Open,
    Close,
}

fn invalid_query(message: impl Into<String>) -> RetrievalError {
    RetrievalError::InvalidQuery(message.into())
}

fn tokenize_query(query: &str) -> Result<Vec<QueryToken>, RetrievalError> {
    if query.chars().count() > crate::query::MAX_QUERY_CHARS {
        return Err(invalid_query("search query exceeds 512 characters"));
    }
    if query.chars().any(char::is_control) {
        return Err(invalid_query("search query contains control characters"));
    }
    let chars: Vec<char> = query.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        let token = match chars[i] {
            '(' => {
                i += 1;
                QueryToken::Open
            }
            ')' => {
                i += 1;
                QueryToken::Close
            }
            '"' => {
                i += 1;
                let mut phrase = String::new();
                let mut closed = false;
                while i < chars.len() {
                    if matches!(chars[i], '\\' | '"') && chars.get(i + 1) == Some(&'"') {
                        phrase.push('"');
                        i += 2;
                    } else if chars[i] == '"' {
                        i += 1;
                        closed = true;
                        break;
                    } else {
                        phrase.push(chars[i]);
                        i += 1;
                    }
                }
                if !closed || phrase.trim().is_empty() {
                    return Err(invalid_query("quoted search phrase is empty or unclosed"));
                }
                QueryToken::Term(phrase)
            }
            _ => {
                let mut term = String::new();
                let mut embedded_parens = 0usize;
                while i < chars.len() && !chars[i].is_whitespace() {
                    match chars[i] {
                        '(' => embedded_parens += 1,
                        ')' if embedded_parens > 0 => embedded_parens -= 1,
                        ')' => break,
                        _ => {}
                    }
                    term.push(chars[i]);
                    i += 1;
                }
                match term.as_str() {
                    "AND" => QueryToken::And,
                    "OR" => QueryToken::Or,
                    "NOT" => QueryToken::Not,
                    _ => QueryToken::Term(term),
                }
            }
        };
        tokens.push(token);
        if tokens.len() > 64 {
            return Err(invalid_query("search query exceeds 64 terms and operators"));
        }
    }
    if tokens.is_empty() {
        return Err(invalid_query("search query is empty"));
    }
    Ok(tokens)
}

/// Distinct literal terms available for bounded empty-result diagnostics.
pub fn search_diagnostic_terms(query: &str) -> Result<Vec<String>, RetrievalError> {
    let mut terms = Vec::new();
    for token in tokenize_query(query)? {
        if let QueryToken::Term(term) = token
            && !terms.contains(&term)
        {
            terms.push(term);
        }
    }
    Ok(terms)
}

fn parse_expression(
    tokens: &[QueryToken],
    position: &mut usize,
    minimum_precedence: u8,
    mode: SearchMatchMode,
    depth: usize,
) -> Result<String, RetrievalError> {
    if depth > 16 {
        return Err(invalid_query("search query nesting exceeds 16 levels"));
    }
    let mut left = match tokens.get(*position) {
        Some(QueryToken::Term(term)) => {
            *position += 1;
            format!("\"{}\"", term.replace('"', "\"\""))
        }
        Some(QueryToken::Open) => {
            *position += 1;
            let nested = parse_expression(tokens, position, 1, mode, depth + 1)?;
            if tokens.get(*position) != Some(&QueryToken::Close) {
                return Err(invalid_query("unclosed search group"));
            }
            *position += 1;
            nested
        }
        _ => return Err(invalid_query("expected a search term or group")),
    };

    while let Some(token) = tokens.get(*position) {
        let (operator, precedence, explicit) = match token {
            QueryToken::Or => ("OR", 1, true),
            QueryToken::And => ("AND", 2, true),
            QueryToken::Not => ("NOT", 3, true),
            QueryToken::Term(_) | QueryToken::Open => match mode {
                SearchMatchMode::Any => ("OR", 1, false),
                SearchMatchMode::All => ("AND", 2, false),
            },
            QueryToken::Close => break,
        };
        if precedence < minimum_precedence {
            break;
        }
        if explicit {
            *position += 1;
        }
        let right = parse_expression(tokens, position, precedence + 1, mode, depth)?;
        left = format!("({left} {operator} {right})");
    }
    Ok(left)
}

/// Build a bounded FTS5 expression while quoting every literal term.
pub fn build_fts_query(query: &str, mode: SearchMatchMode) -> Result<String, RetrievalError> {
    let tokens = tokenize_query(query)?;
    let mut position = 0;
    let expression = parse_expression(&tokens, &mut position, 1, mode, 0)?;
    if position != tokens.len() {
        return Err(invalid_query("unexpected closing search group"));
    }
    Ok(expression)
}

fn lexical_results(fts: Vec<FtsSearchResult>, result_limit: usize) -> Vec<HybridSearchResult> {
    let mut by_unit = BTreeMap::new();
    for (index, hit) in fts.into_iter().enumerate() {
        let rank = index as f64 + 1.0;
        let metadata = hit
            .occurrence_metadata
            .as_deref()
            .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok());
        let json_pointer = metadata
            .as_ref()
            .and_then(|value| value.get("json_pointer"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let source_start_byte = metadata
            .as_ref()
            .and_then(|value| value.get("source_start_byte"))
            .and_then(serde_json::Value::as_u64);
        let source_end_byte = metadata
            .as_ref()
            .and_then(|value| value.get("source_end_byte"))
            .and_then(serde_json::Value::as_u64);
        let position_basis = metadata
            .as_ref()
            .and_then(|value| value.get("position_basis"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        let duplicate_identity = if hit.analyzer_id == "json" {
            let structured = hit
                .canonical_text
                .as_deref()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                .is_some_and(|value| value.is_object() || value.is_array());
            if structured {
                json_pointer.as_ref().map(|pointer| {
                    let start = metadata.as_ref().and_then(|value| value.get("array_start"));
                    let end = metadata.as_ref().and_then(|value| value.get("array_end"));
                    format!("json:{pointer}:{start:?}:{end:?}")
                })
            } else {
                None
            }
        } else if hit.body.len() >= 100 {
            let basename = hit.path.rsplit('/').next().unwrap_or(&hit.path);
            Some(format!("text:{}:{basename}", hit.file_type))
        } else {
            None
        };
        let valid_location = hit.analyzer_id != "json"
            || (source_start_byte.is_some()
                && source_end_byte.is_some()
                && position_basis.is_some());
        let start_line = valid_location
            .then(|| hit.start_line.and_then(|line| line.checked_add(1)))
            .flatten();
        let end_line = valid_location
            .then(|| hit.end_line.map(|line| line.max(start_line.unwrap_or(0))))
            .flatten();
        by_unit
            .entry(hit.retrieval_unit_id.clone())
            .or_insert_with(|| HybridSearchResult {
                retrieval_unit_id: hit.retrieval_unit_id,
                repository_id: hit.repository_id,
                path: hit.path,
                file_type: Some(hit.file_type),
                language: hit.language,
                snippet: Some(bound_snippet(&hit.body)),
                start_line,
                end_line,
                json_pointer,
                source_start_byte,
                source_end_byte,
                position_basis,
                location_status: if valid_location {
                    "indexed_unverified".into()
                } else {
                    "unavailable_reindex_required".into()
                },
                freshness_state: hit.freshness_state,
                canonical_hash: hit.canonical_hash,
                duplicate_identity,
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
            analyzer_id: "generic".into(),
            occurrence_metadata: None,
            canonical_hash: None,
            canonical_text: None,
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
        assert_eq!(results[0].start_line, Some(8));
        assert_eq!(results[0].end_line, Some(9));
        assert_eq!(
            results[0].snippet.as_deref().unwrap().chars().count(),
            MAX_SNIPPET_CHARS + 1
        );
        assert_eq!(results[0].lexical_score, Some(10.0));
    }

    #[test]
    fn legacy_json_locations_are_not_presented_as_live_lines() {
        let mut hit = fts_hit("legacy", 1.0);
        hit.analyzer_id = "json".into();
        hit.start_line = Some(0);
        hit.end_line = Some(1);
        hit.occurrence_metadata = Some(r#"{"json_pointer":"/key"}"#.into());
        let results = lexical_results(vec![hit], 1);
        assert_eq!(results[0].json_pointer.as_deref(), Some("/key"));
        assert_eq!(results[0].start_line, None);
        assert_eq!(results[0].end_line, None);
        assert_eq!(results[0].location_status, "unavailable_reindex_required");
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
    fn search_query_supports_operators_phrases_and_match_modes() {
        assert_eq!(
            build_fts_query("attic.toml OR value", SearchMatchMode::Any).unwrap(),
            "(\"attic.toml\" OR \"value\")"
        );
        assert_eq!(
            build_fts_query("name\"with\"quotes", SearchMatchMode::Any).unwrap(),
            "\"name\"\"with\"\"quotes\""
        );
        assert_eq!(
            build_fts_query("\"exact phrase\" foo", SearchMatchMode::All).unwrap(),
            "(\"exact phrase\" AND \"foo\")"
        );
        for query in ["\"say \\\"hello\\\"\"", "\"say \"\"hello\"\"\""] {
            assert_eq!(
                build_fts_query(query, SearchMatchMode::Any).unwrap(),
                "\"say \"\"hello\"\"\""
            );
        }
        assert_eq!(
            build_fts_query("foo AND (bar OR baz) NOT qux", SearchMatchMode::Any).unwrap(),
            "(\"foo\" AND ((\"bar\" OR \"baz\") NOT \"qux\"))"
        );
        assert_eq!(
            build_fts_query("checkHdbFlow() value", SearchMatchMode::Any).unwrap(),
            "(\"checkHdbFlow()\" OR \"value\")"
        );
        for invalid in ["", "foo OR", "NOT foo", "(foo", "foo)", "\"unterminated"] {
            assert!(
                build_fts_query(invalid, SearchMatchMode::Any).is_err(),
                "{invalid}"
            );
        }
    }
}
