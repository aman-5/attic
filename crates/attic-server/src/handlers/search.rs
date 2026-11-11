use crate::*;

/// Default result count when the caller does not pass `max_results`.
/// Sized for agent consumption: enough to rank, small enough to stay dense.
const DEFAULT_SEARCH_RESULTS: usize = 25;
const MAX_ALTERNATE_LOCATIONS: usize = 64;
const MAX_DIAGNOSTIC_TERMS: usize = 6;

/// Raw lexical search through `HybridSearcher`, ranked by FTS relevance.
///
/// Every result carries `source_type` (`knowledge` / `documentation` /
/// `code` / `config` / `test`). `scope: "knowledge"` returns only knowledge:
/// central knowledge folder hits first (when configured), then repository
/// `knowledge/` hits. Default (`scope` absent or `"all"`) results are
/// unchanged and never include the central folder.
pub(crate) fn handle_search(
    pool: &DbPool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
    knowledge_repository_id: Option<&str>,
) -> Result<CallToolResult, ServerError> {
    let started = std::time::Instant::now();
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

    // Default is deliberately small: every result carries a bounded snippet,
    // so 200-row responses were multi-tens-of-KB blobs. Callers needing the
    // full page pass `max_results` explicitly (hard cap MAX_SEARCH_RESULTS).
    let max_results = match args.get("max_results") {
        None => DEFAULT_SEARCH_RESULTS,
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                ServerError::InvalidArg("max_results must be a positive integer".into())
            })? as usize;
            if n == 0 || n > MAX_SEARCH_RESULTS {
                return Err(ServerError::InvalidArg(format!(
                    "max_results must be 1..={MAX_SEARCH_RESULTS}, got {n}"
                )));
            }
            n
        }
    };

    // [FIX] Candidate depth must be wider than `result_limit`, not equal to
    // it — RRF fusion quality depends on fusing over a wider pool than what
    // gets returned (see `HybridSearchOptions`'s own doc comment). `2x` is a
    // provisional multiplier, same caveat as the underlying candidate-depth
    // constants — not benchmark-derived. The 100 floor matches
    // `DEFAULT_CANDIDATE_DEPTH` so small limits still fetch wide.
    let candidate_depth = (max_results * 2).max(100);
    let mut opts = attic_retrieval::HybridSearchOptions::with_result_limit(max_results);
    opts.fts_candidate_depth = candidate_depth;
    opts.repository_id = repo_id.map(str::to_owned);
    opts.file_type = file_type.map(str::to_owned);
    opts.language = language.map(str::to_owned);
    opts.path_scope = parse_path_scope(args)?;
    opts.allowed_repositories = Some(active_ids.iter().cloned().collect());
    opts.knowledge_paths_only = knowledge_only
        && !(knowledge_repository_id.is_some() && knowledge_repository_id == repo_id);
    opts.exclude_root_readme =
        knowledge_only && knowledge_repository_id.is_some() && knowledge_repository_id == repo_id;
    opts.result_limit = candidate_depth.min(MAX_SEARCH_RESULTS);
    opts.match_mode = match args.get("match_mode") {
        None | Some(Value::Null) => attic_retrieval::SearchMatchMode::Any,
        Some(Value::String(mode)) if mode == "any" => attic_retrieval::SearchMatchMode::Any,
        Some(Value::String(mode)) if mode == "all" => attic_retrieval::SearchMatchMode::All,
        Some(_) => {
            return Err(ServerError::InvalidArg(
                "match_mode must be \"any\" or \"all\"".into(),
            ));
        }
    };
    let searcher = attic_retrieval::HybridSearcher::new(pool);
    let mut response = searcher.search(query, &opts).map_err(search_error)?;
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
            kopts.allowed_repositories = Some(vec![kid.to_owned()]);
            kopts.knowledge_paths_only = false;
            kopts.exclude_root_readme = true;
            central = searcher
                .search(query, &kopts)
                .map_err(search_error)?
                .results;
            results.extend(central.iter().filter(|r| {
                r.repository_id == kid
                    && !attic_retrieval::candidates::is_central_knowledge_readme(&r.path)
            }));
        }
        results.extend(response.results.iter().filter(|r| label(r) == "knowledge"));
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
    let results = collapse_search_results(results, max_results);
    let diagnostics = if results.is_empty() {
        Some(empty_search_diagnostics(
            &searcher,
            query,
            &opts,
            knowledge_repository_id.filter(|_| knowledge_only && repo_id.is_none()),
        )?)
    } else {
        None
    };
    crate::handlers::context::context_response_bounded(
        json!({
            "results": results,
            "search_diagnostics": diagnostics,
            "duplicate_coverage": "bounded_candidate_window",
            "telemetry": {"server_processing_ms": started.elapsed().as_secs_f64() * 1000.0},
        }),
        crate::handlers::context::response_limit(args),
    )
}

