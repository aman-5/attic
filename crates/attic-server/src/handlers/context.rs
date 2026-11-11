use crate::*;

// ─── Phase 4 evidence-driven context tool ─────────────────────────────────────

/// Thin MCP wrapper around the Phase 4 retrieval pipeline. Exposes the
/// assembled context, verified claims and result/confidence verdicts; raw
/// RetrievalPlan internals stay in `ops_retrieval_log`, not in the tool
/// surface.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_context(
    pool: &DbPool,
    writer: &WriterQueueHandle,
    crossrepo_degraded: bool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
    resource_advisory: attic_storage::resource_manager::ResourceAdvisory,
    knowledge_repository_id: Option<String>,
) -> Result<CallToolResult, ServerError> {
    include_diagnostics(args)?;
    if matches!(args.get("mode").and_then(Value::as_str), Some("SEARCH")) {
        return crate::handlers::search::handle_search(
            pool,
            args,
            active_ids,
            knowledge_repository_id.as_deref(),
        );
    }
    if matches!(args.get("mode").and_then(Value::as_str), Some("REFERENCES")) {
        return crate::handlers::references::handle_references(pool, args, active_ids);
    }
    if args.get("repository_id").and_then(Value::as_str).is_none() && active_ids.len() > 1 {
        let started = std::time::Instant::now();
        let mut ids = active_ids.iter().cloned().collect::<Vec<_>>();
        ids.sort();
        let knowledge_owner = knowledge_repository_id.as_ref().and_then(|knowledge| {
            if active_ids.contains(knowledge) {
                Some(knowledge.clone())
            } else {
                ids.first().cloned()
            }
        });
        let mut outputs = Vec::new();
        let mut documents = Vec::new();
        for batch in ids.chunks(4) {
            let outcomes = std::thread::scope(|scope| {
                let workers = batch
                    .iter()
                    .map(|id| {
                        let mut local_args = args.clone();
                        local_args.insert("repository_id".into(), json!(id));
                        let knowledge = if knowledge_owner.as_deref() == Some(id.as_str()) {
                            knowledge_repository_id.clone()
                        } else {
                            None
                        };
                        scope.spawn(move || {
                            prepare_context(
                                pool,
                                writer,
                                crossrepo_degraded,
                                &local_args,
                                active_ids,
                                resource_advisory,
                                knowledge,
                            )
                            .map(|result| (id.clone(), result))
                        })
                    })
                    .collect::<Vec<_>>();
                workers
                    .into_iter()
                    .map(|worker| {
                        worker.join().map_err(|_| {
                            ServerError::Retrieval(
                                "workspace repository query worker panicked".into(),
                            )
                        })?
                    })
                    .collect::<Result<Vec<_>, ServerError>>()
            })?;
            for (id, (answer, document)) in outcomes {
                outputs.push(json!({"repository_id": id, "answer": answer}));
                if let Some(document) = document {
                    documents.push(document);
                }
            }
        }
        let mut payload = aggregate_context_answers(
            &outputs,
            &documents,
            &[],
            ids.len(),
            include_diagnostics(args)?,
        )?;
        payload["crossrepo_ready"] = json!(!crossrepo_degraded);
        payload["telemetry"] = json!({
            "server_processing_ms": started.elapsed().as_secs_f64() * 1000.0,
            "context_bytes_before_sharing": documents.iter().map(|doc| doc.text.len()).sum::<usize>(),
            "context_bytes": payload["context"].as_str().map_or(0, str::len),
        });
        apply_document_budget(&mut payload, &documents, response_limit(args))?;
        return context_response_bounded(payload, response_limit(args));
    }
    let (mut payload, document) = prepare_context(
        pool,
        writer,
        crossrepo_degraded,
        args,
        active_ids,
        resource_advisory,
        knowledge_repository_id,
    )?;
    if let Some(document) = document {
        apply_document_budget(&mut payload, &[document], response_limit(args))?;
    }
    context_response_bounded(payload, response_limit(args))
}

pub(crate) type PreparedContext = (Value, Option<attic_retrieval::context::ContextDocument>);

