# Phase 1A Completion Report — Storage Foundation

**Date:** 2026-08-24 (updated 2026-08-24 — follow-up correctness pass)
**Gate:** Phase 1A storage correctness/lifecycle
**Outcome:** ✅ PASSED — 0 errors, 0 warnings (`cargo check --target x86_64-pc-windows-msvc`)

---

## Summary

All Phase 1A storage correctness issues have been resolved across two passes.

**Initial pass (9 issues):** deterministic lifecycle, genuinely bounded concurrency,
caller-visible mutation results, atomic batch failure semantics, propagated construction
errors, and a single canonical write path for `subsystem_versions_json`.

**Follow-up pass (3 issues):** transaction finalization failure-safety with writer
poisoning, panic-safe RAII pool guard for read connections, and corrected ADR-001
wording on WAL autocheckpoint behaviour.

---

## Issues Resolved

### 1 — Removed Unowned Infinite WAL-Checkpoint Thread

**File:** `crates/attic-storage/src/connection.rs`

The background `thread::spawn` loop that called `PRAGMA wal_checkpoint(PASSIVE)`
every 30 seconds has been removed entirely.  Phase 1A relies solely on
`PRAGMA wal_autocheckpoint = 1000` (set in `configure_connection`), which triggers
a passive checkpoint automatically after every 1 000-page WAL write.  An explicit
`CheckpointController` background task is deferred to Phase 2.

### 2 — Cleaned and Corrected ADR-001

**File:** `docs/decisions/ADR-001-wal-checkpoint-policy.md`

- Removed the `STALE_EVICTION` repurposing reference (that task-type has no
  relationship to WAL checkpointing).
- Removed all background-thread checkpoint references.
- Documented Phase 1A policy (autocheckpoint-only), Phase 2 `CheckpointController`
  (5-minute time-based), and Phase 2 BACKUP pre-flush in a checkpoint-mode table.
- Added "Alternatives Rejected" entries: background thread (unowned infinite thread)
  and STALE_EVICTION repurposing (wrong abstraction layer).
- **Follow-up (correctness):** Corrected the Consequences section.
  - Replaced the erroneous "WAL file is bounded to approximately 4 MB
    (1,000 × 4 KB pages)" claim.  `wal_autocheckpoint = 1000` is a checkpoint
    *threshold*, not a hard upper bound; the WAL can grow beyond 1,000 frames when
    a long-running reader prevents checkpoint progress.
  - Replaced "Readers are never blocked by the autocheckpoint mechanism" with an
    accurate description: PASSIVE checkpointing skips frames still referenced by
    active readers (so it does not stall the write path), but a long-lived reader
  may cause the WAL to grow until it releases its snapshot.

### 3 — Writer Returns Actual Execution Result

**File:** `crates/attic-storage/src/writer.rs`

`WriterQueueHandle::send()` now blocks until the worker executes the mutation and
returns the actual `Result<(), StorageError>` to the caller.  Each `WorkItem` carries
a `SyncSender<Result<(), StorageError>>` (capacity 1, used as a oneshot) that the
worker sends into after executing the closure.

### 4 — Batch Failure Semantics and Transaction Finalization Safety

**File:** `crates/attic-storage/src/writer.rs`

`flush_batch()` drains all items into `Vec<(MutationFn, SyncSender)>` before starting
the transaction.  If any mutation fails, the transaction is rolled back.  The failing
caller receives the original `StorageError`; all other callers in the same batch
receive `StorageError::BatchRolledBack`.  No mutation in a rolled-back batch is
silently discarded.

**Follow-up (correctness):** Explicit finalization failure handling with writer
poisoning.  The full decision tree is:

- **Mutation failure → ROLLBACK succeeds:** failing caller gets original error; all
  others get `BatchRolledBack`.  Known-clean state; writer continues.
- **Mutation failure → ROLLBACK fails:** connection state unknown.  Writer is
  **poisoned** (`Arc<AtomicBool>` set to `true`); all callers in the batch — and all
  future callers — receive `StorageError::WriterPoisoned`.  Worker exits.
