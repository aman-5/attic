# Contract: Analyzer API

## Purpose

Define the input, output, capability levels, diagnostics, cancellation,
resource limits, generic fallback, and version identity for the Analyzer
Registry and all individual analyzers. Every file passes through an analyzer;
no eligible file is silently dropped.

---

## Definitions

### AnalyzerCapabilities

Each analyzer declares which capabilities it provides and at what level.

```
AnalyzerCapabilities {
  analyzer_id       : String
  version           : String   -- semver
  supported_languages: Vec<String>  -- e.g., ["java", "python"]
  supported_extensions: Vec<String> -- e.g., [".java", ".py"]
  capability_levels : HashMap<CapabilityKind, CapabilityLevel>
}
```

### CapabilityKind

```
LEXICAL             -- tokenization and text extraction
STRUCTURAL_PARSE    -- produces a parse tree / AST
SYMBOL_EXTRACTION   -- extracts named symbols (functions, classes, etc.)
IMPORT_EXTRACTION   -- extracts import/require/use statements
REFERENCE_EXTRACTION -- extracts call sites, usages
RELATIONSHIP_RESOLUTION -- resolves references to target symbols/files
BUILD_RESOLUTION    -- understands build system dependencies
SEMANTIC_RESOLUTION -- framework/domain-specific intelligence
```

### CapabilityLevel

```
NONE       -- 0: not provided
BASIC      -- 1: generic/heuristic support
PARTIAL    -- 2: incomplete but useful
FULL       -- 3: complete for this language
```

Numerical summary:
```
Level 0: generic lexical/search support only
Level 1: structural parsing
Level 2: symbols and imports
Level 3: references and resolution
Level 4: package/build awareness
Level 5: framework/domain-specific intelligence
```

---

## Analyzer Input

```
AnalyzerInput {
  file_occurrence_id : Uuid
  path               : String   -- normalized repo-relative path
  content            : AnalyzerContent
  language_hint      : Option<String>
  file_type          : FileType
  size_bytes         : u64
  cancellation_token : CancellationToken
  resource_budget    : ResourceBudget
}

AnalyzerContent
  = FullBytes(Vec<u8>)            -- for normal-sized files
  | StreamingHandle(ReadHandle)   -- for large files (see large_files contract)
  | RedactedBytes(Vec<u8>)        -- content with secret spans replaced
```

---

## Analyzer Output

```
AnalyzerOutput {
  analyzer_id       : String
  analyzer_version  : String
  file_occurrence_id: Uuid
  structural_nodes  : Vec<StructuralNodeSpec>
  symbols           : Vec<SymbolSpec>
  imports           : Vec<ImportSpec>
  relationships     : Vec<RelationshipSpec>
  retrieval_units   : Vec<RetrievalUnitSpec>
  diagnostics       : Vec<AnalyzerDiagnostic>
  fallback_used     : bool  -- true if generic analyzer was used
  capability_used   : CapabilityLevel
}
```

An empty `structural_nodes`, `symbols`, etc. is valid output. An analyzer MUST
always produce a result; it MUST NOT panic or return an unstructured error.

### StructuralNodeSpec

```
StructuralNodeSpec {
  temp_id           : String   -- local ID for parent linking within this output
  parent_temp_id    : Option<String>
  node_type         : String
  structural_identity: String
  source_span       : SourceSpan
  content_hash      : String
  metadata_json     : Option<String>
}
```

### SymbolSpec

```
SymbolSpec {
  qualified_name  : String
  kind            : String
  disambiguator   : Option<String>
  source_span     : SourceSpan
  signature       : Option<String>
  visibility      : Option<String>
  is_definition   : bool
  node_temp_id    : String   -- must match a StructuralNodeSpec.temp_id
}
```

### ImportSpec

```
ImportSpec {
  import_path     : String   -- as written in source
  resolved_path   : Option<String>  -- if resolvable at analysis time
  source_span     : SourceSpan
  import_kind     : String   -- STATIC | DYNAMIC | REQUIRE | USE | etc.
}
```

