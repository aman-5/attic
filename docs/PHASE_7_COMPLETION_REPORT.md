# Phase 7 Completion Report — Attic Production Hardening

Date: 2026-08-28 · Branch: `feature-phase-7` · Verdict at end of document.

Attic is **not** declared production-ready by this report. Development
items 1–12 are complete and verified locally on Windows x86_64. The full
validation sequence (fmt/check/clippy/unit/integration tests) is complete
and green. The large-scale benchmark, soak/stress, and fault-injection work
items (13–17) were **not executed** in this pass — see §7 for why and what
is required before they can be claimed done.

## 1. Development items delivered

| # | Item | Status | Where |
|---|---|---|---|
| 1 | Release packaging architecture (4 targets, archive layout, exclusions) | DONE, bug-fixed | `tools/package.sh` |
| 2 | CI/release workflows building/testing all supported platforms | PRESENT, NOT VERIFIED (no CI run observed this session) | `.github/workflows/ci.yml`, `release.yml` |
| 3 | Production `[profile.release]` | DONE | root `Cargo.toml` (`opt-level=3, lto=thin, codegen-units=1, strip=symbols`) |
| 4 | README rewrite | DONE THIS SESSION | `README.md` — **the file did not exist on disk at session start despite being marked "DONE" in the prior version of this report; the prior Phase 7 commit deleted the stale README and never replaced it.** |
| 5 | Final config/data/cache/temp policy | DONE | `crates/attic-core/src/paths.rs` + README |
| 6 | Resource Manager: real process-RSS enforcement + admission + degradation | DONE, bug-fixed | `crates/attic-storage/src/resource_manager.rs` |
| 7 | CPU concurrency: separate foreground/background capacities | DONE | resource_manager.rs |
| 8 | Graceful shutdown | DONE | `crates/attic-server/src/main.rs` |
| 9 | SQLite maintenance | DONE, bug-fixed | `crates/attic-storage/src/connection.rs` |
| 10 | Developer debris removed from releases | DONE | `tools/package.sh --verify` |
| 11 | Clean end-user workflow documented | DONE THIS SESSION | `README.md` |
| 12 | No Rust/Cargo/MSVC/MinGW/Xcode/GCC/Node/Tree-sitter required for end users | DONE | rusqlite `bundled`, grammars compiled in |

## 2. Bugs found and fixed this session

These were found by actually running the test suite and the server binary
end-to-end, not by inspection alone. Each was a real functional defect, not
a test artifact:

1. **Server crashed on every clean startup.** `verify_connection()` ran
   `PRAGMA foreign_key_check` via `query_row`, which errors with "Query
   returned no rows" whenever there are zero violations — the normal,
   common case. This is why the 9 attic-server MCP integration tests were
   failing (they spawn the real binary, which exited immediately). Fixed by
   querying `pragma_foreign_key_check()` as a table-valued function and
   counting rows instead. **Impact: the packaged binary could not start a
   fresh database before this fix.**
2. **Backup retention was a no-op.** `backup_database()` collected the list
   of existing backups to enforce "keep last 3" (REC-B2) but never deleted
   anything past that point. Fixed to actually prune stale backups.
