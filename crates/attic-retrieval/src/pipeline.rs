//! Retrieval pipeline orchestration (Phase 4): the full
//! question → classification → contract → plan → candidates → fusion →
//! ranking → validation → sufficiency → expansion → context → claims →
//! verification flow, with per-stage observability in the RetrievalPlan.
//!
//! The service mirrors the Phase 1D coordinated-store pattern: reads go
//! through the bounded reader pool (`with_reader`); the only write is the
//! finalized plan, submitted through the writer queue before the answer
//! returns.

use std::path::PathBuf;

use attic_storage::{DbPool, NewRetrievalPlanRecord, StorageError, WriterQueueHandle};

use attic_evidence::{ClaimVerdict, Contradiction, Evidence, EvidenceSourceType as ST};
use rusqlite::Connection;

use crate::budget::BudgetAccountant;
use crate::candidates::{
    CrossRepoGenerator, GeneratorEnv, KnowledgeGenerator, LexicalGenerator, PathExactGenerator,
    RelationshipGenerator, StructuralGenerator, SymbolGenerator, retriever_from_str,
};
use crate::context;
use crate::contract::{FallbackStrategy, QueryEvidenceContract, contract_for};
use crate::contradiction::detect as detect_contradictions;
use crate::error::RetrievalError;
use crate::fuse::fuse;
use crate::manager::{
    ExpansionAction, SufficiencyReport, apply_contradictions, next_expansion, run_graph_expansion,
};
use crate::mode::{AnswerMode, AnswerModePolicy, PolicyOverrides};
use crate::plan::{
    ConfidenceLevel, DroppedEvidence, PlanResult, QUERY_REDACTION_MARKER, RetrievalPlan,
    StepStatus, SubsystemTag,
};
use crate::query::{Classification, QueryType, classify};
use crate::rank::{apply_signals_and_rank, sort_ranked};
use crate::validate::{ValidationVerdict, validate};
use crate::verify;

/// Everything a query needs. Cloneable like `IndexingStore`.
///
/// Source-verification roots are resolved PER REPOSITORY from the index at
/// verification time, so multi-repository workspaces verify against the
/// correct checkout without any caller-supplied path.
#[derive(Clone)]
pub struct RetrievalService {
    /// Bounded read-only connection pool.
    pub readers: DbPool,
    /// Coordinated single-writer endpoint (plan persistence only).
    pub writer: WriterQueueHandle,
    /// Phase 5 disposable semantic layer; `None` keeps byte-identical
    /// Phase 4 behavior (deleting the semantic DB must degrade, never break).
    pub semantic: Option<std::sync::Arc<crate::semantic::SemanticStack>>,
    /// Phase 6 cross-repo subsystem health.  When `true`, cross-repo
    /// evidence generation is skipped (subsystem degraded/unknown).
    /// Local retrieval continues unaffected.
    pub crossrepo_degraded: bool,
}

/// One inbound answer request.
#[derive(Debug, Clone)]
pub struct AnswerRequest {
    pub question: String,
    pub mode: AnswerMode,
    /// Repository filter; empty = workspace-wide.
    pub repository_ids: Vec<String>,
    pub overrides: Option<PolicyOverrides>,
}

impl AnswerRequest {
    pub fn new(question: impl Into<String>, mode: AnswerMode) -> Self {
        Self {
            question: question.into(),
            mode,
            repository_ids: Vec::new(),
            overrides: None,
        }
    }
}

/// Served-claim tuple type (text, verdict tag, evidence ids).
pub type ServedClaims = Vec<(String, String, Vec<String>)>;

/// Final outcome of [`RetrievalService::answer`].
#[derive(Debug, Clone)]
pub struct AnswerOutcome {
    pub plan: RetrievalPlan,
    pub result: PlanResult,
    pub confidence: ConfidenceLevel,
    /// Assembled context text when evidence exists.
    pub context_text: Option<String>,
    /// (claim text, verdict tag, evidence ids) for served claims.
    pub claims: Vec<(String, String, Vec<String>)>,
    /// Unsatisfied requirement labels on PARTIAL/INSUFFICIENT results.
    pub insufficient_reason: Option<String>,
}

fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// Deterministic workspace id: BLAKE3 hex of the repository scope. Raw ids
/// and paths are never stored in plans.
fn scoped_workspace_id(repository_ids: &[String]) -> String {
    match repository_ids.first() {
        Some(r) => blake3::hash(r.as_bytes()).to_hex().to_string(),
        None => blake3::hash(b"workspace").to_hex().to_string(),
    }
}

/// Resolve the canonical on-disk root of a repository from the index.
fn repo_root_for(conn: &Connection, repository_id: &str) -> Option<PathBuf> {
    use std::str::FromStr as _;
    let rid = attic_core::RepositoryId::from_str(repository_id).ok()?;
    let s = attic_storage::get_repository_path(conn, &rid).ok()??;
    let p = PathBuf::from(&s);
    p.canonicalize().ok().or(Some(p))
}

