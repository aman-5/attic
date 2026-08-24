# Open Questions — Phase 0
## Document: OPEN_QUESTIONS.md
## Status: LIVING DOCUMENT — update as questions are resolved or added

Per the operating manual: all requirements that could not be determined from the canonical plan are recorded here rather than invented. Phase 1A implementors must resolve or explicitly defer each item before merging implementation code.

---

## Legend

| Status | Meaning |
|---|---|
| OPEN | No decision made yet |
| DEFERRED | Intentionally deferred to a specific later phase |
| RESOLVED | Decision recorded in the linked document |

---

## OQ-001 — Semantic Embedding Model Identity

**Status**: OPEN  
**Source contract**: `docs/contracts/answer_modes.md` §2 (DEEP mode `semantic_allowed = true`), `docs/contracts/retrieval_plan.md` §2 (SubsystemTag::SEMANTIC_SEARCH)  
**Question**: Which embedding model is used for semantic search in DEEP mode? Is it a local model (e.g., ONNX runtime, llama.cpp), a remote API call, or both depending on configuration? What is the vector dimensionality, similarity metric (cosine / dot product), and index structure (HNSW, IVF, flat)?  
**Impact**: Determines `attic-indexing` crate dependencies, storage schema additions (`core_retrieval_units.embedding_vector` column type and size), and whether an optional network dependency is introduced.  
**Suggested resolution**: Decide in Phase 1A planning; default to a local model to preserve the offline-first principle. Record in `docs/decisions/`.

---

## OQ-002 — Re-ranking Model Identity

**Status**: OPEN  
**Source contract**: `docs/contracts/answer_modes.md` §2 (NORMAL/DEEP mode `reranking_allowed = true`)  
**Question**: What is the re-ranking model? Is it a cross-encoder, a BM25 variant, or a hand-tuned scoring function? Is it the same model as OQ-001 or a separate one?  
**Impact**: Determines `attic-retrieval` crate complexity and whether a separate ML inference subsystem is needed.  
**Suggested resolution**: Resolve alongside OQ-001. A simple BM25 + recency + authority scoring function is sufficient for v0.5 beta; a learned re-ranker can be added for v1.0.

---

## OQ-003 — MCP Transport Protocol

**Status**: OPEN  
**Source contract**: `docs/contracts/retrieval_plan.md` §1 (latency measured "from query receipt at MCP transport layer"), `benchmarks/acceptance.md` §2  
**Question**: Which MCP transport is the primary target? `stdio` (subprocess), HTTP/SSE, or WebSocket? Does the server need to support multiple transports simultaneously?  
**Impact**: Determines `attic-server` crate structure, async runtime choices, and how `plan_id` / `query_id` are correlated with MCP request IDs.  
**Suggested resolution**: `stdio` transport first (simplest, no network required), HTTP/SSE as a second transport in Phase 1B. Record in `docs/decisions/`.

---

## OQ-004 — Git Submodule Handling in DiscoveryPolicy

**Status**: OPEN  
**Source contract**: `docs/contracts/discovery.md` §3 (GlobRule), `docs/contracts/source_revision.md` §2  
**Question**: Are Git submodules treated as separate repositories (each with their own `SourceRevision`) or as part of the parent repository's working tree? If separate, does `WorkspaceSnapshot` automatically include all submodule revisions?  
**Impact**: Affects manifest hash algorithm in source_revision.md (currently unspecified for submodules), the discovery walk implementation, and `core_repositories` row count per workspace.  
**Suggested resolution**: Treat each submodule as a separate repository entry in `core_repositories` with its own `SourceRevision`. Add this to source_revision.md §2.5 when resolved.

---

## OQ-005 — Dirty Working-Tree Manifest Hash Stability

**Status**: OPEN  
**Source contract**: `docs/contracts/source_revision.md` §2.3 (manifest hash algorithm), test SR-07  
**Question**: For dirty working trees, the manifest hash includes modified file content hashes. If a file is modified and then restored to its committed state without a commit, should the manifest hash return to the clean-tree value, or does it remain dirty until the next explicit snapshot? What is the polling interval for detecting working-tree changes?  
**Impact**: Affects `attic-discovery` watcher implementation and the frequency of `SourceRevision` invalidation.  
**Suggested resolution**: Manifest hash must reflect actual content; if file content matches the committed blob, the hash is identical to clean state. Watcher debounce interval is a configuration value (default 500 ms); record in `docs/decisions/`.

---

## OQ-006 — SQLite WAL Checkpoint Trigger Policy

