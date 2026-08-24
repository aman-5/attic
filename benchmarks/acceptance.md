# Benchmark Acceptance Gates — Phase 0 Definition
## Document: benchmarks/acceptance.md
## Status: AUTHORITATIVE — do not modify without updating CONTRACT_CHECKLIST.md

---

## 1. Purpose and Scope

This document defines the **acceptance gates** that must be satisfied by a Phase 1A+ implementation before any benchmark case in `benchmarks/cases/` is considered passing. It specifies:

- Per-`AnswerModePolicy` latency SLAs (P50, P95, P99)  
- Per-`QueryType` minimum pass-rate thresholds  
- Confidence level distribution requirements  
- Resource budget compliance gates  
- Secret-safety gates (zero-tolerance)  
- Benchmark suite composition requirements  
- Overall Phase 0 → Phase 1A readiness criterion  

**These gates are binding.** A build that does not meet all gates in §2 through §8 is not eligible for Phase 1A merge.

---

## 2. Latency Gates by AnswerModePolicy

Latency is measured from **query receipt at the MCP transport layer** to **first byte of the serialized answer** delivered back to the caller. All measurements are taken on the reference hardware profile defined in §9.

### 2.1 FAST Mode Latency Gates

| Percentile | Gate | Rationale |
|---|---|---|
| P50 | ≤ 150 ms | Must feel instant in IDE context |
| P95 | ≤ 280 ms | Within `max_time_ms` = 300 ms budget |
| P99 | ≤ 300 ms | Hard ceiling; queries exceeding this are TIMEOUT failures |
| Max (any) | 300 ms | Enforced by `AnswerModePolicy.max_time_ms`; must return `BUDGET_EXCEEDED` result code, not a partial answer |

FAST mode cases: Q001–Q010, Q021–Q030, Q093–Q094.

### 2.2 NORMAL Mode Latency Gates

| Percentile | Gate | Rationale |
|---|---|---|
| P50 | ≤ 1 200 ms | Responsive for IDE lookup |
| P95 | ≤ 2 800 ms | Within `max_time_ms` = 3 000 ms budget |
| P99 | ≤ 3 000 ms | Hard ceiling |
| Max (any) | 3 000 ms | Enforced hard ceiling; must return `BUDGET_EXCEEDED` |

NORMAL mode cases: Q011–Q020, Q031–Q035, Q041–Q050, Q066–Q070, Q076–Q080, Q086–Q088, Q095–Q097.

### 2.3 DEEP Mode Latency Gates

| Percentile | Gate | Rationale |
|---|---|---|
| P50 | ≤ 12 000 ms | Allowed for thorough analysis |
| P95 | ≤ 28 000 ms | Within `max_time_ms` = 30 000 ms budget |
| P99 | ≤ 30 000 ms | Hard ceiling |
| Max (any) | 30 000 ms | Enforced hard ceiling |

DEEP mode cases: Q036–Q040, Q051–Q065, Q071–Q075, Q081–Q085, Q089–Q092, Q098–Q100.

### 2.4 Latency Measurement Protocol

1. Run each benchmark case **5 times** with warm cache (first run is a warm-up and excluded).
2. Record wall-clock latency for each of the 4 measurement runs.
3. Gate is evaluated against the **P95** across all runs of all cases within a mode bucket.
4. A single case that exceeds `max_time_ms` in any run is a **hard failure** for that case regardless of the aggregate percentile.
5. Clock source: `std::time::Instant` on the server side; transport overhead is included.

---

## 3. Per-QueryType Accuracy Gates

"Passing" a benchmark case requires **all** of the following to be true simultaneously:
- The required evidence types listed in the case are present in the `RetrievalPlan.evidence_used` list.
- The `final_confidence` is at or above the minimum level specified in the case.
- No secret content appears in the returned answer (§7).
- Latency is within the mode's hard ceiling.

### 3.1 Pass-Rate Thresholds

