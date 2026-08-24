# Target Project Tree

This is a target organization, not permission to create empty speculative modules.

```text
attic/
├── Cargo.toml
├── Cargo.lock
├── rust-toolchain.toml
├── rustfmt.toml
├── clippy.toml
├── README.md
├── crates/
│   ├── attic-server/
│   ├── attic-core/
│   ├── attic-storage/
│   ├── attic-discovery/
│   ├── attic-analyzers/
│   ├── attic-indexing/
│   ├── attic-retrieval/
│   ├── attic-evidence/
│   └── attic-test-support/
├── migrations/
├── config/
├── docs/
│   ├── architecture/
│   ├── contracts/
│   └── decisions/
├── fixtures/
│   ├── git/
│   ├── analyzers/
│   ├── secrets/
│   ├── large-files/
│   ├── identity/
│   └── recovery/
├── benchmarks/
│   ├── cases/
│   ├── baselines/
│   └── reports/
└── tools/
```

Runtime workspace data must live outside source-controlled implementation directories, e.g. under a configured workspace `.attic/` directory:

```text
.attic/
├── index.db
├── vectors/
├── artifacts/
├── cache/
├── checkpoints/
├── state/
└── logs/
```
