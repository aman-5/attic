# Contract: Invalidation Dependency DAG

## Purpose

Define the artifact dependency graph, state transitions, propagation rules,
and recomputation separation for Attic's invalidation system. Invalidation
marks artifacts stale or invalid without immediately recomputing them.
Recomputation is a separate scheduled step.

---

## Definitions

### ArtifactType

```
FILE_OCCURRENCE       -- a file's observed state at a source revision
STRUCTURAL_NODE       -- parsed structural element (class, function, section, etc.)
SYMBOL_OCCURRENCE     -- a symbol's definition/occurrence at a source revision
RETRIEVAL_UNIT        -- a text chunk indexed for search
SEMANTIC_REPR         -- an embedding or semantic vector
RELATIONSHIP          -- a dependency/call/import edge
EVIDENCE              -- a canonical evidence record
KNOWLEDGE_ITEM        -- a knowledge document record
```

### InvalidationState

```
CURRENT           -- artifact is valid and up-to-date
STALE             -- artifact may be outdated; still usable with staleness caveat
INVALID           -- artifact must not be used; recomputation required
PENDING_REFRESH   -- recomputation scheduled; not yet complete
```

### InvalidationReason

```
SOURCE_CHANGED        -- underlying file content changed
ANALYZER_UPGRADED     -- analyzer version changed for this artifact type
SCHEMA_MIGRATED       -- schema version changed; artifacts must be rebuilt
POLICY_CHANGED        -- discovery policy changed; file set may differ
DEPENDENCY_INVALID    -- a dependency in the DAG was invalidated
EXPLICIT              -- operator-requested rebuild
DELETION              -- source file deleted
```

---

## Artifact Dependency DAG

The DAG defines which artifacts are derived from which sources.
When a source artifact is invalidated, all dependent artifacts are also
transitively invalidated.

```
SourceRevision
    |
    v
FileOccurrence
    |
    +---> StructuralNode
    |           |
    |           +---> SymbolOccurrence
    |           |
    |           +---> Relationship
    |
    +---> RetrievalUnit
    |           |
    |           +---> SemanticRepr
    |
    +---> KnowledgeItem
    |
    +---> Evidence (aggregates all of the above)
```

### Dependency table

| Artifact | Depends on | Invalidation trigger |
|----------|-----------|---------------------|
| FileOccurrence | SourceRevision (content_hash) | Content hash change or file deletion |
| StructuralNode | FileOccurrence + analyzer version | Either changes |
| SymbolOccurrence | StructuralNode + FileOccurrence | Either changes |
| Relationship | SymbolOccurrence or FileOccurrence | Either source or target invalidated |
| RetrievalUnit | FileOccurrence + segmentation version | Either changes |
| SemanticRepr | RetrievalUnit + embedding model version | Either changes |
| KnowledgeItem | FileOccurrence (knowledge file) | Knowledge file changes |
| Evidence | FileOccurrence + RetrievalUnit + StructuralNode | Any dependency invalidated |

---

## State Transitions

```
[CURRENT]
    |
    | -- source changes / version upgrade / policy change
    v
[STALE]
    |
    | -- propagation or explicit invalidation
    v
[INVALID]
    |
    | -- recomputation scheduled
    v
[PENDING_REFRESH]
    |
    | -- recomputation complete
    v
[CURRENT]

[INVALID] ------> [PENDING_REFRESH] ------> [CURRENT]
   ^
   | -- recomputation fails
[INVALID]  (retry via task scheduler)
```

Artifacts MAY transition directly from CURRENT to INVALID (skipping STALE)
when:
- The file is deleted.
- An INCOMPATIBLE schema migration is triggered.
- An explicit operator rebuild is requested.

STALE is used when the artifact is likely outdated but still safe to serve
with a staleness caveat (e.g., a minor analyzer version bump during
background indexing).

---

## Propagation Rules

When a FileOccurrence is invalidated:

```
1. Mark FileOccurrence as INVALID.
2. For all StructuralNodes where file_occurrence_id = this file:
     Mark INVALID.
3. For all SymbolOccurrences where file_occurrence_id = this file:
     Mark INVALID.
4. For all RetrievalUnits where file_occurrence_id = this file:
     Mark INVALID.
5. For all SemanticReprs where retrieval_unit_id IN (step 4):
     Mark INVALID.
6. For all Relationships where source_entity_id = this file's occurrences:
     Mark INVALID.
7. For all Evidence records where source_id = this file:
     Mark STALE (evidence may still exist; freshness_state updated).
8. For all KnowledgeItems where file_occurrence_id = this file:
     Mark INVALID.
```

Cross-repository propagation:
- Relationships from other repositories pointing to an invalidated entity
  are marked STALE (not INVALID), since the target may be rebuilt.
- After recomputation of the target, dependent cross-repo relationships
  are re-evaluated.

---

## Invalidation Records

Every invalidation event writes a row to `core_invalidation_records`:

```sql
INSERT INTO core_invalidation_records (
    id, artifact_type, artifact_id, reason, invalidated_at, recomputed_at
) VALUES (?, ?, ?, ?, ?, NULL);
```

Recomputation completion updates `recomputed_at`:

```sql
UPDATE core_invalidation_records
SET recomputed_at = ?
WHERE artifact_id = ? AND recomputed_at IS NULL;
```

Pending recomputation is queryable:

