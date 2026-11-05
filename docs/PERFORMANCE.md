# Attic performance and sizing

Attic performs local lexical and graph retrieval over managed per-repository
SQLite indexes. Embeddings, model inference, GPU acceleration and vector search
are not part of the current product. This guide describes the current work
paths and how to measure them; it does not carry forward measurements from the
removed embedding pipeline.

## What affects latency

| Operation | Main cost drivers | Notes |
|---|---|---|
| MCP initialize and tool discovery | Process startup and daemon relay | Independent of repository-index readiness |
| Store verification | SQLite file size, storage speed, integrity-check work | Large stores can take longer than a client's connection deadline; content tools remain unavailable until verification completes |
| Initial indexing | Repository file count/size, analyzer complexity, available CPU, SQLite writes | Each repository has an isolated index; unchanged analysis may be reused |
| Incremental indexing | Number and size of changed files, analysis and writer queue | Watch events are debounced and verified against content |
| `context` retrieval | Lexical candidate count, graph expansion, evidence verification, requested mode | Workspace-wide managed queries process up to four repositories concurrently; each repository still has its own mode budget. `SEARCH` returns lexical candidates; answer modes assemble and validate evidence |
| Cross-repository synchronization | Active repository count, manifests and dependency relationships | Synchronization applies to configured repositories only |

MCP responsiveness is not a readiness guarantee. During startup, `initialize`,
tool discovery and `status` can respond while stores are verified. Workspace
actions and content requests wait until catalog verification completes. Do not
retry content requests in a tight loop; check `status` and retry after the
relevant repository is ready. A 1–2 GB store may require more than 30 seconds
to verify, depending on storage and SQLite work.

## Retrieval characteristics

`context(mode="SEARCH")` performs SQLite FTS5 lexical retrieval and returns
bounded source matches without evidence assembly. Other `context` modes combine
lexical candidates with symbols, explicit graph relationships, dependency
edges and project knowledge, then apply freshness and evidence checks.

`SEARCH` input is treated as literal text: each whitespace-separated token is
quoted before FTS5 matching, so dots, colons and other punctuation (for example
`attic.toml` or `FormCode: value`) are not parsed as FTS5 syntax or column
filters. Each `SEARCH` FTS query is interrupted after 3 seconds. Answer modes
install a SQLite progress-handler deadline matching the mode budget
(`FAST` 300 ms, `NORMAL` 3 s, `DEEP` 30 s per repository). SQLite checks this
deadline periodically during execution. This does not bound connection
acquisition, source verification, writer waits, workspace-wide work or client
processing. Interruptions can degrade a retrieval step or surface a tool error;
they are not an end-to-end response-time guarantee.

This is not vector similarity. Exact identifiers, source phrases and explicit
relationships are useful query anchors; broad conceptual queries may retrieve
differently than they did with embedding-based search. No model or GPU setting
improves retrieval because no model runtime is used.

## Response size and answer quality

Workspace answer modes return unique content once rather than repeating complete
answers under `repository_answers`. That field now contains `repository_id` or
`scope` and a `summary` with the result, confidence, insufficiency reason,
plan ID, evidence counts and timings. Consumers of the former nested `answer`
field must use the top-level `context`, `claims` and `evidence` instead.

Byte-identical excerpt bodies are shared, including across repositories.
Every repository/path/span/evidence citation and its freshness/authority header
is retained above the shared excerpt. Different excerpt bodies are never
merged, and contradiction disclosures remain visible. Claims carry
`evidence_ids`; top-level evidence records retain source-revision/snapshot
provenance and source spans. No additional global byte cap or confidence-based
repository exclusion is applied; existing per-mode retrieval budgets remain.

Shared bodies are explicitly labeled. Contradiction disclosures and withheld
notices carry repository scope, including when multiple repositories use the
same relative path. Excerpt coordinates are separate from retrieval-unit
coordinates: focusing does not change candidate fusion identity. Omission
notices appear outside source fences and never enter live-source verification.
Unicode case-insensitive focusing maps folded matches back to original source
byte offsets and preserves character-column coordinates.

Rejected items are omitted from ordinary answers, not from the persisted plan.
`evidence_dropped` remains an array (empty by default);
`evidence_dropped_summary` always reports `count`, `by_reason` and
`details_included`. Pass `include_diagnostics: true` to include every individual
rejection. This option does not change retrieval or evidence selection.

