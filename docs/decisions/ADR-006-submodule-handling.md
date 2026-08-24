# ADR-006 — Git Submodule Handling in Discovery

**Date**: 2026-08-24
**Status**: ACCEPTED
**Deciders**: Phase 1B implementation
**Resolves**: OQ-004

---

## Context

The Phase 1B discovery pipeline walks a repository's working tree and builds a
per-file manifest.  Git submodules are directories inside the parent working
tree that are themselves independent Git repositories.  Two treatment strategies
were considered for the parent walk:

1. **Opaque subtree** — treat the submodule directory as ordinary files inside
   the parent walk; include submodule object files in the parent manifest.
2. **Boundary stop + skip** — refuse to descend into submodule directories
   during the parent walk; emit a diagnostic and record the boundary so callers
   know about it.

---

## Decision

**Phase 1B implements boundary detection and exclusion only.**

Concretely, the only Phase 1B behaviour is:

- During the `ignore`-crate walk of the parent repository, any directory that
  contains its own `.git` file or `.git/` directory is treated as a **nested
  repository boundary**.
- The walk does **not** descend into that directory.
- A `DiagnosticKind::SubmoduleDetected` diagnostic is emitted, carrying the
  repo-relative path of the detected boundary.
- The submodule's repo-relative prefix is recorded in `WalkResult::submodule_prefixes`
  (a `Vec<String>`) so callers can enumerate detected boundaries.

Phase 1B does **not**:

- Create `core_repositories` rows for submodules.
- Build `WorkspaceSnapshot`, `SourceRevision`, or manifest records for submodules.
- Schedule submodule index passes.
- Read the submodule's `HEAD` SHA or include it in the parent manifest hash.

Registration of submodule boundaries into the storage layer, cross-repository
indexing, and incremental scheduling of submodule discovery passes are
**future work** deferred to Phase 2 and beyond (see §Future Work below).

---

## Rationale

| Criterion | Opaque subtree | Boundary stop (Phase 1B) |
|-----------|---------------|--------------------------|
| Correctness | ❌ Hashes submodule object files (binary blobs, not source) | ✅ Parent manifest contains only parent source files |
| Phase 1B scope | ⚠️ Simpler but wrong — creates manifest debt for Phase 2 | ✅ Clean boundary from day one; submodule work deferred safely |
| Diagnostic visibility | ❌ Submodule silently absorbed | ✅ Caller knows exactly which boundaries were skipped |
| Forward compatibility | ❌ Future cross-repo navigation requires re-manifesting everything | ✅ Storage registration can be added in Phase 2 without retroactive fixes |

The explicit `.git`-presence check is necessary because the `ignore` crate does
**not** automatically stop at `.git` boundaries inside a walk that was started
from a parent root.

---

## Consequences

### Phase 1B (implemented)

- `walk.rs` checks `abs_path.join(".git").exists()` for every directory entry
  it encounters.  When `true`, it:
  1. emits a `DiagnosticKind::SubmoduleDetected` diagnostic,
  2. appends the repo-relative prefix to `WalkResult::submodule_prefixes`, and
  3. skips (does not descend into) that directory for the remainder of the pass.
- `EligibleEntry` values returned by `walk()` contain **only** files from the
  parent repository's own working tree — no submodule files are included.
- The parent `SourceManifest` hash therefore reflects the parent's source files
  only, which is correct.

### Future work (Phase 2+)

- A registration step will iterate `WalkResult::submodule_prefixes`, create or
  update a `core_repositories` row for each submodule, and enqueue each
  submodule for its own discovery pass.
- Each submodule will eventually have its own `SourceManifest` and
  `SourceRevision`.
- The parent manifest may optionally include a `submodule_revisions` field
  (Vec<(String, String)> — relative path + HEAD SHA) so the parent hash changes
  when a submodule advances.  This design is deferred and will be documented in
  a Phase 2 ADR.
- Uninitialized submodules (no `.git` file inside the directory) are treated as
  an empty directory in Phase 1B; Phase 2 may add explicit tracking of
  uninitialized entries.

---

## Alternatives Rejected

**Merge submodule content into parent manifest**: rejected because submodule
object files are opaque binary blobs and do not represent source content.
Including them in the parent manifest would cause spurious hash changes
unrelated to source changes.

**Ignore submodules entirely (no diagnostic)**: rejected because callers would
have no visibility into skipped boundaries, making silent data gaps possible.
The `SubmoduleDetected` diagnostic makes every skipped boundary observable.
