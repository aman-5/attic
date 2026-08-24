# ADR-004 — `IndexGeneration` Per-Subsystem Version Tracking

**Status**: ACCEPTED  
**Date**: 2026-08-24  
**Resolves**: OQ-016  
**Relevant contracts**: `docs/contracts/compatibility.md` §PARTIALLY_REBUILDABLE, `docs/contracts/compatibility.md` §Version Identity Sources

---

## Context

OQ-016 asked: is there a per-subsystem hash within `IndexGeneration` to
identify which artifacts need rebuilding when a single subsystem version
changes?

`compatibility.md` §PARTIALLY_REBUILDABLE specifies that when one or more
non-schema subsystems change (e.g., a single analyzer version bumps), only
the affected artifacts need invalidation — other artifacts remain valid.
The `compatibility.md` §Version Change Scope Table maps each subsystem to
its invalidation scope.

The existing `core_index_generations` table records individual version fields
for the major subsystems (`analyzer_versions_json`, `segmentation_version`,
`indexer_version`, etc.) but lacks a consolidated per-subsystem version map
that can be compared at query time to determine the exact set of changed
subsystems.

OQ-016's suggested resolution was: add a `subsystem_versions` JSON column
mapping subsystem name → version hash, and define `PARTIALLY_REBUILDABLE` as
meaning ≥ 1 but not all subsystem versions changed.

---

## Decision

### 1. Add `subsystem_versions_json` column to `core_index_generations`

Add `subsystem_versions_json TEXT NOT NULL` to `core_index_generations`.

This is a JSON object mapping a stable subsystem key to its version string:

```json
{
  "schema":            "1.0.0",
  "analyzer_registry": "<blake3-hex-of-sorted-analyzer-id-version-pairs>",
  "analyzer.<id>":     "<version>",
  "segmentation":      "1.0.0",
  "indexer":           "1.0.0",
  "ranking":           "1.0.0",
  "embedding_model":   "<model-id-or-null>",
  "secret_detector":   "1",
  "configuration":     "<sha256-hex>"
}
```

The subsystem keys are stable string identifiers defined as constants in
`attic-core`. Analyzers are individually keyed as `"analyzer.<analyzer_id>"`.

### 2. Purpose and use

At startup, the binary computes the current `subsystem_versions_json` from
its known constants and active configuration. It then compares this map with
the stored map in the most recent `IndexGeneration` for each repository to
determine the compatibility class:

- All values match → `COMPATIBLE`
- Only `schema` changed (within major) → `MIGRATABLE`
- One or more non-schema values changed → `PARTIALLY_REBUILDABLE`
  - The diff of the two maps identifies exactly which subsystems changed
  - The `compatibility.md` §Version Change Scope Table maps each changed
    subsystem to its invalidation scope
- `schema` major version differs → `INCOMPATIBLE`

### 3. Relationship to existing columns

The existing individual version columns (`schema_version`, `analyzer_versions_json`,
`segmentation_version`, etc.) remain. They are the canonical source; 
`subsystem_versions_json` is a convenience denormalization that consolidates
all version dimensions into one comparable map.

The two representations must be kept consistent: when writing an
`IndexGeneration` row, `subsystem_versions_json` must be derived from the
individual column values, not independently computed.

### 4. Schema change

Add to `core_index_generations` in `migrations/0001_initial.sql`:

```sql
subsystem_versions_json  TEXT    NOT NULL  -- JSON: subsystem_key → version string
```

Default for the DEFAULT clause: not applicable — this column is always
explicitly set when a new `IndexGeneration` is inserted. There is no
meaningful default for `NOT NULL` without a value, so the application
must always provide a value. (SQLite will enforce this.)

### 5. Constants defined in `attic-core`

```rust
pub mod subsystem_keys {
    pub const SCHEMA: &str            = "schema";
    pub const ANALYZER_REGISTRY: &str = "analyzer_registry";
    pub const SEGMENTATION: &str      = "segmentation";
    pub const INDEXER: &str           = "indexer";
    pub const RANKING: &str           = "ranking";
    pub const EMBEDDING_MODEL: &str   = "embedding_model";
    pub const SECRET_DETECTOR: &str   = "secret_detector";
    pub const CONFIGURATION: &str     = "configuration";
    // Analyzer-specific key: format!("analyzer.{}", analyzer_id)
}
```

---

## Consequences

- The `PARTIALLY_REBUILDABLE` state is now actionable: the binary can
  enumerate exactly which subsystems changed and apply the scope table
  without re-comparing all individual version columns.
- The `subsystem_versions_json` column is always written on `IndexGeneration`
  creation; it cannot be NULL.
- Backward compatibility: because this column is added to the initial
  migration (before any production data exists), there is no migration gap.
- Future subsystem additions simply add new keys to the JSON object; no
  schema change is required.

---

## Alternatives Rejected

- **No consolidated map; compare individual columns only**: requires the
  compatibility checker to have hard-coded knowledge of every version column
  name. The JSON map is extensible without schema changes.
- **Separate `core_subsystem_versions` table**: over-engineered for Phase 1A;
  the JSON column on `core_index_generations` is self-contained and avoids
  an additional JOIN.
- **Bitmask of changed subsystems**: not human-readable; requires tight
  coupling between the bit positions and subsystem identities.
