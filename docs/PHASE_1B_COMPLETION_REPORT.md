# Phase 1B Completion Report — Git-Aware Discovery Pipeline

**Date**: 2026-08-24
**Crate**: `attic-discovery` v0.1.0
**Status**: COMPLETE (second corrective review incorporated)

---

## Summary

Phase 1B implements the full Git-aware, security-hardened repository discovery
pipeline for Attic.  All findings from two rounds of post-completion review have
been resolved.  The crate passes `cargo clippy -D warnings` (0 warnings) and
`cargo test` (114/114 tests pass) on `x86_64-pc-windows-msvc`.

---

## Deliverables

| Module | Purpose |
|--------|---------|
| `lib.rs` | Public API: `discover()`, `preprocess_file_content()`, `DownstreamClassification`, `FileSizeTier`, streaming LARGE scan |
| `walk.rs` | `ignore`-crate walk with security exclusions, default exclusions, and submodule boundary detection |
| `manifest.rs` | BLAKE3 per-file content hashing and `SourceManifest` construction |
| `classification.rs` | Priority classification (High/Normal/Low) and glob-rule evaluation |
| `policy.rs` | `DiscoveryPolicy` builder with validation and hash |
| `git.rs` | Git root detection and `GitRepoMeta` (branch + HEAD SHA) |
| `secrets.rs` | Secret scanning and redaction (PK-001, AWS-001, GH-001, JWT-001, HE-001); O(1) per character |
| `security.rs` | Security boundary enforcement and forbidden-path list |
| `diagnostics.rs` | Non-fatal `Diagnostic` events |
| `error.rs` | `DiscoveryError` enum |

---

## Corrective Review Fixes — Round 1

### Fix 1 — No content retention in `DiscoveryOutput` (large_files.md compliance)

**Finding**: `discover()` was retaining full file content in `DiscoveryOutput`,
violating the bounded-memory contract.

**Resolution**:
- Renamed `DownstreamContent` → `DownstreamClassification`.  The new enum
  carries **no content strings** — only classification metadata
  (`Safe { size_tier }`, `Redacted { size_tier, findings }`, `Excluded`,
  `ScanSkipped { reason }`).
- Added `FileSizeTier` enum (Small / Large / VeryLarge) with thresholds from
  `docs/contracts/large_files.md`: SMALL < 4 MiB, LARGE 4–50 MiB, VERY_LARGE
  above 50 MiB.
- Added `preprocess_file_content(abs_path, repo_relative) -> io::Result<PreprocessResult>`
  as the public lazy per-file content accessor for downstream consumers.

---

### Fix 2 — `include_untracked=false` fails closed (`TrackedFileSetUnavailable`)

**Finding**: When `include_untracked=false` and `git ls-files` was unavailable,
the walk silently broadened scope to include untracked files.

**Resolution** (`error.rs` + `walk.rs`):
- Added `DiscoveryError::TrackedFileSetUnavailable { reason: String }`.
- Walk fails immediately with this error rather than silently expanding scope.

---

### Fix 3 — ADR-006 corrected (no false Phase 2 claims)

**Finding**: ADR-006 falsely claimed Phase 1B creates storage rows, `WorkspaceSnapshot`,
`SourceRevision`, and Phase 2 scheduling for submodules.

**Resolution**: ADR-006 rewritten to describe only actual Phase 1B behaviour.
Phase 2+ storage work documented as future work.

---

### Fix 4 — Uncertain stat treated as unstable (fail-closed)

**Finding**: `manifest.rs` treated a stat failure as "file is stable" via `_ => false`.

**Resolution**: Changed to `_ => true` — any unavailable stat is fail-closed unstable.

---

## Corrective Review Fixes — Round 2

### Fix 5 — LARGE files: bounded streaming scan replacing incorrect head+tail sampling

**Finding**: The original implementation applied a head+tail sample scan to both
LARGE (4–50 MiB) and VERY_LARGE (> 50 MiB) files.  A LARGE file whose secret
existed only in the middle would be misclassified as `Safe` despite never having
the mid-body inspected.  This is a security contract violation.

**Resolution** (`lib.rs`):

- Added constants:
  ```rust
  pub const LARGE_SCAN_CHUNK_BYTES: usize = 128 * 1024; // 128 KiB per chunk
  const CHUNK_OVERLAP_BYTES: usize = 256;               // overlap for boundary secrets
  ```
