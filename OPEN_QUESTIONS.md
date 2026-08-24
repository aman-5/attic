# Open Questions — Phase 0
## Document: OPEN_QUESTIONS.md
## Status: LIVING DOCUMENT — update as questions are resolved or added

Per the operating manual: all requirements that could not be determined from the canonical plan are recorded here rather than invented. Each question is assigned to the phase that owns the decision. Phase 1A owns storage and domain types only; later phases own watcher, MCP transport, retrieval, semantic intelligence, and cross-repository decisions.

---

## Legend

| Status | Meaning |
|---|---|
| OPEN | No decision made yet |
| DEFERRED | Intentionally deferred to a specific later phase |
| RESOLVED | Decision recorded in the linked document |

---

## Phase-to-Question Map

| Phase | Questions owned |
|---|---|
| Phase 1A (storage + domain types) | OQ-006, OQ-007, OQ-014, OQ-016 |
| Phase 1B (discovery + watcher) | OQ-004, OQ-005, OQ-009, OQ-010, OQ-015 |
| Phase 1D (MCP server + transport) | OQ-003 (RESOLVED) |
| Phase 2 (indexing + knowledge) | OQ-008 |
| Phase 4 (retrieval) | OQ-011, OQ-012 |
| Phase 5 (semantic intelligence) | OQ-001, OQ-002, OQ-013 |

---

## OQ-001 — Semantic Embedding Model Identity

**Status**: DEFERRED (Phase 5)  
**Source contract**: `docs/contracts/answer_modes.md` §2 (DEEP mode `semantic_allowed = true`), `docs/contracts/retrieval_plan.md` §2 (SubsystemTag::SEMANTIC_SEARCH)  
**Question**: Which embedding model is used for semantic search in DEEP mode? Is it a local model (e.g., ONNX runtime), a remote API call, or configurable? What is the vector dimensionality, similarity metric, and index structure?  
**Impact**: Determines `attic-indexing` ML dependencies, whether `core_retrieval_units` needs an embedding vector column, and whether an optional network dependency is introduced.  
**Deferral rationale**: No vector storage, model loading, or semantic search code is introduced before Phase 5. Phase 1A must not add vector columns or ML dependencies. Offline-first principle must be preserved as the default.  
**Phase 5 entry action**: Decide model identity and record in `docs/decisions/` before Phase 5 storage schema changes.

---

## OQ-002 — Re-ranking Model Identity

**Status**: DEFERRED (Phase 5)  
**Source contract**: `docs/contracts/answer_modes.md` §2 (NORMAL/DEEP mode `reranking_allowed = true`)  
**Question**: What is the re-ranking model — cross-encoder, BM25 variant, or hand-tuned scoring function?  
**Impact**: Determines `attic-retrieval` complexity and whether a separate ML inference subsystem is needed.  
**Deferral rationale**: Re-ranking requires a retrieval layer (Phase 4) and potentially an embedding model (Phase 5). Neither exists in Phase 1A. A BM25 + recency + authority heuristic is sufficient for Phase 4; a learned re-ranker is Phase 5+.  
**Phase 5 entry action**: Resolve alongside OQ-001.

---

## OQ-003 — MCP Transport Protocol

**Status**: RESOLVED  
**Source contract**: `docs/contracts/retrieval_plan.md` §1, `benchmarks/acceptance.md` §3.1  
**Resolution**: The primary initial MCP transport is **stdio** (subprocess model). This belongs to Phase 1D implementation. Additional transports (HTTP/SSE, WebSocket) are optional future work and do not affect Phase 1A–1C. No Phase 1A decision is required.  
**Document**: Record in `docs/decisions/` when Phase 1D begins.

---

## OQ-004 — Git Submodule Handling in DiscoveryPolicy

**Status**: OPEN  
**Owning phase**: Phase 1B  
**Source contract**: `docs/contracts/discovery.md` §3 (GlobRule), `docs/contracts/source_revision.md` §2  
**Question**: Are Git submodules treated as separate repositories (each with their own `SourceRevision`) or as part of the parent repository's working tree? Does `WorkspaceSnapshot` automatically include all submodule revisions?  
**Impact**: Affects manifest hash algorithm (currently unspecified for submodules), the discovery walk implementation, and `core_repositories` row count per workspace.  
**Suggested resolution**: Treat each submodule as a separate `core_repositories` entry with its own `SourceRevision`. Add to source_revision.md §2.5 when resolved.

---

## OQ-005 — Dirty Working-Tree Manifest Hash Stability

