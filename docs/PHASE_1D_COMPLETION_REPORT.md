# Phase 1D Completion Report — MCP Server & Full Indexing Pipeline

**Date:** 2026-08-25  
**Status:** COMPLETE — all 11 requirements fulfilled, 280 tests pass (0 failures)

---

## Overview

Phase 1D adds the MCP stdio server and wires the complete indexing pipeline
end-to-end. All work is confined to Phase 1D scope; Phase 2 has not been
started.

---

## Requirements Fulfilled

| # | Requirement | Status |
|---|-------------|--------|
| 1 | Real `file` tool via Phase 1B security/safe-content APIs | ✅ |
| 2 | True MCP stdio integration test | ✅ |
| 3 | Real hashes (FNV-1a × 4 seeds) — no fake provenance | ✅ |
| 4 | Indexing routes through approved Phase 1A storage APIs | ✅ |
| 5 | No direct rusqlite mutations in MCP handlers | ✅ |
| 6 | DbPool concurrent-reader design (not `Arc<Mutex<Connection>>`) | ✅ |
| 7 | Absolute DB paths / repo roots never exposed in normal responses | ✅ |
| 8 | Explicit bounds/validation: query length, file paths, result counts | ✅ |
| 9 | Client-visible errors sanitized; internals logged via `tracing` | ✅ |
| 10 | Minimum indexing lifecycle wired (discovery → storage → FTS) | ✅ |
| 11 | Executable end-to-end integration tests | ✅ |

---

## What Was Built

### 1. MCP Stdio Server (`crates/attic-server`)

JSON-RPC 2.0 over stdio using the `rmcp` crate. Three MCP tools are exposed:

| Tool | Description |
|------|-------------|
| `search` | Full-text search via storage FTS, ranked results |
| `lookup_file` | Path-based file occurrence lookup |
| `file` | Returns sanitized file content using Phase 1B `preprocess_file_content` |

The `file` tool calls `preprocess_file_content(abs_path, repo_relative)` and
returns the `content` field. Files where Phase 1B returns
`content = None, stream = Some(…)` (LARGE classification) are refused with a
stable MCP error — streaming is deferred to Phase 2.

### 2. Real Content Hashes — No Fake Provenance

- FNV-1a hashed with four independent seeds, results concatenated to produce a
  64-character hex string.
- All content hashes carry the `"fnv:"` prefix for namespace clarity.
- Manifest hash sourced from Phase 1B discovery output
  (`manifest.manifest_hash`, a real 64-char BLAKE3 hex string).
- No hand-crafted or static test-only hashes reach production code paths.

### 3. Approved Storage API Routing — No Raw SQL Mutations

`attic-indexing` calls only approved Phase 1A storage APIs in the correct
order:

```
upsert_repository
insert_source_revision_with_hashes
insert_index_generation
upsert_file_identity
insert_file_occurrence
publish_file_batch
insert_retrieval_unit_with_fts     (per file)
delete_retrieval_units_for_file    (refresh path only)
```

The only SQL that remains in `attic-indexing` itself is:

- A `BEGIN IMMEDIATE` / `COMMIT` transaction wrapper (write-side fence).
- Read-only `SELECT` queries to look up existing `core_file_occurrences` rows
  for refresh logic — these do not mutate the schema.

### 4. DbPool Concurrent-Reader Design

```rust
impl AtticServer {
    fn new(db_path: &std::path::Path) -> Result<Self, Box<dyn std::error::Error>> {
        let (writer_conn, pool) = open_db(db_path)?;   // only valid constructor
        run_migrations(&writer_conn)?;
        Ok(Self {
            pool,
            _writer: Arc::new(Mutex::new(writer_conn)),
        })
    }
}
```

- `open_db(path)` returns `(Connection, DbPool)` — the only way to obtain a
  `DbPool`; `DbPool::new` is private.
- All read paths use `self.pool.with_reader(|conn| { … })`.
- The writer connection is held in `_writer` solely to keep the WAL writer
  alive for the lifetime of the server; MCP handlers never lock it.

### 5. Security — No Path Leakage

```rust
const MAX_QUERY_LEN: usize = 1_024;
const MAX_PATH_LEN:  usize = 4_096;
const MAX_RESULTS_HARD_CAP: usize = 200;
```

