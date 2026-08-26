# Phase 4 Completion Report — Evidence-Driven Retrieval

Status: **PHASE 4 COMPLETE** — stopped before Phase 5 as instructed.
Date: 2026-08-26

---

## 1. Query Router / QueryType implementation

`crates/attic-retrieval/src/query.rs`

* 12-type taxonomy: the ten approved contract types plus `EXACT_LOOKUP` and
  `CROSS_REPO_QUESTION` (ADR-012 D1).
* Deterministic keyword/signal classifier (no LLM, no network, invariant
  QE-6): ordered rules, signal trace, competing-type capture. Overlapping
  HIGH signals downgrade to MEDIUM certainty — uncertain classification is
  never silently certain.
* Term/path/symbol/config-key extraction; stop-word filtering.
* Malformed/untrusted input REJECTED (empty, >512 chars, control chars) —
  never truncated silently into a different query.

## 2. Query Evidence Contracts

`src/contract.rs` — all twelve types mapped to explicit
required/preferred evidence, freshness floors (`CURRENT_ONLY`,
`CURRENT_OR_STALE`, `ANY`), relationship-confidence minimums, repository
scope, allowed fallbacks and expansion budgets, exactly per
`query_evidence.md` (plus ADR-012 D1 additions). `GENERIC_SEARCH` requires
nothing; unknown intent can never manufacture requirements.

## 3. AnswerModePolicy (FAST/NORMAL/DEEP)

`src/mode.rs` — V1 default table verbatim from `answer_modes.md` §3;
startup overrides with validation (≥50 ms floor, 128K token ceiling,
FAST ⇒ zero FS budget + zero repairs); `semantic_allowed=false` in ALL
modes for Phase 4 (DEEP functions without Phase 5); PolicyExecutionTrace
with budget_fields_hit and final_result emitted per query.

## 4. RetrievalPlanner + RetrievalPlan

`src/plan.rs`, planner section of `pipeline.rs`. Serializable plan capturing
query type + classification confidence/signals/competing types, workspace id
(hash), planned lexical/symbol/structural/graph/knowledge operations,
source-verification policy, evidence requirements, immutable policy snapshot;
append-only steps per subsystem with candidate in/out counts; evidence_used /
evidence_dropped accounting (RP-INV-4); single-shot finalize (RP-INV-1) with
unfinished steps recorded CANCELLED; deterministic JSON round-trip (RP-SR2);
raw-query secret redaction marker (RP-S2); persisted to `ops_retrieval_log`
through the writer queue BEFORE the answer returns (RP-L3), persistence
failure non-blocking (RP-INV-6).

## 5. Candidate generators + fusion

`src/candidates.rs`, `src/fuse.rs` — six generators over existing
intelligence: FTS lexical (phrase-quoted terms — injection-safe), exact path,
Phase 3 symbols (exact→suffix), structural outlines/type scans, direct
relationship edges, knowledge/docs. Every candidate keeps origin retriever,
raw score, query fragment, revision/generation/content-hash provenance
(invariant 1). Fusion dedupes by (source_type, source_id, span-start):
duplicate discovery merges signals elementwise-max and unions origins —
never multiplies one fact into fake independent support.

## 6. Ranking signals (observable)

