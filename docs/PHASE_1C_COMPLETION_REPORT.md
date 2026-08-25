# Phase 1C Completion Report — Analyzer Foundation

**Date:** 2026-08-25
**Crate:** `attic-analyzers` v0.1.0
**Status:** ✅ COMPLETE — 42/42 tests pass, zero clippy warnings

---

## 1. Scope

Phase 1C delivers the `attic-analyzers` crate: the analyzer trait, registry, dispatch
layer, and the mandatory `GenericAnalyzer` fallback. All implementation blockers
identified in the post-Phase-1C review have been resolved, including a post-completion
redesign of `dispatch.rs` to enforce bounded O(1) memory for all LARGE file paths,
followed by two subsequent hardening rounds that added:

- **`SpoolPreparation` enum** with cancellation and time-budget enforcement inside
  the spool loop (no fabricated empty content on preparation failure).
- **`Box<AnalyzerInput>` in the `Ready` variant** to satisfy
  `clippy::large_enum_variant` without changing runtime semantics.

---

## 2. Deliverables

### 2.1 `src/api.rs` — Analyzer contract (unchanged from Phase 1C baseline)

Defines the full public contract:

| Type | Purpose |
|---|---|
| `Analyzer` trait | Single `analyze(&self, input: AnalyzerInput) -> AnalyzerOutput` method |
| `AnalyzerInput` | File content (one of `FullBytes` / `RedactedBytes` / `StreamingHandle`), resource budget, cancellation token |
| `AnalyzerOutput` | Retrieval units, structural nodes, symbols, imports, relationships, diagnostics |
| `ResourceBudget` | Per-invocation limits: `max_retrieval_units`, `max_memory_bytes`, `max_time_ms`, `max_ast_nodes`, `max_recursion_depth` |
| `AnalyzerContent` | Phase 1B security boundary: content arrives pre-redacted; analyzers must never reopen the raw path |
| `RetrievalUnitSpec` | Retrieval-indexed text slice with `SourceSpan` (0-based, exclusive end) and ordinal |
| `diagnostic_codes` | Canonical string constants: `CANCELLED`, `RESOURCE_EXHAUSTED`, `MALFORMED_INPUT`, `REDACTED_INPUT`, `PARTIAL_SCAN`, `PANIC_CAUGHT`, `FALLBACK_USED` |

### 2.2 `src/cancellation.rs` — CancellationToken

`Arc<AtomicBool>`-backed newtype. Clone is cheap; `is_cancelled()` is a single
`Ordering::Acquire` load. Phase 4 migration path to `tokio_util::CancellationToken`
documented (OQ-012).

### 2.3 `src/registry.rs` — AnalyzerRegistry

- `new(generic: Arc<dyn Analyzer>)` — mandatory generic fallback.
- `register_specialized(analyzer)` — language-agnostic analyzers (empty
  `supported_file_types`) are silently ignored.
- `select(file_type)` — returns `(Arc<dyn Analyzer>, is_generic)`. Tie-breaking
  uses `CapabilityLevel` ordering (`None < Basic < Partial < Full`) then
  lexicographic name, making selection deterministic.

### 2.4 `src/generic.rs` — GenericAnalyzer

The mandatory language-agnostic fallback. Capabilities: `LEXICAL:FULL`.

#### Key design decisions

**Span semantics (0-based, exclusive end)**
All `SourceSpan` fields match `attic-core`'s canonical definition:
- `start_line` / `start_col` — 0-based, inclusive.
- `end_line` / `end_col` — 0-based, **exclusive**.

A chunk spanning file lines 0, 1, 2 produces `start_line=0`, `end_line=3`.

**CRLF-aware line splitting**
`split_lines_bytes()` scans for `\n` and strips the preceding `\r` if present.
`str::lines()` is deliberately avoided (it collapses `\r\n` and bare `\r`
inconsistently and discards empty trailing lines).

**Bounded carry — `MAX_CARRY_BYTES = 65_536`**
The streaming path holds a carry buffer for the incomplete final line of each
stream chunk. If the carry or any individual line exceeds 65 536 bytes, it is
split at a UTF-8 char boundary (`floor_char_boundary`) and emitted as virtual
lines. This guarantees O(1) per-chunk memory regardless of file content
(no-newline files, binary-ish files, etc.).

