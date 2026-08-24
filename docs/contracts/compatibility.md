# Contract: IndexGeneration and Version Compatibility

## Purpose

Define how Attic tracks which version of every subsystem produced a set of
derived artifacts, and what action is required when any version changes. This
prevents silently serving stale or incompatible artifacts after upgrades.

---

## Definitions

### IndexGeneration

An `IndexGeneration` records the exact versions of all subsystems that produced
a set of derived artifacts for one `SourceRevision`.

```
IndexGeneration {
  id                      : Uuid
  source_revision_id      : Uuid    -- foreign key → SourceRevision
  schema_version          : String  -- semver string of the DB schema
  analyzer_registry_version: String -- hash of registered analyzer set
  analyzer_versions       : String  -- JSON map: analyzer_id → version string
  segmentation_version    : String  -- semver of the segmentation algorithm
  indexer_version         : String  -- semver of the indexing pipeline
  discovery_policy_hash   : String  -- same as SourceRevision.discovery_policy_hash
  ranking_version         : String  -- semver of the ranking algorithm
  embedding_model_version : Option<String> -- NULL if semantic index not built
  configuration_hash      : String  -- BLAKE3 of all relevant Attic configuration
  created_at              : i64     -- Unix timestamp (microseconds)
}
```

---

## Version Change Scope Table

Each subsystem version change has a defined invalidation scope. The scope
is the minimum set of derived artifacts that MUST be discarded or rebuilt.

| Subsystem changed          | Invalidation scope                                              |
|----------------------------|-----------------------------------------------------------------|
| `schema_version`           | Full migration or full rebuild (see §Migration Rules)           |
| `analyzer_registry_version`| All artifacts produced by any changed or removed analyzer       |
| Specific `analyzer_versions[id]`| Structural nodes, symbols, relationships, retrieval units for files handled by that analyzer |
| `segmentation_version`     | Retrieval units + all dependent semantic representations        |
| `indexer_version`          | All retrieval units; structural artifacts if indexer reads AST  |
| `discovery_policy_hash`    | New `SourceRevision` required; then re-discovery and incremental rebuild |
| `ranking_version`          | No source reindex; ranking re-runs at query time or reindex metadata only |
| `embedding_model_version`  | All semantic representations; structural/symbol artifacts unaffected |
| `configuration_hash`       | Depends on which config keys changed; specific scope recorded   |

---

## Compatibility States

The binary MUST explicitly determine the compatibility state of an existing
`IndexGeneration` before using it.

```
COMPATIBLE         -- fully usable; no action required
MIGRATABLE         -- schema migration available; migrate before use
PARTIALLY_REBUILDABLE -- some artifacts invalid; incremental rebuild required
INCOMPATIBLE       -- full rebuild required
```

### COMPATIBLE

All version fields match the current binary's expectations exactly.

### MIGRATABLE

`schema_version` is older but within the supported migration range.
A migration script exists in `migrations/`. The binary runs migrations
automatically on startup if the schema is within the supported range.

Supported migration range: versions within the current major version.
Cross-major migrations are `INCOMPATIBLE`.

### PARTIALLY_REBUILDABLE

One or more non-schema subsystems have changed (analyzer, segmentation, etc.).
The binary:
1. Marks the affected artifact set as `INVALID`.
2. Schedules targeted incremental recomputation.
3. Continues serving unaffected artifacts.

### INCOMPATIBLE

The `schema_version` major version differs, or the schema is newer than the
binary knows how to read, or the index was produced by an unknown binary.
The binary refuses to use the index and requires a full rebuild.

---

## Migration Rules

### Schema migration

1. Migration files live in `migrations/` as `NNNN_description.sql` where `NNNN`
   is a zero-padded 4-digit sequence number.
2. Each migration file is idempotent: it uses `CREATE TABLE IF NOT EXISTS`,
   `CREATE INDEX IF NOT EXISTS`, and `ALTER TABLE ... ADD COLUMN IF NOT EXISTS`
   (or guarded DDL for SQLite compat).
3. The binary tracks applied migrations in a `core_schema_migrations` table.
4. Migrations run inside a transaction; on failure the transaction rolls back
   and the binary reports `MIGRATION_FAILED` with the migration ID and error.
5. Migrations never delete data without first verifying the schema version
   transition is expected.
6. Downgrade migrations are NOT supported for V1; downgrade requires a full
   rebuild.

### Rebuild trigger

When `INCOMPATIBLE` is detected:
1. The existing `index.db` is renamed to `index.db.backup.<timestamp>`.
2. A fresh database is initialized.
3. A full reindex is scheduled.
4. The backup is kept for at least 24 hours (configurable).

---

## Version Identity Sources