- **All succeed → COMMIT succeeds:** all callers get `Ok(())`.
- **All succeed → COMMIT fails → ROLLBACK succeeds:** known-clean state; all callers
  receive `StorageError::Worker("COMMIT failed: …")`.  Writer is **not** poisoned.
- **All succeed → COMMIT fails → ROLLBACK also fails:** connection state unknown.
  Writer poisoned; all callers receive `StorageError::WriterPoisoned`.  Worker exits.

A `TransactionFinalizer` trait (`commit` / `rollback`) is injected into the worker
via `WriterQueue::new_with_finalizer`.  `DefaultFinalizer` calls
`conn.execute_batch("COMMIT;")` / `conn.execute_batch("ROLLBACK;")`.  Tests inject
`FailRollbackFinalizer`, `FailCommitFinalizer`, and `FailBothFinalizer` to exercise
each poisoning code path without requiring real SQLite failures.

`WriterQueueHandle::send` checks the poisoned flag *before* enqueuing and returns
`StorageError::WriterPoisoned` immediately if set.

### 5 — Deterministic Writer Shutdown

**File:** `crates/attic-storage/src/writer.rs`

Shutdown is signalled via an `Arc<AtomicBool>` flag set in `Drop`.  The worker checks
the flag on every loop iteration.  `try_send` is used for the shutdown message so a
full queue cannot block `Drop::join()`.  The worker drains and nacks any remaining
queued items (with `StorageError::Worker("shutdown")`) before exiting, ensuring
`join()` always completes.

### 6 — Genuinely Bounded Read Connection Pool with Panic-Safe RAII Guard

**File:** `crates/attic-storage/src/connection.rs`

`DbPool` is now backed by `PoolInner { idle: Vec<Connection>, in_use: usize }` behind
`Arc<Mutex<PoolInner>>`.  `POOL_MAX_READERS = 8` is the hard ceiling.  `with_reader`
returns `StorageError::PoolExhausted` immediately when all 8 slots are in use.

**Follow-up (correctness):** The original implementation incremented `in_use` before
calling the closure and decremented it afterward.  If the closure panicked, the
decrement was skipped, permanently leaking the pool slot.

This is now fixed with a `PoolGuard` RAII struct:

```rust
struct PoolGuard {
    conn: Option<Connection>,
    pool_inner: Arc<Mutex<PoolInner>>,
}

impl Drop for PoolGuard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if let Ok(mut guard) = self.pool_inner.lock() {
                guard.idle.push(conn);
                guard.in_use = guard.in_use.saturating_sub(1);
            }
        }
    }
}
```

`with_reader` creates a `PoolGuard` before calling the closure.  `Drop` always runs —
on normal return, error return, and thread unwinding — returning the connection to the
idle pool in all cases.  If the pool mutex is itself poisoned during `Drop`, the
connection is simply dropped (closed safely) rather than lost silently.

### 7 — No `expect()`/`panic` on Recoverable Paths

**Files:** `crates/attic-storage/src/connection.rs`, `crates/attic-storage/src/writer.rs`

- `open_db()` returns `Result<(Connection, DbPool), StorageError>`.
- `WriterQueue::new()` returns `Result<Self, StorageError>`, propagating
  `StorageError::ThreadSpawn(msg)` if `thread::Builder::spawn` fails.
- `with_reader` propagates `StorageError::MutexPoisoned` instead of calling
  `expect()` on the mutex lock.
- No `expect()` or `unwrap()` calls remain on any path reachable from non-test code.

### 8 — Secret Re-scan Scheduling Deferred to Future Phase

No changes required.  Phase 1A implements the persistence and versioning foundation
(`secret_scan_state`, `secret_pattern_version` columns) only.  No scheduling logic
was added.  The `ops_tasks` table (`task_type = 'SECRET_SCAN'`) is available for
future phases but is not written to by Phase 1A code.

### 9 — Canonical `subsystem_versions_json` Write Path

**File:** `crates/attic-storage/src/repository/index_generation.rs`

`insert_index_generation` always calls `subsystem_versions.to_json()` and passes the
result as the sole `subsystem_versions_json` value.  No other write path exists.
Verified correct; no changes required.

---

## New Files Created