/// Everything produced by the DB-bound half of the pipeline.
struct DbPhaseOutcome {
    validated: Vec<Evidence>,
    report: SufficiencyReport,
    contradictions: Vec<Contradiction>,
    budget: BudgetAccountant,
    hard_cancelled: bool,
    semantic: crate::semantic::SemanticOutcome,
}

/// Generation context: everything the per-step generator invocations share.
struct GenCtx<'a> {
    #[allow(dead_code)] // reserved for generator-side service callbacks
    service: &'a RetrievalService,
    conn: &'a Connection,
    repo_filter: &'a Option<String>,
    budget: &'a mut BudgetAccountant,
    collected: &'a mut Vec<crate::candidates::Candidate>,
}

impl GenCtx<'_> {
    fn run<F>(
        &mut self,
        plan: &mut RetrievalPlan,
        subsystem: SubsystemTag,
        operation: &str,
        summary: &str,
        f: F,
    ) where
        F: FnOnce(
            &mut GeneratorEnv<'_>,
        ) -> Result<Vec<crate::candidates::Candidate>, RetrievalError>,
    {
        if !self.budget.candidates_available() || self.budget.time_exceeded() {
            return;
        }
        let s = plan.begin_step(subsystem, operation, summary, now_us());
        let mut env = GeneratorEnv {
            conn: self.conn,
            repository_id: self.repo_filter.clone(),
            budget: self.budget,
            limit: 48,
        };
        match f(&mut env) {
            Ok(cands) => {
                let n = cands.len() as u32;
                self.collected.extend(cands);
                plan.complete_step(s, StepStatus::Completed, "candidates", 0, n, now_us());
            }
            Err(e) => {
                plan.complete_step(s, StepStatus::Failed, &e.to_string(), 0, 0, now_us());
            }
        }
    }
}

/// Admit expansion-round evidence through the same validation gate as the
/// initial pass; rejected items are recorded as dropped.
fn admit_evidence(
    plan: &mut RetrievalPlan,
    contract: &QueryEvidenceContract,
    validated: &mut Vec<Evidence>,
    incoming: Vec<Evidence>,
) {
    let before = validated.len();
    for ev in incoming {
        let verdict: ValidationVerdict = validate(&ev, contract);
        match verdict.drop_reason {
            Some(reason) => plan.evidence_dropped.push(DroppedEvidence {
                evidence_id: ev.id.clone(),
                source_type: ev.source_type.as_str().to_owned(),
                drop_reason: reason,
                score: ev.signals.combined_score.unwrap_or(0.0),
            }),
            None => validated.push(ev),
        }
    }
    let _ = before;
}

