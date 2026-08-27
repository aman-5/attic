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

## 9a. Functional-hardening follow-up pass (2026-08-28)

A second pass focused exclusively on genuine functional/architectural
defects (no benchmarks, no soak/stress). Deferred validation work is now
tracked in `docs/FINAL_VALIDATION_TODO.md`.

**Fixed:**

1. **Resource Manager pressure tiers** (§3 above, now resolved):
   `MIN_FREE_MEMORY_MIB` default lowered 256→100 MiB so the Emergency
   floor sits above the Critical threshold; added
   `resource_manager::safe_min_free_mib` (defensive clamp) and
   `ResourceConfig::validate()` (fail-closed rejection of inconsistent
   `ATTIC_*` overrides), wired into server startup. Regression-tested.
2. **Shutdown ordering — real bug, not cosmetic.** `SchedulerHandle::shutdown()`
   and `IncrementalWatch::stop()` existed but were never called anywhere;
   the watcher/scheduler locals lived in `main()`, separate from the
   `AtticServer` moved into `serve_until_closed`, so they were only
   dropped (a no-op — neither type has a `Drop` impl) after shutdown
   maintenance (WAL checkpoint/backup/writer close) had already run. Fixed
   by threading both handles into `serve_until_closed` and stopping them,
   in order, before DB maintenance.
3. **Ctrl+C shutdown race.** The old `tokio::select!` between
   `running.waiting()` (consumes `self`) and `ctrl_c()` dropped the
   in-flight `waiting()` future — and the `RunningService` it owned —
   without awaiting it whenever Ctrl+C won the race, detaching the actual
   service task while shutdown proceeded to checkpoint/close the DB
   concurrently. Fixed by spawning a small Ctrl+C watcher task that only
   cancels the token, then always fully awaiting `running.waiting()`.
4. **Semantic background enrichment was never wired into the server.**
   `BackgroundEnricher`/`enrich_to_completion` existed and were fully
   tested, but neither was ever invoked outside tests — when
   `ATTIC_SEMANTIC=1`, the enrichment queue was never drained in
   production, so semantic search would always fall back to
   `NoEmbeddings`. Fixed: `main()` now spawns `BackgroundEnricher` when
   the semantic layer opens, and `serve_until_closed` stops it (bounded
   timeout) before DB maintenance.
5. **CI silently skipped Intel macOS tests.** `release.yml` ran
   `cargo test --workspace` for every matrix target except
   `x86_64-apple-darwin`, despite `macos-13` being a native (not
   cross-compiled) runner for that target — an unexplained skip that
   would have let a real regression on Intel Mac ship unnoticed. Removed
   the exclusion.
6. **Dead-code / lint gate cleanup:** removed a genuinely-unused
   `pending_count` field from `WriterQueue`, added
   `#[allow(clippy::too_many_arguments)]` with rationale to a schema-shaped
   insert function, replaced two manual percentage divisions with
   `checked_div`, and fixed a handful of pre-existing `-D warnings`
   violations (unused lifetime, manual `strip_prefix`, `field_reassign_with_default`)
   encountered in files touched this pass.
7. **Debris:** removed tracked `_test.log`; `.gitignore` now covers
   `*.log`, `build_errors*.txt`, and stray `*.db*` files.

**Reviewed, no defect found (light-touch, not exhaustive):** SQLite
connection/writer/backup code, migration ordering, `.cargo/config.toml`
(confirmed no machine-specific/target-forcing config is tracked), release
profile, README platform claims, and a repo-wide `unwrap`/`expect`/`panic!`
grep (every hit outside test-support/bench code fell inside `#[cfg(test)]`
modules).

**Explicitly not re-audited this pass** (time-boxed): the full
resource/backpressure integration matrix (§5 of the task spec — indexing,
structural analysis, graph traversal, cross-repo resolution admission
paths were not individually traced this session beyond the semantic-worker
fix above); `ProductionConfig` in `attic-core` remains constructed nowhere
in the real binary (dead scaffolding — its 20-odd `ATTIC_*` env vars are
documented but inert; distinct from the live `ResourceConfig`/`ResourceMonitor`
path that IS wired in and was fixed above). Both are recorded as open items
for a follow-up pass rather than silently left unmentioned.

