# Phase 2 — Incremental Correctness and Freshness

## Goal
Changes update only affected artifacts, without ghost evidence.

## Sequence
1. filesystem watcher abstraction;
2. early ignore filtering;
3. debounce/coalesce;
4. hash/manifest comparison;
5. file lifecycle transitions;
6. invalidation DAG;
7. stale marking;
8. scheduled recomputation;
9. checkpoints/recovery;
10. source revision publication.

## Critical invariant
Invalidation != recomputation.

On change:
```text
detect
→ identify affected source artifact
→ mark dependent artifacts stale/invalid
→ publish state
→ scheduler recomputes according to priority/budget
```

## Required scenarios
- edit method;
- add file;
- delete file;
- rename file;
- modify `.gitignore`;
- change knowledge file;
- parser/analyzer version change;
- crash after invalidation before recompute;
- crash during recompute;
- source changes again while recompute runs.

## Freshness
Evidence from stale/unknown artifacts cannot silently satisfy CURRENT requirements.

## Gate
No full workspace rebuild for ordinary file changes. No deleted/stale content returned as current.
