# Phase 1C Completion Report — Analyzers Foundation

**Date:** 2026-08-25  
**Target:** `x86_64-pc-windows-msvc`  
**Crate:** `attic-analyzers v0.1.0`

---

## Summary

Phase 1C delivers the analyzers foundation for the Attic indexing pipeline.  
All bounded-spooling, cancellation, resource-budget, fallback, and provenance invariants are implemented and validated.

---

## Gate Results

| Check | Result |
|-------|--------|
| `cargo clippy -p attic-analyzers --target x86_64-pc-windows-msvc -- -D warnings` | ✅ 0 warnings |
| `cargo test -p attic-analyzers --target x86_64-pc-windows-msvc` | ✅ 45 / 45 passed |
| `#![forbid(unsafe_code)]` | ✅ enforced |
| `#![deny(clippy::all)]` | ✅ enforced |

---

## Deliverables

### `crates/attic-analyzers/src/dispatch.rs`

Core dispatch logic with the following invariants:

#### Bounded Spooling (`spool_streaming`)
- `LargeFileStream` content is written chunk-by-chunk to a `tempfile::NamedTempFile`; the entire file is never held in memory simultaneously.
- Spool temp file is cleaned up deterministically via RAII drop on every exit path (success, cancel, budget exhaustion, I/O failure).

#### `SpoolPreparation` Enum
All four variants carry `file_occurrence_id: FileOccurrenceId`:

| Variant | Meaning |
|---------|---------|
| `Ready` | Spool written; analyzer may proceed |
| `Cancelled` | Cancellation token fired during spool |
| `TimeBudgetExhausted` | Wall-clock budget exceeded during spool |
| `IoFailure` | Any spool create / write / flush / open error |

#### Provenance Invariant (the bug fix)
Prior to this phase, the three failure variants (`Cancelled`, `TimeBudgetExhausted`, `IoFailure`) constructed `AnalyzerOutput` with a freshly synthesized `FileOccurrenceId::new_v4()`, breaking the identity chain for downstream consumers.

**Fix:** every failure path in `spool_streaming` threads the original `file_occurrence_id` through to its `SpoolPreparation` payload, and every `dispatch()` match arm destructures and forwards that original id into the returned `AnalyzerOutput`.  No new identity is ever fabricated on failure.

#### Fallback Chain
- Specialized analyzer panic → caught via `std::panic::catch_unwind` → generic fallback runs; `PanicCaught` and `FallbackUsed` diagnostics emitted.
- Specialized analyzer fatal error → generic fallback runs; `FallbackUsed` diagnostic emitted.
- Generic analyzer is the terminal safe fallback; it never panics on well-formed input.

---

## New Tests Added (provenance)

Three new executable tests assert the provenance invariant across every preparation-failure path:

| Test | Failure path covered |
|------|---------------------|
| `cancellation_output_preserves_original_file_occurrence_id` | `SpoolPreparation::Cancelled` |
| `time_budget_exhausted_output_preserves_original_file_occurrence_id` | `SpoolPreparation::TimeBudgetExhausted` |
| `io_failure_output_preserves_original_file_occurrence_id` | `SpoolPreparation::IoFailure` |

Each test:
1. Mints a known `FileOccurrenceId` before constructing the `AnalyzerInput`.
2. Triggers the specific failure mode.
3. Asserts `output.file_occurrence_id == original_id`.

---

## Full Test Inventory (45 tests)

**`dispatch` module (19 tests)**
- `cancellation_output_preserves_original_file_occurrence_id` ✅ *(new)*
- `time_budget_exhausted_output_preserves_original_file_occurrence_id` ✅ *(new)*
- `io_failure_output_preserves_original_file_occurrence_id` ✅ *(new)*
- `dispatch_specialized_fatal_error_runs_generic_fallback_and_produces_units` ✅
- `dispatch_specialized_success_returns_output` ✅
- `dispatch_with_generic_registry_calls_generic_directly` ✅
- `dispatch_specialized_panic_caught_adds_panic_caught_and_fallback_used` ✅
- `generic_analyzer_is_terminal_safe_fallback` ✅
- `spool_io_failure_never_invokes_specialized_with_empty_content` ✅
- `streaming_cancellation_during_spool_returns_cancelled_diagnostic` ✅
- `streaming_dispatch_does_not_collect_entire_file_into_memory` ✅
- `streaming_spool_cleaned_up_on_cancellation` ✅
- `streaming_spool_temp_file_cleaned_up_after_dispatch` ✅
- `streaming_specialized_error_falls_back_with_searchable_output` ✅
- `streaming_specialized_panic_falls_back_with_searchable_output` ✅
- `streaming_specialized_success_remains_bounded` ✅
- `streaming_time_budget_exhausted_during_spool_returns_resource_exhausted` ✅
- `streaming_large_dispatch_bounded_and_secret_safe` ✅

**`generic` module (16 tests)** — all existing, all passing ✅

**`registry` module (9 tests)** — all existing, all passing ✅

**`cancellation` module (3 tests)** — all existing, all passing ✅

---

## Stopping Condition

Per project directive: **STOP in Phase 1C.**  
Phase 1D (`attic-indexing` / MCP / FTS) is not started.

---

## Files Modified

| File | Change |
|------|--------|
| `crates/attic-analyzers/src/dispatch.rs` | Provenance fix + 3 new provenance tests + helper `make_streaming_input_with_id` |
| `docs/PHASE_1C_COMPLETION_REPORT.md` | This file |