**Streaming path (`stream_into_units`)**
Consumes `LargeFileStream` chunk by chunk. Each chunk's `redacted` field (safe
bytes from Phase 1B) is prepended with the carry from the previous chunk, then
line-split. Complete lines are accumulated in `pending_lines`; when
`pending_lines.len() >= MAX_LINES_PER_CHUNK` (500), a retrieval unit is
flushed. The carry stays bounded by `MAX_CARRY_BYTES`.

**Resource enforcement**
All three budget fields are enforced after each chunk emission:
- `max_retrieval_units` — hard cap on output units (inclusive).
- `max_memory_bytes` — cumulative `retrieval_text` bytes.
- `max_time_ms` — wall-clock elapsed since analysis start.
`CancellationToken` is checked at the top of every iteration loop.
All three emit `RESOURCE_EXHAUSTED` (or `CANCELLED`) and return partial output
rather than continuing.

### 2.5 `src/dispatch.rs` — Dispatch with panic recovery and bounded spool

#### Algorithm

```
select analyzer from registry for input.file_type
if is_generic → analyze directly (no overhead)
else:
  prepare_spool(input) → SpoolPreparation
  match preparation:
    Ready { fallback_input, specialized_input, _spool_guard }:
      result = catch_unwind(|| specialized.analyze(*specialized_input))
      match result:
        Ok(output) with no error diagnostics → return as-is
        Ok(output) with error diagnostics    → run GenericAnalyzer(fallback_input)
                                               extend fb_output.diagnostics with
                                               original errors + FALLBACK_USED
        Err(panic)                           → run GenericAnalyzer(fallback_input)
                                               add PANIC_CAUGHT + FALLBACK_USED
      _spool_guard drops here → temp file deleted
    Cancelled           → return CANCELLED diagnostic
    TimeBudgetExhausted → return RESOURCE_EXHAUSTED diagnostic
    IoFailure           → return RESOURCE_EXHAUSTED diagnostic (never empty content)
```

#### `SpoolPreparation` enum

```rust
enum SpoolPreparation {
    Ready {
        fallback_input: AnalyzerInput,
        /// Boxed to reduce the size difference between enum variants
        /// (`clippy::large_enum_variant`).
        specialized_input: Box<AnalyzerInput>,
        _spool_guard: Option<tempfile::NamedTempFile>,
    },
    Cancelled,
    TimeBudgetExhausted,
    IoFailure,
}
```

`specialized_input` is boxed so that the largest variant (`Ready`, 368 bytes) does
not dwarf the unit-like variants, satisfying `clippy::large_enum_variant` without
any runtime overhead (the box is immediately dereferenced with `*specialized_input`
at the match site in `dispatch()`).

#### Bounded spool strategy for `StreamingHandle`

`prepare_spool` handles `AnalyzerContent::StreamingHandle` with a fully bounded,
streaming-end-to-end approach:

1. A `tempfile::NamedTempFile` spool is created.
2. The `LargeFileStream` is consumed **chunk by chunk**; each chunk's `.redacted`
   field (Phase 1B-safe bytes) is written to the spool via `Write::write_all`.
   **Cancellation and time-budget are checked at the top of every iteration** —
   if either fires, the loop returns `SpoolPreparation::Cancelled` or
   `SpoolPreparation::TimeBudgetExhausted` immediately, without fabricating
   empty content.
3. Once all chunks are written and flushed, two independent
   `LargeFileStream::open(spool.path())` handles are opened — one for the
   specialized analyzer, one for `GenericAnalyzer` fallback.
4. The spool `NamedTempFile` is returned as `_spool_guard` and bound in
   `dispatch()` scope. It is dropped (and the temp file deleted) after **both**
   analyzers have finished, providing deterministic cleanup.
5. On I/O failure, `SpoolPreparation::IoFailure` is returned; the caller emits a
   `RESOURCE_EXHAUSTED` diagnostic — never an empty `AnalyzerOutput`.
6. Disk usage is bounded: at most one spool file per active dispatch call.

```rust
// Inside the spool loop — cancellation + budget guard
loop {
    if input.cancellation_token.is_cancelled() {
        return SpoolPreparation::Cancelled;
    }
    if started_at.elapsed().as_millis() as u64 >= input.resource_budget.max_time_ms {
        return SpoolPreparation::TimeBudgetExhausted;
    }
    match stream.next_chunk() {
        None => break,
        Some(Err(_)) => return SpoolPreparation::IoFailure,
        Some(Ok(chunk)) => { spool.write_all(chunk.redacted.as_bytes())?; }
    }
}
```

