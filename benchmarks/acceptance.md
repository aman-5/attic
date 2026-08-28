# Benchmark Acceptance Gates — Phase 0 Definition
## Document: benchmarks/acceptance.md
## Status: AUTHORITATIVE — do not modify without updating CONTRACT_CHECKLIST.md

---

## 1. Purpose and Scope

This document defines **four tiers of acceptance gates** that apply at different points in the project lifecycle. Gates are separated by what exists at evaluation time:

| Tier | Name | Evaluated when |
|---|---|---|
| **T1** | Phase 0 → Phase 1A static gate | Phase 0 complete; no runtime exists |
| **T2** | Phase 4 retrieval quality gate | Retrieval layer implemented (Phase 4) |
| **T3** | Phase 5 semantic-value gate | Semantic intelligence layer implemented (Phase 5) |
| **T4** | Production gate | Pre-production readiness |

**Only T1 gates block Phase 1A.** Runtime latency, accuracy, and semantic quality gates (T2–T4) cannot be evaluated until the corresponding implementation phases are complete. No part of this document requires Phase 1A to implement retrieval, query classification, embedding, or answer assembly.

**Benchmark cases** (`benchmarks/cases/`) are product-level engineering questions posed against benchmark repositories. They do not reference or assume Attic internal function names, DB writer implementation details, `CancellationToken` types, mutation sites, or any other future source layout. Implementation conformance tests (verifying that Attic's internal components satisfy contract invariants) are defined separately in `benchmarks/conformance/` (to be created in Phase 1A).

---

## 2. Tier 1 — Phase 0 → Phase 1A Static Gates

These are the only gates that must pass before Phase 1A implementation begins. All are verifiable without any running Attic server.

| Gate ID | Description | Pass Criterion |
|---|---|---|
| T1-G01 | All Phase 0 contracts authored | All 15 contract files (C01–C15) present in `docs/contracts/` |
| T1-G02 | `migrations/0001_initial.sql` present | File exists at that path |
| T1-G03 | Migration is idempotent DDL | Every table uses `CREATE TABLE IF NOT EXISTS`; every index uses `CREATE INDEX IF NOT EXISTS` |
| T1-G04 | Benchmark suite complete | Exactly 100 cases in `benchmarks/cases/`; all 10 QueryType values covered |
| T1-G05 | Acceptance gates document present | This file present and structurally valid |
| T1-G06 | Open questions registry present | `OPEN_QUESTIONS.md` present; no invented requirements |
| T1-G07 | `cargo fmt --check --all` | Zero diff on existing Rust source files |
| T1-G08 | `cargo clippy --workspace --all-targets --all-features -- -D warnings` | Zero warnings on existing Rust source files |
| T1-G09 | `cargo test --workspace` | All existing tests green |

---

## 3. Tier 2 — Phase 4 Retrieval Quality Gate

Evaluated after Phase 4 (retrieval layer) is implemented. These gates require a running Attic server that can process queries.

### 3.1 Latency SLAs by AnswerModePolicy

Latency is measured from query receipt at the MCP transport layer to first byte of the serialized answer. All measurements use the reference hardware profile in §7.

#### FAST Mode

| Percentile | Gate |
|---|---|
| P50 | ≤ 150 ms |
| P95 | ≤ 280 ms |
| P99 / Max | 300 ms hard ceiling — must return `BUDGET_EXCEEDED`, not a partial answer |

FAST mode cases: Q001–Q010, Q021–Q030, Q093–Q094.

#### NORMAL Mode

| Percentile | Gate |
|---|---|
| P50 | ≤ 1 200 ms |
| P95 | ≤ 2 800 ms |
| P99 / Max | 3 000 ms hard ceiling |

NORMAL mode cases: Q011–Q020, Q031–Q035, Q041–Q050, Q066–Q070, Q076–Q080, Q086–Q088, Q095–Q097.

#### DEEP Mode

| Percentile | Gate |
|---|---|
| P50 | ≤ 12 000 ms |
| P95 | ≤ 28 000 ms |
| P99 / Max | 30 000 ms hard ceiling |

DEEP mode cases: Q036–Q040, Q051–Q065, Q071–Q075, Q081–Q085, Q089–Q092, Q098–Q100.

#### Latency Measurement Protocol

1. Run each case 5 times with warm cache (first run excluded as warm-up).
2. Record wall-clock latency for each of the 4 measurement runs.
3. Gate evaluated against P95 across all runs within a mode bucket.
4. Any single case exceeding `max_time_ms` in any run is a hard failure regardless of aggregate percentile.

### 3.2 Per-QueryType Pass-Rate Thresholds

A benchmark case passes when all of: (a) required evidence types present in the plan, (b) `final_confidence` at or above the case minimum, (c) no secret content in the answer, (d) latency within mode ceiling.

| QueryType | Cases | Min Pass Rate |
|---|---|---|
| DEFINITION_LOOKUP | Q001–Q010 | 90% |
| SYMBOL_NAVIGATION | Q011–Q020 | 85% |
| CONFIGURATION_LOOKUP | Q021–Q030 | 85% |
| DEBUGGING_ROOT_CAUSE | Q031–Q040 | 75% |
| TEST_BEHAVIOR | Q041–Q050 | 80% |
| ARCHITECTURE_EXPLANATION | Q051–Q065 | 70% |
| IMPACT_ANALYSIS | Q066–Q075 | 70% |
| CROSS_REPO_DEPENDENCY | Q076–Q085 | 65% |
| KNOWLEDGE_QUESTION | Q086–Q092 | 65% |
| GENERIC_SEARCH | Q093–Q100 | 75% |

**Overall pass rate gate**: ≥ 78% across all 100 cases.

### 3.3 Resource Budget Compliance

Every case execution must remain within its declared `ResourceBudget`. Violations are hard failures regardless of answer quality.

| Mode | Max RSS increase | Max DB read rows | Max open files |
|---|---|---|---|
| FAST | ≤ 32 MB | 5 000 | 5 |
| NORMAL | ≤ 128 MB | 100 000 | 50 |
| DEEP | ≤ 512 MB | 2 000 000 | 500 |

### 3.4 RetrievalPlan Observability Gates

- All required `RetrievalPlan` fields (see `crates/attic-retrieval/src/plan.rs`) present and non-null in every persisted plan.
- Steps form a valid linear sequence by `step_index`.
- Plans persisted to `ops_retrieval_log` before answer delivery: 100% compliance.
- `plan_json` size limits: FAST ≤ 64 KB, NORMAL ≤ 256 KB, DEEP ≤ 1 MB.

### 3.5 Invalidation and Freshness Gates

- Queries against workspaces with known-stale artifacts must disclose staleness (`STALE_EVIDENCE` result or equivalent); they must never return `SUCCESS` using undisclosed stale evidence. Zero tolerance.
- Artifacts in `INVALID` state must never appear in `evidence_used`. Zero tolerance.

### 3.6 Secret Safety Gates (zero tolerance)

- No secret content (see `crates/attic-discovery/src/secrets.rs` V1 patterns) in any answer text.
- No raw secret bytes in `ops_retrieval_log.plan_json`.
- No secret patterns in server logs during benchmark runs.
- Pre-flight: FTS5 tables (`fts_retrieval_units`, `fts_symbol_names`) must contain no V1 secret pattern matches before the suite runs.

---

## 4. Tier 3 — Phase 5 Semantic-Value Gate

Evaluated after Phase 5 (semantic intelligence) is implemented. Requires semantic search and re-ranking to be operational.

### 4.1 Confidence Distribution Requirements

For DEEP mode cases where `semantic_allowed = true`, the distribution of `final_confidence` values must satisfy:

| QueryType | Min % at CONFIRMED | Min % at CONFIDENT (cumulative) | Max % at UNCERTAIN |
|---|---|---|---|
| DEFINITION_LOOKUP | 70% | 90% | 10% |
| ARCHITECTURE_EXPLANATION | 25% | 65% | 35% |
| KNOWLEDGE_QUESTION | 20% | 60% | 40% |

### 4.2 Pass-Rate Uplift vs Tier 2 Baseline

Enabling semantic search and re-ranking must improve pass rates vs the Tier 2 baseline:

| QueryType | Required uplift |
|---|---|
| ARCHITECTURE_EXPLANATION | ≥ +8 percentage points |
| KNOWLEDGE_QUESTION | ≥ +10 percentage points |
| CROSS_REPO_DEPENDENCY | ≥ +5 percentage points |

---

## 5. Tier 4 — Production Gate

All T2 and T3 gates must already pass. Additional production requirements:

| Gate | Criterion |
|---|---|
| DB writer queue saturation | Queue depth ≤ 90% capacity for no more than 2 consecutive seconds during peak benchmark load |
| Concurrency limits | ≤ 4 simultaneous RETRIEVAL_NORMAL tasks; ≤ 2 simultaneous RETRIEVAL_DEEP tasks |
| Snapshot stability | All plans in a single benchmark run share the same `WorkspaceSnapshot` hash |
| Benchmark harness exit code | Exits 0 only when all applicable tier gates pass |

---

## 6. Implementation Conformance Tests

Implementation conformance tests verify that Attic's internal components satisfy contract invariants. These are **distinct from product benchmark cases** and live in `benchmarks/conformance/` (created during Phase 1A+).

Conformance tests may reference Attic internal types (e.g., the `ResourceBudget` struct, the DB writer queue, the `CancellationToken` abstraction) because they test implementation contracts. Product benchmark cases in `benchmarks/cases/` do not.

Conformance test categories to be defined in Phase 1A:
- `conformance/storage/` — migration idempotency, WAL mode, writer queue back-pressure
- `conformance/invalidation/` — state transitions, propagation correctness
- `conformance/secrets/` — scanner accuracy against V1 patterns, FTS exclusion
- `conformance/resources/` — budget enforcement, cancellation propagation
- `conformance/recovery/` — crash simulation, each of the 10 startup steps

---

## 7. Reference Hardware Profile

Benchmark results (T2+) are valid only when measured on a configuration meeting or exceeding:

| Resource | Minimum Specification |
|---|---|
| CPU | 4 physical cores, x86-64, ≥ 2.5 GHz base clock |
| RAM | 16 GB; ≤ 4 GB consumed by other processes at run time |
| Storage | NVMe SSD, ≥ 500 MB/s sequential read |
| OS | Linux (kernel ≥ 5.15) or Windows 11 (≥ 22H2) |
| Rust toolchain | 1.98.0 (per `rust-toolchain.toml`) |
| SQLite | 3.45.0 or later (WAL mode available) |

T1 gates have no hardware requirements.

---

## 8. Benchmark Suite Composition Requirements (validated at T1)

| Requirement | Gate |
|---|---|
| Total cases | Exactly 100 (Q001–Q100) |
| QueryType coverage | All 10 QueryType values represented |
| AnswerMode coverage | All 3 modes (FAST, NORMAL, DEEP) represented |
| Freshness scenario coverage | At least 5 cases with injected STALE state |
| Secret injection coverage | At least 3 cases where a secret is planted in source; answer must not contain it |
| Cross-repo coverage | At least 10 cases spanning ≥ 2 repositories |
| Empty-result coverage | At least 3 cases with expected `NO_EVIDENCE` result |
| No Attic internals | Cases describe engineering questions; no Attic type names, internal paths, or implementation details assumed |

---

## 9. Versioning

| Gate Document Version | Associated Contract Versions |
|---|---|
| v0.2.0 (this document) | C01–C15; reflects four-tier gate structure |

When any referenced contract is updated in a breaking way, this document must be updated in the same PR and its version incremented.
