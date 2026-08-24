# Contract: File Discovery and Ignore Policy

## Purpose

Define how Attic determines which files in a repository are eligible for
indexing, how ignore rules are layered and resolved, and how security
exclusions interact with ordinary ignore policy. Discovery happens before
any parsing, analysis, or indexing.

---

## Definitions

### DiscoveryPolicy

The complete, serializable set of rules that governs file eligibility for
a repository at a given point in time. Its BLAKE3 hash is recorded in
`SourceRevision.discovery_policy_hash`.

```
DiscoveryPolicy {
  repository_id         : Uuid
  git_aware             : bool          -- use Git index / .gitignore
  include_untracked     : bool          -- include untracked (non-ignored) files
  default_exclusions    : bool          -- apply built-in default exclusions
  security_exclusions   : bool          -- always true; cannot be disabled
  attic_include_rules   : Vec<GlobRule> -- explicit Attic include overrides
  attic_exclude_rules   : Vec<GlobRule> -- explicit Attic exclude rules
  custom_priority_overrides: Vec<PriorityRule>
}
```

### GlobRule

```
GlobRule {
  pattern   : String  -- glob pattern, repository-relative
  negation  : bool    -- true = include (negates prior exclude)
}
```

### DiscoveryClass

The priority class assigned to each eligible file.

```
IGNORED         -- not indexed
LOW_PRIORITY    -- indexed but deprioritized
NORMAL          -- indexed at normal priority
HIGH_PRIORITY   -- indexed first
```

---

## Rule Application Order

Rules are evaluated in strict order. Later rules override earlier ones for
the same path, with the exception of security exclusions (always win).

```
1. SECURITY EXCLUSIONS (absolute; cannot be overridden)
2. Git ignore semantics (.gitignore, .git/info/exclude, global gitconfig)
3. Default Attic exclusions (see §Default Exclusions)
4. Attic workspace-level exclude rules
5. Attic repository-level exclude rules
6. Attic repository-level include rules (can re-include previously excluded)
7. Priority overrides
```

Security exclusions in step 1 are final. No include rule in any later
step can make a security-forbidden path eligible.

---

## Git-Aware Behavior

When `git_aware = true` and the repository contains a `.git/` directory:

1. Start with all files tracked by `git ls-files`.
2. Optionally include untracked files (those not listed in `.gitignore`
   and not staged for deletion) when `include_untracked = true`.
3. Apply nested `.gitignore` files at each directory level, following
   standard Git semantics (child rules override parent for their subtree).
4. Apply `.git/info/exclude` (per-repo local excludes not committed).
5. Apply the user's global `.gitconfig core.excludesFile` if it exists,
   EXCEPT: Attic MUST NOT read from `$HOME/.gitconfig` directly during
   indexing. Instead, the workspace configuration specifies whether global
   gitconfig integration is enabled (default: disabled for security).

When `git_aware = false` (non-Git workspace):
- Walk the filesystem recursively from the repository root.
- Apply default exclusions and Attic rules only.
- No `.gitignore` semantics.

---

## Nested .gitignore Semantics

For Git-aware discovery:

1. `.gitignore` files at the repository root apply to all files.
2. `.gitignore` files in subdirectories apply only to files within that
   subdirectory subtree.
3. Negation rules (`!pattern`) in a `.gitignore` can un-ignore files
   previously excluded by a parent `.gitignore`.
4. An Attic-level include rule can un-include a Git-ignored file only if
   the file is not in a security-forbidden category.
5. `.git/info/exclude` follows the same semantics as root `.gitignore`
   but is per-repository and not committed.

---

## Default Exclusions

Applied when `default_exclusions = true` (the default). These paths are
assigned `DiscoveryClass::IGNORED` unless overridden by an explicit Attic
include rule.

```
.git/
node_modules/
coverage/
.nyc_output/
.cache/
__pycache__/
.pytest_cache/
.venv/
venv/
env/
.env/
.next/
.nuxt/
.svelte-kit/
.parcel-cache/
.gradle/
target/          -- Rust/Maven build output
build/
dist/
out/
_site/
.tox/
*.egg-info/
__mocks__/       -- not excluded; only listed here as a note that it is NORMAL
```

