# Phase 2 Completion Report — Incremental Correctness and Freshness

**Date:** 2026-08-25
**Gate status: PHASE 2 COMPLETE — awaiting user approval before Phase 3.**

---

## Dependencies added and verified

| Dependency | Version | Verification |
|---|---|---|
| `notify-debouncer-full` | `0.7` (pulls `notify` 8.2.x stable) | crates.io API 2026-08-25: debouncer stable = 0.7.0 (2026-01-23); notify stable = **8.2.0**, 9.0.0 rejected (RC only); non-optional dep of debouncer; MSRV 1.88 = exact workspace match. See ADR-008. |
| `blake3` (added to attic-indexing) | workspace 1.8.7 | Already approved in Phase 1B; reused for content-only change detection. |

No other new dependencies. `crossbeam-channel`/`flume` features stay off.

## Watcher / backend decision

`notify-debouncer-full::new_debouncer` (recommended backend: ReadDirectoryChangesW
on Windows, FSEvents on macOS, inotify on Linux). Recursive watch native on
Windows/macOS, emulated per-directory on inotify. Rename pairing treated as a
**hint only** (stitching not guaranteed cross-platform). Backend errors arrive
via `DebounceEventResult::Err` → watcher-errors counter + reconciliation.
Documented in **ADR-008**.

## ADRs created

- **ADR-008** — watcher dependency choice + verification record.
- **ADR-009** — migration `0003_phase2.sql` (`core_identity_links`, task dedup
  indexes), Phase-2-only task framework scope, rename identity policy,
  freshness at the read boundary.

## Event normalization behavior

`attic-incremental::events`: notify batches → repo-relative normalized events
(`Created | Modified | Removed | RenamedFrom | RenamedTo | Other`); directory
and out-of-root paths dropped; early ignore/security filter drops `.git/**`,
parent escapes, build/vendor noise (`target/`, `node_modules/`, …) **before any
queueing**.

## Debounce / coalescing behavior

`coalesce.rs`: bounded BTreeMap pending state (capacity 8192), injected clock
(fully deterministic), per-path collapse of modify storms, create→delete
pre-flush vanish, delete→recreate → single Upsert, rename From/To pairing
within a window with unpaired degradation to Remove/Upsert hints. Overflow sets
a flag consumed by the service → `reconciliation_required`.

## ChangeSet model

`changeset.rs`: hints verified against actual BLAKE3 content vs persisted
occurrence snapshots (never timestamps) → `VerifiedChangeSet { upserts,
deletes, renames, policy_changed }`; no-op touches dropped; identical-content
delete+add pairs promoted to rename records; `.gitignore` /
`.attic-policy.json` flagged as discovery-policy inputs.

## Invalidation DAG implementation

`invalidation.rs` + storage `invalidation_ops.rs`: one coordinated writer
transaction marks occurrence STALE (modify) / INVALID (delete) and propagates
→ structural nodes, symbol occurrences, relationships (schema-complete for
Phase 3+), retrieval units, knowledge items → INVALID; evidence → STALE
(INV-Q2). Every touched artifact gets a `core_invalidation_records` row;
`close_pending_records_for_occurrence` closes them after successful
republication. SemanticRepr intentionally untouched (Phase 5).

## Freshness state machine

Contract values CURRENT / STALE / UNKNOWN / INVALID / PENDING_REFRESH enforced
by `freshness.rs` transition table (INVALID never → CURRENT directly;
self-transitions idempotent). `FreshnessState` enum in attic-core now matches
the migration's canonical values. FTS read boundary serves only
CURRENT/STALE/PENDING_REFRESH units on non-deleted occurrences — UNKNOWN and
INVALID never surface; search results carry explicit `freshness_state`.

## Incremental recomputation behavior

`attic-indexing::incremental::index_changes`: scoped reindex of exactly the
changed paths reusing Phase 1B preprocessing → Phase 1C dispatch → ONE
`submit_index_publication` mutation (old units deleted + new inserted +
tombstones atomically, FTS synchronized in-transaction). Manifest hash is
incremental: trusted stored hashes for unchanged files + fresh hashes for
changed files (`manifest_hash_from_pairs`). No second indexing architecture; a
one-file edit does not touch other files' rows.

## Task scheduler behavior

`scheduler.rs` over durable `ops_tasks`: idempotent enqueue (payload dedup),
atomic claim via writer queue (`priority DESC, created_at ASC, id`),
bounded pending depth (saturation ⇒ UNKNOWN marking + RECONCILIATION task,
never silent loss), retry via `retry_count/max_retries`, checkpoint_json
progress, graceful shutdown that leaves PENDING work durable, cancellation of
PENDING tasks, plus `run_next_task_synchronously` deterministic driver.

## Cancellation / shutdown behavior

Covered by tests: cancelled-before-execution tasks land CANCELLED and never
execute; graceful shutdown joins workers within a deadline and preserves
queued work; RUNNING rows interrupted by a crash reset to PENDING at startup.

## Checkpoint / recovery implementation

`recovery.rs` startup procedure (idempotent): RUNNING tasks → PENDING;
RUNNING `ops_indexing_log` → ABANDONED; PENDING_REFRESH occurrences → STALE
(rescheduled); IN_PROGRESS secret scans → PENDING; watcher epoch bumped in
`ops_server_state`. Offline refresh planner enqueues tasks for every non-CURRENT
occurrence. Authoritative reconciliation walk diffs disk vs DB and emits a
verified ChangeSet applied via `apply_verified_change_set` (bypasses per-hint
disk verification — required for still-on-disk policy exclusions).

## Crash / power-loss handling

