# Attic — Operations & Development Playbook

Practical manual for operating, troubleshooting, recovering, and developing
Attic. For what Attic is and how it's built, see `docs/ARCHITECTURE.md`. For
install/config quick-starts, see `README.md`.

## Operation

### Start

```sh
ATTIC_WORKSPACE_ROOT=/path/to/repo attic-server   # attic-server.exe on Windows
```

Omitting `ATTIC_WORKSPACE_ROOT` still starts the server (serves whatever is
already indexed in `attic.db`); no new indexing happens.

### Stop

Ctrl+C (SIGINT) or close the stdio transport (an MCP client disconnecting).
Both paths run the same graceful shutdown: stop accepting new MCP calls,
stop the watcher, stop the scheduler, stop the semantic enrichment worker
(if running), record a clean-shutdown marker, checkpoint the WAL, write a
crash-recovery backup, close the database.

### First indexing

On first start with a given `ATTIC_WORKSPACE_ROOT`, Attic performs a
**synchronous** full index before it starts serving MCP requests — the
first `tools/call` a client makes will already see `CURRENT` data. Time
scales with repository size; there is no progress-streaming tool, only
`status` once serving begins.

### Subsequent startup

Every startup — first or not — runs `run_startup_recovery` before serving:
resets orphaned tasks, reconciles any indexing run interrupted by a prior
crash, verifies database integrity. If a workspace root is configured, an
offline-refresh pass is scheduled for anything not `CURRENT`, then the
watcher/scheduler resume.

### Incremental indexing

While running, Attic watches the workspace with a native filesystem watcher
(falls back to periodic reconciliation if the OS watch cannot be
established) and re-indexes changed files with a debounce window
(default 500ms). `status`'s `watcher.mode` field reports which mechanism is
actually active — never assume "native" without checking.

### Health / status

Call the `status` MCP tool. Key fields:

- `db` — repository/unit counts, migration count.
- `incremental.state` — `CURRENT` / `INDEXING` / `RECONCILIATION_REQUIRED` /
  `UNKNOWN`.
- `incremental.freshness` — counts of retrieval units by freshness state.
- `watcher.mode` / `watcher.active` — which change-detection mechanism is
  running.
- `resource_pressure.level` / `resource_advisory.advisory` — memory
  pressure tier (`normal`/`degraded`/`pause`/`emergency`) and its effect on
  admission.

### Repository add/remove

Repositories are discovered from the configured workspace root (a single
repo, or Git submodules under a parent directory) — there is no separate
"register a repository" tool. To add a repository, add it (or its
submodule) under the workspace root and restart, or wait for the watcher to
pick up a new submodule on the next reconciliation pass. To remove one,
remove it from the workspace root; its indexed data becomes orphaned and is
cleaned up on the next full reconciliation. There is currently no
single-repository "forget and purge immediately" tool — a data-directory
reset (below) is the supported way to force a clean slate.

### Multi-repository operation

See `docs/ARCHITECTURE.md#process-and-ownership-model`. One process, one
`ATTIC_WORKSPACE_ROOT`, repositories linked as Git submodules underneath it.
**Do not** run two `attic-server` processes against the same `attic.db` —
this is not a supported or tested configuration; give each concurrently
running instance its own `ATTIC_DATA_DIR`.

### Project Knowledge

