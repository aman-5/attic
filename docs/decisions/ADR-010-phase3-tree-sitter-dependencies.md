# ADR-010: Phase 3 Tree-sitter Dependency Set

**Status:** Accepted (Phase 3)
**Date:** 2026-08-26
**Supersedes:** none

## Context

Phase 3 adds structural source-code intelligence. The approved contract
(`docs/contracts/analyzers.md`, AZ-Q3) resolves that grammars are **bundled**
in the binary for V1; runtime loading is out of scope. The phase brief
requires verified versions, licenses, platform support, and API grounding —
never guessed node names or APIs.

## Decision

Adopt the official Tree-sitter Rust runtime plus five official grammar
crates, pinned by Cargo.lock:

| Crate                    | Locked version | License            | Role |
|--------------------------|----------------|--------------------|------|
| `tree-sitter`            | 0.26.13        | MIT OR Apache-2.0  | Parser runtime (`Parser`, `Language`) |
| `tree-sitter-language`   | 0.1.7          | MIT                | `LanguageFn` bridge used by grammar crates |
| `tree-sitter-java`       | 0.23.5         | MIT                | Java grammar |
| `tree-sitter-python`     | 0.25.0         | MIT                | Python grammar |
| `tree-sitter-go`         | 0.25.0         | MIT                | Go grammar |
| `tree-sitter-javascript` | 0.25.0         | MIT                | JavaScript/JSX grammar |
| `tree-sitter-typescript` | 0.23.2         | MIT                | TypeScript grammar (`LANGUAGE_TYPESCRIPT`) |

Notes:
- Grammar crates bundle their generated C sources and build via `cc`; no
  system toolchain beyond the MSVC linker is required. Verified compiling and
  running on `x86_64-pc-windows-msvc`.
- Mixed grammar ABI generations are within the runtime's compatibility
  window; every language assignment goes through
  `Parser::set_language(&LANGUAGE.into())`, whose `Result` is honoured as a
  fatal-fallback path rather than unwrapped.
- All extraction logic is grounded in parse trees dumped from these exact
  versions (`crates/attic-analyzers/tests/parse_probe.rs`, `#[ignore]`d
  probe) and cross-checked against each grammar's bundled `node-types.json`
  (field names like `interfaces`, `module_name`, `source`, `path` were taken
  from there, not assumed).

## Consequences

- Adding a language = add one grammar crate + one `LanguageSpec` module +
  registry line. No storage/indexing/MCP changes (proven by
  `tests/phase3_extensibility.rs`).
- Grammar upgrades require re-running the probe before trusting mappings.
- Licenses permit static bundling under the workspace's MIT/Apache dual
  license.

## Alternatives considered

- **Runtime WASM grammars** — rejected for V1 per contract AZ-Q3.
- **Per-language hand-written parsers** — rejected: maintenance cost and no
  error-recovery parity with Tree-sitter's incremental, error-tolerant
  parsing required by §9 (malformed input must degrade, never fail).
