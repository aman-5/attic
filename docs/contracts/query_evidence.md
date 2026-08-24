# Contract: Query Taxonomy and Evidence Sufficiency

## Purpose

Define the V1 query taxonomy (query types the system recognizes), and for
each query type specify which evidence is required, which is preferred, what
freshness is needed, and what fallback expansion is allowed. This contract
drives the Query Router and Evidence Manager decisions.

---

## Definitions

### QueryType

The system's classification of a query's intent.

```
DEFINITION_LOOKUP       -- "Where is X defined?"
SYMBOL_NAVIGATION       -- "Show me callers/callees of X"
CONFIGURATION_LOOKUP    -- "What value is setting Y?"
ARCHITECTURE_EXPLANATION -- "How does subsystem Z work?"
DEBUGGING_ROOT_CAUSE    -- "Why does X fail / behave unexpectedly?"
IMPACT_ANALYSIS         -- "What would change if I modify X?"
CROSS_REPO_DEPENDENCY   -- "What depends on library X?"
KNOWLEDGE_QUESTION      -- "What does the project document say about X?"
TEST_BEHAVIOR           -- "What does test suite X verify?"
GENERIC_SEARCH          -- "Find code related to X" (no specific intent)
```

### QueryEvidenceContract

Per-query specification of required and preferred evidence.

```
QueryEvidenceContract {
  query_type                : QueryType
  required_evidence         : Vec<EvidenceRequirement>
  preferred_evidence        : Vec<EvidenceRequirement>
  preferred_sources         : Vec<SourceType>
  freshness_requirement     : FreshnessRequirement
  relationship_confidence_min: Option<f64>
  repository_scope          : RepositoryScope
  allowed_fallbacks         : Vec<FallbackStrategy>
  expansion_budget          : ExpansionBudget
}
```

### EvidenceRequirement

```
EvidenceRequirement {
  evidence_type : String    -- e.g., "definition", "implementation_span", "config_value"
  source_types  : Vec<SourceType>  -- acceptable source types
  min_count     : u32       -- minimum number of evidence items required
}
```

### FreshnessRequirement

```
CURRENT_ONLY    -- only CURRENT freshness accepted
CURRENT_OR_STALE -- STALE accepted with caveat
ANY             -- UNKNOWN also accepted (lower-confidence queries)
```

### RepositoryScope

```
SINGLE          -- query scoped to one repository
WORKSPACE       -- query spans all repositories
SPECIFIED       -- caller specifies repository list
```

### FallbackStrategy

```
BROADER_FTS         -- expand FTS query terms
BOUNDED_GRAPH       -- expand graph traversal by 1–2 hops
SOURCE_VERIFICATION -- read directly from authoritative source
KNOWLEDGE_LOOKUP    -- search knowledge docs if not already done
SEMANTIC_SEARCH     -- use semantic retrieval if available (Phase 5+)
```

### ExpansionBudget

```
ExpansionBudget {
  max_expansion_rounds : u32    -- default: 2
  max_extra_candidates : u32    -- default: 50
  max_extra_files      : u32    -- default: 5 (source verification)
  max_extra_bytes      : u64    -- default: 512 KB (source verification)
}
```

---

## V1 Query Evidence Contracts

### DEFINITION_LOOKUP

> "Where is FooService defined?"

```
required:
  - evidence_type: "definition"
    source_types: [SOURCE_CODE, GENERATED_SOURCE]
    min_count: 1

preferred:
  - evidence_type: "symbol_occurrence"
    source_types: [SOURCE_CODE]
  - evidence_type: "implementation_span"
    source_types: [SOURCE_CODE]

freshness_requirement: CURRENT_ONLY
repository_scope: WORKSPACE
allowed_fallbacks: [BROADER_FTS, SOURCE_VERIFICATION]
expansion_budget: { max_expansion_rounds: 2, max_extra_candidates: 20 }
```

### SYMBOL_NAVIGATION

> "Show me callers of processPayment()"

```
required:
  - evidence_type: "symbol_occurrence"
    source_types: [SOURCE_CODE]
    min_count: 1

preferred:
  - evidence_type: "call_relationship"
    source_types: [RELATIONSHIP]
  - evidence_type: "import"
    source_types: [SOURCE_CODE]

freshness_requirement: CURRENT_OR_STALE
relationship_confidence_min: 0.6
repository_scope: WORKSPACE
allowed_fallbacks: [BOUNDED_GRAPH, BROADER_FTS]
expansion_budget: { max_expansion_rounds: 2, max_extra_candidates: 30 }
```

