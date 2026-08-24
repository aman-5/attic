# Attic — AI Agent Implementation Pack

**Project name:** Attic  
**Purpose:** Build the approved Workspace Code Intelligence MCP from scratch with an AI coding agent while minimizing hallucination, speculative design changes, incorrect dependencies, and phase leakage.

This package contains the approved high-level architecture **unchanged** in:

`01_architecture/HIGH_LEVEL_CANONICAL_PLAN_DO_NOT_EDIT.md`

Everything else translates that architecture into low-level execution instructions.

## Golden rule

The AI agent is not the architect. The architecture is already approved.

The agent's job is:

```text
read contract
→ inspect current repository
→ verify external API/dependency facts
→ define acceptance tests
→ implement smallest valid increment
→ run gates
→ inspect diff
→ record decisions
→ proceed only when allowed
```

## Required reading order

1. `00_master/AGENT_OPERATING_MANUAL.md`
2. `01_architecture/HIGH_LEVEL_CANONICAL_PLAN_DO_NOT_EDIT.md`
3. `00_master/PROJECT_BASELINE.md`
4. `00_master/EXECUTION_MAP.md`
5. The current phase file under `03_phases/`
6. Only the contracts referenced by that phase
7. `04_quality/TEST_AND_GATE_MATRIX.md`
8. `04_quality/SECURITY_INVARIANTS.md`

Do **not** load every detailed file into the agent context at once. The pack is intentionally progressive.

## Phase order

```text
BOOTSTRAP
   ↓
PHASE 0 — executable contracts
   ↓
PHASE 1A — persistence
   ↓
PHASE 1B — discovery/security
   ↓
PHASE 1C — analyzer foundation
   ↓
PHASE 1D — minimum MCP + FTS
   ↓
PHASE 2 — incremental correctness/freshness
   ↓
PHASE 3 — structural intelligence
   ↓
PHASE 4 — evidence-driven retrieval
   ↓
PHASE 5 — semantic intelligence
   ↓
PHASE 6 — cross-repository intelligence
   ↓
PHASE 7 — production hardening
```

## Baseline

- Node.js: **>=20**. Already installed per project assumptions. Node is for tooling/harnesses; Attic's core MCP server remains Rust.
- Rust: pin **1.88 or newer compatible stable** initially because the current official `rmcp` 3.x SDK requires Rust 1.88. Re-verify before first dependency lock.
- Git: required.
- SQLite: embedded in Attic; CLI recommended for debugging.
- Primary targets: Linux + macOS.
- MCP: use the official Rust SDK, `rmcp`.
- No Docker, Redis, Elasticsearch, Neo4j, external vector DB, or GPU is required for the initial implementation.

## Important

Dependency versions in this pack are baselines, not permission to blindly paste stale versions. The agent must verify the official package/repository before adding or upgrading a dependency, then pin through `Cargo.lock`.
