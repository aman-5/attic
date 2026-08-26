//! RetrievalPlan (`docs/contracts/retrieval_plan.md`, C13): the
//! serializable, loggable description of every decision made to answer a
//! query. It is the single source of truth for what happened during a query;
//! every subsystem writes a step into it; it is finalized exactly once and
//! persisted before the answer is returned (RP-L3, RP-INV-6 allows the
//! answer through even if persistence fails).

use serde::{Deserialize, Serialize};

use attic_evidence::str_enum;

use crate::mode::{AnswerModePolicy, PolicyExecutionTrace};
use crate::query::{Classification, QueryType};

str_enum! {
    /// Which subsystem emitted a plan step.
    SubsystemTag {
        QueryClassifier => "QUERY_CLASSIFIER",
        FtsSearch => "FTS5_SEARCH",
        SymbolLookup => "SYMBOL_LOOKUP",
        StructuralLookup => "STRUCTURAL_LOOKUP",
        GraphWalk => "GRAPH_WALK",
        SemanticSearch => "SEMANTIC_SEARCH",
        Reranker => "RERANKER",
        SecretFilter => "SECRET_FILTER",
        EvidenceAssembler => "EVIDENCE_ASSEMBLER",
        SourceVerifier => "SOURCE_VERIFIER",
        ContextTrimmer => "CONTEXT_TRIMMER",
        RepairExpander => "REPAIR_EXPANDER",
        PolicyEnforcer => "POLICY_ENFORCER",
    }
}

str_enum! {
    /// Status of one plan step.
    StepStatus {
        Completed => "COMPLETED",
        /// Completed with reduced output due to a budget limit.
        Degraded => "DEGRADED",
        /// Skipped because policy disallows (e.g. FS reads in FAST mode).
        Skipped => "SKIPPED",
        /// Cancelled by deadline or upstream cancellation.
        Cancelled => "CANCELLED",
        /// Internal error; the query can proceed from other evidence.
        Failed => "FAILED",
    }
}

str_enum! {
    /// Why an evidence item was excluded from context.
    DropReason {
        BelowScoreThreshold => "BELOW_SCORE_THRESHOLD",
        ContextTokenLimit => "CONTEXT_TOKEN_LIMIT",
        SecretContentDetected => "SECRET_CONTENT_DETECTED",
        StaleBeyondThreshold => "STALE_BEYOND_THRESHOLD",
        PolicyBlockedSourceType => "POLICY_BLOCKED_SOURCE_TYPE",
        DuplicateContent => "DUPLICATE_CONTENT",
        CandidatesLimitReached => "CANDIDATES_LIMIT_REACHED",
        ProvenanceInvalid => "PROVENANCE_INVALID",
        SpanInvalid => "SPAN_INVALID",
        AuthorityNotApplicable => "AUTHORITY_NOT_APPLICABLE",
        RelationshipConfidenceTooLow => "RELATIONSHIP_CONFIDENCE_TOO_LOW",
        FreshnessBelowRequirement => "FRESHNESS_BELOW_REQUIREMENT",
    }
}

str_enum! {
    /// Final plan result.
    PlanResult {
        /// Contract satisfied; answer produced.
        Success => "SUCCESS",
        /// Contract partially satisfied; low-confidence answer.
        PartialSuccess => "PARTIAL_SUCCESS",
        /// Contract unsatisfied after repair cycles.
        InsufficientEvidence => "INSUFFICIENT_EVIDENCE",
        /// Cancelled by budget enforcement.
        PolicyHardCancelled => "POLICY_HARD_CANCELLED",
        /// Query type not handled in V1.
        QueryTypeUnsupported => "QUERY_TYPE_UNSUPPORTED",
        /// Unexpected subsystem failure.
        InternalError => "INTERNAL_ERROR",
    }
}

str_enum! {
    /// Final answer confidence.
    ConfidenceLevel {
        High => "HIGH",
        Medium => "MEDIUM",
        Low => "LOW",
        None_ => "NONE",
    }
}

/// One ordered pipeline step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub step_id: u16,
    pub subsystem: SubsystemTag,
    pub operation: String,
    pub started_at_us: i64,
    pub ended_at_us: i64,
    pub status: StepStatus,
    /// Compact description of inputs (no raw content — RP-S1).
    pub input_summary: String,
    /// Compact description of outputs (no raw content).
    pub output_summary: String,
    pub candidates_in: u32,
    pub candidates_out: u32,
}

