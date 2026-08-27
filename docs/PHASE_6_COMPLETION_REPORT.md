# Phase 6 Completion Report — Cross-Repository Intelligence

**Status:** COMPLETE — all gates green.
**Baseline:** Phases 0–5 merged, full workspace compiles, 76 crossrepo tests pass (68 unit + 8 E2E).

## 1. Architecture

`attic-crossrepo` crate delivers bounded cross-repository intelligence:

```text
manifest parsing → workspace catalog → resolver → edges → traversal → impact
```

All edges live in `core_relationships` (`rel_type='DEPENDS_ON'`,
`source_repository_id != target_repository_id`), reusing Phase 4's graph
expansion and freshness handling unchanged. Cross-repo intelligence is
**evidence expansion, never truth**.

Server integration: `sync_workspace` runs at startup after bootstrap.
`RetrievalService` exposes `crossrepo_degraded: bool` — when true
(default until first successful sync), `CrossRepoGenerator` is skipped
with a warning.

## 2. Manifest parsers (§6)

Seven ecosystem parsers, all operating on UNTRUSTED bounded bytes:
- **Maven** — pom.xml XML tag scanner (groupId, artifactId, modules, dependencies)
- **Gradle** — build.gradle/settings.gradle keyword extraction
- **Go** — go.mod module/require/replace blocks
- **npm** — package.json dependencies/devDependencies/workspaces
- **Python** — pyproject.toml [project] dependencies + requirements.txt
- **Submodules** — .gitmodules [submodule] blocks
- **Generated API** — .proto import lines

All parsers are pure functions over `&str` with hard byte caps. Raw manifest
bytes never carry into derived state (secret material cannot leak).

**Parser bugs fixed during integration (pre-existing, never compiled):**
- `stripped_key`: failed on `key = value` (space before `=`)
- `go_replace_decl`: `normalize_relative` collapsed `..` segments, losing
  path semantics needed by the resolver
- `parse_gitmodules`: `strip_prefix('=')` failed on `path = value` format
- `parse_requirements_txt`: `-e` flag caused entire line to be rejected

## 3. Workspace catalog (§6)

Derived per-repository catalog of provides + declarations, persisted in:
- `core_workspace_catalog` — provides JSON, BLAKE3 manifest hash, freshness
- `core_dependency_declarations` — parsed declarations per repo

Catalog is rebuilt at any time; never authoritative. `manifest_hash` (BLAKE3
over sorted path+content hashes) enables cheap incremental refresh detection.

Manifest reading uses `attic_discovery::read_bounded` (Phase 1B security)
with `FileTooLarge` error for oversized files. All parsers are pure functions
over `&str` with hard byte caps. Raw manifest bytes never carry into derived
state (secret material cannot leak).

## 4. Resolver (§7)

Evidence-first progressive resolution:
1. **BUILD_RESOLVED** — local path hints canonicalized against registered repos
2. **PACKAGE_RESOLVED** — ecosystem coordinate match to exactly one provider
3. **INFERRED** — module/import context (confidence ≤ 0.5, never "resolved")
4. **No edge** — name-only similarity across repos (anti-laundering)

Ambiguity stays ambiguity: >1 candidate provider = no edge, diagnostic only.
Zero candidates = missing-target diagnostic, never invented edge.

## 5. Graph budgets (§8)

`TraversalBudget` enforces per-walk bounds:
- `max_depth` (default 6)
- `max_repositories` (default 64)
- `max_edges` (default 2,000)
- `max_time_ms` (default 5,000)
- Cooperative cancellation via `CancelToken`

Cycle safety: each repository expanded at most once; BFS terminates.

## 6. Impact analysis (§9)

Four-level classification backed by edge freshness:
- `DIRECT_RESOLVED` — resolved edge, 1 hop
- `INDIRECT_RESOLVED` — fully-resolved path, >1 hop
- `POSSIBLE_INFERRED` — best path contains INFERRED segment
- `UNKNOWN` — path exists but freshness cannot support confident claim

STALE edges downgrade to UNKNOWN (never laundered into confident claims).

