# ADR-001 — WAL Checkpoint Trigger Policy

**Status**: ACCEPTED  
**Date**: 2026-08-24  
**Resolves**: OQ-006  
**Relevant contracts**: `docs/contracts/storage.md` §WAL, `docs/contracts/recovery.md` §6 (REC-B1 through REC-B4)

---

## Context

`docs/contracts/recovery.md` REC-B3 specifies checkpoint frequency:

> Checkpoint frequency: every 1,000 WAL frames OR every 5 minutes, whichever comes first.

OQ-006 asked three questions:
1. What triggers the checkpoint — time-based, size-based, or both?
2. Who initiates it — the DB writer task, a maintenance task, or SQLite's automatic checkpointing?
3. How does REC-B1 (checkpoint before backup copy) interact with the policy?

---

## Decision

### 1. SQLite automatic checkpointing (frame-count trigger) — Phase 1A

Set `PRAGMA wal_autocheckpoint = 1000` on every opened connection.  SQLite's
default is already 1,000 pages; this PRAGMA makes the intent explicit and
documents it in code.  After any write transaction commits that pushes the WAL
to ≥ 1,000 frames, SQLite attempts a PASSIVE checkpoint automatically.

PASSIVE checkpointing does not block readers or the writer; it transfers
frames from the WAL to the main database file only when no reader holds a WAL
snapshot.  This satisfies REC-B4 (backup writes must not block the main write
path).

**Phase 1A relies solely on this mechanism.**  No background checkpoint thread
is created or owned by any Phase 1A component.

### 2. Time-based checkpoint — deferred to a later phase

SQLite does not provide a time-based PRAGMA.  The 5-minute cadence from
REC-B3 will be implemented in a dedicated `CheckpointController` component in
a later phase.  That controller will:

- Own an explicitly cancellable background thread (or async task).
- Issue `PRAGMA wal_checkpoint(PASSIVE)` every 5 minutes via a writer-owned
  connection.
- Be shut down deterministically as part of the storage subsystem lifecycle.

**This is not a Phase 1A deliverable.**  The contract item REC-B3 is
partially satisfied by the autocheckpoint (frame-count trigger); the
time-based trigger is tracked as a Phase 2 requirement.

### 3. Explicit checkpoint before backup (REC-B1) — deferred to a later phase

The BACKUP maintenance task (Phase 2 scheduler) must call
`PRAGMA wal_checkpoint(FULL)` before copying the main database file.  A FULL
checkpoint waits for all readers to release their WAL snapshots and transfers
all frames, ensuring the main file is self-consistent at backup time.

This is not a Phase 1A schema change; it is an operational requirement on the
BACKUP task implementation.

### 4. Checkpoint mode summary

| Trigger | Mode | Responsible component | Phase |
|---------|------|-----------------------|-------|
| WAL reaches 1,000 frames | PASSIVE (autocheckpoint) | SQLite built-in | 1A ✓ |
| Every 5 minutes (time) | PASSIVE | `CheckpointController` | 2 |
| Before backup copy | FULL | BACKUP maintenance task | 2 |

### 5. No schema changes required

This decision requires no DDL changes to `migrations/0001_initial.sql`.
The checkpoint policy for Phase 1A is fully implemented by the
`PRAGMA wal_autocheckpoint = 1000` line in `attic-storage::connection::configure_connection`.

---

## Consequences

- Setting `wal_autocheckpoint = 1000` causes SQLite to *attempt* a PASSIVE
  checkpoint after each write transaction that pushes the WAL to ≥ 1,000
  frames.  This is a checkpoint *threshold*, not a hard upper bound: the WAL
  can grow beyond 1,000 frames when checkpoint progress is prevented — for
  example, by a long-running reader that holds an open WAL snapshot pinning
  older frames.
- PASSIVE checkpointing does not wait for readers; it skips frames that are
  still referenced by active readers and therefore will not stall the write
  path.  However, a long-lived reader that prevents checkpoint progress may
  cause the WAL to grow beyond the autocheckpoint threshold until that reader
  releases its snapshot.
- The 5-minute time-based trigger and the pre-backup FULL checkpoint are
  deferred; REC-B3 is partially satisfied until Phase 2.
- No background threads are leaked by the Phase 1A storage layer.
</parameter>

---

## Alternatives Rejected

- **Background 5-minute checkpoint thread in Phase 1A**: rejected because it
  created an unowned, infinite thread with no deterministic shutdown path.
  Phase 1A must not leak background threads.  The `CheckpointController`
  design deferred to Phase 2 will provide explicit ownership and lifecycle
  management.
- **Repurposing `STALE_EVICTION` task type as a checkpoint trigger**: rejected.
  `STALE_EVICTION` has a defined, separate semantics.  Inventing temporary task
  semantics for checkpoint scheduling obscures intent and creates technical
  debt.  Phase 2 will introduce a first-class `CHECKPOINT` task type or
  controller if needed.
- **TRUNCATE checkpoint always**: would block readers; violates REC-B4.
- **RESTART checkpoint on autocheckpoint**: would reset WAL position
  unnecessarily; PASSIVE is sufficient for normal operation.
- **Schema-based last-checkpoint tracking**: unnecessary overhead for Phase 1A;
  an in-process timestamp in the future `CheckpointController` is sufficient.
