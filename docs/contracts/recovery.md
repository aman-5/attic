# Crash Recovery Contract

**Contract ID:** C15  
**Version:** 1.0.0  
**Status:** DRAFT  
**Depends on:** C04 (storage), C08 (invalidation), C14 (resources)

---

## 1. Purpose

This contract defines the **post-crash and post-restart state** for all Attic subsystems. It enumerates every crash point in the system, specifies the invariants that must hold after recovery, and defines the recovery algorithm the server executes on startup before accepting any queries.

Key design decisions:
- **No crash leaves the database in a permanently unrecoverable state.** SQLite WAL mode provides atomicity; any uncommitted transaction is automatically rolled back by SQLite on next open.
- **Recovery is idempotent.** Running the recovery procedure twice produces the same result as running it once.
- **Recovery MUST complete before the server accepts any MCP tool call.** The server MUST NOT serve queries while in recovery mode.
- **Artifacts are innocent until proven guilty.** An artifact that was being written at crash time is treated as INVALID (not corrupted); it will be recomputed on the next indexing pass.
- **Secrets are never recovered.** Any in-flight secret scan result that was not committed is discarded. The file will be rescanned.

---

## 2. Crash Point Taxonomy

### 2.1 Crash Classes

```
enum CrashClass {
    // Uncommitted write — SQLite WAL rolls back automatically
    UNCOMMITTED_WRITE,

    // Mid-indexing crash — some artifacts written, pipeline not complete
    PARTIAL_INDEXING,

    // Mid-migration crash — schema migration did not complete
    PARTIAL_MIGRATION,

    // Corrupt database file — WAL or main file is unreadable
    CORRUPT_DATABASE,

    // Mid-plan crash — retrieval plan was never persisted
    INCOMPLETE_PLAN,

    // OS-level crash — process killed; any of the above may apply
    OS_KILL,
}
```

### 2.2 Crash Points by Subsystem

| ID    | Subsystem | Crash Point | Recovery Action |
|-------|-----------|-------------|-----------------|
| CP-01 | SQLite writer | Process killed during `BEGIN ... COMMIT` | WAL auto-rollback; no action needed |
| CP-02 | Schema migration | Process killed after partial DDL | Detect via `ops_migration_log`; re-run migration from last completed step |
| CP-03 | FULL_REINDEX | Process killed after writing N of M files | Mark all `core_file_artifacts` with `generation_id` matching the incomplete run as INVALID; schedule re-index |
| CP-04 | INCREMENTAL_UPDATE | Process killed mid-batch | Mark affected file rows as STALE; watcher re-delivers events on next startup |
| CP-05 | ANALYZER_RUN | Process killed mid-analysis | Affected `core_symbol_*` rows for that file: mark INVALID |
| CP-06 | EMBEDDING_GENERATION | Process killed mid-batch | Delete partial embedding rows; regenerate from stored retrieval units |
| CP-07 | SECRET_SCAN | Process killed during scan | Discard in-flight scan results; mark file's `secret_scan_state = PENDING` |
| CP-08 | RETRIEVAL_PLAN write | Process killed before plan INSERT | Plan is lost; query result was already returned; no data integrity issue |
| CP-09 | WAL checkpoint | Process killed during checkpoint | SQLite recovers automatically; WAL replayed on next open |
| CP-10 | DB file corruption | Main database file unreadable | See §5 (Corrupt Database Recovery) |

---

## 3. Server Startup Recovery Procedure

The server MUST execute the following steps **sequentially** on every startup, before binding the MCP socket:

