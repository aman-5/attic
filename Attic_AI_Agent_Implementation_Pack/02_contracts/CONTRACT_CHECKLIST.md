# Phase 0 Contract Checklist

The agent must materialize these contracts in the Attic repository before dependent implementation.

## C01 SourceRevision
Define:
- eligible-file manifest algorithm;
- normalized path encoding;
- content hashing;
- file mode handling;
- dirty/untracked/deleted files;
- repository changes during capture;
- retry/unstable behavior.

## C02 WorkspaceSnapshot
Define atomicity expectations across multiple repositories. It is acceptable that repositories are captured sequentially if each SourceRevision is immutable and the snapshot records capture times; do not falsely claim simultaneous filesystem atomicity.

## C03 IndexGeneration
Define versions and compatibility:
- schema
- analyzer registry
- analyzer implementation
- segmentation
- indexer
- discovery policy
- ranking
- embeddings.

## C04 Identity
Define:
- FileIdentity vs FileOccurrence;
- SymbolIdentity vs SymbolOccurrence;
- exact vs heuristic continuity;
- rename/copy/move cases;
- confidence.

## C05 SQLite
Define actual tables, keys, indexes, foreign keys, FTS tables, transaction publication, WAL, busy handling, writer queue, migrations.

## C06 Discovery
Define:
- Git-aware behavior;
- nested `.gitignore`;
- `.git/info/exclude`;
- explicit Attic include/exclude;
- default exclusions;
- security exclusions;
- symlinks;
- non-Git workspaces.

## C07 Secrets
Define scanning/redaction point before derived persistence. Secret bytes must not enter FTS, embeddings, summaries, logs, telemetry, caches, or LLM context.

## C08 Analyzer API
Define:
- input;
- capabilities;
- output;
- diagnostics;
- cancellation;
- resource limits;
- generic fallback;
- version identity.

## C09 Invalidation DAG
Define artifact types, `derived_from`, state transitions, propagation, recomputation separation.

## C10 Evidence
Define canonical Evidence fields, source types, authority semantics, freshness, verification state.

## C11 Query Evidence Contracts
Define V1 query taxonomy and required/preferred evidence.

## C12 AnswerModePolicy
Define FAST/NORMAL/DEEP budgets as configuration, not scattered constants.

## C13 RetrievalPlan
Define serializable plan and observability trace.

## C14 Failure/Recovery
Define crash points and post-restart state.

## C15 Compatibility
Define migration/rebuild scope for each versioned subsystem.

A contract is not complete without examples and tests.