#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_context(
    pool: &DbPool,
    writer: &WriterQueueHandle,
    crossrepo_degraded: bool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
    resource_advisory: attic_storage::resource_manager::ResourceAdvisory,
    knowledge_repository_id: Option<String>,
) -> Result<PreparedContext, ServerError> {
    let started = std::time::Instant::now();
    let diagnostics = include_diagnostics(args)?;
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or_else(|| ServerError::InvalidArg("query required".into()))?;
    validate_filter("query", query, 512)?;

    // Phase 7 graceful degradation: DEEP expansions are paused under
    // Pause/Emergency resource advisories; the query still runs at NORMAL
    // depth so foreground work is never starved by its own expensive mode.
    let mut mode = match args.get("mode").and_then(Value::as_str) {
        None | Some("NORMAL") => attic_retrieval::AnswerMode::Normal,
        Some("FAST") => attic_retrieval::AnswerMode::Fast,
        Some("DEEP") => attic_retrieval::AnswerMode::Deep,
        Some(other) => {
            return Err(ServerError::InvalidArg(format!(
                "mode must be FAST|NORMAL|DEEP|SEARCH|REFERENCES, got {other}"
            )));
        }
    };
    if mode == attic_retrieval::AnswerMode::Deep
        && matches!(
            resource_advisory,
            attic_storage::resource_manager::ResourceAdvisory::Restricted
        )
    {
        mode = attic_retrieval::AnswerMode::Normal;
    }

    let mut request = attic_retrieval::AnswerRequest::new(query, mode);
    request.path_scope = parse_path_scope(args)?;
    if let Some(id) = args.get("repository_id").and_then(Value::as_str) {
        validate_repository_id(id)?;
        require_active_member(active_ids, id)?;
        request.repository_ids.push(id.to_owned());
    } else {
        // Workspace-wide context operates over current membership only
        // (§25/§26): historical/inactive repositories never feed retrieval.
        request.repository_ids = active_ids.iter().cloned().collect();
        request.repository_ids.sort();
    }
    request.knowledge_repository_id = knowledge_repository_id;
    let mut workspace_ids = active_ids.iter().cloned().collect::<Vec<_>>();
    workspace_ids.sort();
    request.workspace_repository_ids = Some(workspace_ids);
    let path_resolution = resolve_path_hints(
        pool,
        query,
        &request.repository_ids,
        request.path_scope.as_ref(),
    )?;

    let service = attic_retrieval::RetrievalService {
        readers: pool.clone(),
        writer: writer.clone(),
        crossrepo_degraded,
    };
    let outcome = service
        .answer(&request)
        .map_err(|e| ServerError::Retrieval(e.to_string()))?;
    let mut drop_reasons = std::collections::BTreeMap::<String, usize>::new();
    for dropped in &outcome.plan.evidence_dropped {
        *drop_reasons
            .entry(dropped.drop_reason.as_str().to_owned())
            .or_default() += 1;
    }
    let payload = json!({
        "result": outcome.result.as_str(),
        "confidence": outcome.confidence.as_str(),
        "insufficient_reason": outcome.insufficient_reason,
        "plan_id": outcome.plan.plan_id,
        "evidence_used": outcome.plan.evidence_used.len(),
        "evidence_dropped": if diagnostics { json!(outcome.plan.evidence_dropped) } else { json!([]) },
        "evidence_dropped_summary": {
            "count": outcome.plan.evidence_dropped.len(),
            "by_reason": drop_reasons,
            "details_included": diagnostics,
        },
        "claims": outcome.claims.iter().map(|(text, verdict, evidence_ids)| json!({
            "text": text,
            "verdict": verdict,
            "evidence_ids": evidence_ids,
        })).collect::<Vec<_>>(),
        // REAL provenance for every served evidence item: callers (and the
        // Phase 6 gate) must be able to trace a cross-repo conclusion to its
        // exact SourceRevision and WorkspaceSnapshot instead of trusting a
        // verdict token. `workspace_snapshot_id` is present only when the
        // evidence is genuinely cross-repository and backed by a snapshot.
        "evidence": outcome.served_evidence.iter().map(|e| json!({
            "evidence_id": e.id,
            "source_type": e.source_type.as_str(),
            "repository_id": e.repository_id,
            "path": e.path,
            "source_revision_id": e.source_revision_id,
            "workspace_snapshot_id": e.workspace_snapshot_id,
            "freshness_state": e.freshness_state.as_str(),
            "confidence": e.confidence,
            "source_span": e.excerpt_span.or(e.source_span).map(|span| span.to_string()),
            "retrieval_unit_id": e.indexed_unit_id,
            "json_pointer": e.json_pointer,
            "location_status": e.location_status,
            "position_basis": e.position_basis,
        })).collect::<Vec<_>>(),
        "context": outcome.context_document.as_ref()
            .map(|doc| attic_retrieval::context::compact_documents(std::slice::from_ref(doc)))
            .or(outcome.context_text).unwrap_or_default(),
        "path_resolution": path_resolution,
        "telemetry": {
            "retrieval_ms": started.elapsed().as_secs_f64() * 1000.0,
            "phases": outcome.plan.steps.iter().map(|step| json!({
                "operation": step.operation,
                "status": step.status.as_str(),
                "elapsed_ms": (step.ended_at_us - step.started_at_us).max(0) as f64 / 1000.0,
            })).collect::<Vec<_>>(),
        },
    });
    Ok((payload, outcome.context_document))
}

