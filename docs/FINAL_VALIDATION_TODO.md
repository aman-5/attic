# Final Validation TODO — Deferred From Phase 7 Functional-Hardening Pass

Status of every item below is **NOT VERIFIED** unless explicitly marked
otherwise. Deferral is not a pass. This document is the authoritative list
of what must be executed, with what workload, and against what acceptance
criteria, before Attic can be called production-ready.

## Platform CI validation

- [ ] Windows x86_64 MSVC full validation
  - Purpose: confirm the officially packaged Windows target builds and
    passes the full test suite (not the GNU target used for ad-hoc local
    dev on this machine).
  - Command: `.github/workflows/ci.yml` (`windows-latest`, default MSVC
    host) and `.github/workflows/release.yml` (`x86_64-pc-windows-msvc`).
  - Acceptance: `cargo fmt --check`, `cargo clippy -D warnings`,
    `cargo test --workspace` all green on an actual CI run (or local MSVC
    Build Tools install) — an observed run, not an assumption.
  - Evidence location: link the CI run URL here once executed.

- [ ] Linux x86_64 CI validation
  - Command/harness: `.github/workflows/ci.yml` (`ubuntu-latest`).
  - Acceptance: same as above.

- [ ] macOS x86_64 (Intel) CI validation
  - Command/harness: `.github/workflows/release.yml` (`macos-13` runner,
    `x86_64-apple-darwin`). This pass fixed a bug where this target's test
    step was unconditionally skipped (`if: matrix.target !=
    'x86_64-apple-darwin'`) — confirm the corrected workflow actually runs
    `cargo test --workspace` green here, not just that it builds.
  - Acceptance: full workspace test suite green, observed via an actual
    CI run.

- [ ] macOS ARM64 (Apple Silicon) CI validation
  - Command/harness: `.github/workflows/ci.yml` and `release.yml`
    (`macos-latest`).
  - Acceptance: same as above.

## Legal

- [ ] License copyright-holder confirmation — `LICENSE-MIT`/`LICENSE-APACHE`
  currently attribute copyright to "Attic Contributors", matching
  `Cargo.toml`'s `license = "MIT OR Apache-2.0"` declaration. No new legal
  decision was made when these files were added; whether "Attic
  Contributors" is the correct/final copyright holder (vs. a specific
  individual or organization) has not been confirmed by any authoritative
  source and should be verified by whoever owns the release before the
  first public release.

## Packaging / release archive

- [ ] Clean binary installation test
  - Prerequisite: a release archive built by CI (not hand-staged).
  - Workload: on a machine with no Rust/Cargo/MSVC/MinGW/Xcode/GCC/Node
    installed, extract the archive and run `attic-server` against
    `ATTIC_WORKSPACE_ROOT` pointing at a real repo; drive one `initialize`
    → `tools/call(search)` round-trip over stdio.
  - Acceptance: server starts, indexes, answers a query, shuts down
    cleanly (clean-shutdown marker recorded) without any toolchain present.

- [ ] Release archive inspection
  - Command: `tools/package.sh --target <triple> --out dist` then
    `tools/package.sh --verify dist/<archive-dir>` for each of the 4
    targets, using the REAL CI-built binary (this pass only re-verified
    the verifier's pass/fail logic locally with a hand-staged Windows
    binary).
  - Acceptance: exactly one binary, README, license files, `docs/`;
    no dev debris (`build_errors*.txt`, `*.log`, `target/`).

## Scale benchmarks

- [ ] 25–30 repository scale test
  - Workload: synthetic multi-repo workspace, 25–30 repositories, mixed
    languages, realistic size distribution.
  - Metrics: cold index time, incremental index time, peak RSS, CPU,
    on-disk DB size.
  - Acceptance criteria: not yet defined numerically — record baseline
    numbers first, then set regression thresholds.

- [ ] ~500K retrieval-unit scale test
  - Workload: workspace sized so `core_retrieval_units` reaches ~500,000
    rows.
  - Metrics: FAST/NORMAL/DEEP query latency at that scale, FTS index size,
    memory under sustained query load.
  - Acceptance: query latency stays within the answer-mode budgets defined
    in `docs/contracts/answer_modes.md`.

## Indexing / startup benchmarks

- [ ] Cold indexing benchmark — time-to-first-CURRENT on an empty DB.
- [ ] Incremental indexing benchmark — time to reconcile N changed files
      post-bootstrap.
- [ ] Startup/recovery benchmark — time from process start to "ready to
      serve" after (a) clean shutdown and (b) crash recovery
      (`run_startup_recovery` path).

## Latency / resource benchmarks

- [ ] FAST mode latency (p50/p95) under realistic corpus size.
- [ ] NORMAL mode latency (p50/p95).
- [ ] DEEP mode latency (p50/p95), including semantic path when
      `ATTIC_SEMANTIC=1`.
- [ ] Peak RAM under sustained foreground + background load — measured via
      `ResourceMonitor::process_rss_mib()` / `peak_memory_used_mib()`
      cross-checked against OS-level measurement (Task Manager / `ps`),
      not accounted-only numbers.
