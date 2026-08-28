# Attic — Operations & Development Playbook

Practical manual for operating, troubleshooting, recovering, and developing
Attic. For what Attic is and how it's built, see `docs/ARCHITECTURE.md`. For
install/config quick-starts, see `README.md`.

## Operation

### Start

```sh
ATTIC_WORKSPACE_ROOT=/path/to/repo target/release/attic   # target\release\attic.exe on Windows
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

Workspace membership is managed at runtime through the `workspace` MCP tool
(no restart, no config editing): `workspace {action:"add", path:"D:\new-service"}`
validates + canonicalizes the root, indexes it, starts its watcher, and
persists membership atomically to `<ATTIC_HOME>/config.toml`;
`{action:"remove", path:...}` stops the watcher and drops the repository
from active retrieval immediately (its historical indexed data stays in
storage but can never leak into search/context/status); `{action:"set",
paths:[...]}` authoritatively replaces the whole membership;
`{action:"inspect"}` reports the current state. Tell your AI client
"add D:\new-service to Attic" and it does exactly this.

### Multi-repository operation

See `docs/ARCHITECTURE.md#process-and-ownership-model`. One process, one
logical workspace, any number of configured repository roots. Configuration
precedence: `ATTIC_CONFIG` (explicit config file with one
`[[repositories]] path = "..."` entry per root) → the persistent
`<ATTIC_HOME>/config.toml` (default `~/.attic/config.toml`, written by the
`workspace` MCP tool) → `ATTIC_WORKSPACE_ROOT` (legacy single repository) →
UNCONFIGURED (server starts fine; configure via MCP). Configured roots may
live anywhere on disk with no common parent, no symlinks required, and no
`.gitmodules` requirement. Roots configured but temporarily unavailable
(e.g. an unmounted external drive) are reported by `status` under
`workspace.unavailable_repositories` with `degraded: true` — the remaining
roots stay usable.
**Do not** run two `attic` processes against the same `attic.db` —
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

Quick reference — see the detailed entries below each row for exact checks:

| Problem | What to check |
|---|---|
| MCP won't connect | executable path, stderr (not stdout), stdio not intercepted |
| Repository missing | `ATTIC_WORKSPACE_ROOT`/submodules, `.gitignore`, `status.db.repository_count` |
| File appears "ignored" | discovery/`.gitignore` policy (`DiscoveryPolicy::default_git()`) |
| Results stale | startup reconciliation, `[index freshness: ...]` note on `file` |
| Indexing appears stuck | `status.incremental.tasks`, `watcher_errors`, stderr scheduler errors |
| Watcher degraded | `status.watcher.mode` (`periodic-reconciliation` vs `native-watcher`) |
| Cross-repo unavailable | stderr `cross-repo workspace sync failed`; single-repo unaffected |
| Semantic unavailable | expected unless `ATTIC_SEMANTIC=1`; check stderr `semantic layer unavailable` |
| High disk usage | `attic.db*`, `semantic.db`, `backups/` under the data dir — not `target/` (see Development) |
| High memory / "server busy" | `status.resource_pressure`, `ATTIC_TOTAL_MEMORY_BUDGET_MIB` / `ATTIC_MAX_FOREGROUND_QUERIES` |

- **MCP connection failure**: confirm the client is invoking the exact
  binary path and that stdio is not being intercepted by another wrapper.
  Check stderr (not stdout) for startup errors — stdout carries only MCP
  JSON-RPC and will look empty/silent to a human on failure.
- **Startup failure (process exits immediately)**: check stderr for a
  fail-closed message. Common causes: corrupted database (try a fresh
  `ATTIC_DATA_DIR` to isolate), workspace path doesn't exist or isn't
  readable, or invalid `ATTIC_*` resource configuration
  (`ResourceConfig::validate()` rejects self-contradictory overrides).
- **Repository missing from `search`/`repo_map`**: confirm it is part of
  the configured workspace (`workspace {action:"inspect"}`) and not
  excluded by `.gitignore`/discovery policy; check
  `status.workspace.configured_repository_count`. A repository still in the
  DB but removed from membership is intentionally invisible — that is the
  membership-authoritative isolation contract, not data loss.
- **`workspace not configured` errors from `search`/`file`/`context`**: the
  server started with no workspace configuration (fresh install). This is
  the intended first-run state — use the `workspace` MCP tool (or set
  `ATTIC_CONFIG`/`ATTIC_WORKSPACE_ROOT`) to configure it.
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
- **DB recovery**: if startup reports an integrity violation, Attic refuses
  to serve but does **not** automatically move the corrupt file aside —
  manually rename/remove `attic.db`, `attic.db-wal`, `attic.db-shm` first,
  then either copy the most recent file from `backups/` into place as
  `attic.db` or fall back to a full rebuild (above) if backups are also
  unusable.
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

**`target/`** is Cargo's build/cache output directory (debug and release
artifacts, incremental compilation cache) — it is not Attic's runtime
index (that lives in the user-global data directory, see README) and is
not part of the product repository or a release archive. `cargo clean`
safely removes it at any time; it will be regenerated on the next build.

## Maintenance

- **Schema migration**: `migrations/` holds Attic's ordered SQLite schema
  migrations, required at every startup. Never delete or rename an
  already-applied migration — even one whose filename carries historical
  phase terminology (e.g. `0002_phase1d.sql`); future migrations should use
  descriptive production names instead. Add a new file, wire it into
  `run_migrations` (`crates/attic-storage/src/migration.rs`); migrations
  apply forward-only and are rejected if unrecognized (see Recovery above).
- **IndexGeneration compatibility**: see
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
