# Contract: Central Storage Architecture (SQLite)

## Purpose

Define the central SQLite database schema, table design, concurrency model,
WAL configuration, transaction semantics, writer queue, busy handling, and
migration bootstrap. This is the authoritative specification for
`workspace/.mcp/index.db`.

---

## Workspace Layout

```
workspace/
  .mcp/
    index.db          -- central SQLite database (this contract)
    index.db-wal      -- WAL file (auto-managed by SQLite)
    index.db-shm      -- shared memory file (auto-managed by SQLite)
    vectors/          -- optional vector storage (Phase 5)
    artifacts/        -- large derived artifact blobs
    cache/            -- transient caches (can be deleted)
    checkpoints/      -- crash recovery checkpoints
    state/            -- operational state files
    logs/             -- structured operational logs
```

---

## SQLite Configuration

On every connection open:

```sql
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;       -- WAL mode makes this safe and fast
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;        -- 5 seconds; see §Busy Handling
PRAGMA cache_size = -32768;        -- 32 MB page cache per connection
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 536870912;      -- 512 MB mmap for readers
```

WAL mode allows concurrent reads during writes. Only one writer at a time.

---

## Concurrency Model

```
Query Workers (N readers)
    |
    v
concurrent SQLite readers (WAL mode)


Analyzer / Index Workers (M workers)
    |
    v
produce mutations in memory
    |
    v
bounded write queue (single channel)
    |
    v
DB Writer / Transaction Coordinator (1 writer)
    |
    v
SQLite WAL writer
```

Rules:
1. Only the DB Writer thread executes `BEGIN IMMEDIATE` / `COMMIT`.
2. Analyzer workers send mutations to the write queue; they never open write
   transactions directly.
3. The write queue is bounded (configurable max depth, default: 512 pending
   mutations). Back-pressure is applied when the queue is full.
4. Read-only queries use separate reader connections; they never acquire the
   write lock.
5. The DB Writer batches small mutations into transactions of up to
   configurable size (default: 256 mutations or 50 ms, whichever comes first).

---

## Busy Handling

SQLite `PRAGMA busy_timeout = 5000` is set on all connections.

If a write transaction cannot acquire the lock within 5 seconds:
1. The DB Writer logs `DB_WRITE_TIMEOUT` with the pending mutation count.
2. The write is retried up to 3 times with exponential backoff (1s, 2s, 4s).
3. After 3 failures, the mutation is marked `FAILED` and a diagnostic is
   recorded. The system continues; failed mutations are retried at next
   scheduled opportunity.

Reader timeouts (5 seconds) result in `DB_READ_TIMEOUT` diagnostic and an
error returned to the caller. They are never silently retried by the reader.

---

## Table Namespace Convention

```
core_*    -- canonical intelligence / source state
ops_*     -- operational / task / scheduling state
```

`core_*` tables hold the durable canonical record of what has been indexed.
`ops_*` tables hold transient operational state that can be discarded and
rebuilt after a crash without losing canonical evidence.

---

## Core Tables

### core_schema_migrations

Tracks applied migration files.

```sql
CREATE TABLE IF NOT EXISTS core_schema_migrations (
    id          TEXT    NOT NULL PRIMARY KEY,  -- e.g., "0001_initial"
    applied_at  INTEGER NOT NULL               -- Unix timestamp microseconds
);
```

### core_repositories

```sql
CREATE TABLE IF NOT EXISTS core_repositories (
    id              TEXT    NOT NULL PRIMARY KEY,   -- UUID hex
    root_path       TEXT    NOT NULL UNIQUE,        -- canonical absolute path (UTF-8)
    display_name    TEXT    NOT NULL,
    is_git          INTEGER NOT NULL DEFAULT 1,     -- BOOLEAN
    case_sensitive  INTEGER NOT NULL DEFAULT 1,     -- BOOLEAN
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);
```

### core_source_revisions

