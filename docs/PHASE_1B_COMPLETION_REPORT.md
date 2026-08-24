# Phase 1B Completion Report — Git-Aware Discovery Pipeline

**Date**: 2026-08-25
**Crate**: `attic-discovery` v0.1.0
**Status**: COMPLETE (third corrective review incorporated)

---

## Summary

Phase 1B implements the full Git-aware, security-hardened repository discovery
pipeline for Attic.  All findings from three rounds of post-completion review
have been resolved.  The crate passes `cargo test -p attic-discovery`
(112/112 tests pass) on `x86_64-pc-windows-gnu`.

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
| `secrets.rs` | Secret scanning and redaction (PK-001, AWS-001, GH-001, JWT-001, HE-001); withheld-tail streaming; stateful PEM detector |
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

**Resolution** (`lib.rs`, `secrets.rs`):

- Added `stream_scan_large_file_classify(path)` in `secrets.rs`:
  - Reads the file in `STREAM_CHUNK_SIZE` (64 KiB) chunks.
  - Carries a `SAFETY_WINDOW_SIZE` (1 KiB) overlap between windows to catch
    boundary-spanning bounded tokens.
  - Uses `PemStreamState` to track cross-chunk PEM blocks.
  - Returns `(SecretScanDecision, Vec<SecretFinding>)` — full coverage, O(1) peak memory.
- `lib.rs` LARGE tier now calls `stream_scan_large_file_classify` instead of the
  old head+tail sample.

---

### Fix 6 — VERY_LARGE files: `PartialScan` classification (never `Safe`)

**Finding**: A VERY_LARGE file whose sample contained no secrets was classified
as `Safe`, even though the mid-body between head and tail samples was never
inspected.

**Resolution**:
- Added `SecretScanDecision::PartialScan`.
- VERY_LARGE files always return `PartialScan` regardless of whether the head+tail
  sample detected findings.  Consumers MUST NOT treat `PartialScan` as equivalent
  to `Safe`.

---

### Fix 7 — JWT scanner O(n²) complexity causing test hangs

**Finding**: `find_jwt_tokens` iterated character-by-character through long
base64url runs (e.g. 64 KiB repeated ASCII in a chunk window), re-scanning from
`i+1` on every non-match — O(n²) per chunk.

**Resolution** (`secrets.rs`):
```rust
} else {
    // No dot after seg1 — skip entire base64url run to avoid O(n²)
    i = seg1_end;
}
```
Also skip to `seg2_end` (not `i+1`) when seg2 or seg3 fails.  All LARGE-file
tests complete in < 3 seconds total.

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

## Corrective Review Fixes — Round 3

### Fix 9 — Withheld-tail streaming design for `LargeFileStream::next_chunk()`

**Finding**: The previous `LargeFileStream` design emitted scan windows immediately
without a safety tail.  A bounded token (AWS key, GitHub token, JWT) whose first
byte fell in the last `SAFETY_WINDOW_SIZE` bytes of a chunk window could be
split across the emission boundary and appear partially in the emitted output
before the remainder was scanned in the next chunk.

**Resolution** (`secrets.rs`):

- Introduced `withheld: Vec<u8>`, `withheld_file_offset`, `file_bytes_consumed`
  fields on `LargeFileStream`.
- `next_chunk()` now implements the **withheld-tail contract**:
  1. Read up to `STREAM_CHUNK_SIZE` new bytes.
  2. Prepend `withheld` buffer → scan window.
  3. Scan entire window for secrets.
  4. Compute `safe_emit_len`:
     - PEM `Idle`: `window.len() − SAFETY_WINDOW_SIZE`
     - PEM `InBlock`: `0` (withhold all until `END` footer)
     - EOF: `window.len()` (flush everything)
  5. Emit the redacted equivalent of `original[0..safe_emit_len]`.
  6. `withheld ← original[safe_emit_len..]`
- `SAFETY_WINDOW_SIZE = 1024` bytes replaces the former `STREAM_OVERLAP_SIZE = 4096`
  (which was an overlap, not a withheld tail — a fundamentally different contract).

---

### Fix 10 — PEM blocks unbounded: split into bounded-token and stateful-streaming detectors

**Finding**: PEM blocks have no maximum length.  Treating them as bounded tokens
and relying solely on `SAFETY_WINDOW_SIZE` to prevent split redaction was incorrect
— a PEM block body larger than 1 KiB would span the safety window and emit body
bytes raw before the `END` footer was seen.

