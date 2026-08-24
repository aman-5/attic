# Contract: Stable Identity Model

## Purpose

Define how Attic assigns stable logical identities to files and symbols across
renames, moves, re-clones, branch changes, parser changes, and re-segmentation.
Row IDs in SQLite are internal implementation details; they are never exposed as
identities outside the storage layer.

---

## Definitions

### FileIdentity

A `FileIdentity` represents the logical concept of "this file" independent of
where it currently lives.

```
FileIdentity {
  id             : Uuid    -- stable opaque identifier
  repository_id  : Uuid    -- foreign key → Repository
  stable_id_basis: String  -- see §File Identity Basis below
}
```

### FileOccurrence

A `FileOccurrence` records a specific observed state of a logically identified
file at a given source revision.

```
FileOccurrence {
  id               : Uuid
  file_identity_id : Uuid    -- foreign key → FileIdentity
  source_revision_id: Uuid   -- foreign key → SourceRevision
  path             : String  -- normalized repo-relative path (UTF-8, forward slashes)
  content_hash     : String  -- BLAKE3 hex
  size_bytes       : i64
  language         : Option<String>
  file_type        : String  -- see FileType enum
  discovery_class  : String  -- see DiscoveryClass enum
  security_state   : String  -- see SecurityState enum
  existence_state  : String  -- ACTIVE | DELETED | EXCLUDED | INACCESSIBLE | TOO_LARGE | BINARY | SECRET_REDACTED | PARSER_FAILED
}
```

### SymbolIdentity

A `SymbolIdentity` represents the logical concept of "this named symbol" within
a language and repository, independent of its current location.

```
SymbolIdentity {
  id             : Uuid
  repository_id  : Uuid
  language       : String  -- e.g., "java", "python", "rust"
  qualified_name : String  -- language-normalized fully-qualified name
  kind           : String  -- FUNCTION | CLASS | INTERFACE | CONSTANT | TYPE | MODULE | etc.
  disambiguator  : Option<String> -- used when qualified_name alone is ambiguous
}
```

### SymbolOccurrence

A `SymbolOccurrence` records a specific observed definition or significant
declaration of a symbol at a given source location.

```
SymbolOccurrence {
  id                 : Uuid
  symbol_identity_id : Uuid
  file_occurrence_id : Uuid
  source_revision_id : Uuid
  source_span        : String   -- "start_line:start_col–end_line:end_col"
  signature          : Option<String>
  visibility         : Option<String>
  is_definition      : bool
}
```

---

## Invariants

1. `FileIdentity.id` is never reused, even if the file is deleted and later
   re-created at the same path. Re-creation produces a new `FileIdentity`.

   Exception: if content_hash and path match exactly within a short time window,
   Attic MAY reuse the identity with an explicit `REUSE_EXACT` confidence flag.
   This is conservative; implementations should default to new identity.

2. `SymbolIdentity.id` is never reused after a symbol is fully removed.

3. Two `SymbolIdentity` records with the same `(repository_id, language,
   qualified_name, kind)` are the same symbol if there is no ambiguity.
   `disambiguator` resolves overloads or other cases where the tuple is
   non-unique (e.g., Java method overloads).

4. Identity matching confidence MUST be explicit. The system never silently
   promotes a HEURISTIC match to EXACT.

5. A `FileOccurrence` with `existence_state = DELETED` invalidates all dependent
   derived artifacts (symbols, relationships, retrieval units) from prior
   revisions. It does not delete the `FileIdentity`.

6. Cross-occurrence continuity (linking occurrences across revisions) is a
   separate heuristic step; it does not mutate identity records themselves.

---

## File Identity Basis

### Primary basis: Git rename tracking

When a file is tracked by Git:
- On initial discovery, `stable_id_basis = git:<blob-sha>:<initial-path>`.
- When Git reports a rename (`R<score>` in `git diff --name-status`), the
  existing `FileIdentity` is linked to the new path via a new `FileOccurrence`.
  The identity itself is not re-created.

### Secondary basis: content hash continuity

When Git rename tracking is absent (non-Git repo, or rename not detected):
- If a file disappears at path A and a new file appears at path B with
  `content_hash_similarity >= 0.90` (Jaccard on 4-gram character sets), a
  HEURISTIC rename link is created with `confidence = HEURISTIC`.
- `stable_id_basis = content:<content_hash>:<initial-path>`.

### Default basis: path

When neither Git rename nor content similarity applies:
- `stable_id_basis = path:<normalized-path>`.
- A file at a new path always produces a new `FileIdentity`.

---

## Symbol Identity Continuity

### Exact continuity

A symbol is the same across revisions if:
- `(repository_id, language, qualified_name, kind, disambiguator)` matches, AND
- at least one `FileOccurrence` with that symbol exists in the new revision.

Confidence: `EXACT`.