```sql
CREATE TABLE IF NOT EXISTS core_source_revisions (
    id                          TEXT    NOT NULL PRIMARY KEY,
    repository_id               TEXT    NOT NULL REFERENCES core_repositories(id),
    commit_sha                  TEXT,               -- NULL if non-Git
    branch                      TEXT,               -- NULL if non-Git or detached
    working_tree_manifest_hash  TEXT    NOT NULL,
    discovery_policy_hash       TEXT    NOT NULL,
    unstable_capture            INTEGER NOT NULL DEFAULT 0,  -- BOOLEAN
    captured_at                 INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_source_revisions_repo
    ON core_source_revisions(repository_id, captured_at DESC);
```

### core_workspace_snapshots

```sql
CREATE TABLE IF NOT EXISTS core_workspace_snapshots (
    id          TEXT    NOT NULL PRIMARY KEY,
    created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS core_workspace_snapshot_revisions (
    snapshot_id         TEXT    NOT NULL REFERENCES core_workspace_snapshots(id),
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    PRIMARY KEY (snapshot_id, source_revision_id)
);
```

### core_index_generations

```sql
CREATE TABLE IF NOT EXISTS core_index_generations (
    id                        TEXT    NOT NULL PRIMARY KEY,
    source_revision_id        TEXT    NOT NULL REFERENCES core_source_revisions(id),
    schema_version            TEXT    NOT NULL,
    analyzer_registry_version TEXT    NOT NULL,
    analyzer_versions_json    TEXT    NOT NULL,   -- JSON object
    segmentation_version      TEXT    NOT NULL,
    indexer_version           TEXT    NOT NULL,
    discovery_policy_hash     TEXT    NOT NULL,
    ranking_version           TEXT    NOT NULL,
    embedding_model_version   TEXT,               -- NULL if no semantic index
    configuration_hash        TEXT    NOT NULL,
    created_at                INTEGER NOT NULL
);
```

### core_file_identities

```sql
CREATE TABLE IF NOT EXISTS core_file_identities (
    id              TEXT    NOT NULL PRIMARY KEY,
    repository_id   TEXT    NOT NULL REFERENCES core_repositories(id),
    stable_id_basis TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_file_identities_repo
    ON core_file_identities(repository_id);
```

### core_file_occurrences

```sql
CREATE TABLE IF NOT EXISTS core_file_occurrences (
    id                  TEXT    NOT NULL PRIMARY KEY,
    file_identity_id    TEXT    NOT NULL REFERENCES core_file_identities(id),
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    index_generation_id TEXT    REFERENCES core_index_generations(id),
    path                TEXT    NOT NULL,
    content_hash        TEXT    NOT NULL,
    size_bytes          INTEGER NOT NULL,
    language            TEXT,
    file_type           TEXT    NOT NULL,   -- SOURCE | CONFIG | DOCUMENT | INFRA | GENERATED | BINARY | UNKNOWN
    discovery_class     TEXT    NOT NULL,   -- IGNORED | LOW_PRIORITY | NORMAL | HIGH_PRIORITY
    security_state      TEXT    NOT NULL,   -- SAFE | FORBIDDEN | SECRET_REDACTED | PARTIALLY_REDACTED
    existence_state     TEXT    NOT NULL,   -- ACTIVE | DELETED | EXCLUDED | INACCESSIBLE | TOO_LARGE | BINARY | SECRET_REDACTED | PARSER_FAILED
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT',  -- CURRENT | STALE | UNKNOWN | INVALID | PENDING_REFRESH
    last_indexed_at     INTEGER
);

CREATE INDEX IF NOT EXISTS idx_file_occ_identity
    ON core_file_occurrences(file_identity_id);
CREATE INDEX IF NOT EXISTS idx_file_occ_revision
    ON core_file_occurrences(source_revision_id);
CREATE INDEX IF NOT EXISTS idx_file_occ_path
    ON core_file_occurrences(path, source_revision_id);
CREATE INDEX IF NOT EXISTS idx_file_occ_content_hash
    ON core_file_occurrences(content_hash);
```

### core_symbol_identities

