# ADR-008: Phase 2 Filesystem Watcher Dependency

**Status:** Accepted
**Date:** 2026-08-25
**Phase:** 2 (Incremental Correctness and Freshness)
**Supersedes:** none

## Context

Phase 2 introduces filesystem watching for the first time. Before adding a
watcher dependency the operating manual requires verification of the current
maintained version, official API, platform support, recursive-watch behavior,
rename semantics, backend limitations, and overflow/error behavior.

## Decision

Adopt **`notify-debouncer-full` version `0.7`** (which re-exports
`notify = 8.2.x`) as the only filesystem-watching dependency.

## Verification record (performed 2026-08-25)

| Item | Finding | Source |
|---|---|---|
| Stable version | `notify-debouncer-full` stable = **0.7.0** (2026-01-23); 0.8 is RC only | crates.io API |
| Core notify version | stable = **8.2.0**; 9.0.0 still at rc.4 — rejected (RC) | crates.io API |
| Dependency shape | debouncer-full 0.7.0 depends on `notify ^8.2.0`, non-optional | crates.io deps API |
| MSRV | 1.88 — matches workspace `rust-version = 1.88` exactly | GitHub README / docs.rs |
| Platforms | Windows (`ReadDirectoryChangesW`), macOS FSEvents, Linux inotify, kqueue, plus polling fallback | official README/docs.rs |
| Recursive watch | native on Windows/FSEvents; emulated by per-directory watches on inotify/kqueue | official docs "known problems" |
| Rename semantics | backends emit separate From/To events; debouncer stitches to a single rename when matchable, optionally via OS file IDs (Windows/FSEvents). Pairing is NOT guaranteed on every platform → treated as hint only | debouncer README feature list |
| Duplicate suppression | debouncer already merges duplicate creates/modifies and does not emit Modify after Create | debouncer README |
| Overflow / loss | backend errors are delivered through the callback as `DebounceEventResult::Err(Vec<notify::Error>)`; events may be lost silently by the OS on overflow → Attic MUST treat any error batch as potential event loss | docs.rs `DebounceEventResult` |

## Consequences

1. Watcher events are **hints**, never source truth; every debounced batch is
   verified against actual filesystem state (BLAKE3 content hash) before any
   canonical mutation.
2. Any `Err(_)` batch from the debouncer sets the workspace
   `reconciliation_required` flag and triggers a bounded authoritative rescan;
   affected state becomes UNKNOWN until reconciliation completes.
3. Debouncing uses the library's quiet-period debounce; coalescing, bounded
   queues, ChangeSet formation, and identity/rename heuristics remain Attic-side
   so they can be tested deterministically without a real watcher.
4. No other watcher dependency is added. The `crossbeam-channel` / `flume`
   features stay off (default std-channel callback is used).

## Alternatives considered

- Raw `notify` without debouncer: rejected — reimplementing platform-correct
  rename stitching and duplicate suppression invites defects.
- `hotwatch` / `watchexec` libraries: not maintained to the same standard /
  pull extra runtime dependencies.
- Polling only: cannot meet latency expectations; kept only as notify's own
  fallback backend.