### Heuristic continuity

If `qualified_name` changes (e.g., rename refactor):
- Heuristic match uses structural similarity of the symbol's containing scope
  and signature.
- Confidence: `HEURISTIC`.
- Must not be treated as definitively the same symbol in evidence or answer
  claims.

### No continuity

If neither exact nor heuristic criteria are met, the old symbol is treated as
`DELETED` and the new symbol is a distinct `SymbolIdentity`.

---

## Rename / Copy / Move Cases

| Case | File Identity | Symbol Identity |
|------|--------------|----------------|
| Rename (Git-tracked) | Same identity, new occurrence | Same if qualified_name unchanged |
| Rename (no Git) | New identity with HEURISTIC link | HEURISTIC if name unchanged, else new |
| Copy (new path, same content) | New identity (copy is new file) | New occurrences; original unaffected |
| Move + rename | Treat as rename of file; symbol continuity per above |
| Re-clone (same content) | Content-hash match → reuse identity (EXACT) | Same if qualified_names unchanged |
| Branch switch | New `SourceRevision`; file occurrences may reuse identities | Symbol occurrences linked to new revision |
| Parser change | Identities unchanged; new occurrences replace old for that revision | Symbol occurrences rebuilt |
| Re-segmentation | Retrieval unit IDs reset; file/symbol identities unchanged | |

---

## Confidence Levels

```
EXACT       -- deterministic match (Git SHA, identical qualified name)
HEURISTIC   -- plausible match via similarity; never claimed as certain
NONE        -- no continuity; fresh identity
```

Confidence MUST appear on all cross-revision identity links, not just internally.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Git rename detection errors | Fall back to path-based identity; log diagnostic |
| Content similarity timeout | Fall back to path-based identity; log diagnostic |
| Ambiguous symbol (same qualified_name, multiple files) | Both are valid occurrences of same identity; `disambiguator` populated |
| Completely unparseable file | `FileOccurrence.existence_state = PARSER_FAILED`; no `SymbolOccurrence` created |

---

## Observability

Identity resolution produces a structured log per file/symbol processed:

```
file_path
identity_id
continuity_kind: EXACT | HEURISTIC | NEW
confidence
prior_path (if rename)
prior_identity_id (if reuse)
```

---

## Examples

### File renamed from `src/Foo.java` to `src/bar/Foo.java` (Git-tracked)

```
FileIdentity { id: "fi-001", stable_id_basis: "git:abc123:src/Foo.java" }

Revision 1:
  FileOccurrence { file_identity_id: "fi-001", path: "src/Foo.java", ... }

Revision 2 (after git rename):
  FileOccurrence { file_identity_id: "fi-001", path: "src/bar/Foo.java", ... }
  -- Same identity; new occurrence
```

### Symbol renamed `FooService` → `BarService` (no Git tracking)

```
Old: SymbolIdentity { qualified_name: "com.example.FooService", kind: "CLASS" }
New: SymbolIdentity { qualified_name: "com.example.BarService", kind: "CLASS" }
  -- New identity if no structural heuristic fires
  -- HEURISTIC link if structural similarity >= threshold
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| ID-01 | Same file, same content, two revisions | Same `FileIdentity`; two `FileOccurrence` records |
| ID-02 | File renamed, Git-tracked | Same `FileIdentity`; new `FileOccurrence` with new path |
| ID-03 | File content changed, same path | Same `FileIdentity`; new `FileOccurrence` with new `content_hash` |
| ID-04 | File deleted, new file at same path | New `FileIdentity`; old occurrence `DELETED` |
| ID-05 | File copied to new path | New `FileIdentity` for copy; original unchanged |
| ID-06 | Symbol unchanged across two revisions | Same `SymbolIdentity`; two `SymbolOccurrence` records |
| ID-07 | Symbol deleted in new revision | Old `SymbolOccurrence` has no match; identity preserved |
| ID-08 | Symbol rename (HEURISTIC) | New `SymbolIdentity`; link marked `HEURISTIC` |
| ID-09 | Overloaded Java methods (same qualified_name) | Same identity; `disambiguator` populated |
| ID-10 | File in non-Git repo, no rename | Path-based identity; new identity on path change |
| ID-11 | Re-clone: same content at same path | EXACT content-hash match; identity reused |
| ID-12 | Parser change | File/symbol identities unchanged; occurrences rebuilt |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| ID-Q1 | Minimum content-similarity threshold for HEURISTIC rename: 0.90 Jaccard 4-gram — is this correct for typical refactoring patterns? | No — can be tuned; safe to use 0.90 as provisional |
| ID-Q2 | For Java overloads, should `disambiguator` use full parameter types or just arity? | No — use full parameter types for precision |
| ID-Q3 | Should re-creation at same path within one minute use `REUSE_EXACT` or always new identity? | No — default to new identity; conservative |
