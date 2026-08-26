# Phase 5 Completion Report — Semantic Intelligence

**Status:** COMPLETE — all gates green, stopped before Phase 6.
**Baseline:** Phase 4 merged (PR #6), 519/519 tests, benchmark A-tier preserved.

## 1. Semantic architecture

Exactly the mandated shape (PHASE_5_SEMANTIC): canonical source → retrieval
units → **SemanticUnitSelection** → **SemanticProvider** → embeddings →
disposable semantic index (separate `semantic.db`) → semantic candidate
generator → **existing Phase 4 fusion/ranking/validation**. Critical
invariant enforced and tested: `semantic failure != canonical-index failure`.

New crate `crates/attic-semantic` (provider/identity/selection/store/
enrich/invalidate). Retrieval-side integration lives in
`attic-retrieval/src/semantic.rs` (`SemanticStack`, generator, fallback
reasons); the server opens an optional stack beside the canonical DB.

## 2. Provider abstraction + decision (OQ-001 → ADR-013)

`SemanticProvider` trait is the only embedding contract; zero vendor types
leak. Default provider = in-tree deterministic `HashingEmbedder`
("hashed-ngram-v1", 256 dims) chosen over fastembed/candle/cloud after the
evaluation recorded in ADR-013 (reproducible offline gates won V1; bge-small
documented as sanctioned swap-in; cloud rejected by default for privacy).

## 3. Selection policy ("sem-sel-v1", §4)

Explicit weights + hard exclusions + per-repo/global caps + duplicate-first +
16 KiB ceiling (never silently truncated) — every exclusion counted in
`SelectionReport`. Demand signal comes from a disposable query-demand table
bumped by real retrievals.

## 4. Identity / versioning (§5)

unit × SourceRevision × IndexGeneration × selection-version × provider/model
× content-hash(BLAKE3 of embedded text). Reconcile invalidates exactly the
mismatched rows; model change purges only inactive-model vectors; ranking
changes rebuild nothing. All proven by tests.

## 5. Storage (§8)

Separate SQLite (`semantic.db`), L2-normalized vectors, bounded brute-force
kNN at policy-capped k. External vector DB rejected without evidence
(OQ-023 records the revisit trigger).

## 6. Background enrichment / crash behavior (§9/§11)

Queue-driven, batch ≤ 16, attempts ≤ 3, wall-clock budget, cooperative
cancellation, INFLIGHT→PENDING recovery on open, permanent quarantine for
secret-bearing or oversized inputs. Committed work survives "power loss"
(tested via file-backed store reopen).

## 7. Hybrid retrieval & modes (§12–§14)

Generator feeds unchanged fusion/rank/validation. New explicit SEMANTIC rank
slot: weight ZERO for ExactLookup/DefinitionLookup/ConfigurationLookup;
`SEMANTIC_MIN_SIMILARITY = 0.34` noise floor makes hybrid strictly
non-regressing. FAST never touches semantics (policy-invariant + runtime
test); NORMAL bounded (48 cand / 250 ms); DEEP broader (120 / 2000 ms).
Fallbacks always recorded in the plan trace.

## 8. Reranking (§16/OQ-002) and summaries (§17)

DEFERRED with evidence (ADR-014 D7/D8): after the floor fix there is no
candidate-ordering problem left on the gated corpus; both carry their future
obligations in the ADR.

## 9. Security/privacy (§18) and LARGE files (§19)

Defense-in-depth: coordinator re-scans each unit text with the Phase 1B
detector BEFORE any provider call and permanently refuses findings (tested
by injecting an AWS key behind Phase 1B — provider observed nothing). Query
text redacted upstream (RP-S2). Oversized units are excluded with counted
reasons, never truncated into embeddings.

## 10. Benchmark methodology & results (§23, shared case table B01–B13)

A/B/C tiers run against one fixture; KG-MCP remains NOT VERIFIED (external).

| tier | R@5 | R@10 | MRR | nDCG@10 |
|---|---|---|---|---|
| A Phase 4 | 1.000 | 1.000 | 0.962 | 0.900 |
| B embeddings-only | 0.923 | 1.000 | 0.716 | 0.820 |
| C hybrid (final) | **1.000** | **1.000** | **0.962** | **0.900** |

Operational: full embed 5–7 ms; incremental enrich after one edit 5 ms;
hybrid foreground latency ≈ unchanged (~50 ms/case incl. verification);
index = 256 f32 × selected units. No-regression slices (exact/definition/
config/simple lexical) R@5 = 1.000 under C. Deletion of the semantic layer
leaves canonical retrieval byte-identical (tested).

## 11. Value-gate verdict (§24)

B alone is worse than lexical → not shipped as a mode. C with floor equals A
everywhere while adding coverage headroom and paraphrase robustness potential
→ retained as the smallest justified layer: selection + hashing provider +
store + hybrid generator + policies. Reranking/summaries rejected for lack
of measurable value today.

## 12. Tests added (§22 — 30 new)

* attic-semantic unit (10): identity determinism/sensitivity; store roundtrip,
  kNN ordering/model filtering, purge-inactive, queue lifecycle incl.
  crash-reset; selection exclusions/duplicates/oversize/caps.
* phase5_semantic_stack (19): enrichment→candidates through NORMAL pipeline;
  exact-lookup non-degradation; FAST-disabled; partial-enrichment fallback;
  NO_EMBEDDINGS reason; unavailable/failing providers; slow-provider budget;
  cancellation-without-quarantine; crash resume (file store); model-change
  purge (canonical untouched); source-edit invalidation; duplicate-once;
  generated-output never embedded; oversize excluded; **secret never reaches
  provider**; background-enricher coexists + deterministic shutdown;
  similarity≠authority under CURRENT_ONLY validation.
* phase5_semantic_benchmark (1): A/B/C gate incl. no-regression slices and
  canonical-untouched assertion.
* Mode table updated in router contracts (FAST=off, NORMAL/DEEP bounded-on).

## 13. Incidents during the phase

* Mutex self-deadlock in `queue_take_batch` (my regression during the Sync
  refactor) — hung one test run ~15 min with output swallowed by pipeline
  filters. Fixed by lock scoping; process discipline adopted per user rule:
  no-output >10 s ⇒ immediate owned-process investigation, graceful stop,
  root cause, rerun. Two later stalls were caught within seconds using it.
* Selection scored 0 units because DB `file_type` stores language strings,
  not the schema-comment enum — classifier now accepts both shapes.
* Permanent-quarantine bug looped a poisoned item until budget exhaustion —
  replaced with explicit `queue_fail_permanently`.
No endpoint-security events occurred.

## 14. Exact commands

```
cargo check --workspace --target x86_64-pc-windows-msvc
cargo clippy --workspace --all-targets --all-features --target x86_64-pc-windows-msvc -- -D warnings
cargo fmt --all -- --check
cargo test --workspace --target x86_64-pc-windows-msvc
cargo test -p attic-retrieval -p attic-semantic --target x86_64-pc-windows-msvc
```

## 15. Gate status

fmt PASS · clippy `-D warnings` PASS · **workspace tests PASS — 550 passed / 0 failed**
(incl. MCP context tool + rmcp stdio integration) · P4 gate PASS · P5 gate PASS ·
§22 coverage COMPLETE · **STOP before Phase 6.**