impl RetrievalService {
    /// Run the complete pipeline for one question.
    ///
    /// The plan is ALWAYS finalized and persisted before returning (RP-L3);
    /// persistence failure logs but does not block the answer (RP-INV-6).
    pub fn answer(&self, req: &AnswerRequest) -> Result<AnswerOutcome, RetrievalError> {
        // ── RP-S2 query redaction ───────────────────────────────────────────
        let scan = attic_discovery::secrets::scan_and_redact(&req.question);
        let stored_query = if !scan.findings.is_empty() {
            tracing::warn!("query contained suspected secret; redacted in plan");
            QUERY_REDACTION_MARKER.to_owned()
        } else {
            req.question.clone()
        };

        let classification = classify(&req.question)?;
        let contract = contract_for(classification.query_type);
        let policy = match &req.overrides {
            Some(o) => AnswerModePolicy::with_overrides(req.mode, o)?,
            None => {
                let p = AnswerModePolicy::for_mode(req.mode);
                p.validate()?;
                p
            }
        };

        let mut plan = RetrievalPlan::create(
            uuid::Uuid::new_v4().to_string(),
            now_us(),
            stored_query,
            &classification,
            scoped_workspace_id(&req.repository_ids),
            &contract,
            policy.clone(),
        );

        // ── AM-I1: FAST + explanation-type queries are incompatible ────────
        if policy.mode == AnswerMode::Fast
            && matches!(
                classification.query_type,
                QueryType::ArchitectureExplanation | QueryType::KnowledgeQuestion
            )
        {
            let s = plan.begin_step(
                SubsystemTag::PolicyEnforcer,
                "incompatible_policy_warning",
                "FAST with explanation-type query",
                now_us(),
            );
            tracing::warn!(
                "IncompatiblePolicyForQuery: FAST + {}; result flagged POTENTIALLY_INCOMPLETE",
                classification.query_type.as_str()
            );
            plan.complete_step(
                s,
                StepStatus::Degraded,
                "POTENTIALLY_INCOMPLETE",
                0,
                0,
                now_us(),
            );
        }

        // ── Planner: record intended operations ─────────────────────────────
        let ex = &classification.extracted;
        if !ex.terms.is_empty() {
            plan.planned_lexical_queries.push(ex.terms.join(" "));
        }
        if let Some(sym) = &ex.symbol_hint {
            plan.planned_symbol_lookups.push(sym.clone());
        }
        if matches!(
            classification.query_type,
            QueryType::ArchitectureExplanation
                | QueryType::ImpactAnalysis
                | QueryType::DependencyQuestion
        ) {
            plan.planned_structural_ops
                .push("outline_of_candidates".into());
        }
        if contract
            .allowed_fallbacks
            .contains(&FallbackStrategy::BoundedGraph)
        {
            plan.planned_graph_ops
                .push(format!("bounded_walk_depth_{}", policy.max_graph_depth));
        }
        if classification.query_type == QueryType::KnowledgeQuestion && !ex.terms.is_empty() {
            plan.planned_knowledge_ops.push(ex.terms.join(" "));
        }

        // ── DB-bound phases on one pooled read-only connection ──────────────
        let repo_filter = req.repository_ids.first().cloned();
        let single_repo = !req.repository_ids.is_empty();

        let phase: Result<DbPhaseOutcome, StorageError> = self.readers.with_reader(|conn| {
            self.run_db_phases(
                conn,
                &mut plan,
                BudgetAccountant::new(&policy),
                &classification,
                &contract,
                &policy,
                &repo_filter,
                single_repo,
            )
        });
        let DbPhaseOutcome {
            validated,
            report,
            contradictions,
            budget,
            hard_cancelled,
            semantic: semantic_outcome,
        } = phase.map_err(RetrievalError::Storage)?;

        // ── Outcome assembly ────────────────────────────────────────────────
        let (result, confidence, insufficient_reason) =
            compute_result(&report, validated.len(), hard_cancelled);

        let (context_text, claims_out): (Option<String>, ServedClaims) =
            if validated.is_empty() || hard_cancelled {
                (None, Vec::new())
            } else {
                build_context_and_claims(
                    self,
                    &mut plan,
                    &contract,
                    &classification,
                    &policy,
                    &validated,
                    &contradictions,
                )
            };

        // ── Trace + finalize + persist ──────────────────────────────────────
        plan.policy_trace.time_elapsed_ms = budget.elapsed_ms();
        plan.policy_trace.candidates_examined = budget.candidates_used;
        plan.policy_trace.graph_nodes_visited = budget.graph_nodes_used;
        plan.policy_trace.fs_files_read = budget.fs_files_used;
        plan.policy_trace.fs_bytes_read = budget.fs_bytes_used;
        plan.policy_trace.context_tokens_used = plan.context_tokens;
        plan.policy_trace.semantic_invoked = semantic_outcome.candidates > 0;
        // Truthful observability: NO reranker exists in Phase 5 (ADR-014 D7).
        // Policy permission is NOT execution — this stays false until a
        // reranker actually runs.
        plan.policy_trace.reranking_invoked = false;
        plan.policy_trace.repair_cycles = plan.repair_cycles;
        plan.policy_trace.semantic_candidates_returned = semantic_outcome.candidates as u32;
        plan.policy_trace.semantic_fallback_reason = semantic_outcome.fallback.as_str().to_owned();
        plan.policy_trace.budget_fields_hit = budget.limits_hit().to_vec();
        plan.insufficiency_reason = insufficient_reason.clone();
        plan.policy_trace.final_result =
            budget.derive_final_result(!report.sufficient, hard_cancelled);

        plan.finalize(result, confidence, now_us());

        persist_plan(&self.writer, &plan)?;

        Ok(AnswerOutcome {
            plan,
            result,
            confidence,
            context_text,
            claims: claims_out,
            insufficient_reason,
        })
    }

