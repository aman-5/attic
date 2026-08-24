# ADR-006 — Git Submodule Handling in Discovery

**Date**: 2026-08-24
**Status**: ACCEPTED
**Deciders**: Phase 1B implementation
**Resolves**: OQ-004

---

## Context

The Phase 1B discovery pipeline walks a repository's working tree and builds a
`WorkspaceSnapshot` containing a `SourceRevision` and a per-file manifest.  Git
submodules are directories inside the parent working tree that are themselves
independent Git repositories.  Two treatment strategies were possible:

1. **Opaque subtree** — treat the submodule directory as ordinary files inside
   the parent walk; include it in the parent `SourceRevision` manifest.
2. **Separate repository** — refuse to descend into submodule directories during
   the parent walk; represent each submodule as its own `core_repositories` row
   with its own `SourceRevision`.

---

## Decision

**Each Git submodule is treated as a separate repository with its own
`SourceRevision`.**

Concretely:

- During the `ignore`-crate walk of the parent repository, any path that
  resolves to a directory containing its own `.git` file or `.git/` directory is
  treated as a nested repository root and is **not descended into** by the parent
  walk.
- The discovery engine records the submodule path and its `HEAD` SHA as a
  separate `core_repositories` entry in the storage layer.
- Each submodule has its own `WorkspaceSnapshot`, manifest hash, and
  `SourceRevision`.
- The parent `WorkspaceSnapshot` includes a `submodule_revisions` field
  (Vec<(String, String)> — relative path + HEAD SHA) so the parent manifest
  hash still changes when any submodule advances.
- Uninitialized submodules (no `.git` file inside) are treated as an empty
  directory and do not produce a `core_repositories` row.

---

## Rationale

| Criterion | Opaque subtree | Separate repository |
|-----------|---------------|---------------------|
| Manifest hash accuracy | ❌ Hashes submodule object files that are opaque binary blobs | ✅ Each submodule's hash computed from its own working tree |
| Storage efficiency | ❌ Duplicate indexing of shared library code | ✅ One row per repo; content deduplicated via file hash |
| Cross-repository navigation | ❌ Not possible — symbols are mixed into parent namespace | ✅ Symbols are queryable per-repository or across workspace |
| Phase 1B scope | ⚠️ Simpler to implement but creates debt for Phase 2+ | ✅ Correct boundary from the start |

The "separate repository" model aligns with how Git itself models submodules and
with the storage contract's `core_repositories` multi-row design.

---

## Consequences

- `attic-discovery` `walk.rs` detects submodule roots actively: for every
  directory entry encountered during a walk pass, it checks whether
  `abs_path.join(".git").exists()`.  If `true`, the directory is treated as a
  nested repository boundary — a `DiagnosticKind::SubmoduleDetected` diagnostic
  is emitted, the directory's repo-relative prefix is recorded in
  `submodule_prefixes`, and all files underneath it are skipped for the
  remainder of that pass.  This explicit check is necessary because the `ignore`
  crate does **not** automatically stop at `.git` boundaries inside a walk that
  was started from a parent root.
- `WorkspaceSnapshot` gains a `submodule_revisions` field in Phase 1B.  If the
  field is empty the parent hash is computed as before; if non-empty the
  submodule SHAs are included in the BLAKE3 hash input.
- Phase 2 indexing must iterate `submodule_revisions` and enqueue each submodule
  for its own index pass.
- The `source_revision.md` contract §2.5 is updated to document submodule
  semantics.

---

## Alternatives Rejected

**Merge submodule content into parent manifest**: rejected because the submodule
working tree is a separate `HEAD` and its files change on a different cadence
than the parent.  Mixing them makes incremental invalidation impractical.

**Ignore submodules entirely**: rejected because a workspace containing
submodules would silently miss large portions of its own codebase.
