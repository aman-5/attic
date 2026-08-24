# Phase 0 — Executable Contracts

## Goal
Turn approved architecture into precise implementation contracts.

## Required order

1. SourceRevision
2. WorkspaceSnapshot
3. IndexGeneration/compatibility
4. stable identity
5. SQLite schema/transactions
6. file lifecycle
7. discovery/ignore
8. secrets
9. Analyzer API
10. large-file behavior
11. invalidation DAG
12. freshness
13. Evidence
14. query taxonomy/evidence sufficiency
15. AnswerModePolicy
16. RetrievalPlan
17. resource enforcement
18. failure/recovery
19. benchmark dataset
20. acceptance gates

## Mandatory outputs

```text
docs/contracts/source_revision.md
docs/contracts/identity.md
docs/contracts/storage.md
docs/contracts/discovery.md
docs/contracts/secrets.md
docs/contracts/analyzers.md
docs/contracts/large_files.md
docs/contracts/invalidation.md
docs/contracts/evidence.md
docs/contracts/query_evidence.md
docs/contracts/answer_modes.md
docs/contracts/retrieval_plan.md
docs/contracts/resources.md
docs/contracts/recovery.md
docs/contracts/compatibility.md
migrations/0001_initial.sql
benchmarks/cases/*
benchmarks/acceptance.md
```

## Contract quality rule

Each contract must contain:
- purpose;
- definitions;
- invariants;
- data shape;
- algorithm/state machine where relevant;
- failure behavior;
- observability;
- examples;
- test matrix;
- unresolved questions.

## Gate
No critical unresolved item may be deferred into code with "we'll figure it out."