fn resolve_path_hints(
    pool: &DbPool,
    query: &str,
    repository_ids: &[String],
    scope: Option<&attic_storage::PathScope>,
) -> Result<Value, ServerError> {
    let classification = attic_retrieval::query::classify(query)
        .map_err(|error| ServerError::InvalidArg(error.to_string()))?;
    let (hints, explicit) = match scope {
        Some(attic_storage::PathScope::Exact(path)) => (vec![path.clone()], true),
        _ => (classification.extracted.path_hints, false),
    };
    let started = std::time::Instant::now();
    let mut resolved = Vec::new();
    for hint in hints.iter().take(8) {
        let normalized = hint.replace('\\', "/");
        let path = normalized.trim_start_matches("./");
        if started.elapsed() >= Duration::from_millis(150) {
            resolved.push(json!({"hint": hint, "status": "incomplete", "matches": [], "error": "path resolution time budget"}));
            continue;
        }
        let deadline = started + Duration::from_millis(150);
        let candidates = pool.with_reader(|conn| {
            conn.progress_handler(1_000, Some(move || std::time::Instant::now() >= deadline))?;
            let result = attic_storage::indexed_path_candidates(
                conn,
                path,
                !explicit,
                repository_ids,
                scope,
                21,
            );
            conn.progress_handler(0, None::<fn() -> bool>)?;
            result
        });
        match candidates {
            Ok(mut matches) => {
                let truncated = matches.len() > 20;
                matches.truncate(20);
                let status = if truncated || matches.len() > 1 {
                    "ambiguous"
                } else if matches.is_empty() {
                    "missing"
                } else {
                    "unique"
                };
                resolved.push(json!({
                    "hint": hint,
                    "status": status,
                    "explicit": explicit,
                    "matches": matches.iter().map(|candidate| json!({
                        "repository_id": candidate.repository_id,
                        "path": candidate.path,
                    })).collect::<Vec<_>>(),
                    "truncated": truncated,
                }));
            }
            Err(error) => resolved.push(json!({
                "hint": hint, "status": "incomplete", "explicit": explicit,
                "matches": [], "error": error.to_string(),
            })),
        }
    }
    Ok(json!({
        "hints": resolved,
        "omitted_hints": hints.len().saturating_sub(8),
        "scope": if explicit { "explicit" } else { "inferred" },
    }))
}

