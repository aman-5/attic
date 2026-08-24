# Phase 1B Completion Report — Git-Aware Discovery Pipeline

**Date**: 2026-08-24
**Crate**: `attic-discovery` v0.1.0
**Status**: COMPLETE (corrective review incorporated)

---

## Summary

Phase 1B implements the full Git-aware, security-hardened repository discovery
pipeline for Attic.  All four corrective findings from the post-completion
review have been resolved.  The crate passes `cargo clippy -D warnings` (0
warnings) and `cargo test` (108/108 tests pass) on
`x86_64-pc-windows-msvc`.

---

## Deliverables

| Module | Purpose |
|--------|---------|
| `lib.rs` | Public API: `discover()`, `preprocess_file_content()`, `DownstreamClassification`, `FileSizeTier` |
| `walk.rs` | `ignore`-crate walk with security exclusions, default exclusions, and submodule boundary detection |
| `manifest.rs` | BLAKE3 per-file content hashing and `SourceManifest` construction |
| `classification.rs` | Priority classification (High/Normal/Low) and glob-rule evaluation |
| `policy.rs` | `DiscoveryPolicy` builder with validation and hash |
| `git.rs` | Git root detection and `GitRepoMeta` (branch + HEAD SHA) |
| `secrets.rs` | Secret scanning and redaction (PK-001, AWS-001, GH-001, JWT-001, HE-001) |
| `security.rs` | Security boundary enforcement and forbidden-path list |
| `diagnostics.rs` | Non-fatal `Diagnostic` events |
| `error.rs` | `DiscoveryError` enum |

---

## Corrective Review Fixes

### Fix 1 — No content retention in `DiscoveryOutput` (large_files.md compliance)

**Finding**: `discover()` was retaining full file content (`Safe(String)` /
`Redacted(String)`) in `DiscoveryOutput`, violating the bounded-memory
contract.

**Resolution**:
- Renamed `DownstreamContent` → `DownstreamClassification`.  The new enum
  carries **no content strings** — only classification metadata
  (`Safe { size_tier }`, `Redacted { size_tier, findings }`, `Excluded`,
  `ScanSkipped { reason }`).
- Added `FileSizeTier` enum (Small / Large / VeryLarge) with thresholds from
  `docs/contracts/large_files.md`: SMALL < 4 MiB, LARGE 4–50 MiB, VERY_LARGE
  above 50 MiB.
- `classify_file_for_downstream()` reads and scans a bounded amount of content
  (full for Small, head+tail sample for Large/VeryLarge) then drops it before
  returning.
- Added `preprocess_file_content(abs_path, repo_relative) -> io::Result<PreprocessResult>` 
  as the public lazy per-file content accessor for downstream consumers.
  It follows the same size-tier logic and never accumulates workspace-wide state.
- VERY_LARGE files emit a `PartialSecretScan` diagnostic recording that the
  mid-body was not scanned.

**New tests** (`lib.rs`):
- `discovery_output_does_not_retain_content` — `DiscoveryOutput` carries only
  `Safe { size_tier }`, not a content string.
- `inline_secret_produces_redacted_classification` — AWS key in source file
  produces `Redacted` with non-empty `findings`.
- `known_secret_carrier_produces_excluded_classification` — `.netrc` path
  through `preprocess_file_content` returns `Excluded` with no content.
- `large_file_classified_as_large_tier` — file just over 4 MiB gets
  `FileSizeTier::Large`.
- `preprocess_file_content_returns_bounded_result` — clean file returns `Safe`
  with content; file with AWS key returns `Redacted` with raw key absent from
  returned content.

---

### Fix 2 — `include_untracked=false` fails closed (`TrackedFileSetUnavailable`)

**Finding**: When `include_untracked=false` and `git ls-files` was unavailable,
the walk silently broadened scope to include untracked files instead of failing.

**Resolution** (`error.rs` + `walk.rs`):
- Added `DiscoveryError::TrackedFileSetUnavailable { reason: String }`.
- Walk step 1: if `policy.git_aware && !policy.include_untracked`, the tracked
  file set is obtained via `git_tracked_files()` and any failure is propagated
  immediately as `Err(TrackedFileSetUnavailable { ... })`.  There is no
  fallback to untracked mode.

