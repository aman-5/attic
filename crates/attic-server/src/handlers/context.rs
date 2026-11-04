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
    let (payload, _) = prepare_context(
        pool,
        writer,
        crossrepo_degraded,
        args,
        active_ids,
        resource_advisory,
        knowledge_repository_id,
    )?;
    context_response(payload)
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
                "mode must be FAST|NORMAL|DEEP|SEARCH, got {other}"
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
    if let Some(id) = args.get("repository_id").and_then(Value::as_str) {
        validate_repository_id(id)?;
        require_active_member(active_ids, id)?;
        request.repository_ids.push(id.to_owned());
    } else {
        // Workspace-wide context operates over current membership only
        // (§25/§26): historical/inactive repositories never feed retrieval.
        request.repository_ids = active_ids.iter().cloned().collect();
    }
    request.knowledge_repository_id = knowledge_repository_id;

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
        })).collect::<Vec<_>>(),
        "context": outcome.context_document.as_ref()
            .map(|doc| attic_retrieval::context::compact_documents(std::slice::from_ref(doc)))
            .or(outcome.context_text).unwrap_or_default(),
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

pub(crate) fn repository_summary(output: &Value) -> Result<Value, ServerError> {
    let mut summary = output["answer"]
        .as_object()
        .cloned()
        .ok_or_else(|| ServerError::Retrieval("repository answer is not an object".into()))?;
    let context_bytes = summary
        .get("context")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    for key in ["context", "claims", "evidence", "evidence_dropped"] {
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
}
