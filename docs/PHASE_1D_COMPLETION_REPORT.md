# Phase 1D Completion Report — MCP Server & Full Indexing Pipeline

**Date:** 2026-08-25  
**Status:** COMPLETE — all Phase 1D blockers resolved. No Phase 2 work started.

---

## Overview

Phase 1D delivers the MCP stdio server (`rmcp` 3.1.4) and the complete
indexing pipeline wired end-to-end through **coordinated single-writer
publication**. This revision of the report reflects the *actual current
implementation* after the Phase 1D hardening pass; earlier drafts contained
stale claims (FNV hashing, LARGE-file skipping, mutexed-writer design) that no
longer describe the code and have been removed.

---

## Requirements Fulfilled

| # | Requirement | Status |
|---|-------------|--------|
| 1 | Real `file` tool via Phase 1B security/safe-content APIs | ✅ |
| 2 | Genuine official-rmcp-client ↔ server stdio integration gate | ✅ |
| 3 | Real hashes (BLAKE3 content + policy, BLAKE3 manifest) — no fake provenance | ✅ |
| 4 | Indexing writes execute through coordinated `WriterQueueHandle` publication — atomic, nested-transaction-free | ✅ |
| 5 | No direct rusqlite mutations anywhere outside approved storage APIs; `attic-indexing` accepts no raw write connection | ✅ |
| 6 | `DbPool` concurrent readers + dedicated `WriterQueue` worker (single writer) | ✅ |
| 7 | Absolute DB paths / repo roots never exposed in responses | ✅ |
| 8 | Checked conversions + explicit line/byte region and total response-size limits enforced **before** expensive work | ✅ |
| 9 | UTF-8-safe deterministic byte-offset semantics (floor to char boundary) | ✅ |
|10 | LARGE files genuinely streamed in bounded chunks (never accumulated) | ✅ |
|11 | Client-visible errors sanitized; internals logged via `tracing` | ✅ |

---

## What Was Built

### 1. Coordinated Writer/Publication Service (`attic-storage/src/indexing_publication.rs`)

The core Phase 1D write path. One indexing run submits **one**
[`submit_index_publication`] mutation through the Phase 1A
`WriterQueueHandle`. The submitted closure executes on the dedicated writer
thread inside the queue's ambient `BEGIN IMMEDIATE … COMMIT`, composing only
transaction-assuming primitives:

```
upsert_repository (when new)
insert_source_revision_with_hashes
insert_index_generation
upsert_file_identity + insert_file_occurrence   (per file)
delete_retrieval_units_for_file                 (refresh path)
insert_retrieval_unit_with_fts                  (per unit)
```

Properties:

- **No nested transactions.** `publish_file_batch` and `run_migrations` open
  their own `BEGIN IMMEDIATE` and are never invoked inside a queue closure;
  migrations run once at startup on the bootstrap connection *before* the
  `WriterQueue` is constructed.
- **Atomicity.** Any primitive failure rolls back the whole batch — verified
  by a rollback test asserting zero repository/revision/occurrence rows.
- **Result delivery.** Detailed stats (`IndexPublicationStats`) travel back
  through a shared slot; primitive errors propagate verbatim via the queue's
  batch-rollback error path.

### 2. Indexing Pipeline (`attic-indexing`)

`attic-indexing` **cannot receive a raw write connection** — its only entry
point takes an [`IndexingStore`]:

```rust
pub struct IndexingStore<'a> {
    pub readers: &DbPool,              // bounded read pool (Phase 1A)
    pub writer:  &WriterQueueHandle,   // coordinated writer (Phase 1A)
}
```

Flow per run:

1. `discover()` → real BLAKE3 manifest hash, git meta, downstream classifications.
2. Reads via approved accessors against committed state (`lookup_repository_by_root_path`,
   `lookup_file_identity_by_basis`, `lookup_latest_file_occurrence_for_path`).
3. Per-file analysis is pure: Phase 1B preprocessing → Phase 1C dispatch →
   pending units (LARGE files enter as `AnalyzerContent::StreamingHandle`).
4. Everything is published in one coordinated mutation (§1).

Content hashes come from the Phase 1B manifest (64-char BLAKE3 hex); the
policy hash is BLAKE3 over the canonical JSON of every `DiscoveryPolicy`
field. There is no FNV hashing anywhere in the pipeline. `rusqlite` remains
only a *dev-dependency* of `attic-indexing` for read-only verification
queries in tests.

### 3. MCP Stdio Server (`crates/attic-server`)

JSON-RPC 2.0 over stdio via `rmcp` 3.1.4 (`server` + `client` +
`transport-io` features). Tools: `file`, `search`, `repo_map`, `status`.

`bootstrap_workspace` reuses an existing repository row when present,
otherwise indexes through `IndexingStore` — there is **no** secondary SQLite
writer connection and no `open_rw()` bypass anywhere in the server.

A lifecycle bug found during this hardening pass was fixed: the previous
`main()` dropped the rmcp `RunningService` immediately, cancelling the
service right after the handshake. `main()` now parks on
`running.waiting().await` so the server lives until stdin closes.

### 4. `file` Tool — Bounded, UTF-8-Safe Retrieval

- **Checked parsing.** Every numeric argument goes through
  `parse_u64_arg`: missing/null → default; anything that is not a non-negative
  integer (negatives, floats, strings, >u64::MAX) is a stable client-visible
  error. Values above `MAX_REGION_VALUE = 2^48` are rejected outright — no
  `as` casts anywhere on the request path.