    /// All phases that need the read-only connection: candidate generation,
    /// fusion, ranking, validation, sufficiency + bounded expansion,
    /// contradiction detection.
    #[allow(clippy::too_many_arguments)]
    fn run_db_phases(
        &self,
        conn: &Connection,
        plan: &mut RetrievalPlan,
        mut budget: BudgetAccountant,
        classification: &Classification,
        contract: &QueryEvidenceContract,
        policy: &AnswerModePolicy,
        repo_filter: &Option<String>,
        single_repo: bool,
    ) -> Result<DbPhaseOutcome, StorageError> {
        let qt = classification.query_type;
        let ex = &classification.extracted;
        let mut collected: Vec<crate::candidates::Candidate> = Vec::new();

        {
            let mut ctx = GenCtx {
                service: self,
                conn,
                repo_filter,
                budget: &mut budget,
                collected: &mut collected,
            };
            if !ex.terms.is_empty() {
                ctx.run(
                    plan,
                    SubsystemTag::FtsSearch,
                    "lexical_generation",
                    &format!("terms={}", ex.terms.join(",")),
                    |env| LexicalGenerator::run(env, &ex.terms),
                );
            }
            if let Some(path) = ex.path_hint.clone() {
                ctx.run(plan, SubsystemTag::FtsSearch, "path_lookup", &path, |env| {
                    PathExactGenerator::run(env, &path)
                });
            }
            if let Some(sym) = ex.symbol_hint.clone() {
                ctx.run(
                    plan,
                    SubsystemTag::SymbolLookup,
                    "symbol_search",
                    &sym,
                    |env| SymbolGenerator::run(env, &sym),
                );
            }
            if qt == QueryType::KnowledgeQuestion && !ex.terms.is_empty() {
                ctx.run(
                    plan,
                    SubsystemTag::EvidenceAssembler,
                    "knowledge_lookup",
                    "knowledge/docs",
                    |env| KnowledgeGenerator::run(env, &ex.terms),
                );
            }
            // ctx still borrows budget; seeds read only `collected`, so
            // compute them through the ctx's collected reference instead.
        }

        // Seeds from what was found so far (pre-fusion ids suffice); ctx
        // dropped above so `collected` is exclusively ours again.
        let seed_files: Vec<String> = collected
            .iter()
            .filter(|c| c.evidence.source_type != ST::Relationship)
            .take(6)
            .map(|c| c.evidence.source_id.clone())
            .collect();

        {
            let mut ctx = GenCtx {
                service: self,
                conn,
                repo_filter,
                budget: &mut budget,
                collected: &mut collected,
            };
            if qt == QueryType::ArchitectureExplanation && !seed_files.is_empty() {
                ctx.run(
                    plan,
                    SubsystemTag::StructuralLookup,
                    "structural_outline",
                    "seed files",
                    |env| StructuralGenerator::run_for_files(env, &seed_files),
                );
            }
            if contract
                .allowed_fallbacks
                .contains(&FallbackStrategy::BoundedGraph)
                && !seed_files.is_empty()
            {
                ctx.run(
                    plan,
                    SubsystemTag::GraphWalk,
                    "direct_relationship_edges",
                    "seeds",
                    |env| RelationshipGenerator::run(env, &seed_files),
                );
                // Phase 6: cross-repository dependency edges from other
                // workspaces the user has added to the catalog.
                if self.crossrepo_degraded {
                    tracing::warn!(
                        "cross-repo subsystem degraded; skipping cross-repo evidence generation"
                    );
                } else {
                    ctx.run(
                        plan,
                        SubsystemTag::GraphWalk,
                        "cross_repo_dependency_edges",
                        "seeds",
                        |env| CrossRepoGenerator::run(env, &seed_files),
                    );
                }
            }
        }

        // ── Phase 5: semantic candidate generation (§12–§15) ────────────────
        // Another bounded producer feeding the SAME fusion/rank/validation
        // chain below. Every non-contribution reason is recorded.
        let mut semantic_outcome = crate::semantic::SemanticOutcome {
            candidates: 0,
            fallback: crate::semantic::SemanticFallback::Disabled,
        };
        if let Some(stack) = self.semantic.as_ref() {
            // Query text for embedding: extracted terms (+ symbol/path hints)
            // — the same canonical extraction every other generator consumes.
            let mut semantic_query = ex.terms.join(" ");
            if let Some(sym) = &ex.symbol_hint {
                semantic_query.push(' ');
                semantic_query.push_str(sym);
            }
            if let Some(p) = &ex.path_hint {
                semantic_query.push(' ');
                semantic_query.push_str(p);
            }
            if !semantic_query.trim().is_empty() {
                let s_sem = plan.begin_step(
                    SubsystemTag::FtsSearch, // nearest tag; per-candidate origin is SEMANTIC
                    "semantic_knn",
                    &format!(
                        "provider={} model={}",
                        stack.provider_id(),
                        stack.model_id()
                    ),
                    now_us(),
                );
                let mut env = GeneratorEnv {
                    conn,
                    repository_id: repo_filter.clone(),
                    budget: &mut budget,
                    limit: 48,
                };
                match crate::semantic::SemanticCandidateGenerator::run(
                    &mut env,
                    stack,
                    policy,
                    &semantic_query,
                ) {
                    Ok((cands, outcome)) => {
                        semantic_outcome = outcome;
                        let n = cands.len() as u32;
                        collected.extend(cands);
                        plan.complete_step(
                            s_sem,
                            if n > 0 {
                                StepStatus::Completed
                            } else {
                                StepStatus::Skipped
                            },
                            if n > 0 {
                                "candidates"
                            } else {
                                outcome.fallback.as_str()
                            },
                            0,
                            n,
                            now_us(),
                        );
                    }
                    Err(e) => {
                        semantic_outcome.fallback =
                            crate::semantic::SemanticFallback::ProviderUnavailable;
                        plan.complete_step(
                            s_sem,
                            StepStatus::Failed,
                            &e.to_string(),
                            0,
                            0,
                            now_us(),
                        );
                    }
                }
            }
        }

        // ── Fusion + ranking ────────────────────────────────────────────────
        let mut fused = fuse(std::mem::take(&mut collected));

        // EXACT_LOOKUP scope: when a concrete path was requested, candidates
        // outside that path cannot serve the contract; drop them
        // observably instead of diluting the context.
        if qt == QueryType::ExactLookup
            && let Some(hint) = &ex.path_hint
        {
            let hint = hint.trim_start_matches("./");
            fused.retain(|c| {
                let matches =
                    c.evidence.path == hint || c.evidence.path.ends_with(&format!("/{hint}"));
                if !matches {
                    plan.evidence_dropped.push(DroppedEvidence {
                        evidence_id: c.evidence.id.clone(),
                        source_type: c.evidence.source_type.as_str().to_owned(),
                        drop_reason: crate::plan::DropReason::PolicyBlockedSourceType,
                        score: c.evidence.signals.combined_score.unwrap_or(0.0),
                    });
                }
                matches
            });
        }

        let mut ranked: Vec<Evidence> = fused
            .into_iter()
            .filter(|c| c.evidence.freshness_state != attic_core::FreshnessState::Invalid)
            .map(|c| {
                let kind = retriever_from_str(
                    c.evidence
                        .retrieval_sources
                        .first()
                        .map(|s| s.retriever_type.as_str())
                        .unwrap_or("FTS"),
                )
                .unwrap_or(crate::candidates::RetrieverKind::Fts);
                apply_signals_and_rank(c.evidence, qt, kind, single_repo)
            })
            .collect();
        sort_ranked(&mut ranked);
        ranked.truncate(policy.max_candidates as usize);

        // ── Validation ──────────────────────────────────────────────────────
        let s_val = plan.begin_step(
            SubsystemTag::EvidenceAssembler,
            "validation",
            "contract checks",
            now_us(),
        );
        let mut validated: Vec<Evidence> = Vec::new();
        for ev in ranked {
            let verdict = validate(&ev, contract);
            match verdict.drop_reason {
                Some(reason) => plan.evidence_dropped.push(DroppedEvidence {
                    evidence_id: ev.id.clone(),
                    source_type: ev.source_type.as_str().to_owned(),
                    drop_reason: reason,
                    score: ev.signals.combined_score.unwrap_or(0.0),
                }),
                None => {
                    let mut ev = ev;
                    if !verdict.counts_toward_required {
                        ev.retrieval_sources.push(attic_evidence::RetrievalSource {
                            retriever_type: "CONTEXT_ONLY".to_owned(),
                            score: 0.0,
                            query_fragment: verdict.explanation.clone(),
                        });
                    }
                    validated.push(ev);
                }
            }
        }
        plan.complete_step(
            s_val,
            StepStatus::Completed,
            &format!("validated={}", validated.len()),
            validated.len() as u32,
            validated.len() as u32,
            now_us(),
        );

        // ── Proactive CHECKSUM/FULL verification of top evidence ───────────
        // answer_modes.md: NORMAL=CHECKSUM, DEEP=FULL. This is what makes a
        // DIRTY WORKING TREE detectable: an indexed row can still be flagged
        // CURRENT while the live file has moved on; only reading the secure
        // source path reveals that. Bounded by the FS budget and item cap.
        if policy.fs_reads_permitted() && !budget.time_exceeded() {
            let s_pv = plan.begin_step(
                SubsystemTag::SourceVerifier,
                "proactive_checksum",
                &format!(
                    "level={} items<=5",
                    policy.source_verification_level.as_str()
                ),
                now_us(),
            );
            let mut changed = 0usize;
            let mut degraded = false;
            let targets: Vec<usize> = validated
                .iter()
                .enumerate()
                .filter(|(_, ev)| ev.source_type != ST::Relationship && ev.snippet.is_some())
                .take(5)
                .map(|(i, _)| i)
                .collect();
            for i in targets {
                let Some(repo_root) = repo_root_for(conn, &validated[i].repository_id) else {
                    continue;
                };
                match verify::verify_evidence(&mut validated[i], &repo_root, policy, &mut budget) {
                    Ok(verify::VerifyOutcome::ContentChanged) => {
                        validated[i].freshness_state = attic_core::FreshnessState::Stale;
                        changed += 1;
                    }
                    Ok(verify::VerifyOutcome::BlockedByBudget) => {
                        degraded = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.to_string();
                        plan.complete_step(s_pv, StepStatus::Failed, &msg, 0, 0, now_us());
                        return Err(StorageError::Worker(msg));
                    }
                }
            }
            plan.complete_step(
                s_pv,
                if degraded {
                    StepStatus::Degraded
                } else {
                    StepStatus::Completed
                },
                &format!("changed={changed}"),
                0,
                0,
                now_us(),
            );
        }

        // ── Sufficiency + bounded targeted expansion ────────────────────────
        let mut report = SufficiencyReport::evaluate(contract, &validated);
        let mut used_fallbacks: Vec<FallbackStrategy> = Vec::new();
        let mut rounds = 0u32;
        let mut hard_cancelled = false;

        while !report.sufficient
            && rounds < contract.expansion_budget.max_expansion_rounds
            && !budget.time_exceeded()
        {
            let Some(action) = next_expansion(contract, &used_fallbacks, &validated) else {
                break;
            };
            rounds += 1;
            plan.repair_cycles = rounds
                .min(u8::MAX as u32)
                .min(policy.repair_attempts.max(1) as u32) as u8;

            // Per-action subsystem tags keep the plan honest about which
            // mechanism performed each expansion.
            let (s_idx, tag) = match &action {
                ExpansionAction::SourceVerification(_) => {
                    let s = plan.begin_step(
                        SubsystemTag::SourceVerifier,
                        "source_verification",
                        "stale/unverified targets",
                        now_us(),
                    );
                    (s, SubsystemTag::SourceVerifier)
                }
                ExpansionAction::BoundedGraph(_) => {
                    let s = plan.begin_step(
                        SubsystemTag::GraphWalk,
                        "bounded_graph_expansion",
                        "seed entities",
                        now_us(),
                    );
                    (s, SubsystemTag::GraphWalk)
                }
                _ => {
                    let s = plan.begin_step(
                        SubsystemTag::RepairExpander,
                        "targeted_expansion",
                        &format!("round {rounds}"),
                        now_us(),
                    );
                    (s, SubsystemTag::RepairExpander)
                }
            };
            let _ = tag;

            match action {
                ExpansionAction::BroaderFts(terms) => {
                    used_fallbacks.push(FallbackStrategy::BroaderFts);
                    if terms.is_empty() {
                        plan.complete_step(
                            s_idx,
                            StepStatus::Skipped,
                            "no broaden terms",
                            0,
                            0,
                            now_us(),
                        );
                        continue;
                    }
                    let mut env = GeneratorEnv {
                        conn,
                        repository_id: repo_filter.clone(),
                        budget: &mut budget,
                        limit: 48,
                    };
                    match LexicalGenerator::run(&mut env, &terms) {
                        Ok(cands) => {
                            let new_ev: Vec<Evidence> = fuse(cands)
                                .into_iter()
                                .map(|c| rank_one(c, qt, single_repo))
                                .collect();
                            let added = new_ev.len();
                            admit_evidence(plan, contract, &mut validated, new_ev);
                            plan.complete_step(
                                s_idx,
                                StepStatus::Completed,
                                &format!("+{added}"),
                                0,
                                added as u32,
                                now_us(),
                            );
                        }
                        Err(e) => plan.complete_step(
                            s_idx,
                            StepStatus::Failed,
                            &e.to_string(),
                            0,
                            0,
                            now_us(),
                        ),
                    }
                }
                ExpansionAction::KnowledgeLookup(terms) => {
                    used_fallbacks.push(FallbackStrategy::KnowledgeLookup);
                    if terms.is_empty() {
                        plan.complete_step(
                            s_idx,
                            StepStatus::Skipped,
                            "no knowledge terms",
                            0,
                            0,
                            now_us(),
                        );
                        continue;
                    }
                    let mut env = GeneratorEnv {
                        conn,
                        repository_id: repo_filter.clone(),
                        budget: &mut budget,
                        limit: 48,
                    };
                    match KnowledgeGenerator::run(&mut env, &terms) {
                        Ok(cands) => {
                            let new_ev: Vec<Evidence> = fuse(cands)
                                .into_iter()
                                .map(|c| rank_one(c, qt, single_repo))
                                .collect();
                            let added = new_ev.len();
                            admit_evidence(plan, contract, &mut validated, new_ev);
                            plan.complete_step(
                                s_idx,
                                StepStatus::Completed,
                                &format!("+{added} knowledge"),
                                0,
                                added as u32,
                                now_us(),
                            );
                        }
                        Err(e) => plan.complete_step(
                            s_idx,
                            StepStatus::Failed,
                            &e.to_string(),
                            0,
                            0,
                            now_us(),
                        ),
                    }
                }
                ExpansionAction::BoundedGraph(seeds) => {
                    used_fallbacks.push(FallbackStrategy::BoundedGraph);
                    match run_graph_expansion(
                        conn,
                        contract,
                        policy.max_graph_depth,
                        seeds,
                        &mut budget,
                    ) {
                        Ok(new_ev) => {
                            let added = new_ev.len();
                            admit_evidence(plan, contract, &mut validated, new_ev);
                            plan.complete_step(
                                s_idx,
                                StepStatus::Completed,
                                &format!("graph +{added}"),
                                0,
                                added as u32,
                                now_us(),
                            );
                        }
                        Err(e) => plan.complete_step(
                            s_idx,
                            StepStatus::Failed,
                            &e.to_string(),
                            0,
                            0,
                            now_us(),
                        ),
                    }
                }
                ExpansionAction::SourceVerification(ids) => {
                    used_fallbacks.push(FallbackStrategy::SourceVerification);
                    if !policy.fs_reads_permitted() {
                        // AM-02: observable refusal, never a silent skip and
                        // never an attempted read.
                        plan.complete_step(
                            s_idx,
                            StepStatus::Failed,
                            "PolicyViolation: filesystem reads forbidden by mode budget",
                            0,
                            0,
                            now_us(),
                        );
                    } else {
                        let mut degraded = false;
                        for id in ids {
                            let Some(pos) = validated.iter_mut().position(|e| e.id == id) else {
                                continue;
                            };
                            // Per-repository secure root from the index
                            // (never caller-supplied paths).
                            let Some(repo_root) =
                                repo_root_for(conn, &validated[pos].repository_id)
                            else {
                                plan.complete_step(
                                    s_idx,
                                    StepStatus::Degraded,
                                    "repository root unavailable",
                                    0,
                                    0,
                                    now_us(),
                                );
                                continue;
                            };
                            match verify::verify_evidence(
                                &mut validated[pos],
                                &repo_root,
                                policy,
                                &mut budget,
                            ) {
                                Ok(verify::VerifyOutcome::VerifiedCurrent) => {}
                                Ok(verify::VerifyOutcome::ContentChanged) => {
                                    validated[pos].freshness_state =
                                        attic_core::FreshnessState::Stale;
                                }
                                Ok(verify::VerifyOutcome::BlockedByBudget) => {
                                    degraded = true;
                                    break;
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    let msg = e.to_string();
                                    plan.complete_step(
                                        s_idx,
                                        StepStatus::Failed,
                                        &msg,
                                        0,
                                        0,
                                        now_us(),
                                    );
                                    return Err(StorageError::Worker(msg));
                                }
                            }
                        }
                        plan.complete_step(
                            s_idx,
                            if degraded {
                                StepStatus::Degraded
                            } else {
                                StepStatus::Completed
                            },
                            "source verification round",
                            0,
                            0,
                            now_us(),
                        );
                    }
                }
            }

            report = SufficiencyReport::evaluate(contract, &validated);
        }

        if budget.time_exceeded() {
            hard_cancelled = true;
        }

        // ── Contradiction handling (surface, never silently resolve) ────────
        let contradictions = detect_contradictions(&validated);
        if !contradictions.is_empty() {
            apply_contradictions(&mut validated, &contradictions);
            report = SufficiencyReport::evaluate(contract, &validated);
        }

        Ok(DbPhaseOutcome {
            validated,
            report,
            contradictions,
            budget,
            hard_cancelled,
            semantic: semantic_outcome,
        })
    }
}