Crash points covered by construction + tests: uncommitted publication → WAL
rollback (atomic writer batches); crash between invalidation and recomputation
→ units hidden, occurrence observable-STALE, task survives; dead-writer scoped
publication → full rollback, previous coherent state intact; interrupted task →
reschedule. Recovery run twice/thrice converges (tested).

## FTS incremental consistency

Proven by suite: add → searchable; modify → old text purged + new searchable;
delete → no ghost + tombstone INVALID; rename → old path gone, new path
searchable; failed publication → previous coherent state; external-content
'delete' protocol inside the same transaction as base-row removal.

## `.gitignore` / policy reconciliation

`.gitignore` modification schedules targeted RECONCILIATION rediscovery (no
blind rebuild). Real-git test (isolated GIT_CONFIG_GLOBAL/NOSYSTEM env)
verifies newly-ignored files vanish from FTS; Attic-policy exclusion test
verifies the same via `GlobRule`; newly-included files are indexed by
reconciliation.

## MCP / status changes

- `status`: adds `incremental { state: CURRENT|INDEXING|RECONCILIATION_REQUIRED|
  UNKNOWN, events_ingested, hints_dropped, watcher_errors,
  reconciliation_required, freshness totals, task counts }`.
- `search`: results carry `freshness_state`; INVALID/UNKNOWN/deleted excluded.
- `file`: annotates responses with `[index freshness: …]` when the latest
  occurrence is not CURRENT (body remains live authoritative disk content).
- Server watch mode (`ATTIC_WORKSPACE_ROOT`): recovery → bootstrap → offline
  refresh scheduling → scheduler threads + watcher pump; clean-shutdown marker
  recorded. No Phase 4 routing/Evidence Manager behavior added.

## Tests added (31 new)

Unit: ops_tasks (4), server_state (1), invalidation_ops (3), identity_links
(1), freshness (4), events (2), fts freshness (existing extended).
Integration `phase2_lifecycle` (9): modification, creation, deletion/no-ghost,
rename+identity-link, rename+modify (uncertain stays uncertain), rapid-modify
storm collapse, create+delete pre-debounce, delete+recreate, duplicate bursts.
Integration `phase2_policy_freshness` (9): .gitignore triggers rediscovery,
real-git .gitignore removes newly-ignored from FTS, policy exclusion removes
from FTS, newly-included indexed, knowledge-file modification isolation,
unaffected repository untouched, invalidation visibility + refresh restores
CURRENT, event-storm boundedness, saturation → UNKNOWN + reconcile.
Integration `phase2_recovery` (7): interrupted-task restart, crash-gap between
invalidation and recomputation, atomic rollback on dead writer, offline source
drift caught by reconciliation, repeated-restart idempotency, cancellation,
graceful shutdown with pending work.

All tests: temp dirs, no network/home/global-git/machine paths, explicit
deterministic time budgets, cleanup of processes/watchers/DBs/temp repos.

## Exact commands executed (local target `x86_64-pc-windows-msvc`, never committed)

```text
cargo check --workspace --target x86_64-pc-windows-msvc
cargo fmt --all && cargo fmt --all -- --check        # clean
cargo clippy --workspace --all-targets --all-features \
  --target x86_64-pc-windows-msvc -- -D warnings     # clean
cargo test  --workspace --target x86_64-pc-windows-msvc
```

## Results

| Suite | Result |
|---|---|
| attic-analyzers | PASS (45) |
| attic-core | PASS (13) |
| attic-discovery | PASS (145) |
| attic-evidence | PASS (1) |
| attic-incremental unit | PASS (6) |
| phase2_lifecycle | PASS (9) |
| phase2_policy_freshness | PASS (9) |
| phase2_recovery | PASS (7) |
| attic-indexing | PASS (16) |
| attic-retrieval | PASS (1) |
| attic-server (+rmcp stdio integration) | PASS (54 + 2) |
| attic-storage | PASS (62) |
| **Total** | **PASS — 371 tests, 0 failures** |

fmt: PASS · clippy `-D warnings`: PASS · check: PASS.

## Hangs / timeouts encountered

None. One transient PowerShell environment issue (`cargo`/`rg` not on PATH →
invoked cargo via absolute path; heredoc unsupported → used Write tool). No
process kills were necessary; no endpoint-security events occurred.

## Open questions

- OQ-008 (knowledge ingest pipeline): Phase 2 note added — freshness for
  knowledge files works through the standard pipeline; table population/taxonomy
  decision remains OPEN.
- OQ-017 (new): Git-tracked EXACT rename basis deferred until Git plumbing is
  introduced; schema ready.
- OQ-018 (new, RESOLVED): tombstone occurrences pinned to INVALID freshness.

## Known limitations

1. Rename continuity is HEURISTIC/content-based (ADR-009); Git blob-SHA EXACT
   renames await Git plumbing (OQ-017).
2. `core_knowledge_items` is not yet populated by any analyzer (pre-existing
   Phase 1D scope boundary); knowledge-file changes flow through retrieval-unit
   freshness instead.
3. Reconciliation walk hashes every eligible file (authoritative but O(files));
   bounded per-run, triggered only on watcher errors, overflow, saturation, or
   policy change — not on ordinary edits.
4. Process-level kill/restart is exercised at DB/WAL + task-state level rather
   than by killing a live OS process mid-write (deterministic requirement);
   the atomic-publication invariant makes these equivalent for the covered
   crash classes.

## Gate

**PHASE 2 GATE: MET** — no full-workspace rebuild for ordinary edits; no
deleted/stale/unknown/invalid content served as CURRENT; recovery idempotent.

**STOPPING here per instruction. Phase 3 requires explicit approval.**
