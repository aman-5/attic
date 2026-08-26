# ADR-014: Phase 5 Semantic Layer — Selection, Storage, Enrichment, Hybrid Policy

**Status:** Accepted (Phase 5)
**Date:** 2026-08-26
**Related:** OQ-002 (reranking), OQ-023 (kNN scale)

## D1 — Disposable derived layer; canonical never touched

Semantic state lives in `semantic.db`, a SEPARATE SQLite file beside the
canonical index. Deleting it degrades nothing: the pipeline falls back with
recorded reasons (`SEMANTIC_DISABLED`, `NO_EMBEDDINGS_FOR_MODEL`,
`PROVIDER_UNAVAILABLE`). Canonical tables gain no vector columns.

## D2 — Explicit SemanticUnitSelection ("sem-sel-v1")

~N units ≠ ~N embeddings. A versioned, inspectable scorer decides:
signals = source_class(1.2) + size_fit(0.6, ~800-char sweet spot) +
structural nodes(0.8) + symbol defs(0.5) + repo focus(0.2) +
query_demand(1.2, from disposable demand table) + recency(0.4);
hard exclusions counted per reason: generated paths/types, lockfiles,
`IGNORED` discovery class, >16 KiB units (never silently truncated),
duplicate content (first deterministic winner), score < 0.30,
per-repo cap 512, global cap 20 000. Ordering: score desc, unit id asc.
Every exclusion reason is observable in `SelectionReport`.

## D3 — Deterministic lineage

Every embedding row stores unit id × revision × generation × selection
version × provider/model × BLAKE3 content hash of the exact embedded text.
Reconcile deletes exactly rows whose lineage no longer matches; model swaps
purge all inactive-model rows; ranking changes rebuild nothing.

## D4 — Storage: bounded brute-force kNN in the same disposable DB

Vectors stored L2-normalized (cosine = dot), top-k via bounded insertion at
policy-capped k ≤ 120 over the active model only. At Phase-5 scales this is
sub-millisecond measured; an external vector store would add operational
cost with no demonstrated need (OQ-023 records the revisit trigger).

## D5 — Bounded background enrichment

Canonical indexing completes first; enrichment is a queue-driven job with
batch ≤ 16, attempts ≤ 3, wall-clock budget per drive, cooperative
cancellation, and INFLIGHT→PENDING recovery on open (crash/power-loss §11).
Committed embeddings survive restarts; failures quarantine after max
attempts; secret-bearing or oversized inputs are refused BEFORE any provider
call (`queue_fail_permanently`) — raw secrets can never reach a provider,
cache, or log (§18). Foreground queries only READ the store and never wait
on it; the Phase 7 adaptive scheduler stays out of scope.

## D6 — Hybrid policy and the similarity noise floor

The semantic generator is one more Phase 4 producer feeding unchanged
fusion → ranking → validation. Ranking gains an explicit SEMANTIC weight
slot that is ZERO for ExactLookup / DefinitionLookup / ConfigurationLookup
(those slices cannot regress by construction). Similarity alone never
satisfies a contract: stale evidence with cosine 1.0 is still rejected
(tested).

Measured on the shared benchmark corpus:

| tier | R@5 | R@10 | MRR | nDCG@10 |
|---|---|---|---|---|
| A Phase 4 | 1.000 | 1.000 | 0.962 | 0.900 |
| B embeddings-only | 0.923 | 1.000 | 0.716 | 0.820 |
| C hybrid, no floor | 1.000 | 1.000 | 0.904 | **0.846** ← regressive noise |
| C hybrid, floor ≥0.34 | 1.000 | 1.000 | 0.962 | **0.900** |

`SEMANTIC_MIN_SIMILARITY = 0.34` makes hybrid strictly non-regressing while
preserving coverage headroom (B reaches R@10=1.0 where lexical-only tiers
historically did not). Operational: full-corpus embed 5–7 ms, incremental
enrichment after one edit 5 ms, hybrid adds ≈0 ms to foreground latency
(kNN in-memory); index size 256 f32/selected unit.

## D7 — Reranking DEFERRED (resolves OQ-002 for now)

Sequence followed: embeddings → benchmark → hybrid → benchmark. After the
noise-floor fix there is NO remaining candidate-ordering problem on the
gated corpus (C == A on MRR/nDCG), so a reranker has nothing measurable to
improve and would add cost/complexity against §24. Revisit only when a
benchmark shows ordering loss that reranking demonstrably repairs; any such
addition requires its own ADR + failure/degradation/bound tests.

## D8 — Summaries DEFERRED

No benchmark evidence yet justifies LLM-generated summary artifacts;
provenance/freshness/redaction obligations they would carry are recorded
here so a future adoption inherits them unchanged.

## Consequences

* Smallest justified semantic layer retained (§24): selection + hashing
  provider + disposable store + hybrid generator with noise floor.
* All Phase 4 guarantees, contracts, and benchmark numbers are preserved
  byte-for-byte when the semantic DB is absent or deleted.
