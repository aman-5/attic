# ADR-002 — Secret Detector Pattern Versioning

**Status**: ACCEPTED  
**Date**: 2026-08-24  
**Resolves**: OQ-007  
**Relevant contracts**: `docs/contracts/secrets.md` §3 (V1 baseline patterns), `docs/contracts/recovery.md` R-6, `docs/contracts/compatibility.md` §PARTIALLY_REBUILDABLE

---

## Context

OQ-007 asked:
1. How are new secret patterns deployed without a full re-scan of all files?
2. Is there a `secret_pattern_version` column on `core_file_occurrences`?
3. What triggers re-scan when patterns are updated?

The `secrets.md` contract establishes that `core_file_occurrences.secret_scan_state`
tracks per-file scan status (`PENDING | IN_PROGRESS | CLEAN | HAS_SECRETS`).
Recovery step R-6 relies on this column to detect in-flight scans after a crash.

The `compatibility.md` contract specifies that `IndexGeneration` records the
versions of all subsystems. A secret pattern version change is a subsystem
version change that requires re-scanning affected files.

---

## Decision

### 1. `secret_pattern_version` column on `core_file_occurrences`

Add `secret_pattern_version INTEGER NOT NULL DEFAULT 1` to `core_file_occurrences`.

This column records which version of the secret detector pattern set was used
when a file's `secret_scan_state` last transitioned to `CLEAN` or `HAS_SECRETS`.

When the secret detector pattern set is updated (version bumped), the system
identifies files where `secret_pattern_version < current_version` and sets
their `secret_scan_state = 'PENDING'` for re-scan. No full re-index of
structural/symbol artifacts is required; only the secret scan pass re-runs.

### 2. Version numbering

Secret pattern version is an `INTEGER` starting at `1`. It is a monotonically
increasing counter, not a semver string, because pattern additions are always
additive (removing a pattern requires a decision record per `secrets.md`).

The current version is defined as a constant in `attic-storage`:
```
pub const SECRET_PATTERN_VERSION: i64 = 1;
```

### 3. `IndexGeneration.secret_detector_version` dimension

Add `secret_detector_version INTEGER NOT NULL DEFAULT 1` to `core_index_generations`.

This records which pattern version was active when a given `IndexGeneration`
was produced. When the pattern version changes:
- Compatibility class: `PARTIALLY_REBUILDABLE` (only secret-scan artifacts
  and derived `security_state` fields need updating; structural/symbol/FTS
  artifacts are unaffected unless security_state changes affect them).
- Action: mark `core_file_occurrences.secret_scan_state = 'PENDING'` for all
  files in the affected generation where
  `secret_pattern_version < SECRET_PATTERN_VERSION`.

### 4. Re-scan trigger

Re-scan is triggered at startup when:
```sql
SELECT COUNT(*) FROM core_file_occurrences
WHERE secret_pattern_version < :current_pattern_version
  AND secret_scan_state IN ('CLEAN', 'HAS_SECRETS')
```
returns a non-zero count. The server schedules `SECRET_SCAN` ops_tasks for
all such files. This happens as part of Phase 2 incremental update; in Phase
1A, the column is present but the re-scan scheduler is not yet implemented.

### 5. Schema changes required

Two DDL additions to `migrations/0001_initial.sql`:

**`core_file_occurrences`**: add
```sql
secret_pattern_version  INTEGER NOT NULL DEFAULT 1
```

**`core_index_generations`**: add
```sql
secret_detector_version  INTEGER NOT NULL DEFAULT 1
```

Both default to `1` to represent the V1 baseline pattern set defined in
`secrets.md` §3.

---

## Consequences

- Files can be re-scanned for new secret patterns without invalidating or
  rebuilding structural/symbol/FTS artifacts.
- The `secret_pattern_version` column is always set when `secret_scan_state`
  transitions to `CLEAN` or `HAS_SECRETS`.
- `secret_scan_state = 'PENDING'` files always have `secret_pattern_version`
  equal to whatever it was before the pending scan (i.e., possibly stale).
- The `IndexGeneration` `secret_detector_version` field allows cross-generation
  queries to determine whether artifacts are consistent with the current
  pattern set.

---

## Alternatives Rejected

- **Global re-scan flag only (no per-file version)**: does not allow incremental
  re-scan; forces a full re-scan of all files on any pattern update.
- **Semver for pattern version**: unnecessary; pattern additions are always
  forward-compatible additive changes. A simple counter is sufficient and
  avoids semver parsing in the re-scan query.
- **Separate `secret_scans` table**: over-engineered for Phase 1A; per-file
  columns on `core_file_occurrences` are sufficient.