pub(crate) fn merge_path_resolutions(outputs: &[Value], incomplete: bool) -> Value {
    let mut merged = Vec::<Value>::new();
    let mut omitted_hints = 0u64;
    for output in outputs {
        let resolution = &output["answer"]["path_resolution"];
        omitted_hints = omitted_hints.max(resolution["omitted_hints"].as_u64().unwrap_or(0));
        for item in resolution["hints"].as_array().into_iter().flatten() {
            let hint = item["hint"].as_str().unwrap_or("");
            let index = merged.iter().position(|entry| entry["hint"] == hint);
            let entry = if let Some(index) = index {
                &mut merged[index]
            } else {
                merged.push(
                    json!({"hint": hint, "explicit": item["explicit"], "matches": [],
                    "truncated": false, "incomplete": incomplete, "errors": []}),
                );
                merged.last_mut().expect("entry just inserted")
            };
            if item["status"] == "incomplete" {
                entry["incomplete"] = json!(true);
                if let Some(error) = item["error"].as_str() {
                    entry["errors"]
                        .as_array_mut()
                        .expect("errors array")
                        .push(json!(error));
                }
            }
            if item["truncated"] == true {
                entry["truncated"] = json!(true);
            }
            for candidate in item["matches"].as_array().into_iter().flatten() {
                let matches = entry["matches"].as_array_mut().expect("matches array");
                if !matches.contains(candidate) {
                    if matches.len() < 20 {
                        matches.push(candidate.clone());
                    } else {
                        entry["truncated"] = json!(true);
                    }
                }
            }
        }
    }
    for entry in &mut merged {
        let count = entry["matches"].as_array().map_or(0, Vec::len);
        entry["status"] = json!(if entry["incomplete"] == true {
            "incomplete"
        } else if entry["truncated"] == true || count > 1 {
            "ambiguous"
        } else if count == 1 {
            "unique"
        } else {
            "missing"
        });
    }
    json!({"hints": merged, "omitted_hints": omitted_hints})
}

pub(crate) fn aggregate_context_answers(
    outputs: &[Value],
    documents: &[attic_retrieval::context::ContextDocument],
    missing: &[Value],
    requested: usize,
    diagnostics: bool,
) -> Result<Value, ServerError> {
    let claims = outputs
        .iter()
        .filter_map(|output| output["answer"]["claims"].as_array())
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let evidence = outputs
        .iter()
        .filter_map(|output| output["answer"]["evidence"].as_array())
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let evidence_dropped = outputs
        .iter()
        .filter_map(|output| output["answer"]["evidence_dropped"].as_array())
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let mut drop_reasons = std::collections::BTreeMap::<String, u64>::new();
    let mut dropped_count = 0u64;
    for output in outputs {
        let summary = &output["answer"]["evidence_dropped_summary"];
        dropped_count += summary["count"].as_u64().unwrap_or(0);
        if let Some(reasons) = summary["by_reason"].as_object() {
            for (reason, count) in reasons {
                *drop_reasons.entry(reason.clone()).or_default() += count.as_u64().unwrap_or(0);
            }
        }
    }
    let successful = outputs.iter().any(|output| {
        matches!(
            output["answer"]["result"].as_str(),
            Some("SUCCESS" | "PARTIAL_SUCCESS")
        )
    });
    let partial = !missing.is_empty()
        || outputs
            .iter()
            .any(|output| output["answer"]["result"] != "SUCCESS");
    let confidence = outputs
        .iter()
        .filter(|output| {
            matches!(
                output["answer"]["result"].as_str(),
                Some("SUCCESS" | "PARTIAL_SUCCESS")
            )
        })
        .filter_map(|output| output["answer"]["confidence"].as_str())
        .min_by_key(|value| match *value {
            "HIGH" => 3,
            "MEDIUM" => 2,
            "LOW" => 1,
            _ => 0,
        })
        .unwrap_or("NONE");
    let plan_ids = outputs
        .iter()
        .filter_map(|output| output["answer"]["plan_id"].as_str())
        .collect::<Vec<_>>();
    let repository_answers = outputs
        .iter()
        .map(repository_summary)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({
        "result": if successful { if partial { "PARTIAL_SUCCESS" } else { "SUCCESS" } } else { "INSUFFICIENT_EVIDENCE" },
        "context": attic_retrieval::context::compact_documents(documents),
        "claims": claims,
        "evidence_used": evidence.len(),
        "evidence": evidence,
        "confidence": confidence,
        "plan_id": plan_ids.first(),
        "plan_ids": plan_ids,
        "evidence_dropped": evidence_dropped,
        "evidence_dropped_summary": {
            "count": dropped_count,
            "by_reason": drop_reasons,
            "details_included": diagnostics,
        },
        "repository_answers": repository_answers,
        "path_resolution": merge_path_resolutions(outputs, !missing.is_empty()),
        "coverage": {"requested": requested, "unavailable": missing},
    }))
}

