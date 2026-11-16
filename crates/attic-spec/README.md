# Spec-first EGM workflow

Attic supplies verified source context, human approval, fenced implementation
waves and optional change-budget hooks. Reviewer contexts run in your agent host;
the built-in `egm` guidance supplies their three missions and pack handoff.
The pinned upstream EGM plugin is an optional host adapter, not installed by Attic.
No upstream tool allowlist or host settings are changed.
This is workflow governance, not a security sandbox or proof of code correctness.

Absolute file-tool targets are checked against their longest planning-enabled
active root, independently of the hook's working directory. Metadata and
lifecycle requests do not open unrelated repository indexes; an unavailable
unrelated root does not block status, heartbeat or abandon.

During completion grace, staged-scope receipts cover approved glob-matched files
as well as exact citations. They bind the actual staged operation/mode/blob,
not later unstaged worktree edits. Branch receipts retain their longer-lived
semantics. Missing or unreadable design documents invalidate verified/approval
evidence conservatively, but cannot prevent lease revocation, reopen or abandon;
historical approval/run records remain available.

## Activation and report-only defaults

EGM defaults to enabled for all active roots in manual, report-only mode. An
explicit `enabled = false` remains disabled. To change activation or restrict
roots, run `attic egm enable --root PATH --mode manual` (or `--mode autopilot`),
then restart your Attic MCP server. Use `attic egm status` and the always-available
MCP `egm` tool to inspect activation; `attic egm disable` preserves all workflow
history. `attic egm guidance` / MCP `egm action=guidance` supply mode-aware
reviewer handoff instructions. No configuration hand-editing is needed.
Bare enable selects the current directory; `--all-roots` selects all active roots.
Enable always defaults to report-only, even after a previous enforced session;
`--enforce` is required to select enforcement again.
CLI changes preserve unrelated configuration and comments and never open indexes
or workflow state. Reconnecting to an existing daemon is not a restart.

Alternatively, edit the settings directly:

In the existing runtime `attic.toml` (not the workspace-membership `config.toml`):

```toml
[planning]
enabled = true
mode = "manual" # or "autopilot"; human approval is still mandatory
designs_dir = "docs/designs"

[planning.fast_path]
enforce = false
max_files = 2
max_lines = 100
scope = "staged"
allow_binary = false
commit_grace_hours = 24

[planning.hooks]
fail_closed = false
```

With EGM enabled, server startup opens its workflow state database; later normal
indexing records analysis receipts. Explicitly disabled hook
entrypoints exit successfully before parsing payloads or creating directories,
opening workflow databases, starting a daemon, or migrating a legacy layout.
Enabling planning records analysis receipts on subsequent **normally authorized**
indexing; it never initiates a migration/reindex/reset of existing indexes.

`roots = ["/absolute/checkout"]` restricts planning to a subset of active workspace
roots; an empty list selects all active roots. Nested settings reject unknown
fields. See the commented runtime configuration template for bounded lease,
verification-attempt, freshness, retention, and pack settings.

## Workflow

1. Author `docs/designs/<spec_id>.md`; IDs are 1–63 lowercase letters/digits,
   `_` or `-`, starting with a letter/digit. Include exactly one closed
   `attic-citations` fence with at least one `TARGET_*`:

   ````markdown
   ```attic-citations
   EXISTING_REF provider src/api.rs Api
   TARGET_MODIFY consumer src/main.rs
   TARGET_CREATE_FILE consumer src/new.rs
   TARGET_CREATE_SYMBOL provider src/api.rs NewApi
   CROSS_REPO_DEPENDENCY consumer -> provider
   ALLOW_GLOB consumer tests/**
   ```
   ````

2. Use MCP `spec` actions `register`, `verify`, and `pack`. The schema is strict.
   `status` returns the next required actor/action. Root basenames are convenient
   aliases, but ambiguity is an error; IDs/full stable root keys disambiguate.
3. EGM reviewers consume separate `zoom_out`, `codebase_details`, and
   `implementation_readiness` packs. File handles include path, revision, run ID,
   hash and byte count; explicit `delivery="inline"` discloses caller-context cost.
   The server reuses existing repository-map summaries for zoom-out, current
   non-inferred consumer→provider edges, and preprocessed/redacted source excerpts.
4. The **user**, not an agent, runs `attic spec approve <id> [--root PATH]` in their
   terminal. All three standard streams must be terminals. A six-character random
   challenge binds the verified document/run; concurrent document/state changes
   reject approval. There is no MCP approval action or production test bypass.
5. MCP `begin` re-verifies approval, creates provider-first waves and returns a
   lease/fence. Send both on `heartbeat`, `wave_complete` and implementation
   `drift`. Transit through non-written repositories and dependency SCCs are
   preserved. Expired workers must stop; `resume` obtains a new fenced lease.
6. Complete every current-revision wave, **stage intended content**, then `drift`.
   Only unchanged approved scope with completed waves can become `IMPLEMENTED`.
   Drift checks citations, not tests. `reopen` needs a reason and renewed human
   approval. `attic spec abandon <id> --reason TEXT` is also available.

