# Contract: Large-File Behavior

## Purpose

Define how Attic handles files that exceed normal processing size limits,
ensuring that large files never cause memory exhaustion, parser hangs, or
system instability, while remaining searchable. No eligible file becomes
invisible solely because of its size.

---

## Definitions

### File Size Tiers

```
SMALL     < 256 KB     -- full content loaded; normal analysis
MEDIUM    256 KB – 4 MB -- full content loaded; structural analysis with node limits
LARGE     4 MB – 50 MB  -- streaming/chunked; coarse structural scan only
VERY_LARGE > 50 MB     -- metadata + sampling; no full parse; content indexed in regions
```

Thresholds are configurable in workspace configuration. Defaults above are
for V1 on the minimum hardware profile (8 cores / 16 GB).

### FilePolicy

Assigned during the file pipeline before any analyzer is invoked.

```
FilePolicy
  NORMAL                  -- SMALL or MEDIUM; standard analyzer pipeline
  HIERARCHICAL_LARGE_FILE -- LARGE; streaming structural scan + region map
  METADATA_ONLY           -- VERY_LARGE or binary; metadata and sampled text only
  IGNORED                 -- excluded by discovery (not a large-file policy)
```

### RegionMap

An ordered list of byte-range descriptors produced by a coarse scan of a
large file.

```
RegionMap {
  file_occurrence_id : Uuid
  regions            : Vec<Region>
}

Region {
  region_index  : u32
  start_byte    : u64
  end_byte      : u64
  region_type   : String  -- e.g., "FUNCTION", "CLASS", "SECTION", "CHUNK"
  label         : Option<String>
  nesting_depth : u32
}
```

---

## Size-Aware Processing Pipeline

```
File
  |
  v
Size measurement (stat, no read)
  |
  +-- SMALL / MEDIUM: load full content → normal analyzer pipeline
  |
  +-- LARGE:
  |     |
  |     v
  |   [1] Metadata scan (path, extension, size)
  |   [2] Coarse/streaming structural scan (line-by-line or block-by-block)
  |   [3] Build RegionMap
  |   [4] Select high-value regions for deeper parsing (up to MAX_DEEP_REGIONS)
  |   [5] Parse selected regions
  |   [6] Produce hierarchical retrieval units per region
  |
  +-- VERY_LARGE:
        |
        v
      [1] Metadata scan
      [2] Sample first MAX_SAMPLE_BYTES and last MAX_SAMPLE_BYTES
      [3] Produce METADATA_ONLY retrieval unit with path + size + sampled text
      [4] existence_state = TOO_LARGE (informational)
      [5] FileOccurrence recorded; no structural nodes or symbols
```

---

## Processing Limits

All limits are applied per file. Exceeding any limit triggers the behavior
in §Resource Enforcement.

```
MAX_FULL_LOAD_BYTES    = 4 MB          (MEDIUM threshold)
MAX_DEEP_REGIONS       = 20            (regions parsed deeply in LARGE tier)
MAX_REGION_PARSE_BYTES = 512 KB        (max bytes parsed per deep region)
MAX_SAMPLE_BYTES       = 8 KB          (bytes sampled from start/end for VERY_LARGE)
MAX_STREAMING_LINES    = 500,000       (coarse scan line limit)
MAX_NESTING_DEPTH      = 500           (recursion depth cap for structural scan)
MAX_MEMORY_PER_FILE    = 256 MB        (analyzer memory budget)
MAX_PARSE_TIME_MS      = 30,000        (30 seconds per file; from analyzer contract)
```

All configurable in workspace configuration. Defaults here are V1 minimums.

---

## Streaming Large-File Scan

For LARGE-tier files, the coarse scan operates line-by-line or block-by-block
without loading the full file into memory.

Algorithm:
```
1. Open file as streaming reader.
2. Scan for structural boundaries:
   - Language-aware: function/class/section headers (regex or simple heuristic).
   - Language-agnostic: large blank-line clusters, heading patterns, XML tags.
3. Record byte offsets for each detected boundary.
4. Build RegionMap from detected boundaries.
5. Sort regions by estimated value (heuristic: shorter = more dense).
6. Select top MAX_DEEP_REGIONS regions for deeper parsing.
7. Seek to each selected region's byte offset; parse up to MAX_REGION_PARSE_BYTES.
8. Produce one RetrievalUnit per region.
```

Memory usage during streaming scan must not exceed MAX_MEMORY_PER_FILE at any
point. The scanner must track peak memory and abort if limit approached.

---

## Very Large File (METADATA_ONLY) Handling

Files in the VERY_LARGE tier:

1. Stat the file; record `size_bytes`, `path`, `content_hash` (streaming BLAKE3).
2. Read the first `MAX_SAMPLE_BYTES` and last `MAX_SAMPLE_BYTES`.
3. Produce one `RetrievalUnit` with:
   - `retrieval_text = "[LARGE FILE: <path> (<size> bytes)]\n<first_sample>\n...\n<last_sample>"`
4. Set `existence_state = TOO_LARGE`.
5. No structural nodes, no symbols.
6. File remains searchable via path and sampled content.

---

## Example: Large XML File (150 MB)

```
File: data/catalog.xml (150 MB)
Policy: METADATA_ONLY (> 50 MB threshold)

Result:
  RegionMap: not produced (METADATA_ONLY)
  RetrievalUnit: 1 unit with header + 8 KB sample
  existence_state: TOO_LARGE
  Searchable: yes (path, sampled content)
  Symbols: none
  Structural nodes: none
```

