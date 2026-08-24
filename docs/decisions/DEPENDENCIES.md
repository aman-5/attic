# Dependency Decision Record — Bootstrap Phase

Recorded: 2026-08-24

## Rust Toolchain

| Item | Value | Notes |
|------|-------|-------|
| Toolchain | `1.98.0` | Portable — no platform suffix. Windows GNU setup: see `docs/local-setup/WINDOWS.md` |
| MSRV | 1.88 | Minimum required by rmcp 3.x |

## Workspace Dependencies (pinned 2026-08-24)

| Crate | Version | Source | Reason |
|-------|---------|--------|--------|
| tokio | 1.53.1 | crates.io | Async runtime for server + indexing workers |
| serde | 1.0.229 | crates.io | Serialization for domain/config/protocol data |
| serde_json | 1.0.151 | crates.io | JSON serialization |
| thiserror | 2.0.20 | crates.io | Ergonomic error type derivation |
| tracing | 0.1.44 | crates.io | Structured logging (to stderr only) |
| tracing-subscriber | 0.3.23 | crates.io | Tracing output formatting |

## Deferred to Later Phases

| Crate | Deferred to | Reason |
|-------|-------------|--------|
| rmcp | Phase 1D | MCP SDK — not needed until stdio transport skeleton |
| rusqlite / sqlx | Phase 1A | Storage layer not yet implemented |
| tree-sitter | Phase 3 | Structural analyzers not yet implemented |
| notify | Phase 2 | File watching not yet implemented |
| fastembed / ort | Phase 5 | Semantic embeddings — Phase 5 only |

## Verification Sources

- rmcp 3.1.4 verified at https://crates.io/crates/rmcp on 2026-08-24; MSRV 1.88 confirmed.
- All bootstrap dep versions verified on crates.io on 2026-08-24.
- See `Attic_AI_Agent_Implementation_Pack/06_reference/DEPENDENCY_VERIFICATION_2026-08-24.md` for full verification log.
