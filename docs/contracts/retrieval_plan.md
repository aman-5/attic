# Retrieval Plan Contract

**Contract ID:** C13  
**Version:** 1.0.0  
**Status:** DRAFT  
**Depends on:** C09 (evidence), C11 (query_evidence), C12 (answer_modes)

---

## 1. Purpose

A `RetrievalPlan` is a **serializable, loggable description of every decision** made to answer a query. It is created at query intake, mutated in-place as the pipeline executes, and persisted to the ops log on completion. It is the primary observability artifact for the retrieval subsystem.

Key design decisions:
- The plan is the **single source of truth** for what happened during a query — not logs, not metrics.
- Every sub-system that participates in answering a query MUST write a `PlanStep` into the plan.
- The plan is immutable once the query is complete; the final plan is written atomically to `ops_retrieval_log`.
- Plans are retained for at least 7 days for debugging; retention policy is configurable.
- `RetrievalPlan` is NOT a query scheduler or task graph — it is a post-hoc trace structure that is filled in as execution proceeds.

---

## 2. RetrievalPlan Struct

```
RetrievalPlan {
    // Identity
    plan_id:           Uuid,              // stable identifier for this plan instance
    query_id:          Uuid,              // correlates with the inbound MCP tool call
    created_at_us:     i64,              // microseconds since Unix epoch (UTC)
    completed_at_us:   Option<i64>,      // None if plan is still active

    // Input snapshot
    raw_query:         String,            // original query text, verbatim
    query_type:        QueryType,         // classified type (from query_evidence.md)
    workspace_id:      String,            // workspace root path hash (not the path itself)
    source_revision:   WorkspaceSnapshot, // snapshot at time of query (from source_revision.md)

    // Policy
    policy:            AnswerModePolicy,  // immutable once set (from answer_modes.md)

    // Execution trace
    steps:             Vec<PlanStep>,     // ordered list of steps taken
    evidence_used:     Vec<EvidenceRef>,  // references to evidence items that made it to context
    evidence_dropped:  Vec<DroppedEvidence>, // evidence considered but excluded

    // Result
    result:            PlanResult,
    final_confidence:  ConfidenceLevel,
    context_tokens:    u32,              // actual tokens in assembled context
    repair_cycles:     u8,               // number of repair cycles executed
    policy_trace:      PolicyExecutionTrace, // from answer_modes.md §7
}
```

### 2.1 PlanStep

```
PlanStep {
    step_id:       u16,           // sequential within this plan (0-based)
    subsystem:     SubsystemTag,  // which subsystem emitted this step
    operation:     String,        // human-readable operation name (e.g., "fts5_search")
    started_at_us: i64,
    ended_at_us:   i64,
    status:        StepStatus,
    input_summary: String,        // compact description of inputs (no raw content)
    output_summary: String,       // compact description of outputs (no raw content)
    candidates_in:  u32,          // retrieval units entering this step
    candidates_out: u32,          // retrieval units exiting this step
    detail:        Option<serde_json::Value>, // structured detail; subsystem-specific
}
```

### 2.2 SubsystemTag

```
enum SubsystemTag {
    QUERY_CLASSIFIER,
    FTS5_SEARCH,
    SYMBOL_LOOKUP,
    GRAPH_WALK,
    SEMANTIC_SEARCH,
    RERANKER,
    SECRET_FILTER,
    EVIDENCE_ASSEMBLER,
    SOURCE_VERIFIER,
    CONTEXT_TRIMMER,
    REPAIR_EXPANDER,
    POLICY_ENFORCER,
}
```

### 2.3 StepStatus

```
enum StepStatus {
    COMPLETED,         // completed normally
    DEGRADED,          // completed but with reduced output due to budget limit
    SKIPPED,           // skipped because policy disallows (e.g., semantic in FAST mode)
    CANCELLED,         // cancelled by deadline or upstream cancellation
    FAILED,            // internal error; query can still proceed from other evidence
}
```

### 2.4 EvidenceRef

```
EvidenceRef {
    evidence_id:    Uuid,           // references Evidence.id
    source_type:    SourceType,     // from evidence.md
    rank:           u16,            // final rank in assembled context (0 = highest)
    score:          f32,            // composite relevance score [0.0, 1.0]
    token_count:    u32,            // tokens contributed to context
}
```

### 2.5 DroppedEvidence

