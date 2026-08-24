# Answer Mode Policy Contract

**Contract ID:** C12  
**Version:** 1.0.0  
**Status:** DRAFT  
**Depends on:** C01 (source_revision), C09 (evidence), C11 (query_evidence)

---

## 1. Purpose

`AnswerModePolicy` defines **explicit, enforceable resource budgets** for the three operating modes: `FAST`, `NORMAL`, and `DEEP`. These are not informal hints passed between subsystems — they are opaque, serializable policy objects that every subsystem receiving a query **must** respect.

Key design decisions:
- Budgets are **compile-time defaults** overridable at startup via configuration; they are **not** per-request user choices in V1.
- The policy object travels with the `RetrievalPlan` and is logged for every query.
- Exceeding a budget is a **hard cancellation**, not a soft warning.
- `FAST` mode **never** performs semantic search, re-ranking, or source verification beyond checksum.

---

## 2. AnswerModePolicy Struct

```
AnswerModePolicy {
    mode:                      AnswerMode,        // which policy tier
    max_time_ms:               u64,               // wall-clock deadline for entire pipeline
    max_candidates:            u32,               // max retrieval units considered before ranking
    max_graph_depth:           u8,                // max edge hops in dependency graph traversal
    max_graph_nodes:           u32,               // max nodes visited in any graph walk
    max_fs_files:              u32,               // max filesystem files read end-to-end
    max_fs_bytes:              u64,               // max total bytes read from filesystem
    max_context_tokens:        u32,               // max tokens assembled for context window
    semantic_allowed:          bool,              // whether semantic/vector search is permitted
    reranking_allowed:         bool,              // whether re-ranking pass is permitted
    source_verification_level: VerificationLevel, // how deeply to verify evidence freshness
    repair_attempts:           u8,               // max automatic recovery cycles if result is INSUFFICIENT_EVIDENCE
}
```

### 2.1 AnswerMode Enum

```
enum AnswerMode {
    FAST,    // sub-second, index-only, no semantic search
    NORMAL,  // default; balanced latency/quality
    DEEP,    // highest quality; no time pressure assumed
}
```

### 2.2 VerificationLevel Enum

```
enum VerificationLevel {
    NONE,       // trust stored hash; no filesystem stat
    CHECKSUM,   // re-read file from disk and compare BLAKE3 hash
    FULL,       // re-parse content and verify all metadata fields
}
```

---

## 3. V1 Default Policy Table

All durations are in milliseconds. All byte counts are in bytes.

| Field                    | FAST           | NORMAL         | DEEP            |
|--------------------------|----------------|----------------|-----------------|
| `max_time_ms`            | 300            | 3 000          | 30 000          |
| `max_candidates`         | 50             | 500            | 5 000           |
| `max_graph_depth`        | 1              | 3              | 8               |
| `max_graph_nodes`        | 25             | 200            | 2 000           |
| `max_fs_files`           | 0              | 20             | 200             |
| `max_fs_bytes`           | 0              | 4 194 304      | 104 857 600     |
| `max_context_tokens`     | 4 096          | 16 384         | 65 536          |
| `semantic_allowed`       | false          | true           | true            |
| `reranking_allowed`      | false          | true           | true            |
| `source_verification_level` | NONE        | CHECKSUM       | FULL            |
| `repair_attempts`        | 0              | 1              | 3               |

> **Note:** `max_fs_files = 0` and `max_fs_bytes = 0` in `FAST` mode means the pipeline MUST NOT perform any live filesystem read. All evidence must come from the index.

---

## 4. Policy Enforcement Rules

### 4.1 Hard Deadline Enforcement

```
RULE AM-E1:
  IF elapsed_time >= max_time_ms THEN
    cancel all in-flight futures immediately
    return AnswerModeViolation { field: "max_time_ms", budget: max_time_ms, consumed: elapsed }
  END
```

Enforcement is **per-query**, measured from the moment the `RetrievalPlan` is activated.

### 4.2 Candidate Budget

```
RULE AM-E2:
  Candidate count is incremented every time a retrieval unit enters the ranking pool.
  IF candidate_count > max_candidates THEN
    stop adding candidates; rank only what has been collected so far
  END
```

This is a soft stop on the retrieval side — the query still produces an answer from the collected candidates.

### 4.3 Graph Budget

```
RULE AM-E3:
  Graph traversal MUST stop when EITHER max_graph_depth OR max_graph_nodes is exceeded.
  Nodes visited at the boundary are EXCLUDED (not partially expanded).
```

### 4.4 Filesystem Budget

```
RULE AM-E4:
  IF semantic_allowed = false THEN
    ANY attempt to read a filesystem path returns an immediate PolicyViolation
  END

  IF fs_files_read > max_fs_files OR fs_bytes_read > max_fs_bytes THEN
    stop reading; mark remaining evidence as NOT_VERIFIED
  END
```

### 4.5 Context Token Budget

```
RULE AM-E5:
  Context assembly MUST truncate output at max_context_tokens.
  Truncation strategy: drop lowest-scored evidence items first.
  The truncation boundary MUST be recorded in the RetrievalPlan trace.
```

### 4.6 Semantic and Re-ranking Gates

```
RULE AM-E6:
  IF semantic_allowed = false THEN
    the pipeline MUST NOT invoke any embedding model or vector similarity search.
    Violation is a hard error, not a silent skip.
  END

RULE AM-E7:
  IF reranking_allowed = false THEN
    the pipeline MUST NOT invoke any re-ranking model or cross-encoder.
  END
```

### 4.7 Repair Cycle Cap

