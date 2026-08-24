# Dependency and External API Policy

## Objective

Prevent an AI agent from inventing dependencies or coding against outdated APIs.

## Mandatory dependency workflow

Before adding a crate:

1. State the capability required.
2. Check whether std/existing workspace dependencies already provide it.
3. Identify the official project/crate.
4. Verify latest stable release and MSRV.
5. Verify exact features required.
6. Verify license compatibility.
7. Verify Linux/macOS support.
8. Add with Cargo rather than manually guessing transitive dependencies.
9. Run compile/tests.
10. Record in dependency decision log.

## MCP baseline

Use official `rmcp` from `modelcontextprotocol/rust-sdk`.

At package creation time:
- official workspace version: 3.0.1;
- workspace Rust minimum: 1.88;
- edition: 2024;
- official docs recommend adding `rmcp` with the `server` feature and selecting transport features as needed.

Re-verify at implementation time.

## SQLite selection

Phase 0 must choose the Rust SQLite binding. Required capabilities:
- SQLite bundled/system strategy explicitly decided;
- FTS5 support verified on Linux/macOS;
- WAL;
- transactions;
- prepared statements;
- busy timeout/handler;
- safe concurrent read connections;
- controlled writer;
- migration support either via crate or Attic code.

Do not choose based solely on ORM convenience.

## Git/discovery selection

Prefer a library that correctly implements Git ignore semantics rather than hand-implementing `.gitignore`.

Verify:
- nested ignore rules;
- negation;
- hidden files;
- symlink behavior;
- explicit overrides.

## Tree-sitter

Add Tree-sitter only in the structural phase. Grammar crates are individually versioned dependencies. Each analyzer must pin/test its grammar and node expectations.

Never write queries based on remembered node names without fixture tests.

## Semantic dependencies

Phase 5 only. The semantic provider must be behind an interface so embeddings are disposable. Do not make core schema depend on a specific model/vendor.
