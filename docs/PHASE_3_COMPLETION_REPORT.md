# Phase 3 Completion Report — Structural Intelligence

**Date:** 2026-08-26
**Status:** GATE PASSED — STOP (Phase 4 NOT started)

---

## 1. Structural framework architecture

```text
AnalyzerRegistry
   ├── GenericAnalyzer                      (universal L0 fallback, unchanged)
   └── structural layer (attic-analyzers::structural)
        ├── Tree-sitter ENGINE              (shared parser mechanics)
        │     parser creation · grammar registration · parsing ·
        │     cancellation · time/AST-node/depth budgets · traversal
        │     accounting · span conversion · malformed-tree handling ·
        │     diagnostics · canonical node production
        └── LanguageSpec adapters           (per-language knowledge ONLY)
              JavaSpec · PythonSpec · GoSpec · JavaScriptSpec · TypeScriptSpec
```

- Parser mechanics live in ONE place (`structural/mod.rs`, `engine::run`);
  languages supply only grammar, file types, capability matrix and
  extraction rules via the `pub(crate) trait LanguageSpec`.
- Adding a language = grammar crate + spec module + registry line.
  Proven executable: a mock NON-tree-sitter language registers through the
  generic registry and dispatches with zero central changes
  (`tests/phase3_extensibility.rs`).

## 2. Parser backend abstraction

Tree-sitter types (`Node`, `Tree`) never leave the `structural` module.
Storage/indexing/retrieval/server/incremental depend only on canonical
`AnalyzerOutput` specs; a manifest-scanning test enforces that no other
crate gains a tree-sitter dependency. A future non-TS backend implements
`trait Analyzer` directly, exactly like the mock language.

## 3. Registered languages & capability matrix (declared independently)

| Capability            | Java | Python | Go | JS | TS |
|-----------------------|------|--------|----|----|----|
| StructuralParse       | Full | Full   | Full | Full | Full |
| SymbolExtraction      | Full | Full   | Full | Full | Full |
| ImportExtraction      | Full | Full   | Full | Full | Full |
| ReferenceExtraction   | Basic (intra-file) ×5 |
| RelationshipResolution| Basic ×5 |
| BuildResolution       | None (layout evidence only, resolver-side) |
| SemanticResolution    | not declared |

## 4. Extraction per language (grounded in probe + node-types.json)

- **Java:** package/class/interface/enum/methods/ctors/fields;
  static+wildcard imports; extends/implements (incl. `generic_type`
  bases); final→Constant; overload disambiguation `overload:N`.
- **Python:** imports/from-imports (relative prefixes preserved as
  `<module>:<name>`), decorated defs (span includes decorators),
  async fns, nested functions, module UPPER_CASE constants, base classes.
- **Go:** package clause, grouped/single imports (+alias), funcs, methods
  with receivers (`pkg.T.Name`), structs/interfaces/type aliases,
  const/var, interface method signatures (`is_definition=false`),
  intra-file constructor calls.
- **JS:** ESM default/named/namespace, `require()`, dynamic `import()`,
  `export…from`; classes/private `#fields`/getters; arrow functions;
  exported→public; nesting-aware qnames; calls to local symbols →
  CALL/SymbolResolved, to imported bindings → REFERENCES/Syntactic.
- **TS:** everything JS plus interfaces (+member signatures),
  type aliases, enums(+members), namespaces, abstract classes and
  abstract-method signatures, type-only imports, implements targets
  through `generic_type`.

## 5. Symbols/imports/references persistence

Canonical mapping into existing Phase-1A tables (no migration needed):
`core_structural_nodes` (rename-stable BLAKE3 identity basis, content hash,
parent chain), `core_symbol_identities` (idempotent upsert on
repo+language+qname+kind+disambiguator) + definition occurrences,
`core_relationships` (rel_type/basis/resolution/confidence/provenance),
`core_retrieval_unit_nodes` navigation links.

## 6. Resolution behaviour (honesty ladder)

Analyzer edges are SYNTACTIC only. Indexing resolver upgrades strictly on
evidence: Java layout+known-class → SYMBOL_RESOLVED 0.95 / PACKAGE 0.85;
Go module-prefix (go.mod read, ≤64 KiB) → PACKAGE_RESOLVED/GO_MODULE 0.9;
Python relative/dotted vs layout → PYTHON_PACKAGE 0.85; JS/TS relative
probing (12 suffix patterns) → IMPORT-basis PACKAGE_RESOLVED 0.8;
heritage EXTENDS/IMPLEMENTS resolved to defining occurrence → 0.9.
Unresolved edges persist with deterministic logical ids (ADR-011) at
confidence ≤0.7 — never reported resolved.