**Why not `collect_all`?** The previous implementation used
`attic_discovery::secrets::collect_all(&mut stream)` to drain the entire
`LargeFileStream` into a heap `String` before splitting. This violated the O(1)
memory invariant for LARGE files. The spool strategy replaces it: the raw file
path is never reopened (preserving Phase 1B security), and peak heap usage
remains O(one stream chunk).

---

## 3. Resolved Implementation Blockers

| # | Blocker | Resolution |
|---|---|---|
| 1 | Error-diagnostic path returned specialized empty output instead of running GenericAnalyzer | `dispatch.rs`: `Ok(specialized_output)` arm now calls `GenericAnalyzer::new().analyze(fb_input)` |
| 2 | Streaming dispatch used `collect_all()` — O(file size) heap allocation for LARGE files | `prepare_spool`: `StreamingHandle` branch spools redacted chunks to `NamedTempFile`, opens two independent `LargeFileStream` handles; O(1) memory end-to-end |
| 3 | GenericAnalyzer carry buffer was unbounded | `MAX_CARRY_BYTES = 65_536` + `floor_char_boundary` splits in `stream_into_units` and `emit_possibly_oversized_line` |
| 4 | SourceSpan semantics were inconsistent | All spans use 0-based exclusive end throughout `build_retrieval_unit_from_lines`; verified by 5 span-specific tests |
| 5 | Spool loop did not check cancellation or time budget during spooling | Cancellation token and `elapsed >= max_time_ms` checked at each iteration; returns named variant instead of fabricated empty content |
| 6 | `SpoolPreparation::Ready.specialized_input` caused `clippy::large_enum_variant` (368 B vs 32 B) | `specialized_input: Box<AnalyzerInput>`; unboxed with `*specialized_input` at the match destructure site |

---

## 4. Test Coverage

**42 tests, 0 failures, 0 ignored**

| Module | Tests | What's covered |
|---|---|---|
| `cancellation` | 4 | new token not cancelled, cancel visible to clones, idempotent, default |
| `dispatch` | 15 | generic direct path, specialized success, **error→GenericAnalyzer fallback+units**, panic→fallback+units, terminal-safe fallback, IoFailure path; **10 streaming dispatch tests** |
| `generic` | 15 | empty file, LF spans, CRLF spans, unterminated line, multi-chunk split, redacted input, invalid UTF-8, cancellation, resource unit limit, resource memory limit, streaming units, **streaming bounded carry (no-OOM no-newline)**, **streaming 0-based spans**, floor_char_boundary UTF-8 split |
| `registry` | 8 | generic fallback, specialized registration, capability level ordering, deterministic tie-breaking, language-agnostic ignored, descriptors deduplicated/sorted |

#### Dispatch tests (15 total)

| Test | What it verifies |
|---|---|
| `dispatch_with_generic_registry_calls_generic_directly` | Generic path incurs no spool overhead |
| `dispatch_specialized_success_returns_output` | Specialized analyzer output returned as-is when no error diagnostics |
| `dispatch_specialized_fatal_error_runs_generic_fallback_and_produces_units` | Error diagnostics trigger `GenericAnalyzer` fallback; fallback units present |
| `dispatch_specialized_panic_caught_adds_panic_caught_and_fallback_used` | Panic in specialized path; `PANIC_CAUGHT` + `FALLBACK_USED`; units non-empty |
| `generic_analyzer_is_terminal_safe_fallback` | `GenericAnalyzer` used directly when registry has no specialized entry |
| `spool_io_failure_never_invokes_specialized_with_empty_content` | I/O failure during spool emits `RESOURCE_EXHAUSTED`; specialized is never called with fabricated empty content |
| `streaming_specialized_success_remains_bounded` | Specialized analyzer receives `StreamingHandle`; returns units without fallback |
| `streaming_specialized_error_falls_back_with_searchable_output` | Specialized error triggers `GenericAnalyzer(fb_stream)`; fallback units present |
| `streaming_specialized_panic_falls_back_with_searchable_output` | Panic in specialized streaming path; `PANIC_CAUGHT` + `FALLBACK_USED`; units non-empty |
| `streaming_dispatch_does_not_collect_entire_file_into_memory` | Memory usage stays O(chunk) — no `collect_all` |
| `streaming_spool_temp_file_cleaned_up_after_dispatch` | Temp file deleted after dispatch returns (Drop semantics verified) |
| `streaming_spool_cleaned_up_on_cancellation` | Temp file deleted even when spool is cancelled mid-stream |
| `streaming_cancellation_during_spool_returns_cancelled_diagnostic` | Cancellation mid-spool returns `CANCELLED` diagnostic; specialized never invoked |
| `streaming_time_budget_exhausted_during_spool_returns_resource_exhausted` | Time budget exceeded mid-spool returns `RESOURCE_EXHAUSTED`; specialized never invoked |
| `streaming_large_dispatch_bounded_and_secret_safe` | Large multi-chunk file spools correctly; output units match; redacted bytes only |

