use crate::*;

/// `search` is a thin caller over `HybridSearcher`, which fuses lexical
/// (FTS) and semantic (kNN) candidates via RRF. When `semantic` is `None`
/// (semantic disabled), every result is lexical-only, ranked exactly as
/// `fts_search` ranks it.
///
/// Every result carries `source_type` (`knowledge` / `documentation` /
/// `code` / `config` / `test`). `scope: "knowledge"` returns only knowledge:
/// central knowledge folder hits first (when configured), then repository
/// `knowledge/` hits. Default (`scope` absent or `"all"`) results are
/// unchanged and never include the central folder.
pub(crate) fn handle_search(
    pool: &DbPool,
    semantic: Option<&attic_retrieval::semantic::SemanticStack>,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
    knowledge_repository_id: Option<&str>,
) -> Result<CallToolResult, ServerError> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("query required".into()))?;
    validate_filter("query", query, 512)?;
    let knowledge_only = match args.get("scope").and_then(Value::as_str) {
        None | Some("all") => false,
        Some("knowledge") => true,
        Some(other) => {
            return Err(ServerError::InvalidArg(format!(
                "scope must be \"all\" or \"knowledge\", got {other}"
            )));
        }
    };

    let repo_id = args.get("repository_id").and_then(Value::as_str);
    if let Some(id) = repo_id {
        validate_repository_id(id)?;
        require_active_member(active_ids, id)?;
    }
    let file_type = args.get("file_type").and_then(Value::as_str);
    if let Some(ft) = file_type {
        validate_filter("file_type", ft, 32)?;
    }
    let language = args.get("language").and_then(Value::as_str);
    if let Some(lg) = language {
        validate_filter("language", lg, 64)?;
    }

    // [FIX] Candidate depth must be wider than `result_limit`, not equal to
    // it — RRF fusion quality depends on fusing over a wider pool than what
    // gets returned (see `HybridSearchOptions`'s own doc comment). The
    // previous code set all three fields to `MAX_SEARCH_RESULTS`, silently
    // defeating that invariant on the only production call site. `2x` is a
    // provisional multiplier, same caveat as the underlying candidate-depth
    // constants — not benchmark-derived.
    let mut opts = attic_retrieval::HybridSearchOptions::with_result_limit(MAX_SEARCH_RESULTS);
    opts.fts_candidate_depth = MAX_SEARCH_RESULTS * 2;
    opts.semantic_candidate_depth = MAX_SEARCH_RESULTS * 2;
    opts.repository_id = repo_id.map(str::to_owned);
    opts.file_type = file_type.map(str::to_owned);
    opts.language = language.map(str::to_owned);
    let searcher = attic_retrieval::HybridSearcher::new(pool, semantic);
    let mut response = searcher.search(query, &opts)?;
    // Membership-authoritative retrieval scope (§16/§26): a workspace-wide
    // search (no explicit repository_id) must never surface hits from
    // repositories that have left the configured workspace but still exist
    // in storage.
    response
        .results
        .retain(|r| active_ids.contains(&r.repository_id));

    let label = |r: &attic_retrieval::HybridSearchResult| {
        if knowledge_repository_id == Some(r.repository_id.as_str()) {
            "knowledge"
        } else {
            attic_retrieval::candidates::search_label_for_path(&r.path)
        }
    };
    let mut results: Vec<&attic_retrieval::HybridSearchResult> = Vec::new();
    let central;
    if knowledge_only {
        if let (Some(kid), None) = (knowledge_repository_id, repo_id) {
            let mut kopts = opts.clone();
            kopts.repository_id = Some(kid.to_owned());
            central = searcher.search(query, &kopts)?.results;
            results.extend(central.iter().filter(|r| {
                r.repository_id == kid
                    && !attic_retrieval::candidates::is_central_knowledge_readme(&r.path)
            }));
        }
        results.extend(response.results.iter().filter(|r| label(r) == "knowledge"));
        results.truncate(MAX_SEARCH_RESULTS);
    } else {
        results.extend(response.results.iter());
    }
    let results: Vec<Value> = results
        .into_iter()
        .map(|r| {
            let mut v = serde_json::to_value(r)?;
            if let Value::Object(m) = &mut v {
                m.insert("source_type".into(), Value::from(label(r)));
            }
            Ok(v)
        })
        .collect::<Result<_, serde_json::Error>>()?;
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&json!({
            "results": results,
            "semantic_degraded": response.semantic_degraded,
            "semantic_degraded_reason_text": response
                .semantic_degraded
                .map(|reason| reason.description()),
        }))?,
    )]))
}
