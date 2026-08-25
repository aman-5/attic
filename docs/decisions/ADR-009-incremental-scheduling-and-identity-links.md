# ADR-009: Phase 2 Incremental Scheduling State and Identity Links

**Status:** Accepted
**Date:** 2026-08-25
**Phase:** 2 (Incremental Correctness and Freshness)
**Depends on:** contracts `identity.md`, `invalidation.md`, `recovery.md`

## Context

Phase 2 needs (a) durable, crash-recoverable task state for incremental
recomputation, (b) an explicit record of cross-revision file-identity links so
rename continuity confidence is observable, and (c) freshness filtering at the
FTS read boundary.

## Decision

### 1. Migration `0003_phase2.sql`

Adds, idempotently:

- `core_identity_links` — one row per observed cross-revision file-identity
  continuation:
  `(id, repository_id, from_identity_id, to_identity_id, prior_path, new_path,
    confidence EXACT|HEURISTIC|NONE, basis GIT_RENAME|CONTENT_MATCH|NONE,
    created_at)`.
  Identity rows themselves are never mutated (identity contract invariant 6:
  "linking occurrences across revisions … does not mutate identity records").
- Partial index `idx_tasks_pending_dedup ON ops_tasks(task_type, repository_id)`
  `WHERE state = 'PENDING'` for idempotent enqueue dedup.
- Partial index `idx_tasks_type_state ON ops_tasks(task_type, state)`.

No existing column or table is altered.

### 2. Task framework scope (Phase 2 only)

Tasks live exclusively in `ops_tasks` (already defined in migration 0001).
Task types used: `INCREMENTAL_INDEX`, `RECONCILIATION`. States used exactly as
defined there: `PENDING | RUNNING | DONE | FAILED | CANCELLED`.
Claim/dedup/retry/cancel/checkpoint semantics are implemented in
`attic-storage::ops_tasks` and orchestrated by `attic-incremental`. No adaptive
scheduling (Phase 7 concern).

### 3. Rename identity policy

The Phase 1D identity basis is `"<repository_id>/<repo-relative-path>"`, which
matches the identity contract's **default basis** ("path").  Mapping one
identity UUID to two concurrent path bases is not representable in
`core_file_identities` (PK = id, lookup by basis), and switching the global
basis scheme would be a forbidden Phase 1 redesign.  Therefore:

- A moved/renamed file produces a **new** path-based identity (contract
  default basis behaviour), and
- when the deletion of path A and creation of path B share an **identical
  BLAKE3 content hash** in the same verified ChangeSet (or an explicit paired
  rename was observed), a `core_identity_links` row is written with
  `confidence = HEURISTIC, basis = CONTENT_MATCH`, making the continuity
  explicit and observable without ever mutating identity rows.
- Any uncertain pairing produces a fresh identity and NO link; uncertainty is
  preserved and never silently promoted to exact.
- Git-tracked rename detection (blob-SHA basis, EXACT confidence) is deferred
  until Git plumbing is introduced; the link schema already accommodates it.

### 4. Freshness at the read boundary

`fts_search` gains a `freshness_state` projection (occurrence-level) and
excludes rows whose occurrence or retrieval unit is `INVALID`. `STALE` rows
remain searchable but carry explicit staleness metadata (invalidation contract
INV-Q1 resolution: always serve staleness metadata).

## Consequences

- Recovery is idempotent: all recovery updates are conditional state
  transitions that converge after repeated runs.
- Duplicate watcher bursts cannot create duplicate canonical mutations:
  dedup happens at enqueue AND at publication (old-unit delete + new insert in
  ONE writer transaction keyed by fresh UUIDs).