Create `knowledge/*.md` files in a repository when you have durable facts
worth recording that aren't obvious from the code — architectural rationale,
domain vocabulary, conventions, ownership, deployment topology. See
`knowledge/README.md` in this repository for the full template and rules
(what belongs, what doesn't, never secrets).

Practical guidance:

- **Scope**: keep each file focused (one topic per file — `architecture.md`,
  `domain.md`, etc.) rather than one sprawling document; this keeps
  incremental re-indexing cheap and keeps individual claims easy to verify.
- **Keeping current**: there's no separate sync step — edit the file like
  any other tracked file and Attic's incremental watcher re-indexes it
  through the normal pipeline. Freshness is not evaluated differently than
  source code: a knowledge file with unindexed edits shows as `STALE` in
  `status` just like any other pending change.
- **Contradictions with source/tests**: Attic does **not** silently prefer
  knowledge over code, or vice versa — a `context` query surfaces both the
  knowledge claim and the contradicting source/test evidence with their
  respective authority and freshness, rather than picking one. Treat a
  detected contradiction as a signal that the knowledge file is stale and
  needs a human update, not as an Attic bug.
- **Never store secrets** — knowledge files are indexed and served through
  the same `file`/`search`/`context` tools as source, so anything written
  there is as discoverable as source code.
- **Durable facts, not chat instructions**: a knowledge file should describe
  something true about the project regardless of who's asking or why — not
  "for this task, do X." Session-scoped instructions belong in your AI
  tool's own configuration, not in `knowledge/`.

### Semantic enable/disable

Disabled by default. Enable with `ATTIC_SEMANTIC=1`. Disabling again (unset
or `0`) at any time is safe — delete `semantic.db` if you also want to
reclaim disk; canonical retrieval never depends on it.

## Troubleshooting

- **MCP connection failure**: confirm the client is invoking the exact
  binary path and that stdio is not being intercepted by another wrapper.
  Check stderr (not stdout) for startup errors — stdout carries only MCP
  JSON-RPC and will look empty/silent to a human on failure.
- **Startup failure (process exits immediately)**: check stderr for a
  fail-closed message. Common causes: corrupted database (try a fresh
  `ATTIC_DATA_DIR` to isolate), workspace path doesn't exist or isn't
  readable, or invalid `ATTIC_*` resource configuration
  (`ResourceConfig::validate()` rejects self-contradictory overrides).
- **Repository missing from `search`/`repo_map`**: confirm it's actually
  under `ATTIC_WORKSPACE_ROOT` (or a submodule of it) and not excluded by
  `.gitignore`/discovery policy; check `status.db.repository_count`.
- **File appears "ignored"**: discovery is gitignore-aware by default
  (`DiscoveryPolicy::default_git()`); a `.gitignore`'d file is not indexed
  even if you can read it manually.
- **Stale result after external changes** (e.g. `git checkout` while Attic
  was stopped): startup reconciliation should catch this; if not, force a
  fresh index (see Recovery below). The `file` tool always reads live from
  disk and will append an explicit `[index freshness: ...]` note if the
  index disagrees with what's on disk.
- **Indexing appears stuck**: check `status.incremental.tasks` (pending/
  running counts) and `watcher_errors`/`raw_batches_dropped`. A non-zero,
  non-decreasing `pending` count across repeated `status` calls indicates a
  stall — check stderr for scheduler errors.
- **Watcher degraded**: `status.watcher.mode` reports
  `periodic-reconciliation` instead of `native-watcher` when the OS watch
  handle failed or the native watcher couldn't start; this is a documented
  fallback, not a crash — indexing still happens, just on a timer instead
  of real-time events.
- **Cross-repo degraded**: `context` responses that need cross-repository
  evidence are withheld until startup's `sync_workspace` completes
  successfully; check stderr for `cross-repo workspace sync failed`.
  Single-repository retrieval is unaffected.
- **Semantic unavailable**: `ATTIC_SEMANTIC=1` was set but the semantic
  database failed to open — check stderr for `semantic layer unavailable`.
  The server continues serving non-semantic retrieval; this is by design
  (ADR-014 D1), not a failure to fix urgently.
- **DB problem**: see database integrity failures logged at startup
  (`database integrity violation during startup`) — the process refuses to
  serve rather than risk corrupt data. See Recovery below.
- **High memory**: check `status.resource_pressure`; raise
  `ATTIC_TOTAL_MEMORY_BUDGET_MIB` if the machine has headroom, or reduce
  concurrently indexed repositories. `"server busy"` tool errors mean
  foreground concurrency capacity (`ATTIC_MAX_FOREGROUND_QUERIES`) was
  exhausted — retry, or raise the limit.
- **High disk usage**: `attic.db`/`attic.db-wal`, `semantic.db` (if
  enabled), and `backups/` (last 3 retained) live under the data directory
  (see README). Disk growth tracks indexed content volume, not workspace
  size directly (structural/relationship data adds overhead beyond raw
  file bytes).

## Recovery

- **Interrupted indexing** (process killed mid-run): handled automatically
  by `run_startup_recovery` on the next start — no manual action needed.
  Nothing is ever exposed as `CURRENT` from a partial publication.
- **Safe reset/rebuild**: stop Attic, delete `attic.db`, `attic.db-wal`,
  `attic.db-shm` from the data directory, restart. This is always safe:
  all index state is derived from source and will be rebuilt on next
  startup. Source repositories are never touched by this or any Attic
  operation.
- **Disposable derived state**: `attic.db*`, `semantic.db`, `backups/`,
  `tmp/` are all disposable/reconstructible. Nothing under a workspace root
  is ever written by Attic — only the user-global data directory holds
  Attic's own state.
- **DB recovery**: if startup reports an integrity violation, restore from
  the most recent file in `backups/` (rename it over `attic.db`, remove
  `attic.db-wal`/`-shm`) or fall back to a full rebuild (above) if backups
  are also unusable.
- **Semantic rebuild**: delete `semantic.db` and restart with
  `ATTIC_SEMANTIC=1`; the background enrichment worker repopulates it from
  scratch. Canonical retrieval is unaffected while this happens.
- **Migration failure**: `run_migrations` refuses to serve from a database
  whose `core_schema_migrations` contains an entry the running binary
  doesn't recognize (e.g. an older binary opening a newer database) — this
  is intentional fail-closed behavior, not a bug. Upgrade the binary, or
  restore/rebuild the database as above.

## Development

```sh
git clone <repo>
cd attic
rustup show                                   # installs the pinned toolchain (rust-toolchain.toml)
cargo build --package attic-server            # debug build
cargo test -p <crate>                         # focused test, fast inner loop
cargo test --workspace                        # complete suite (slow — see FINAL_VALIDATION_TODO.md for CI status)
cargo fmt --all                               # formatting (rustfmt.toml)
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --package attic-server --target x86_64-pc-windows-msvc   # release build (adjust target per platform)
```

**Windows prerequisites (recommended: MSVC target)**: Rust via
[rustup](https://rustup.rs) (defaults to `x86_64-pc-windows-msvc`), plus
Microsoft "Build Tools for Visual Studio" with the **C++ build tools**
workload (Windows SDK is included by default in that workload). Visual
Studio IDE itself is not required — the standalone Build Tools are
sufficient.

**Windows alternative (no MSVC, GNU/MinGW toolchain)**: if you can't install
MSVC Build Tools, install MinGW user-locally via
[Scoop](https://scoop.sh) (`scoop install mingw`, no admin required), then
`rustup target add x86_64-pc-windows-gnu` and build with
`cargo build --target x86_64-pc-windows-gnu`. This requires a **local,
untracked** linker override in your global `%USERPROFILE%\.cargo\config.toml`
(not the repo's `.cargo/config.toml`, which stays portable and
machine-independent):

```toml
[build]
target = "x86_64-pc-windows-gnu"

[target.x86_64-pc-windows-gnu]
linker = "C:\\Users\\<you>\\scoop\\apps\\mingw\\current\\bin\\gcc.exe"
```

**Linux/macOS prerequisites**: Rust via rustup, plus a system `cc`/`clang`
toolchain (tree-sitter grammars build their bundled C sources via `cc`) —
typically already present, or installed via `xcode-select --install` on
macOS or your distribution's `build-essential`/`gcc` package on Linux.

None of the above is required for end users of a prebuilt binary — see
README's Install section.

## Maintenance

- **Schema migration**: add a new file under `migrations/`, wire it into
  `run_migrations` (`crates/attic-storage/src/migration.rs`); migrations
  apply forward-only and are rejected if unrecognized (see Recovery above).
- **IndexGeneration compatibility**: see
  `docs/decisions/ADR-004-index-generation-subsystem-versions.md` and
  `crates/attic-core/src/domain/subsystem_versions.rs` — bump the relevant
  subsystem version when changing what an analyzer/generation produces, so
  stale generations are correctly invalidated rather than silently reused.
- **Analyzer/grammar update**: bump the `tree-sitter-<language>` dependency
  in the workspace `Cargo.toml`, re-verify its ABI is within
  `tree-sitter`'s `MIN_COMPATIBLE_LANGUAGE_VERSION` (see ADR-010), re-run
  that language's analyzer test fixtures under `fixtures/analyzers/`. Each
  index generation records the running `ANALYZER_REGISTRY_VERSION`, but
  **nothing currently diffs it automatically against a stored generation's
  version to trigger re-indexing** — after bumping an analyzer/grammar
  version, manually trigger a full re-index of affected repositories
  (safe reset, above) rather than assuming Attic will detect the change on
  its own.
- **New workspace dependency**: verify the dependency's declared license is
  compatible with Attic's `MIT OR Apache-2.0` (`LICENSE-MIT`,
  `LICENSE-APACHE`) before adding it, and prefer a crate with genuine
  Linux/macOS/Windows support over one with platform-specific gaps.
- **Adding a language**: unsupported languages already work via
  `GenericAnalyzer` (full-text search only). To add structural richness,
  add a `tree-sitter-<language>` grammar dependency and a new analyzer under
  `crates/attic-analyzers/src/structural/`, registered in the analyzer
  dispatch table — see the existing Java/Python/Go/JS/TS analyzers as the
  reference shape.
- **Retrieval changes**: modify the Query Evidence Contract or candidate
  generation in `crates/attic-retrieval`; re-run the relevant benchmark in
  `benchmarks/` against its baseline before merging (see
  `benchmarks/acceptance.md`).
- **Release process**: bump `version` in the root `Cargo.toml`, then for
  each supported target run
  `tools/package.sh --target <triple> --out dist` (builds, stages, verifies,
  and archives). CI (`.github/workflows/release.yml`) runs this across all
  four supported targets on tag push. Never weaken `tools/package.sh
  --verify`'s exclusion checks (no `target/`, `*.db*`, logs, hidden files)
  to make a release pass.

Never recommend weakening endpoint security controls (antivirus/EDR
exclusions, code-signing bypass, etc.) to work around a build or packaging
failure — diagnose the actual cause instead.