**Status**: OPEN  
**Owning phase**: Phase 1B  
**Source contract**: `docs/contracts/source_revision.md` §2.3 (manifest hash algorithm), test SR-07  
**Question**: If a file is modified and then restored to its committed content without a commit, does the manifest hash return to the clean-tree value? What is the watcher debounce interval?  
**Impact**: Affects `attic-discovery` watcher implementation and `SourceRevision` invalidation frequency.  
**Suggested resolution**: Manifest hash must reflect actual content; restored files produce the same hash as the clean tree. Debounce interval is a configuration value; record default in `docs/decisions/`.

---

## OQ-006 — SQLite WAL Checkpoint Trigger Policy

**Status**: RESOLVED  
**Owning phase**: Phase 1A  
**Source contract**: `docs/contracts/storage.md` §5 (WAL mode), `docs/contracts/recovery.md` §4 (backup policy REC-B1)  
**Resolution**: `PRAGMA wal_autocheckpoint = 1000` (PASSIVE, frame-count trigger) on the writer connection; DB Writer background loop issues `PRAGMA wal_checkpoint(PASSIVE)` every 5 minutes; BACKUP task uses `PRAGMA wal_checkpoint(FULL)` before file copy. No schema changes. See `docs/decisions/ADR-001-wal-checkpoint-policy.md`.

---

## OQ-007 — Secret Detector Pattern Versioning and Rollout

**Status**: RESOLVED  
**Owning phase**: Phase 1A  
**Source contract**: `docs/contracts/secrets.md` §3 (V1 baseline patterns), `docs/contracts/recovery.md` R-6  
**Resolution**: Added `secret_pattern_version INTEGER NOT NULL DEFAULT 1` to `core_file_occurrences` and `secret_detector_version INTEGER NOT NULL DEFAULT 1` to `core_index_generations`. Pattern version bumps trigger `PARTIALLY_REBUILDABLE`; re-scan scheduler marks affected files `PENDING`. See `docs/decisions/ADR-002-secret-pattern-versioning.md` and updated `migrations/0001_initial.sql`.

---

## OQ-008 — `core_knowledge_items` Population Source

**Status**: OPEN  
**Owning phase**: Phase 2  
**Source contract**: `docs/contracts/storage.md` §6 (core_knowledge_items table), `docs/contracts/evidence.md` §2 (SourceType::KNOWLEDGE_BASE)  
**Question**: What is the authoritative source for `core_knowledge_items`? Specifically: (a) `knowledge/*.md` files are the first-class `KnowledgeItem` source; (b) README and `docs/` Markdown are documentation evidence (`SourceType::DOCUMENTATION`) and must not be automatically promoted to `KnowledgeItem`; (c) LLM-generated summaries are deferred to Phase 5.  
**Impact**: Affects the Phase 2 indexing pipeline. The distinction between knowledge (explicit, first-class) and documentation (contextual evidence) must be enforced from Phase 2 onward.  
**Phase 2 entry action**: Define the `knowledge/` directory convention, the `knowledge_type` taxonomy, and the ingest pipeline. Confirm that `docs/` Markdown is indexed as documentation evidence only, not as KnowledgeItems. LLM summarization is not introduced until Phase 5.

---

## OQ-009 — FTS5 Tokenizer Selection

**Status**: OPEN  
**Owning phase**: Phase 1B  
**Source contract**: `docs/contracts/storage.md` §7 (FTS5 configuration), `migrations/0001_initial.sql` §12 (`tokenize='unicode61'`)  
**Question**: Is `unicode61` adequate for code search? Code identifiers (camelCase, snake_case) may benefit from a tokenizer that splits on case transitions and underscores.  
**Impact**: Affects search quality for DEFINITION_LOOKUP and SYMBOL_NAVIGATION benchmark cases; may require a custom SQLite extension.  
**Suggested resolution**: Default `unicode61` for Phase 1A (no change to migration). Evaluate SQLite `trigram` tokenizer (available ≥ 3.44) for symbol names in Phase 1B. Record evaluation in `docs/decisions/`.

---

## OQ-010 — Maximum Workspace Size Operational Limits

**Status**: OPEN  
**Owning phase**: Phase 1B  
**Source contract**: `docs/contracts/large_files.md` §2, `docs/contracts/resources.md` §3  
**Question**: What are the per-workspace operational limits on number of repositories, files, and symbols? Are these enforced at admission time or advisory?  
**Suggested resolution**: Phase 1A target (advisory, not enforced): ≤ 50 repositories, ≤ 2 million files, ≤ 20 million symbols. Hard limits and admission enforcement are Phase 1B decisions.

---

## OQ-011 — `max_context_tokens` Scope and Tokenizer

