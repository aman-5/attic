# Phase 1A — Storage Foundation

## Goal
Implement canonical persistence without retrieval sophistication.

## Sequence

### S1 DB configuration
- create DB under workspace `.attic/`;
- enable foreign keys;
- configure WAL;
- configure busy behavior;
- create separate read connections and one controlled writer path.

### S2 Migrations
Implement append-only migrations. Empty DB → latest schema must work.

### S3 Core repositories
Implement storage for:
- workspace/repository;
- SourceRevision;
- WorkspaceSnapshot;
- IndexGeneration;
- FileIdentity/FileOccurrence;
- StructuralNode;
- RetrievalUnit/RetrievalUnitNode;
- SymbolIdentity/SymbolOccurrence;
- Relationship;
- KnowledgeItem;
- Evidence metadata;
- artifact dependency/freshness;
- ops task/checkpoint state.

Do not add fields merely because they might be useful later.

### S4 Publication transaction
Define transaction boundary for publishing one coherent file indexing result.

A query must never see half of:
```text
new file occurrence
new structural nodes
old stale symbol set
```

### S5 FTS skeleton
Create FTS tables only according to contract. No secret or forbidden content may be inserted.

### S6 Writer queue
Analyzer/indexing work may be parallel; canonical DB writes pass through bounded coordination.

## Tests
- FK violations rejected;
- migration from empty;
- transaction rollback;
- concurrent readers;
- writer backpressure;
- deleted occurrence behavior;
- FTS insert/delete synchronization;
- DB reopen;
- WAL behavior.

## Gate
Storage tests green; no retrieval business logic in storage crate.