```sql
CREATE TABLE IF NOT EXISTS core_symbol_identities (
    id              TEXT    NOT NULL PRIMARY KEY,
    repository_id   TEXT    NOT NULL REFERENCES core_repositories(id),
    language        TEXT    NOT NULL,
    qualified_name  TEXT    NOT NULL,
    kind            TEXT    NOT NULL,   -- FUNCTION | CLASS | INTERFACE | CONSTANT | TYPE | MODULE | FIELD | ENUM | ENUM_VARIANT | MACRO | OTHER
    disambiguator   TEXT                -- NULL if unambiguous
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_symbol_identity_unique
    ON core_symbol_identities(repository_id, language, qualified_name, kind, COALESCE(disambiguator, ''));

CREATE INDEX IF NOT EXISTS idx_symbol_identity_name
    ON core_symbol_identities(qualified_name);
```

### core_symbol_occurrences

```sql
CREATE TABLE IF NOT EXISTS core_symbol_occurrences (
    id                  TEXT    NOT NULL PRIMARY KEY,
    symbol_identity_id  TEXT    NOT NULL REFERENCES core_symbol_identities(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    source_revision_id  TEXT    NOT NULL REFERENCES core_source_revisions(id),
    source_span         TEXT    NOT NULL,   -- "start_line:start_col-end_line:end_col"
    signature           TEXT,
    visibility          TEXT,
    is_definition       INTEGER NOT NULL DEFAULT 0  -- BOOLEAN
);

CREATE INDEX IF NOT EXISTS idx_symbol_occ_identity
    ON core_symbol_occurrences(symbol_identity_id);
CREATE INDEX IF NOT EXISTS idx_symbol_occ_file
    ON core_symbol_occurrences(file_occurrence_id);
CREATE INDEX IF NOT EXISTS idx_symbol_occ_revision
    ON core_symbol_occurrences(source_revision_id);
```

### core_structural_nodes

```sql
CREATE TABLE IF NOT EXISTS core_structural_nodes (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    parent_id           TEXT    REFERENCES core_structural_nodes(id),
    node_type           TEXT    NOT NULL,
    structural_identity TEXT    NOT NULL,
    source_span         TEXT    NOT NULL,
    content_hash        TEXT    NOT NULL,
    analyzer_id         TEXT    NOT NULL,
    analyzer_version    TEXT    NOT NULL,
    metadata_json       TEXT,               -- analyzer-specific JSON
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT'
);

CREATE INDEX IF NOT EXISTS idx_structural_nodes_file
    ON core_structural_nodes(file_occurrence_id);
CREATE INDEX IF NOT EXISTS idx_structural_nodes_parent
    ON core_structural_nodes(parent_id);
```

### core_retrieval_units

```sql
CREATE TABLE IF NOT EXISTS core_retrieval_units (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    index_generation_id TEXT    NOT NULL REFERENCES core_index_generations(id),
    retrieval_text      TEXT    NOT NULL,   -- searchable text content
    lexical_state       TEXT    NOT NULL DEFAULT 'CURRENT',
    semantic_state      TEXT    NOT NULL DEFAULT 'NONE',   -- NONE | PENDING | CURRENT | STALE
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT'
);

CREATE INDEX IF NOT EXISTS idx_retrieval_units_file
    ON core_retrieval_units(file_occurrence_id);
CREATE INDEX IF NOT EXISTS idx_retrieval_units_generation
    ON core_retrieval_units(index_generation_id);
```

### core_retrieval_unit_nodes (normalized junction)

```sql
CREATE TABLE IF NOT EXISTS core_retrieval_unit_nodes (
    retrieval_unit_id   TEXT    NOT NULL REFERENCES core_retrieval_units(id),
    structural_node_id  TEXT    NOT NULL REFERENCES core_structural_nodes(id),
    ordinal             INTEGER NOT NULL,
    PRIMARY KEY (retrieval_unit_id, structural_node_id)
);

CREATE INDEX IF NOT EXISTS idx_run_by_node
    ON core_retrieval_unit_nodes(structural_node_id);
```

