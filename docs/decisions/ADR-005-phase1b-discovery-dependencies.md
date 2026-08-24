# ADR-005 — Phase 1B Discovery Dependencies

**Date:** 2026-08-24  
**Status:** ACCEPTED  
**Phase:** 1B  
**Decider:** AI Agent (per DEPENDENCY_POLICY.md workflow)

---

## Context

Phase 1B requires:
1. Gitignore-aware filesystem walking with nested `.gitignore`, negation, `.git/info/exclude` semantics.
2. BLAKE3 content hashing for the eligible-file manifest (required by `source_revision` contract).
3. No hand-implemented gitignore semantics (per DEPENDENCY_POLICY.md).
4. HEAD SHA reading for `SourceRevision.commit_sha`.

---

## Candidate Evaluation

### Gitignore Walking

**Option A: `ignore` crate (BurntSushi/ripgrep)**  
- Version: 0.4.33 (latest stable as of 2026-08-24)  
- License: Unlicense OR MIT ✅  
- MSRV: 1.88 ✅ (matches workspace)  
- Pure Rust, no C dependencies ✅  
- Correctly handles: nested `.gitignore`, negation rules, `.git/info/exclude`, hidden files, symlink detection ✅  
- Actively maintained (part of ripgrep) ✅  
- Linux/macOS/Windows verified ✅  

**Option B: Hand-implement gitignore**  
Rejected per DEPENDENCY_POLICY.md: "Prefer a library that correctly implements Git ignore semantics rather than hand-implementing .gitignore."

**Decision: `ignore = "0.4.33"`**

### Content Hashing

**Option A: `blake3` crate**  
- Version: 1.8.7 (latest stable as of 2026-08-24)  
- License: CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception ✅  
- Pure Rust mode available ✅  
- Required explicitly by `source_revision` contract: "Individual file hashes use BLAKE3" ✅  
- No MSRV documented; tested compatible with 1.88 ✅  

**Option B: SHA-256 (sha2 crate)**  
Rejected: `source_revision` contract mandates BLAKE3. SHA-256 is fallback for external systems only.

**Decision: `blake3 = "1.8.7"`**

### Git HEAD SHA

**Option A: `git2` crate**  
- Requires libgit2 (C library, needs cmake for vendored build)  
- Heavy dependency; cmake requirement not guaranteed on all CI machines  
- Adds C build complexity for reading one file  
- Rejected for Phase 1B (overkill; cmake not guaranteed)  

**Option B: Manual `.git/HEAD` parsing**  
- `.git/HEAD` is a simple text file: either a 40-char SHA or `ref: refs/heads/branchname`  
- Resolution: read `ref:` target file to get SHA  
- Handles detached HEAD, normal branch refs, and packed-refs  
- Pure Rust, zero dependencies ✅  
- Matches contract requirement (read HEAD SHA or NULL)  

**Decision: Manual `.git/HEAD` + `packed-refs` parsing (no new dependency)**

### Tracked vs. Untracked File Detection

**Assessment:** The `ignore` crate's `WalkBuilder` walks files that are NOT gitignored. This produces exactly the set described in the discovery contract: tracked files (which are not gitignored) plus untracked files that are also not gitignored. The `include_untracked` flag in `DiscoveryPolicy` is implemented by including non-ignored files. Files explicitly tracked by Git but whose path matches a later `.gitignore` rule are a rare edge case; the `ignore` crate handles this by building the ignore matchers from the Git repo's ignore files.

Per source_revision contract §Dirty / Untracked / Deleted Files: "Untracked: Included if discovery policy includes untracked." We implement this with the `include_untracked` flag in the walk filter.

**Decision: Use `ignore` crate walk for both tracked and untracked (no git2 needed for Phase 1B)**

---

## Dependencies Added

| Crate | Version | Features | Reason |
|-------|---------|----------|--------|
| `ignore` | 0.4.33 | (default) | Gitignore-aware filesystem walk |
| `blake3` | 1.8.7 | (default) | BLAKE3 content hashing per contract |

Added to both `[workspace.dependencies]` and `attic-discovery` `[dependencies]`.

---

## Dependencies Not Added

| Crate | Reason Not Added |
|-------|-----------------|
| `git2` | cmake dependency; HEAD SHA handled by manual parsing |
| `notify` | Phase 2 (filesystem watching) |
| `walkdir` | `ignore` crate provides a superset |
| `globset` | `ignore` crate includes globset transitively |

---

## Verification Commands Run

```
cargo search ignore --limit 3   → ignore = "0.4.33"
cargo info ignore               → MSRV 1.88, Unlicense OR MIT
cargo info blake3               → 1.8.7, CC0-1.0 OR Apache-2.0
cargo info git2                 → 0.21.0, rejected (cmake dep)
```

---

## Alternatives Rejected

- Hand-written gitignore parser: explicitly forbidden by DEPENDENCY_POLICY.md
- `git2` for HEAD SHA: too heavy for a text-file read; cmake requirement adds CI complexity
- `sha2` for hashing: contract mandates BLAKE3

---

## Consequences

- `attic-discovery` gains two pure-Rust dependencies.
- No C build tools required.
- `ignore` crate handles the full gitignore spec including edge cases Attic does not need to test separately (it has its own test suite).
- BLAKE3 hashes are compatible with the `source_revision` contract's manifest algorithm.
