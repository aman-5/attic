# ADR-011: Relationship Persistence Semantics for Phase 3

**Status:** Accepted (Phase 3)

## Context

`core_relationships.target_entity_id` is declared without a foreign key (by
design in migration 0001), while its comment describes entity references.
Phase 3 must persist import edges whose targets frequently do NOT exist as
entities (external crates, npm packages, JDK classes) without fabricating
ghost entities, and must never present syntactic edges as resolved.

## Decision

1. **Resolved edges** store a real entity UUID in `target_entity_id`
   (`target_entity_type='FILE_OCCURRENCE'`). Import edges resolve to the
   defining FILE occurrence; heritage/call edges originating at a symbol use
   `source_entity_type='SYMBOL_OCCURRENCE'` via `source_symbol_index`.
2. **Unresolved edges** keep `target_entity_type='FILE_OCCURRENCE'` and store
   a deterministic logical id `"logical:" <blake3(target)[..32]>`. The raw
   target string is preserved in `provenance_json.logical_target` /
   `specifier`. `resolution` stays `SYNTACTIC` with confidence ≤ 0.7.
3. **Resolution honesty ladder** (contract enum): analyzers emit only
   `SYNTACTIC`; the indexing resolver may upgrade to `PACKAGE_RESOLVED`
   (manifest layout evidence) or `SYMBOL_RESOLVED` (known symbol definition).
   `BUILD_RESOLVED` / `FRAMEWORK_RESOLVED` are never claimed in Phase 3.
4. **Replacement semantics**: republication deletes all prior nodes, symbol
   occurrences, unit↔node links AND relationships anchored at an occurrence
   (either endpoint, file-level) inside the same transaction, so ghost edges
   cannot survive edits.

## Consequences

- No schema migration needed.
- Unresolved-but-preserved edges give Phase 6 cross-repository work a stable
  join key (the logical id).
- Consumers MUST treat `logical:` ids as non-resolvable placeholders.

## Alternatives considered

- Null-target rows — rejected (column NOT NULL, loses evidence).
- Ghost FILE_OCCURRENCE rows for external packages — rejected: pollutes file
  inventory and freshness machinery.
