# Contract: SourceRevision and WorkspaceSnapshot

## Purpose

Define the exact semantics of a `SourceRevision` — the complete identification
of source state used to produce any derived artifact — and `WorkspaceSnapshot`,
the collection of per-repository source revisions representing a point-in-time
workspace view.

Every piece of derived intelligence (structural node, symbol, retrieval unit,
evidence, relationship) MUST be traceable to exactly one `SourceRevision`. This
makes all derived data reproducible, stale-detectable, and replaceable.

---

## Definitions

### SourceRevision

A `SourceRevision` uniquely identifies the source used to produce a set of
derived artifacts for one repository at one moment in time.

It captures:

- The HEAD commit SHA (or NULL if not a Git repository).
- A deterministic content-hash of all eligible working-tree files at capture
  time (the **working-tree manifest hash**).
- The hash of the effective discovery policy in force at capture time.
- The wall-clock instant at which capture completed.

```
SourceRevision {
  id                        : Uuid          -- stable opaque identifier
  repository_id             : Uuid          -- foreign key → Repository
  commit_sha                : Option<String> -- 40-char hex SHA-1, or NULL
  branch                    : Option<String> -- current branch name, or NULL
  working_tree_manifest_hash: String        -- see §Algorithm below
  discovery_policy_hash     : String        -- SHA-256 of canonical policy bytes
  captured_at               : i64           -- Unix timestamp (microseconds)
}
```

### WorkspaceSnapshot

A `WorkspaceSnapshot` is an ordered collection of `SourceRevision` IDs, one per
participating repository, recorded together so that queries can be answered
against a consistent logical snapshot.

```
WorkspaceSnapshot {
  id                 : Uuid
  created_at         : i64           -- Unix timestamp (microseconds)
  source_revision_ids: Vec<Uuid>     -- one per repository, ordered deterministically
}
```

**Atomicity note**: Repositories are captured sequentially; no cross-repository
filesystem atomicity is claimed. The snapshot records per-repository capture
times inside each `SourceRevision.captured_at`. Callers that require strict
simultaneous consistency must treat this as a best-effort approximation.

---

## Invariants

1. Every `SourceRevision` ID is globally unique and never reused.
2. `working_tree_manifest_hash` is computed from eligible-file content only;
   it does not include file metadata (mtime, permissions) that does not affect
   content.
3. `discovery_policy_hash` is computed from the canonical serialized form of the
   discovery policy, ensuring that a policy change produces a distinct revision.
4. Two `SourceRevision` records for the same repository are considered equivalent
   iff both `commit_sha` and `working_tree_manifest_hash` and
   `discovery_policy_hash` are equal.
5. A `WorkspaceSnapshot` never contains two `SourceRevision` IDs from the same
   repository.
6. Derived artifacts MUST record the `source_revision_id` that produced them.
   An artifact without a valid `source_revision_id` is treated as `INVALID`.

---

## Eligible-File Manifest Algorithm

The manifest hash covers exactly the files that the discovery policy would
include for indexing. The algorithm:

```
1. Apply discovery policy to the repository root to produce the ordered
   eligible-file list (see discovery contract).

2. For each eligible file, in deterministic order (UTF-8 path bytes,
   lexicographic ascending):
     a. Read file content bytes.
     b. Compute BLAKE3 hash of content bytes.
     c. Append to manifest input: path_bytes_len(4 BE) | path_bytes | hash(32).

3. Compute BLAKE3 hash of the full manifest input bytes.
   This is working_tree_manifest_hash.
```

Properties:
- Adding, removing, or renaming any eligible file changes the hash.
- Modifying any eligible file's content changes the hash.
- Files excluded by discovery policy do not affect the hash.
- The algorithm is deterministic across OS/locale given the same inputs.

---

## Content Hashing

Individual file hashes use BLAKE3 (32 bytes, hex-encoded for storage).

Rationale: BLAKE3 is fast, has no known collisions relevant to content
integrity, and has a maintained Rust crate (`blake3`). SHA-256 is the fallback
for contexts where an external system requires it.

---

## Normalized Path Encoding

All paths within a `SourceRevision` are:

1. Relative to the repository root.
2. UTF-8 encoded.
3. Forward-slash separated (`/`), regardless of host OS.
4. No leading slash; no `.` or `..` components.
5. Case-normalized according to the repository's declared case sensitivity
   (default: case-sensitive; case-insensitive repositories record normalized
   lowercase paths).

A path that cannot be normalized (e.g., contains null bytes, is non-UTF-8) is
treated as `INACCESSIBLE` and logged as a diagnostic, not silently dropped.

---

## File Mode Handling

File modes (execute bit, symlink, etc.) are NOT included in the content hash.
They are recorded as file metadata but do not influence `working_tree_manifest_hash`.

Rationale: Mode changes without content changes should not invalidate structural
analysis. Mode recording allows mode-sensitive operations without polluting the
content identity.

Symlinks are resolved according to the discovery contract (security boundary
applies). A symlink whose target is outside the allowed root is treated as
`INACCESSIBLE`.

---

## Dirty / Untracked / Deleted Files