**Verification run this pass:** `cargo fmt --all` (clean), `cargo build
--workspace --all-targets` (clean), `cargo test --workspace` (100% pass,
every crate, 0 failures), `cargo clippy --workspace --all-targets`
(6 pre-existing style warnings remain, all in test/bench files, unrelated
to this pass's changes — `-D warnings` was not driven fully clean across
the whole workspace; the ones surfaced in files touched this pass were
fixed). No benchmark, soak, stress, or fault-injection work was performed,
per the pass's explicit scope — see `docs/FINAL_VALIDATION_TODO.md`.

## 9b. Second follow-up pass — remaining checklist items (2026-08-28)

8. **Migration downgrade was unguarded.** `run_migrations` only checked
   whether *its own* known versions were already applied — a database
   already advanced by a newer binary (unrecognized migration ids present)
   would silently run zero migrations and proceed to serve, rather than
   failing closed on a schema state this build can't reason about. Added an
   explicit check that rejects any unrecognized `core_schema_migrations`
   entry before applying anything. Regression-tested
   (`unrecognized_future_migration_is_rejected`).
9. **`RUST_LOG`/`ATTIC_LOG` had zero effect.** `tracing_subscriber`'s
   `env-filter` feature was enabled in `Cargo.toml` but never wired to the
   subscriber in `main()` — log verbosity was hardcoded regardless of
   environment. Fixed: `ATTIC_LOG` (checked first) or `RUST_LOG`, default
   `info`.
10. **Semantic background enrichment ignored resource pressure.** The
    `BackgroundEnricher` wired in during the first pass ran on a fixed
    cadence with no awareness of `ResourceMonitor` — it did not pause under
    `Pause`/`Emergency` advisories the way the incremental scheduler does,
    contradicting ADR-014's "semantic is the lowest-priority background
    subsystem" and this task's explicit backpressure-integration
    requirement. Fixed: the worker now checks `current_advisory()` before
    each drive cycle and backs off under pressure, mirroring the
    scheduler's policy. No second scheduler introduced.
11. **Silently swallowed worker-thread panics on shutdown.** Both
    `WriterQueue::drop` and `SchedulerHandle::shutdown` discarded
    `JoinHandle::join()`'s `Err` (thread panicked) with `let _ = ...`,
    meaning a worker crash during shutdown left no trace anywhere. Both now
    log an `error!`/`warn!` with the panic payload instead of discarding it.
12. **Reviewed and found no defects:** SQL string construction (the
    `format!("UPDATE {table} ...")` pattern in
    `attic-storage/src/invalidation_ops.rs` only ever receives compile-time
    string literals, never external input — not an injection vector despite
    the pattern looking risky at a glance); all `Command::new` call sites
    (git subprocess calls use argv arrays, no shell; the only other call
    sites are `#[cfg(test)]` helpers spawning the built test binary); path
    traversal / symlink escape guards (already tested:
    `path_escape_should_return_none`, `file_traversal_rejected`); graph
    traversal bounds (`MAX_GRAPH_DEPTH`/`MAX_GRAPH_NODES` enforced in
    `graph.rs`); the four production (non-test) `let _ = x;` sites in the
    JS/TS structural analyzers are unused-parameter suppressions, not
    swallowed `Result`s; README/`paths.rs` runtime-data-layout docs are
    already internally consistent.
13. **Known gap, not fixed (out of code-hardening scope):** no
    `LICENSE-MIT`/`LICENSE-APACHE` files exist in the repo root, so every
    release archive `tools/package.sh` produces ships with zero license
    text (the copy step is a silent no-op when the files are absent).
    Choosing/drafting a license is a legal/business decision, not a code
    defect — flagged here rather than fixed.

All 15 tracked functional-hardening tasks are now complete. Full workspace
verification after this pass: `cargo fmt --all` (clean), `cargo test
--workspace` (0 failures, every crate).

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
