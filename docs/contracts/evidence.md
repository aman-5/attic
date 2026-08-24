# Contract: Canonical Evidence Object

## Purpose

Define the canonical `Evidence` object, its required fields, source types,
authority semantics, freshness, verification states, and how all retrievers
produce evidence candidates that flow through ranking, validation, and the
Evidence Manager before entering context.

---

## Definitions

### Evidence

A first-class canonical object that represents a piece of information that
can support an answer claim. All retrievers ultimately produce Evidence
candidates.

```
Evidence {
  id                      : Uuid

  repository_id           : Uuid          -- foreign key → Repository
  source_type             : SourceType
  source_id               : String        -- file_occurrence_id or knowledge_item_id
  path                    : String        -- normalized repo-relative path

  source_revision_id      : Uuid          -- foreign key → SourceRevision
  index_generation_id     : Uuid          -- foreign key → IndexGeneration
  source_span             : Option<String>  -- "start_line:start_col-end_line:end_col"
  content_hash            : String        -- BLAKE3 hex of the evidenced content

  freshness_state         : FreshnessState
  authority               : AuthorityLevel
  confidence              : f64           -- 0.0–1.0; retrieval confidence
  relationship_confidence : Option<f64>   -- set when via graph traversal

  retrieval_sources       : Vec<RetrievalSource>   -- which retrievers found this
  ranking_signals         : RankingSignals

  verification_state      : VerificationState
}
```

### SourceType

```
SOURCE_CODE       -- implementation source file
TEST              -- test file (behavioral expectation)
CONFIGURATION     -- config file (configured behavior)
DOCUMENTATION     -- doc file (non-knowledge)
KNOWLEDGE         -- knowledge/*.md (project documentation)
RELATIONSHIP      -- derived from graph traversal
GENERATED_SOURCE  -- auto-generated source file
```

### AuthorityLevel

Represents how authoritative this source type is for the claim being evaluated.

```
IMPLEMENTATION    -- source code; highest for behavioral/correctness claims
TEST_EXPECTATION  -- test; authoritative for expected behavior
CONFIGURED        -- configuration; authoritative for configured behavior
PROJECT_KNOWLEDGE -- knowledge docs; authoritative for documented intent
DOCUMENTATION     -- general docs; medium authority
DERIVED           -- relationship/graph traversal; confidence-dependent
```

Authority is not a total order across all query types. A configuration
file has higher authority than source code for the question "what port
does the server listen on?" but lower authority for "how does retry logic
work?".

### FreshnessState

```
CURRENT           -- evidence matches current source revision
STALE             -- evidence from prior revision; source may have changed
UNKNOWN           -- freshness cannot be determined
INVALID           -- evidence must not be served
PENDING_REFRESH   -- recomputation in progress
```

### VerificationState

```
UNVERIFIED        -- not yet checked against current source
VERIFIED          -- confirmed against current source revision
STALE             -- source revision changed since last verification
CONTRADICTED      -- other evidence contradicts this
```

### RetrievalSource

Records which retrieval mechanism produced this evidence candidate.

```
RetrievalSource {
  retriever_type : String   -- FTS | SYMBOL | STRUCTURAL | KNOWLEDGE | GRAPH | SEMANTIC
  score          : f64      -- raw retrieval score from that retriever
  query_fragment : String   -- the query or subquery that produced this result
}
```

### RankingSignals

Observable per-dimension ranking signals. A combined score may exist
operationally but component signals remain observable.

```
RankingSignals {
  lexical_score           : Option<f64>
  symbol_match_score      : Option<f64>
  query_intent_match      : Option<f64>
  repository_relevance    : Option<f64>
  freshness_score         : Option<f64>  -- 1.0 = CURRENT; 0.0 = INVALID
  structural_proximity    : Option<f64>
  relationship_confidence : Option<f64>
  knowledge_authority     : Option<f64>
  test_relevance          : Option<f64>
  semantic_score          : Option<f64>
  combined_score          : Option<f64>  -- derived; not the only signal used
}
```

---

## Evidence Pipeline

All retrievers produce evidence candidates. Candidates flow through:

```
Retrievers (FTS, symbol, structural, knowledge, graph, semantic)
    |
    v
Evidence Candidates
    |
    v
Evidence Ranking (per RankingSignals)
    |
    v
Evidence Validation (freshness, authority, provenance, contract satisfaction)
    |
    v
Evidence Manager
    |
    +-- sufficient → Context Builder
    |
    +-- insufficient → Targeted Expansion → back to Evidence Manager
    |
    +-- still insufficient → INSUFFICIENT_EVIDENCE
```

---

## Provenance Requirements

Every `Evidence` record MUST carry:

```
repository_id         -- which repository
path                  -- which file
content_hash          -- exact content state
source_revision_id    -- which source revision produced this
index_generation_id   -- which index generation derived this
source_span           -- optional; exact lines/bytes if available
```

An evidence record without a valid `source_revision_id` is treated as
`verification_state = STALE` and must be re-verified before use.

---

## Freshness Rules

| freshness_state | Serving behavior |
|-----------------|-----------------|
| CURRENT | Serve normally |
| STALE | Serve with staleness caveat; trigger background refresh |
| UNKNOWN | Serve with unknown-freshness caveat; schedule verification |
| INVALID | Never serve; trigger recomputation |
| PENDING_REFRESH | May serve prior STALE version with caveat; or hold |

Evidence with `freshness_state = INVALID` is filtered out before ranking.

Evidence with `freshness_state = STALE` or `UNKNOWN` may be included in
ranking but MUST carry the staleness flag in the context presented to the
LLM.

---

## Authority Semantics

Authority guides evidence selection when multiple pieces of evidence
address the same claim with conflicting information.