### CONFIGURATION_LOOKUP

> "What database URL is configured for production?"

```
required:
  - evidence_type: "config_value"
    source_types: [CONFIGURATION]
    min_count: 1

preferred:
  - evidence_type: "config_schema"
    source_types: [SOURCE_CODE, DOCUMENTATION]
  - evidence_type: "knowledge_note"
    source_types: [KNOWLEDGE]

freshness_requirement: CURRENT_ONLY
repository_scope: WORKSPACE
allowed_fallbacks: [BROADER_FTS, KNOWLEDGE_LOOKUP, SOURCE_VERIFICATION]
expansion_budget: { max_expansion_rounds: 2, max_extra_candidates: 20 }
```

### ARCHITECTURE_EXPLANATION

> "How does the retry system work in the payment service?"

```
required:
  - evidence_type: "implementation"
    source_types: [SOURCE_CODE]
    min_count: 1

preferred:
  - evidence_type: "knowledge_architecture"
    source_types: [KNOWLEDGE]
  - evidence_type: "test_expectation"
    source_types: [TEST]
  - evidence_type: "configuration"
    source_types: [CONFIGURATION]
  - evidence_type: "caller_chain"
    source_types: [RELATIONSHIP]

freshness_requirement: CURRENT_OR_STALE
relationship_confidence_min: 0.5
repository_scope: WORKSPACE
allowed_fallbacks: [BOUNDED_GRAPH, KNOWLEDGE_LOOKUP, BROADER_FTS, SOURCE_VERIFICATION]
expansion_budget: { max_expansion_rounds: 3, max_extra_candidates: 50, max_extra_files: 5 }
```

### DEBUGGING_ROOT_CAUSE

> "Why does authentication fail when the token is expired?"

```
required:
  - evidence_type: "implementation"
    source_types: [SOURCE_CODE]
    min_count: 1

preferred:
  - evidence_type: "test_expectation"
    source_types: [TEST]
  - evidence_type: "error_handling_span"
    source_types: [SOURCE_CODE]
  - evidence_type: "configuration"
    source_types: [CONFIGURATION]
  - evidence_type: "knowledge_note"
    source_types: [KNOWLEDGE]
  - evidence_type: "dependency_chain"
    source_types: [RELATIONSHIP]

freshness_requirement: CURRENT_ONLY
relationship_confidence_min: 0.5
repository_scope: WORKSPACE
allowed_fallbacks: [SOURCE_VERIFICATION, BOUNDED_GRAPH, KNOWLEDGE_LOOKUP, BROADER_FTS]
expansion_budget: { max_expansion_rounds: 3, max_extra_candidates: 50, max_extra_files: 10, max_extra_bytes: 1048576 }
```

### IMPACT_ANALYSIS

> "What would break if I change the UserService interface?"

```
required:
  - evidence_type: "definition"
    source_types: [SOURCE_CODE]
    min_count: 1

preferred:
  - evidence_type: "callers"
    source_types: [RELATIONSHIP, SOURCE_CODE]
  - evidence_type: "dependents"
    source_types: [RELATIONSHIP]
  - evidence_type: "test_coverage"
    source_types: [TEST]

freshness_requirement: CURRENT_OR_STALE
relationship_confidence_min: 0.5
repository_scope: WORKSPACE
allowed_fallbacks: [BOUNDED_GRAPH, BROADER_FTS]
expansion_budget: { max_expansion_rounds: 2, max_extra_candidates: 40 }
```

### CROSS_REPO_DEPENDENCY

> "Which services depend on the payment-lib package?"

```
required:
  - evidence_type: "dependency_declaration"
    source_types: [CONFIGURATION, SOURCE_CODE, RELATIONSHIP]
    min_count: 1

preferred:
  - evidence_type: "transitive_dependency"
    source_types: [RELATIONSHIP]
  - evidence_type: "build_config"
    source_types: [CONFIGURATION]

freshness_requirement: CURRENT_OR_STALE
relationship_confidence_min: 0.6
repository_scope: WORKSPACE
allowed_fallbacks: [BOUNDED_GRAPH, BROADER_FTS]
expansion_budget: { max_expansion_rounds: 2, max_extra_candidates: 30 }
```

### KNOWLEDGE_QUESTION

> "What does the runbook say about rotating secrets?"