### RelationshipSpec

```
RelationshipSpec {
  source_entity_temp_id : String
  target_path_or_symbol : String
  rel_type              : String
  dependency_basis      : String
  resolution            : String
  confidence            : f64
}
```

### RetrievalUnitSpec

```
RetrievalUnitSpec {
  retrieval_text  : String   -- pre-secret-scanned; must not contain secret bytes
  node_temp_ids   : Vec<String>
}
```

### AnalyzerDiagnostic

```
AnalyzerDiagnostic {
  severity : FATAL | ERROR | WARNING | INFO
  code     : String   -- e.g., "PARSE_ERROR", "SYMBOL_AMBIGUOUS"
  message  : String   -- human-readable; MUST NOT contain secret content
  span     : Option<SourceSpan>
}
```

---

## Analyzer Registry

The `AnalyzerRegistry` maps files to analyzers.

Selection algorithm:

```
1. Check security_state: if FORBIDDEN, reject before registry lookup.
2. Detect language from extension and optional content sniffing.
3. Look up specialized analyzer for (language, file_type).
4. If found and capabilities >= BASIC: use specialized analyzer.
5. If specialized analyzer fails (returns FATAL diagnostic or panics):
     → fall back to GenericAnalyzer immediately
     → record FALLBACK_USED diagnostic
6. If no specialized analyzer: use GenericAnalyzer directly.
```

### GenericAnalyzer

The `GenericAnalyzer` MUST handle every file that reaches it without panicking.

Capabilities:
- LEXICAL: FULL (line-based tokenization + text extraction)
- All others: NONE

Output:
- One or more `RetrievalUnitSpec` containing the file's text (or sections
  of large files), line-chunked.
- No symbols, no structural nodes.

This ensures every eligible file is searchable regardless of language support.

---

## Cancellation

Each analyzer receives a `CancellationToken`. The analyzer MUST check it at
regular intervals (at minimum once per 1,000 AST nodes processed or equivalent
work unit).

If cancelled:
1. Stop analysis immediately.
2. Return partial output with `diagnostics` containing a `CANCELLED` entry.
3. Partial output is not persisted; the file is rescheduled.

The cancellation token is set by the task system when:
- The parent task is cancelled by the operator.
- The resource budget (see §Resource Limits) is exhausted.
- The system is shutting down.

---

## Resource Limits

```
ResourceBudget {
  max_memory_bytes  : u64   -- default: 256 MB per file
  max_time_ms       : u64   -- default: 30,000 ms (30 seconds) per file
  max_ast_nodes     : u64   -- default: 1,000,000 nodes
  max_recursion_depth: u32  -- default: 500
}
```

If any limit is exceeded:
1. The analyzer is cancelled (cancellation token set).
2. The file receives `existence_state = PARSER_FAILED` with a
   `RESOURCE_EXHAUSTED` diagnostic.
3. The `GenericAnalyzer` processes the file as a fallback to preserve
   text searchability.

Resource limits are configurable per file type and analyzer in the
Attic workspace configuration.

---

## Version Identity

Each analyzer has a `VERSION` constant (semver string) compiled into the binary.
This is recorded in `IndexGeneration.analyzer_versions_json`.

When an analyzer version changes:
- All artifacts for files handled by that analyzer are invalidated.
- See compatibility contract for scope and rebuild rules.

The `analyzer_id` is a stable string identifier for the analyzer
(e.g., `"java-treesitter"`, `"python-treesitter"`, `"generic"`).
It never changes for the lifetime of the analyzer. A replacement analyzer
for the same language uses a distinct `analyzer_id`.

---

## Invariants

1. Every eligible file produces either an `AnalyzerOutput` or a
   `PARSER_FAILED` state — never silence.
2. `AnalyzerOutput.retrieval_units` MUST NOT contain secret content.
   This is enforced by the pipeline before the analyzer is called
   (redacted content is passed in) and verified at output.
3. An analyzer MUST NOT execute repository content as code or shell commands.
4. An analyzer MUST NOT access paths outside the `AnalyzerInput.path`'s
   repository root.