- Added `stream_scan_large_file_classify(abs_path, repo_relative)`:
  - Opens the file and reads it in `LARGE_SCAN_CHUNK_BYTES` chunks via `BufReader`.
  - Carries the last `CHUNK_OVERLAP_BYTES` of each chunk as an overlap prefix on
    the next window to catch secrets that span a chunk boundary.
  - At most `LARGE_SCAN_CHUNK_BYTES + CHUNK_OVERLAP_BYTES` bytes are live at any
    one time — the full file is never loaded into memory.
  - Returns `Safe { Large }`, `Redacted { Large, findings }`, or `ScanSkipped`.
- `classify_file_for_downstream` updated: LARGE tier now calls
  `stream_scan_large_file_classify` instead of the old head+tail `read_sample`.
- `preprocess_file_content` updated: LARGE tier returns `content: None` (not
  buffered); the streaming scan determines the decision and findings only.

**New tests**:
- `large_file_secret_in_middle_is_detected_by_streaming_scan` — AWS key at exact
  midpoint of a 4+ MiB file must produce `Redacted` + `content=None`.
- `large_clean_file_classifies_safe_with_no_content` — clean LARGE file produces
  `Safe` + `content=None`.

---

### Fix 6 — VERY_LARGE files: `PartialScan` classification (never `Safe`)

**Finding**: A VERY_LARGE file whose sample contained no secrets was classified
as `Safe`, even though the mid-body between head and tail samples was never
inspected.  Downstream consumers could treat it as fully audited.

**Resolution**:

- Added `SecretScanDecision::PartialScan` to `secrets.rs`:
  ```rust
  /// Only a sample of the file was scanned (VERY_LARGE tier).
  /// Mid-body was NOT inspected. MUST NOT be treated as equivalent to `Safe`.
  PartialScan,
  ```
- Added `DownstreamClassification::PartialScan { findings }` to `lib.rs`:
  ```rust
  /// VERY_LARGE file: only head + tail sample scanned.
  /// Body between samples NOT inspected.
  /// Consumers MUST NOT treat this as equivalent to Safe.
  PartialScan { findings: Vec<SecretFinding> },
  ```
- `classify_file_for_downstream` VERY_LARGE arm:
  - Reads head + tail sample via `read_sample`.
  - Records `PARTIAL_SECRET_SCAN` diagnostic (mid-body not inspected).
  - Returns `DownstreamClassification::PartialScan { findings }` — **never** `Safe`,
    regardless of whether findings were detected in the sample.
- `preprocess_file_content` VERY_LARGE arm returns `SecretScanDecision::PartialScan`
  with `content: Some(sample)` so callers can display the sampled portion.

**New tests**:
- `very_large_clean_file_classified_as_partial_scan_not_safe` — clean VERY_LARGE
  sparse file must produce `PartialScan`, not `Safe`.
- `very_large_file_with_secret_in_sample_classifies_partial_scan_with_findings` —
  secret in VERY_LARGE head sample produces `PartialScan` with non-empty findings.
- `very_large_file_emits_partial_secret_scan_diagnostic` — `PARTIAL_SECRET_SCAN`
  diagnostic must be recorded by `discover()` for every classified VERY_LARGE file.

---

### Fix 7 — JWT scanner O(n²) complexity causing test hangs

**Finding**: `find_jwt_tokens` in `secrets.rs` iterated character-by-character
through long base64url runs (e.g. 128 KiB repeated ASCII in a LARGE-file chunk
window).  After finding no dot separator, the loop fell through to `i += 1` and
rescanned from the next position — O(n²) per chunk window.

This caused the three LARGE-file tests to hang indefinitely (>60 seconds each).

**Root cause**: The `advance_base64url` helper scans forward to the end of the
base64url run in O(k) time.  Without skipping past `seg1_end` on a non-match,
the outer loop re-entered the same run from `i+1` through `i+k-1`, yielding
O(k²) total work for a single run of length k.

**Resolution** (`secrets.rs`):
```rust
if seg1_end < len && bytes[seg1_end] == b'.' {
    // ... try seg2, seg3 ...
    // On seg2/seg3 failure: skip past seg2_end, not i+1
    i = seg2_end;
} else {
    // No dot after seg1 — skip entire base64url run to avoid O(n²)
    i = seg1_end;
}
```

All three LARGE-file tests now complete in < 2 seconds total.

**No change to detected patterns** — the fix only changes the scan position on
non-matches; valid JWTs (which always have dots separating three base64url
segments) are still detected correctly.

---

### Fix 8 — `docs/contracts/source_revision.md` submodule boundary section

Added a dedicated **§Submodule Boundary Handling** section clarifying:
- Phase 1B only detects boundaries, emits `SubmoduleDetected` diagnostics, and
  populates `WalkResult.submodule_prefixes`.