```sql
SELECT * FROM core_invalidation_records WHERE recomputed_at IS NULL;
```

---

## Invalidation vs. Recomputation Separation

**Invalidation** is synchronous and cheap:
- Marks state flags in DB rows.
- Writes `core_invalidation_records` rows.
- Does NOT trigger any parsing, analysis, or re-indexing.
- Does NOT block reads on non-invalidated artifacts.

**Recomputation** is asynchronous and expensive:
- Scheduled by the task system as `ops_tasks` rows.
- Priority based on artifact type and repository importance.
- Readers may observe STALE/INVALID artifacts between invalidation and
  recomputation completion.
- STALE artifacts are served with a staleness caveat.
- INVALID artifacts are not served.

---

## Scoped Invalidation Examples

### File content change

```
Trigger: FileOccurrence.content_hash changes
Scope:
  FileOccurrence → STALE (new occurrence created for new revision)
  StructuralNodes → INVALID
  SymbolOccurrences → INVALID
  RetrievalUnits → INVALID
  SemanticReprs → INVALID
  Relationships (source = this file) → INVALID
  Evidence (source = this file) → STALE
Repository scope: single file in single repository
Other repositories: unaffected unless cross-repo relationships point here
```

### Analyzer version upgrade (Java only)

```
Trigger: IndexGeneration.analyzer_versions["java-treesitter"] changes
Scope:
  All StructuralNodes where analyzer_id = "java-treesitter" → INVALID
  All SymbolOccurrences derived from those nodes → INVALID
  All Relationships derived from those nodes → INVALID
  RetrievalUnits: unaffected (segmentation unchanged)
  SemanticReprs: unaffected (retrieval units unchanged)
  Python/Go/etc. artifacts: completely unaffected
```

### Segmentation version change

```
Trigger: IndexGeneration.segmentation_version changes
Scope:
  All RetrievalUnits → INVALID
  All SemanticReprs → INVALID
  StructuralNodes: unaffected
  SymbolOccurrences: unaffected
```

### Embedding model change

```
Trigger: IndexGeneration.embedding_model_version changes
Scope:
  All SemanticReprs → INVALID
  Everything else: unaffected
```

### Discovery policy change

```
Trigger: DiscoveryPolicy.discovery_policy_hash changes
Scope:
  New SourceRevision required
  Re-discovery run for repository
  Newly excluded files: FileOccurrence.existence_state → EXCLUDED
  Newly included files: new FileOccurrence → full pipeline
  Priority changes only: re-prioritize in queue; no re-index
```

---

## Invariants

1. An artifact in state INVALID is never returned as valid evidence.
2. An artifact in state STALE may be returned with `freshness_state = STALE`
   in the evidence object; the caller can observe the staleness.
3. Invalidation propagation is complete before recomputation begins.
   Recomputation never starts on an artifact whose dependencies are still
   being invalidated.
4. The invalidation DAG is acyclic. Cycle detection is enforced at schema
   definition time, not at runtime.
5. Invalidation records are written atomically with the state flag updates
   in the same DB transaction.
6. No artifact is silently deleted due to invalidation. Invalidated rows
   remain in the DB until explicitly pruned (future maintenance phase).

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Invalidation transaction fails | Roll back; retry; log `INVALIDATION_FAILED` |
| Recomputation fails | Artifact stays INVALID; retry via task scheduler |
| Cycle detected in DAG during propagation | Log `DAG_CYCLE_DETECTED`; halt propagation; open question filed |
| Cross-repo target unreachable during propagation | Mark relationship STALE; continue; log diagnostic |
| Invalidation records table grows unbounded | Pruning ops_task scheduled; pending records never pruned |

---

## Observability

Invalidation event log:

```
trigger_type: SOURCE_CHANGED | ANALYZER_UPGRADED | ...
repository_id
artifact_type
artifact_count_invalidated
propagation_depth_max
duration_ms
pending_recomputation_count
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| INV-01 | File content changes | FileOccurrence STALE; StructuralNodes, SymbolOccurrences, RetrievalUnits INVALID |
| INV-02 | Java analyzer version bump | Java structural/symbol artifacts INVALID; Python artifacts unaffected |
| INV-03 | Segmentation version change | RetrievalUnits + SemanticReprs INVALID; StructuralNodes unaffected |
| INV-04 | Embedding model change | SemanticReprs INVALID only |
| INV-05 | File deleted | FileOccurrence DELETED; all derived artifacts INVALID |
| INV-06 | Discovery policy change | New SourceRevision; newly excluded files get EXCLUDED state |
| INV-07 | Cross-repo relationship when target invalidated | Relationship marked STALE; source repo unaffected |
| INV-08 | Invalidation during active query | Query observes STALE/INVALID artifacts with freshness metadata |
| INV-09 | Recomputation completes | Artifact transitions to CURRENT; invalidation record updated |
| INV-10 | Two rapid file changes | Both produce invalidation records; recomputation uses latest content |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| INV-Q1 | Should STALE artifacts be served without caveat during a background reindex in progress, or should all callers receive explicit staleness metadata? | No — always serve staleness metadata; caller decides |
| INV-Q2 | Should evidence records be deleted when INVALID, or kept with INVALID state for audit? | No — keep with INVALID state for V1; delete in maintenance pass |
| INV-Q3 | Maximum propagation depth to prevent runaway cross-repo cascades? | No — depth cap of 10 hops for V1; configurable |