### core_relationships

```sql
CREATE TABLE IF NOT EXISTS core_relationships (
    id                    TEXT    NOT NULL PRIMARY KEY,
    source_repository_id  TEXT    NOT NULL REFERENCES core_repositories(id),
    source_entity_id      TEXT    NOT NULL,   -- file_occurrence_id or symbol_occurrence_id
    source_entity_type    TEXT    NOT NULL,   -- FILE_OCCURRENCE | SYMBOL_OCCURRENCE
    target_repository_id  TEXT    NOT NULL REFERENCES core_repositories(id),
    target_entity_id      TEXT    NOT NULL,
    target_entity_type    TEXT    NOT NULL,
    rel_type              TEXT    NOT NULL,   -- IMPORT | CALL | EXTENDS | IMPLEMENTS | DEPENDS_ON | etc.
    dependency_basis      TEXT    NOT NULL,   -- MAVEN | GRADLE | GO_MODULE | NPM | PYTHON_PACKAGE | IMPORT | HEURISTIC | etc.
    resolution            TEXT    NOT NULL,   -- SYNTACTIC | PACKAGE_RESOLVED | SYMBOL_RESOLVED | BUILD_RESOLVED | FRAMEWORK_RESOLVED | INFERRED
    confidence            REAL    NOT NULL DEFAULT 1.0,
    provenance_json       TEXT,
    source_revision_id    TEXT    NOT NULL REFERENCES core_source_revisions(id),
    freshness_state       TEXT    NOT NULL DEFAULT 'CURRENT'
);

CREATE INDEX IF NOT EXISTS idx_relationships_source
    ON core_relationships(source_entity_id);
CREATE INDEX IF NOT EXISTS idx_relationships_target
    ON core_relationships(target_entity_id);
CREATE INDEX IF NOT EXISTS idx_relationships_type
    ON core_relationships(rel_type);
```

### core_knowledge_items

```sql
CREATE TABLE IF NOT EXISTS core_knowledge_items (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    NOT NULL REFERENCES core_repositories(id),
    file_occurrence_id  TEXT    NOT NULL REFERENCES core_file_occurrences(id),
    source              TEXT    NOT NULL,   -- "knowledge/<path>"
    authority           TEXT    NOT NULL,   -- SOURCE_CODE | TEST | KNOWLEDGE | CONFIGURATION | RELATIONSHIP
    last_verified_at    INTEGER,
    applicable_versions TEXT,
    supersedes_id       TEXT    REFERENCES core_knowledge_items(id),
    confidence          REAL    NOT NULL DEFAULT 1.0,
    content_hash        TEXT    NOT NULL,
    freshness_state     TEXT    NOT NULL DEFAULT 'CURRENT'
);
```

### core_evidence

```sql
CREATE TABLE IF NOT EXISTS core_evidence (
    id                    TEXT    NOT NULL PRIMARY KEY,
    repository_id         TEXT    NOT NULL REFERENCES core_repositories(id),
    source_type           TEXT    NOT NULL,  -- SOURCE_CODE | TEST | CONFIGURATION | DOCUMENTATION | KNOWLEDGE | RELATIONSHIP | GENERATED_SOURCE
    source_id             TEXT    NOT NULL,  -- file_occurrence_id or knowledge_item_id
    path                  TEXT    NOT NULL,
    source_revision_id    TEXT    NOT NULL REFERENCES core_source_revisions(id),
    index_generation_id   TEXT    NOT NULL REFERENCES core_index_generations(id),
    source_span           TEXT,
    content_hash          TEXT    NOT NULL,
    freshness_state       TEXT    NOT NULL DEFAULT 'CURRENT',
    authority             TEXT    NOT NULL,
    confidence            REAL    NOT NULL DEFAULT 1.0,
    relationship_confidence REAL,
    verification_state    TEXT    NOT NULL DEFAULT 'UNVERIFIED',  -- UNVERIFIED | VERIFIED | STALE | CONTRADICTED
    ranking_signals_json  TEXT
);

CREATE INDEX IF NOT EXISTS idx_evidence_repo
    ON core_evidence(repository_id);
CREATE INDEX IF NOT EXISTS idx_evidence_revision
    ON core_evidence(source_revision_id);
CREATE INDEX IF NOT EXISTS idx_evidence_source
    ON core_evidence(source_id);
```

