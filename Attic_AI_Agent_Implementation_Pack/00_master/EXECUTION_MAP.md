# Attic Execution Map

## Branching paths

The implementation has controlled branches, not arbitrary branching.

```text
Bootstrap
   |
Phase 0 contracts
   |
   +-----------------------+
   |                       |
SQLite binding choice   Hashing choice
(contracted)            (contracted)
   |                       |
   +-----------+-----------+
               |
Phase 1A storage
               |
Phase 1B discovery/security
               |
Phase 1C analyzer foundation
               |
Phase 1D MCP + FTS
               |
Phase 2 incremental
               |
Phase 3 structural
   |
   +-------------------------------+
   |              |                |
Java/Python     Go              JS/TS
analyzers       analyzer        analyzers
   |              |                |
   +--------------+----------------+
                  |
Phase 4 retrieval/evidence
                  |
           Non-semantic gate
                  |
           +------+------+
           |             |
     quality enough   quality gaps
           |             |
       Phase 5        diagnose first
       semantic       (do not assume
                       embeddings fix it)
```

## Path selection rules

### Additional languages
Add an analyzer only when:
- repositories contain the language;
- generic/structural support is insufficient for benchmark questions;
- tests/fixtures exist.

### Semantic retrieval
Do not start because "semantic search is modern." Start only after Phase 4 baseline exists.

### External DB/service
Do not introduce unless measured SQLite/local limitations are documented.

### HTTP transport
Start with stdio unless product requirements explicitly require HTTP. Keep transport isolated so it can be added later.

### Additional MCP tools
Do not expose internal services as tools. Keep external tool surface aligned with canonical plan.

## Gate ownership

A phase gate requires:
- tests green;
- no critical open questions;
- docs/contracts updated;
- benchmark slice run where specified;
- security invariants green;
- no architecture drift.