/// Rank one freshly generated candidate for admission during expansion.
fn rank_one(c: crate::candidates::Candidate, qt: QueryType, single_repo: bool) -> Evidence {
    let kind = retriever_from_str(
        c.evidence
            .retrieval_sources
            .first()
            .map(|s| s.retriever_type.as_str())
            .unwrap_or("FTS"),
    )
    .unwrap_or(crate::candidates::RetrieverKind::Fts);
    apply_signals_and_rank(c.evidence, qt, kind, single_repo)
}

/// Map sufficiency to (result, confidence, reason).
fn compute_result(
    report: &SufficiencyReport,
    validated_len: usize,
    hard_cancelled: bool,
) -> (PlanResult, ConfidenceLevel, Option<String>) {
    if hard_cancelled {
        return (
            PlanResult::PolicyHardCancelled,
            ConfidenceLevel::None_,
            None,
        );
    }
    if report.sufficient {
        let conf = if report.satisfied.iter().all(|(_, ids)| ids.len() > 1) {
            ConfidenceLevel::High
        } else {
            ConfidenceLevel::Medium
        };
        (PlanResult::Success, conf, None)
    } else if validated_len > 0 {
        (
            PlanResult::PartialSuccess,
            ConfidenceLevel::Low,
            Some(report.unsatisfied.join(",")),
        )
    } else {
        (
            PlanResult::InsufficientEvidence,
            ConfidenceLevel::None_,
            Some(report.unsatisfied.join(",")),
        )
    }
}

