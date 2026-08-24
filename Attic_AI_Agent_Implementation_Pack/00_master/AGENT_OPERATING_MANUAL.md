# Attic AI Agent Operating Manual

This is the highest-priority implementation guide after the approved architecture.

## 1. Anti-hallucination protocol

For every task, the agent must follow:

```text
A. READ
B. LOCATE CONTRACT
C. INSPECT EXISTING CODE
D. VERIFY EXTERNAL FACTS
E. STATE INVARIANTS
F. DEFINE TESTS
G. IMPLEMENT MINIMUM CHANGE
H. RUN CHECKS
I. INSPECT DIFF
J. RECORD DECISION
K. STOP AT GATE
```

### A. READ
Read the current phase only plus explicitly referenced contracts.

### B. LOCATE CONTRACT
If no contract exists for a behavior that affects persistence, identity, security, compatibility, public MCP API, or correctness, do not invent one. Add an open question.

### C. INSPECT EXISTING CODE
Never assume a module is absent/present. Search the repository.

### D. VERIFY EXTERNAL FACTS
Before using external APIs:
- inspect Cargo metadata/lockfile;
- inspect installed crate source/docs when available;
- consult official docs/repository;
- use a minimal compile experiment if needed.

Never fabricate:
- Rust crate feature flags;
- rmcp handler signatures;
- MCP protocol fields;
- Tree-sitter node names;
- SQLite extension behavior;
- Git ignore semantics;
- configuration keys.

### E. STATE INVARIANTS
Write down what must remain true.

### F. DEFINE TESTS
Prefer tests that would fail under a plausible wrong implementation.

### G. IMPLEMENT MINIMUM CHANGE
No opportunistic architecture refactors.

### H. RUN CHECKS
Minimum Rust checks:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Run phase-specific tests too.

### I. INSPECT DIFF
Check for:
- unrelated files;
- accidental dependency additions;
- secret/logging leaks;
- TODOs hiding correctness gaps;
- public API drift.

### J. RECORD DECISION
For meaningful choices append a decision record.

### K. STOP AT GATE
Do not begin the next phase until the current gate passes.

## 2. What the agent may decide independently

Safe reversible details:
- private function names;
- module-local helper organization;
- test helper names;
- internal iterator style;
- formatting.

## 3. What requires a contract or explicit decision

- schema changes
- semantic identity
- persistence format
- MCP tool schema
- security behavior
- secret policy
- ignore precedence
- SourceRevision semantics
- invalidation propagation
- evidence authority
- query sufficiency
- answer-mode budgets
- dependency technology changes
- new external service

## 4. No silent fallback

Fallback must be observable.

Examples:
- parser failed → GenericAnalyzer + diagnostic
- semantic index unavailable → non-semantic plan + diagnostic
- budget exhausted → partial/insufficient state, never pretend complete
- stale evidence → reject or verify, according to contract

## 5. No `unwrap` policy

`unwrap()`/`expect()` may be used only in tests or on construction-time invariants that are genuinely impossible after validated static setup. Never use them for repository input, DB results, paths, parsers, MCP requests, or filesystem state.

## 6. Determinism

Tests must avoid:
- real network;
- user home directory;
- wall-clock timing assumptions;
- global Git config;
- machine-specific paths;
- random ordering without a fixed seed.

## 7. Task completion report

Every agent task must end with:

```text
TASK:
CONTRACT:
IMPLEMENTED:
FILES CHANGED:
DEPENDENCIES ADDED/CHANGED:
TESTS:
COMMANDS RUN:
RESULT:
INVARIANTS VERIFIED:
DECISIONS RECORDED:
OPEN QUESTIONS:
NEXT ALLOWED TASK:
```

If a required command was not run, state that explicitly.