## 7. GenericAnalyzer fallback / security / LARGE / budgets

- Malformed-but-parseable → partial specialized output + non-fatal
  PARSE_ERROR warning (file keeps rich output).
- Grammar-init failure / AST-node budget exceeded → Error diagnostic →
  dispatcher routes to GenericAnalyzer (searchability preserved).
- Panic safety unchanged (catch_unwind in dispatch).
- Content exclusively via Phase-1B pipeline; redacted bytes proven never to
  reach units/symbols/relationships/diagnostics (analyzer-level AND e2e).
- LARGE files: bounded ~4 MiB prefix parse + STRUCTURAL_TRUNCATED warning +
  remainder consumed as lexical units (coverage ≥ generic); unit caps emit
  RESOURCE_EXHAUSTED warnings.

## 8. Incremental invalidation integration

Scoped re-analysis publishes replacement structural artifacts atomically:
links cleaned first, then nodes/symbols/relationships deleted (both
endpoints for file-anchored rels), then fresh rows inserted CURRENT inside
the writer transaction. Untouched files keep byte-identical rows
(test-proven). DAG propagation now covers FILE_OCCURRENCE-anchored
relationship edges; audit-record closure includes them.
ANALYZER_REGISTRY subsystem version recorded per generation
(`0.2.0`, delta-invalidation wiring deferred → OQ-020).

## 9. Tests (33 new; workspace total 426 PASS / 0 FAIL)

| Suite | Count | Covers |
|---|---|---|
| phase3_smoke | 2 | all-language happy paths |
| phase3_language_matrix | 11 | §16 matrix ×5 langs: valid/malformed/incomplete/empty/comments-code-like/spans(CRLF,no-newline,unicode)/redaction/cancel/AST-budget/determinism |
| phase3_language_specific | 5 | overloads, aliases, import forms, receivers, private fields, interfaces/enums/namespaces/signatures |
| phase3_large_files | 1 | bounded prefix + truncation observable + full coverage |
| phase3_extensibility | 6 | §10 architecture proofs |
| phase3_structural (indexing) | 7 | e2e java/go/python/ts resolution, single-file scoping, ghost removal, secret e2e |
| phase3_benchmark | 1 | §18 gate |

Fixtures: `tests/fixtures/{OrderService.java,sample.py,server.go,widget.js,widget.ts}`.

## 10. Benchmark results (fixture corpus, committed metrics)

| metric                | baseline (L0) | structural |
|-----------------------|---------------|------------|
| symbol definitions    | 0             | 10         |
| resolved imports      | 0             | 4          |
| resolved references   | 0             | 1          |
| navigation links      | 0             | 8          |
| FTS hits (8 queries)  | 13            | 17 (≥ baseline) |

## 11. Commands executed

```
cargo fmt --all -- --check                                    PASS
cargo check --workspace --target x86_64-pc-windows-msvc        PASS
cargo clippy --workspace --all-targets --all-features
     --target x86_64-pc-windows-msvc -- -D warnings            PASS
cargo test --workspace --target x86_64-pc-windows-msvc         426 PASS / 0 FAIL
```

## 12. Hangs/timeouts/security events

None. No endpoint-security events. No stale processes killed. The LARGE test
runs ~10 s by design (5 MiB fixture) within its budget.

## 13. Open questions / known limitations

- **OQ-019**: Maven/Gradle build-aware Java resolution (BUILD_RESOLVED level).
- **OQ-020**: automatic analyzer-version delta invalidation wiring.
- TSX shares the TS grammar; `.jsx` maps to the JS analyzer (no JSX-specific
  extraction).
- Enum members model as Constant under a Class-kind parent (no dedicated
  SymbolKind.Enum — contract enum is fixed).
- Interface method elements / abstract signatures persist as
  `is_definition=false` occurrences (API-surface signals).
- Cross-file CALL resolution is intentionally deferred (references stay
  intra-file or import-bound) — Phase 6 territory.

## 14. Gate status

**PHASE 3 COMPLETE.** Stopping here; Phase 4 requires explicit approval.