## Example: Large Generated Source (8 MB Java)

```
File: generated/protobuf/BigProto.java (8 MB)
Policy: HIERARCHICAL_LARGE_FILE (4–50 MB)

Result:
  Coarse scan: detects 120 class/method declarations
  RegionMap: 120 regions
  Deep parsing: top 20 regions (MAX_DEEP_REGIONS)
  RetrievalUnits: 20 (one per deep region) + 1 summary unit
  existence_state: ACTIVE
  discovery_class: LOW_PRIORITY (generated/)
  Symbols: from 20 parsed regions only
```

---

## Invariants

1. No file causes unbounded memory growth in the analyzer or pipeline.
2. Every eligible file — regardless of size — produces at least one
   `RetrievalUnit` and is therefore text-searchable.
3. `VERY_LARGE` files are never fully loaded into memory.
4. LARGE-tier streaming scan never exceeds `MAX_MEMORY_PER_FILE`.
5. The coarse scan never executes file content as code.
6. Deep-region parsing respects the same `ResourceBudget` as normal files
   (see analyzer contract).
7. A file whose streaming scan exceeds `MAX_PARSE_TIME_MS` is cancelled;
   it falls back to METADATA_ONLY handling.

---

## Interaction with Secret Scanning

Secret scanning for LARGE and VERY_LARGE files:

- LARGE: secret scanner operates on the streaming content. Each chunk is
  scanned before being passed to the analyzer. Redacted spans are tracked
  per-chunk.
- VERY_LARGE: only the sampled bytes (first + last MAX_SAMPLE_BYTES) are
  scanned. If secrets are found in the sample, the file is `SECRET_REDACTED`.
  If the body (unsampled) is not scanned, a `PARTIAL_SECRET_SCAN` diagnostic
  is recorded.

---

## Interaction with Invalidation

Large-file artifacts (RegionMap, RetrievalUnits) participate in the
invalidation DAG exactly as normal-file artifacts:

- Content hash change → all regions invalid → rescan.
- Segmentation version change → retrieval units invalid; region map preserved
  if structural scan version unchanged.
- Analyzer version change → deep-parsed regions invalid; coarse scan
  preserved if the coarse scanner version is unchanged.

---

## Failure Behavior

| Failure | Behavior |
|---------|----------|
| Streaming scan OOM | Cancel; fall back to METADATA_ONLY; `RESOURCE_EXHAUSTED` diagnostic |
| Streaming scan timeout | Cancel; fall back to METADATA_ONLY; `PARSE_TIMEOUT` diagnostic |
| File grows during scan (race) | Complete scan with observed content; `UNSTABLE_CAPTURE` diagnostic |
| File shrinks to zero during scan | Mark `INACCESSIBLE`; diagnostic |
| Deep-region parse fails | Skip that region; log `REGION_PARSE_FAILED`; other regions unaffected |
| BLAKE3 streaming hash error | Capture fails; file marked `INACCESSIBLE` |

---

## Observability

Per-file processing log:

```
file_path
size_bytes
policy_applied: NORMAL | HIERARCHICAL_LARGE_FILE | METADATA_ONLY
regions_detected (LARGE only)
regions_deep_parsed (LARGE only)
retrieval_units_produced
peak_memory_bytes
duration_ms
fallback_reason (if applicable)
```

---

## Test Matrix

| Test ID | Scenario | Expected |
|---------|----------|----------|
| LF-01 | 100 KB file | Policy: NORMAL; full analysis |
| LF-02 | 2 MB file | Policy: NORMAL (MEDIUM); full analysis with node limits |
| LF-03 | 8 MB Java file | Policy: HIERARCHICAL_LARGE_FILE; region map; 20 deep regions |
| LF-04 | 200 MB XML file | Policy: METADATA_ONLY; sampled; existence_state: TOO_LARGE |
| LF-05 | Large file with secret in sample | SECRET_REDACTED; PARTIAL_SECRET_SCAN diagnostic |
| LF-06 | Streaming scan exceeds time limit | Fallback to METADATA_ONLY; PARSE_TIMEOUT diagnostic |
| LF-07 | Streaming scan exceeds memory limit | Fallback to METADATA_ONLY; RESOURCE_EXHAUSTED diagnostic |
| LF-08 | Large file, all 20 regions parse-fail | 0 deep regions; 1 summary RetrievalUnit; diagnostics |
| LF-09 | Very large binary file | Policy: METADATA_ONLY (binary); no text scan |
| LF-10 | Large file content changes between captures | Content hash changes; all region artifacts invalidated |

---

## Unresolved Questions

| ID | Question | Blocking? |
|----|----------|-----------|
| LF-Q1 | Should the RegionMap itself be persisted in the DB for incremental large-file updates? | No — not for V1; recomputed on change |
| LF-Q2 | Should VERY_LARGE threshold be per-file-type (e.g., 4 MB for source, 50 MB for data)? | No — single threshold for V1; per-type in later phase |
| LF-Q3 | For PARTIAL_SECRET_SCAN on VERY_LARGE files, should the entire file be SECRET_REDACTED conservatively? | No — record diagnostic; index sample; operator can configure to always SECRET_REDACT if preferred |