- Phase 1B does **not** create `core_repositories` rows, `SourceRevision`
  records, or schedule Phase 2 work for submodules.
- Phase 2+ workspace orchestration is responsible for registering submodules as
  independent `Repository` entries.

---

## Test Coverage Summary

```
running 114 tests
... 114 passed; 0 failed; finished in 1.74s
```

| Module | Tests |
|--------|-------|
| `lib.rs` (integration) | 18 |
| `walk.rs` | 17 |
| `manifest.rs` | 9 |
| `secrets.rs` | 14 |
| `classification.rs` | 13 |
| `security.rs` | 14 |
| `git.rs` | 10 |
| `policy.rs` | 8 |
| `diagnostics.rs` | 2 |
| **Total** | **114** |

---

## Large-File Contract Summary

| Tier | Size | Scan strategy | `preprocess_file_content` content | Classification |
|------|------|--------------|----------------------------------|----------------|
| SMALL | < 4 MiB | Full text scan | `Some(redacted_text)` | `Safe` / `Redacted` / `Excluded` |
| LARGE | 4–50 MiB | Full streaming chunked scan (128 KiB windows, 256-byte overlap) | `None` — not buffered | `Safe` / `Redacted` / `Excluded` |
| VERY_LARGE | > 50 MiB | Head + tail sample only (8 KiB each) | `Some(sample)` — partial | **Always** `PartialScan` — never `Safe` |

---

## Invariants Verified

| Invariant | Status |
|-----------|--------|
| `DiscoveryOutput` retains no file content | ✅ `DownstreamClassification` carries no `String` fields |
| SMALL files fully scanned | ✅ `classify_file_for_downstream` full content read |
| LARGE files fully scanned via streaming | ✅ `stream_scan_large_file_classify` — entire file, O(1) memory |
| LARGE: peak memory = `LARGE_SCAN_CHUNK_BYTES + CHUNK_OVERLAP_BYTES` | ✅ Never allocates full file |
| LARGE: `preprocess_file_content` returns `content=None` | ✅ Caller must stream separately |
| VERY_LARGE always classified `PartialScan`, never `Safe` | ✅ Enforced in `classify_file_for_downstream` and `preprocess_file_content` |
| VERY_LARGE emits `PARTIAL_SECRET_SCAN` diagnostic | ✅ Recorded before classification returns |
| Secret in LARGE file mid-body is detected | ✅ `large_file_secret_in_middle_is_detected_by_streaming_scan` |
| JWT scanner is O(n) per input | ✅ Skip to `seg1_end` / `seg2_end` on non-match |
| `include_untracked=false` + unavailable tracked set → hard error | ✅ `TrackedFileSetUnavailable` returned |
| Submodule boundaries detected, not descended into | ✅ `SubmoduleDetected` diagnostic + `submodule_prefixes` |
| ADR-006 describes only implemented behaviour | ✅ Phase 2 storage claims removed |
| `source_revision.md` submodule section accurate | ✅ Phase 1B / Phase 2+ boundary documented |
| Uncertain stat (None/None) treated as unstable | ✅ `_ => true` in manifest match |
| No raw secret values in `findings` | ✅ `no_raw_secret_value_in_findings` test |
| `cargo clippy -D warnings` clean | ✅ 0 warnings |
| `cargo test` all pass | ✅ 114/114 |

---

## Files Modified

### Round 1 (first corrective pass)

| File | Change |
|------|--------|
| `crates/attic-discovery/src/lib.rs` | `DownstreamClassification` (no content), `FileSizeTier`, `preprocess_file_content` |
| `crates/attic-discovery/src/error.rs` | Added `TrackedFileSetUnavailable` variant |
| `crates/attic-discovery/src/walk.rs` | Fail-closed tracked-file-set step |
| `crates/attic-discovery/src/manifest.rs` | `_ => true` fail-closed |
| `docs/decisions/ADR-006-submodule-handling.md` | Removed false Phase 2 claims; future work section added |

### Round 2 (second corrective pass)

| File | Change |
|------|--------|
| `crates/attic-discovery/src/lib.rs` | Streaming LARGE scan (`stream_scan_large_file_classify`); `PartialScan` classification; 6 new tests |
| `crates/attic-discovery/src/secrets.rs` | `SecretScanDecision::PartialScan` variant; O(n) JWT scanner fix |
| `docs/contracts/source_revision.md` | Added §Submodule Boundary Handling section |
| `docs/PHASE_1B_COMPLETION_REPORT.md` | This document |
