# Phase 1B Completion Report — `attic-discovery`

**Date:** 2026-08-24  
**Crate:** `attic-discovery` v0.1.0  
**Target:** `x86_64-pc-windows-msvc`  
**Rust edition:** 2024 (MSRV 1.88)

---

## Summary

Phase 1B is complete. All six review-identified gaps have been addressed, all nine new required tests pass, and the crate compiles with zero warnings under `cargo clippy -D warnings`.

**Final gate results:**

| Gate | Result |
|------|--------|
| `cargo check` | ✅ 0 errors |
| `cargo clippy -D warnings` | ✅ 0 warnings |
| `cargo test` (103 tests) | ✅ 103 passed, 0 failed |

---

## Gaps Addressed

### Gap 1 — `include_untracked = false` uses real tracked-file set

**Problem:** Pass 1 (gitignore-aware walk) would prune gitignored files before the tracked-file filter could rescue them. Tracked-but-gitignored files were silently lost.

**Fix (`walk.rs`):**
- `git_tracked_files()` calls `git ls-files --cached --full-name -z` and returns a `HashSet<String>` of NUL-separated repo-relative paths.
- Pass 1 uses `tracked_files` to admit only tracked files (+ explicit include-rule matches).
- **Pass 2** (gitignore-disabled walk) runs whenever `include_untracked=false` and the tracked set is non-empty, capturing any tracked files the gitignore walker pruned in Pass 1.
- If `git ls-files` fails (shallow clone, no commits), an `IoError` diagnostic is emitted and the walk falls back to `include_untracked=true` semantics.

**New tests:**
- `include_untracked_false_excludes_untracked_files` — real `git init` + `git add`; untracked files absent.
- `tracked_file_matching_gitignore_still_returned` — `git add -f` on a gitignored file; file survives the walk.

---

### Gap 2 — Attic include-rule precedence over gitignore

**Problem:** An `attic_include_rule` matching a gitignored path would have no effect because the `ignore`-crate walker pruned the entire directory before the rule could be evaluated.

**Fix (`walk.rs`):**
- **Pass 3** (gitignore-disabled walk) runs when `attic_include_rules` is non-empty. Only paths matching an include rule that were **not** already captured in Pass 1 or Pass 2 are admitted.
- Security-forbidden paths remain excluded unconditionally before any include-rule check.

**New tests:**
- `gitignored_dir_explicitly_reincluded_by_attic` — gitignored `private/data.rs` re-included by Attic rule; appears in output.
- `security_forbidden_path_cannot_be_reincluded` — `.env` listed in an include rule; still absent from output.

---

### Gap 3 — Secrets preprocessing at discovery boundary

**Problem:** Raw file content was being passed downstream without secrets scanning, violating the security invariant that no secret-bearing content should leave the discovery layer unredacted.

**Fix (`lib.rs`):**
- `DownstreamContent` enum added:
  ```rust
  pub enum DownstreamContent {
      Safe(String),
      Redacted { content: String, findings: Vec<SecretFinding> },
      Excluded,
  }
  ```
- `DiscoveryOutput` gains `downstream_contents: Vec<(String, DownstreamContent)>`.
- `discover()` calls `secrets::preprocess()` for every entry after manifest build; result is mapped to `DownstreamContent` and stored.
- Manifest hash still uses raw BLAKE3 bytes (not downstream content).

**New tests (`lib.rs`):**
- `inline_secret_produces_redacted_downstream_content` — AWS key in file → `DownstreamContent::Redacted`.
- `known_secret_carrier_produces_excluded_downstream_content` — `.netrc`-style file → `DownstreamContent::Excluded`.

---

### Gap 4 — Submodule detection

**Problem:** The `ignore` crate does not automatically stop at nested `.git` boundaries; submodule contents would be indexed as if they belonged to the parent repository.