| Version string | Source |
|----------------|--------|
| `schema_version` | Hardcoded constant in binary (`CURRENT_SCHEMA_VERSION`) |
| `analyzer_registry_version` | BLAKE3 of sorted list of `(analyzer_id, version)` pairs |
| `analyzer_versions[id]` | Each analyzer's `VERSION` constant |
| `segmentation_version` | Hardcoded constant in segmentation module |
| `indexer_version` | Hardcoded constant in indexing pipeline |
| `ranking_version` | Hardcoded constant in ranking module |
| `embedding_model_version` | Model identifier string (e.g., `"nomic-embed-text-v1.5"`) |
| `configuration_hash` | BLAKE3 of canonical serialized Attic config at startup |

Version strings use semver (`MAJOR.MINOR.PATCH`). Major version increments
indicate incompatible changes. Minor version increments indicate
partially-rebuildable changes. Patch increments are compatible.

---

## Invariants

1. An `IndexGeneration` record is immutable once written.
2. The binary never writes new artifacts against a stale `IndexGeneration`;
   it always creates a new `IndexGeneration` when any version changes.
3. Every derived artifact references the `index_generation_id` that produced it.
4. Artifacts from an `INCOMPATIBLE` `IndexGeneration` are never served.
5. A `MIGRATABLE` migration runs to completion or rolls back; it never leaves
   the database in a partial state.
6. Full workspace rebuilds are never triggered unless the state is `INCOMPATIBLE`
   or explicitly requested by the operator.

---

## State Machine

```
[NEW]
  |
  | -- schema matches, versions match
  v
[COMPATIBLE]
  |
  | -- analyzer version changed
  v
[PARTIALLY_REBUILDABLE] --> incremental rebuild --> [COMPATIBLE]
  |
  | -- schema version changed (within major)
  v
[MIGRATABLE] --> migration --> [COMPATIBLE]
  |
  | -- schema major version changed
  v
[INCOMPATIBLE] --> full rebuild --> [COMPATIBLE]
```

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Migration file missing for required version | `INCOMPATIBLE`; prompt full rebuild |
| Migration transaction failure | Roll back; binary exits with `MIGRATION_FAILED` |
| Version string not parseable as semver | `INCOMPATIBLE`; rebuild |
| Backup rename fails on `INCOMPATIBLE` | Log error; halt; do not destroy existing index |
| Unknown `index_generation_id` on artifact | Treat artifact as `INVALID` |

---

## Observability

On startup, the binary logs:

```
current_schema_version
detected_schema_version
compatibility_state: COMPATIBLE | MIGRATABLE | PARTIALLY_REBUILDABLE | INCOMPATIBLE
migration_applied: [list of migration IDs] or []
rebuild_triggered: bool
```

---

## Examples

### Analyzer upgrade (minor version bump)

```
Before: analyzer_versions["java"] = "1.2.0"
After:  analyzer_versions["java"] = "1.3.0"

State: PARTIALLY_REBUILDABLE
Action: Invalidate all Java structural nodes, symbols, relationships, retrieval
        units. Schedule incremental recomputation for Java files only.
```

### Segmentation change

```
Before: segmentation_version = "1.0.0"
After:  segmentation_version = "2.0.0"  (major bump)

State: PARTIALLY_REBUILDABLE
Action: Invalidate all retrieval units and semantic representations.
        Structural nodes and symbols unaffected.
```

### Schema major version bump

```
Before: schema_version = "1.5.2"
After:  schema_version = "2.0.0"

State: INCOMPATIBLE
Action: Backup and full rebuild.
```

### Embedding model change

```
Before: embedding_model_version = "nomic-embed-text-v1.5"
After:  embedding_model_version = "nomic-embed-text-v2.0"

State: PARTIALLY_REBUILDABLE (semantic only)
Action: Invalidate all semantic representations. All structural/lexical
        artifacts remain valid.
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| CM-01 | Binary starts with matching versions | State: COMPATIBLE; no migration |
| CM-02 | Schema minor version older; migration exists | State: MIGRATABLE; migration runs |
| CM-03 | Schema major version differs | State: INCOMPATIBLE; rebuild triggered |
| CM-04 | Java analyzer version bumped (minor) | State: PARTIALLY_REBUILDABLE; Java artifacts invalidated; Python unaffected |
| CM-05 | Segmentation version bumped | Retrieval units invalidated; symbols unaffected |
| CM-06 | Embedding model changed | Semantic representations invalidated; structural layer unaffected |
| CM-07 | Migration fails mid-run | Transaction rolled back; binary exits with MIGRATION_FAILED |
| CM-08 | Unknown index_generation_id on artifact | Artifact treated as INVALID |
| CM-09 | Ranking version changed | No invalidation; ranking metadata updated at query time |
| CM-10 | Full rebuild triggered | Old DB renamed with timestamp; new DB initialized |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| CM-Q1 | Should `configuration_hash` changes always trigger PARTIALLY_REBUILDABLE, or should specific config keys be mapped to specific scopes? | No — provisional: full scope for any config change; refine per key in Phase 1A |
| CM-Q2 | Should the backup retention period (default 24h) be enforced by the binary or left to the operator? | No — operator-managed for V1 |