Approval exit codes: 3 non-TTY, 4 stale document/run, 5 cancellation, 6 concurrent
state change; success is 0. `status` is read-only and `approve`/`abandon` open only
the authoritative spec store, never a catalog/index writer or daemon.

## Optional hooks and content accounting

`attic spec hooks print --host claude` prints mergeable SessionStart/PreToolUse
guidance; it never changes real settings. `attic spec skill print` prints pinned
EGM persona/context instructions without expanding reviewer Read/Grep/Glob access.

Explicit Git installation only:

```text
attic spec hooks install --git --root PATH [--chain]
attic spec hooks uninstall --git --root PATH
```

Installation honors `core.hooksPath`, preserves unmanaged hooks, and only chains
on request. Husky/pre-commit projects receive instructions instead of mutations.
The generated hook bakes in missing-binary policy and calls `check-commit`.

Capture a baseline **before any mutating tool** using SessionStart or
`attic spec baseline-start --root PATH --session ID`. Resume does not replace it.
Initially dirty content is exempt from the pre-write file budget only while its
content/existence is unchanged. Changed-again content counts. Missing/late
baselines and generic calls without session identity are conservative; never
capture a late baseline to retrospectively exempt changes.

`check-write --path PATH [--session ID]` counts uncovered dirty paths and the
prospective target. Claude payload mode is `--host claude`; NotebookEdit uses
`notebook_path`. Only bounded Markdown authoring under `designs_dir` is exempt,
not any code paths. Bash checks are advisory except approval/`--no-verify`
patterns; they are not a shell sandbox.

`check-commit` checks actual staged scope, including pre-existing developer edits.
For branch-wide budgets set `scope="branch"` and `base_branch="origin/main"`.
Successful completion stores **actual staged** content receipts, bound to root,
revision, approved run, operation/existence, mode and blob. Stage intended changes
before final drift to retain branch coverage across commits/reopen and beyond
grace. Partial staging cannot borrow different working-tree approval. New blobs,
renames, deletions and modes are re-evaluated. Receipts do not confer authorship.

Longest-root scope wins; related unregistered linked checkouts get no inherited
approval and receive uncovered checks plus registration guidance. Non-Git
enforcement remains deferred. Git hooks are bypassable; history cannot prove
whether hooks ran.

## Evidence, persistence and compatibility

Authoritative state is the global catalog's sibling `spec_state.db`, independent
of `repositories/<stable-id>/index.db`. Catalog/index rebuilds must not reset it.
Normalized documents use monotonic revision/version/fence CAS. Immutable run and
citation payloads retain full provenance alongside queryable normalized columns.
Events/approval bindings are append-only; even voided approvals permanently pin
their runs/citations through retention and reopen.

Store upgrades are contiguous, serialized, forward-only, and backed up before
mutation. Unknown/corrupt/newer formats fail closed. An interrupted migration can
leave a lock: opens return `ERR_BUSY`, never delete an unproven owner's lock.
Recovery requires independently proving no process still owns the migration.

Slow hashing/prompting happens outside short writes. Reconciliation is committed
separately before a mutating operation; read-only status/checks use its effective
projection. UTC microseconds are used throughout. Every implementation exit
clears both lease fields and increments the fence.

Optional `spec_index_analysis_receipts` is lazily added by enabled publication,
inside existing WriterQueue transactions. It binds analyzer name/version,
descriptor symbol capability, actual capability/fallback/errors/completeness,
bytes, source revision and generation to the exact file occurrence—even when
analysis produces no nodes. Publication with EGM disabled creates no receipt table
and retains the original cache-version format. Old occurrences without receipts,
lexical/fallback/partial/syntax-error analysis and misbound receipts cannot prove
symbol absence. Suffixes or node absence are never negative-symbol evidence.

Verification uses one read snapshot per repository plus before/after raw-byte
hashes; there is no claimed global atomic snapshot. Exact qualified definitions
must be unambiguous. Moving bytes/indexes produce bounded freshness retries or
an explicit error. Source/span changes do not alter semantic fingerprints;
qualified symbol signatures, stable root keys, kinds and tiers do.

Packs are atomically published outside indexed roots, size-bounded and redacted.
Unix modes are private; Windows applies a protected current-user-only DACL
through Windows PowerShell/.NET, failing explicitly if unavailable. Reopen/
abandon invalidates packs after the state transaction. Lazy pack-time cleanup
removes old files after 24 hours; this is **not** a hard expiration guarantee.

## Validation and external pilot

Focused fixtures live only under ignored `target/egm-tests`; hook installation
tests never touch this checkout's live hooks. Tests cover producer receipts,
legacy/unproven analysis, approval races, fencing, provider-first/SCC waves,
content baselines, actual index/branch receipts, SHA-256 initial commits,
private/redacted packs, CLI errors, explicit hook install and official rmcp flow.

The coordinator still owns integrated review, workspace quality gates,
cross-platform CI and separately approved rollout. Before enforcement, verify the
installed Claude version, real payloads, private out-of-workspace Read permission,
managed policy and shell/timeout behavior. Exit 2 blocks PreToolUse; ordinary
startup failures/timeouts may not. Current `onFailure=block` support requires
Claude Code **>=2.1.295** and must not be assumed on older hosts.