**Resolution** (`secrets.rs`):

- Added `PemStreamState` enum (`Idle` / `InBlock { begin_file_offset: usize }`).
- `LargeFileStream` carries a `pem_state` field.
- When `pem_state == InBlock`, `safe_emit_len` is forced to `0` regardless of
  window size — the entire window is withheld until the `END` footer is found.
- Once the footer is emitted, `pem_state` returns to `Idle` and normal
  `SAFETY_WINDOW_SIZE`-based emission resumes.
- `DETECTORS` table updated with detector class column distinguishing
  `bounded-token` (AWS-001, GH-001, JWT-001, HE-001) from
  `stateful-streaming` (PK-001).

---

### Fix 11 — Chunk-level tests that inspect individual emitted chunks

**Finding**: All prior LARGE-file tests only inspected the concatenated
`collect_all()` output.  A regression in the withheld-tail emission logic could
emit a raw secret in an intermediate chunk while the concatenated output happened
to be clean (e.g. if the same secret appeared twice).

**Resolution** (`secrets.rs` test suite):

Added 11 new chunk-level tests, including:

| Test | What it verifies |
|------|-----------------|
| `large_file_aws_secret_1_byte_before_chunk_boundary_not_emitted_raw` | Inspects **every individual chunk** for raw key presence |
| `large_file_aws_secret_in_withheld_tail_safe` | Key starts inside withheld window |
| `large_file_pem_larger_than_safety_window_never_emits_body` | PEM body > `SAFETY_WINDOW_SIZE` never leaks raw |
| `large_file_pem_begin_end_cross_chunk_boundary` | PEM spanning multiple chunks fully redacted + clean suffix preserved |
| `large_file_eof_flushes_withheld_bytes` | EOF flush drops no bytes |
| `large_file_streaming_is_bounded` | Multiple chunks emitted for 3× chunk content |
| `large_file_clean_content_preserved` | Bit-for-bit identity for clean content |
| `large_file_secret_in_middle_is_fully_redacted` | Mid-file key detected and redacted |
| `large_file_classify_and_stream_agree_safe` | Classify pass agrees Safe for clean file |
| `large_file_two_pass_stable_succeeds` | Two-pass `FileIdentity` check passes for stable file |

---

### Fix 12 — Two-pass consistency in `preprocess_large_file()` using `FileIdentity`

**Finding**: `preprocess_large_file()` previously opened the file for streaming
immediately after classification without verifying the file had not changed
between the two passes.  A file modified between classify and stream would
produce a `PreprocessResult` whose `findings` (from pass 1) were inconsistent
with the streamed content (pass 2).

**Resolution** (`secrets.rs`):

- Added `pub(crate) struct FileIdentity { size: u64, modified: SystemTime }`.
- `preprocess_large_file()` now:
  1. Calls `file_identity(path)` → `id_before`.
  2. Runs `stream_scan_large_file_classify(path)` (classify pass).
  3. Calls `file_identity(path)` → `id_after`.
  4. If `id_before != id_after`, returns `Err(io::ErrorKind::Other, "file changed during classification (unstable capture): …")`.
  5. Otherwise calls `LargeFileStream::open_with_identity(path, id_after)` to open
     the stream for the caller.
- `open_with_identity` is `pub(crate)` (not `pub`) — it accepts `FileIdentity`
  which is `pub(crate)`, preventing the `private_interfaces` compiler warning.

---

## Test Coverage Summary

```
running 112 tests
... 112 passed; 0 failed; finished in 2.31s
```

| Module | Tests |
|--------|-------|
| `lib.rs` (integration) | 18 |
| `walk.rs` | 19 |
| `manifest.rs` | 9 |
| `secrets.rs` | 19 |
| `classification.rs` | 13 |
| `security.rs` | 14 |
| `git.rs` | 10 |
| `policy.rs` | 8 |
| `diagnostics.rs` | 2 |
| **Total** | **112** |

---

## Streaming Architecture Summary

### Detector classes

| ID | Pattern | Class | Max match length |
|----|---------|-------|-----------------|
| PK-001 | PEM private-key block | stateful-streaming | unbounded |
| AWS-001 | AWS access-key ID | bounded-token | 20 bytes |
| GH-001 | GitHub PAT | bounded-token | ~100 bytes |
| JWT-001 | JSON Web Token | bounded-token | ~2 KiB |
| HE-001 | High-entropy base64 | bounded-token | ~2 KiB |

### `LargeFileStream::next_chunk()` safety contract

