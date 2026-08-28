# Attic

Attic is a local-first code intelligence server for AI coding assistants. It
indexes your repositories into a SQLite-backed store and exposes retrieval —
search, file access, repo maps, and evidence-grounded question answering —
over the [Model Context Protocol](https://modelcontextprotocol.io) (MCP), so
any MCP-capable client (Claude Code, Claude Desktop, etc.) can ground its
answers in your actual code instead of guessing.

Attic runs entirely on your machine: no code or query leaves your computer
unless you explicitly opt in to the (disabled-by-default) semantic layer.

## Install (binary, no build tools required)

1. Download the archive for your platform from the project's Releases page:
   - `attic-v<version>-x86_64-pc-windows-msvc.zip` — Windows x86_64
   - `attic-v<version>-x86_64-unknown-linux-gnu.tar.gz` — Linux x86_64
   - `attic-v<version>-x86_64-apple-darwin.tar.gz` — macOS x86_64 (Intel)
   - `attic-v<version>-aarch64-apple-darwin.tar.gz` — macOS ARM64 (Apple Silicon)
2. Extract it anywhere. The archive contains exactly one binary
   (`attic-server` / `attic-server.exe`), a `README.md`, license files, and
   `docs/`. Nothing else needs to be installed — Attic bundles its own
   SQLite; end users do not need Rust, Cargo, MSVC Build Tools, MinGW, Xcode
   command-line tools, GCC, Node.js, or Tree-sitter development packages.
3. (Optional) Put the extracted directory on your `PATH`, or reference the
   binary by full path in your MCP client configuration below.

> **Supported platforms**: Windows x86_64, Linux x86_64, macOS x86_64, and
> macOS ARM64 are built and packaged in CI on every release
> (`.github/workflows/release.yml`). A platform is only claimed "verified"
> once CI has actually run the full test suite on it — see
> `docs/FINAL_VALIDATION_TODO.md` for the current, authoritative per-platform
> verification status.

## Configure a workspace

Attic indexes one repository root per running server instance, set via an
environment variable:

```sh
ATTIC_WORKSPACE_ROOT=/path/to/your/repo attic-server
```

On first start, Attic performs a one-time synchronous index of the workspace,
then watches it for changes (native filesystem watcher, falling back to
periodic reconciliation where unavailable) and re-indexes incrementally as
files change. If `ATTIC_WORKSPACE_ROOT` is unset, Attic still starts and
serves whatever repositories are already in its database (useful for
inspecting a previously-indexed workspace, or for pure MCP tool testing).

For multi-repository / cross-repo dependency resolution, point
`ATTIC_WORKSPACE_ROOT` at a parent directory whose repositories are linked as
Git submodules (each submodule becomes its own `core_repositories` entry —
see ADR-006). A **single** `attic-server` process owns the workspace: one
watcher, one MCP transport, one SQLite writer. Cross-repo dependency
resolution runs automatically at startup within that one process — see
`crates/attic-crossrepo` and `docs/contracts/query_evidence.md`.

Attic does **not** support multiple `attic-server` processes writing to the
same database concurrently — the writer, watcher, and startup-recovery logic
all assume single-process ownership of a given `attic.db`. Point every
workspace's `ATTIC_DATA_DIR` (or `ATTIC_DB_PATH`) at its own, distinct
location if you run more than one Attic instance on a machine.

## Configure your MCP client

Attic speaks MCP over stdio. Example configuration (Claude Code /
Claude Desktop `mcp_config.json`-style):

```json
{
  "mcpServers": {
    "attic": {
      "command": "/absolute/path/to/attic-server",
      "args": [],
      "env": {
        "ATTIC_WORKSPACE_ROOT": "/absolute/path/to/your/repo"
      }
    }
  }
}
```

On Windows, use `attic-server.exe` and Windows-style paths
(`C:\\Users\\you\\code\\myrepo`).

Once connected, Attic exposes these MCP tools:

| Tool | Purpose |
|---|---|
| `search` | Full-text search over indexed content |
| `file` | Read a file (or bounded region) from the live workspace |
| `context` | Evidence-grounded answer to a natural-language question (`FAST` / `NORMAL` / `DEEP` modes) |
| `repo_map` | Structural overview of a repository |
| `status` | Server health, watcher mode, resource-pressure advisory |

## Run

```sh
attic-server
```

Attic logs to stderr (structured, `tracing`-based) and speaks MCP JSON-RPC
on stdin/stdout — this is the same contract your MCP client uses, so you can
also drive it manually for debugging:

```sh
printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"debug","version":"0"}}}\n' | attic-server
```

Stop the server with Ctrl+C (SIGINT) or by closing its stdio transport —
either path runs the same graceful shutdown: in-flight work finishes, a WAL
checkpoint and crash-recovery backup are written, and the database is closed
cleanly before exit.

## Semantic search (optional, opt-in)

Attic's semantic (embedding-based) retrieval layer is **disabled by
default**. It only activates when explicitly requested:

```sh
ATTIC_SEMANTIC=1 ATTIC_WORKSPACE_ROOT=/path/to/repo attic-server
```

When disabled, Attic never creates the semantic database file, never
computes embeddings, and retrieval falls back cleanly to lexical/structural
search only — deleting the semantic database at any time is always safe and
degrades gracefully rather than breaking the server.

## Project Knowledge (optional)

```text
repository/
├── src/
├── docs/
├── README.md
└── knowledge/
    ├── README.md        (see knowledge/README.md in this repo for the template)
    ├── architecture.md
    ├── domain.md
    └── conventions.md
```

Attic recognizes one deliberately curated authority tier: any Markdown file
under `knowledge/**` in an indexed repository is treated as
**Project Knowledge** — the highest documentation authority Attic assigns
(`AuthorityLevel::ProjectKnowledge`). Everything else — `README.md`,
`docs/**`, an `ARCHITECTURE.md` sitting anywhere outside `knowledge/` — is
ordinary **documentation evidence**: fully searchable and returned by
`context`/`search` like any other file, just not elevated to the same
authority as content you've deliberately placed under `knowledge/`.

This is optional. A repository with no `knowledge/` directory works exactly
the same, just without that top evidence tier. See `knowledge/README.md` in
this repository for a ready-to-copy template and full guidance, and
`docs/PLAYBOOK.md` for when/how to maintain knowledge files in practice.

## Runtime data: what's workspace-local vs. user-global

Attic never writes into your workspace — no `.attic/` directory appears in
your repo, so indexing a read-only or third-party checkout is always safe.
All index state, caches, and logs are **user-global**, stored once per
machine and shared across every workspace you point Attic at:

| Path | Contents |
|---|---|
| `attic.db` | Main SQLite index (all indexed repositories) |
| `semantic.db` | Semantic embeddings (only created when `ATTIC_SEMANTIC=1`) |
| `backups/` | Crash-recovery database backups (most recent 3 retained) |
| `tmp/` | Process scratch space (safe to delete while Attic is not running) |

The data root defaults to the platform's standard application-data
directory, and can be overridden:

- **Resolution order**: `ATTIC_DATA_DIR` env var → `ATTIC_DB_PATH`'s parent
  directory (legacy single-variable override) → platform default → `./attic-data`
  in the current directory as a last-resort fallback.
- **Windows**: `%LOCALAPPDATA%\attic`
- **macOS**: `~/Library/Application Support/attic`
- **Linux/BSD**: `$XDG_DATA_HOME/attic` (falls back to `~/.local/share/attic`)

See `crates/attic-core/src/paths.rs` for the normative policy and its tests.

### Resource limits

Attic ships with production defaults (1024 MiB total memory budget, 64
concurrent foreground MCP queries, background indexing/semantic workers
capped strictly below the foreground capacity so interactive queries are
never starved). All of these are overridable via `ATTIC_*` environment
variables — see `crates/attic-storage/src/resource_manager.rs` for the full
list (`ATTIC_TOTAL_MEMORY_BUDGET_MIB`, `ATTIC_MAX_FOREGROUND_QUERIES`,
`ATTIC_MAX_BACKGROUND_WORKERS`, etc.).

## Troubleshooting

- **Server exits immediately on startup**: check stderr for a fail-closed
  message. Attic refuses to serve rather than risk stale/incorrect answers —
  common causes are a corrupted database (run with a fresh `ATTIC_DATA_DIR`
  to isolate) or a workspace bootstrap failure (bad permissions, workspace
  path doesn't exist).
- **`status` tool reports `degraded` cross-repo state**: cross-repository
  dependency resolution hasn't completed yet (runs once at startup) or
  failed; local single-repo retrieval is unaffected.
- **High memory / "server busy" errors**: Attic is enforcing its configured
  memory/concurrency budget. Raise `ATTIC_TOTAL_MEMORY_BUDGET_MIB` /
  `ATTIC_MAX_FOREGROUND_QUERIES` if your machine has headroom, or reduce the
  number of concurrently indexed repositories.
- **Stale results after external changes** (e.g. `git checkout` while Attic
  was stopped): Attic reconciles on startup, but you can force a fresh
  index by removing `attic.db*` from the data directory (see above) and
  restarting.

## Uninstall

Attic makes no registry entries, no services, and no changes outside its
data directory:

1. Delete the extracted binary/directory.
2. Delete the data directory listed above (`attic.db*`, `backups/`, `tmp/`)
   if you want to remove all indexed data — this is optional; leaving it in
   place has no effect once the binary is gone.
3. Remove the `attic` entry from your MCP client's configuration.

## Building from source

End users should use the prebuilt binaries above. Building from source is
only needed for development or unsupported platforms, and requires:

- Rust (see `rust-toolchain.toml` for the pinned version) via
  [rustup](https://rustup.rs)
- A working linker for your platform (MSVC Build Tools on Windows, or a
  GNU/MinGW toolchain; system `cc`/`clang` on Linux/macOS) — see
  `docs/PLAYBOOK.md` (Development) for exact per-platform setup

```sh
cargo build --release --package attic-server
# binary at target/release/attic (or attic.exe on Windows)
```

To build a release archive with the same layout as the official releases:

```sh
tools/package.sh --target <x86_64-pc-windows-msvc|x86_64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin> --out dist
```

## Documentation

- `docs/ARCHITECTURE.md` — how Attic is built: pipeline, process/ownership
  model, storage concurrency, security, crash recovery.
- `docs/PLAYBOOK.md` — operations manual: start/stop, troubleshooting,
  recovery, development, and maintenance procedures.
- `docs/FINAL_VALIDATION_TODO.md` — authoritative list of what has and has
  not yet been independently verified (platform CI, scale/soak/stress).
- `docs/contracts/` — normative behavioral contracts (retrieval, evidence,
  recovery, resources, etc.).
- `docs/decisions/` — architecture decision records.
