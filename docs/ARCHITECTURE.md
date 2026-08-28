# Attic — Architecture

This document describes Attic **as it exists in the current codebase**, not
as a history of how it was built. For decision rationale on specific
technical choices, see `docs/decisions/` (ADRs). For normative behavioral
contracts (exact invariants, edge cases), see `docs/contracts/`.

## What Attic is

Attic is a local-first code-intelligence server. It indexes source
repositories into a single SQLite database and serves retrieval — full-text
search, bounded file reads, repository maps, health status, and
evidence-grounded question answering — over the Model Context Protocol
(MCP), so an MCP-capable AI client can ground its answers in real,
verifiable source rather than guessing.

The guiding separation, preserved throughout the pipeline:

```text
SOURCE  !=  INDEX  !=  RETRIEVAL CANDIDATE  !=  EVIDENCE  !=  CONTEXT  !=  ANSWER
```

Repositories on disk are always the source of truth. Every index, graph,
embedding, and cache is derived and disposable — Attic can always
reconstruct them from source (see `docs/PLAYBOOK.md` for reset/rebuild).

## Pipeline

```text
Workspace (ATTIC_WORKSPACE_ROOT)
   |
   v
Discovery + security          (attic-discovery)
   - gitignore-aware walk, path-traversal/symlink guards, secrets scan
   - Git submodules => one core_repositories entry per submodule (ADR-006)
   |
   v
SourceRevision / WorkspaceSnapshot   (attic-core domain types)
   - content-addressed identity (BLAKE3) for files and repositories
   |
   v
Analyzer Registry              (attic-analyzers)
   - GenericAnalyzer: universal fallback, every text file is searchable
   - structural analyzers (tree-sitter): Java, Python, Go, JavaScript,
     TypeScript get symbols/structure in addition to full-text search
   |
   v
Canonical model
   - files, retrieval units, structural nodes, symbols, relationships
   |
   v
Storage: SQLite + FTS5          (attic-storage)
   - one coordinated WriterQueue (single writer, serialized transactions)
   - DbPool of concurrent read-only connections (WAL mode)
   - attic-indexing publishes each indexing run atomically through the
     writer; no other code path writes index data
   |
   v
Incremental freshness / invalidation   (attic-incremental)
   - native filesystem watcher, falling back to periodic reconciliation
   - freshness states: CURRENT / STALE / UNKNOWN / pending refresh
   - startup recovery reconciles any interrupted work before serving
   |
   v
Retrieval Planner + candidate generation   (attic-retrieval)
   - Query Evidence Contract selects required evidence per query intent
     (definition/navigation/configuration/architecture/debugging/impact/
     dependency/test/knowledge)
   - candidates: lexical (FTS5) + symbol + structural + relationship graph
     + optional semantic
   |
   v
Evidence Manager                (attic-evidence)
   - verifies claims against retrieved evidence, assigns confidence,
     returns INSUFFICIENT_EVIDENCE explicitly rather than guessing
   |
   v
Cross-repository intelligence   (attic-crossrepo)
   - resolves dependency edges across submodule repositories once at
     startup; gates cross-repo-dependent answers while degraded
   |
   v
MCP surface                     (attic-server)
   - rmcp stdio transport; tools: file, search, repo_map, status, context
```

## Process and ownership model

- **One `attic-server` process owns one workspace.** A single process holds
  the one SQLite writer (`WriterQueue`), the one filesystem watcher, and the
  one MCP stdio transport for a given database file. This is not a
  configuration choice — `run_startup_recovery`, the watcher epoch, and
  `ops_server_state` all assume single-process ownership. Attic does **not**
  support multiple processes concurrently writing to the same database.
- **Multi-repository workspaces** are supported by pointing
  `ATTIC_WORKSPACE_ROOT` at a parent directory whose repositories are linked
  as Git submodules; each submodule becomes its own `core_repositories` row.
  Cross-repository dependency resolution (`attic-crossrepo::maintenance::
  sync_workspace`) runs once at startup, inside that single process.
- **Repository isolation / stable identity**: every repository, file, and
  retrieval unit has a stable, content-addressed identity independent of
  path, so renames and moves don't fragment history and cross-repository
  references resolve deterministically.

## Storage concurrency

