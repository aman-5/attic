# 📘 Attic — Operations & Development Playbook

Practical manual for running, troubleshooting, recovering and extending Attic.
For what Attic is, start with the [README](../README.md); for how it is built,
[`ARCHITECTURE.md`](ARCHITECTURE.md).

## Contents

- [Operation](#operation)
  - [Start and stop](#start-and-stop) · [Indexing lifecycle](#indexing-lifecycle)
  - [Checking health](#checking-health) · [Managing repositories](#managing-repositories)
  - [Several windows, one workspace](#several-windows-one-workspace)
  - [Project knowledge](#project-knowledge) · [Semantic search on/off](#semantic-search-onoff)
- [Troubleshooting](#troubleshooting)
- [Recovery](#recovery)
- [Adding a language or platform](#adding-a-language-or-platform)
- [Development](#development)
- [Maintenance](#maintenance)

## Operation

### Start and stop

Your MCP client starts Attic for you. To run it by hand:

```sh
ATTIC_WORKSPACE_ROOT=/path/to/repo target/release/attic   # target\release\attic.exe on Windows
```

With no workspace configured anywhere, Attic starts **unconfigured**: it
answers `status` and `workspace`, and query tools return a clear "workspace
not configured" error until you add a repository.

Attic stops on Ctrl+C, or — as a daemon — once no client has been connected
for the idle timeout (90 s by default). Both paths run the same graceful
shutdown: stop accepting work, stop watchers and the scheduler, stop semantic
workers, record a clean-shutdown marker, prune old records, checkpoint the
WAL, write a crash-recovery backup and close the databases.

### Indexing lifecycle

```mermaid
flowchart LR
    S[Startup] --> R[Recovery<br/>fail-closed]
    R --> B[Full index pass<br/>per configured root]
    B --> X[Cross-repo sync]
    X --> W[Watchers + scheduler]
    W -->|file saved| I[Verify → invalidate<br/>→ re-index changed files]
    I --> W
```

- **Startup** — `run_startup_recovery` runs before anything is served:
  interrupted tasks return to `PENDING`, interrupted refreshes return to
  `STALE`, in-progress secret scans restart, and the database integrity check
  must pass.
- **Full index pass** — runs in the background right after startup for
  every configured root (tools answer immediately; `status` shows each
  repository's state). It is authoritative: paths deleted or newly excluded
  while Attic was stopped are tombstoned, and unchanged files are reproduced
  from the analysis cache instead of being re-analyzed.
- **Incremental** — a native file watcher (falling back to periodic
  reconciliation if the OS watch cannot be established) debounces changes
  for 500 ms, verifies them against real content hashes, invalidates the
  affected artifacts and re-indexes only the changed files. `status`'s
  `watcher.mode` says which mechanism is active.

### Checking health

Ask your client for Attic's status, or call the `status` tool. Key fields:

| Field | Meaning |
|---|---|
| `status` | `ok`, or `unconfigured` until a repository is configured |
| `workspace.repositories[]` | Per-repository state, watcher mode and any error |
| `incremental.state` | `CURRENT` / `INDEXING` / `RECONCILIATION_REQUIRED` / `UNKNOWN` |
| `incremental.tasks` | Pending/running background tasks |
| `semantic_progress` | Embedding queue depth and completion |
| `diagnostics.why_slow` | Plain-language bottleneck diagnosis |
| `resource_pressure` | Memory tier (`normal`/`warning`/`critical`/`emergency`) and effective limits |
| `resource_mode` / `resource_mode_source` | Selected low/balanced/performance tier and where it came from |

### Managing repositories

Membership is managed live through the `workspace` tool — no restart, no file
editing. Tell your client *"add D:\new-service to Attic"*, or call it directly:

| Action | Effect |
|---|---|
| `{"action":"inspect"}` | Configured and active roots, per-repository state |
| `{"action":"add","path":"..."}` | Validate, index, watch and persist a new root |
| `{"action":"remove","path":"..."}` | Stop watching; the repository immediately disappears from search, context and status |
| `{"action":"set","paths":[...]}` | Replace the whole membership |

Membership is written atomically to `<ATTIC_HOME>/config.toml`. A removed
repository's old index rows stay in storage until pruned but can never leak
into results. A configured root that is temporarily unavailable (say, an
unmounted drive) is reported under `workspace.unavailable_repositories` while
the others keep working.

### Several windows, one workspace

Any number of clients may launch Attic against the same `ATTIC_HOME`. The
first launch becomes the **daemon** (single writer, watchers, recovery); later
launches become **relays** over a local socket (Unix domain socket or Windows
named pipe). If the daemon exits or crashes, a relay re-elects itself as the
daemon without disconnecting its own client. Different `ATTIC_HOME`s are fully
independent workspaces.

### Project knowledge

Create `knowledge/*.md` files in a repository for durable facts that the code
does not show: architecture rationale, domain vocabulary, conventions,
ownership, deployment topology. See [`knowledge/README.md`](../knowledge/README.md)
for the template.

- Keep one topic per file; edit it like any other file — the watcher
  re-indexes it.
- A contradiction between knowledge and current source is **surfaced**, never
  silently resolved — treat it as a sign the knowledge file needs updating.
- Never store secrets there: knowledge files are served like source code.

### Semantic search on/off

Semantic search is **on by default**. Turn it off with `ATTIC_SEMANTIC=0` or
`[semantic] enabled = false`; delete `semantic.db` to reclaim the disk.
Canonical (lexical/structural) retrieval never depends on it. Embedding
workers scale with the resource mode (1 / 3 / 8 for low / balanced /
performance). See [`PERFORMANCE.md`](PERFORMANCE.md) for expected throughput.

### Attic home layout

`ATTIC_HOME` defaults to `~/.attic`. Startup creates the home directory, the
main `attic.db*` files, and `attic.toml` when missing. Other directories are
lazy: `models/` is created only for model downloads (or an explicit
`ATTIC_MODEL_CACHE_DIR` elsewhere), `logs/` only after the `logging` tool is
turned on, and `backups/` only after the shutdown backup first succeeds.
Attic does not read from or write to `~/.cache/huggingface`; model assets live
under the Attic model cache.

## Troubleshooting

| Problem | What to check |
|---|---|
| MCP won't connect | Absolute `command` path; errors are on **stderr** — stdout carries only MCP |
| Exits immediately | stderr: invalid `attic.toml` or `ATTIC_*` value (both fail closed), unreadable root, failed integrity check |
| Repository missing | `workspace {"action":"inspect"}`; `status.workspace.unavailable_repositories` |
| File missing | `.gitignore`, built-in skipped folders, `[indexing] exclude` |
| Results stale | `status.incremental.state`; the `file` tool reads live and appends an `[index freshness: …]` note on drift |
| Indexing seems stuck | `status.incremental.tasks` not decreasing across calls; stderr scheduler errors |
| Watcher degraded | `status.watcher.mode` = `periodic-reconciliation` — a documented fallback, not a crash |
| Cross-repo answers withheld | Startup cross-repo sync not finished or failed (stderr `cross-repo workspace sync failed`); single-repo retrieval unaffected |
| No semantic results | `status.semantic_progress`; stderr `semantic layer unavailable` means lexical-only by design |
| "server busy" / memory | `status.resource_pressure`; raise `total_memory_budget_mib` / `max_foreground_queries`, or index fewer repositories at once |
| Disk usage | `attic.db*`, `semantic.db`, lazy `models/` / `backups/` under `ATTIC_HOME` — not Cargo's `target/` or `~/.cache/huggingface` |

<details>
<summary><strong>Details</strong></summary>

- **Relay says the daemon never published an address.** Another process holds
  `attic.lock` but has no `attic.ipc`: it is still starting, serving a single
  client because its socket could not be created, or hung. Stop it and
  relaunch.
- **"workspace not configured"** is the intended first-run state — configure
  through the `workspace` tool, `ATTIC_CONFIG` or `ATTIC_WORKSPACE_ROOT`.
- **Disk over time.** Every clean shutdown (including the daemon's idle exit)
  prunes tombstones, invalidation records and finished tasks past their
  retention (90 / 90 / 30 days) and vacuums both databases, so size tracks
  indexed content rather than growing without bound.

</details>

## Recovery

| Situation | What to do |
|---|---|
| Killed mid-index | Nothing — startup recovery resumes; partial publications are never visible |
| Start over | Stop Attic, delete `attic.db`, `attic.db-wal`, `attic.db-shm`; restart. Always safe: everything is rebuilt from source |
| Integrity check fails | Attic refuses to serve. Move `attic.db*` aside, then copy the newest file from `backups/` to `attic.db`, or start over |
| Rebuild embeddings | Delete `semantic.db` and restart |
| "schema version not supported" | An older binary opened a newer database: upgrade the binary, or start over |

Attic never writes into a workspace, so no recovery step can touch your
source.

## Adding a language or platform

Every language or platform is an **analyzer plugin**
(`crates/attic-analyzers/src/plugin.rs`). Indexing, storage, retrieval and the
server never change when you add one. Pick the smallest path that fits:

| Path | Effort | You get | Example |
|---|---|---|---|
| **A · Tags query** | ~30 lines | Symbol definitions + in-file references | Kotlin, Swift, Rust |
| **B · Full analyzer** | A few hundred lines | Symbols, imports, relationships | Java, Python, Go |
| **C · Platform plugin** | Your own `AnalyzerPlugin` | Path-aware classification + custom structure | AEM |

<details open>
<summary><b>A · A language with a tree-sitter grammar (worked example: Kotlin)</b></summary>

1. **Add the grammar** to the workspace `Cargo.toml` and to
   `crates/attic-analyzers/Cargo.toml`:

   ```toml
   tree-sitter-kotlin-ng = { workspace = true }   # workspace: "1.1"
   ```

2. **Write (or reuse) a tags query** in
   `crates/attic-analyzers/src/structural/tags_generic.rs`. Use the grammar's
   own `queries/tags.scm` when it ships one; otherwise author the definitions
   you want from its `node-types.json`:

   ```rust
   const KOTLIN_TAGS_QUERY: &str = r#"
   (class_declaration name: (identifier) @name) @definition.class
   (object_declaration name: (identifier) @name) @definition.object
   (function_declaration name: (identifier) @name) @definition.function
   "#;
   ```

3. **Add one row** to the language table in the same file:

   ```rust
   TagsLanguageSpec {
       analyzer_id: "kotlin-tags",
       language_tag: "kotlin",
       description: "tree-sitter-tags structural analyzer for Kotlin: …",
       grammar: tree_sitter_kotlin_ng::LANGUAGE,
       tags_query: KOTLIN_TAGS_QUERY,
       locals_query: "",
   },
   ```

4. **Claim the file extensions** with one line in `builtin_plugins()` in
   `plugin.rs`:

   ```rust
   tier2("kotlin", "Kotlin: classes, objects, functions and type aliases",
         &[], &[("kt", "kotlin"), ("kts", "kotlin")]),
   ```

5. **Test it** with a small fixture in
   `crates/attic-analyzers/tests/structural_tags_tier2.rs`, and add the id to
   the `Built-in ids` comment in `ATTIC_TOML_TEMPLATE`
   (`crates/attic-core/src/config.rs`) — a unit test fails until you do.

</details>

<details>
<summary><b>B · A full, hand-written analyzer</b></summary>

Implement `TreeSitterLanguageSpec` in a new module under
`crates/attic-analyzers/src/structural/` (use `java.rs` or `python.rs` as the
template: symbols, imports, heritage, calls), then register it in
`builtin_plugins()` with `Registration::Specialized(your_module::analyzer)`.
Declare capabilities honestly in the analyzer descriptor — never claim a
relationship level the analyzer does not produce. Add a fixture under
`crates/attic-analyzers/tests/fixtures/` and cover it in
`language_specific_extraction.rs` and `language_invariants_matrix.rs`.

</details>

<details>
<summary><b>C · A platform plugin (like AEM)</b></summary>

Implement the four-method trait. Paths arrive normalized (`/` separators,
lowercase), so a plugin behaves identically on Windows, macOS and Linux:

```rust
use std::sync::Arc;
use attic_analyzers::{AnalyzerPlugin, AnalyzerRegistry, PluginPath};

struct TerraformPlugin;

impl AnalyzerPlugin for TerraformPlugin {
    fn id(&self) -> &'static str { "terraform" }                  // attic.toml id
    fn description(&self) -> &'static str { "Terraform modules and resources" }
    fn language_hint(&self, path: &PluginPath) -> Option<&'static str> {
        matches!(path.extension(), Some("tf") | Some("tfvars")).then_some("terraform")
    }
    fn register(&self, registry: &mut AnalyzerRegistry) {
        registry.register_for_language("terraform", Arc::new(TerraformAnalyzer::new()));
    }
}
```

Add it to `builtin_plugins()` — before the generic extension rules if it
claims paths by layout, as `aem.rs` does for `jcr_root/` — or compose it at
runtime with `PluginCatalog::builtin().with_plugin(Arc::new(TerraformPlugin))`
(duplicate ids are rejected). Parse tolerantly: malformed input should yield
partial structure plus a warning, never a failed file.

</details>

**Rules for every path:** a disabled plugin's files stay searchable through
`GenericAnalyzer`; plugin ids are lowercase and stable (they appear in
`attic.toml`); and bump `ANALYZER_REGISTRY_VERSION`
(`crates/attic-core/src/constants.rs`) whenever an existing analyzer's output
changes, so cached analyses are recomputed on the next start.

## Development

```sh
rustup show                                   # installs the pinned toolchain
cargo build --package attic-server            # debug build → target/debug/attic
cargo test -p <crate>                         # focused, fast inner loop
cargo test --workspace                        # everything
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
```

### Pre-commit checks and local install

The same commands on every OS; no feature flags or `--target` needed (the
DirectML GPU backend is built in automatically on Windows MSVC).

```sh
cargo fmt --all
cargo fmt --all --check
cargo check  --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test   --workspace
cargo build  --release -p attic-server
```

The real-GPU end-to-end test runs automatically inside `cargo test` when
`~/.attic/models/onnx-fp16` exists; `ATTIC_RUN_MODEL_E2E=0` skips it.

Install the release build as the local server — Windows (PowerShell):

```powershell
$out = if ($env:CARGO_BUILD_TARGET) { ".\target\$env:CARGO_BUILD_TARGET\release" } else { ".\target\release" }
Get-Item "$out\attic.exe"                                   # confirm it linked
Get-Process attic* -ErrorAction SilentlyContinue | ForEach-Object { Stop-Process -Id $_.Id -Force }
Start-Sleep -Seconds 2                                      # let Windows release file locks
Copy-Item "$out\attic.exe" "$env:USERPROFILE\.attic\attic-server.exe" -Force
Copy-Item "$out\*.dll"     "$env:USERPROFILE\.attic\" -Force   # DirectML/ONNX Runtime DLLs
```

Linux / macOS:

```sh
pkill -x attic-server || true
install -m 755 target/release/attic ~/.attic/attic-server
```

If `%USERPROFILE%\.cargo\config.toml` forces `target = "x86_64-pc-windows-gnu"`
(see below), delete that line when MSVC is installed, or run
`$env:CARGO_BUILD_TARGET='x86_64-pc-windows-msvc'` once per shell before the
commands above.

<details>
<summary><b>Toolchains per platform</b></summary>

- **Windows (recommended):** rustup's default `x86_64-pc-windows-msvc` plus
  "Build Tools for Visual Studio" with the C++ workload. The DirectML GPU
  backend is built in automatically with MSVC — plain `cargo build` /
  `cargo test` include it, no feature flag.

  > **The GNU override below silently disables the GPU build.** ONNX Runtime
  > publishes no `x86_64-pc-windows-gnu` binaries, so a `[build] target`
  > override in `%USERPROFILE%\.cargo\config.toml` produces a CPU-only
  > binary. If you have that override set but also have MSVC installed,
  > either delete the override or pass the target explicitly:
  >
  > ```
  > cargo build --release --target x86_64-pc-windows-msvc
  > ```
  >
  > and install from `target/x86_64-pc-windows-msvc/release/`, copying the
  > `onnxruntime*.dll` and `DirectML.dll` staged beside the binary along
  > with it — DirectML fails to load if they are not adjacent to the exe.
- **Windows without MSVC:** MinGW via [Scoop](https://scoop.sh)
  (`scoop install mingw`, no admin), `rustup target add x86_64-pc-windows-gnu`,
  and a **local, untracked** override in `%USERPROFILE%\.cargo\config.toml`:

  ```toml
  [build]
  target = "x86_64-pc-windows-gnu"

  [target.x86_64-pc-windows-gnu]
  linker = "C:\\Users\\<you>\\scoop\\apps\\mingw\\current\\bin\\gcc.exe"
  ```

- **Linux:** a system C compiler (`build-essential` / `gcc`).
- **macOS:** `xcode-select --install`.

</details>

<details>
<summary><b>Opt-in tests, benchmarks and test hooks</b></summary>

| Variable | Purpose |
|---|---|
| `ATTIC_BENCH_INDEX=1` (+ `ATTIC_BENCH_ROOT`, …) | Indexing benchmark — see [`PERFORMANCE.md`](PERFORMANCE.md) |
| `ATTIC_BENCH_QWEN=1` (+ `ATTIC_BENCH_QWEN_CORPUS`, …) | Real-model embedding benchmark |
| `ATTIC_RUN_MODEL_E2E=0/1` | Real-model e2e: runs automatically on Windows MSVC when `~/.attic/models/onnx-fp16` exists; `0` skips, `1` forces (CPU if no GPU assets) |
| `ATTIC_ACCEPTANCE_DUMP=<dir>` | Frozen-count acceptance run over the reference JSON-export corpus |
| `ATTIC_ACCEPTANCE_WORKSPACE=<dir>` | Frozen-count acceptance run over the reference 20-repository workspace |
| `ATTIC_FORCE_RESOURCE_PRESSURE` | Fault injection: start at a fixed pressure tier |
| `ATTIC_FORCE_CPU_CORES=<n>` | Test hook: pin the detected physical core count so resource tests don't depend on the machine |
| `ATTIC_PRESSURE_OVERRIDE_FILE` | Fault injection: a file whose content (`normal`…`emergency`) forces the tier while it exists |
| `ATTIC_FAST_RECOVERY_MS` | Fault injection: shorten graduated-recovery dwell times |
| `ATTIC_MOCK_WORKER` | Inference-worker test double: `echo` / `hang` / `corrupt` / `crash` |

The acceptance runs assert exact counts for their reference corpora and are
skipped when the variable is unset; everything else in the suite is
self-contained (temporary directories, no network, no global Git config).

</details>

`target/` is Cargo's build cache, not Attic's index — `cargo clean` is always
safe.

## Maintenance

<details open>
<summary><strong>Procedures</strong></summary>

- **Schema changes** — migrations are ordered and forward-only. Add
  `migrations/000N_<name>.sql` (core, wired into `run_migrations` in
  `crates/attic-storage/src/migration.rs`) or
  `migrations/semantic/000N_<name>.sql` (wired into `SemanticStore::migrate`).
  Each script records itself; never edit one that has shipped. A database
  carrying a version the binary does not know is refused, not guessed at.
- **Analyzer or grammar update** — bump the `tree-sitter-*` dependency, check
  its ABI against `tree-sitter`'s supported range, re-run the analyzer tests,
  and bump `ANALYZER_REGISTRY_VERSION` if output changes.
- **New dependency** — its license must be compatible with
  `MIT OR Apache-2.0`, and it must support Linux, macOS and Windows.
- **Retrieval changes** — modify contracts or candidate generation in
  `crates/attic-retrieval`, then run its regression gates in
  `crates/attic-retrieval/tests/`.
- **Release** — bump `version` in the root `Cargo.toml`; CI
  (`.github/workflows/release.yml`) runs `tools/package.sh --target <triple>`
  for every supported target on tag push. Never weaken
  `tools/package.sh --verify`'s exclusion checks (no `target/`, `*.db*`, logs
  or hidden files) to make a release pass, and never work around a build
  failure by weakening endpoint security (antivirus exclusions, signing
  bypasses) — find the actual cause.

</details>
