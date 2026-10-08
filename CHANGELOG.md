# Changelog

All notable changes to Attic are documented here.

## [Unreleased] — PR #27 (`feature-code-fix-6`)

Scope: 126 files, +26.6k / −7.1k lines against `main` (3 commits).

### Highlights

- **Foreground MCP requests are never refused for memory pressure.** The
  `server_busy` / "temporarily under memory pressure" rejection of `workspace`
  and `context` at Emergency is gone. Pressure now only throttles background
  work and degrades foreground depth.
- **Eleven languages upgraded to full Tier-1 analyzers** with import resolvers.
- **Semantic layer hardening:** bounded query-embedding wait with lexical
  fallback, honest availability diagnostics, verified ONNX assets, and a
  repository remove/re-add eviction race fix.
- **Security hardening:** nested `.git`/`.ssh` blocked at any depth, pinned
  model checksums, private IPC/files.
- **`attic-server/src/main.rs` split** from 9,080 to 4,127 lines with no
  behaviour change.

### Changed

#### Memory pressure and admission (`attic-server`, `attic-storage`)

- Removed the memory-tier rejection from the `call_tool` admission gate in
  `crates/attic-server/src/main.rs`, together with the now-dead
  `McpWorkClass` enum and `classify_mcp_tool`. Every tool (`status`, `logging`,
  `file`, `search`, `repo_map`, `context`, `workspace`) is admitted at every
  tier (Normal, Warning, Critical, Emergency).
- The only remaining foreground refusal is the concurrency-slot limit
  (`try_foreground`), which is flood protection, not memory policy.
- `DEEP` retrieval still downgrades to `NORMAL` under pressure (via the
  `ResourceAdvisory` plumbed into `handle_context`) — degrade, never refuse.
- Background gating is unchanged: the incremental scheduler, embedding
  enrichment and indexing-heavy permits still throttle or park under
  Warning / Critical / Emergency.
- `status.resource_pressure.mcp_pressure_rejections` is kept for schema
  stability; it now stays at 0.
- Pressure tiers gained absolute-headroom guards
  (`attic-storage/src/resource_manager.rs`): **Critical** needs ≥ 82 % used
  *and* < 8 GiB available; **Emergency** needs < 2 GiB available, or ≥ 90 %
  used *and* < 4 GiB available. A 32 GB machine at 84–87 % used no longer
  reaches Emergency by percentage alone.

#### Semantic search and embedding

- Query embedding waits a bounded time behind a running batch, then falls back
  to lexical search with an explicit reason (`SEMANTIC_QUERY_TIMED_OUT`).
- `status` gained a `semantic_availability` block (pressure tier, available
  RAM, whether enrichment is parked and why, coverage, and the reason search is
  or is not using embeddings).
- Embedding throughput is now measured where the work happens
  (`attic-semantic/src/throughput.rs`): every committed batch is recorded with
  its wall-clock span, fixing chunks/sec and ETA swinging between 1–20
  chunks/s depending on when a client polled.
- GPU-aware semantic defaults: unit caps default to 500,000 on a GPU and 2,560
  on CPU; `[semantic] max_units_per_repo` / `max_units_total` are validated
  against an upper bound.
- `status` gained `semantic_provider_backoff` (not-ready streak, last error,
  next retry), `semantic_selection_coverage` (eligible vs selected units,
  per-repo/global cap exclusions, plain-language reason) and
  `incremental_stuck_tasks` (tasks RUNNING past a generous bound).

#### Language analysis (`attic-analyzers`, `attic-indexing`)

- Kotlin, Scala, Lua, Ruby, PHP, Swift, C, C++, C#, Rust and Dockerfile moved
  from the generic `tags.scm` engine to full Tier-1 structural analyzers, each
  with an import resolver and tests (`tier1_*.rs`, fixtures under
  `crates/attic-analyzers/tests/fixtures/`).