fn occurrence(result: &Value) -> Value {
    let keys = [
        "retrieval_unit_id",
        "repository_id",
        "path",
        "json_pointer",
        "start_line",
        "end_line",
        "position_basis",
        "location_status",
        "freshness_state",
        "source_type",
    ];
    let mut output = serde_json::Map::new();
    for key in keys {
        if let Some(value) = result.get(key) {
            output.insert(key.into(), value.clone());
        }
    }
    Value::Object(output)
}

/// Group exact equivalent candidates without discarding occurrence provenance.
pub(crate) fn collapse_search_results(results: Vec<Value>, limit: usize) -> Vec<Value> {
    let mut grouped: Vec<Value> = Vec::new();
    let mut positions = HashMap::<(String, String), usize>::new();
    for result in results {
        let key = result
            .get("canonical_hash")
            .and_then(Value::as_str)
            .zip(result.get("duplicate_identity").and_then(Value::as_str))
            .map(|(hash, identity)| (hash.to_owned(), identity.to_owned()));
        if let Some(index) = key.as_ref().and_then(|key| positions.get(key)).copied() {
            let mut locations = vec![occurrence(&result)];
            if let Some(alternates) = result.get("also_in").and_then(Value::as_array) {
                locations.extend(alternates.iter().cloned());
            }
            let omitted = result["omitted_duplicate_locations"].as_u64().unwrap_or(0);
            let target = &mut grouped[index];
            let Some(object) = target.as_object_mut() else {
                continue;
            };
            let mut dropped = omitted;
            let alternates = object
                .entry("also_in")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(alternates) = alternates.as_array_mut() {
                for location in locations {
                    if alternates.len() < MAX_ALTERNATE_LOCATIONS {
                        alternates.push(location);
                    } else {
                        dropped += 1;
                    }
                }
            }
            if dropped > 0 {
                let previous = object
                    .get("omitted_duplicate_locations")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                object.insert(
                    "omitted_duplicate_locations".into(),
                    json!(previous + dropped),
                );
            }
        } else {
            if let Some(key) = key {
                positions.insert(key, grouped.len());
            }
            grouped.push(result);
        }
    }
    grouped.truncate(limit);
    grouped
}

fn empty_search_diagnostics(
    searcher: &attic_retrieval::HybridSearcher<'_>,
    query: &str,
    opts: &attic_retrieval::HybridSearchOptions,
    central_knowledge: Option<&str>,
) -> Result<Value, ServerError> {
    let terms = attic_retrieval::hybrid::search_diagnostic_terms(query).map_err(search_error)?;
    let started = std::time::Instant::now();
    let mut checked = Vec::new();
    let mut errors = Vec::new();
    let mut probe = opts.clone();
    probe.result_limit = 1;
    probe.fts_candidate_depth = 1;
    probe.timeout = Duration::from_millis(100);
    for term in terms.iter().take(MAX_DIAGNOSTIC_TERMS) {
        if started.elapsed() >= Duration::from_millis(300) {
            break;
        }
        let quoted = format!("\"{}\"", term.replace('"', "\"\""));
        let mut matched = match searcher.search(&quoted, &probe) {
            Ok(response) => !response.results.is_empty(),
            Err(error) => {
                errors.push(json!({"term": term, "error": error.to_string()}));
                continue;
            }
        };
        if !matched && let Some(id) = central_knowledge {
            let mut central = probe.clone();
            central.repository_id = Some(id.to_owned());
            central.allowed_repositories = Some(vec![id.to_owned()]);
            central.knowledge_paths_only = false;
            central.exclude_root_readme = true;
            match searcher.search(&quoted, &central) {
                Ok(response) => matched = !response.results.is_empty(),
                Err(error) => {
                    errors.push(json!({"term": term, "error": error.to_string()}));
                    continue;
                }
            }
        }
        checked.push(json!({"term": term, "has_indexed_match": matched}));
    }
    Ok(json!({
        "terms": checked,
        "complete": checked.len() == terms.len() && errors.is_empty(),
        "terms_total": terms.len(),
        "errors": errors,
        "note": "Diagnostics cover the selected indexed scope only; empty results do not prove source absence.",
    }))
}

fn search_error(error: attic_retrieval::RetrievalError) -> ServerError {
    match error {
        attic_retrieval::RetrievalError::InvalidQuery(message) => ServerError::InvalidArg(message),
        attic_retrieval::RetrievalError::Storage(error) => ServerError::Storage(error),
        other => ServerError::Retrieval(other.to_string()),
    }
}