Rules:
1. Higher authority does not override conflicting evidence; it informs
   the Evidence Manager's contradiction detection.
2. The query type determines which authority is relevant (see
   query_evidence contract).
3. `DERIVED` authority (from graph traversal) always carries
   `relationship_confidence` and is never treated as `IMPLEMENTATION`.
4. Knowledge authority (`PROJECT_KNOWLEDGE`) for claims about architecture
   or intent may outweigh source code if the knowledge item is recent and
   verified.

---

## Contradiction Detection

The Evidence Manager checks for contradictions:

```
For each claim type in the query:
  Collect evidence for that claim type.
  If two CURRENT evidence items from the same source_type make
  incompatible claims → record CONTRADICTED state on both.
  If KNOWLEDGE item contradicts SOURCE_CODE → surface both; do not suppress.
  If TEST item contradicts SOURCE_CODE → surface both; flag as behavioral mismatch.
```

Contradictions are surfaced to the Context Builder and included in the
answer context as explicit contradictions, not silently resolved.

---

## Insufficient Evidence State

The Evidence Manager tracks whether the `QueryEvidenceContract` is satisfied:

```
For each required_evidence type in the contract:
  If no CURRENT or STALE evidence of that type exists in the candidate set:
    → insufficient
```

On insufficiency:
1. Targeted expansion (broader FTS, bounded graph traversal, source verification).
2. Re-evaluate contract satisfaction.
3. If still insufficient after expansion budget exhausted:
   → Return `INSUFFICIENT_EVIDENCE` state.
   → Never force a confident answer from weak evidence.

---

## Invariants

1. Every `Evidence` record has a non-null `source_revision_id`.
2. `content_hash` in an Evidence record matches the content at the
   referenced `source_span` in the referenced `source_revision_id`.
   This is verified during evidence validation.
3. Evidence with `freshness_state = INVALID` is never included in the
   context presented to the LLM.
4. `ranking_signals` is never empty for served evidence;
   at minimum `combined_score` must be set.
5. Secret bytes never appear in any `Evidence` field. Evidence derived from
   `SECRET_REDACTED` files is not created.
6. `CONTRADICTED` evidence is surfaced, not silently dropped.
7. `relationship_confidence` is set and < 1.0 whenever `source_type = RELATIONSHIP`.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Evidence record has no valid source_revision_id | Treated as INVALID; not served |
| content_hash mismatch during verification | verification_state → STALE; trigger re-verification |
| Contradiction detection errors | Log diagnostic; surface evidence without contradiction flag |
| Evidence Manager budget exhausted | Return INSUFFICIENT_EVIDENCE; never force answer |
| All candidates filtered as INVALID/STALE | Return INSUFFICIENT_EVIDENCE |

---

## Observability

Per-query evidence log:

```
query_id
candidates_generated_total
candidates_ranked
candidates_validated
candidates_rejected_freshness
candidates_rejected_authority
candidates_contradicted
evidence_selected_count
evidence_insufficient: bool
expansion_triggered: bool
expansion_rounds
```

---

## Examples

### Source code evidence

```
Evidence {
  id: "ev-001"
  repository_id: "repo-a"
  source_type: SOURCE_CODE
  source_id: "fo-123"   -- FooService.java occurrence
  path: "src/main/java/com/example/FooService.java"
  source_span: "45:1-67:1"
  content_hash: "abc123..."
  source_revision_id: "sr-456"
  index_generation_id: "ig-789"
  freshness_state: CURRENT
  authority: IMPLEMENTATION
  confidence: 0.92
  ranking_signals: {
    lexical_score: 0.91
    symbol_match_score: 1.00
    freshness_score: 1.00
    combined_score: 0.97
  }
  verification_state: VERIFIED
}
```

### Knowledge evidence (project doc)

```
Evidence {
  id: "ev-002"
  source_type: KNOWLEDGE
  path: "knowledge/architecture.md"
  source_span: "12:1-25:1"
  authority: PROJECT_KNOWLEDGE
  confidence: 0.85
  verification_state: UNVERIFIED
  freshness_state: CURRENT
}
```

### Graph relationship evidence

```
Evidence {
  id: "ev-003"
  source_type: RELATIONSHIP
  authority: DERIVED
  confidence: 0.74
  relationship_confidence: 0.74
  ranking_signals: {
    relationship_confidence: 0.74
    structural_proximity: 0.60
    combined_score: 0.68
  }
  verification_state: UNVERIFIED
}
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| EV-01 | Evidence with valid source_revision_id | Served normally |
| EV-02 | Evidence with no source_revision_id | Treated as INVALID; not served |
| EV-03 | INVALID freshness evidence | Filtered before ranking |
| EV-04 | STALE evidence included | Served with staleness caveat in context |
| EV-05 | Two CURRENT items contradict each other | Both marked CONTRADICTED; both surfaced |
| EV-06 | RELATIONSHIP evidence | relationship_confidence set; < 1.0 |
| EV-07 | No evidence satisfies contract | INSUFFICIENT_EVIDENCE returned |
| EV-08 | Expansion round finds evidence | Evidence added; contract re-evaluated |
| EV-09 | SECRET_REDACTED source | No evidence record created |
| EV-10 | content_hash mismatch on verification | verification_state → STALE |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| EV-Q1 | Should `retrieval_sources` be persisted in `core_evidence`, or only kept in memory during the query pipeline? | No — in-memory during query; not persisted for V1 |
| EV-Q2 | Should `ranking_signals` be stored in the DB or recomputed on each query? | No — recomputed at query time for V1; DB storage optional for analytics |
| EV-Q3 | Should evidence from GENERATED_SOURCE files have reduced authority by default? | No — same as SOURCE_CODE authority; discovery_class handles priority |