3. **Cross-repo / relationship evidence has never reached an MCP response.**
   `context::build()` drops any evidence item with an empty `snippet`
   field; neither `RelationshipGenerator` nor `CrossRepoGenerator` ever set
   `snippet`, so 100% of relationship-type evidence was silently discarded
   before serialization — regardless of confidence, resolution level, or
   `workspace_snapshot_id`. This was masked by a pre-existing unit test
   whose Gate 4b assertion was written as conditional ("if any snapshot
   evidence exists, it must look valid") rather than required ("snapshot
   evidence must exist"), so it always passed trivially. The stricter
   integration test (`rmcp_client_crossrepo_multi_repo_fixture`) caught it.
   Root-caused via targeted instrumentation of the retrieval pipeline
   (fusion → validation → context-assembly stages) against a live 3-repo Go
   fixture; fixed by setting `snippet` to the edge's human-readable
   description in both generators. **Impact: cross-repo dependency claims
   have never been visible to an MCP client, in any prior phase, despite
   Phase 6 claiming this was delivered.**
4. **Real-RSS sampling was dead for the first 250 ms of server life.**
   `refresh_process_memory()`'s sample-interval throttle compared "now" to
   "last sample time," both of which start near zero at construction, so
   the very first call was always skipped as "too soon" even though no
   sample had ever been taken. Fixed to always sample when no prior sample
   exists.
5. Two pre-existing unit tests in `resource_manager.rs` had incorrect test
   math (an inverted off-by-one in a pressure-threshold boundary, and an
   exact-equality assertion on two independently-sampled RSS values that
   could legitimately differ by rounding) — fixed the tests, not the
   product code, after confirming the product logic was correct.
6. `corruption_is_detected_by_verify_connection` used a hand-crafted
   garbage byte buffer that tripped SQLite's hard `NotADatabase` error
   before `integrity_check` ever ran, rather than the soft "corrupt but
   openable" case it was meant to exercise. Rewrote it to corrupt a real,
   populated database's data pages instead — a realistic on-disk corruption
   scenario — and to accept either an `Ok(violations)` or a corruption-shaped
   hard `Err`, since both are valid "unusable database" signals and callers
   fail closed on either.
7. **`tools/package.sh` could never have produced a working release
   archive.** It built the `attic-server` Cargo *package*, but that
   package's `[[bin]]` target is named `attic`, so cargo emits
   `target/<triple>/release/attic[.exe]` — a file the script never looked
   for (it looked for `attic-server[.exe]`, which cargo never produces).
   Every build invocation would have failed at the "binary not found"
   check. Fixed to build the real artifact name and stage it under the
   user-facing `attic-server` name.
8. **The archive verifier's binary-count check is broken on Windows.**
   `[[ -f "$DIR/attic-server" ]]` returns **true** on Windows/MSYS bash
   (the exact shell GitHub's `windows-latest` runner uses for `run: bash
   ...` steps) even when only `attic-server.exe` exists, because MSYS's
   `stat` resolves bare names through `.exe`-suffix matching. This caused
   the verifier to always report "2 binaries found" for a correctly-staged
   Windows archive and fail every legitimate build. Confirmed by
   reproducing it live (staged a real release binary, watched `--verify`
   fail, traced it with `bash -x` to the exact `[[ -f ]]` line). Fixed by
   listing real directory entries via `find` instead of stat-testing
   hypothesized names.

Also fixed: two trivial pre-existing unused-import warnings and ~10
clippy style warnings (`needless_borrow`, `useless_borrows_in_formatting`,
`redundant_field_names`) via `cargo clippy --fix`, all in test/bench code.

## 3. Known limitation surfaced, not fixed

`ResourceMonitor::compute_pressure` checks Emergency (`free < min_free`)
before Critical (`pct >= 85%`). With the production defaults (1024 MiB
total budget, 256 MiB min-free = 25% of budget), the Emergency floor
(>75% used) falls *below* the Critical floor (≥85% used) — so **Critical
pressure is unreachable with default configuration**: any usage level that
would read 85%+ has already tripped Emergency. This is a real design
inconsistency in the resource-pressure tiers, not merely a test artifact
(confirmed by deriving it from the actual constants). It does not violate
safety (Emergency is the stricter response), but the four-tier model as
documented doesn't behave as documented under defaults. Left unfixed
pending a product decision on whether to lower `MIN_FREE_MEMORY_MIB`,
raise `TOTAL_MEMORY_BUDGET_MIB`, or accept that Critical is a rare
low-total-memory-budget-only state.

## 4. Validation sequence — actual results (Windows x86_64, this machine)

Commands run, in order, this session:

```
cargo fmt --check                          → clean (after `cargo fmt`)
cargo clippy --workspace --all-targets     → 0 errors, ~15 pre-existing style warnings (test/bench code only)
cargo test --workspace                     → 0 failures across every crate (unit + integration)
```

Per-crate test counts (this run): attic-core 45, attic-storage 80,
attic-retrieval 145, attic-indexing 60, attic-incremental (multiple
integration files) 58+16+19+..., attic-crossrepo 58+3(e2e), attic-server
58 (unit) + 3 (rmcp stdio integration), attic-semantic, attic-analyzers,
attic-discovery — full breakdown is reproducible via
`cargo test --workspace -- --nocapture`.

Packaging verification: built a real release binary
(`cargo build --release --package attic-server --target
x86_64-pc-windows-gnu`), staged it by hand into the documented archive
layout, and confirmed `tools/package.sh --verify` correctly **fails** on a
missing README and correctly **passes** on a complete archive (both
outcomes observed directly, not assumed).

MCP protocol: manually drove the real `attic.exe` binary over stdio
(`initialize` → `notifications/initialized` → `tools/call`) against a live
3-repository Go fixture (provider/dependent/unrelated modules) to
root-cause bug #3 above; confirmed cross-repo evidence now appears in the
`context` tool's response with a populated `workspace_snapshot_id` after
the fix.

## 5. Platform verification status

| Platform | Build | Tests | Verified by |
|---|---|---|---|
| Windows x86_64 | PASS (gnu toolchain, this machine) | PASS (full workspace) | Local, this session |
| Linux x86_64 | NOT VERIFIED | NOT VERIFIED | Requires CI run — not observed this session |
| macOS x86_64 | NOT VERIFIED | NOT VERIFIED | Requires CI run — not observed this session |
| macOS ARM64 | NOT VERIFIED | NOT VERIFIED | Requires CI run — not observed this session |

**Per policy: platforms not verified locally or by an observed CI run are
NOT claimed verified.** The Windows build in this report used the
`x86_64-pc-windows-gnu` target (this machine's configured toolchain, via
`~/.cargo/config.toml`); the officially supported/packaged Windows target
is `x86_64-pc-windows-msvc`, which was not built or tested this session
(no MSVC Build Tools available in this environment). This is a real
verification gap, not a formality — the MSVC and GNU builds are not
guaranteed identical.

## 6. Items 13–17: scale benchmark, soak/stress, fault injection, quality regression

**Not executed this session.** These require dedicated, long-running
resource-intensive test runs (25–30 synthetic repositories at ~500K
retrieval units, repeated-edit soak loops, concurrent-query stress,
deliberate storage/permission/watcher/parser/semantic-provider failure
injection). Given this session's scope was consumed by finding and fixing
five genuine functional bugs blocking basic correctness (server startup,
backup retention, cross-repo evidence delivery, resource sampling, and the
packaging pipeline itself), running an honest large-scale benchmark was
not attempted rather than produce estimated or partial numbers under time
pressure.

**Explicitly NOT claimed**: no cold/incremental indexing latency, peak
memory, CPU, disk usage, SQLite/FTS size, semantic overhead, soak-stability,
or fault-injection numbers exist for this report. Do not treat their
absence as passing — treat them as **NOT VERIFIED**, required before a
production-readiness claim can be made.

Existing automated coverage that partially substitutes for dedicated
soak/fault runs (already exercised as part of §4's `cargo test --workspace`,
but not a substitute for the dedicated large-scale runs above):
- `crates/attic-incremental/tests/phase2_recovery.rs` — crash/restart
  recovery, abandoned-run detection, watcher-epoch bump.
- `crates/attic-incremental/tests/phase2_runtime_fixes.rs`,
  `phase2_policy_freshness.rs` — scheduler runtime behavior under policy
  constraints.
- `crates/attic-storage/src/connection.rs` tests — corruption detection,
  WAL checkpoint/truncate, backup creation+retention.
- `crates/attic-retrieval/tests/phase5_semantic_benchmark.rs`,
  `phase3_benchmark.rs` — exist but were not run as dedicated benchmarks
  this session (only as part of the pass/fail unit-test run, without
  capturing/recording their timing output).

## 7. What is required before Phase 7 can be marked complete

1. Run the actual 25–30 repo / ~500K unit scale benchmark and record real
   numbers (§13–14 of the original task spec).
2. Run soak/stress and fault-injection passes (§15–16).
3. Run the Phase 4/6 quality-regression benchmark and compare against prior
   phase baselines (§17).
4. Get an actual CI run (or MSVC local build) for Windows x86_64-msvc,
   Linux, and both macOS targets before claiming them verified.
5. Resolve or explicitly accept the Critical-pressure-unreachable finding
   in §3.

## 8. PASS/FAIL/NOT VERIFIED gates

| Gate | Result |
|---|---|
| `cargo fmt --check` | PASS |
| `cargo clippy --workspace --all-targets` (0 errors) | PASS |
| `cargo test --workspace` (unit + integration) | PASS (0 failures) |
| MCP server manual protocol verification | PASS |
| Packaging build + archive-layout verification | PASS (after 2 bugs fixed) |
| Windows x86_64 (MSVC, the officially packaged target) | NOT VERIFIED |
| Linux x86_64 / macOS x86_64 / macOS ARM64 | NOT VERIFIED |
| Scale benchmark (25–30 repos / ~500K units) | NOT VERIFIED / NOT RUN |
| Soak / stress testing | NOT VERIFIED / NOT RUN |
| Fault injection | NOT VERIFIED / NOT RUN |
| Phase 4/6 quality-regression benchmark | NOT VERIFIED / NOT RUN |

## 9. Remaining limitations

- Linux/macOS builds and the officially-packaged Windows MSVC target are
  unverified — no CI run and no local MSVC toolchain were available this
  session.
- Semantic layer remains experimental, opt-in, hashing-baseline embedder
  (ADR-013).
- Critical resource-pressure tier is unreachable under default
  configuration (§3) — Emergency preempts it; not a safety issue but a
  documented-behavior mismatch.
- No large-scale (25–30 repo) benchmark, soak, stress, or fault-injection
  results exist. This is the single largest gap before a production
  readiness claim can be made.

## 10. Production-readiness verdict

**NOT production-ready.** Phase 7 development is functionally complete and,
as of this session, actually works end-to-end on Windows (it did not
before this session's fixes — the server could not start a fresh database
and cross-repo evidence never reached a client). The full validation
sequence through unit/integration tests passes cleanly. However, the
large-scale benchmark, soak/stress, fault-injection, and quality-regression
work items required by the original Phase 7 spec were not executed, and
only one of four target platforms has been verified at all. Do not ship
this as a finished product until §7's remaining items are completed.
