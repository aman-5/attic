# Test and Gate Matrix

## Test layers

### Unit
Pure domain/state-machine behavior.

### Component
SQLite, discovery, analyzer, evidence subsystem independently.

### Integration
Multiple Attic crates with temporary repos/DBs.

### MCP conformance/integration
Server launched over actual transport.

### Benchmark
Real engineering question dataset.

### Recovery
Kill/restart/failure injection.

### Security
Path, symlink, secrets, malicious content.

## Required gate by phase

| Phase | Required proof |
|---|---|
| Bootstrap | fmt/clippy/test; reproducible build |
| 0 | all contracts reviewed; schema creates |
| 1A | storage integration + rollback/concurrency |
| 1B | Git ignore/security fixtures |
| 1C | generic fallback + analyzer failure |
| 1D | end-to-end MCP search |
| 2 | incremental/delete/rename/crash tests |
| 3 | per-language structural benchmark |
| 4 | non-semantic answer/evidence benchmark |
| 5 | semantic A/B benchmark |
| 6 | cross-repo benchmark |
| 7 | security/recovery/performance/packaging |

## Always-run Rust checks
```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

## Regression rule
A higher average score does not excuse material regression in:
- exact lookup;
- symbol lookup;
- configuration lookup;
- simple search.