/// Reference to evidence that reached the assembled context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub evidence_id: String,
    /// Canonical source type string.
    pub source_type: String,
    /// Final rank in the assembled context (0 = highest).
    pub rank: u16,
    /// Composite relevance score [0.0, 1.0] (f32 per contract).
    pub score: f32,
    /// Tokens contributed to the context (approx. bytes/4).
    pub token_count: u32,
}

/// Evidence considered but excluded, with its deterministic reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DroppedEvidence {
    pub evidence_id: String,
    pub source_type: String,
    pub drop_reason: DropReason,
    pub score: f64,
}

/// The canonical serializable RetrievalPlan.
///
/// Planned operations capture WHAT the planner intends; steps capture what
/// actually happened; evidence refs / dropped items account for EVERY piece
/// of considered evidence (RP-INV-4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalPlan {
    // Identity
    pub plan_id: String,
    pub query_id: String,
    pub created_at_us: i64,
    pub completed_at_us: Option<i64>,

    // Input snapshot
    /// Original query text verbatim — or the RP-S2 redaction marker when the
    /// query itself matched a secret pattern.
    pub raw_query: String,
    pub query_type: QueryType,
    pub classification_confidence: String,
    pub classification_signals: Vec<String>,
    pub competing_types: Vec<QueryType>,
    /// SHA-256 hex of workspace root path (never the path itself).
    pub workspace_id: String,

    // Planned operations (planner output; observable/debuggable)
    pub planned_lexical_queries: Vec<String>,
    pub planned_symbol_lookups: Vec<String>,
    pub planned_structural_ops: Vec<String>,
    pub planned_graph_ops: Vec<String>,
    pub planned_knowledge_ops: Vec<String>,
    /// Source-verification policy tag (NONE | CHECKSUM | FULL).
    pub source_verification_policy: String,
    /// Serialized requirement labels from the applied contract.
    pub evidence_requirements: Vec<String>,
    /// Mode budgets snapshot for traceability.
    pub policy: AnswerModePolicy,

    // Execution trace
    pub steps: Vec<PlanStep>,
    pub evidence_used: Vec<EvidenceRef>,
    pub evidence_dropped: Vec<DroppedEvidence>,

    // Result
    pub result: PlanResult,
    pub final_confidence: ConfidenceLevel,
    pub context_tokens: u32,
    pub repair_cycles: u8,
    pub policy_trace: PolicyExecutionTrace,
    /// Set on INSUFFICIENT_EVIDENCE with the unsatisfied requirement label.
    pub insufficiency_reason: Option<String>,
}

/// Redaction marker mandated by RP-S2.
pub const QUERY_REDACTION_MARKER: &str = "<REDACTED: suspected_secret_in_query>";