| QueryType | Cases | Min Pass Rate (v1.0) | Min Pass Rate (v0.5 beta) |
|---|---|---|---|
| DEFINITION_LOOKUP | Q001–Q010 | 95% (≥ 10/10 with 1 allowed near-miss) | 80% |
| SYMBOL_NAVIGATION | Q011–Q020 | 90% (≥ 9/10) | 75% |
| CONFIGURATION_LOOKUP | Q021–Q030 | 90% (≥ 9/10) | 80% |
| DEBUGGING_ROOT_CAUSE | Q031–Q040 | 80% (≥ 8/10) | 65% |
| TEST_BEHAVIOR | Q041–Q050 | 85% (≥ 9/10 rounded) | 70% |
| ARCHITECTURE_EXPLANATION | Q051–Q065 | 75% (≥ 11/15 rounded) | 60% |
| IMPACT_ANALYSIS | Q066–Q075 | 75% (≥ 8/10 rounded) | 60% |
| CROSS_REPO_DEPENDENCY | Q076–Q085 | 70% (≥ 7/10) | 55% |
| KNOWLEDGE_QUESTION | Q086–Q092 | 70% (≥ 5/7) | 55% |
| GENERIC_SEARCH | Q093–Q100 | 80% (≥ 6/8) | 65% |

**Overall pass rate gate**: ≥ 82% across all 100 cases (≥ 82 cases passing).

> **Note**: A "near-miss" for DEFINITION_LOOKUP is when the correct symbol is present in `evidence_used` but at lower authority than expected (e.g., `INFERRED` when `DIRECT` was required). Near-misses count as ½ point toward the pass count.

### 3.2 Confidence Level Distribution Requirements

For each QueryType bucket, the distribution of `final_confidence` values across all passing cases must satisfy:

| QueryType | Minimum % at CONFIRMED | Minimum % at CONFIDENT | Maximum % at UNCERTAIN |
|---|---|---|---|
| DEFINITION_LOOKUP | 70% | 90% cumulative | 10% |
| SYMBOL_NAVIGATION | 60% | 85% cumulative | 15% |
| CONFIGURATION_LOOKUP | 65% | 88% cumulative | 12% |
| DEBUGGING_ROOT_CAUSE | 30% | 70% cumulative | 30% |
| TEST_BEHAVIOR | 40% | 75% cumulative | 25% |
| ARCHITECTURE_EXPLANATION | 20% | 60% cumulative | 40% |
| IMPACT_ANALYSIS | 25% | 65% cumulative | 35% |
| CROSS_REPO_DEPENDENCY | 20% | 60% cumulative | 40% |
| KNOWLEDGE_QUESTION | 15% | 55% cumulative | 45% |
| GENERIC_SEARCH | 30% | 70% cumulative | 30% |

`ConfidenceLevel` enum values in ascending order: `NO_EVIDENCE < UNCERTAIN < POSSIBLE < CONFIDENT < CONFIRMED`.

---

## 4. Resource Budget Compliance Gates

Every benchmark case execution must remain within its declared `ResourceBudget` (from `docs/contracts/resources.md`). Violations are **hard failures** regardless of answer quality.

### 4.1 Memory Ceiling

| Mode | Max RSS increase during query | Rationale |
|---|---|---|
| FAST | ≤ 32 MB above idle baseline | Tight—FAST should hit cache only |
| NORMAL | ≤ 128 MB above idle baseline | File reads + graph traversal |
| DEEP | ≤ 512 MB above idle baseline | Full graph + semantic allowed |

Measurement: RSS delta reported by the OS between query start and answer delivery.

### 4.2 DB Read Row Ceiling

| Mode | max_db_read_rows gate | Must not exceed |
|---|---|---|
| FAST | 5 000 | 5 000 |
| NORMAL | 100 000 | 100 000 |
| DEEP | 2 000 000 | 2 000 000 |

Enforcement: tracked via `ResourceBudget.max_db_read_rows`; `BUDGET_EXCEEDED` returned if crossed.

### 4.3 Open File Ceiling

| Mode | max_open_files gate |
|---|---|
| FAST | 5 |
| NORMAL | 50 |
| DEEP | 500 |

### 4.4 DB Writer Queue Back-Pressure Gate

- During benchmark runs, the DB writer queue depth must not exceed **90%** of its maximum capacity (`RC-A3` from resources.md) for more than 2 consecutive seconds.
- If this threshold is breached during a run, the run is marked `BACKPRESSURE_VIOLATION` and excluded from latency statistics (but counted as a failure for pass-rate purposes).

### 4.5 Concurrency Limit Gate

- No query of class `RETRIEVAL_FAST` may use more than **1 DB writer slot** concurrently per resources.md `RC-A1`.
- No more than **4 RETRIEVAL_NORMAL** tasks may be simultaneously active.
- No more than **2 RETRIEVAL_DEEP** tasks may be simultaneously active.
- Violations are counted as concurrency policy failures (separate from accuracy failures).

---

## 5. RetrievalPlan Observability Gates

Every query response must produce a `RetrievalPlan` that satisfies all of the following:

### 5.1 Required Fields Gate

All fields listed as required in `docs/contracts/retrieval_plan.md §2.1` must be present and non-null in every persisted plan. Missing fields are **schema violations** and the benchmark case is a hard failure regardless of answer quality.

Required: `plan_id`, `query_id`, `created_at_us`, `raw_query`, `query_type`, `policy`, `steps`, `evidence_used`, `result`, `final_confidence`, `context_tokens`.

### 5.2 Step Sequence Integrity Gate

- `steps` must form a valid linear sequence by `step_index` (no gaps, no duplicates).
- Every step with `status = COMPLETED` must have `duration_us > 0`.
- Every step with `status = FAILED` must have a non-empty `error_message`.
- Every step with `status = SKIPPED` must have a non-empty `skip_reason`.

### 5.3 Plan Persistence Gate

Plans must be persisted to `ops_retrieval_log` before the answer is delivered to the caller (per `RP-INV-7` from retrieval_plan.md). In benchmark runs, validate that a row for the case's `plan_id` exists in the DB **at the time the latency stop-clock is triggered**.

Gate: 100% of plans persisted before answer delivery. Zero tolerance.

### 5.4 Plan JSON Size Gate

The `plan_json` column in `ops_retrieval_log` must not exceed:

| Mode | Max plan_json size |
|---|---|
| FAST | 64 KB |
| NORMAL | 256 KB |
| DEEP | 1 MB |

Plans exceeding this threshold are flagged as `PLAN_OVERFLOW` failures and must be truncated deterministically (only `evidence_dropped` and non-critical step details may be omitted; core fields are never truncated).

---

## 6. Invalidation and Freshness Gates

### 6.1 Stale Artifact Gate

For benchmark cases where the query workspace has been modified after indexing (test scenarios involving `STALE` state):
- The `RetrievalPlan` must include at least one step with `subsystem = FRESHNESS_CHECKER`.
- The plan `result` must be `STALE_EVIDENCE` or `REPAIR_CYCLE_LIMIT_REACHED`; it must **not** return `SUCCESS` while using known-stale evidence without disclosing it.
- Cases: INV-adjacent scenarios, Q031–Q040 (debugging scenarios where staleness is injected).

Gate: 100% compliance. Returning a success answer built on undisclosed stale evidence is a **critical failure**.

### 6.2 INVALID Artifact Gate

For benchmark cases where an artifact has been explicitly invalidated (state = `INVALID`):
- It must **never** appear in `evidence_used`.
- It may appear in `evidence_dropped` with `drop_reason = INVALID_ARTIFACT`.

Gate: 100% compliance. Zero tolerance on invalid evidence use.

---

## 7. Secret Safety Gates

All gates in this section are **zero-tolerance**. A single violation is an immediate **suite-wide failure** that blocks Phase 1A regardless of all other metrics.

### 7.1 Secret Content in Answer Gate

No content classified as a secret (via the `SecretState` / `RedactedSpan` contract in `docs/contracts/secrets.md`) may appear in any benchmark answer text.

Validation method: Run the V1 secret detector patterns over every answer text in the benchmark run. Any match is a `SECRET_LEAK` failure.

### 7.2 Secret Content in Plan Gate

`ops_retrieval_log.plan_json` must not contain raw secret bytes. `RedactedSpan` representations (showing span location and pattern ID but not the matched text) are allowed.

### 7.3 Secret Content in Logs Gate

Server logs emitted during benchmark runs must not contain patterns matching V1 secret detector rules. Log scanning is part of the benchmark harness.

### 7.4 Secret Content in FTS Index Gate

A separate index integrity check (run once before the benchmark suite):
- Query the FTS5 `fts_retrieval_units` and `fts_symbol_names` tables with V1 secret detector patterns.
- Any match is a `SECRET_IN_INDEX` failure.

This check is run once as a pre-flight gate; if it fails, the benchmark suite does not run.

---

## 8. SourceRevision Stability Gate

All benchmark cases within a single run share a **pinned `WorkspaceSnapshot`**. The snapshot is frozen at the start of the run and must not change during execution.

Gate: The `source_revision` field on every plan produced during a single benchmark run must hash to the same `WorkspaceSnapshot`. Mixed snapshots within a run are a `SNAPSHOT_INSTABILITY` failure.

---

## 9. Reference Hardware Profile

Benchmark results are only valid when measured on a configuration meeting or exceeding:

