# Phase 3 — Structural Intelligence

## Goal
Add richer language intelligence incrementally without making parser availability a search prerequisite.

## Analyzer capability ladder
```text
L0 generic lexical
L1 structure
L2 symbols/imports
L3 references/resolution
L4 package/build awareness
L5 framework/domain intelligence
```

## Initial priority
Java, Python, Go, JavaScript, TypeScript.

This is not a language limit.

## Tree-sitter workflow per language
1. select verified grammar crate/version;
2. add fixtures;
3. inspect actual parse trees;
4. define canonical node mapping;
5. define symbol/import extraction;
6. test malformed/incomplete source;
7. test large source;
8. add relationship resolution separately;
9. benchmark.

Never assume node names from another grammar/language.

## Relationship resolution
Preserve:
- dependency basis;
- resolution type;
- confidence;
- provenance;
- revision.

Examples:
syntactic, package_resolved, symbol_resolved, build_resolved, framework_resolved, inferred.

## Gate
Each added capability must improve its benchmark slice without harming generic fallback.