## 7. Maintenance (§12)

- `sync_repository` — scan + persist for one repo (fails with `NoSourceRevision` if unindexed)
- `sync_workspace` — reader phase (scan all) → resolver → writer phase (persist edges)
  - Two-stage architecture: reader phase on reader conn, writer phase on writer queue
  - Repos without `SourceRevision` are skipped gracefully (diagnostic recorded)
- `repository_removed` — cleanup all crossrepo state (catalog, declarations, edges)
- `incremental_sync` — manifest change detection → invalidates ALL stale outgoing edges
  for the affected repo → re-syncs catalog → re-resolves workspace → persists new edges.
  Handles edge removal correctly (old stale edges deleted before new ones inserted).
- `invalidate_edges_targeting` — mark consumer edges STALE on provider change

Writer-queue contract (Phase 1A): all writes happen in a single atomic
transaction via `WriterQueueHandle::send`.

**Key fix:** `incremental_sync` now bulk-deletes ALL stale outgoing edges for the
affected repository before re-resolution. Previously, only edges matching new resolved
identity were deleted, leaving orphaned stale edges when a dependency was removed.

## 8. Storage (§10)

Migration 0004 adds:
- `core_workspace_catalog` — UNIQUE on `repository_id`
- `core_dependency_declarations` — indexed on `repository_id` and `(ecosystem, name)`

`crossrepo_ops.rs` provides full CRUD: catalog upsert, declaration insert/delete,
edge insert/delete, repository removal, cross-edge enumeration for traversal.

## 9. Tests

76 tests across attic-crossrepo (68 unit + 8 E2E):
- **manifest** (11) — all parsers, path detection, oversized rejection, malformed degradation
- **resolver** (8) — empty workspace, single resolution, ambiguous/missing targets, ordering, limits
- **traversal** (8) — linear chains, depth enforcement, repo limits, cycles, cancellation, stale/invalid exclusion, bidirectional
- **impact** (8) — classification logic, rank ordering, DB-seeded integration
- **maintenance** (7) — incremental sync, invalidation, repository removal, catalog persistence
- **catalog** (10) — bounded reading, escape rejection, oversized detection, truncation
- **E2E** (8) — multi-repo sync, stale/invalid edge, removal cascade, integrated indexing→crossrepo→traversal, quality metrics, manifest change recomputation, missing SourceRevision fail-closed, manifest change edge removal via pipeline

## 10. Open questions resolved

- **Cross-repo edges in core_relationships**: confirmed — Phase 4 graph
  expansion picks them up without modification
- **Anti-laundering**: name similarity never produces edges; only explicit
  build/package evidence resolves
- **Secret material**: parsers never copy raw manifest bytes into outputs

## 11. What was NOT implemented (deferred)

- **Network/package manager execution**: all parsing is offline, static analysis only
- **Build script execution**: never executed for dependency discovery
- **True cross-repo symbol resolution**: beyond import/coordinate matching
- **WorkspaceSnapshot provenance table**: deferred (can be added later)
- **Resolution run audit trail**: deferred (can be added later)
- **SourceRevision fail-closed**: repos without indexed SourceRevision are skipped
  with `NoSourceRevision` error; the error is caught and logged, not propagated

## 12. Gate status

| Gate | Status |
|------|--------|
| Compiles | ✅ 0 errors workspace-wide |
| Tests | ✅ 76/76 pass (68 unit + 8 E2E) |
| Cross-repo benchmark | ✅ 6 benchmarks pass (~19µs resolver_10, ~29µs resolver_100, ~27µs resolver_1000, ~320µs traversal, ~202µs impact, ~51ms integrated) |
| Graph explosion prevention | ✅ Tested (depth/repo/edge/time limits) |
| Anti-laundering | ✅ Tested (ambiguous/missing targets produce no edges) |
| SourceRevision fail-closed | ✅ Tested (unindexed repos skipped gracefully) |
| Incremental edge removal | ✅ Tested (manifest change removes stale edges) |
| Clippy clean | ✅ No errors (pre-existing warnings only) |