impl RetrievalPlan {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        query_id: String,
        now_us: i64,
        raw_query: String,
        classification: &Classification,
        workspace_id: String,
        contract: &crate::contract::QueryEvidenceContract,
        policy: AnswerModePolicy,
    ) -> Self {
        Self {
            plan_id: uuid::Uuid::new_v4().to_string(),
            query_id,
            created_at_us: now_us,
            completed_at_us: None,
            raw_query,
            query_type: classification.query_type,
            classification_confidence: classification.confidence.as_str().to_owned(),
            classification_signals: classification.matched_signals.clone(),
            competing_types: classification.competing_types.clone(),
            workspace_id,
            planned_lexical_queries: Vec::new(),
            planned_symbol_lookups: Vec::new(),
            planned_structural_ops: Vec::new(),
            planned_graph_ops: Vec::new(),
            planned_knowledge_ops: Vec::new(),
            source_verification_policy: policy.source_verification_level.as_str().to_owned(),
            evidence_requirements: contract
                .required_evidence
                .iter()
                .map(|r| r.evidence_type.clone())
                .collect(),
            policy,
            steps: Vec::new(),
            evidence_used: Vec::new(),
            evidence_dropped: Vec::new(),
            result: PlanResult::InternalError,
            final_confidence: ConfidenceLevel::None_,
            context_tokens: 0,
            repair_cycles: 0,
            policy_trace: PolicyExecutionTrace::new(),
            insufficiency_reason: None,
        }
    }

    /// Append a step; returns its index. Steps are append-only (RP-INV-2).
    pub fn begin_step(
        &mut self,
        subsystem: SubsystemTag,
        operation: &str,
        input_summary: &str,
        now_us: i64,
    ) -> usize {
        let id = self.steps.len() as u16;
        self.steps.push(PlanStep {
            step_id: id,
            subsystem,
            operation: operation.to_owned(),
            started_at_us: now_us,
            ended_at_us: 0,
            status: StepStatus::Completed,
            input_summary: truncate_summary(input_summary),
            output_summary: String::new(),
            candidates_in: 0,
            candidates_out: 0,
        });
        id as usize
    }

    /// Complete a previously begun step.
    pub fn complete_step(
        &mut self,
        idx: usize,
        status: StepStatus,
        output_summary: &str,
        candidates_in: u32,
        candidates_out: u32,
        now_us: i64,
    ) {
        if let Some(s) = self.steps.get_mut(idx) {
            s.status = status;
            s.output_summary = truncate_summary(output_summary);
            s.candidates_in = candidates_in;
            s.candidates_out = candidates_out;
            s.ended_at_us = now_us.max(s.started_at_us);
        }
    }

    /// Finalize the plan exactly once. Double finalization is a programming
    /// error and panics (RP-INV-1) — this is a construction-time invariant
    /// owned by the pipeline.
    pub fn finalize(&mut self, result: PlanResult, confidence: ConfidenceLevel, now_us: i64) {
        assert!(
            self.completed_at_us.is_none(),
            "RetrievalPlan finalized twice"
        );
        self.completed_at_us = Some(now_us);
        self.result = result;
        self.final_confidence = confidence;
        // Any step never completed is recorded as CANCELLED during
        // finalization (RP-L3 / RP-09).
        for s in &mut self.steps {
            if s.ended_at_us == 0 {
                s.status = StepStatus::Cancelled;
                s.ended_at_us = now_us;
            }
        }
    }

    /// Deterministic JSON serialization of the full plan.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Round-trip reconstruction (used by tests and debugging tooling).
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

/// Summaries must never contain raw content or potential secrets; cap them
/// at a bounded length.
fn truncate_summary(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        return s.to_owned();
    }
    let mut cut = MAX;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::contract_for;
    use crate::mode::AnswerModePolicy;
    use crate::query::{ClassificationConfidence, classify};

    fn sample_plan(query: &str) -> RetrievalPlan {
        let cls = classify(query).unwrap();
        let contract = contract_for(cls.query_type);
        RetrievalPlan::create(
            uuid::Uuid::new_v4().to_string(),
            1_000,
            query.to_owned(),
            &cls,
            "workspace-hash".into(),
            &contract,
            AnswerModePolicy::for_mode(crate::mode::AnswerMode::Normal),
        )
    }

    #[test]
    fn plan_round_trips_through_json_without_loss() {
        let mut p = sample_plan("Where is Router defined?");
        let s0 = p.begin_step(
            SubsystemTag::FtsSearch,
            "fts5_search",
            "terms=router",
            1_100,
        );
        p.complete_step(s0, StepStatus::Completed, "5 hits", 0, 5, 1_150);
        p.finalize(PlanResult::Success, ConfidenceLevel::High, 1_200);
        let json = p.to_json().unwrap();
        let back = RetrievalPlan::from_json(&json).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn double_finalization_panics() {
        let mut p = sample_plan("callers of process_payment");
        p.finalize(PlanResult::Success, ConfidenceLevel::Medium, 2);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            p.finalize(PlanResult::Success, ConfidenceLevel::High, 3);
        }));
        assert!(r.is_err());
    }

    #[test]
    fn unfinished_steps_become_cancelled_on_finalize() {
        let mut p = sample_plan("what port is configured");
        let _ = p.begin_step(SubsystemTag::GraphWalk, "graph_walk", "seed=x", 10);
        p.finalize(PlanResult::InsufficientEvidence, ConfidenceLevel::None_, 20);
        assert_eq!(p.steps[0].status, StepStatus::Cancelled);
        assert_eq!(p.steps[0].ended_at_us, 20);
    }

    #[test]
    fn summaries_are_capped() {
        let long = "x".repeat(5_000);
        let mut p = sample_plan("status endpoint");
        let i = p.begin_step(SubsystemTag::SymbolLookup, "op", &long, 1);
        assert!(p.steps[i].input_summary.len() <= 210);
    }

    #[test]
    fn classifier_confidence_strings_stable() {
        assert_eq!(ClassificationConfidence::High.as_str(), "HIGH");
    }
}