```
DroppedEvidence {
    evidence_id:    Uuid,
    source_type:    SourceType,
    drop_reason:    DropReason,
    score:          f32,
}

enum DropReason {
    BELOW_SCORE_THRESHOLD,
    CONTEXT_TOKEN_LIMIT,
    SECRET_CONTENT_DETECTED,
    STALE_BEYOND_THRESHOLD,
    POLICY_BLOCKED_SOURCE_TYPE,
    DUPLICATE_CONTENT,
    CANDIDATES_LIMIT_REACHED,
}
```

### 2.6 PlanResult

```
enum PlanResult {
    SUCCESS,                     // evidence contract satisfied; answer produced
    PARTIAL_SUCCESS,             // evidence contract partially satisfied; low-confidence answer
    INSUFFICIENT_EVIDENCE,       // evidence contract not satisfied after repair cycles
    POLICY_HARD_CANCELLED,       // cancelled by budget enforcement
    QUERY_TYPE_UNSUPPORTED,      // query type not handled in V1
    INTERNAL_ERROR { message: String }, // unexpected subsystem failure
}
```

### 2.7 ConfidenceLevel

```
enum ConfidenceLevel {
    HIGH,     // all required evidence present and verified
    MEDIUM,   // required evidence present; some preferred evidence absent
    LOW,      // minimum evidence threshold met; significant gaps
    NONE,     // INSUFFICIENT_EVIDENCE or error condition
}
```

---

## 3. Plan Lifecycle

```
┌─────────────────────────────────────────────────────────────┐
│                     Plan Lifecycle                          │
│                                                             │
│  INTAKE ──► CLASSIFYING ──► PLANNING ──► EXECUTING          │
│                                              │               │
│                                         (steps written)     │
│                                              │               │
│                                          ASSEMBLING          │
│                                              │               │
│                                    ┌─────────┴──────────┐   │
│                                    │                    │   │
│                                 COMPLETE            REPAIR   │
│                                    │                    │   │
│                                    └─────────┬──────────┘   │
│                                              │               │
│                                          PERSISTED           │
└─────────────────────────────────────────────────────────────┘
```

### 3.1 Plan Creation

```
RULE RP-L1:
  A RetrievalPlan MUST be created before any subsystem is invoked.
  plan_id and query_id MUST be assigned atomically at creation time.
  created_at_us MUST be captured before any I/O.
```

### 3.2 Step Recording

```
RULE RP-L2:
  Each subsystem MUST call plan.add_step() BEFORE starting its work,
  recording started_at_us at that point.
  Each subsystem MUST call plan.complete_step() AFTER finishing,
  recording ended_at_us and final status.
  A step that was never completed (e.g., due to panic) is recorded
  as CANCELLED during plan finalization.
```

### 3.3 Plan Finalization

```
RULE RP-L3:
  Plan finalization occurs exactly once, when the query pipeline
  returns to the server handler.
  Finalization MUST:
    1. Set completed_at_us
    2. Compute final_confidence from evidence_used
    3. Write policy_trace
    4. Persist the complete plan to ops_retrieval_log via a single
       INSERT (no partial writes)
    5. Only THEN return the answer to the MCP caller
```

### 3.4 Persistence Schema

Plans are persisted to the `ops_retrieval_log` table defined in `storage.md`:

```sql
-- Logical record (exact DDL in migrations/0001_initial.sql)
ops_retrieval_log (
    plan_id         TEXT PRIMARY KEY,   -- UUID string
    query_id        TEXT NOT NULL,
    created_at_us   INTEGER NOT NULL,
    completed_at_us INTEGER,
    workspace_id    TEXT NOT NULL,
    query_type      TEXT NOT NULL,
    result          TEXT NOT NULL,
    confidence      TEXT NOT NULL,
    policy_mode     TEXT NOT NULL,
    context_tokens  INTEGER NOT NULL,
    repair_cycles   INTEGER NOT NULL,
    plan_json       TEXT NOT NULL       -- full RetrievalPlan as JSON
)
```

`plan_json` stores the complete serialized `RetrievalPlan`. It is the authoritative record; the other columns are projections for efficient querying.

---

## 4. Content Safety in Plans

```
RULE RP-S1:
  The `input_summary` and `output_summary` fields of PlanStep MUST NOT
  contain raw file content, symbol source code, or any string that
  could be a secret value.

RULE RP-S2:
  The `raw_query` field stores the verbatim query text. If the query
  itself contains a suspected secret pattern (detected by the secret
  scanner), the field MUST be replaced with:
    "<REDACTED: suspected_secret_in_query>"
  and a warning MUST be emitted.

RULE RP-S3:
  EvidenceRef and DroppedEvidence MUST reference evidence by ID only.
  No content excerpts are stored in the plan.
```