Lexical excerpts retain the existing approximately 1,200-byte budget but focus
on the line matching the most query terms instead of always taking the start
of a retrieval unit. Returned spans identify the excerpt; use `file` with
`repository_id`, `path`, `start_line` and `end_line` to expand source context.
Extensionless legal files (`LICENSE`, `LICENCE`, `NOTICE`, `COPYING`,
`COPYRIGHT`) are documentation, not implementation evidence; they remain
searchable. These are targeted relevance fixes, not a claim of vector recall
or a broadly benchmarked ranking improvement.

## Measuring query latency

For answer modes, inspect the response's `telemetry`:

- `response_bytes`: exact UTF-8 bytes of the JSON text inside the MCP content
  block; excludes the outer JSON-RPC envelope and transport escaping.
- `serialization_ms`: first encoding pass, before adding size/timing fields.
  Final encoding and transport/client work are not included.
- `retrieval_ms` and `phases`: preparation/retrieval time and recorded pipeline
  step durations. Workspace responses include these in each repository summary.
- Managed workspace responses also include `fanout_ms`, `catalog_ms`,
  `aggregation_ms`, `server_processing_ms`, `context_bytes_before_sharing` and
  `context_bytes`. The total is measured before final response encoding.
  `context_bytes_before_sharing` sums original, unshared context documents;
  it does not sum already-compacted repository responses.

Managed-tool logs include `queue_ms` (waiting for a blocking worker) and
`execution_ms` (managed handler, including encoding), with no query/source
content added to timing logs. Enable persistent info-level logging with
`logging(action="on", level="info")` when diagnosing these waits.

Compare FAST, NORMAL and a repository-scoped call using the same query. Record
client wall-clock duration separately. Large payloads can add client ingestion
cost, but server timing and response bytes alone cannot prove the cause of an
entire slow agent answer. These metrics do not measure model inference,
client retries, tool chaining or network/transport delivery.

## Measuring indexing

Run the opt-in indexing benchmark against a repository. It reads the source
tree and reports indexing timings; it does not measure large-store startup
verification or user-query p95 latency.

<details open>
<summary><b>PowerShell</b></summary>

```powershell
$env:ATTIC_BENCH_INDEX = '1'
$env:ATTIC_BENCH_ROOT = 'C:\code\my-repo'
cargo test --release -p attic-indexing --test index_throughput_bench -- --ignored --nocapture
```

</details>

<details>
<summary><b>Linux / macOS shell</b></summary>

```sh
ATTIC_BENCH_INDEX=1 ATTIC_BENCH_ROOT=/code/my-repo \
  cargo test --release -p attic-indexing --test index_throughput_bench -- --ignored --nocapture
```

</details>

Omit `ATTIC_BENCH_ROOT` to use the benchmark's synthetic corpus. Treat results
as local measurements: filesystem cache state, CPU, SQLite storage, corpus
composition and build profile affect timings. Do not compare them directly
with measurements from older releases unless corpus, hardware and benchmark
procedure match.

## Sizing and diagnosis

- Index size scales with indexed files, retrieval units, symbols,
  relationships and evidence. Each configured repository has a separate
  `data/repositories/<stable-id>/index.db`; `data/catalog.db` stores workspace
  membership and bounded cross-repository metadata.
- For slow startup, use `status` to separate catalog/store verification from
  indexing and cross-repository synchronization. Keep the daemon running
  while verification proceeds.
- For slow retrieval, compare `context` modes and inspect response `telemetry`
  and managed-tool timing logs; `SEARCH` excludes evidence assembly, while
  answer modes include graph expansion and evidence validation.
- For indexing pressure, inspect `status.resource_pressure`,
  `status.incremental.tasks` and `scheduler_workers` in `attic.toml`. Reduce
  background concurrency only when resource pressure or contention warrants
  it.
- Legacy `attic.db`, `semantic.db` and model files are preserved, not used by
  current retrieval, and not automatically removed. They do not improve
  indexing or search performance.

No current end-to-end query-latency or large-store verification benchmark is
published here. Measure those separately on representative hardware before
setting a production startup deadline or making performance claims.