```
STARTUP RECOVERY PROCEDURE

Step R-1: Open SQLite database
  - Open with WAL mode; SQLite automatically replays any committed WAL frames
  - If open fails: transition to CORRUPT_DATABASE recovery (§5)

Step R-2: Verify schema integrity
  - Execute: PRAGMA integrity_check(100)
  - If errors found: transition to CORRUPT_DATABASE recovery (§5)
  - Execute: PRAGMA foreign_key_check
  - If violations found: log and mark affected rows as INVALID; do not abort startup

Step R-3: Check migration state
  - Query ops_migration_log for any migration with status = 'RUNNING' or 'FAILED'
  - If found: re-run the migration from the failed step (§4)
  - If no incomplete migrations: verify current schema_version matches binary's expected version
  - If mismatch: run pending migrations in order

Step R-4: Detect incomplete indexing runs
  - Query: SELECT generation_id FROM ops_indexing_log WHERE status = 'RUNNING'
  - For each incomplete generation_id:
      UPDATE core_file_artifacts SET invalidation_state = 'INVALID'
      WHERE generation_id = ? AND invalidation_state != 'CURRENT'
      UPDATE ops_indexing_log SET status = 'ABANDONED' WHERE generation_id = ?

Step R-5: Mark stale file artifacts
  - For every file in core_files where last_seen_at_us is older than the
    current watcher epoch: mark as STALE (not INVALID; will be refreshed by watcher)

Step R-6: Verify secret scan completeness
  - Query: SELECT file_id FROM core_files WHERE secret_scan_state = 'IN_PROGRESS'
  - For each: UPDATE core_files SET secret_scan_state = 'PENDING'
    (file will be rescanned during next indexing pass)

Step R-7: Recover incomplete plans
  - Query: SELECT plan_id FROM ops_retrieval_log WHERE completed_at_us IS NULL
  - For each: UPDATE ops_retrieval_log SET result = 'INTERNAL_ERROR',
    confidence = 'NONE',
    plan_json = json_patch(plan_json, '{"result":"INTERNAL_ERROR","completed_at_us":?}')
    WHERE plan_id = ?
  - These plans were in-flight at crash time; the query result was never returned

Step R-8: Validate reference integrity
  - Confirm all core_symbol_occurrences.file_id values exist in core_files
  - Orphaned occurrences (file deleted): DELETE from core_symbol_occurrences
  - Log count of deleted orphan rows

Step R-9: Emit RecoveryComplete event
  - Log: recovery duration, rows affected per step, any anomalies found
  - Set server_state = READY

Step R-10: Begin accepting MCP tool calls
```

---

## 4. Migration Recovery

If a migration is found in state `RUNNING` or `FAILED` at startup:

```
MIGRATION RECOVERY PROCEDURE

1. Load the migration script for the failed migration_id
2. Identify the last successfully executed statement (recorded in ops_migration_log.progress_json)
3. Re-execute all statements AFTER the last completed one
4. Each statement is wrapped in its own transaction (DDL auto-commit in SQLite)
5. If re-execution succeeds: UPDATE ops_migration_log SET status = 'COMPLETED'
6. If re-execution fails again:
   - Mark migration status = 'FAILED'
   - Log the error with full DDL statement
   - Transition to CORRUPT_DATABASE recovery (§5)
   - Do NOT proceed with startup
```

Migration scripts MUST be written as **idempotent DDL** (using `CREATE TABLE IF NOT EXISTS`, `CREATE INDEX IF NOT EXISTS`, etc.) so that partial re-execution does not cause errors.

---

## 5. Corrupt Database Recovery

Triggered when `PRAGMA integrity_check` returns errors or the database file cannot be opened:

```
CORRUPT DATABASE RECOVERY PROCEDURE

1. Rename the corrupt database to: attic.db.corrupt.<timestamp>
2. Log a CorruptDatabaseDetected event with the integrity_check output
3. Check for a backup database (attic.db.backup) created by the last successful checkpoint
   IF backup exists AND backup passes integrity_check:
     Copy backup to attic.db
     Log: RestoreFromBackup
     Return to Step R-1 of startup procedure
   ELSE:
     Create a new empty database
     Run all migrations from schema version 0
     Mark all workspace content as requiring FULL_REINDEX
     Log: FreshDatabaseCreated — all indexes will be rebuilt
4. Continue startup; the index will be rebuilt by the background FULL_REINDEX task
```

The server MUST **not** delete the corrupt file — it is retained for forensic analysis.

---

## 6. Backup Policy

```
RULE REC-B1:
  After every successful WAL checkpoint, the server MUST copy the
  main database file to attic.db.backup (atomic rename pattern:
  write to attic.db.backup.tmp, then rename).

RULE REC-B2:
  Backups are retained for the most recent 3 checkpoints only.
  Older backups are deleted on the next successful checkpoint.

RULE REC-B3:
  Checkpoint frequency: every 1 000 WAL frames OR every 5 minutes,
  whichever comes first. Both thresholds are configurable at startup.

RULE REC-B4:
  Backup writes MUST NOT block the main write path. The checkpoint
  and backup copy run in a LOW priority background task.
```

---

## 7. Watcher Epoch and Re-delivery

On startup, the file watcher assigns a new **watcher epoch** (monotonically increasing integer stored in `ops_server_state`). Files that were STALE at crash time will be re-delivered by the watcher during its initial scan and processed by `INCREMENTAL_UPDATE` tasks.