| Resource | Minimum Specification |
|---|---|
| CPU | 4 physical cores, x86-64, ≥ 2.5 GHz base clock |
| RAM | 16 GB system RAM; ≤ 4 GB consumed by other processes at run time |
| Storage | NVMe SSD, ≥ 500 MB/s sequential read |
| OS | Linux (kernel ≥ 5.15) or Windows 11 (≥ 22H2) |
| Rust toolchain | 1.98.0 (per `rust-toolchain.toml`) |
| SQLite | 3.45.0 or later (WAL mode available) |

Results on hardware below this profile are informational only and do not constitute acceptance evidence.

---

## 10. Benchmark Suite Composition Requirements

Before the suite is considered valid for acceptance evaluation:

| Requirement | Gate |
|---|---|
| Total cases | Exactly 100 (Q001–Q100 as defined in `benchmarks/cases/`) |
| QueryType coverage | All 10 QueryType values represented |
| AnswerMode coverage | All 3 modes (FAST, NORMAL, DEEP) represented |
| Freshness scenario coverage | At least 5 cases with injected STALE state |
| Secret injection coverage | At least 3 cases where a secret is planted in source; answer must not contain it |
| Cross-repo coverage | At least 10 cases spanning ≥ 2 repositories |
| Empty-result coverage | At least 3 cases with expected `NO_EVIDENCE` result |

All requirements above are satisfied by `q001_to_q050.md` + `q051_to_q100.md`.

---

## 11. Phase 0 → Phase 1A Readiness Gate (Summary)

All of the following must be satisfied before Phase 1A implementation begins:

| Gate ID | Description | Threshold |
|---|---|---|
| ACC-G01 | Overall benchmark pass rate | ≥ 82 / 100 cases |
| ACC-G02 | FAST mode P95 latency | ≤ 280 ms |
| ACC-G03 | NORMAL mode P95 latency | ≤ 2 800 ms |
| ACC-G04 | DEEP mode P95 latency | ≤ 28 000 ms |
| ACC-G05 | Secret content in answers | 0 violations |
| ACC-G06 | Secret content in plan_json | 0 violations |
| ACC-G07 | Secret content in FTS index | 0 violations (pre-flight) |
| ACC-G08 | INVALID evidence used in answer | 0 violations |
| ACC-G09 | Undisclosed stale evidence in SUCCESS answer | 0 violations |
| ACC-G10 | Plan persistence before answer delivery | 100% |
| ACC-G11 | ResourceBudget hard ceiling exceeded | 0 violations |
| ACC-G12 | All contracts authored and CONTRACT_CHECKLIST complete | 100% |
| ACC-G13 | migrations/0001_initial.sql present and idempotent | Pass |
| ACC-G14 | `cargo fmt --check --all` | Zero diff |
| ACC-G15 | `cargo clippy --workspace --all-targets -- -D warnings` | Zero warnings |
| ACC-G16 | `cargo test --workspace` | All tests green |

> Gates ACC-G14 through ACC-G16 apply to the Phase 0 scaffolding code only (stub `lib.rs` files and the migration SQL do not generate Rust compilation artifacts; these gates apply once Phase 1A skeleton code is introduced). For Phase 0 specifically, the applicable gates are ACC-G12 and ACC-G13 plus any Rust code that does exist compiling cleanly.

---

## 12. Benchmark Harness Contract

The benchmark harness (to be implemented in Phase 1B or later) must:

1. Accept a `WorkspaceSnapshot` identifier and a path to a cases directory as inputs.
2. Execute each case in isolation with a fresh per-case `RetrievalPlan` context.
3. Record per-case: latency (ms), pass/fail, `final_confidence`, `plan_id`, evidence count, resource metrics.
4. Produce a machine-readable JSON report at `benchmarks/reports/<run_id>.json` and a human-readable summary at `benchmarks/reports/<run_id>.md`.
5. Run the secret-safety scans (§7) as pre-flight and post-flight steps.
6. Exit with code 0 only if all gates in §11 are satisfied; exit with code 1 otherwise.

The report schema (to be formalized in Phase 1B) must include per-case results, per-gate pass/fail, and aggregate statistics keyed by QueryType and AnswerMode.

---

## 13. Versioning

This acceptance gate document is versioned alongside the contracts it references:

| Gate Document Version | Associated Contract Versions |
|---|---|
| v0.1.0 (this document) | C01–C15, answer_modes v0.1, retrieval_plan v0.1, resources v0.1, recovery v0.1 |

When any referenced contract is updated in a breaking way, this document must be updated in the same PR and its version incremented.