---

## 5. Plan Querying

Plans stored in `ops_retrieval_log` support the following access patterns:

| Pattern | SQL hint |
|---------|----------|
| Lookup by plan_id | `WHERE plan_id = ?` |
| All plans for a workspace | `WHERE workspace_id = ? ORDER BY created_at_us DESC` |
| Slow queries (> N ms) | `WHERE (completed_at_us - created_at_us) > ? * 1000` |
| Queries by result type | `WHERE result = 'INSUFFICIENT_EVIDENCE'` |
| Plans using DEEP mode | `WHERE policy_mode = 'DEEP'` |

---

## 6. Serialization Requirements

```
RULE RP-SR1:
  All RetrievalPlan fields MUST serialize to JSON deterministically.
  Floating-point fields (score) use 6 decimal places of precision.
  UUID fields serialize as lowercase hyphenated strings.
  Timestamp fields serialize as i64 microseconds.
  Enum fields serialize as SCREAMING_SNAKE_CASE strings.

RULE RP-SR2:
  plan_json MUST round-trip: deserialize(serialize(plan)) == plan
  for all valid plan states.

RULE RP-SR3:
  A plan with status INTERNAL_ERROR MUST still serialize successfully.
  The error message MUST be included verbatim (no truncation in the
  stored record; truncate only in log output to 512 chars).
```

---

## 7. Invariants

| ID      | Invariant |
|---------|-----------|
| RP-INV-1 | A `RetrievalPlan` MUST have exactly one finalization call. Double-finalization is a programming error. |
| RP-INV-2 | `steps` are append-only; no step may be removed or reordered after being added. |
| RP-INV-3 | `policy` is set at plan creation and MUST NOT be mutated during execution. |
| RP-INV-4 | `evidence_used` and `evidence_dropped` together account for ALL evidence considered. No evidence is silently ignored. |
| RP-INV-5 | `plan_id` is globally unique. Two plans for the same query (e.g., after a server restart) have distinct `plan_id` values. |
| RP-INV-6 | A plan that was not persisted due to storage failure MUST cause a `PlanPersistenceFailure` error in the server log, but MUST NOT prevent the answer from being returned to the caller. |
| RP-INV-7 | The sum of `token_count` over all `EvidenceRef` items MUST equal `context_tokens`. |

---

## 8. Test Matrix

| ID      | Scenario | Expected Outcome |
|---------|----------|-----------------|
| RP-01   | Query completes normally with 3 evidence items | Plan has 3 `EvidenceRef`, `result = SUCCESS`, `completed_at_us` is set |
| RP-02   | Query cancelled by deadline mid-graph-walk | Final step has `status = CANCELLED`; `result = POLICY_HARD_CANCELLED` |
| RP-03   | Evidence item contains suspected secret | Item appears in `evidence_dropped` with `drop_reason = SECRET_CONTENT_DETECTED` |
| RP-04   | Query text contains suspected secret | `raw_query` replaced with redaction marker; warning emitted |
| RP-05   | Plan serialized to JSON and deserialized | Full round-trip equality; no precision loss on `score` field |
| RP-06   | Plan persistence fails (disk full) | Answer still returned; `PlanPersistenceFailure` logged; no crash |
| RP-07   | Token count in evidence_used does not sum to context_tokens | Invariant violation detected; plan marked INTERNAL_ERROR |
| RP-08   | Two repair cycles executed for DEEP query | `repair_cycles = 2`, two REPAIR_EXPANDER steps recorded |
| RP-09   | Subsystem panics mid-execution | Incomplete step recorded as CANCELLED; plan finalized with INTERNAL_ERROR |
| RP-10   | Plans queried by result = INSUFFICIENT_EVIDENCE for a workspace | Returns only plans for that workspace with matching result column |

---

## 9. Open Questions

| ID     | Question | Impact |
|--------|----------|--------|
| RP-Q1  | Should `plan_json` be compressed (zstd) before storage given potentially large plans? | Deferred; V1 stores uncompressed JSON |
| RP-Q2  | Should a streaming plan variant exist for long DEEP queries? | Deferred to post-V1 |
| RP-Q3  | What is the maximum retention period for plans before they must be pruned? | 7-day default; configurable at startup |
