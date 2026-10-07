use crate::*;

// ─── Phase 4 evidence-driven context tool ─────────────────────────────────────

/// Thin MCP wrapper around the Phase 4 retrieval pipeline. Exposes the
/// assembled context, verified claims and result/confidence verdicts; raw
/// RetrievalPlan internals stay in `ops_retrieval_log`, not in the tool
/// surface.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_context(
    semantic: Option<Arc<attic_retrieval::semantic::SemanticStack>>,
    pool: &DbPool,
    writer: &WriterQueueHandle,
    crossrepo_degraded: bool,
    args: &HashMap<String, Value>,
    active_ids: &HashSet<String>,
    resource_advisory: attic_storage::resource_manager::ResourceAdvisory,
    knowledge_repository_id: Option<String>,
) -> Result<CallToolResult, ServerError> {
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
                "mode must be FAST|NORMAL|DEEP, got {other}"
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
        semantic,
        crossrepo_degraded,
    };
    let outcome = service
        .answer(&request)
        .map_err(|e| ServerError::Retrieval(e.to_string()))?;
    let semantic_fallback_reason = (!outcome
        .plan
        .policy_trace
        .semantic_fallback_reason
        .is_empty())
    .then_some(outcome.plan.policy_trace.semantic_fallback_reason.clone());

    let payload = json!({
        "result": outcome.result.as_str(),
        "confidence": outcome.confidence.as_str(),
        "insufficient_reason": outcome.insufficient_reason,
        "plan_id": outcome.plan.plan_id,
        "evidence_used": outcome.plan.evidence_used.len(),
        "semantic_fallback_reason": semantic_fallback_reason,
        "semantic_fallback_reason_text": semantic_fallback_reason
            .as_deref()
            .and_then(attic_retrieval::semantic::semantic_fallback_reason_text),
        // RP-INV-4: every piece of considered evidence must be accounted
        // for — this is the "excluded" half (evidence_used above is the
        // "included" half), each with a deterministic drop_reason so a
        // caller can tell why a candidate never reached the answer.
        "evidence_dropped": outcome.plan.evidence_dropped,
        "claims": outcome.claims.iter().map(|(text, verdict, _)| json!({
            "text": text,
            "verdict": verdict,
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
        })).collect::<Vec<_>>(),
        "context": outcome.context_text.unwrap_or_default(),
    });
    Ok(CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&payload)?,
    )]))
}
