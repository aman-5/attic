# Phase 7 Completion Report — Attic Production Hardening

Date: 2026-08-28 · Branch: `feature-phase-7`

This report describes the **current, final state** of the functional-hardening
work only. It supersedes all prior drafts of this file — earlier intermediate
findings (e.g. an unfixed Resource Manager tier bug) are not repeated here
once corrected; see git history for the narrative if needed.

**Verdict: FUNCTIONAL CODE HARDENING COMPLETE. NOT production-validated.**
Scale/soak/stress/fault-injection/quality-regression work and full
multi-platform CI validation are explicitly deferred — see
`docs/FINAL_VALIDATION_TODO.md`. Do not treat anything marked NOT VERIFIED
below as passing.

## 1. Functional fixes (FIXED, locally verified)

| Area | Fix |
|---|---|
| Resource Manager pressure tiers | `MIN_FREE_MEMORY_MIB` default lowered 256→100 MiB; added `safe_min_free_mib` clamp and `ResourceConfig::validate()` (fail-closed on bad `ATTIC_*` overrides). All four tiers (Normal/Warning/Critical/Emergency) are now reachable. |
| Shutdown ordering | `SchedulerHandle::shutdown()` / `IncrementalWatch::stop()` existed but were never called — watcher/scheduler now stop, in order, before DB checkpoint/backup/writer close. |
| Ctrl+C shutdown race | Old `select!` could drop the live service task without awaiting it; now always fully awaited. |
| Semantic background enrichment wiring | `BackgroundEnricher` was built and tested but never spawned in production — `ATTIC_SEMANTIC=1` silently never produced embeddings. Now spawned at startup and stopped (bounded) at shutdown. |
| Semantic enrichment backpressure | The enricher ignored resource pressure entirely; now checks the same `current_advisory()` the scheduler uses and pauses under `Pause`/`Emergency`. |
| CI: Intel macOS test skip | `release.yml` unconditionally skipped `cargo test` on `x86_64-apple-darwin` despite `macos-13` being a native (non-cross-compiled) runner. Fixed to test all four targets. |
| Migration downgrade | An older binary opening a DB a newer binary had already migrated further would silently apply zero migrations and serve anyway. `run_migrations` now rejects any unrecognized `core_schema_migrations` entry (regression-tested). |
| Logging configuration | `RUST_LOG`/`ATTIC_LOG` had zero effect — the `env-filter` Cargo feature was enabled but never wired to the subscriber. Fixed; defaults to `info`. |
| Swallowed worker-thread panics | `WriterQueue::drop` and `SchedulerHandle::shutdown` discarded `JoinHandle::join()` panic errors silently; both now log them. |
| Multi-repository workflow documentation | README recommended running multiple `attic-server` processes against one shared `ATTIC_DB_PATH` — an architecture the writer/watcher/startup-recovery code does not support (single-process-per-database ownership throughout). Corrected to document the actually-implemented model: one process, one workspace root, cross-repo resolution via Git submodules within that root. Guarded by `crates/attic-server/tests/readme_workflow_claims.rs`. |
| License metadata inconsistency | `Cargo.toml` declares `license = "MIT OR Apache-2.0"` but no license text files existed anywhere in the repo — every release archive shipped unlicensed. Added `LICENSE-MIT` and `LICENSE-APACHE` (standard texts, `Attic Contributors` copyright) matching the already-declared SPDX expression; no license *choice* was made here, only the already-declared choice materialized as the standard required files. `tools/package.sh` already copies these when present — no packaging script change needed. |
| Debris / dead code | Removed tracked `_test.log`; `.gitignore` covers `*.log`/`build_errors*.txt`/stray `*.db*`; removed a genuinely-unused `WriterQueue::pending_count` field; fixed pre-existing clippy `-D warnings` violations in touched files. |
| Security review | No SQL injection (all dynamic `format!` SQL uses compile-time-literal identifiers only), no command injection (`Command::new` calls are argv-based git subprocess calls or test-only), path-traversal/symlink guards already tested, graph-explosion bounds enforced, MCP input bounds enforced. No defects found. |

## 2. Windows MSVC status — CORRECTED

**Windows x86_64 MSVC: locally build+test PASS this session.** Prior drafts
of this report claimed MSVC Build Tools were unavailable in this
environment; that was incorrect (or the environment changed). This pass
confirmed:

- `rustup show`: active toolchain is `1.98.0-x86_64-pc-windows-msvc`
  (MSVC, not GNU), per `rust-toolchain.toml`.
- `vswhere` finds MSVC Build Tools installed
  (`Microsoft Visual Studio\18\BuildTools`, VC.Tools.x86.x64 component).
- `cargo build -p attic-server --target x86_64-pc-windows-msvc` succeeds and
  produces a real linked binary (`target/x86_64-pc-windows-msvc/debug/attic.exe`).
- `cargo test --workspace --target x86_64-pc-windows-msvc` has been run
  repeatedly this session with **0 failures**, including the rmcp stdio
  integration tests that spawn and drive the actual compiled binary.

