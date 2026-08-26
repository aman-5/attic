# ADR-013: Semantic Provider Abstraction & Default Model Choice

**Status:** Accepted (Phase 5)
**Date:** 2026-08-26
**Resolves:** OQ-001
**Supersedes:** none

## Context

Phase 5 requires embeddings but forbids hardwiring Attic to one vendor,
runtime, or model (PHASE_5_SEMANTIC §6–§7). Attic must remain useful with
the semantic layer absent, and every stored artifact must carry a
deterministic model identity.

## Decision

### D1 — Provider-neutral trait, no vendor leakage

`attic_semantic::SemanticProvider` is the ONLY embedding contract in the
codebase (`id`, `model_id`, `dimensions`, `max_input_bytes`, `available`,
`embed_batch(batch, cancel, usage)`). Retrieval/storage/MCP depend on the
trait and on `SemanticStack`; no ONNX/tokenizer/vendor type appears outside
`attic-semantic`.

### D2 — Default provider: built-in deterministic hashing embedder

V1 ships `HashingEmbedder` ("hashing" / "hashed-ngram-v1", 256 dims): word
unigrams + char trigrams feature-hashed via BLAKE3 into an L2-normalized
bag-of-features vector.

Evaluation that led here:

| Criterion | fastembed-rs 5.17.x (+bge-small-en-v1.5) | candle/ort direct | cloud APIs | HashingEmbedder |
|---|---|---|---|---|
| Maintained (2026) | yes (~845k dl/90d) | yes | n/a | in-tree |
| Offline/deterministic | after ~50 MB download | model-dependent | no | **always** |
| Windows/macOS/Linux | yes | yes | — | **yes** |
| CPU/RAM | moderate | moderate | — | **negligible** |
| GPU optional-not-required | yes | yes | — | n/a |
| Reproducible CI gates | fragile (network) | fragile | no | **byte-stable** |
| License | Apache-2.0 / MIT weights | varies | TOS | MIT/Apache workspace |
| Privacy | local | local | source leaves machine | **local** |
| Code/multilingual quality | strong | strong | strongest | lexical-level |

bge-small-en-v1.5 (MIT, 384d) remains the RECOMMENDED neural swap-in via
fastembed when quality demands it; jina-embeddings-v2-base-code (Apache-2.0,
768d) is the code-specialized alternative. Both implement the same trait;
neither is required for V1 because remote downloads break reproducible
benchmark/test gates and add first-run failure modes without measured need
(see ADR-014 value-gate data).

Cloud providers are REJECTED by default: §18 forbids source leaving the
machine without an explicit product decision.

### D3 — Query-time inputs follow the same rules as enrichment inputs

Query text is bounded by `max_input_bytes`, redacted upstream by the RP-S2
gate, and never logged with results attached.

## Consequences

* All §22 tests and both benchmark gates are byte-deterministic.
* Swapping in a neural provider later changes ZERO lines outside
  `attic-semantic::providers` plus one construction site per binary.
* Embedding-quality headroom is intentionally deferred until a workload
  demonstrates the hashing baseline is insufficient (value gate).