SQLite runs in WAL mode: one dedicated writer connection processes all
mutations serially through `WriterQueue` (a bounded work queue drained by a
single worker thread, inside the writer's own transaction per publication);
any number of reader connections (`DbPool`) run concurrently against the
same file without blocking the writer or each other. There is no second
writer path anywhere in the codebase — `attic-indexing`, `attic-incremental`,
and `attic-crossrepo` all route mutations through the same `WriterQueueHandle`.

## Language support

Rich structural analysis (symbols, definitions, relationships) is currently
implemented for **Java, Python, Go, JavaScript, and TypeScript** via
tree-sitter grammars. Every other text-based language or format — Rust,
Swift, C++, Kotlin, config files, docs, build files, anything not on that
list — is **not** unsupported: it falls back to `GenericAnalyzer`, which
still makes it fully searchable via `search` and readable via `file`, just
without symbol-level structure. Rich language support is additive, not a
gate on usability.

## Project Knowledge authority model

Attic distinguishes two documentation tiers, enforced purely by path, in
`crates/attic-retrieval/src/candidates.rs::source_type_for_path`:

- **`knowledge/**`** — deliberately curated content → `EvidenceSourceType::
  Knowledge` → `AuthorityLevel::ProjectKnowledge`, the highest documentation
  authority Attic assigns.
- **Everything else** (`README.md`, any `docs/**`, an `ARCHITECTURE.md`
  living outside `knowledge/`, etc.) → `EvidenceSourceType::Documentation` →
  `AuthorityLevel::Doc`, a medium authority. Still fully indexed, searchable,
  and usable as evidence — just not equal to curated project knowledge.

This is an **authority** distinction, not an indexing exclusion: ordinary
documentation is never excluded from search or `context` results, it simply
doesn't carry the same weight when the evidence manager resolves
contradictions. The boundary is the `knowledge/` path prefix only —
filenames are never special-cased outside it. See `knowledge/README.md` in
this repository for the end-user-facing explanation and template, and
`docs/contracts/evidence.md` for the full `AuthorityLevel`/`EvidenceSourceType`
contract this implements.

## Known design limitations (not blocking, no action taken)

These are honest, currently-accurate statements about gaps between the
schema/contracts and the current implementation — not defects introduced by
this pass, and not silently resolved by it. Each needs a product decision
before being closed:

- **`core_knowledge_items` is schema-only.** The table exists (referenced by
  cascading invalidation in `attic-storage::invalidation_ops`) but nothing
  currently writes to it — knowledge evidence is derived directly from path
  classification (`knowledge/**`) against the standard FTS/file-occurrence
  data, not from this dedicated table. Populating it would enable richer
  semantics (explicit supersession chains, `applicable_versions`) but has no
  current consumer.
- **Rename detection is heuristic only.** `core_identity_links` supports a
  `GIT_RENAME`/`EXACT` basis in its schema, but no code path currently
  computes it — all renames/moves are recorded via `CONTENT_MATCH`
  (content-hash equality), a `HEURISTIC` confidence level. Evidence claims
  about "the same file across a rename" are therefore never `EXACT` today.
- **Java import resolution is source-layout-only.** Resolution uses
  `src/main/java/...`-style path candidates plus in-run symbol evidence, not
  `pom.xml`/`build.gradle` dependency-scope parsing. The relationship schema
  already carries `dependency_basis=MAVEN|GRADLE` for when this is added.
- **No automatic re-index on analyzer-version bump.** Each index generation
  records the running `ANALYZER_REGISTRY_VERSION`, but nothing diffs a
  stored generation's recorded version against the current one to schedule
  invalidation automatically — an analyzer upgrade requires a manual
  re-index (see `docs/PLAYBOOK.md` Maintenance). Republication does replace
  analyzer-derived artifacts wholesale once triggered.

## Semantic layer (optional, default-disabled)

Semantic (embedding-based) retrieval is **disabled by default** and only
activates when `ATTIC_SEMANTIC=1` is set. The currently shipped embedder
(`HashingEmbedder`) is a deterministic hashing baseline — an experimental
placeholder, not a validated neural embedding model. When disabled or
degraded, canonical (lexical/structural) retrieval is entirely unaffected;
the semantic layer never gates or blocks an answer (ADR-014, decision D1).
See ADR-013/ADR-014 for the full rationale.

## Resource management

A `ResourceMonitor` (`attic-storage::resource_manager`) tracks real process
RSS and enforces configurable budgets: total memory, foreground MCP query
concurrency, and background worker concurrency. Foreground (interactive MCP
calls) is never starved by background work (indexing, semantic enrichment) —
background capacity is capped strictly below foreground capacity, and under
memory pressure (`Pause`/`Emergency` advisories) expensive `DEEP` retrieval
mode is automatically downgraded to `NORMAL` rather than failing outright.

## Security

- Path traversal and symlink escapes are rejected before any file is read
  (`canonicalize_within_root`).
- A secrets-scanning layer redacts or excludes matched content before it can
  reach an MCP response, regardless of which tool requests it.
- `.git` internals are blocked at the server layer unconditionally.
- All MCP tool arguments are validated (length, character class, numeric
  bounds) before use; no raw string is interpolated into SQL — dynamic SQL
  uses compile-time-literal identifiers only.

## Crash recovery

On every startup, before serving any MCP request, Attic runs
`run_startup_recovery`: it resets orphaned tasks, reconciles any indexing run
that was interrupted mid-publication, and records the watcher epoch. This is
fail-closed — if recovery cannot establish a safe state, or the subsequent
database integrity check fails, the process refuses to serve rather than
present possibly-stale or corrupt data as `CURRENT`. On clean shutdown,
Attic performs an explicit WAL checkpoint and writes a crash-recovery backup
(most recent 3 retained) before exiting. See `docs/contracts/recovery.md`
for the full contract.

## MCP surface

Attic speaks MCP exclusively over stdio: **stdout carries only the MCP
JSON-RPC protocol; every log line goes to stderr** (`tracing`, controlled by
`ATTIC_LOG`/`RUST_LOG`). This has been verified by a smoke test that spawns
the release binary and inspects both streams directly. The five registered
tools (`file`, `search`, `repo_map`, `status`, `context`) are documented in
the README; their exact schemas are defined once in
`crates/attic-server/src/main.rs::make_tools()` and returned verbatim via
`tools/list` — that function is the single source of truth for the tool
surface.