This is genuine **local** MSVC validation (build + full workspace test +
live binary exercise), on the official target. It is **not** a substitute
for an actual CI run on GitHub's `windows-latest` runner — that remains
`NOT VERIFIED` in `docs/FINAL_VALIDATION_TODO.md` (environment/runner
differences are still unverified). The official Windows target remains
`x86_64-pc-windows-msvc`; no GNU substitution was made or is proposed
anywhere in tracked configuration.

## 3. License status

**Release blocker, now resolved at the file level; ownership question
remains open.** `Cargo.toml` already declares `license = "MIT OR Apache-2.0"`
— an authoritative project-file decision — so the standard `LICENSE-MIT` and
`LICENSE-APACHE` texts were added to match it (copyright line:
`Attic Contributors`, matching `Cargo.toml` authorship). No new legal
decision was made by this pass. **Open question:** whether "Attic
Contributors" is the correct/final copyright holder (vs. a specific
individual or organization) has not been confirmed by any authoritative
source and should be verified by whoever owns the release before the first
public release.

## 4. Multi-repository workflow — CORRECTED

The README previously recommended running multiple `attic-server` processes
against a shared `ATTIC_DB_PATH` for cross-repo scenarios. Reviewing the
actual implementation (`crates/attic-storage/src/writer.rs`,
`crates/attic-incremental`, `run_startup_recovery`, `ops_server_state`
singleton invariant, ADR-006) shows this is **not** a supported or tested
architecture: writer ownership, watcher epoch tracking, and startup recovery
all assume exactly one process owns a given database file. The actually
implemented and tested multi-repository model is: **one `attic-server`
process, one `ATTIC_WORKSPACE_ROOT`**, whose repositories are linked as Git
submodules (each submodule becomes its own `core_repositories` entry per
ADR-006); cross-repo dependency resolution runs once at startup within that
single process. README corrected accordingly; a regression test
(`crates/attic-server/tests/readme_workflow_claims.rs`) guards against the
incorrect claim reappearing.

## 5. Release-artifact recheck

Static inspection after the above corrections:

- `tools/package.sh` / `.github/workflows/release.yml`: official Windows
  target is `x86_64-pc-windows-msvc` throughout; no GNU substitution.
  `release.yml` runs `cargo test` on all four matrix targets (fixed this
  pass — see §1).
- License files now exist and `tools/package.sh` already copies
  `LICENSE-MIT`/`LICENSE-APACHE` into every archive when present (existing
  conditional copy, unchanged) — archives will include them going forward.
  Not yet re-verified via an actual archive build this pass (deferred —
  see §6).
- Archive exclusion rules (`--verify`) already reject `target/`, `*.db*`,
  `*.log`, `build_errors*`, hidden files, `.attic/` — unchanged, still
  correct.
- No documentation of multi-process DB sharing remains in README.

## 6. Focused checks run this pass

- `cargo build -p attic-server --target x86_64-pc-windows-msvc` — PASS
  (real linked binary produced).
- `cargo test -p attic-storage --target x86_64-pc-windows-msvc` (migration
  downgrade regression test) — PASS.
- `cargo test --workspace --target x86_64-pc-windows-msvc` — PASS, 0
  failures (run multiple times across this and the prior pass).
- `cargo fmt --all` — clean.
- New `readme_workflow_claims` test — PASS.

**Not run this pass** (per explicit scope): full release archive build for
all four targets, `tools/package.sh --verify` end-to-end re-run, clippy
`-D warnings` full workspace sweep, any benchmark/soak/stress/fault
campaign.

## 7. PASS / FAIL / NOT VERIFIED

| Gate | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo test --workspace` (Windows, local) | PASS (0 failures) |
| Windows x86_64 MSVC — local build + test | PASS |
| Windows x86_64 MSVC — CI run | NOT VERIFIED |
| Linux x86_64 / macOS x86_64 / macOS ARM64 — CI run | NOT VERIFIED |
| License files present, matching declared SPDX expression | PASS |
| License copyright-holder correctness | NOT VERIFIED (open question, §3) |
| Multi-repo workflow docs match implementation | PASS (corrected + tested) |
| Release archive rebuild/re-verify after license-file addition | NOT VERIFIED (deferred) |
| Scale / soak / stress / fault-injection / quality-regression | NOT VERIFIED / NOT RUN |

## 8. Remaining genuine blockers

None found in code this pass. Two non-code items before a public release:

1. Confirm the license copyright-holder attribution (§3).
2. Run the deferred validation in `docs/FINAL_VALIDATION_TODO.md` before any
   production-readiness claim.

## 9. Production-readiness verdict

**NOT production-ready; functional hardening is complete.** All genuine
functional/architectural/security/configuration/packaging defects found
across both hardening passes have been fixed and locally verified
(`cargo fmt` + full `cargo test --workspace`, 0 failures, on the official
Windows MSVC target). Large-scale benchmarks, soak/stress, fault injection,
cross-platform CI runs, and quality-regression comparisons remain
`NOT VERIFIED` and are tracked in `docs/FINAL_VALIDATION_TODO.md`. Do not
ship as a finished product until those are completed.