**Status**: OPEN  
**Source contract**: `docs/contracts/storage.md` §5 (WAL mode), `docs/contracts/recovery.md` §4 (backup policy REC-B1)  
**Question**: What triggers a WAL checkpoint: time-based (every N seconds), size-based (WAL file exceeds N bytes), or both? Who initiates the checkpoint — the DB writer task, a dedicated maintenance task, or SQLite's automatic checkpointing?  
**Impact**: Affects `ops_tasks` scheduling, backup timing (REC-B1 requires checkpoint before backup copy), and recovery integrity.  
**Suggested resolution**: Use SQLite's `PRAGMA wal_autocheckpoint` with a threshold of 1000 pages (≈4 MB), supplemented by an explicit checkpoint in the BACKUP maintenance task. Record as a default in `docs/decisions/`.

---

## OQ-007 — Secret Detector Pattern Versioning and Rollout

**Status**: OPEN  
**Source contract**: `docs/contracts/secrets.md` §3 (V1 baseline patterns SE-01 through SE-10)  
**Question**: How are new secret patterns added without requiring a full re-scan of the entire corpus? Is there a `secret_pattern_version` column in `core_file_occurrences` to track which pattern version scanned each file? What triggers a re-scan when patterns are updated?  
**Impact**: Affects `core_index_generations` schema (adding a `secret_detector_version` dimension) and the recovery procedure step R-6.  
**Suggested resolution**: Add `secret_pattern_version INTEGER NOT NULL DEFAULT 1` to `core_file_occurrences`. Bump `IndexGeneration.secret_detector_version` when patterns change; `INCOMPATIBLE` if bumped to a non-migratable version. Record in compatibility.md when resolved.

---

## OQ-008 — `core_knowledge_items` Population Source

**Status**: OPEN  
**Source contract**: `docs/contracts/storage.md` §6 (core_knowledge_items table), `docs/contracts/evidence.md` §2 (SourceType::KNOWLEDGE_BASE)  
**Question**: What populates `core_knowledge_items`? Is it manually curated content, auto-extracted from README/docs files, generated by an LLM summarization step, or all three? Who owns the `knowledge_type` taxonomy?  
**Impact**: Affects whether `attic-indexing` needs an LLM summarization pipeline and what external dependencies are introduced.  
**Suggested resolution**: Phase 1A: populate only from README, CHANGELOG, and `docs/` Markdown files via a lightweight text extraction pass. LLM summarization is deferred to Phase 2. Record in `docs/decisions/`.

---

## OQ-009 — FTS5 Language / Tokenizer Selection

**Status**: OPEN  
**Source contract**: `docs/contracts/storage.md` §7 (FTS5 configuration), `migrations/0001_initial.sql` §12 (`tokenize='unicode61'`)  
**Question**: Is `unicode61` the correct tokenizer for code search? Code identifiers (camelCase, snake_case, PascalCase) may benefit from a custom tokenizer that splits on case transitions and underscores. Is the `porter` stemmer appropriate for symbol names?  
**Impact**: Affects search quality for DEFINITION_LOOKUP and SYMBOL_NAVIGATION benchmark cases; may require a custom SQLite extension.  
**Suggested resolution**: Default to `unicode61` for Phase 1A; evaluate trigram tokenizer (`trigram` available in SQLite ≥ 3.44) for symbol names. Record evaluation results in `docs/decisions/`.

---

## OQ-010 — Maximum Workspace Size Bounds

**Status**: OPEN  
**Source contract**: `docs/contracts/large_files.md` §2 (file size tiers), `docs/contracts/resources.md` §3 (ResourceBudget)  
**Question**: What is the maximum number of repositories, files, and symbols that a single Attic workspace is expected to handle? Are there hard limits that must be enforced at admission time (e.g., refuse to index a workspace with > 10 million files)?  
**Impact**: Affects DB schema index choices, `core_repositories` capacity, and whether sharding is needed.  
**Suggested resolution**: Phase 1A target: ≤ 50 repositories, ≤ 2 million files, ≤ 20 million symbols per workspace. Document as operational limits, not enforced schema constraints, for now.

---

## OQ-011 — `answer_budget_tokens` vs `max_context_tokens` Relationship

