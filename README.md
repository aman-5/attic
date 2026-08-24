# Attic

Rust MCP server providing workspace code intelligence across 25–30 repositories.

## Status

**Bootstrap phase complete.** No product behaviour yet.

## Requirements

- Rust `1.98.0-x86_64-pc-windows-gnu` (pinned via `rust-toolchain.toml`)
- MinGW gcc (GNU linker — no MSVC required)
- Node ≥ 20 (for MCP host integration)

## Build

```bash
cargo build --workspace
```

## Checks

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

## Crates

| Crate | Role |
|-------|------|
| `attic-core` | Domain types, no async/DB/MCP deps |
| `attic-storage` | SQLite persistence layer (Phase 1A) |
| `attic-discovery` | Repository and file discovery (Phase 1B) |
| `attic-analyzers` | Language-specific AST analyzers (Phase 1C) |
| `attic-indexing` | Indexing orchestration |
| `attic-retrieval` | Query and retrieval engine |
| `attic-evidence` | Provenance and citation tracking |
| `attic-server` | MCP stdio server binary |
| `attic-test-support` | Shared test helpers (dev-dependency only) |

## Architecture

See [`docs/architecture/HIGH_LEVEL_CANONICAL_PLAN_DO_NOT_EDIT.md`](docs/architecture/HIGH_LEVEL_CANONICAL_PLAN_DO_NOT_EDIT.md).

## Dependency Decisions

See [`docs/decisions/DEPENDENCIES.md`](docs/decisions/DEPENDENCIES.md).

## Runtime Data

Runtime workspace data (index DB, vectors, cache) is stored outside the source tree under a per-workspace `.attic/` directory. See `docs/architecture/` for details.