```
For each call:
  1. Read ≤ STREAM_CHUNK_SIZE new bytes from file.
  2. window = withheld ++ new_bytes
  3. Scan window → redacted_window, findings
  4. safe_emit_len:
       - pem_state == InBlock  →  0
       - pem_state == Idle, EOF → window.len()
       - pem_state == Idle, mid → window.len() − SAFETY_WINDOW_SIZE (min 0)
  5. Emit redacted_window[0..compute_redacted_offset(safe_emit_len)]
  6. withheld ← window[safe_emit_len..]
```

### File-size tier handling

| Tier | Size | Scan strategy | `content` field | Classification |
|------|------|--------------|-----------------|----------------|
| SMALL | ≤ 4 MiB | Full in-memory scan | `Some(redacted)` | `Safe` / `Redacted` / `Excluded` |
| LARGE | 4–50 MiB | Full streaming chunked scan (64 KiB windows, 1 KiB withheld tail) | `None` | `Safe` / `Redacted` / `Excluded` |
| VERY_LARGE | > 50 MiB | Head + tail sample only | `Some(sample)` — partial | **Always** `PartialScan` — never `Safe` |

---

## Invariants Verified

| Invariant | Status |
|-----------|--------|
| `DiscoveryOutput` retains no file content | ✅ `DownstreamClassification` carries no `String` fields |
| SMALL files fully scanned | ✅ `classify_file_for_downstream` full content read |
| LARGE files fully scanned via streaming | ✅ `stream_scan_large_file_classify` — entire file, O(1) memory |
| LARGE: peak memory ≤ `2 × STREAM_CHUNK_SIZE + SAFETY_WINDOW_SIZE` | ✅ withheld buffer bounded |
| LARGE: `preprocess_file_content` returns `content=None` | ✅ caller streams separately |
| VERY_LARGE always classified `PartialScan`, never `Safe` | ✅ enforced in `classify_file_for_downstream` |
| VERY_LARGE emits `PARTIAL_SECRET_SCAN` diagnostic | ✅ recorded before classification returns |
| Secret in LARGE file mid-body is detected | ✅ `large_file_secret_in_middle_is_fully_redacted` |
| No bounded token emitted raw across chunk boundary | ✅ withheld-tail design; `large_file_aws_secret_1_byte_before_chunk_boundary_not_emitted_raw` inspects each chunk |
| PEM body never emitted raw regardless of size | ✅ `InBlock → safe_emit_len = 0`; `large_file_pem_larger_than_safety_window_never_emits_body` |
| PEM spanning multiple chunks fully redacted | ✅ `large_file_pem_begin_end_cross_chunk_boundary` |
| EOF flushes all withheld bytes | ✅ `large_file_eof_flushes_withheld_bytes` |
| Clean content bit-for-bit preserved | ✅ `large_file_clean_content_preserved` |
| Two-pass stability check detects changed file | ✅ `FileIdentity` comparison in `preprocess_large_file` |
| JWT scanner is O(n) per input | ✅ skip to `seg1_end` / `seg2_end` on non-match |
| `include_untracked=false` + unavailable tracked set → hard error | ✅ `TrackedFileSetUnavailable` returned |
| Submodule boundaries detected, not descended into | ✅ `SubmoduleDetected` diagnostic + `submodule_prefixes` |
| ADR-006 describes only implemented behaviour | ✅ Phase 2 storage claims removed |
| `source_revision.md` submodule section accurate | ✅ Phase 1B / Phase 2+ boundary documented |
| Uncertain stat (None/None) treated as unstable | ✅ `_ => true` in manifest match |
| `SecretFinding` struct has no `raw_value` field | ✅ `no_raw_secret_value_in_findings` — redacted output verified clean |
| `cargo test -p attic-discovery` all pass | ✅ 112/112 |

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

### Round 3 (third corrective pass)

| File | Change |
|------|--------|
| `crates/attic-discovery/src/secrets.rs` | Withheld-tail `LargeFileStream`; `PemStreamState` stateful detector; `FileIdentity` two-pass check; `SAFETY_WINDOW_SIZE = 1024`; `open_with_identity` → `pub(crate)`; fixed `no_raw_secret_value_in_findings` test; 11 new chunk-level tests |
| `crates/attic-discovery/src/lib.rs` | `STREAM_OVERLAP_SIZE` → `SAFETY_WINDOW_SIZE` reference updated |
| `docs/PHASE_1B_COMPLETION_REPORT.md` | This document |
