# Freshness: verify before answer

Attic never answers from an index it has not just checked against the files on
disk. This document describes the mechanism, its guarantees and limits, and its
measured cost.

## Why

Watchers and background reconciliation can miss or delay events (branch
switches, pulls, bursts of edits, plain folders copied over, network drives).
Before this change an answer could combine current files, an older index and
outdated cross-repository links without saying so.

## The check (`ensure_current`)

Every `context` (all modes), `file` and `repo_map` call runs the check for the
repositories it reads before any index data is read
(`crates/attic-incremental/src/gate.rs`, `crates/attic-server/src/freshness.rs`):

1. Take the repository lock (shared with the scheduler and full indexing: one
   check or index run per repository at a time).
2. Walk the folder and compare each file with what was indexed:
   - **unchanged without reading** only when size, last-write time, change time
     (NTFS ChangeTime / Unix ctime) and file id all match and both timestamps
     are older than the indexed capture time minus 2 s (racy-file rule);
   - everything else is hashed (BLAKE3); only real content changes re-index.
3. Re-index added / changed / deleted / renamed files in fenced chunks of 500:
   a publication is applied only if the store is still at the version the check
   started from; otherwise nothing is written and the check retries (3 retries,
   then `SOURCE_CHANGING`).
4. Return the store version and the source label.

Callers asking about the same repository at the same time share work: one
running check plus at most one queued follow-up per repository (a caller never
receives a check that started before its question). A check longer than 30 s
returns `TIMEOUT` and keeps running; the retry joins it.

## One version per answer

Each store has a persisted `(epoch, version)` (`ops_store_version`) bumped
inside every write transaction that changes index data. After an answer's reads
the server re-reads it; if it moved, the answer is retried once and otherwise
refused with `VERSION_EXPIRED`. The `version_token` returned with each answer
pins later calls of the same answer to the same versions.

The `file` tool serves live bytes, so it additionally checks that the file still
matches its indexed version (hash for files up to 1 MiB, size/timestamps/file
id above). Unindexed paths are served with `file_indexed: false`.

## Never delete what could not be seen

- Missing root, or an empty root whose directory identity differs from the
  indexed one (unmounted drive, replaced folder) → `SOURCE_UNAVAILABLE`.
- A re-cloned root (new identity, files present) → full hash, deletions only
  if the walk had no errors.
- Unreadable subdirectory → everything indexed under it is uncertain.
- Unreadable / locked file → uncertain; the rest of the repository publishes.

## Cross-repository links

Links are built from the **indexed** bytes of dependency manifests and `.proto`
files (stored at index time for files the secret scan found clean), never from
newer disk bytes. A catalog version (repositories, roots, dependency content
hashes, provider anchors) is stamped on each link snapshot; answers that use
links refresh the catalog first and relink only when that version changed.
Ordinary source edits do not relink. Resolution outcomes that produce no link
(`UNRESOLVED`, `AMBIGUOUS`) are persisted and returned with the answer;
external-world packages without a workspace provider are not reported as
problems. Each package link carries `version_compat`: npm semver ranges, Maven
and OSGi version ranges and PEP 440 specifiers are evaluated; anything that
cannot be decided is `UNCHECKED`.

## Imports between files

When a file is republished its incoming import edges move to the new version;
when it is deleted they become unresolved with `target_missing`; when a file
appears (added, restored, renamed-to) importers whose imports could now resolve
are re-resolved in the same publication.

## Production commit mode (`at_commit`)

`at_commit` materializes the named commit through a private Git index
(`read-tree` + `checkout-index --prefix`) into
`<data>/commits/<repository>/<sha>/`, indexes it under the same repository id
and answers from it. `HEAD`, the user's index and working tree are untouched.
Up to 4 commit stores stay open; commit data is capped at 2 GiB (least recently
used first). Plain folders return `COMMIT_UNAVAILABLE`.

## Limits

- A local checkout is not proof of what is deployed: use `at_commit` with the
  deployed SHA.
- Unsaved editor buffers are not seen.
- A tool that rewrites content while restoring size, both timestamps and the
  file id evades the fast path (same limitation as Git); `deep_verify` or
  `[freshness] always_hash_paths` hash everything.
- Cross-repository links are dependency-level, not function-call-level.

## Measured cost

Release build, Windows 11, NTFS, warm cache, one thread per repository
(`crates/attic-incremental/tests/freshness_bench.rs`). The server checks the
repositories of one answer in parallel.

| Repository | Files | Full index (ms) | Check, nothing changed (ms) | Check, hash everything (ms) | Check after 10 edits (ms) |
|---|---:|---:|---:|---:|---:|
| forms-engine | 668 | 2731 | 234 | 413 | 1073 |
| FormsSSOService | 779 | 2132 | 977 | 1045 | 1247 |
| HDFC_AssistedSavings_FORMS | 527 | 1888 | 236 | 307 | 260 |
| HDFC_ConsentManagementCommon_FORMS | 583 | 1851 | 228 | 249 | 173 |
| HDFC_CustomerInfo | 654 | 1685 | 189 | 262 | 219 |
| HDFC_ETB_CC_WO_FORMS | 1000 | 3511 | 357 | 588 | 755 |
| HDFC_ETBSavings_Forms | 474 | 1704 | 187 | 244 | 155 |
| HDFC_Forms_BaaS_Common | 258 | 560 | 89 | 119 | 111 |
| hdfc_forms_common_v2 | 686 | 1852 | 265 | 404 | 964 |
| HDFC_FormsCommon | 1875 | 3009 | 627 | 853 | 1322 |
| HDFC_FormsKYCService | 273 | 826 | 106 | 174 | 126 |
| HDFC_FormsPaymentService | 744 | 2023 | 246 | 345 | 262 |
| HDFC_FormsSecurity | 286 | 635 | 86 | 115 | 110 |
| HDFC_HAF | 564 | 2529 | 325 | 429 | 481 |
| HDFC_INSURANCE_FORMS | 156 | 349 | 50 | 64 | 155 |
| HDFC_SAVINGS_FORMS | 911 | 3621 | 264 | 383 | 425 |
| HDFC_Savings_UnifiedURL | 406 | 1043 | 155 | 227 | 162 |
| HDFC_SavingsCommon | 854 | 2099 | 270 | 371 | 343 |
| HDFC_VRMSavings_Forms | 1035 | 4631 | 422 | 658 | 1038 |
| HDFC_Xpressway | 952 | 2886 | 567 | 857 | 2416 |
| HDFCBankFormsBAASTestUtility | 128 | 903 | 166 | 202 | 168 |
| impact-analyser | 102 | 852 | 33 | 51 | 105 |
| **Total (sequential)** | 13915 | 43320 | 6079 | 8360 | 12070 |

Reproduce:

```powershell
$env:ATTIC_BENCH_WORKSPACE = "C:\repo1;C:\repo2"
$env:ATTIC_BENCH_OUT = "bench.md"
cargo test -p attic-incremental --release --test freshness_bench -- --ignored --nocapture
```

## Upgrading

The schema changed. On first start an index created by an older Attic is
deleted (catalog database and Attic-owned repository/commit stores only) and
every configured repository is re-indexed from source. Configuration files are
not touched.
