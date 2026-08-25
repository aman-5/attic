# ADR-003 — `ops_server_state` Single-Row Invariant Enforcement

**Status**: ACCEPTED  
**Date**: 2026-08-24  
**Resolves**: OQ-014  
**Relevant contracts**: `migrations/0001_initial.sql` §13, `docs/contracts/recovery.md` §7

---

## Context

`ops_server_state` is designed as a singleton table: exactly one row, always
with `id = 'singleton'`. The Phase 0 migration defines the table with
`id TEXT NOT NULL PRIMARY KEY DEFAULT 'singleton'` but does not enforce that
no other `id` value can be inserted.

OQ-014 asked whether the single-row intent should be enforced via a `CHECK`
constraint or left to the application layer.

`docs/contracts/recovery.md` §7 (Watcher Epoch and Re-delivery) relies on
`ops_server_state.watcher_epoch` being read and incremented reliably on each
startup. If multiple rows existed, the watcher epoch increment would be
ambiguous.

---

## Decision

### 1. Add `CHECK (server_id = 'singleton')` — name the column `server_id`

The existing column `id` is the PRIMARY KEY with `DEFAULT 'singleton'`. To
add a `CHECK` constraint on an existing SQLite table, the approach is to
include the constraint in the initial `CREATE TABLE` DDL (since SQLite does
not support `ALTER TABLE ADD CONSTRAINT`).

Because migration `0001_initial` is the only migration and the table has not
yet been used in production, the constraint is added to the `CREATE TABLE`
statement directly in `0001_initial.sql`:

```sql
CREATE TABLE IF NOT EXISTS ops_server_state (
    id              TEXT    NOT NULL PRIMARY KEY DEFAULT 'singleton'
                            CHECK (id = 'singleton'),
    ...
);
```

This inline `CHECK` on the column is valid SQLite syntax and is the simplest
approach.

### 2. Application layer always upserts with `id = 'singleton'`

All application code that writes to `ops_server_state` must use:

```sql
INSERT INTO ops_server_state (id, ...) VALUES ('singleton', ...)
ON CONFLICT(id) DO UPDATE SET ... WHERE id = 'singleton';
```

The CHECK constraint provides a database-level backstop; the application
convention provides the operational guarantee.

### 3. Schema change scope

Modify `migrations/0001_initial.sql` Section 13 (`ops_server_state`) to add
the inline `CHECK (id = 'singleton')` to the `id` column definition.

No other tables are affected.

---

## Consequences

- Any INSERT with `id != 'singleton'` is rejected at the SQLite layer with a
  constraint violation, not silently ignored.
- Application code is simplified: it only ever has to handle the upsert path.
- Recovery step R-7 (watcher epoch increment) is safe because there is always
  exactly zero or one row in this table.

---

## Alternatives Rejected

- **Application-only enforcement**: invisible to DB-level tooling; a future
  direct SQL query or migration bug could insert a second row without
  detection. The CHECK constraint adds negligible overhead and provides
  defense in depth.
- **UNIQUE index on a separate boolean column**: unnecessarily complex when
  an inline CHECK on the primary key achieves the same result more clearly.