5. An analyzer MUST NOT make network requests.
6. `fallback_used = true` is always observable in diagnostics.
7. The `GenericAnalyzer` never fails fatally; it always produces output.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Specialized analyzer returns FATAL | GenericAnalyzer runs; FALLBACK_USED diagnostic |
| Specialized analyzer panics (caught) | GenericAnalyzer runs; FALLBACK_USED + PANIC_CAUGHT diagnostic |
| GenericAnalyzer fails | File: PARSER_FAILED; error logged; no panic propagated |
| Memory limit exceeded | RESOURCE_EXHAUSTED; file: PARSER_FAILED after generic fallback |
| Time limit exceeded | RESOURCE_EXHAUSTED; cancellation; file: PARSER_FAILED after generic fallback |
| Secret content in output | Output rejected; file: SECRET_REDACTED; secret pipeline error |

---

## Observability

Per-file analysis log entry:

```
file_path
analyzer_id
analyzer_version
capability_level_used
fallback_used: bool
structural_nodes_produced
symbols_produced
retrieval_units_produced
diagnostics_count: { FATAL, ERROR, WARNING, INFO }
duration_ms
memory_peak_bytes
```

---

## Examples

### Java file: full analysis

```
Input: src/main/java/com/example/FooService.java
Analyzer: java-treesitter v1.0.0
Output:
  structural_nodes: [ClassDecl, MethodDecl×5, FieldDecl×3]
  symbols: [FooService(CLASS), doFoo(FUNCTION), ...]
  imports: [com.example.BarClient, java.util.List, ...]
  retrieval_units: [class body chunk, method chunk×5]
  fallback_used: false
  capability_used: Level 3
```

### Unknown format: generic fallback

```
Input: config/deployment.hcl (no HCL analyzer registered)
Analyzer: generic v1.0.0
Output:
  structural_nodes: []
  symbols: []
  retrieval_units: [text chunk×3]  -- line-chunked
  fallback_used: true (no specialized analyzer)
  capability_used: Level 0
```

### Parser crash: safe fallback

```
Input: src/corrupt.py (malformed syntax)
Analyzer: python-treesitter v1.0.0 → FATAL: ParseError
Fallback: generic v1.0.0
Output:
  structural_nodes: []
  symbols: []
  retrieval_units: [text chunk]
  fallback_used: true
  diagnostics: [PARSE_ERROR(FATAL), FALLBACK_USED(INFO)]
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| AZ-01 | Java file with known analyzer | Symbols and structural nodes produced |
| AZ-02 | Unknown extension | GenericAnalyzer used; file still searchable |
| AZ-03 | Analyzer panics | Panic caught; GenericAnalyzer fallback; FALLBACK_USED |
| AZ-04 | File exceeds memory budget | RESOURCE_EXHAUSTED; GenericAnalyzer fallback |
| AZ-05 | File exceeds time budget | Cancellation; RESOURCE_EXHAUSTED; GenericAnalyzer fallback |
| AZ-06 | Cancellation during analysis | Partial output discarded; file rescheduled |
| AZ-07 | SECRET_REDACTED file reaches analyzer | Redacted content analyzed; no secret bytes in output |
| AZ-08 | GenericAnalyzer on large text file | Line-chunked retrieval units produced; no panic |
| AZ-09 | Analyzer version bumped | IndexGeneration updated; prior artifacts invalidated |
| AZ-10 | Two analyzers claim same language | Registry priority rules; deterministic selection |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| AZ-Q1 | When two analyzers support the same language (e.g., generic + specialized), should priority be explicit in config or hardcoded? | No — hardcoded: specialized > generic; configurable override in Phase 3 |
| AZ-Q2 | Should analyzer output include raw AST nodes for debugging, or only the processed specs? | No — processed specs only for V1; raw AST optional debug mode in later phase |
| AZ-Q3 | Should Tree-sitter grammars be bundled in the binary or loaded from disk at runtime? | No — bundled for V1 (Phase 3); runtime loading would require safe loading contract |