The following are NOT excluded by default (configurable):
```
vendor/
generated/
fixtures/
snapshots/
migrations/
```

Their default `DiscoveryClass` when present:
```
vendor/      LOW_PRIORITY
generated/   LOW_PRIORITY
fixtures/    LOW_PRIORITY
snapshots/   LOW_PRIORITY
migrations/  NORMAL
```

---

## Security Exclusions

Security exclusions are always enforced regardless of any other rule.
The `security_exclusions` flag in `DiscoveryPolicy` is informational only;
setting it to `false` is rejected with a configuration error.

Security-excluded paths:

```
.git/objects/     -- raw Git object store (not useful; potentially huge)
.git/refs/        -- not needed for indexing
.ssh/             -- anywhere in the workspace
.gnupg/           -- anywhere in the workspace
*.pem             -- PEM-encoded keys
*.key             -- private key files (heuristic; configurable extension list)
*.p12             -- PKCS#12 bundles
*.jks             -- Java keystores
.env              -- dotenv files containing secrets (scanned, not indexed raw)
.env.*            -- variants
```

Additionally, any file that the secret scanner marks as containing
high-confidence secret material receives `security_state = FORBIDDEN` and
is never indexed in unredacted form, regardless of discovery class.

---

## Symlink Handling

1. Symlinks within a repository are resolved to their canonical target.
2. If the resolved canonical target path is outside the configured allowed
   root(s), the symlink is treated as `INACCESSIBLE` with a
   `SYMLINK_ESCAPE` diagnostic. It is never followed.
3. Circular symlinks are detected (via visited-inode tracking) and treated
   as `INACCESSIBLE` with a `SYMLINK_CYCLE` diagnostic.
4. Symlinks pointing to directories are traversed (within allowed roots).
5. Symlinks to files are treated as the file content at the target.

---

## Non-Git Workspace Behavior

When no `.git/` directory exists at the repository root:

1. `git_aware` must be `false`.
2. Files are discovered via recursive filesystem walk.
3. Standard default exclusions apply.
4. Attic include/exclude rules apply.
5. No `.gitignore` processing.
6. All eligible files are treated as `untracked` (equivalent to
   `include_untracked = true` semantics).

---

## Priority Classification

Default priority assignments:

```
node_modules/          IGNORED
.git/                  IGNORED (except .git/info/exclude, read but not indexed)
compiled build output  IGNORED
coverage output        IGNORED

vendor/                LOW_PRIORITY
generated/             LOW_PRIORITY
fixtures/              LOW_PRIORITY
snapshots/             LOW_PRIORITY
tests/                 NORMAL
__tests__/             NORMAL
spec/                  NORMAL
docs/                  NORMAL
config/                NORMAL
migrations/            NORMAL

src/                   HIGH_PRIORITY
lib/                   HIGH_PRIORITY
app/                   HIGH_PRIORITY
services/              HIGH_PRIORITY
knowledge/             HIGH_PRIORITY
cmd/                   HIGH_PRIORITY
pkg/                   HIGH_PRIORITY
```

Priority affects indexing order and scheduling, NOT retrieval ranking.

---

## Policy Change Behavior

When the `DiscoveryPolicy` changes (new `discovery_policy_hash`):

1. A new `SourceRevision` is required for the affected repository.
2. The discovery pass re-runs for that repository.
3. Files newly excluded by the updated policy receive
   `existence_state = EXCLUDED` in their next `FileOccurrence`.
4. Files newly included receive a new `FileOccurrence` and are scheduled
   for indexing.
5. Files whose `DiscoveryClass` changes (e.g., LOW_PRIORITY → HIGH_PRIORITY)
   are reprioritized in the indexing queue without re-indexing unless content
   changed.
6. The change is recorded as an `InvalidationRecord` with
   `reason = POLICY_CHANGED` for all affected artifacts.

---

## Invariants

1. A file in `security_state = FORBIDDEN` is never made eligible by any
   include rule, regardless of rule order or source.
2. `DiscoveryClass::IGNORED` files produce no `FileOccurrence` record (they
   are not tracked at all, not even as EXCLUDED).
