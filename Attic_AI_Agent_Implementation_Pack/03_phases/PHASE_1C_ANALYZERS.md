# Phase 1C — Analyzer Foundation

## Goal
Make every eligible textual file searchable without requiring a language-specific parser.

## Step 1 Analyzer API
Implement only the Phase 0 contract.

Outputs must use canonical model types, not parser-library-specific structs.

## Step 2 AnalyzerRegistry
Responsibilities:
- analyzer registration;
- capability advertisement;
- deterministic selection;
- version identity;
- fallback selection.

## Step 3 GenericAnalyzer
This is mandatory before specialized analyzers.

It must:
- handle decodable text;
- create bounded hierarchical/region units;
- preserve source offsets;
- avoid giant chunks;
- obey cancellation and size budgets;
- emit diagnostics.

## Step 4 Basic structured/document analyzers
Implement only formats actually needed for Phase 1 benchmark, potentially Markdown and common JSON/YAML/XML configuration, using verified parsers or safe structural logic.

SQL is not to be treated as generic configuration if richer parsing is later required.

## Step 5 Specialized language analyzers
Phase 1 may include minimal high-priority analyzers only if needed. Deep Tree-sitter intelligence belongs to Phase 3.

## Fallback invariant
Any specialized analyzer failure on eligible text:
```text
diagnostic
→ GenericAnalyzer
→ lexical searchable output
```

## Gate
Unknown/custom text is searchable. Malformed specialized input does not disappear.