`src/rank.rs` — per-dimension signals exactly as contracted (lexical,
symbol-match, intent-match via explicit origin×intent table, repo-relevance,
freshness, structural proximity, relationship confidence, knowledge
authority, test relevance; semantic_score structurally absent in Phase 4)
+ derived combined_score from an explicit per-QueryType weight table.
Ranking ("likely useful?") is separate from validation ("can it support the
requirement?") — no opaque merged score.

## 7. Evidence model + validation

`attic-evidence` crate + `src/validate.rs` — canonical Evidence object per
evidence.md; validation independently checks provenance presence, INVALID
filtration, span sanity, freshness vs contract, relationship confidence
floor AND resolution honesty (SYNTACTIC edges are context-only, never count
as resolved facts), authority applicability. Verdicts carry explanations.

## 8. Evidence Manager + sufficiency + expansions

`src/manager.rs`, `src/graph.rs`, `src/verify.rs`, expansion loop in
`pipeline.rs` — SufficiencyReport evaluates every required slot
(min_count, source-types, freshness, confidence floor, contradiction
exclusion). Unsatisfied contracts trigger bounded targeted expansion:
BROADER_FTS, BOUNDED_GRAPH (deterministic BFS; depth/node budgets enforced;
per-edge type/resolution/confidence/hop provenance retained; unresolved
logical targets labeled), SOURCE_VERIFICATION (ADR-012 D3/D4/D5),
KNOWLEDGE_LOOKUP. SEMANTIC_SEARCH exists only as an explicitly unavailable
strategy. Budget exhaustion is observable everywhere; INSUFFICIENT_EVIDENCE
is returned rather than any fabricated answer.

## 9. Contradiction handling

`src/contradiction.rs` — ambiguous definitions across files, conflicting
configuration values, knowledge-vs-implementation/config mismatches,
stale-vs-current duplicates. Both sides stay surfaced; CONTRADICTED marking +
context disclosure section; claims touching contradictions require
disclosure verdicts.

## 10. Context Builder

`src/context.rs` — validated-evidence-only assembly; primary-section
leadership; two-tier score floor + per-section caps (ADR-012 D6);
content-fingerprint dedup; byte/token budget dropping lowest-first with
recorded reasons; staleness/verification caveats inline; contradiction
disclosure section; bounded snippets (no whole-file dumps).

**Secret-safety pass (implemented, two layers):**
1. Per-item, pre-inclusion: the approved Phase 1B detector scans each fully
   assembled block (header + snippet). Any finding drops the ENTIRE item —
   the block never enters `text`, no EvidenceRef is created for it, and the
   drop is recorded as `SECRET_CONTENT_DETECTED` (truthful accounting).
2. Fail-closed whole-document scan after assembly: if anything is still
   flagged, ALL served refs are demoted to recorded secret drops, the refs
   list is emptied (so claims cannot cite withheld support), and only a
   skeleton + disclosure section is served. Token accounting is computed
   AFTER this decision (RP-INV-7 holds in every path).

## 11. Claim / Answer Verification

`attic-evidence::claim`, `src/claims.rs` — deterministic claim derivation
per evidence type; verifier checks id existence, claim-type support rules,
span validity, freshness consistency, relationship resolution/confidence
floors, contradiction disclosure. Unsupported ⇒ REJECTED (never served);
served claims must be context-grounded (ADR-012 D7). No second LLM required.

## 12. Freshness / Phase 2 integration

Artifact states flow through joins at read time; INVALID filtered
pre-ranking; CURRENT_ONLY rejects STALE unless recovered by live-source
verification; STALE/UNKNOWN/PENDING_REFRESH served only with explicit
caveats; unaffected CURRENT artifacts answer normally mid-refresh.

## 13. MCP changes

One new tool: `context` (thin wrapper: query/mode/repository_id → result,
confidence, insufficient_reason, plan_id, evidence_used count, verified
claims, assembled context). Plan internals remain in ops_retrieval_log.
`file` / `search` / `repo_map` / `status` unchanged (regression-tested:
legacy tools still listed and functional).

## 14. Security & resources

Repository text stays untrusted: parameterized SQL only; FTS phrases quoted;
control-char queries rejected; verification paths via
canonicalize_within_root + secrets preprocessing (redacted streams, LARGE-file
streaming bounds, .git forbidden); secrets never enter plans (RP-S1/S2/S3),
evidence or context (defense-in-depth final scan). Budgets enforced per mode
(candidates/time/graph nodes/FS files+bytes/context tokens/repair cycles)
with observable exhaustion fields in PolicyExecutionTrace.

## 15. Tests added (executable, deterministic)

| Suite | Count | Coverage |
|---|---|---|
| lib unit | 28 | classifier determinism/malformed input, policy invariants, plan round-trip/finalize, fusion determinism, ranking signals, graph ids |
| phase4_router_contracts | 14 | §3 taxonomy ×12, ambiguity honesty, malformed/untrusted rejection, contracts per type incl. debugging/nav floors/CURRENT_ONLY set, FAST FS refusal observability, NORMAL deadline, override validation |
| phase4_pipeline_e2e | 14 | definition/config/knowledge/test/navigation/impact/dependency/generic/exact end-to-end on REAL index; empty-corpus INSUFFICIENT_EVIDENCE; plan round-trip+persistence+traceability+token-sum; reproducibility; provenance completeness; planted-secret leak guard |
| phase4_evidence_quality | 12 | stale high-rank rejection/recovery, INVALID filtration, weak-relationship exclusion, graph budget caps, dirty-tree verification recovery, config contradiction surfacing, knowledge mismatch flagging, PENDING_REFRESH/UNKNOWN handling, FAST-never-reads-FS, context budget trimming records, unsupported-claim rejection, resolved-edge relationship assertions |
| phase4_benchmark | 1 | §22 gate below |
| phase4_lineage | 3 | verification NEVER rewrites indexed freshness/revision/generation; live truth via flag; sufficiency acceptance traces to verifier steps |
| phase4_fs_budgets | 4 | actual-byte accounting: SMALL overrun ≤2048 B blocked+recorded; LARGE stream stop within 64 KiB; LARGE early-marker success; `max_fs_files` hard cap |
| phase4_context_secrets | 1 | builder-level secret block on post-validation injection; truthful drops; benign served |
| attic-server mcp_context | 1 | capability advertised + legacy tools intact + missing-query error hygiene |

## 16. Benchmark methodology + results (§22 gate)

Corpus: multi-language fixture workspace (Java service+registry+test,
Python module, YAML config, knowledge note, runbook) indexed through the
REAL pipeline. 13 cases spanning definition/exact/configuration/knowledge/
test/navigation/impact/dependency/debugging/generic. Graded relevance:
expected=3, related=2. Tiers are capability-faithful slices through public
storage APIs (L0=Phase 1D lexical; L1=+definitions; L2=+resolved
relationships). KG-MCP baseline: external binary unavailable here — recorded
**NOT VERIFIED**, not estimated.

```
tier       R@5    R@10   MRR    nDCG@10
L0 (1D)    0.923  0.923  0.750  0.786
L1 (+sym)  0.923  0.923  0.750  0.786
L2 (+rel)  0.923  0.923  0.750  0.809
P4 (full)  1.000  1.000  0.962  0.900   <- PASS: >= every tier on all four
KG-MCP     NOT VERIFIED (external system unavailable)
```

Evidence/answer metrics (P4): evidence precision 0.833 (gate ≥0.80);
provenance correctness 1.000; contract satisfaction 1.000; contradiction
surfacing 1/1 scenario; groundedness 37/37 served claims; unsupported-claim
rate 0.000 (verifier rejects pre-serve); correct INSUFFICIENT_EVIDENCE on
empty corpus asserted. No-regression slices (exact/definition/config/
lexical) Recall@5 = 1.000.

Latency: ~51 ms/case wall in debug builds printed informationally; T2 SLA
percentiles require reference-hardware release runs (NOT VERIFIED here — no
falsified numbers).

## 17. Regressions found/fixed during the phase

* Lexical/symbol generators lacked revision provenance → everything would
  have failed validation; fixed via header-fill before ranking.
* Generators omitted spans → verification always bailed; spans now parsed
  from units/occurrences.
* Whole-file-hash verification semantics replaced by span-local containment
  (correct granularity, ADR-012 D3).
* EXACT_LOOKUP initially shared code-only requirements → unsatisfiable for
  config files; split contract (D1).
* RP-INV-7 token-sum violated by boilerplate bytes → context_tokens now Σ refs.
* Context dumped unvalidated-margin items → floors/caps introduced; this
  ALONE lifted nDCG 0.775→0.900 and precision 0.487→0.833 while recall rose
  to 1.000.

## 17b. Review-hardening fixes (final correctness/security round)

| # | Issue | Fix | Proof tests |
|---|---|---|---|
| 1 | Verification mutated stale indexed evidence to CURRENT, falsifying artifact lineage | `Evidence.live_source_verified` flag + `verification_state=VERIFIED` express live truth; freshness/revision/generation NEVER rewritten; sufficiency/claims accept the flag under CURRENT_ONLY; context renders lineage-honest caveats (`STALE as indexed — fact VERIFIED…`) | `phase4_lineage.rs` ×3: DB revision/generation/freshness byte-identical before↔after SUCCESS-with-verification; direct verifier call asserts flag set with Stale state and ids untouched |
| 2 | Fixed 4096-byte estimate charged, then reads up to 256 KiB | Reader consumes at most `fs_bytes_remaining()` sanitized bytes (SMALL truncates; LARGE streams stop pulling chunks), commits ACTUAL bytes via `commit_verification_read`; truncation that prevents a containment verdict ⇒ `BlockedByBudget`; file slots charged separately so `max_fs_files` is hard | `phase4_fs_budgets.rs` ×4: SMALL overrun stops ≤2048 B w/ limits_hit; LARGE (4 MiB) late-marker stream blocked within 64 KiB; LARGE early-marker verifies inside budget; second file blocked at `max_fs_files=1` |
| 3 | Context Builder documented but did not run the final secret scan | Two-layer pass: per-item block scan pre-inclusion (finding ⇒ whole item dropped, no ref created, recorded `SECRET_CONTENT_DETECTED`) + fail-closed whole-document scan (any finding ⇒ every served ref demoted to recorded secret drop, refs emptied, skeleton+disclosure only); tokens computed after decision so RP-INV-7 always holds | `phase4_context_secrets.rs`: AWS-shaped key injected directly post-validation ⇒ absent from text, no ref for it, truthful drop recorded, benign item still served |

All three preserve Phase 1–3 guarantees, the existing retrieval architecture,
and benchmark results (re-run unchanged: P4 R@5/R@10 = 1.000, MRR 0.962,
nDCG@10 0.900, precision 0.833).

## 18. Exact commands executed (final gate)

```
cargo fmt --all -- --check
cargo check --workspace --target x86_64-pc-windows-msvc
cargo clippy --workspace --all-targets --all-features --target x86_64-pc-windows-msvc -- -D warnings
cargo test --workspace --target x86_64-pc-windows-msvc
```
(x86_64-pc-windows-msvc is machine-local execution config; never committed.)

Results: see §20 — all PASS after the fixes above.

## 19. Hangs / timeouts / security events

None. No endpoint-security interference occurred. All suites completed
within their run windows; MCP child processes terminated via the suite's
existing kill-on-exit pattern.

## 20. Gate status summary

| Check | Result |
|---|---|
| cargo fmt --check | PASS |
| cargo check --workspace | PASS |
| cargo clippy -D warnings | PASS |
| cargo test --workspace | **PASS — 519 passed / 0 failed** |
| §21 required test areas | COVERED (table §15) |
| §22 non-semantic benchmark gate | PASS (metrics §16) |
| Phase boundary (no embeddings/vector/LLM summaries) | ENFORCED |

## 21. Open questions added

* OQ-021 — populate `core_knowledge_items` at indexing time? (path-derived
  classification suffices for V1 retrieval.)
* OQ-022 — should proactive checksum breadth scale with mode beyond ≤5 items?

## 22. Known limitations

* Classifier remains lexical heuristics (QE-Q1 defers ML).
* Relationship facts limited to Phase 3 resolution levels; BUILD_RESOLVED
  awaits OQ-019 build-system parsing.
* Benchmark corpus is fixture-scale; acceptance.md hardware SLAs and the
  full 100-case product suite remain release-gate work.
* Cross-repository answers reuse the dependency contract within the local
  workspace index (Phase 6 owns true cross-repo intelligence).

**STOP — Phase 4 complete. Phase 5 requires explicit approval.**
