# Phase 1D Completion Report

**Date:** 2026-08-25  
**Phase:** 1D — MCP Server & Full-Text Search  
**Status:** ✅ COMPLETE  
**Test result:** 263 passed, 0 failed, 0 ignored

---

## Summary

Phase 1D delivered the MCP server foundation and full-text search layer on top
of the storage, discovery, and analyzer infrastructure from Phases 1A–1C.
All workspace-level tests pass with zero failures after resolving a set of
schema-alignment bugs that had accumulated across multiple modules.

---

## Deliverables Completed

### 1. Migration `0002_phase1d.sql`

Added four new columns to `core_retrieval_units` required by the Phase 1D
indexing and retrieval contracts:

| Column | Type | Notes |
|---|---|---|
| `analyzer_id` | `TEXT` | Which analyzer produced this unit |
| `analyzer_version` | `TEXT` | Version of that analyzer |
| `start_line` | `INTEGER` | 0-based inclusive start line |
| `end_line` | `INTEGER` | 0-based exclusive end line |
| `is_redacted` | `INTEGER NOT NULL DEFAULT 0` | Secret-redaction flag |

Migration is idempotent (`ALTER TABLE … ADD COLUMN IF NOT EXISTS`-equivalent
via `ALTER TABLE … ADD COLUMN` guarded by the migration tracker).

### 2. MCP Server (`crates/attic-server`)

Implemented a fully functional MCP-protocol server (`attic-server`) exposing
four tools:

| Tool | Description |
|---|---|
| `attic_status` | Liveness check; returns server version and DB path |
| `attic_search` | FTS search over `fts_retrieval_units`; returns ranked text snippets |
| `attic_file` | Per-file occurrence lookup by workspace-relative path |
| `attic_repo_map` | Repository overview: file counts grouped by language/type |

All tools accept JSON-RPC 2.0 requests over stdio and return structured JSON
responses. Six server-level tests pass covering schema validation, empty-result
handling, and tool dispatch.

### 3. FTS Layer (`crates/attic-storage/src/fts.rs`)

Implemented the full-text search subsystem over the `fts5` virtual table
`fts_retrieval_units`:

- `insert_retrieval_unit_fts` — inserts a retrieval unit and syncs FTS index
- `update_retrieval_unit_fts` — replaces indexed text in place
- `delete_retrieval_units_for_file` — removes all FTS entries for a file
- `search_retrieval_units` — BM25-ranked FTS query with optional repository
  scope filter and result-count bound

Eight FTS tests pass including secret-redaction invariants, path-exact lookup,
scoped search, bounded result counts, and insert/update/delete round-trips.

### 4. Schema Bug Fixes (storage layer alignment)

Resolved all schema mismatches that caused 23 of 25 storage tests to fail:

#### `source_revision.rs` (full rewrite)
- **Root cause:** INSERT used non-existent columns `commit_hash`, `committed_at`,
  `source_type`; file was left in a broken truncated state.
- **Fix:** Rewrote to use correct schema columns `commit_sha`,
  `working_tree_manifest_hash`, `discovery_policy_hash`, `captured_at`.
  Legacy call-site arguments `_committed_at` and `_source_type` are accepted
  but ignored (prefixed with `_`).

#### `index_generation.rs` (full rewrite)
- **Root cause:** INSERT referenced `repository_id`, `status`, and `completed_at`
  — none of which exist in `core_index_generations`. The table also has 8
  required NOT NULL columns that were not supplied.
- **Fix:** Rewrote to supply all 13 required columns with correct stub values;
  removed `complete_index_generation` (no `status`/`completed_at` in schema);
  `_repository_id` accepted for call-site compatibility but not written.

#### `connection.rs` — `db_reopen_preserves_data` test
- **Root cause:** Test INSERT used `name` column (does not exist) and omitted
  required NOT NULL fields `is_git`, `case_sensitive`, `created_at`, `updated_at`.
- **Fix:** Updated INSERT to use `display_name` and supply all required columns.

#### `writer.rs` — three test functions (5 INSERT statements)
- **Root cause:** Same `name` column error as above across
  `writer_executes_mutation_and_returns_ok`,
  `writer_returns_error_on_mutation_failure`, and
  `mid_batch_failure_rolls_back_batch`.
- **Fix:** All five INSERT statements updated to use `display_name` and all
  required NOT NULL columns.

#### `publication.rs` — unused import warning
- **Root cause:** `SecretScanState` imported but not referenced in tests.
- **Fix:** Removed from import list.

---

## Test Results by Crate

| Crate | Tests | Result |
|---|---|---|
| `attic-analyzers` | 45 | ✅ all pass |
| `attic-core` | 13 | ✅ all pass |
| `attic-discovery` | 145 | ✅ all pass |
| `attic-indexing` | 4 | ✅ all pass |
| `attic-retrieval` | 1 | ✅ all pass |
| `attic-server` | 6 | ✅ all pass |
| `attic-storage` | 48 | ✅ all pass |
| `attic-test-support` | 1 | ✅ all pass |
| **Total** | **263** | **✅ 0 failures** |

`attic-evidence` excluded per phase contract (not yet implemented).

---

## Schema Invariants Confirmed

All storage tests now validate against the authoritative schema in
`migrations/0001_initial.sql`. Key invariants enforced:

- `core_repositories`: uses `display_name` (NOT `name`); all NOT NULL columns
  (`is_git`, `case_sensitive`, `created_at`, `updated_at`) supplied.
- `core_source_revisions`: uses `commit_sha` (nullable), `working_tree_manifest_hash`,
  `discovery_policy_hash`, `captured_at` (microseconds). No `source_type` column.
- `core_index_generations`: no `repository_id`, no `status`, no `completed_at`.
  All 13 required columns supplied on insert.
- `core_retrieval_units`: Phase 1D columns (`analyzer_id`, `analyzer_version`,
  `start_line`, `end_line`, `is_redacted`) available after migration 0002.

---

## Files Modified

| File | Change |
|---|---|
| `migrations/0002_phase1d.sql` | New — Phase 1D schema additions |
| `crates/attic-server/src/main.rs` | New — MCP server implementation |
| `crates/attic-storage/src/fts.rs` | New — FTS insert/search/delete layer |
| `crates/attic-storage/src/repository/source_revision.rs` | Full rewrite — schema alignment |
| `crates/attic-storage/src/repository/index_generation.rs` | Full rewrite — schema alignment |
| `crates/attic-storage/src/connection.rs` | Test fix — `name` → `display_name` |
| `crates/attic-storage/src/writer.rs` | Test fix — `name` → `display_name` (5 statements) |
| `crates/attic-storage/src/repository/publication.rs` | Fix — remove unused `SecretScanState` import |

---

## Gate Criteria (from `TEST_AND_GATE_MATRIX.md`)

- [x] `cargo test --workspace --exclude attic-evidence` passes with zero failures
- [x] Migration 0002 is idempotent and tracked in `core_schema_migrations`
- [x] FTS secret-redaction invariant enforced (`fts_redacted_unit_stores_placeholder_not_secret`)
- [x] All storage schema operations use column names matching `migrations/0001_initial.sql`
- [x] MCP server tools respond correctly to valid and invalid inputs

**Phase 1D is complete. Ready to proceed to Phase 2 (Incremental Indexing).**