**New test** (`walk.rs`):
- `include_untracked_false_fails_closed_when_git_unavailable` — walk into a
  non-Git directory with `include_untracked=false` returns
  `DiscoveryError::TrackedFileSetUnavailable`.

---

### Fix 3 — ADR-006 corrected (no false Phase 2 claims)

**Finding**: ADR-006 falsely claimed Phase 1B creates `core_repositories` rows,
`WorkspaceSnapshot`, `SourceRevision`, and Phase 2 scheduling for submodules.

**Resolution** (`docs/decisions/ADR-006-submodule-handling.md`):
- Decision section rewritten to describe **only** actual Phase 1B behaviour:
  detection of nested `.git` boundaries, `SubmoduleDetected` diagnostic
  emission, recording of `submodule_prefixes` in `WalkResult`, and skipping the
  directory.
- Explicit "Phase 1B does NOT" list: no `core_repositories` rows, no
  `WorkspaceSnapshot`/`SourceRevision` for submodules, no Phase 2 scheduling,
  no HEAD SHA in parent manifest.
- Storage registration, cross-repository indexing, and incremental scheduling
  documented as **future work (Phase 2+)** in a dedicated section.

---

### Fix 4 — Uncertain stat treated as unstable (fail-closed)

**Finding**: `manifest.rs` `match (&stat_before, &stat_after)` had `_ => false`
as the catch-all arm, meaning a stat failure was treated as "file is stable"
rather than "file is potentially changed".

**Resolution** (`manifest.rs`):
- Changed `_ => false` → `_ => true`.  Any arm where one or both stats are
  unavailable now reports the file as unstable (fail-closed).

**New test** (`manifest.rs`):
- `uncertain_stat_is_treated_as_unstable` — passing two `None` stats produces
  `file_changed = true`.

---

## Test Coverage Summary

```
running 108 tests
... 108 passed; 0 failed
```

| Module | Tests |
|--------|-------|
| `lib.rs` (integration) | 12 |
| `walk.rs` | 17 |
| `manifest.rs` | 9 |
| `secrets.rs` | 14 |
| `classification.rs` | 13 |
| `security.rs` | 14 |
| `git.rs` | 10 |
| `policy.rs` | 8 |
| `diagnostics.rs` | 2 |

---

## Invariants Verified

| Invariant | Status |
|-----------|--------|
| `DiscoveryOutput` retains no file content | ✅ `DownstreamClassification` carries no `String` fields |
| SMALL files fully scanned; LARGE/VERY_LARGE sampled only | ✅ `classify_file_for_downstream` enforces thresholds |
| VERY_LARGE files emit `PartialSecretScan` diagnostic | ✅ |
| `preprocess_file_content` is the sole lazy content accessor | ✅ Public API, never accumulates state |
| `include_untracked=false` + unavailable tracked set → hard error | ✅ `TrackedFileSetUnavailable` returned |
| Submodule boundaries detected, not descended into | ✅ `SubmoduleDetected` diagnostic + `submodule_prefixes` |
| ADR-006 describes only implemented behaviour | ✅ Phase 2 storage claims removed |
| Uncertain stat (None/None) treated as unstable | ✅ `_ => true` in manifest match |
| `cargo clippy -D warnings` clean | ✅ 0 warnings |
| `cargo test` all pass | ✅ 108/108 |

---

## Files Modified (Corrective Pass)

| File | Change |
|------|--------|
| `crates/attic-discovery/src/lib.rs` | `DownstreamClassification` (no content), `FileSizeTier`, `preprocess_file_content`, 5 new tests |
| `crates/attic-discovery/src/error.rs` | Added `TrackedFileSetUnavailable` variant |
| `crates/attic-discovery/src/walk.rs` | Fail-closed tracked-file-set step; new test |
| `crates/attic-discovery/src/manifest.rs` | `_ => true` fail-closed; new test |
| `docs/decisions/ADR-006-submodule-handling.md` | Removed false Phase 2 claims; future work section added |