### core_invalidation_records

```sql
CREATE TABLE IF NOT EXISTS core_invalidation_records (
    id              TEXT    NOT NULL PRIMARY KEY,
    artifact_type   TEXT    NOT NULL,  -- FILE_OCCURRENCE | STRUCTURAL_NODE | SYMBOL_OCCURRENCE | RETRIEVAL_UNIT | SEMANTIC_REPR | RELATIONSHIP | EVIDENCE
    artifact_id     TEXT    NOT NULL,
    reason          TEXT    NOT NULL,  -- SOURCE_CHANGED | ANALYZER_UPGRADED | SCHEMA_MIGRATED | POLICY_CHANGED | EXPLICIT
    invalidated_at  INTEGER NOT NULL,
    recomputed_at   INTEGER            -- NULL until recomputed
);

CREATE INDEX IF NOT EXISTS idx_invalidation_artifact
    ON core_invalidation_records(artifact_type, artifact_id);
CREATE INDEX IF NOT EXISTS idx_invalidation_pending
    ON core_invalidation_records(recomputed_at)
    WHERE recomputed_at IS NULL;
```

---

## FTS5 Tables

### fts_retrieval_units

Full-text search over retrieval unit content.

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_retrieval_units USING fts5(
    retrieval_text,
    content='core_retrieval_units',
    content_rowid='rowid',
    tokenize='unicode61 remove_diacritics 1'
);
```

Note: `content=` mode (external content FTS5) is used to avoid duplicating
retrieval text. The `core_retrieval_units` table is the authoritative store;
the FTS index is a derived artifact that can be rebuilt.

FTS rebuild trigger: any time `core_retrieval_units` rows are inserted,
updated, or deleted, the FTS index must be updated via:

```sql
INSERT INTO fts_retrieval_units(fts_retrieval_units, rowid, retrieval_text)
    VALUES('delete', old.rowid, old.retrieval_text);
INSERT INTO fts_retrieval_units(rowid, retrieval_text)
    VALUES(new.rowid, new.retrieval_text);
```

(Handled by the DB Writer, not by triggers, to avoid SQLite trigger complexity.)

### fts_symbol_names

Full-text search over symbol qualified names for fuzzy lookup.

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_symbol_names USING fts5(
    qualified_name,
    kind,
    content='core_symbol_identities',
    content_rowid='rowid',
    tokenize='unicode61'
);
```

---

## Operational Tables

### ops_tasks

```sql
CREATE TABLE IF NOT EXISTS ops_tasks (
    id                  TEXT    NOT NULL PRIMARY KEY,
    repository_id       TEXT    REFERENCES core_repositories(id),
    task_type           TEXT    NOT NULL,   -- FULL_INDEX | INCREMENTAL_INDEX | SEMANTIC_ENRICH | etc.
    priority            INTEGER NOT NULL DEFAULT 50,
    state               TEXT    NOT NULL DEFAULT 'PENDING',  -- PENDING | RUNNING | DONE | FAILED | CANCELLED
    memory_budget_bytes INTEGER,
    cpu_budget_ms       INTEGER,
    timeout_ms          INTEGER,
    checkpoint_json     TEXT,
    retry_count         INTEGER NOT NULL DEFAULT 0,
    max_retries         INTEGER NOT NULL DEFAULT 3,
    created_at          INTEGER NOT NULL,
    started_at          INTEGER,
    completed_at        INTEGER,
    error_message       TEXT
);

CREATE INDEX IF NOT EXISTS idx_tasks_state_priority
    ON ops_tasks(state, priority DESC, created_at ASC)
    WHERE state = 'PENDING';
```

### ops_freshness_log

