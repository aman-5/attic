# Bootstrap Phase — Create Attic from Scratch

## Goal
A clean, reproducible, warning-free Cargo workspace with no product behavior.

## Steps

### B1 Verify host
Run:
```bash
node --version
git --version
rustup --version
rustc --version
cargo --version
```
Require Node >=20.

### B2 Verify official MCP SDK baseline
Before pinning Rust:
- verify official `modelcontextprotocol/rust-sdk`;
- verify current stable `rmcp`;
- verify MSRV and needed transport features.

Baseline known when this pack was created: rmcp 3.0.1, Rust >=1.88.

### B3 Initialize Git
Create project directory `attic`, initialize Git, add `.gitignore`.

### B4 Pin Rust
Create `rust-toolchain.toml` with verified stable channel. Add `rustfmt` and `clippy` components.

### B5 Create Cargo workspace
Create the approved crates only. Each crate must compile with minimal code.

### B6 Add only bootstrap dependencies
Expected:
- tokio where async runtime is required;
- serde/serde_json for domain/config/protocol data;
- tracing;
- error crate(s);
- rmcp only when MCP skeleton is being verified.

Do not add SQLite/Tree-sitter/notify/vector dependencies yet unless required by an approved bootstrap spike.

### B7 Logging
Configure structured tracing to stderr. Never log MCP protocol output to stdout when using stdio transport.

### B8 CI
CI must run:
```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

### B9 Docs
Copy the canonical architecture and contracts into project docs.

## Gate
- clean checkout builds;
- checks pass;
- Node >=20 recorded;
- Rust pinned;
- lockfile committed;
- no architecture implementation yet;
- no unnecessary dependencies.
