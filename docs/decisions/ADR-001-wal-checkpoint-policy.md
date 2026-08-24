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

### 1. SQLite automatic checkpointing (frame-count trigger)

Set `PRAGMA wal_autocheckpoint = 1000` on the writer connection at open time.
SQLite's default is already 1,000 pages; this PRAGMA makes the intent explicit
and documents it in code. When the WAL reaches 1,000 frames, SQLite will
attempt a PASSIVE checkpoint after any write transaction commits.

Passive checkpointing does not block readers or the writer; it transfers
frames from WAL to the main database file only when no reader holds a WAL
snapshot. This satisfies REC-B4 (backup writes must not block the main write
path).

### 2. Time-based checkpoint (5-minute trigger)

SQLite does not provide a time-based PRAGMA. The 5-minute cadence is enforced
by an `ops_tasks` entry of type `STALE_EVICTION` (repurposed until a dedicated
`CHECKPOINT` task type is defined in Phase 2) or, more accurately, via a
background loop in the DB Writer task that wakes every 5 minutes and calls
`PRAGMA wal_checkpoint(PASSIVE)` if no checkpoint has run in that window.

The last checkpoint timestamp is tracked in an in-process variable within the
DB Writer; it does not require a schema column for Phase 1A.

### 3. Explicit checkpoint before backup (REC-B1)

The BACKUP maintenance task (Phase 2 scheduler) must call
`PRAGMA wal_checkpoint(FULL)` before copying the main database file. A FULL
checkpoint waits for all readers to release their WAL snapshots and then
transfers all frames, ensuring the main file is self-consistent at backup
time.

This is not a Phase 1A schema change; it is an operational requirement on the
BACKUP task implementation.

### 4. Checkpoint mode

- **Writer idle (5-minute timer)**: `PRAGMA wal_checkpoint(PASSIVE)` — non-blocking.
- **Backup task**: `PRAGMA wal_checkpoint(FULL)` — blocks until readers drain; bounded by `busy_timeout`.
- **Automatic (autocheckpoint)**: PASSIVE (SQLite default for autocheckpoint).

### 5. No schema changes required

This decision requires no DDL changes to `migrations/0001_initial.sql`.
The checkpoint policy is implemented in `attic-storage::connection` module
logic and documented in this ADR.

---

## Consequences

- WAL file is bounded to approximately 4 MB (1,000 × 4 KB pages) under normal operation.
- Readers are never blocked by checkpointing.
- The BACKUP task must be implemented in Phase 2 before REC-B1 is satisfied.
- The 5-minute time-based trigger requires the DB Writer task to be a long-lived
  background thread (established in Phase 1A S6).

---

## Alternatives Rejected

- **TRUNCATE checkpoint always**: would block readers; violates REC-B4.
- **RESTART checkpoint on autocheckpoint**: would reset WAL position unnecessarily; PASSIVE is sufficient for normal operation.
- **Schema-based last-checkpoint tracking**: unnecessary overhead for Phase 1A; an in-process timestamp is sufficient.