- [ ] CPU utilization under sustained load (foreground + background
      concurrently, verifying the foreground/background slot split from
      §4/§5 holds under real contention).
- [ ] Runtime disk usage (DB + WAL + backups + semantic.db + cache) for a
      representative workspace.
- [ ] SQLite/FTS size growth curve vs. indexed unit count.

## Soak / stress

- [ ] Prolonged MCP soak test — sustained query traffic over hours,
      watching for FD/memory leaks (`ResourceMonitor` peak drift over
      time), backup retention correctness (REC-B2, "keep last 3") under
      real elapsed time rather than a single test run.
- [ ] Repeated-edit stress test — rapid save/edit loop on a watched
      repository, confirming the watcher debounce (500ms default) and
      scheduler dedup (`ops_tasks` dedup_key) hold up without unbounded
      queue growth.
- [ ] Watcher storm test — large batch of simultaneous file changes (e.g.
      branch switch, `git clean`), confirming `ATTIC_INCREMENTAL_TASK_QUEUE_CAPACITY`
      backpressure and fallback-to-periodic-reconciliation behavior.
- [ ] Concurrent-query stress test — foreground slot exhaustion behavior
      under real concurrent MCP clients (this pass only unit-tested slot
      accounting with synthetic threads, not real concurrent MCP traffic).
- [ ] Restart/recovery soak test — repeated kill -9 / process-restart
      cycles, confirming `run_startup_recovery` always converges and no
      generation is ever left in an ambiguous CURRENT state.

## Fault injection

- [ ] SQLite/storage fault injection — simulate disk-full, permission
      revocation mid-write, and corrupted WAL; confirm fail-closed behavior
      (no silently-served stale/corrupt CURRENT data).
- [ ] Permission failure — repository root becomes unreadable mid-run.
- [ ] Watcher failure — OS watch handle drops/errors; confirm fallback to
      `FallbackGuard` periodic reconciliation actually engages and is
      reported via `status`.
- [ ] Parser failure — malformed/adversarial source input; confirm
      GenericAnalyzer fallback + diagnostic, never a panic.
- [ ] Semantic-provider failure — provider returns errors/timeouts mid
      `drive()`; confirm bounded retry (`max_attempts`), quarantine, and
      that canonical retrieval is never blocked (ADR-014 D1).
- [ ] Corrupted operational state — hand-corrupt `ops_tasks`/
      `ops_server_state` rows; confirm startup recovery fails closed
      rather than guessing.
- [ ] Process termination/restart — SIGKILL during active publication;
      confirm the next startup's recovery pass resolves it deterministically
      (no partial publication ever exposed as CURRENT).
- [ ] Migration failure — inject a failing migration mid-sequence; confirm
      the DB is left in a state `run_migrations` refuses to serve from
      rather than continuing degraded.

## Quality regression benchmarks

- [ ] Final Phase 4 retrieval-quality regression benchmark — compare
      against the Phase 4 baseline in `benchmarks/`.
- [ ] Final Phase 6 cross-repository quality benchmark — compare against
      the Phase 6 baseline, specifically re-verifying that cross-repo
      evidence now reaches the MCP `context` response (fixed this cycle
      per the Phase 7 hardening pass, bug #3) at benchmark scale,
      not just in the 3-repo fixture used to root-cause it.
- [ ] Unsupported-claim regression — verify the evidence-sufficiency gate
      still rejects claims without adequate grounding at scale.
- [ ] Exact lookup regression — symbol/path exact-match precision at
      benchmark scale.
- [ ] Symbol lookup regression — cross-language symbol resolution
      precision/recall at benchmark scale.
- [ ] Configuration lookup regression — config-file evidence retrieval
      accuracy at benchmark scale.

## Follow-up scale-tuning items (transferred from OPEN_QUESTIONS.md)

- [ ] Mode-scaled proactive verification breadth — NORMAL/DEEP currently
  proactively checksum a fixed top-5 ranked evidence items (ADR-012 D4).
  Whether this breadth should scale with mode depth or corpus size (without
  violating filesystem-scan budgets) needs measurement on a larger corpus
  than the current fixtures provide.
- [ ] Brute-force kNN scale ceiling — semantic kNN (when `ATTIC_SEMANTIC=1`)
  is currently a bounded brute-force scan over `semantic.db` (ADR-014 D4);
  measured sub-millisecond at fixture scale. Revisit an in-DB vector index
  or external store only if/when selected-unit counts reach ~10^5 on real
  workspaces AND query-time kNN latency is measured to exceed the NORMAL
  semantic time budget (`docs/contracts/answer_modes.md`).

## Open items surfaced but not independently re-benchmarked this pass

- Resource Manager pressure tiers were fixed and unit-tested
  (`crates/attic-storage/src/resource_manager.rs`), but the fix has not
  been exercised under a real sustained-load benchmark — only synthetic
  unit-level threshold math. The soak/stress items above cover this.
- The semantic background enrichment worker was wired into the server for
  the first time this pass (previously constructed but never spawned in
  production — see completion report). It has unit/integration test
  coverage but no production-scale soak run; covered by the soak and
  scale items above.