```
RULE AM-E8:
  IF result = INSUFFICIENT_EVIDENCE AND repair_attempts_used < repair_attempts THEN
    emit QueryRepairAttempt event
    expand evidence window by one scope level (file → module → crate → workspace)
    re-run retrieval within remaining time budget
    repair_attempts_used += 1
  ELSE
    return INSUFFICIENT_EVIDENCE to caller — do not further expand
  END
```

---

## 5. Policy Construction

### 5.1 Default Construction

The system MUST construct a default `AnswerModePolicy` for every inbound query that does not specify one. The default mode is `NORMAL`.

### 5.2 Override Hierarchy

```
1. Compile-time defaults (V1 table above)
2. Startup configuration file (overrides compile-time defaults; all fields optional)
3. Per-query override (NOT supported in V1; reserved for future)
```

Any startup configuration that sets `max_time_ms < 50` MUST be rejected with a configuration error at startup.

### 5.3 Serialization

`AnswerModePolicy` MUST be fully serializable to JSON (for the plan log) and MUST round-trip without precision loss. Boolean fields serialize as JSON `true`/`false`. Enum fields serialize as uppercase string tags.

---

## 6. Interaction with QueryEvidenceContract

```
RULE AM-I1:
  FAST mode is INCOMPATIBLE with query types that require semantic evidence
  (ARCHITECTURE_EXPLANATION, KNOWLEDGE_QUESTION, IMPACT_ANALYSIS in multi-repo scope).
  The pipeline MUST emit IncompatiblePolicyForQuery warning and degrade to:
    - return available indexed evidence marked as POTENTIALLY_INCOMPLETE
    - set answer confidence = LOW
```

```
RULE AM-I2:
  For DEEP mode with repair_attempts = 3, each repair cycle that expands scope
  MUST NOT exceed max_fs_bytes / repair_attempts bytes per cycle.
```

---

## 7. Observability Requirements

Every `AnswerModePolicy` execution MUST emit a `PolicyExecutionTrace` containing:

```
PolicyExecutionTrace {
    mode:                AnswerMode,
    query_id:            Uuid,
    time_elapsed_ms:     u64,
    candidates_examined: u32,
    graph_nodes_visited: u32,
    fs_files_read:       u32,
    fs_bytes_read:       u64,
    context_tokens_used: u32,
    semantic_invoked:    bool,
    reranking_invoked:   bool,
    repair_cycles:       u8,
    budget_fields_hit:   Vec<String>,   // which limits were reached
    final_result:        PolicyResult,
}

enum PolicyResult {
    COMPLETED_WITHIN_BUDGET,
    DEGRADED_BY_TIME,
    DEGRADED_BY_CANDIDATES,
    DEGRADED_BY_FS_BUDGET,
    DEGRADED_BY_TOKENS,
    INSUFFICIENT_EVIDENCE,
    HARD_CANCELLED,
}
```

---

## 8. Invariants

| ID     | Invariant |
|--------|-----------|
| AM-INV-1 | `AnswerModePolicy` is immutable once attached to a `RetrievalPlan`. |
| AM-INV-2 | `FAST` mode MUST NEVER trigger a filesystem read or embedding lookup. Any code path that does so is a contract violation. |
| AM-INV-3 | A `repair_attempts` value > 0 in `FAST` mode is a configuration error. `FAST` mode MUST always have `repair_attempts = 0`. |
| AM-INV-4 | `max_time_ms` MUST be > 0. |
| AM-INV-5 | `max_context_tokens` MUST be ≤ 131 072 in V1 (128K hard ceiling). |
| AM-INV-6 | The `PolicyExecutionTrace` MUST be written before the answer is returned to the caller. |
| AM-INV-7 | Cancellation due to budget exhaustion MUST propagate to all child tasks — no orphaned background work. |

---

## 9. Test Matrix

| ID      | Scenario | Expected Outcome |
|---------|----------|-----------------|
| AM-01   | `FAST` mode query triggers semantic search code path | Hard error: `PolicyViolation { field: "semantic_allowed" }` |
| AM-02   | `FAST` mode query attempts filesystem read | Hard error: `PolicyViolation { field: "max_fs_files" }` |
| AM-03   | `NORMAL` query exceeds `max_time_ms` mid-graph-walk | All futures cancelled; `DEGRADED_BY_TIME` result |
| AM-04   | `NORMAL` query hits `max_candidates` limit | Ranking runs on collected set; result marked `DEGRADED_BY_CANDIDATES` |
| AM-05   | `DEEP` query produces `INSUFFICIENT_EVIDENCE`; `repair_attempts = 3` | Up to 3 repair cycles; each expands scope; final return if still insufficient |
| AM-06   | Startup config sets `max_time_ms = 30` | Configuration error at startup; process does not start |
| AM-07   | `PolicyExecutionTrace` emitted for query that completed within all budgets | Trace has `budget_fields_hit = []`, `final_result = COMPLETED_WITHIN_BUDGET` |
| AM-08   | `DEEP` query hits `max_context_tokens`; lowest-scored evidence dropped | Truncation recorded in trace; context fits within limit |
| AM-09   | `FAST` mode configured with `repair_attempts = 1` | Configuration error at startup |
| AM-10   | `AnswerModePolicy` serialized to JSON and deserialized | Round-trips without any field loss or type coercion |

---

## 10. Open Questions

| ID     | Question | Impact |
|--------|----------|--------|
| AM-Q1  | Should per-tool (MCP tool) mode overrides be supported in V1 or deferred? | Deferred to post-V1 per current plan |
| AM-Q2  | Is a `STREAMING` answer mode needed for long `DEEP` queries? | Deferred; not in V1 scope |
| AM-Q3  | What is the correct `max_context_tokens` ceiling if the target LLM changes? | Configuration parameter; V1 ceiling is 128K |