- **Limits enforced before work:**

  | Limit | Value |
  |-------|-------|
  | `MAX_LINE_SPAN` | 100 000 lines per window |
  | `MAX_BYTE_SPAN` | 8 MiB per window |
  | `MAX_RESPONSE_BYTES` | 1 MiB total response body |
  | `MAX_STREAM_SCAN_BYTES` | 64 MiB absolute scan bound |

- **UTF-8 semantics (deterministic):** byte offsets that do not land on a
  character boundary are floored DOWN (`floor_char_boundary`) — the partially
  addressed character is included at a start offset and excluded at an end
  offset; offsets past EOF clamp; inverted windows yield empty output. Slicing
  can never panic.
- **LARGE files (4–50 MiB)** stream through `StreamWindowCollector`, which
  consumes sanitized `LargeFileStream` chunks incrementally, emits only the
  requested window, stops pulling chunks once the window/cap is satisfied, and
  appends an explicit `[truncated: …]` marker. `collect_all` is not used; the
  complete file is never held in memory. Full-file requests are capped at
  `MAX_RESPONSE_BYTES`.

### 5. Security (unchanged Phase 1B/1D guarantees)

- Repo-relative paths validated; traversal rejected via
  `canonicalize_within_root`; `.git/**` blocked at the server layer.
- Absolute DB paths / repo roots never appear in any tool response.
- Excluded/Redacted decisions surface as policy messages; internal errors log
  via `tracing::error!` while clients receive sanitized text.

### 6. Integration Test Gates

- **Required:** `crates/attic-server/tests/rmcp_stdio_integration.rs` drives
  the spawned `attic` binary with the **official rmcp client stack**
  (`().serve((stdout, stdin))`), covering: negotiated peer info, `tools/list`,
  `tools/call status`, unknown-tool error content, and a full end-to-end flow
  (workspace auto-index → search → live `file` retrieval → bounded region).
  Every await is bounded by a 10 s timeout that kills the child process on
  expiry; a missing binary fails the gate (`CARGO_BIN_EXE_attic`) instead of
  silently passing.
- **Supplemental:** manual JSON-RPC protocol tests remain in `main.rs`, but
  their false-pass (`if !bin.exists() { return; }`) is removed —
  `require_binary()` panics when the binary cannot be located — and their
  handshake uses a supported protocol version (`2025-06-18`) with the required
  `capabilities` field.

---

## Bug Fixes During Hardening

| Symptom | Root cause | Fix |
|---------|-----------|-----|
| Server exited immediately after MCP handshake | `main()` dropped the rmcp `RunningService`, whose `Drop` cancels the service loop | Park on `running.waiting().await` |
| Manual MCP tests "passed" without ever running | `if !bin.exists() { return; }` false-pass + binary path missing `.exe` on Windows | `require_binary()` hard-fails; `.exe` suffix handled |
| Handshake rejected (`CustomRequest{method:"initialize"}`) | Legacy payload lacked required `capabilities` field / unsupported `2024-11-05` version string | Use `2025-06-18` + `capabilities:{}` |
| Nested-transaction hazard when routing writes through the queue | Batch primitives opened their own `BEGIN IMMEDIATE` inside the ambient transaction | Coordinated publication composes only transaction-assuming primitives |

---

## Files Modified

- `crates/attic-storage/src/indexing_publication.rs` — NEW coordinated publication service (+3 tests).
- `crates/attic-storage/src/lib.rs` — exports; `Cargo.toml` — tempfile dev-dep.
- `crates/attic-indexing/src/lib.rs` — full refactor onto `IndexingStore` + coordinated submission (+16 tests); `Cargo.toml` — rusqlite moved to dev-deps.
- `crates/attic-server/src/main.rs` — coordinated bootstrap, bounded streaming, UTF-8-safe regions, checked limits, fixed server lifetime, hardened supplemental tests.
- `crates/attic-server/tests/rmcp_stdio_integration.rs` — NEW required rmcp client gate.
- `crates/attic-server/Cargo.toml` — rmcp `client`, tokio `process`/`io-util`.
- `docs/PHASE_1D_COMPLETION_REPORT.md` — rewritten from actual implementation.

---

## Known Limitations (Deferred to Phase 2)

| Limitation | Reason deferred |
|------------|----------------|
| Incremental re-indexing (file-level change detection) | Phase 2 incremental pipeline spec |
| Cross-repository search | Phase 6 scope |
| Semantic / embedding-based search | Phase 5 scope |
| Explicit checkpoint/backup controller | ADR-001 §Future |

---

## Phase Gate

- [x] All blockers implemented with real code (no stubs, no false-passes).
- [x] Workspace indexing writes route exclusively through the coordinated writer/publication service.
- [x] `attic-indexing` has no production dependency on raw write connections or ad-hoc SQL.
- [x] LARGE-file retrieval is bounded; response sizes capped; numeric arguments overflow-safe.
- [x] Byte-region semantics are UTF-8-deterministic and panic-free.
- [x] Required rmcp client↔server stdio integration gate executable and failing loudly when unverifiable.
- [x] All Phase 1A–1C guarantees and endpoint-security rules preserved.
- [x] Full MSVC target validation executed (`cargo fmt --check`, `clippy -D warnings`, workspace tests).

**Phase 2 has not been started.**