pub(crate) fn include_diagnostics(args: &HashMap<String, Value>) -> Result<bool, ServerError> {
    match args.get("include_diagnostics") {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ServerError::InvalidArg(
            "include_diagnostics must be a boolean".into(),
        )),
    }
}

pub(crate) fn context_response(mut payload: Value) -> Result<CallToolResult, ServerError> {
    let started = std::time::Instant::now();
    let encoded = serde_json::to_string(&payload)?;
    payload["telemetry"]["serialization_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
    payload["telemetry"]["response_bytes"] = json!(encoded.len());
    // Include the size field itself in the reported UTF-8 byte count.
    let text = loop {
        let text = serde_json::to_string(&payload)?;
        if payload["telemetry"]["response_bytes"].as_u64() == Some(text.len() as u64) {
            break text;
        }
        payload["telemetry"]["response_bytes"] = json!(text.len());
    };
    tracing::info!(response_bytes = text.len(), "context response encoded");
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

pub(crate) fn response_limit(args: &HashMap<String, Value>) -> usize {
    match args.get("mode").and_then(Value::as_str) {
        Some("FAST") => 64 * 1024,
        Some("DEEP") => 256 * 1024,
        _ => 128 * 1024,
    }
}

pub(crate) fn apply_document_budget(
    payload: &mut Value,
    documents: &[attic_retrieval::context::ContextDocument],
    max_bytes: usize,
) -> Result<(), ServerError> {
    let context_limit = max_bytes / 3;
    let mut selected = Vec::new();
    let mut used = 0usize;
    let mut total_sections = 0usize;
    for document in documents {
        if document.sections.len() != document.refs.len() {
            return Err(ServerError::Retrieval(
                "context sections and evidence references are inconsistent".into(),
            ));
        }
        total_sections += document.sections.len();
        let overhead = document.prefix.len() + document.suffix.len() + 2;
        if used.saturating_add(overhead) > context_limit {
            selected.push(None);
            continue;
        }
        used += overhead;
        let mut copy = document.clone();
        copy.sections.clear();
        copy.refs.clear();
        copy.text.clear();
        selected.push(Some(copy));
    }
    let max_sections = documents
        .iter()
        .map(|document| document.sections.len())
        .max()
        .unwrap_or(0);
    for section_index in 0..max_sections {
        for (index, document) in documents.iter().enumerate() {
            let Some(copy) = selected[index].as_mut() else {
                continue;
            };
            let Some(section) = document.sections.get(section_index) else {
                continue;
            };
            let cost = section.header.len() + section.body.len() + 1;
            if used.saturating_add(cost) > context_limit {
                continue;
            }
            used += cost;
            copy.sections.push(section.clone());
            copy.refs.push(document.refs[section_index].clone());
        }
    }
    let documents = selected.into_iter().flatten().collect::<Vec<_>>();
    let retained = documents
        .iter()
        .flat_map(|doc| doc.refs.iter())
        .map(|reference| reference.evidence_id.clone())
        .collect::<HashSet<_>>();
    let omitted_evidence = total_sections.saturating_sub(retained.len());
    payload["context"] = json!(attic_retrieval::context::compact_documents(&documents));
    if payload["telemetry"]["context_bytes"].is_number() {
        payload["telemetry"]["context_bytes"] =
            json!(payload["context"].as_str().map_or(0, str::len));
    }
    let mut omitted_claims = 0usize;
    if let Some(evidence) = payload["evidence"].as_array_mut() {
        evidence.retain(|item| {
            item["evidence_id"]
                .as_str()
                .is_some_and(|id| retained.contains(id))
        });
        payload["evidence_used"] = json!(evidence.len());
    }
    if let Some(claims) = payload["claims"].as_array_mut() {
        let original = claims.len();
        claims.retain(|claim| {
            claim["evidence_ids"].as_array().is_some_and(|ids| {
                !ids.is_empty()
                    && ids
                        .iter()
                        .all(|id| id.as_str().is_some_and(|id| retained.contains(id)))
            })
        });
        omitted_claims = original - claims.len();
    }
    payload["response_budget"] = json!({
        "max_bytes": max_bytes,
        "truncated": omitted_evidence > 0 || omitted_claims > 0,
        "omitted_evidence": omitted_evidence,
        "omitted_claims": omitted_claims,
    });
    if omitted_evidence > 0 || omitted_claims > 0 {
        if payload["result"] == "SUCCESS" {
            payload["result"] = json!("PARTIAL_SUCCESS");
        }
        payload["response_budget"]["continuation"] =
            json!("Narrow the repository or path scope to retrieve omitted evidence.");
    }
    Ok(())
}

pub(crate) fn context_response_bounded(
    mut payload: Value,
    max_bytes: usize,
) -> Result<CallToolResult, ServerError> {
    if payload.get("response_budget").is_none() {
        payload["response_budget"] = json!({
            "max_bytes": max_bytes, "truncated": false,
            "omitted_evidence": 0, "omitted_claims": 0,
        });
    }
    let remaining = max_bytes.saturating_sub(256);
    for field in [
        "evidence_dropped",
        "repository_answers",
        "results",
        "matches",
        "plan_ids",
    ] {
        loop {
            if serde_json::to_vec(&payload)?.len() <= remaining {
                break;
            }
            let Some(array) = payload.get_mut(field).and_then(Value::as_array_mut) else {
                break;
            };
            if array.is_empty() {
                break;
            }
            let removed = (array.len() / 4).max(1);
            let new_len = array.len() - removed;
            let first_removed = if field == "matches" {
                array.get(new_len).cloned()
            } else {
                None
            };
            array.truncate(new_len);
            let key = format!("omitted_{field}");
            let previous = payload["response_budget"][&key].as_u64().unwrap_or(0);
            payload["response_budget"][&key] = json!(previous + removed as u64);
            payload["response_budget"]["truncated"] = json!(true);
            payload["response_budget"]["continuation"] =
                json!("Narrow the repository or path scope to retrieve omitted results.");
            if payload["result"] == "SUCCESS" {
                payload["result"] = json!("PARTIAL_SUCCESS");
            }
            if field == "matches" {
                payload["coverage"]["complete"] = json!(false);
                payload["coverage"]["absence_proven"] = json!(false);
                if let Some(hit) = first_removed {
                    let mut continuation = json!({
                        "repository_id": hit["repository_id"],
                        "path": hit["path"],
                    });
                    if let Some(offset) = hit["reference_offset"].as_u64() {
                        continuation["reference_offset"] = json!(offset);
                    } else if let Some(line) = hit["line"].as_u64() {
                        continuation["reference_start_line"] = json!(line);
                    }
                    payload["response_budget"]["continuation"] = continuation;
                    if let Some(repositories) = payload["coverage"]["repositories"].as_array_mut() {
                        for repository in repositories {
                            if repository["repository_id"] == hit["repository_id"] {
                                repository["coverage"]["complete"] = json!(false);
                                repository["coverage"]["absence_proven"] = json!(false);
                                if let Some(reasons) =
                                    repository["coverage"]["incomplete_reasons"].as_array_mut()
                                {
                                    reasons.push(json!({"reason":"response_limit"}));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if serde_json::to_vec(&payload)?.len() > remaining
        && payload["context"].as_str().is_some_and(|s| !s.is_empty())
    {
        let omitted_evidence = payload["evidence"].as_array().map_or(0, Vec::len);
        let omitted_claims = payload["claims"].as_array().map_or(0, Vec::len);
        payload["context"] = json!("");
        payload["evidence"] = json!([]);
        payload["claims"] = json!([]);
        payload["evidence_used"] = json!(0);
        payload["response_budget"]["omitted_evidence"] = json!(
            payload["response_budget"]["omitted_evidence"]
                .as_u64()
                .unwrap_or(0)
                + omitted_evidence as u64
        );
        payload["response_budget"]["omitted_claims"] = json!(
            payload["response_budget"]["omitted_claims"]
                .as_u64()
                .unwrap_or(0)
                + omitted_claims as u64
        );
        payload["response_budget"]["truncated"] = json!(true);
        payload["response_budget"]["continuation"] =
            json!("Narrow the repository or path scope to retrieve omitted evidence.");
        payload["result"] = json!("INSUFFICIENT_EVIDENCE");
        payload["confidence"] = json!("NONE");
    }
    if serde_json::to_vec(&payload)?.len() > remaining {
        return Err(ServerError::Retrieval(format!(
            "response metadata exceeds the {max_bytes}-byte limit; narrow repository or path scope"
        )));
    }
    let response = context_response(payload)?;
    let response_bytes = response.content[0]
        .as_text()
        .map_or(0, |text| text.text.len());
    if response_bytes > max_bytes {
        return Err(ServerError::Retrieval(format!(
            "serialized response exceeds the {max_bytes}-byte limit"
        )));
    }
    Ok(response)
}

pub(crate) fn repository_summary(output: &Value) -> Result<Value, ServerError> {
    let mut summary = output["answer"]
        .as_object()
        .cloned()
        .ok_or_else(|| ServerError::Retrieval("repository answer is not an object".into()))?;
    let context_bytes = summary
        .get("context")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    for key in [
        "context",
        "claims",
        "evidence",
        "evidence_dropped",
        "path_resolution",
    ] {
        summary.remove(key);
    }
    summary.insert("context_bytes".into(), json!(context_bytes));
    Ok(json!({
        "repository_id": output["repository_id"],
        "scope": output["scope"],
        "summary": summary,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_remove_only_duplicated_fields() {
        let output = json!({
            "repository_id": "repo-one",
            "answer": {
                "context": "unique context",
                "claims": [{"text": "unique claim", "evidence_ids": ["ev-one"]}],
                "evidence": [{"evidence_id": "ev-one"}],
                "evidence_dropped": [{"evidence_id": "ev-two"}],
                "evidence_dropped_summary": {"count": 1},
                "result": "PARTIAL_SUCCESS",
                "confidence": "LOW",
                "plan_id": "plan-one",
                "insufficient_reason": "relationship not verified",
                "telemetry": {"retrieval_ms": 1.0},
            },
        });
        let summary = repository_summary(&output).unwrap();
        assert_eq!(summary["repository_id"], "repo-one");
        assert_eq!(summary["summary"]["context_bytes"], 14);
        for key in ["context", "claims", "evidence", "evidence_dropped"] {
            assert!(summary["summary"].get(key).is_none());
            assert!(output["answer"].get(key).is_some());
        }
        for key in [
            "result",
            "confidence",
            "plan_id",
            "insufficient_reason",
            "evidence_dropped_summary",
            "telemetry",
        ] {
            assert_eq!(summary["summary"][key], output["answer"][key]);
        }
    }

    #[test]
    fn response_byte_count_is_exact_for_unicode_and_preserves_evidence() {
        let evidence = json!([{"evidence_id": "ev-one", "path": "\u{4e16}\u{754c}.rs"}]);
        let payload = json!({"context": "fn hello() {}", "evidence": evidence});
        let response = context_response(payload).unwrap();
        let text = response.content[0].as_text().unwrap();
        let decoded: Value = serde_json::from_str(&text.text).unwrap();
        assert_eq!(decoded["telemetry"]["response_bytes"], text.text.len());
        assert_eq!(decoded["evidence"], evidence);
        assert_eq!(decoded["context"], "fn hello() {}");
        assert!(decoded["telemetry"]["serialization_ms"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn diagnostics_are_explicit_and_type_checked() {
        assert!(!include_diagnostics(&HashMap::new()).unwrap());
        assert!(
            include_diagnostics(&HashMap::from([(
                "include_diagnostics".into(),
                json!(true)
            ),]))
            .unwrap()
        );
        assert!(
            include_diagnostics(&HashMap::from([(
                "include_diagnostics".into(),
                json!("true")
            ),]))
            .is_err()
        );
    }

    #[test]
    fn response_preserves_unique_content_above_the_rejected_size_cap() {
        let context = "fn unique_content() {}\n".repeat(12_000);
        let evidence = json!([{"evidence_id": "ev-one", "repository_id": "repo-one"}]);
        let response = context_response(json!({
            "context": context,
            "evidence": evidence,
            "claims": [{"text": "unique_content exists", "evidence_ids": ["ev-one"]}],
        }))
        .unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        let decoded: Value = serde_json::from_str(text).unwrap();
        assert!(text.len() > 64 * 1024);
        assert_eq!(decoded["context"], context);
        assert_eq!(decoded["evidence"], evidence);
        assert_eq!(decoded["claims"][0]["evidence_ids"][0], "ev-one");
        assert_eq!(decoded["telemetry"]["response_bytes"], text.len());
    }

    #[test]
    fn response_budget_keeps_whole_evidence_and_supported_claims() {
        use attic_retrieval::context::{ContextDocument, ContextSection};
        use attic_retrieval::plan::EvidenceRef;
        let mut documents = Vec::new();
        let mut evidence = Vec::new();
        let mut claims = Vec::new();
        for repository in 0..2 {
            let mut sections = Vec::new();
            let mut refs = Vec::new();
            for index in 0..5 {
                let id = format!("ev-{repository}-{index}");
                sections.push(ContextSection {
                    header: format!("\n## [{id}]\n- evidence_id: {id}\n"),
                    body: format!("```\n{}\n```\n", "multiline\n".repeat(900)),
                });
                refs.push(EvidenceRef {
                    evidence_id: id.clone(),
                    source_type: "SOURCE_CODE".into(),
                    rank: index,
                    score: 1.0,
                    token_count: 100,
                });
                evidence.push(json!({"evidence_id": id, "repository_id": repository}));
                claims.push(json!({"text": "supported", "evidence_ids": [id]}));
            }
            documents.push(ContextDocument {
                text: String::new(),
                tokens: 0,
                refs,
                dropped: Vec::new(),
                sections,
                prefix: format!("repo {repository}\n"),
                suffix: String::new(),
            });
        }
        let mut payload =
            json!({"result":"SUCCESS", "context":"", "evidence":evidence, "claims":claims});
        apply_document_budget(&mut payload, &documents, 64 * 1024).unwrap();
        let response = context_response_bounded(payload, 64 * 1024).unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        let value: Value = serde_json::from_str(text).unwrap();
        assert!(text.len() <= 64 * 1024);
        assert_eq!(value["response_budget"]["truncated"], true);
        assert!(
            value["response_budget"]["omitted_evidence"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(value["result"], "PARTIAL_SUCCESS");
        let evidence = value["evidence"].as_array().unwrap();
        assert!(evidence.iter().any(|entry| entry["repository_id"] == 0));
        assert!(evidence.iter().any(|entry| entry["repository_id"] == 1));
        for claim in value["claims"].as_array().unwrap() {
            for id in claim["evidence_ids"].as_array().unwrap() {
                assert!(evidence.iter().any(|entry| entry["evidence_id"] == *id));
            }
        }
        for entry in evidence {
            assert!(
                value["context"]
                    .as_str()
                    .unwrap()
                    .contains(entry["evidence_id"].as_str().unwrap())
            );
        }
    }

    #[test]
    fn response_budget_trims_complete_search_results_and_reports_omissions() {
        let results = (0..100)
            .map(|index| json!({"path": format!("file-{index}"), "snippet": "x".repeat(2000)}))
            .collect::<Vec<_>>();
        let response = context_response_bounded(json!({"results": results}), 16 * 1024).unwrap();
        let text = &response.content[0].as_text().unwrap().text;
        let value: Value = serde_json::from_str(text).unwrap();
        let kept = value["results"].as_array().unwrap().len();
        let omitted = value["response_budget"]["omitted_results"]
            .as_u64()
            .unwrap() as usize;
        assert!(text.len() <= 16 * 1024);
        assert!(kept > 0 && omitted > 0);
        assert_eq!(kept + omitted, 100);
        assert_eq!(value["results"][0]["snippet"].as_str().unwrap().len(), 2000);
    }
}