**Status**: OPEN  
**Source contract**: `docs/contracts/answer_modes.md` §2 (`max_context_tokens`), `docs/contracts/retrieval_plan.md` §2 (`context_tokens`)  
**Question**: `max_context_tokens` in `AnswerModePolicy` constrains the total tokens of evidence fed into answer assembly. Does this include the tokens used for the final answer text, or is that a separate budget? Is the token count measured by a specific tokenizer (tiktoken, HuggingFace tokenizer), and does it need to match the downstream LLM?  
**Impact**: Affects `attic-retrieval` assembly step and whether a tokenizer dependency is needed.  
**Suggested resolution**: `max_context_tokens` covers evidence context only; answer generation budget is out of scope for Attic (it belongs to the calling LLM). Use a simple word-count approximation (1 token ≈ 4 characters) for Phase 1A; replace with proper tokenizer in Phase 2.

---

## OQ-012 — `CancellationToken` Inter-Process Propagation

**Status**: OPEN  
**Source contract**: `docs/contracts/resources.md` §5 (CancellationToken, RC-C1 through RC-C4)  
**Question**: If a child task spawned by a parent task needs to be cancelled, how is the `CancellationToken` propagated across async task boundaries in Tokio? Is a `CancellationToken` from the `tokio-util` crate (or equivalent) the implementation target?  
**Impact**: Affects `attic-core` task abstraction implementation.  
**Suggested resolution**: Use `tokio_util::sync::CancellationToken` as the implementation backing type. Map the contract's `CancellationToken` to a newtype wrapper. Record in `docs/decisions/`.

---

## OQ-013 — Analyzer Plugin Hot-Reload

**Status**: DEFERRED (Phase 2)  
**Source contract**: `docs/contracts/analyzers.md` §4 (AnalyzerRegistry)  
**Question**: Can analyzers be added, removed, or updated at runtime without restarting the server? If so, what happens to in-progress indexing tasks that are using the old analyzer version?  
**Impact**: Would require dynamic library loading or an out-of-process analyzer model. High complexity.  
**Deferral rationale**: Phase 1A analyzers are statically registered at compile time. Hot-reload is Phase 2+.

---

## OQ-014 — `ops_server_state` Single-Row Invariant Enforcement

**Status**: OPEN  
**Source contract**: `migrations/0001_initial.sql` §13 (`ops_server_state` table), `docs/contracts/recovery.md` §7  
**Question**: `ops_server_state` is designed as a single-row table (one row per server instance). The schema uses `server_id TEXT PRIMARY KEY`. Should there be a `CHECK` constraint or a trigger to enforce that only one row ever exists, or is enforcement left to the application layer?  
**Impact**: Minor schema hardening question; affects migration SQL.  
**Suggested resolution**: Add `CHECK (server_id = 'singleton')` constraint and always insert/update with `server_id = 'singleton'`. Update migration in Phase 1A.

---

## OQ-015 — Benchmark Fixture Repository Identity

**Status**: OPEN  
**Source contract**: `benchmarks/cases/q001_to_q050.md`, `benchmarks/cases/q051_to_q100.md`, `fixtures/git/`  
**Question**: The benchmark cases reference a "reference Rust workspace" and "multi-repo workspace" as query targets. What are the actual fixture repositories? Are they synthetic (generated for testing), real open-source projects (e.g., `ripgrep`, `tokio`), or a combination?  
**Impact**: Determines `fixtures/git/` content and whether external network access is needed to clone fixture repos during CI.  
**Suggested resolution**: Phase 1A: create a minimal synthetic Rust workspace in `fixtures/git/` with enough structure to cover all 100 benchmark cases. Avoid real external repositories in CI to prevent flakiness.

---

## OQ-016 — `IndexGeneration` Hash Algorithm for Partial Rebuilds

**Status**: OPEN  
**Source contract**: `docs/contracts/compatibility.md` §3 (PARTIALLY_REBUILDABLE compatibility class)  
**Question**: When an `IndexGeneration` is `PARTIALLY_REBUILDABLE`, which subsystem versions have changed and which are still valid? Is there a per-subsystem hash within `IndexGeneration` to identify exactly which artifacts need rebuilding, or is it an all-or-nothing rebuild for the changed subsystem?  
**Impact**: Affects the `core_index_generations` schema and the invalidation propagation logic.  
**Suggested resolution**: Add a `subsystem_versions` JSON column to `core_index_generations` that maps subsystem name to its version hash. `PARTIALLY_REBUILDABLE` means at least one (but not all) subsystem versions changed. Record in compatibility.md when resolved.

---

## Resolution Procedure

When an open question is resolved:
1. Update this file: change `Status` to `RESOLVED`, add a `Resolution` field with a one-line summary and a link to the document where the decision was recorded.
2. Update the relevant contract document(s) to reflect the decision.
3. If the resolution changes a schema or invariant, update `migrations/0001_initial.sql` and increment the migration version.
4. Record the decision in `docs/decisions/` with full rationale.