/// Assemble the bounded context and verified claims (post-DB phases).
#[allow(clippy::too_many_arguments)]
fn build_context_and_claims(
    _service: &RetrievalService,
    plan: &mut RetrievalPlan,
    contract: &QueryEvidenceContract,
    classification: &Classification,
    policy: &AnswerModePolicy,
    validated: &[Evidence],
    contradictions: &[Contradiction],
) -> (Option<String>, ServedClaims) {
    // ── §15 priority floor: context serves the strongest evidence, not a
    // dump of everything validated. Two tiers keep contract-PREFERRED
    // supporting slices (tests/config/knowledge/docs) while cutting noise:
    //   hard floor: 55% of top score (any type)
    //   soft floor: 35% of top score (preferred source types only)
    let top = validated
        .iter()
        .map(|e| e.signals.combined_score.unwrap_or(0.0))
        .fold(0.0f64, f64::max);
    let hard_floor = (top * 0.65).max(0.20);
    let soft_floor = (top * 0.35).max(0.15);
    let preferred: std::collections::HashSet<ST> = contract
        .preferred_evidence
        .iter()
        .flat_map(|r| r.source_types.iter())
        .copied()
        .collect();
    let (keep, weak): (Vec<Evidence>, Vec<Evidence>) = validated.iter().cloned().partition(|e| {
        let s = e.signals.combined_score.unwrap_or(0.0);
        s >= hard_floor || (preferred.contains(&e.source_type) && s >= soft_floor)
    });
    for w in &weak {
        plan.evidence_dropped.push(DroppedEvidence {
            evidence_id: w.id.clone(),
            source_type: w.source_type.as_str().to_owned(),
            drop_reason: crate::plan::DropReason::BelowScoreThreshold,
            score: w.signals.combined_score.unwrap_or(0.0),
        });
    }

    // ── Per-section contribution caps (context diversity bound): the
    // contract-primary section serves fully; every OTHER section contributes
    // at most its two strongest items. Prevents a dominant section from
    // drowning the answer window with marginally-related material.
    let primary_st = contract
        .required_evidence
        .first()
        .and_then(|r| r.source_types.first())
        .copied();
    let mut section_counts: std::collections::HashMap<i32, usize> =
        std::collections::HashMap::new();
    let mut capped: Vec<Evidence> = Vec::with_capacity(keep.len());
    {
        let mut ordered_keep: Vec<&Evidence> = keep.iter().collect();
        ordered_keep.sort_by(|a, b| {
            let ap = primary_st.is_some_and(|p| p == a.source_type);
            let bp = primary_st.is_some_and(|p| p == b.source_type);
            bp.cmp(&ap)
                .then_with(|| {
                    b.signals
                        .combined_score
                        .unwrap_or(0.0)
                        .partial_cmp(&a.signals.combined_score.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        for ev in ordered_keep {
            let is_primary = primary_st.is_some_and(|p| p == ev.source_type);
            let sec = crate::context::section_rank_of(ev.source_type) as i32
                + if is_primary { -100 } else { 0 };
            let served_elsewhere: usize = section_counts
                .iter()
                .filter(|(k, _)| **k != sec)
                .map(|(_, v)| *v)
                .sum();
            let cap = if served_elsewhere > 0 { 1 } else { 2 };
            let count = section_counts.entry(sec).or_insert(0);
            if !is_primary && *count >= cap {
                plan.evidence_dropped.push(DroppedEvidence {
                    evidence_id: ev.id.clone(),
                    source_type: ev.source_type.as_str().to_owned(),
                    drop_reason: crate::plan::DropReason::BelowScoreThreshold,
                    score: ev.signals.combined_score.unwrap_or(0.0),
                });
                continue;
            }
            *count += 1;
            capped.push(ev.clone());
        }
    }
    let keep = capped;

    let s_ctx = plan.begin_step(
        SubsystemTag::ContextTrimmer,
        "context_assembly",
        "validated evidence",
        now_us(),
    );
    let primary = contract
        .required_evidence
        .first()
        .and_then(|r| r.source_types.first())
        .copied();
    let doc = context::build(
        &keep,
        contradictions,
        classification.query_type,
        policy.max_context_tokens,
        primary,
    );
    // RP-INV-7: context_tokens equals the SUM of per-item contributions;
    // boilerplate (headers/disclosures) is reported in the step summary.
    let ref_tokens: u64 = doc.refs.iter().map(|r| r.token_count as u64).sum();
    plan.context_tokens = ref_tokens.min(u32::MAX as u64) as u32;
    plan.evidence_used.extend(doc.refs.iter().cloned());
    for d in &doc.dropped {
        plan.evidence_dropped.push(d.clone());
    }
    plan.complete_step(
        s_ctx,
        if doc.dropped.is_empty() {
            StepStatus::Completed
        } else {
            StepStatus::Degraded
        },
        &format!("tokens={} items={}", doc.tokens, doc.refs.len()),
        doc.refs.len() as u32,
        doc.refs.len() as u32,
        now_us(),
    );

    let derived =
        crate::claims::derive_claims(classification.query_type, validated, contradictions);
    let cfg = crate::claims::VerifyConfig {
        freshness_requirement: contract.freshness_requirement,
        relationship_confidence_min: contract.relationship_confidence_min.unwrap_or(0.6),
    };
    let verified = crate::claims::verify_claims(derived, &keep, contradictions, &cfg);
    // Serve ONLY claims whose backing evidence made it into the assembled
    // context — a claim without visible support is not servable.
    let served_ids: std::collections::HashSet<&str> =
        doc.refs.iter().map(|r| r.evidence_id.as_str()).collect();
    let claims = verified
        .into_iter()
        .filter(|v| v.verdict != ClaimVerdict::Rejected)
        .filter(|v| {
            v.claim
                .evidence_ids
                .iter()
                .all(|id| served_ids.contains(id.as_str()))
        })
        .map(|v| {
            (
                v.claim.text,
                v.verdict.as_str().to_owned(),
                v.claim.evidence_ids,
            )
        })
        .collect();
    (Some(doc.text), claims)
}

/// Persist the finalized plan through the coordinated writer queue.
fn persist_plan(writer: &WriterQueueHandle, plan: &RetrievalPlan) -> Result<(), RetrievalError> {
    let rec = NewRetrievalPlanRecord {
        plan_id: plan.plan_id.clone(),
        query_id: plan.query_id.clone(),
        created_at_us: plan.created_at_us,
        completed_at_us: plan.completed_at_us,
        workspace_id: plan.workspace_id.clone(),
        query_type: plan.query_type.as_str().to_owned(),
        result: plan.result.as_str().to_owned(),
        confidence: plan.final_confidence.as_str().to_owned(),
        policy_mode: plan.policy.mode.as_str().to_owned(),
        context_tokens: plan.context_tokens as i64,
        repair_cycles: plan.repair_cycles as i64,
        plan_json: plan.to_json()?,
    };
    if let Err(e) = writer.send(move |conn| attic_storage::insert_retrieval_plan(conn, &rec)) {
        // RP-INV-6: persistence failure never blocks the answer.
        tracing::error!("PlanPersistenceFailure: {e}");
    }
    Ok(())
}