In a Git repository, eligible files fall into categories:

| Git state       | Treatment                                         |
|-----------------|---------------------------------------------------|
| Tracked, clean  | Included per discovery policy                     |
| Tracked, modified (dirty) | Included; working-tree content used    |
| Untracked       | Included if discovery policy includes untracked   |
| Deleted (working tree) | Recorded as `DELETED`; excluded from hash  |
| Ignored by Git  | Excluded unless explicitly included by Attic policy |
| Submodule root  | Treated as a separate repository; not recursed    |

`commit_sha` reflects HEAD. If the working tree has modifications,
`working_tree_manifest_hash` will differ from what a clean checkout of HEAD
would produce. Both facts are recorded; callers can detect dirty state by
comparing the two.

---

## Repository Changes During Capture

The filesystem may change while capture is in progress. Attic does not lock the
repository.

Behavior:
- If a file is observed at path listing time but disappears before it can be
  read, it is treated as `DELETED` for this revision.
- If a file's content changes between listing and hashing, the read content is
  used, and a `UNSTABLE_CAPTURE` diagnostic is attached to the `SourceRevision`.
- The `UNSTABLE_CAPTURE` flag does not invalidate the revision; it signals that
  the next scheduled capture should re-examine this repository sooner.

---

## Retry / Unstable Behavior

If a capture produces `UNSTABLE_CAPTURE`:
1. Record the `SourceRevision` with the diagnostic.
2. Schedule a re-capture after a configurable stabilization delay
   (default: 2 seconds, configurable).
3. After re-capture, if the hash is stable, promote the new revision to current.
4. After a configurable maximum retry count (default: 3), accept the unstable
   revision and record an `UNSTABLE_FINAL` diagnostic for monitoring.

---

## Failure Behavior

| Failure                            | Behavior                                        |
|------------------------------------|-------------------------------------------------|
| Repository root inaccessible       | Capture fails; repository marked `INACCESSIBLE` |
| Git metadata unreadable            | `commit_sha = NULL`; capture continues          |
| Individual file unreadable         | File marked `INACCESSIBLE`; capture continues   |
| Discovery policy load failure      | Capture fails; error returned                   |
| Hash computation error             | Capture fails; error returned                   |

Capture failures are never silently swallowed. All failures produce diagnostics.

---

## Observability

Each `SourceRevision` capture produces a structured log entry including:

```
repository_id
commit_sha (or NULL)
working_tree_manifest_hash
discovery_policy_hash
eligible_file_count
inaccessible_file_count
deleted_file_count
unstable_capture: bool
duration_ms
captured_at
```

---

## Examples

### Clean Git repository

```
repository_id: a1b2...
commit_sha: "d4e5f6..."
branch: "main"
working_tree_manifest_hash: "ab12cd..."
discovery_policy_hash: "ff00ee..."
captured_at: 1724499600_000_000
```

### Dirty working tree (uncommitted edits)

```
repository_id: a1b2...
commit_sha: "d4e5f6..."     -- HEAD unchanged
branch: "main"
working_tree_manifest_hash: "99aabb..."  -- differs from clean checkout hash
discovery_policy_hash: "ff00ee..."
captured_at: 1724499700_000_000
```

### Non-Git workspace

```
repository_id: c3d4...
commit_sha: NULL
branch: NULL
working_tree_manifest_hash: "1234ab..."
discovery_policy_hash: "ff00ee..."
captured_at: 1724499800_000_000
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| SR-01 | Clean Git repo, capture twice unchanged | Same `working_tree_manifest_hash` both times |
| SR-02 | Modify one eligible file, recapture | Different `working_tree_manifest_hash` |
| SR-03 | Change discovery policy, recapture same files | Different `discovery_policy_hash` |
| SR-04 | Delete one eligible file, recapture | Different hash; deleted file in diagnostics |
| SR-05 | Non-UTF-8 filename in repo | File marked `INACCESSIBLE`; capture continues |
| SR-06 | Symlink pointing outside allowed root | File marked `INACCESSIBLE`; no path escape |
| SR-07 | Non-Git workspace | `commit_sha = NULL`; hash computed correctly |
| SR-08 | File disappears during capture | `DELETED`; no panic |
| SR-09 | Two revisions, same content, different policy | Different `discovery_policy_hash`; treated as distinct |
| SR-10 | `WorkspaceSnapshot` with 3 repos | 3 distinct `source_revision_ids`; `created_at` recorded |
| SR-11 | Dirty working tree | `working_tree_manifest_hash != clean_hash`; `commit_sha` = HEAD |
| SR-12 | Unstable file (changes during capture) | `UNSTABLE_CAPTURE` diagnostic recorded; re-capture scheduled |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| SR-Q1 | Should submodule roots be auto-discovered as separate repositories, or only when explicitly configured? | No — safe default: require explicit config |
| SR-Q2 | Case-insensitive repository detection: rely on filesystem probe or explicit config? | No — safe default: explicit config |
| SR-Q3 | Is BLAKE3 acceptable for compliance requirements (e.g., FIPS environments)? | No — BLAKE3 used internally; SHA-256 exposed for external contracts if needed |