- Absolute paths (`/`, `C:\`) and `..` traversal sequences are rejected at
  validation time with a stable `"Invalid path"` error.
- DB file path and repository root are never included in any MCP tool response.
- `tracing::error!` is used for internal diagnostic messages; clients receive
  only `"Internal error — see server logs"`.

### 6. Minimum Indexing Lifecycle

The complete pipeline runs in a single `index_repository` call:

```
discover()            → DiscoveryOutput (manifest, file list)
preprocess_file()     → PreprocessResult { decision, content, stream, findings }
run_analyzers()       → Vec<Evidence>
storage write APIs    → DB rows + FTS units
```

LARGE files (`content = None`) are skipped; a `tracing::debug!` record is
emitted. Second-run refresh correctly deletes stale retrieval units before
inserting new ones (see Bug Fixes below).

---

## Files Modified

### `crates/attic-server/src/main.rs`

- `AtticServer::new()` rewritten to use `open_db(db_path)` pattern.
- Input validation constants added.
- Path-security check added to `file` and `lookup_file` handlers.
- Error sanitization: all `?` propagation from pool readers converts to a
  logged + stable MCP error before returning to the client.
- 15 tests added / fixed; all pass.

### `crates/attic-indexing/src/lib.rs`

- `FileRecord` struct gains `old_fo_id: Option<String>` field.
- Pre-insert lookup uses `fo.rowid DESC` (the `created_at` column does not
  exist in `core_file_occurrences`).
- `delete_retrieval_units_for_file` is called with `old_fo_id`, not the
  newly-inserted `fo_id`.
- FNV-1a × 4 content hash generation replaces all placeholder values.
- Real manifest hash threaded through from Phase 1B output.
- LARGE file skipping with `tracing::debug!` trace.
- 11 integration tests; all pass.

### `migrations/0002_phase1d.sql`

- Schema additions required by Phase 1D tooling.

---

## Bug Fixes

### `second_index_run_refreshes_units` — units_deleted was 0

**Root cause.**  
`delete_retrieval_units_for_file` was called with the *newly* inserted
`fo_id`, which had no retrieval units attached yet. The delete was a no-op, so
stale units from the first run survived.

**Fix.**  
Capture the existing `fo_id` from `core_file_occurrences` **before**
inserting the new occurrence, then pass that `old_fo_id` to the delete call:

```rust
let old_fo_id: Option<String> = conn
    .query_row(
        "SELECT fo.id
           FROM core_file_occurrences fo
           JOIN core_file_identities  fi ON fo.file_identity_id = fi.id
          WHERE fi.repository_id = ?1 AND fo.path = ?2
          ORDER BY fo.rowid DESC
          LIMIT 1",
        rusqlite::params![repo_id.to_string_repr(), entry.repo_relative],
        |r| r.get(0),
    )
    .optional()?;
```

### `no such column: fo.created_at`

**Root cause.**  
The old_fo_id lookup originally ordered by `fo.created_at`, which is not a
column in the `core_file_occurrences` schema.

**Fix.**  
Order by `fo.rowid DESC` instead — `rowid` is always present in SQLite and
monotonically increases with insert order.

### `WriterQueueHandle::open()` / `DbPool::new(path, n)` compile errors

**Root cause.**  
Neither API exists: `WriterQueueHandle::open()` is absent from the codebase;
`DbPool::new` is private (only callable inside `connection.rs`).

**Fix.**  
Use the only valid entry point: `open_db(path) -> (Connection, DbPool)`.

---

## Test Results

| Crate | Tests | Result |
|-------|------:|--------|
| attic-analyzers | 45 | ✅ all pass |
| attic-core | 13 | ✅ all pass |
| attic-discovery | 145 | ✅ all pass |
| attic-indexing | 11 | ✅ all pass |
| attic-retrieval | 1 | ✅ all pass |
| attic-server | 15 | ✅ all pass |
| attic-storage | 50 | ✅ all pass |
| **Total** | **280** | **✅ 0 failures** |

---

## Known Limitations (Deferred to Phase 2)

| Limitation | Reason deferred |
|------------|----------------|
| LARGE file streaming via `LargeFileStream` | Phase 1D scope ends at text content; streaming chunked retrieval is a Phase 2 retrieval concern |
| Incremental re-indexing (file-level change detection) | Phase 2 incremental pipeline spec |
| Cross-repository search | Phase 6 scope |
| Semantic / embedding-based search | Phase 5 scope |

---

## Architecture Decisions

### Why `open_db()` is the sole entry point for `DbPool`

`DbPool::new` is intentionally private inside `connection.rs`. This enforces
the invariant that every `DbPool` is paired with exactly one writer `Connection`
that performed migrations. Callers cannot accidentally create a reader-only pool
against an un-migrated database file.

### Why the writer connection is held in `_writer` and never locked by handlers

The WAL writer connection must remain open to prevent SQLite from reclaiming the
WAL file. Keeping it in `_writer: Arc<Mutex<Connection>>` satisfies the lifetime
requirement without exposing it to request handlers. All handler reads go through
`DbPool::with_reader`, which issues connections from a connection pool and
supports true concurrent reads under SQLite WAL mode.

### Why `fo.rowid DESC` instead of a timestamp column

The `core_file_occurrences` table was designed without a `created_at` column
(insertion order is implied by `rowid`). Using `rowid DESC` is semantically
equivalent, is always available in SQLite without a schema migration, and avoids
clock-skew issues in tests that insert multiple rows rapidly.

### Why FNV-1a × 4 seeds for content hashes

BLAKE3 is used for the manifest hash (Phase 1B, file-system level). For
content-level identity inside the DB, FNV-1a with four independent seeds
provides a 64-char hex fingerprint that is fast (no crypto overhead), has
negligible collision probability at file-corpus scale, and can be computed
entirely in stable Rust without additional dependencies.

---

## Phase Gate

Phase 1D is complete. The following gates are satisfied:

- [x] All 11 explicit requirements implemented with real code (not stubs).
- [x] 280 tests pass, 0 failures across the workspace.
- [x] No fake hashes, no hardcoded provenance.
- [x] No raw SQL mutations in MCP handlers or indexing write path.
- [x] No absolute paths or DB paths leaked to MCP clients.
- [x] Input validation and error sanitization in place.
- [x] End-to-end integration tests executable via `cargo test`.
- [x] Completion report written after behaviors implemented (not before).

**Phase 2 has not been started.**
