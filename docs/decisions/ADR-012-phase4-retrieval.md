# ADR-012 — Phase 4 Evidence-Driven Retrieval Architecture

Status: ACCEPTED (Phase 4)
Date: 2026-08-26
Depends on: ADR-004 (subsystem versions), ADR-009 (identity links),
ADR-011 (relationship semantics), contracts evidence.md / query_evidence.md /
answer_modes.md / retrieval_plan.md / resources.md

## Context

Phase 4 delivers the evidence-driven retrieval brain BEFORE any semantic
capability. The approved contracts define the objects; this record captures
the implementation decisions taken where contracts left implementation
latitude, so later phases inherit explicit reasoning rather than accidents.

## Decisions

### D1 — Query taxonomy: 12 types (10 approved + 2 additions)

`query_evidence.md` defines ten QueryType values. The Phase 4 brief requires
"at minimum" also exact lookup, behavior explanation, and cross-repository
questions. Implemented as:

* `EXACT_LOOKUP` — new; required evidence `file_content` accepted from ANY
  current artifact type at that location (`SOURCE_CODE | CONFIGURATION |
  DOCUMENTATION | KNOWLEDGE | TEST`). Sharing DEFINITION_LOOKUP's
  code-only requirement made exact path questions unsatisfiable for config
  files and forced noise expansion rounds.
* `CROSS_REPO_QUESTION` — new workspace-scope variant of the
  CROSS_REPO_DEPENDENCY contract.
* Behavior explanations classify into `ARCHITECTURE_EXPLANATION`; dependency
  phrasings classify into `DEPENDENCY_QUESTION`.

All twelve carry full QueryEvidenceContracts; unrecognized input defaults to
`GENERIC_SEARCH` with LOW classification confidence.

### D2 — Knowledge evidence is path-derived; `core_knowledge_items` stays unwritten

Retrieval classifies canonical SourceType from the repo-relative path
(`knowledge/*` + README/ARCHITECTURE → KNOWLEDGE; test markers → TEST;
config extensions → CONFIGURATION; doc extensions → DOCUMENTATION; else
SOURCE_CODE). Populating `core_knowledge_items` during indexing is deferred:
nothing in V1 consumes its extra columns (authority/supersedes/applicable
versions), and FTS over knowledge content already flows through retrieval
units. Recorded as OQ-021.

### D3 — Source verification is SPAN-LOCAL containment, not file-hash equality

The index stores whole-file BLAKE3 hashes; a query-relevant window rarely
hashes to them, so CHECKSUM-level verification confirms that the evidenced
FACT (first non-empty snippet line, whitespace-normalized) still appears
verbatim inside the corresponding region of the live file read through the
Phase 1B secure path (`canonicalize_within_root` + secrets preprocessing).
Outcome mapping: contained → VERIFIED (+ freshness upgraded to CURRENT where
contractually recoverable); absent → STALE/ContentChanged; unreadable/
excluded/no-span → Unavailable; zero FS budget → BlockedByBudget (FAST never
reads). Repository roots are resolved PER REPOSITORY from the index at
verification time — caller-supplied roots do not exist in the API.

### D4 — Proactive checksum pass in NORMAL/DEEP

`answer_modes.md` sets `source_verification_level = CHECKSUM/FULL` for these
modes; a dirty working tree is undetectable without reading. The pipeline
therefore verifies the top ≤5 ranked non-relationship items BEFORE sufficiency
evaluation (bounded by the FS budget). FAST skips by policy and records an
observable PolicyViolation step if a contract requests verification.

### D5 — Stale-first expansion ordering

When a CURRENT_ONLY contract holds stale/unverified candidates,
SOURCE_VERIFICATION is attempted FIRST regardless of declared fallback order
(cheapest route back to satisfiability; every other strategy re-discovers the
same stale rows). Otherwise strategies fire in declared order, each once.

### D6 — Context assembly: primary-section leadership + two-tier floor + caps

Context serves VALIDATED evidence only, ordered contract-primary section
first, then section rank, then score. Admission applies a hard floor
(max(0.65·top, 0.20)); contract-PREFERRED source types get a soft floor
(max(0.35·top, 0.15)); every non-primary section contributes ≤1 item once the
primary has spoken (≤2 otherwise); duplicates dedupe by content fingerprint;
final text passes the Phase 1B secret scan (defense in depth). Trims record
`BELOW_SCORE_THRESHOLD` / `CONTEXT_TOKEN_LIMIT` / `SECRET_CONTENT_DETECTED`
in the plan. RP-INV-7 holds: context_tokens == Σ EvidenceRef.token_count.

### D7 — Claims serve only context-grounded support

A verified claim is served ONLY if every cited evidence id appears in the
assembled context refs ("claim without visible support is not servable").
Unsupported/rejected claims never reach output; contradictions force
SUPPORTED_WITH_DISCLOSURE verdicts.

### D8 — Plans persist through the writer queue pre-answer

Finalized plans INSERT into `ops_retrieval_log` via `WriterQueueHandle` before
the answer returns; persistence failure logs `PlanPersistenceFailure` but
never blocks the answer (RP-INV-6). Workspace id = BLAKE3 of repository scope
ids (never raw paths).

## Benchmark honesty

KG-MCP is an external system not present on this machine: recorded NOT
VERIFIED, never fabricated. Baseline tiers L0/L1/L2 are capability-faithful
slices computed through the same public storage APIs each prior phase
exposed (FTS-only; +symbol definitions; +resolved relationships). Latency SLAs
from benchmarks/acceptance.md T2 require reference hardware; debug-build
latencies (~51 ms/case) are printed informationally, not gated here.

## Consequences

* Phase 5 can add SEMANTIC_SEARCH as a fallback strategy + semantic_score
  signal without touching contracts or validation.
* `core_knowledge_items` population becomes a small indexing-side change if
  OQ-021 resolves positively.
* Verification semantics are intentionally conservative; FULL-level field
  re-parse remains available for future deep verification.