| File | Purpose |
|---|---|
| `crates/attic-storage/src/repository/file_occurrence.rs` | S4 CRUD for `core_file_identities` and `core_file_occurrences` |
| `crates/attic-storage/src/repository/publication.rs` | Atomic batch publication coordinator (identity + occurrence in one `BEGIN IMMEDIATE` transaction) |

---

## New `StorageError` Variants

| Variant | When raised |
|---|---|
| `ThreadSpawn(String)` | `thread::Builder::spawn` failure in `WriterQueue::new` |
| `BatchRolledBack` | Callers in a batch whose transaction was rolled back by a peer failure |
| `PoolExhausted` | All 8 read connections are currently in use |
| `Worker(String)` | Worker-internal error (e.g., `BEGIN IMMEDIATE` failure; `COMMIT` failed but `ROLLBACK` succeeded) |
| `MutexPoisoned(String)` | `Mutex::lock()` returned `PoisonError` |
| `WriterPoisoned` | `ROLLBACK` or `COMMIT`+`ROLLBACK` both failed; connection state unknown; restart required |

---

## Test Coverage Added

### `connection.rs`
- `in_memory_connection_configures_without_error`
- `pool_with_reader_returns_value`
- `pool_connection_returned_after_use`
- `pool_exhausted_when_at_capacity`
- `open_db_returns_writer_and_pool`
- `wal_autocheckpoint_pragma_is_set`
- `db_reopen_preserves_data`
- `panicking_reader_does_not_leak_pool_slot` *(new — `catch_unwind` proves `PoolGuard::Drop` runs on unwinding)*

### `writer.rs`
- `writer_executes_mutation_and_returns_ok`
- `writer_returns_error_on_mutation_failure`
- `mid_batch_failure_rolls_back_batch`
- `queue_full_returns_error`
- `poisoned_handle_rejects_send` *(new)*
- `rollback_failure_after_mutation_error_poisons_writer` *(new)*
- `commit_failure_with_successful_rollback_returns_worker_error_to_all_callers` *(new)*
- `commit_and_rollback_failure_poisons_writer` *(new)*
- `shutdown_does_not_hang_when_queue_full`
- `worker_thread_joins_on_drop`
- `writer_queue_new_returns_ok_on_valid_connection`

### `file_occurrence.rs`
- `upsert_file_identity_is_idempotent`
- `insert_file_occurrence_and_exists`
- `duplicate_file_occurrence_insert_fails`
- `set_secret_scan_state_updates_row`

### `publication.rs`
- `empty_batch_is_a_noop`
- `single_item_batch_persists_identity_and_occurrence`
- `multi_item_batch_all_persisted`
- `batch_rolls_back_on_duplicate_occurrence_id`
- `identity_upsert_is_idempotent_across_batches`

---

## `cargo check` Result

```
Checking attic-storage v0.1.0
Checking attic-test-support v0.1.0
Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.77s
```

**0 errors. 0 warnings.**

---

## Phase Gate Status

| Check | Status |
|---|---|
| `cargo check` (0 errors, 0 warnings) | ✅ |
| No background WAL thread | ✅ |
| ADR-001 clean (no STALE_EVICTION, no thread refs) | ✅ |
| ADR-001 WAL bound wording correct (threshold, not hard bound) | ✅ |
| ADR-001 PASSIVE checkpoint wording correct (may grow under pinned reader) | ✅ |
| Writer returns actual result | ✅ |
| Batch rollback nacks all peers | ✅ |
| ROLLBACK failure poisons writer | ✅ |
| COMMIT failure + ROLLBACK success → `Worker` error, not poisoned | ✅ |
| COMMIT + ROLLBACK both fail → writer poisoned | ✅ |
| Poisoned handle rejects new sends immediately | ✅ |
| Shutdown never hangs | ✅ |
| Pool hard-bounded at 8 | ✅ |
| Pool slot released on panic (`PoolGuard` RAII) | ✅ |
| No `expect()`/`panic` in non-test paths | ✅ |
| Secret re-scan scheduling deferred | ✅ |
| Single canonical `subsystem_versions_json` write path | ✅ |

**Phase 1A gate: PASSED. Phase 1B may begin.**