3. The `DiscoveryPolicy` is fully serializable and its canonical
   representation is deterministic (same policy = same hash).
4. Discovery does not execute repository content as code or shell commands.
5. The discovery walk never escapes the configured allowed root, even via
   symlinks.
6. Every change to ignore rules or Attic policy produces a new
   `discovery_policy_hash`, which triggers a new `SourceRevision`.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| `.gitignore` file unreadable | Log diagnostic; treat as absent; continue |
| Symlink target outside allowed root | File marked `INACCESSIBLE`; `SYMLINK_ESCAPE` diagnostic |
| Circular symlink detected | File marked `INACCESSIBLE`; `SYMLINK_CYCLE` diagnostic |
| Directory unreadable during walk | Log diagnostic; skip subtree; continue |
| `.git/info/exclude` unreadable | Log diagnostic; treat as absent; continue |
| Policy serialization fails | Discovery fails; error returned; no partial capture |

---

## Observability

Each discovery pass logs:

```
repository_id
discovery_policy_hash
total_files_walked
files_eligible: { NORMAL, HIGH_PRIORITY, LOW_PRIORITY }
files_ignored
files_excluded_security
files_excluded_default
files_excluded_attic
files_inaccessible
symlink_escape_count
symlink_cycle_count
duration_ms
```

---

## Examples

### Standard Git project

```
Repository root: /workspace/my-project/
.gitignore: node_modules/, dist/, *.log
Attic exclude: vendor/
Attic include: vendor/critical-lib/   (re-includes subset of vendor)

Result:
  node_modules/    IGNORED (Git + default)
  dist/            IGNORED (Git + default)
  *.log files      IGNORED (Git)
  vendor/          EXCLUDED (Attic)
  vendor/critical-lib/  NORMAL (Attic include re-include)
  src/             HIGH_PRIORITY
  tests/           NORMAL
```

### Security override attempt

```
File: .env (contains API_KEY=secret123)
Attic include rule: .env   <-- operator attempts to include

Result:
  Secret scanner marks .env as FORBIDDEN
  FORBIDDEN overrides all include rules
  .env never indexed
  Diagnostic: SECURITY_OVERRIDE_REJECTED
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| DI-01 | Git repo; `node_modules/` present | `node_modules/` files: IGNORED |
| DI-02 | `.gitignore` excludes `dist/`; Attic adds no override | `dist/` files: IGNORED |
| DI-03 | `.gitignore` excludes `vendor/`; Attic includes `vendor/lib/` | `vendor/lib/` files: NORMAL (re-included) |
| DI-04 | Symlink to path outside allowed root | Marked INACCESSIBLE; SYMLINK_ESCAPE diagnostic |
| DI-05 | Circular symlink | Marked INACCESSIBLE; SYMLINK_CYCLE diagnostic |
| DI-06 | `.env` file with secret content | FORBIDDEN; not indexed regardless of include rules |
| DI-07 | Non-Git workspace | Files discovered by filesystem walk; no .gitignore processing |
| DI-08 | Nested `.gitignore` in subdirectory | Subdirectory rules applied only to that subtree |
| DI-09 | Discovery policy changes | New `discovery_policy_hash`; newly excluded files get EXCLUDED state |
| DI-10 | Unreadable `.gitignore` | Diagnostic logged; discovery continues without that file's rules |
| DI-11 | `knowledge/` directory present | HIGH_PRIORITY |
| DI-12 | `src/` directory present | HIGH_PRIORITY |
| DI-13 | `migrations/` directory | NORMAL (not excluded) |
| DI-14 | Generated source in `generated/` | LOW_PRIORITY |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| DI-Q1 | Should `.git/info/exclude` reading be gated by the `git_aware` flag, or always applied when `.git/` exists? | No — always apply when `.git/` exists |
| DI-Q2 | Should global `~/.gitconfig core.excludesFile` be supported? Raises security concerns (reads user home). | No — disabled by default; opt-in via workspace config |
| DI-Q3 | `.pem` and `.key` file exclusion: should extension list be hardcoded or operator-configurable? | No — hardcoded default; operator can add extensions but cannot remove security defaults |
