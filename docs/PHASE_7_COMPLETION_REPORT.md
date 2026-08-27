# Phase 7 Completion Report — Attic Production Hardening

Date: 2026-08-27 · Branch: `feature-phase-7` · Verdict at end of document.

## 1. Development items delivered

| # | Item | Status | Where |
|---|---|---|---|
| 1 | Release packaging architecture (4 targets, archive layout, exclusions) | DONE | `tools/package.sh` |
| 2 | CI/release workflows building/testing all supported platforms | DONE | `.github/workflows/ci.yml`, `release.yml` |
| 3 | Production `[profile.release]` (measured, conservative) | DONE | root `Cargo.toml` |
| 4 | README rewrite (product, binary install, MCP config, troubleshooting, uninstall, build-from-source) | DONE | `README.md` |
| 5 | Final config/data/cache/temp policy (workspace vs user-global, documented) | DONE | `crates/attic-core/src/paths.rs` + README |
| 6 | Resource Manager: real process-RSS enforcement + admission + degradation | DONE | `crates/attic-storage/src/resource_manager.rs` |
| 7 | CPU concurrency: separate foreground/background capacities | DONE | resource_manager.rs (derived from production config) |
| 8 | Graceful shutdown: Ctrl+C/SIGINT, WAL checkpoint, backup, ordered teardown | DONE | `crates/attic-server/src/main.rs` (`serve_until_closed`) |
| 9 | SQLite maintenance: `checkpoint_wal(TRUNCATE)`, `run_maintenance`, corruption/backup tests | DONE | `crates/attic-storage/src/connection.rs` |
| 10 | Developer debris removed (`build_errors*.txt`, `__rsfiles.txt`); archive verifier blocks dev artifacts | DONE | repo root + `tools/package.sh --verify` |
| 11 | Clean end-user workflow (download binary → configure workspace → configure MCP → run) | DONE | README §Install/§Configure |
| 12 | End users need no Rust/Cargo/MSVC/MinGW/Xcode/GCC/Node/Tree-sitter | DONE | binaries bundle SQLite (rusqlite `bundled`); grammars compile into the binary |

## 2. Resource enforcement model (Phase 7 §6–7)

- **Real memory**: `ResourceMonitor::refresh_process_memory()` samples actual process RSS via `sysinfo` (rate-limited to 250 ms) before every admission decision. Effective memory = max(worker accounting, real RSS). Pressure/advisory decisions are therefore driven by genuine process memory, not manually incremented counters.
- **Foreground admission**: every MCP tool call must acquire a foreground slot (capacity from `ATTIC_MAX_FOREGROUND_QUERIES`, default 64). At capacity the caller receives an explicit `server busy` error. Foreground priority is preserved: slots are never consumed by background work.
- **Background admission**: incremental scheduler workers must acquire a background slot (capacity = `ATTIC_MAX_BACKGROUND_WORKERS`, default 12 = 8 indexing + 4 semantic, always clamped strictly below foreground capacity). Under `Pause`/`Emergency` advisories background slots are refused — verified by test.
- **Degradation**: `context` DEEP mode degrades to NORMAL under Pause/Emergency advisories (foreground still served). Task scheduling gate under Critical/Emergency pressure retained (priority < 70 deferred).
- **Config**: `ResourceConfig::load()` + `apply_to()` now genuinely reconfigures the live monitor (previously logged only).

## 3. Graceful shutdown (Phase 7 §8)

Ordered teardown on transport close **or** Ctrl+C/SIGINT (first-class `tokio::signal::ctrl_c` + rmcp cancellation token): stop MCP work → scheduler/watcher drop (in-flight tasks finish) → clean-shutdown marker → explicit `PRAGMA wal_checkpoint(TRUNCATE)` → crash-recovery backup (atomic rename, 3 retained) → WriterQueue drain+join → DB close.

## 4. SQLite production behavior (Phase 7 §9)

- Startup: `integrity_check(100)` + `foreign_key_check`, fail-closed on corruption (pre-existing, verified by new garbage-DB test).
- Shutdown/maintenance: explicit TRUNCATE checkpoint (WAL emptied — asserted in test), VACUUM-capable `run_maintenance`, backup creation + retention tests.
- Migrations: transactional (BEGIN IMMEDIATE/ROLLBACK), idempotent, 5 applied — verified by existing migration tests.

## 5. Platform verification status

| Platform | Build | Tests | Verified by |
|---|---|---|---|
| Windows x86_64 (MSVC) | LOCAL: PASS | LOCAL (see §6) | local run on win32 10.0.26200 |
| Linux x86_64 (gnu) | NOT VERIFIED locally | — | CI `release.yml` (runs on push tag / dispatch) |
| macOS x86_64 | NOT VERIFIED locally | — | CI |
| macOS ARM64 | NOT VERIFIED locally | — | CI |

**Per policy: platforms not verified locally are NOT claimed verified until CI passes on them.**

## 6. Local validation results (Windows x86_64, this machine)

Commands: `cargo fmt --all` · `cargo clippy --workspace --all-targets` (0 errors) · `cargo test --workspace`

<!-- TEST_RESULTS -->

## 7. Scale benchmark (actual tested scale)

<!-- BENCH_RESULTS -->

Target was 25–30 repositories / ~500K retrieval units. Actuals recorded below; no extrapolation.

## 8. Soak / stress / fault injection

<!-- SOAK_RESULTS -->

## 9. Quality benchmark (Phase 4/6 regression gate)

<!-- QUALITY_RESULTS -->

## 10. PASS/FAIL/NOT VERIFIED gates

<!-- GATES -->

## 11. Remaining limitations

- Linux/macOS builds pending CI verification (no local cross-toolchain assumed).
- Semantic layer remains experimental, opt-in, hashing-baseline embedder (ADR-013).
- Release binaries produced by CI on tag push; local `tools/package.sh` verified layout rules only on Windows.
- Scale/soak benchmarks bounded by local machine resources; actuals recorded, not extrapolated.

## 12. Production-readiness verdict

<!-- VERDICT -->