```
required:
  - evidence_type: "knowledge_content"
    source_types: [KNOWLEDGE, DOCUMENTATION]
    min_count: 1

preferred:
  - evidence_type: "related_source"
    source_types: [SOURCE_CODE]
  - evidence_type: "related_config"
    source_types: [CONFIGURATION]

freshness_requirement: CURRENT_OR_STALE
repository_scope: WORKSPACE
allowed_fallbacks: [KNOWLEDGE_LOOKUP, BROADER_FTS]
expansion_budget: { max_expansion_rounds: 1, max_extra_candidates: 20 }
```

### TEST_BEHAVIOR

> "What scenarios does the AuthService test suite cover?"

```
required:
  - evidence_type: "test_content"
    source_types: [TEST]
    min_count: 1

preferred:
  - evidence_type: "subject_implementation"
    source_types: [SOURCE_CODE]
  - evidence_type: "test_fixture"
    source_types: [TEST, CONFIGURATION]

freshness_requirement: CURRENT_OR_STALE
repository_scope: WORKSPACE
allowed_fallbacks: [BROADER_FTS, SOURCE_VERIFICATION]
expansion_budget: { max_expansion_rounds: 2, max_extra_candidates: 30 }
```

### GENERIC_SEARCH

> "Find code that handles webhook delivery"

```
required:
  (none — any evidence is accepted)

preferred:
  - source_types: [SOURCE_CODE, TEST, CONFIGURATION, KNOWLEDGE]

freshness_requirement: ANY
repository_scope: WORKSPACE
allowed_fallbacks: [BROADER_FTS, KNOWLEDGE_LOOKUP]
expansion_budget: { max_expansion_rounds: 1, max_extra_candidates: 50 }
```

---

## Invariants

1. Every query type has a defined `QueryEvidenceContract` in this contract.
   A query whose type is not recognized defaults to `GENERIC_SEARCH`.
2. `required_evidence` with `min_count > 0` must be satisfied before the
   Evidence Manager considers evidence sufficient.
3. `freshness_requirement = CURRENT_ONLY` causes STALE evidence to be
   filtered unless it is the only evidence available, in which case
   `INSUFFICIENT_EVIDENCE` is returned after expansion.
4. `relationship_confidence_min` is enforced: graph-derived evidence with
   lower confidence is not counted toward required evidence.
5. Expansion is bounded by `expansion_budget`; infinite expansion never
   occurs.
6. Query type classification is deterministic given the same input query.
   The classifier does not make network calls or LLM calls; it uses
   heuristic pattern matching in V1.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Query type unrecognized | Default to GENERIC_SEARCH |
| Required evidence not found after all expansion rounds | INSUFFICIENT_EVIDENCE |
| Expansion budget exceeded | Stop expansion; evaluate with current evidence |
| Freshness requirement CURRENT_ONLY but only STALE available | INSUFFICIENT_EVIDENCE after expansion |
| relationship_confidence_min not met | Relationship evidence excluded from required count |

---

## Observability

Per-query contract log:

```
query_id
query_type_detected
evidence_contract_applied
required_evidence_satisfied: bool
expansion_rounds_used
expansion_budget_remaining
insufficient_evidence_reason (if applicable)
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| QE-01 | DEFINITION_LOOKUP; symbol found in index | required evidence satisfied; no expansion |
| QE-02 | DEFINITION_LOOKUP; symbol not found | expansion triggered; if still absent: INSUFFICIENT_EVIDENCE |
| QE-03 | CONFIGURATION_LOOKUP; STALE evidence only | CURRENT_ONLY: INSUFFICIENT_EVIDENCE after expansion |
| QE-04 | ARCHITECTURE_EXPLANATION; implementation + knowledge both present | Both included; knowledge authority noted |
| QE-05 | CROSS_REPO_DEPENDENCY; low-confidence relationship | Excluded from required count; expansion triggered |
| QE-06 | GENERIC_SEARCH | No required evidence; any result accepted |
| QE-07 | DEBUGGING_ROOT_CAUSE; source verification used | max_extra_files / bytes respected |
| QE-08 | Unrecognized query type | Classified as GENERIC_SEARCH |
| QE-09 | IMPACT_ANALYSIS; expansion rounds exhausted | Return with best available; INSUFFICIENT flag if required not met |
| QE-10 | TEST_BEHAVIOR; test file found | Test evidence surfaced; subject implementation preferred |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| QE-Q1 | V1 query type classifier: simple keyword heuristic or lightweight ML? | No — keyword heuristic for V1; ML in later phase |
| QE-Q2 | Should callers be able to override query type, or is classification always automatic? | No — automatic for V1; operator override in later phase |
| QE-Q3 | Should `min_count` for required evidence be configurable per deployment? | No — hardcoded per query type for V1; configurable later |