```sql
CREATE TABLE IF NOT EXISTS ops_freshness_log (
    id              TEXT    NOT NULL PRIMARY KEY,
    entity_type     TEXT    NOT NULL,
    entity_id       TEXT    NOT NULL,
    prior_state     TEXT    NOT NULL,
    new_state       TEXT    NOT NULL,
    changed_at      INTEGER NOT NULL,
    reason          TEXT
);
```

---

## Transaction Rules

1. All writes go through the single DB Writer thread via the write queue.
2. Reads may occur on any reader connection concurrently.
3. `core_*` tables are written only within explicit `BEGIN IMMEDIATE` /
   `COMMIT` transactions from the DB Writer.
4. `ops_*` tables may use shorter-lived transactions and can be cleared on
   crash recovery without data loss.
5. FTS index updates are always co-located in the same transaction as the
   corresponding `core_retrieval_units` / `core_symbol_identities` changes.
6. Foreign key constraints are enforced (PRAGMA foreign_keys = ON). Any
   constraint violation causes the enclosing transaction to roll back.

---

## Invariants

1. Every `core_*` table row with an FK to `core_source_revisions` has a valid,
   non-NULL `source_revision_id`.
2. `core_retrieval_units.retrieval_text` is NEVER the raw content of a
   SECRET_REDACTED file. Secret scanning happens before text enters this table.
3. `fts_retrieval_units` always reflects the current state of
   `core_retrieval_units`; inconsistency is detectable via the
   `integrity-check` operation.
4. No user-controlled string (path, symbol name, file content) is ever
   concatenated into SQL. All queries use parameter binding.
5. `core_evidence` rows are never updated in place; stale evidence rows are
   marked with `freshness_state = STALE` and new rows are inserted.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| WAL corruption detected at startup | Log `WAL_CORRUPT`; attempt `PRAGMA integrity_check`; if failed, trigger full rebuild |
| Foreign key violation | Transaction rolls back; mutation marked FAILED in write queue |
| FTS update fails | Transaction rolls back; FTS rebuild scheduled as ops_task |
| Write queue overflow | Back-pressure applied to indexing workers; `WRITE_QUEUE_FULL` logged |
| Disk full | Write transaction fails; `DISK_FULL` logged; indexing paused; not a crash |

---

## Observability

DB Writer emits periodic metrics:

```
write_queue_depth
mutations_per_second
transaction_batch_size (avg/max)
write_latency_p50_ms
write_latency_p99_ms
reader_connections_active
wal_size_bytes
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| ST-01 | Insert repository and source revision | FKs satisfied; rows readable from reader connection |
| ST-02 | Insert retrieval unit with secret content | Rejected; `SECRET_REDACTED` state; text not persisted |
| ST-03 | FTS query after retrieval unit insert | FTS returns correct row |
| ST-04 | FTS query after retrieval unit delete | FTS does not return deleted row |
| ST-05 | Concurrent reader + writer | Reader sees consistent snapshot (WAL isolation) |
| ST-06 | Write queue overflow simulation | Back-pressure applied; no data loss; no panic |
| ST-07 | FK violation attempt | Transaction rolls back; other rows unaffected |
| ST-08 | Migration applied | `core_schema_migrations` updated; schema updated |
| ST-09 | `PRAGMA foreign_keys = ON` enforced | Orphan row insert rejected |
| ST-10 | Retrieval unit text search | FTS5 returns ranked results |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| ST-Q1 | Should `retrieval_text` be stored in `core_retrieval_units` or referenced by span? For large files, span references save space but require source reads. | No — store text directly for V1; introduce span references if storage benchmarks justify |
| ST-Q2 | Should `ops_tasks` use a separate file (`ops.db`) to avoid WAL contention on the main DB? | No — single file for V1; split if write contention benchmarks justify |
| ST-Q3 | `fts5` tokenizer choice: `unicode61` vs `porter`? Porter adds stemming but makes exact match less precise. | No — `unicode61` for V1; Porter can be added as a secondary tokenizer |