**Status**: OPEN  
**Owning phase**: Phase 4  
**Source contract**: `docs/contracts/answer_modes.md` §2 (`max_context_tokens`), `docs/contracts/retrieval_plan.md` §2 (`context_tokens`)  
**Question**: Does `max_context_tokens` cover evidence context only or include answer text? Which tokenizer is used?  
**Impact**: Affects `attic-retrieval` assembly step. No tokenizer dependency is introduced before Phase 4.  
**Suggested resolution**: `max_context_tokens` covers evidence context only. Answer generation budget belongs to the calling LLM. Use word-count approximation (1 token ≈ 4 chars) for Phase 4 initial implementation; replace with proper tokenizer in Phase 5 if needed.

---

## OQ-012 — `CancellationToken` Implementation Backing Type

**Status**: OPEN  
**Owning phase**: Phase 4  
**Source contract**: `docs/contracts/resources.md` §5 (CancellationToken, RC-C1 through RC-C4)  
**Question**: What Rust type backs the `CancellationToken` abstraction for cross-task cancellation propagation in Tokio?  
**Impact**: Affects `attic-core` task abstraction when async tasks are introduced. Not required for Phase 1A (storage layer only; no async tasks spawned).  
**Suggested resolution**: Use `tokio_util::sync::CancellationToken` as the backing type, wrapped in a newtype. Record in `docs/decisions/` at Phase 4.

---

## OQ-013 — Analyzer Plugin Hot-Reload

**Status**: DEFERRED (Phase 5)  
**Source contract**: `docs/contracts/analyzers.md` §4 (AnalyzerRegistry)  
**Question**: Can analyzers be added, removed, or updated at runtime without restarting the server?  
**Deferral rationale**: Requires dynamic library loading or an out-of-process model. Phase 1A analyzers are statically registered at compile time. Hot-reload is Phase 5+.

---

## OQ-014 — `ops_server_state` Single-Row Invariant Enforcement

**Status**: RESOLVED  
**Owning phase**: Phase 1A  
**Source contract**: `migrations/0001_initial.sql` §13 (`ops_server_state` table), `docs/contracts/recovery.md` §7  
**Resolution**: Added `CHECK (id = 'singleton')` inline on the `id` column in the `CREATE TABLE` DDL. Application layer always upserts with `id = 'singleton'`. See `docs/decisions/ADR-003-ops-server-state-constraint.md` and updated `migrations/0001_initial.sql`.

---

## OQ-015 — Benchmark Fixture Repository Identity

**Status**: OPEN  
**Owning phase**: Phase 1B  
**Source contract**: `benchmarks/cases/q001_to_q050.md`, `benchmarks/cases/q051_to_q100.md`, `fixtures/git/`  
**Question**: What are the actual fixture repositories used to evaluate the 100 benchmark cases? Synthetic workspace, real open-source projects, or a combination?  
**Impact**: Determines `fixtures/git/` content and CI network access requirements.  
**Suggested resolution**: Phase 1B: create a minimal synthetic Rust workspace in `fixtures/git/` with enough structure to cover all 100 cases. Avoid real external repositories in CI.

---

## OQ-016 — `IndexGeneration` Per-Subsystem Version Tracking

**Status**: RESOLVED  
**Owning phase**: Phase 1A  
**Source contract**: `docs/contracts/compatibility.md` §3 (PARTIALLY_REBUILDABLE)  
**Resolution**: Added `subsystem_versions_json TEXT NOT NULL` to `core_index_generations`. This JSON map (subsystem key → version string) is the consolidated comparison target for compatibility checks. `PARTIALLY_REBUILDABLE` = ≥ 1 non-schema subsystem version changed. Subsystem key constants defined in `attic-core`. See `docs/decisions/ADR-004-index-generation-subsystem-versions.md` and updated `migrations/0001_initial.sql`.

---

## Phase 1A Blockers (storage and domain types only)

The following questions must be resolved before Phase 1A implementation is merged:

| OQ | Question | Why Phase 1A | Status |
|---|---|---|---|
| OQ-006 | WAL checkpoint trigger policy | Affects `attic-storage` connection management implementation | **RESOLVED** — ADR-001 |
| OQ-007 | Secret detector pattern versioning | Requires `secret_pattern_version` column addition to migration SQL | **RESOLVED** — ADR-002 |
| OQ-014 | `ops_server_state` single-row enforcement | Minor migration SQL hardening needed before the table is used | **RESOLVED** — ADR-003 |
| OQ-016 | `IndexGeneration` per-subsystem versions | Requires `subsystem_versions` JSON column in `core_index_generations` | **RESOLVED** — ADR-004 |

All other questions belong to later phases and do not block Phase 1A.

---

## Resolution Procedure

When an open question is resolved:
1. Update this file: change `Status` to `RESOLVED`, add a `Resolution` field with a one-line summary and a link to the document where the decision was recorded.
2. Update the relevant contract document(s) to reflect the decision.
3. If the resolution changes a schema or invariant, update `migrations/0001_initial.sql` and note the migration version change.
4. Record the decision in `docs/decisions/` with full rationale.