```
RULE REC-W1:
  The watcher MUST perform a full directory scan on startup (not just
  subscribe to OS events). This ensures files modified while the server
  was down are detected.

RULE REC-W2:
  The startup scan MUST run as a background task AFTER the server
  begins accepting MCP queries (to avoid blocking query availability).
  Queries issued before the startup scan completes may see STALE evidence;
  this is acceptable and MUST be indicated in the RetrievalPlan
  (evidence marked as POTENTIALLY_STALE).
```

---

## 8. Recovery State Machine

```
          ┌──────────────────┐
          │    STARTING      │
          └────────┬─────────┘
                   │ open DB
          ┌────────▼─────────┐
          │  INTEGRITY_CHECK │◄──── PRAGMA integrity_check
          └────────┬─────────┘
          pass │       │ fail
               │       ▼
               │  ┌─────────────────┐
               │  │ CORRUPT_RECOVER  │──► restore from backup OR create fresh DB
               │  └────────┬────────┘
               │           │ complete
          ┌────▼───────────▼─────────┐
          │   MIGRATION_CHECK        │◄──── detect incomplete migrations
          └────────┬─────────────────┘
          clean │       │ incomplete
                │       ▼
                │  ┌─────────────────┐
                │  │ MIGRATION_REPAIR │──► re-run failed migration steps
                │  └────────┬────────┘
                │           │ complete
          ┌─────▼───────────▼────────┐
          │   ARTIFACT_RECOVERY      │◄──── Steps R-4 through R-8
          └────────┬─────────────────┘
                   │ complete
          ┌────────▼─────────┐
          │     READY        │──► accept MCP tool calls
          └──────────────────┘
```

---

## 9. Invariants

| ID       | Invariant |
|----------|-----------|
| REC-INV-1 | The server MUST NOT accept any MCP tool call while in any recovery state other than READY. |
| REC-INV-2 | Recovery is idempotent: running the recovery procedure on an already-recovered database produces no changes. |
| REC-INV-3 | No recovery step MUST delete user data (source files). Only derived artifacts (indexes, embeddings, plans) may be invalidated or deleted. |
| REC-INV-4 | A corrupt database file MUST be renamed, never deleted, to preserve forensic evidence. |
| REC-INV-5 | After recovery, every file in `core_files` has `secret_scan_state` in one of: `CLEAN`, `HAS_SECRETS`, `PENDING`. The state `IN_PROGRESS` MUST NOT exist after recovery. |
| REC-INV-6 | After recovery, no `ops_indexing_log` row has `status = 'RUNNING'`. |
| REC-INV-7 | After recovery, no `ops_retrieval_log` row has `completed_at_us IS NULL`. |
| REC-INV-8 | The backup file (attic.db.backup) is written ONLY after a successful WAL checkpoint, never during recovery. |

---

## 10. Test Matrix

| ID      | Scenario | Expected Outcome |
|---------|----------|-----------------|
| REC-01  | Process killed during SQLite `COMMIT` | WAL rollback on next open; no partial data visible; integrity check passes |
| REC-02  | Process killed mid-FULL_REINDEX (50% complete) | Incomplete generation artifacts marked INVALID; reindex triggered |
| REC-03  | Process killed during schema migration DDL | Migration re-runs from failed statement; startup completes |
| REC-04  | Database `integrity_check` returns page errors | Corrupt file renamed; backup restored; server starts with restored data |
| REC-05  | No backup exists when database is corrupt | Fresh database created; FULL_REINDEX scheduled; server starts |
| REC-06  | Process killed mid-secret-scan | Affected files have `secret_scan_state = PENDING` after recovery |
| REC-07  | Process killed before retrieval plan INSERT | Plan row updated to `INTERNAL_ERROR`; no crash on startup |
| REC-08  | Orphaned symbol_occurrence rows (file deleted) | Orphaned rows deleted during Step R-8; count logged |
| REC-09  | Recovery procedure run twice on clean database | Second run produces zero changes; no errors |
| REC-10  | Watcher startup scan finds files modified while server was down | Files marked STALE; INCREMENTAL_UPDATE triggered; queries see POTENTIALLY_STALE evidence |

---

## 11. Open Questions

| ID     | Question | Impact |
|--------|----------|--------|
| REC-Q1 | Should the server expose a `/health` HTTP endpoint (in addition to MCP) to signal recovery state to process supervisors? | Deferred; V1 uses exit code on fatal recovery failure |
| REC-Q2 | Should backups be stored in a configurable external path (e.g., for network backup)? | Deferred to post-V1 |
| REC-Q3 | What is the correct behavior if recovery takes > 60 seconds (e.g., large corrupt DB)? | Log warning every 10 seconds; no timeout in V1 |