---

## 5. Key Constants

| Constant | Value | Location |
|---|---|---|
| `MAX_LINES_PER_CHUNK` | 500 | `generic.rs` |
| `MAX_CARRY_BYTES` | 65 536 (64 KiB) | `generic.rs` |

---

## 6. Security Invariants Maintained

- Analyzers never receive a file path for reopening. All content arrives through
  `AnalyzerContent`; `AnalyzerInput::path` is documented as "for logging/diagnostics only."
- Streaming fallback spools only `chunk.redacted` bytes — the Phase 1B-safe field.
  The original raw file path is never accessed inside `attic-analyzers`. No
  `std::fs::File::open` calls exist in this crate.
- The spool temp file is written from the `LargeFileStream` (which was already opened
  by Phase 1B via `LargeFileStream::open`). Re-opening the spool path inside
  `prepare_spool` is safe because the spool contains only redacted content.
- `#![forbid(unsafe_code)]` enforced workspace-wide; no `unsafe` blocks in this crate.
- `REDACTED_INPUT` diagnostic is emitted for all `RedactedBytes` inputs so callers
  know the retrieval units reflect sanitized content.
- Cancellation and time-budget are enforced **during** spool preparation, not only
  after — preventing unbounded blocking on adversarial or very large inputs.

---

## 7. Phase Gate Criteria

| Criterion | Status |
|---|---|
| `cargo clippy -p attic-analyzers --target x86_64-pc-windows-msvc -- -D warnings` | ✅ 0 warnings |
| `cargo test -p attic-analyzers --target x86_64-pc-windows-msvc` | ✅ 42/42 pass |
| `#![forbid(unsafe_code)]` | ✅ enforced |
| `SourceSpan` 0-based exclusive-end semantics | ✅ verified by 5 tests |
| Streaming carry bounded to `MAX_CARRY_BYTES` | ✅ `streaming_bounded_carry_no_oom` |
| Error-diagnostic path runs GenericAnalyzer | ✅ `dispatch_specialized_fatal_error_runs_generic_fallback_and_produces_units` |
| `StreamingHandle` dispatch is O(1) memory (no `collect_all`) | ✅ bounded spool strategy; 10 streaming dispatch tests |
| Spool uses only Phase 1B-safe (`chunk.redacted`) bytes | ✅ raw path never accessed in `attic-analyzers` |
| Spool temp file deleted deterministically after dispatch | ✅ `NamedTempFile` Drop in `dispatch()` scope |
| Cancellation + time budget enforced during spool loop | ✅ `streaming_cancellation_during_spool_returns_cancelled_diagnostic`, `streaming_time_budget_exhausted_during_spool_returns_resource_exhausted` |
| No fabricated empty content on spool failure | ✅ `spool_io_failure_never_invokes_specialized_with_empty_content` |
| `clippy::large_enum_variant` resolved | ✅ `SpoolPreparation::Ready.specialized_input: Box<AnalyzerInput>` |

---

## 8. Next Phase

Phase 1D (MCP + Full-Text Search) may proceed. The `attic-analyzers` crate provides
a stable, tested interface:

```rust
// Primary entry point
pub fn dispatch(registry: &AnalyzerRegistry, input: AnalyzerInput) -> AnalyzerOutput

// Fallback always available
pub struct GenericAnalyzer;
impl Analyzer for GenericAnalyzer { ... }

// Registry
pub struct AnalyzerRegistry;
impl AnalyzerRegistry {
    pub fn new(generic: Arc<dyn Analyzer>) -> Self;
    pub fn register_specialized(&mut self, analyzer: Arc<dyn Analyzer>);
    pub fn select(&self, file_type: FileType) -> (Arc<dyn Analyzer>, bool);
}
