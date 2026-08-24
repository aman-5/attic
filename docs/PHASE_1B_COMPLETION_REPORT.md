# Phase 1B Completion Report — Git-Aware Discovery and Security

**Date**: 2026-08-24
**Crate**: `attic-discovery` v0.1.0
**Phase**: 1B (Git-Aware Discovery and Security)
**Status**: ✅ COMPLETE — all gates passed

---

## 1. Scope

Phase 1B implements the full discovery pipeline for the Attic code-intelligence
server.  The pipeline takes a filesystem root, applies a `DiscoveryPolicy`, and
produces a `WorkspaceSnapshot` that downstream phases (Phase 1C analyzers, Phase
2 indexing) consume.

Key deliverables per the Phase 1B contract:

| Deliverable | Contract reference |
|---|---|
| Git root detection and HEAD / branch resolution | `source_revision.md` §2 |
| `.gitignore`-aware file walk | `discovery.md` §4 |
| Security exclusion layer (always-on, never bypassed) | `discovery.md` §Security Exclusions |
| Secret detection and redaction | `secrets.md` §3 |
| File classification and priority assignment | `discovery.md` §3 |
| Per-file BLAKE3 content hash manifest | `source_revision.md` §2.3 |
| Discovery diagnostics (warnings without aborting) | `discovery.md` §Diagnostics |
| `DiscoveryPolicy` validation | `discovery.md` §2 |

---

## 2. Module Inventory

| Module | Lines (approx.) | Responsibility |
|---|---|---|
| `lib.rs` | 120 | Public API: `discover()`, `WorkspaceSnapshot` |
| `policy.rs` | 280 | `DiscoveryPolicy`, `GlobRule`, `DiscoveryPriority`, validation |
| `git.rs` | 260 | `.git/HEAD` parsing, packed-refs, root detection |
| `security.rs` | 210 | Security-forbidden path enforcement, path canonicalization |
| `secrets.rs` | 490 | Secret detectors (PK-001, AWS-001, GH-001, JWT-001, HE-001), redaction |
| `classification.rs` | 320 | Priority classification, glob rule application, explicit include/exclude |
| `walk.rs` | 350 | `ignore`-crate integration, gitignore-aware walk, sorted deterministic output |
| `manifest.rs` | 180 | BLAKE3 manifest hash, `ManifestEntry`, `WorkspaceManifest` |
| `diagnostics.rs` | 90 | `DiscoveryDiagnostic` type, diagnostic accumulator |
| `error.rs` | 80 | `DiscoveryError` enum |

---

## 3. Test Results

```
running 91 tests
...
test result: ok. 91 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s
```

All 91 unit tests pass on `x86_64-pc-windows-msvc`.

### Test coverage by module

| Module | Tests |
|---|---|
| `classification` | 15 |
| `diagnostics` | 2 |
| `git` | 10 |
| `manifest` | 6 |
| `policy` | 6 |
| `secrets` | 13 |
| `security` | 14 |
| `walk` | 11 |
| `lib` (integration) | 8 |
| **Total** | **91** |

---

## 4. Clippy Gate

```
cargo clippy --target x86_64-pc-windows-msvc -p attic-discovery -- -D warnings
Finished `dev` profile — 0 warnings, 0 errors
```

All `-D warnings` lints pass.  Issues resolved during Phase 1B:

| Lint | Location | Fix |
|---|---|---|
| `clippy::manual_strip` | `classification.rs` | `strip_suffix('/')` |
| `clippy::question_mark` | `git.rs` | `let parent = current.parent()?` |
| `clippy::collapsible_if` | `git.rs` | `if let … && …` |
| `clippy::collapsible_if` | `secrets.rs` | `if let … && …` |
| `clippy::collapsible_if` | `security.rs` | `if let … && …` |
| `dead_code` | `secrets.rs` | Removed unused `PEM_PRIVATE`, `PEM_RSA` constants |

---

## 5. Security Invariants Verified

All invariants from `SECURITY_INVARIANTS.md` applicable to Phase 1B:

| Invariant | Test |
|---|---|
| `.git/` tree is never returned by walk | `walk::dot_git_contents_never_returned` |
| `.ssh/` and `.gnupg/` are security-forbidden | `security::ssh_dir_is_forbidden`, `gnupg_dir_is_forbidden` |
| `.pem`, `.key`, `.p12`, `.jks` files are forbidden | `security::pem_extension_is_forbidden`, etc. |
| `.env` and `.env.*` files are forbidden | `security::dotenv_is_forbidden` |
| Scan-exempt paths cannot overlap forbidden prefixes | `security::scan_exempt_ssh_rejected` |
| No include rule can override a forbidden path | `lib::discover_excludes_security_forbidden_files` |
| PEM private keys are fully redacted | `secrets::detects_pem_private_key` |
| AWS keys partially redacted (first 4 chars preserved) | `secrets::detects_aws_access_key`, `partial_redact_keeps_first_four` |
| Path traversal (`..`) cannot escape root | `security::normalize_repo_relative_traversal_rejected` |

---

## 6. Key Design Decisions Made During Phase 1B

### 6.1 Rule Application Order

Security exclusions → gitignore → default exclusions → attic exclude rules →
attic include rules → priority overrides.  Security exclusions are always first
and cannot be overridden by any subsequent rule.

### 6.2 Secret De-overlap Algorithm

All detectors are run over the input, all match ranges are collected, sorted by
start offset (ties broken longest-first), then de-overlapped in a single pass.
Redaction is applied in a single forward pass with an offset delta.  This
prevents earlier redactions from corrupting the byte offsets of later matches.

### 6.3 `is_base64_char` Excludes `=`

The hex-entropy detector (`HE-001`) excludes `=` from its base64-character set.
This prevents `key=AKIA…` from being treated as a single HE-001 candidate that
shadows the more specific AWS-001 match on the value portion.

### 6.4 Explicit Include Semantics

A path matched by an `attic_include_rules` rule (i.e. a `GlobRule` with
`negation = false`) is `explicitly_included`.  Such paths bypass both default
exclusions and attic exclude rules, and are assigned at least `Normal` priority.
This is the correct inversion of gitignore semantics where `!pattern` un-ignores
a previously ignored path.

### 6.5 Git Submodule Handling (OQ-004)

Each submodule is a separate `core_repositories` entry.  See
`docs/decisions/ADR-006-submodule-handling.md`.

---

## 7. Open Questions Resolved

| OQ | Resolution summary |
|---|---|
| OQ-004 | Submodules → separate `core_repositories` entries. ADR-006. |
| OQ-005 | Manifest hash from file content, not Git objects; restored files → same hash. |
| OQ-009 | `unicode61` retained for Phase 1B; `trigram` deferred to Phase 1D. |
| OQ-010 | Advisory limits: ≤ 50 repos, ≤ 2M files, ≤ 20M symbols; hard enforcement Phase 2. |
| OQ-015 | Synthetic Rust workspace in `fixtures/git/`; no real repos in CI. |

---

## 8. Dependencies Added (ADR-005)

| Crate | Version | Justification |
|---|---|---|
| `ignore` | 0.4.33 | gitignore-aware directory walk |
| `blake3` | 1.8.7 | BLAKE3 manifest hashing (source_revision contract) |
| `tempfile` | 3 (dev) | Test fixture directory creation |

No `git2` / cmake dependency was introduced.  Git metadata is read via manual
`.git/HEAD` and `packed-refs` parsing (ADR-005 §3).

---

## 9. Phase 1C Entry Checklist

The following are confirmed ready for Phase 1C (analyzers):

- [x] `WorkspaceSnapshot` is stable and serialisable
- [x] `DiscoveryPriority` is exported from `attic-discovery` public API
- [x] `DiscoveryDiagnostic` accumulator pattern is established
- [x] Security boundary is enforced before any file content reaches analyzers
- [x] Secret redaction runs before file content is stored in the manifest
- [x] All Phase 1B OQs resolved or deferred with documented rationale
- [x] 91 tests pass, 0 clippy warnings
