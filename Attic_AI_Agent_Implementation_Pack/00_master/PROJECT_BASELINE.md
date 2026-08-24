# Attic Project Baseline

## 1. Product boundary

Attic is a local/workspace-oriented code intelligence MCP that provides evidence-backed answers across 25–30 repositories.

The repository filesystem/Git state is authoritative. SQLite, FTS, graph data, embeddings, summaries, and caches are derived.

## 2. Runtime choice

Core implementation: **Rust**.

Node.js >=20 is a developer/tooling baseline only. Do not rewrite the server in TypeScript because Node exists.

## 3. MCP dependency baseline

Use the official `modelcontextprotocol/rust-sdk` crate `rmcp`.

As of the package preparation date, the official workspace is on `rmcp` 3.0.1, Rust edition 2024, with minimum Rust 1.88. The official SDK supports the current MCP 2026-07-28 protocol while retaining compatibility with older supported versions.

Before creating `Cargo.toml`:

1. verify the official repository/crate;
2. verify current stable release;
3. verify MSRV;
4. verify required server/stdio feature flags;
5. record the result in `docs/decisions/DEPENDENCIES.md`;
6. add the dependency with Cargo;
7. commit `Cargo.lock`.

Do not use a fork merely because a search result has a similar README.

## 4. Initial repository layout

```text
attic/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── rustfmt.toml
├── clippy.toml
├── .gitignore
├── README.md
├── crates/
│   ├── attic-server/
│   ├── attic-core/
│   ├── attic-storage/
│   ├── attic-discovery/
│   ├── attic-analyzers/
│   ├── attic-indexing/
│   ├── attic-retrieval/
│   ├── attic-evidence/
│   └── attic-test-support/
├── migrations/
├── config/
├── docs/
│   ├── architecture/
│   ├── contracts/
│   └── decisions/
├── fixtures/
├── benchmarks/
└── tools/
```

Avoid a single giant crate. Avoid creating dozens of micro-crates beyond the approved boundaries.

## 5. Crate boundaries

### attic-core
Pure domain model:
- IDs
- SourceRevision
- WorkspaceSnapshot
- IndexGeneration
- file/symbol identity and occurrences
- freshness states
- Evidence
- RetrievalPlan
- AnswerModePolicy

Must not depend on SQLite, MCP transport, Tree-sitter, notify, or vector implementations.

### attic-storage
- SQLite connection/config
- migrations
- repositories/DAOs
- FTS5
- transaction coordinator
- write queue integration

### attic-discovery
- repository discovery
- Git-aware ignore behavior
- path canonicalization
- security boundary checks
- file classification
- source manifest capture helpers

### attic-analyzers
- Analyzer trait/API
- AnalyzerRegistry
- GenericAnalyzer
- format/language analyzers
- Tree-sitter adapters when Phase 3 reaches them

### attic-indexing
- orchestration
- SourceRevision capture
- indexing tasks
- invalidation DAG
- freshness transitions
- checkpoints

### attic-retrieval
- query classification
- RetrievalPlanner
- lexical/symbol/structural retrievers
- candidate fusion
- graph/semantic adapters later
- context building

### attic-evidence
- evidence conversion
- ranking signals
- validation
- sufficiency
- contradiction handling
- claim/evidence verification

### attic-server
- MCP protocol adapter
- tool definitions
- transport setup
- configuration/bootstrap
- dependency wiring

Business logic must not live inside MCP handler methods.

## 6. Initial dependency categories

Do not add all dependencies on day one. Add only when the relevant phase starts.

Expected categories:

| Need | Candidate family | Phase |
|---|---|---|
| MCP | official `rmcp` | Bootstrap/1D |
| async | `tokio` | Bootstrap |
| serialization | `serde`, `serde_json` | Bootstrap |
| errors | `thiserror`; `anyhow` only at application boundaries if desired | Bootstrap |
| logging | `tracing`, subscriber | Bootstrap |
| SQLite | choose maintained Rust SQLite binding during Phase 0/1A | 1A |
| hashing | maintained cryptographic/fast hash crate chosen by contract | 0/1B |
| Git ignore walking | `ignore`-style Git-aware walker or verified equivalent | 1B |
| file watch | `notify` or verified equivalent | 2 |
| parsing | `tree-sitter` + grammar crates | 3 |
| CLI/config | verified lightweight crates if needed | Bootstrap |
| vectors | only after Phase 5 gate | 5 |

The agent must not assume a crate feature or API from memory.

## 7. Prohibited early infrastructure

Before benchmark evidence proves need, do not add:

- Elasticsearch
- OpenSearch
- Neo4j
- Redis
- Kafka
- external vector database
- distributed worker system
- Kubernetes dependency
- mandatory Docker runtime
- cloud-only service