- `ANALYZER_REGISTRY_VERSION` bumped to `0.3.0` so existing indexes re-analyze.
- Structural pipeline extended for the new analyzers
  (`attic-indexing/src/structural_pipeline.rs`).

#### Server structure (`attic-server`) — pure refactor

- `main.rs`: 9,080 → 4,127 lines. Extracted:
  - `tools.rs` — `make_tools()` and `json_schema` (output byte-identical to
    the previous version),
  - `validate.rs` — `validate_filter`, `validate_repository_id`,
    `require_active_member`,
  - `handlers/{file,search,repo_map,status,context}.rs` — tool handlers,
  - `tests.rs` — the in-file unit-test module.
- No behaviour or MCP schema change; moved items changed only in visibility
  (`pub(crate)`). `main.rs` keeps startup, wiring, the `AtticServer`
  impl (including `handle_workspace`) and daemon election glue.

#### Configuration and layout

- New `[knowledge]` table: a central folder of Markdown notes served as
  project knowledge to every repository. **On by default** at
  `<ATTIC_HOME>/knowledge`; an explicit path that does not exist turns the
  feature off with the reason shown in `status` and never fails startup.
- New `[logging]` table: `file_level` (`off`…`trace`) applied at every start;
  the `logging` MCP tool still changes it at runtime.
- `AtticPaths` reorganised into `config/`, `data/` and models directories under
  `ATTIC_HOME`, with a rollback-safe migration of the previous layout.
- Install flow: `attic-server setup-models` downloads and verifies embedding
  models at install time (`setup.sh` / `setup.ps1`, `cargo xtask install`); skip
  with `-SkipModels` / `ATTIC_SKIP_MODELS=1`. Exit codes: 0 ready, 1 usage,
  2 download failed, 3 model-cache directory creation failed.
- Model-cache hygiene (`attic-semantic/src/model_cache.rs`): removes the ONNX
  download cache and Windows duplicate blobs after download (~1.15 GB each).

### Fixed

- **Repository remove/re-add race.** Removal now enqueues a background
  `STALE_EVICTION` task (`attic-server/src/eviction.rs`,
  `attic-storage/src/repo_eviction.rs`) that deletes `attic.db` rows in small
  FK-ordered batches, then `semantic.db` rows. Membership is re-checked inside
  every delete transaction and before the semantic wipe, so a root re-added
  mid-eviction cancels it and keeps its data (zombie-resurrection guard).
  Tasks resume idempotently after a crash.
- **False STALLED diagnosis:** stall clock fixed; stuck-task detection added;
  ONNX `.onnx_data` readiness checked; load-failure visibility, INFO logging
  and a panic hook added.

### Security

- Nested `.git` and `.ssh` directories are blocked at any depth, including a
  symlink-bypass fix in `attic-retrieval/src/verify.rs`.
- ONNX model assets are verified against pinned SHA-256 hashes (checked against
  Hugging Face); the inference worker **fails closed** on a cache miss.
- Unix socket and data files created private (0600 / 0700).

### Tests

- `emergency_starts_no_new_heavy_work` now asserts under forced Emergency that
  `workspace` and `context` are admitted, `mcp_pressure_rejections` stays 0,
  and `status.semantic_availability.pressure_tier` is `"emergency"`.
- New suites: `tier1_*` analyzer tests, `semantic_gpu_pressure_gate`,
  `semantic_reconcile_stability`, `central_knowledge`, and resolver tests for
  C#, Rust and Dockerfile.

### Docs

- `docs/ARCHITECTURE.md` — Resource management states that foreground MCP
  calls are never refused for memory pressure; `docs/PLAYBOOK.md`,
  `docs/PERFORMANCE.md` and `knowledge/README.md` synced.

### Verification

- `cargo fmt --all --check` — pass
- `cargo clippy --workspace --all-targets -- -D warnings` — pass
- `cargo test --workspace --no-fail-fast` — pass (no failures); after the final
  `tests.rs` move, `cargo test -p attic-server` re-run: 128 unit + 27
  integration tests passing.