**Fix (`walk.rs`):**
- For every directory entry at non-root depth: check `abs_path.join(".git").exists()`.
- If true: emit `DiagnosticKind::SubmoduleDetected` diagnostic, record the prefix in `submodule_prefixes`, and skip the directory.
- Subsequent file entries whose `repo_relative` path starts with any recorded prefix are skipped.
- Detection covers both `.git/` (directory form) and `.git` (file form, used by worktrees and real submodule checkouts).

**ADR-006** corrected to reflect active detection logic (removed false claim that the `ignore` crate handled this automatically).

**New tests:**
- `nested_repo_detected_as_submodule` — `.git/` directory form; `SubmoduleDetected` diagnostic emitted, submodule files absent.
- `nested_repo_with_git_file_detected_as_submodule` — `.git` file form (worktree); same guarantees.

---

### Gap 5 — Unstable-capture detection

**Problem:** A file modified while being hashed (concurrent write, log rotation, etc.) could produce an inconsistent manifest entry with no indication of the instability.

**Fix (`manifest.rs`):**
- `FileStat` internal struct: `{ size: u64, modified: Option<SystemTime> }` with `PartialEq`.
- `FileStat::read()` stats the file before and after `hash_file_content()`.
- If `stat_before != stat_after`: `DiagnosticKind::UnstableCapture` diagnostic emitted, `ManifestEntry.unstable = true`.
- `SourceManifest.unstable_captures: Vec<Diagnostic>` collected separately.
- `is_stable()` returns `true` only when both `read_errors` and `unstable_captures` are empty.
- `discover()` extends `all_diagnostics` with `manifest.unstable_captures`.

**New tests (`manifest.rs`):**
- `stable_file_produces_no_unstable_capture_diagnostic` — normal file; no unstable entry.
- `file_stat_detects_size_change` — `FileStat` comparison when sizes differ.
- `unstable_capture_detected_when_stats_differ` — mutated `FileStat` triggers `UnstableCapture`.

---

### Gap 6 — Policy hash versioning

**Problem:** A policy change that altered behaviour but not the serialized fields would produce an identical hash, breaking cache invalidation.

**Fix (`policy.rs`):** (completed in previous session)
- `policy_version: u32` added as first field of `DiscoveryPolicy`.
- `default_git()` and `default_non_git()` both set `policy_version: 1`.
- Policy hash includes `policy_version` in the JSON serialization.

**New test (`policy.rs`):** (completed in previous session)
- `policy_version_change_changes_hash` — incrementing `policy_version` produces a different hash.

---

## Walk Architecture (Three-Pass)

```
Pass 1  gitignore ON   → main_entries (all normally eligible files)
Pass 2  gitignore OFF  → tracked-but-gitignored files
         (only when git_aware=true, include_untracked=false, tracked set non-empty)
Pass 3  gitignore OFF  → attic_include_rule overrides
         (only when git_aware=true, attic_include_rules non-empty)

All passes: security_forbidden() checked FIRST, unconditionally.
```

---

## Files Modified

| File | Change |
|------|--------|
| `crates/attic-discovery/src/walk.rs` | Three-pass walk; `git_tracked_files()`; submodule detection; 9 new tests |
| `crates/attic-discovery/src/manifest.rs` | `FileStat`; unstable-capture detection; `ManifestEntry.unstable`; 3 new tests |
| `crates/attic-discovery/src/lib.rs` | `DownstreamContent` enum; secrets preprocessing in `discover()`; 2 new tests |
| `crates/attic-discovery/src/policy.rs` | `policy_version: u32`; 1 new test *(previous session)* |
| `docs/decisions/ADR-006-submodule-handling.md` | Corrected Consequences section |

---

## Test Count Delta

| Module | Before Phase 1B | After Phase 1B |
|--------|----------------|----------------|
| `walk` | 10 | 17 (+7) |
| `manifest` | 7 | 10 (+3) |
| `lib` (integration) | 7 | 9 (+2) |
| `policy` | 5 | 6 (+1) |
| **Total** | **29** | **103** *(all crate tests)* |

---

## Outstanding Items / Carry-Forward

None. All Phase 1B gates pass. Phase 1C (analyzers) may proceed.
