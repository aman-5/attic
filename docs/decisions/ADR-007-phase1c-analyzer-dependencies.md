# ADR-007 — Phase 1C Analyzer Foundation: Dependency Decisions

**Date**: 2026-08-25
**Status**: Accepted
**Phase**: 1C — Analyzer Foundation
**Crate**: `attic-analyzers` v0.1.0

---

## Context

Phase 1C establishes the analyzer subsystem that consumes preprocessed file
content from Phase 1B and produces `RetrievalUnit` records for downstream
indexing.  The design must:

1. Support both in-memory (`Bytes`) and streaming (`LargeFileStream`) content
   without loading large files fully into memory.
2. Be cancellable mid-analysis without data loss or corruption.
3. Emit non-fatal `Diagnostic` records on partial or degraded results, rather
   than failing the entire pipeline.
4. Provide a deterministic registry so that for a given `FileType` the same
   analyzer is always selected.
5. Guarantee that **any** eligible text file is searchable — the `GenericAnalyzer`
   fallback must never be bypassed.

---

## Decision 1 — No new parser crates in Phase 1C

**Decision**: `attic-analyzers` v0.1.0 depends only on crates already present in
the workspace (`attic-core`, `attic-discovery`, `bytes`, `tracing`, `uuid`).
No Tree-sitter, `syn`, markdown parser, or other language-specific parsing
library is introduced at this phase.

**Rationale**:
- Phase 1C implements the **foundation** (API contract, registry, generic
  fallback).  Language-specific analyzers belong to Phase 1C Steps 4–5 or Phase
  3 per the canonical plan.
- Keeping the dependency surface minimal reduces compilation time, audit surface,
  and the risk of supply-chain issues during the foundation phase.
- The `GenericAnalyzer` (LEXICAL:FULL) produces `RetrievalUnit`s for any text
  file, satisfying the Phase 1C gate requirement without parser dependencies.

**Alternatives considered**:
- Importing `pulldown-cmark` for a Markdown analyzer in Phase 1C — rejected;
  the Phase 1C gate does not require Markdown-specific structure, and the
  generic analyzer already makes Markdown searchable.
- Importing `serde_json` for a JSON analyzer — rejected for the same reason;
  no benchmark case in Phase 1 requires JSON-specific symbol extraction.

---

## Decision 2 — `AnalyzerContent::StreamingHandle` boxes `LargeFileStream`

**Decision**: The `StreamingHandle` variant of `AnalyzerContent` is defined as
`StreamingHandle(Box<LargeFileStream>)`, not `StreamingHandle(LargeFileStream)`.

**Rationale**:
- `LargeFileStream` is a large struct (file handle, buffers, hasher, state
  fields).  Without boxing, the `AnalyzerContent` enum would have a variant
  that is substantially larger than `Bytes`, triggering the
  `clippy::large_enum_variant` lint (denied via `#![deny(clippy::all)]`).
- Boxing moves the `LargeFileStream` to the heap, making `AnalyzerContent`
  pointer-sized for its streaming variant and passing the lint without any
  `#[allow]` bypass.

**Alternatives considered**:
- `#[allow(clippy::large_enum_variant)]` — rejected; workspace policy forbids
  silencing lints without documented justification, and boxing is the correct
  solution here.
- Separating `AnalyzerContent` into two enums — rejected; creates unnecessary
  API complexity.

---

## Decision 3 — `CancellationToken` as `Arc<AtomicBool>` newtype

**Decision**: `CancellationToken` is a newtype over `Arc<AtomicBool>`, cloneable
and cheaply shareable across the analysis pipeline.

**Rationale**:
- Cancellation must be detectable from any thread or async task that holds a
  clone of the token.  `Arc<AtomicBool>` provides shared ownership with
  lock-free reads (`Ordering::Relaxed` is sufficient for a stop flag where
  only eventual visibility is required).
- No external `tokio-util` or `CancellationToken` crate is required; the
  lightweight newtype covers the Phase 1C use cases (cancellation within a
  single `analyze()` call).
- The newtype boundary prevents accidental construction of a `CancellationToken`
  from an arbitrary `Arc<AtomicBool>` in consuming code.

**Alternatives considered**:
- `tokio_util::sync::CancellationToken` — rejected for Phase 1C; introduces
  a tokio dependency that is not yet required in the crate.
- `std::sync::Mutex<bool>` — rejected; adds unnecessary locking overhead for a
  simple flag.

---

## Decision 4 — Deterministic registry selection by `CapabilityKind` then name

**Decision**: `AnalyzerRegistry::select()` chooses the best specialized analyzer
for a `FileType` by:
1. Finding the entry whose `AnalyzerCapabilities` contains the highest-valued
   `CapabilityKind` (ordinal order: Lexical < Structural < … < SemanticResolution).
2. Breaking ties by lexicographic order of the analyzer's `name` field (ascending).

If no specialized analyzer is registered for the given `FileType`, the
`GenericAnalyzer` is returned with `fallback_used = true`.

**Rationale**:
- Determinism is required: given the same registry state, the same analyzer must
  always be selected.  This is observable in tests and critical for reproducible
  indexing runs.
- Sorting by `CapabilityKind` ordinal maximizes the semantic richness of the
  selected analyzer (a structural analyzer is preferred over a pure lexical one
  for the same file type).
- Lexicographic name tie-breaking is stable, reproducible, and free of any
  runtime randomness.

**Alternatives considered**:
- First-registered wins — rejected; non-deterministic across different
  registration orders.
- Priority field on `AnalyzerDescriptor` — rejected; introduces an additional
  API surface that is not required for Phase 1C and can be added later if
  priorities beyond capability level are needed.

---

## Decision 5 — `GenericAnalyzer` chunks at 500 lines per `RetrievalUnit`

**Decision**: `GenericAnalyzer` emits one `RetrievalUnit` per 500-line window
(`MAX_LINES_PER_CHUNK = 500`).  The final chunk may have fewer than 500 lines.

**Rationale**:
- A 500-line chunk is large enough to provide useful retrieval context (a typical
  function or section) and small enough to avoid exceeding embedding model
  token limits in Phase 5 semantic indexing.
- Line-based chunking is language-agnostic and does not require any parser.
- The constant is module-scoped, allowing it to be adjusted without API changes.

**Alternatives considered**:
- Byte-based chunking — rejected; byte boundaries do not respect line structure,
  making retrieved text harder to display and reason about.
- Token-based chunking — rejected; requires a tokenizer dependency not justified
  for Phase 1C.
- 100-line chunks — rejected; produces too many small units for typical
  source files, increasing index size without retrieval benefit.

---

## Consequences

- `attic-analyzers` introduces **zero new external crates** beyond the workspace
  baseline.
- The `Analyzer` trait is sealed via `AnalyzerDescriptor` returning a `FileType`
  set; specialized analyzers can be added in Phases 1C Steps 4–5 and Phase 3
  without breaking the registry contract.
- `StreamingHandle(Box<LargeFileStream>)` is a committed API shape; changing it
  to an unboxed variant in future would be a breaking change.
- The 500-line chunk constant is **not** part of any public contract; it may be
  adjusted based on Phase 5 embedding benchmarks.

---

## References

- `docs/decisions/ADR-005-phase1b-discovery-dependencies.md`
- `crates/attic-analyzers/src/api.rs`
- `crates/attic-analyzers/src/generic.rs`
- `crates/attic-analyzers/src/registry.rs`
- `crates/attic-analyzers/src/cancellation.rs`
