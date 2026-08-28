# Attic

Attic is a local MCP server that gives AI coding agents persistent,
evidence-backed understanding of large codebases and multi-repository
workspaces.

## Why Attic?

- **Fast code search** — full-text search over indexed content, not a slow re-grep every turn.
- **Structural understanding** — symbols, definitions, and relationships for supported languages.
- **Incremental indexing** — a filesystem watcher keeps the index current without full rebuilds.
- **Evidence-backed context** — answers are checked against real source spans, with an explicit `INSUFFICIENT_EVIDENCE` result instead of a guess.
- **Multi-repository relationships** — cross-repo dependency resolution across Git submodules in one workspace.
- **Curated project knowledge** — a `knowledge/` tier for facts that aren't obvious from source.
- **Local-first** — runs entirely on your machine; nothing leaves it unless you opt in to the (disabled-by-default) semantic layer.

## Quick Start

```sh
git clone https://github.com/aman-5/attic
cd attic
cargo build --release --package attic-server
```

The binary is built at `target/release/attic` (`target\release\attic.exe` on
Windows). See [Platform setup](#platform-setup) below for prerequisites
(Rust toolchain, a linker).

## Connect to your AI/MCP client

Attic is an MCP server: transport is **stdio**. Your AI client starts the
`attic` process, Attic indexes and maintains the workspace you point it at,
and the client calls Attic's tools when it needs repository knowledge.

Add it to your client's MCP server configuration:

```json
{
  "mcpServers": {
    "attic": {
      "command": "/absolute/path/to/target/release/attic",
      "args": [],
      "env": {
        "ATTIC_WORKSPACE_ROOT": "/absolute/path/to/your/repo"
      }
    }
  }
}
```

**Windows example:**

```json
{
  "mcpServers": {
    "attic": {
      "command": "C:\\Users\\you\\code\\attic\\target\\release\\attic.exe",
      "args": [],
      "env": {
        "ATTIC_WORKSPACE_ROOT": "C:\\Users\\you\\code\\myrepo"
      }
    }
  }
}
```

**Linux/macOS example:**

```json
{
  "mcpServers": {
    "attic": {
      "command": "/home/you/code/attic/target/release/attic",
      "args": [],
      "env": {
        "ATTIC_WORKSPACE_ROOT": "/home/you/code/myrepo"
      }
    }
  }
}
```

This configuration shape is client-neutral: place the same
command/args/env into whichever MCP-server settings your client exposes
(Claude Code, Claude Desktop, or any other MCP-capable client).

## Use it

Once connected, just ask your AI client questions about the workspace — it
invokes Attic's MCP tools automatically. You don't need to construct MCP
JSON-RPC requests by hand. For example:

```text
"Find where authentication tokens are validated."
"Show me every implementation of PaymentProvider."
"Which repositories depend on the shared auth package?"
"If I change UserService.create(), what may be affected?"
"Explain how checkout flows from the API to persistence."
"Find the configuration controlling retry behavior."
```

## What Attic can do

On first start with `ATTIC_WORKSPACE_ROOT` set, Attic performs a one-time
synchronous index of the workspace, then watches it for changes (native
filesystem watcher, falling back to periodic reconciliation) and re-indexes
incrementally. Repositories on disk are always the source of truth — every
index, graph, and cache is derived and disposable.

## MCP Tools

| Tool | Purpose |
|---|---|
| `search` | Full-text search over indexed workspace content |
| `file` | Read a bounded, verified region of a file from the live workspace |
| `repo_map` | Structural overview of a repository |
| `context` | Evidence-backed answer to a natural-language question (`FAST` / `NORMAL` / `DEEP` modes) |
| `status` | Server/indexing health, watcher mode, resource-pressure advisory |

Exact schemas are defined once in
`crates/attic-server/src/main.rs::make_tools()` and returned via
`tools/list` — that function is the single source of truth for the tool
surface. Detailed argument reference lives in `docs/PLAYBOOK.md` where
useful; this table is the everyday reference.

## Project Knowledge

```text
my-project/
├── src/
├── tests/
├── docs/
└── knowledge/
    ├── architecture.md
    ├── domain.md
    ├── conventions.md
    └── ownership.md
```

Any Markdown file under `knowledge/**` in an indexed repository is treated
as curated **Project Knowledge** — Attic's highest documentation authority
tier. Everything else (`README.md`, `docs/**`, an `ARCHITECTURE.md` outside
`knowledge/`) is ordinary documentation: still fully searchable, just not
elevated to the same authority.

Put in `knowledge/`:

- architecture intent and rationale
- domain terminology
- ownership
- project conventions
- deployment assumptions
- decisions not obvious from source

Never put in `knowledge/`: secrets, API keys, temporary prompts, or
transient chat instructions — knowledge files are indexed and served
through the same tools as source code, so anything written there is as
discoverable as any file in the repo.

This is optional — a repository with no `knowledge/` directory works the
same, just without that top evidence tier. See `knowledge/README.md` in
this repository for a ready-to-copy template.

## Multi-repository workspaces

```text
workspace/
├── frontend/
├── api/
├── auth-service/
├── shared-libs/
└── infrastructure/
```

Point `ATTIC_WORKSPACE_ROOT` at a parent directory whose repositories are
linked as **Git submodules**; each submodule becomes its own
`core_repositories` entry. Cross-repository dependency resolution
(`attic-crossrepo`) runs automatically at startup, resolving edges like
"which repositories depend on the shared auth package" or "what does
changing this symbol affect elsewhere in the workspace."

A **single** `attic` process owns one workspace/database: one
watcher, one MCP transport, one SQLite writer. Attic does not support
multiple processes writing to the same database concurrently — give each
concurrently running instance its own `ATTIC_DATA_DIR`.

## Architecture overview

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for diagrams and design
details: the indexing pipeline, retrieval/evidence model, incremental
freshness, cross-repository resolution, and language support.

## Platform setup

Building from source requires:

- **Rust** — pinned in `rust-toolchain.toml` (currently `1.98.0`,
  MSRV `1.88`); `rustup show` in the repo root installs it automatically.
- **A linker for your platform**:
  - **Windows (recommended)**: Microsoft "Build Tools for Visual Studio"
    with the C++ build tools workload (Visual Studio IDE not required).
  - **Windows (no MSVC)**: GNU/MinGW via [Scoop](https://scoop.sh)
    (`scoop install mingw`), then `rustup target add x86_64-pc-windows-gnu`
    — see `docs/PLAYBOOK.md` (Development) for the linker override needed.
  - **Linux**: system `cc`/`clang` (e.g. `build-essential` on Debian/Ubuntu)
    — tree-sitter grammars build their bundled C sources via `cc`.
  - **macOS**: `xcode-select --install` (Command Line Tools).

Attic bundles its own SQLite; no separate database install is required on
any platform.

## Configuration

All configuration is via environment variables — there are no CLI flags.

| Variable | Purpose |
|---|---|
| `ATTIC_WORKSPACE_ROOT` | Repository (or submodule parent) root to index and watch. Omit to serve whatever is already indexed without indexing anything new. |
| `ATTIC_DATA_DIR` | Overrides the data directory (default: platform application-data dir). |
| `ATTIC_DB_PATH` | Legacy single-variable override; the data dir is derived from its parent. |
| `ATTIC_SEMANTIC` | Set to `1` to opt in to the (disabled-by-default, experimental) semantic retrieval layer. |
| `ATTIC_LOG` / `RUST_LOG` | Log verbosity (`tracing`'s `EnvFilter` syntax); defaults to `info`. `ATTIC_LOG` takes precedence when both are set. |
| `ATTIC_TOTAL_MEMORY_BUDGET_MIB` | Total memory budget enforced by the resource monitor. |
| `ATTIC_MAX_FOREGROUND_QUERIES` | Concurrent foreground MCP query cap. |
| `ATTIC_MAX_BACKGROUND_WORKERS` | Concurrent background (indexing/semantic) worker cap. |
| `ATTIC_MIN_FREE_MEMORY_MIB` / `ATTIC_PER_REPO_MEMORY_BUDGET_MIB` / `ATTIC_MAX_IO_OPS_PER_SEC` | Additional resource-pressure tuning — see `crates/attic-storage/src/resource_manager.rs`. |
| `ATTIC_WRITER_BATCH_SIZE` / `ATTIC_WRITER_FLUSH_INTERVAL_MS` / `ATTIC_WRITER_QUEUE_CAPACITY` | Writer-queue tuning for indexing throughput. |

Data root resolution order: `ATTIC_DATA_DIR` → `ATTIC_DB_PATH`'s parent
directory → platform default (`%LOCALAPPDATA%\attic` on Windows,
`~/Library/Application Support/attic` on macOS, `$XDG_DATA_HOME/attic` or
`~/.local/share/attic` on Linux) → `./attic-data` as a last resort. Attic
never writes into your workspace — all index state is user-global.

## Troubleshooting

- **Server exits immediately on startup**: check stderr for a fail-closed
  message — usually a corrupted database (try a fresh `ATTIC_DATA_DIR` to
  isolate) or a workspace bootstrap failure (bad permissions, path doesn't
  exist).
- **`status` reports degraded cross-repo state**: cross-repository
  resolution hasn't completed yet or failed; single-repo retrieval is
  unaffected.
- **High memory / "server busy" errors**: Attic is enforcing its
  configured resource budget — raise `ATTIC_TOTAL_MEMORY_BUDGET_MIB` /
  `ATTIC_MAX_FOREGROUND_QUERIES` or reduce concurrently indexed repos.
- **Stale results after external changes**: Attic reconciles on startup;
  to force a fresh index, stop Attic and remove `attic.db*` from the data
  directory.

See `docs/PLAYBOOK.md` for a fuller troubleshooting table and recovery
procedures.

## Development

```sh
cargo test -p <crate>                                 # focused test
cargo test --workspace                                # full suite
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
```

See `docs/PLAYBOOK.md` (Development, Maintenance) for the full developer
workflow: schema migrations, adding an analyzer, and the release process.

Precompiled release archives (built by `tools/package.sh` / CI on tag push)
are also available for users who don't want to build from source — see
`.github/workflows/release.yml` and `docs/FINAL_VALIDATION_TODO.md` for
current per-platform verification status. This is a convenience path, not
the primary workflow. Note the binary name differs between the two paths:
a source build produces `target/release/attic` (`attic.exe` on Windows),
while `tools/package.sh` renames it to `attic-server` (`attic-server.exe`)
inside the packaged archive for end-user clarity — point your MCP client's
`command` at whichever one you actually have.

## Documentation

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — visual system design:
  pipeline, ownership model, storage concurrency, security, crash recovery.
- [`docs/PLAYBOOK.md`](docs/PLAYBOOK.md) — operations manual: start/stop,
  troubleshooting, recovery, development, and maintenance procedures.
- [`docs/FINAL_VALIDATION_TODO.md`](docs/FINAL_VALIDATION_TODO.md) —
  authoritative list of what has and hasn't yet been independently
  verified (platform CI, scale/soak/stress).
