# ⏱️ Attic — Performance and sizing

What to expect when Attic indexes your repositories, and how to measure it on
your own code. Every number here is a direct measurement unless marked
**estimate**.

Reference machine: 14-core / 20-thread laptop CPU (Intel i7-1370P), 32 GB RAM,
4 GB laptop GPU (NVIDIA RTX A500), Windows, SSD.

## Contents

- [The short version](#the-short-version)
- [Indexing (lexical + structural)](#indexing-lexical--structural)
- [Embeddings (semantic layer)](#embeddings-semantic-layer)
- [Sizing your own workspace](#sizing-your-own-workspace)
- [Tuning](#tuning)
- [Benchmark your own repositories](#benchmark-your-own-repositories)
- [Not yet measured](#not-yet-measured)

## The short version

| You want | Typical time |
|---|---|
| Search a freshly added repository | Seconds to about a minute — lexical and structural indexing is fast |
| See an edit reflected | Well under a second of work per changed file, after a short debounce |
| Full semantic coverage | Background work: minutes on a GPU, hours on a CPU for large repositories |

Search never waits for embeddings: results are lexical until vectors exist,
then semantic candidates join automatically. The embedding queue is persisted,
so progress survives restarts.

## Indexing (lexical + structural)

Cold database, production pipeline, optimized build:

| Corpus | Size | Result |
|---|---|---|
| Synthetic mixed-language benchmark (the default bench corpus) | 800 files, 112,784 retrieval units | **21.8 s** cold · 26.0 s full re-index of the unchanged tree · **4.1 s** incremental republish of 300 edited files |
| JSON environment exports | 18 files, 21 MB (five ~4.5 MB exports, 3 unsupported DOCX/PDF reported) | **9.6 s** · 45,110 units collapsing to 10,596 distinct bodies (77% canonical dedup) |
| Enterprise multi-repository workspace (AEM + Java) | 20 nested repositories, ~224k files on disk, 12,161 eligible | **≈ 1 min** · 95,886 units |

Where the time goes (synthetic benchmark, cold): per-file analysis ≈ 53%,
database publication ≈ 44%, discovery ≈ 2%. Analysis runs on all but two
logical CPUs (largest files first, so one huge file never serializes the tail);
publication goes through the single writer in large prepared-statement
batches.

## Embeddings (semantic layer)

Model: `Qwen/Qwen3-Embedding-0.6B`. Throughput is real (unpadded) tokens per
second; parity is the minimum cosine similarity against the Hugging Face
reference implementation.

| Backend | Throughput | Parity |
|---|---|---|
| GPU — ONNX Runtime / DirectML, fp16 (Windows GPU build) | **3,130 tok/s** | 0.99996 |
| CPU — Candle f32 (8-item batches, 1.6 KB chunks) | **62 tok/s** | batched = unbatched (cosine 1.000000) |
| CPU — Candle Q8 GGUF | 16 tok/s | 0.9992 |
| GPU — DirectML Q8 / int8 exports | slower than fp16 | 0.75 / 0.72 — **rejected** |

Batching never changes a vector: batches are right-padded with a causal
attention mask, and the benchmark asserts batched and single-item vectors
match. The model is a one-time ~1.2 GB download, verified against pinned
SHA-256 hashes; it is never counted in indexing time.

### GPU batching (ONNX / DirectML)

- **Length buckets.** Inputs are grouped by token length into buckets of
  64/128/256/… (up to `onnx_seq_len`) and padded only to their bucket, not
  to the full window. Code chunks are mostly short, so this removes most
  padding work. Vectors are unchanged (cosine ≥ 0.9999 vs fixed-512 padding).
- **Token budget per pass.** Each forward pass carries
  `gpu_batch_tokens / bucket` items (power-of-two sizes, so DirectML sees a
  small fixed set of shapes and its memory arena stops growing).
- **Stays on the GPU.** Shapes that already ran are always admitted; a new
  shape runs only if free VRAM covers it, otherwise the pass shrinks. An
  out-of-memory pass is retried at half size — never demoted to CPU.
- **Thermal guard.** At `gpu_temp_pause_c − 1` the token budget halves; at
  `gpu_temp_pause_c` (default 90 °C) embedding pauses until the GPU cools to
  `gpu_temp_resume_c` (default 85 °C). Sensor: `nvidia-smi` (NVIDIA on
  Windows/Linux) or Linux hwmon; macOS and other Windows adapters have no
  readable sensor, so the OS's own thermal management applies.
- **Poison isolation.** If a multi-item batch fails on content or crashes
  the worker, it is split in halves until the offender is found: good items
  commit, only the offender is marked failed, so one bad chunk never stalls
  the queue.
- **Lazy load, idle unload.** The model worker starts only when chunks are
  pending or a semantic query arrives — never at server startup, and never
  when `[semantic] enabled = false`. After `gpu_idle_unload_secs` (default
  900) with no embedding work the worker process exits, so the OS reclaims
  all of its VRAM (including the driver's pool). The next chunk or query
  reloads it (≈2–7 s on an RTX A500); a query's time budget is extended by
  the load time, so a cold query never times out. `status` →
  `semantic_identity.worker` shows `not loaded` / `loading` / `loaded (load
  took …)` / `unloaded (idle 15m)` and the last-use time.
- **GPU eligibility, decided once at startup.** The adapter DirectML will
  use (high-performance order, so the discrete GPU on hybrid laptops) is
  checked before any model download: less dedicated VRAM than
  `gpu_min_vram_mb` (default 4096; a nominal 4 GB card reporting ≈3.9 GB
  qualifies) or an integrated GPU (unless `allow_integrated_gpu = true`)
  means CPU from the start. `status` → `semantic_identity.device` says which
  and why, e.g. `GPU: NVIDIA RTX A500 Laptop GPU (3965 MB)` or
  `CPU: GPU … has 2048 MB VRAM < gpu_min_vram_mb=4096`. If the model then
  fails to load on an eligible GPU, the CPU fallback takes over and the line
  reads `CPU: GPU failed at runtime: …`.

## Sizing your own workspace

**Estimate** the semantic workload from the text that will actually be
embedded:

1. Start from source bytes, minus what `.gitignore`, built-in skips,
   `[indexing] exclude`, `[semantic] exclude_globs` and
   `[semantic] max_file_bytes` (256 KiB default) remove. Identical JSON
   subtrees are embedded once.
2. Tokens ≈ bytes ÷ 4 for code and prose.
3. Time ≈ tokens ÷ throughput, plus roughly 50% for tokenization, queueing
   and vector commits.

| Unique tokens | GPU (fp16) | CPU (f32) |
|---|---|---|
| 1 M (a large service) | ≈ 8–12 min | ≈ 6–7 h |
| 10 M | ≈ 1.5 h | ≈ 2.5–3 days |
| 40 M (a big multi-repo estate) | ≈ 5–6 h | not practical — use the GPU build or narrow `exclude_globs` |

## Tuning

| Goal | Change |
|---|---|
| Faster embeddings on Windows | Build with `--features ort-directml --target x86_64-pc-windows-msvc`. The fp16 ONNX export downloads automatically on first run; `ATTIC_ONNX_MODEL_DIR` is only needed to point at your own export |
| Faster embeddings on Apple Silicon | Build with `--features candle-metal` (the default for `aarch64-apple-darwin` release builds) |
| Faster embeddings on Linux + NVIDIA | Build with `--features candle-cuda` on a machine with the CUDA toolkit installed |
| Less embedding work | `[semantic] exclude_globs` for generated, vendored or snapshot data; lower `max_file_bytes`; raise `min_score` (default 0.30) |
| More semantic coverage | Lower `[semantic] min_score` (0.0 embeds every eligible unit) and raise `max_units_per_repo` (default 2560) / `max_units_total` (default 100000) |
| Bigger GPU passes on a larger card | Raise `[semantic] gpu_batch_tokens` (default 4096, sized for a 4 GB card) |
| GPU running hot | Lower `[semantic] gpu_temp_pause_c` / `gpu_temp_resume_c` (defaults 90 / 85 °C) |
| Free GPU memory sooner / never | `[semantic] gpu_idle_unload_secs` (default 900; 0 keeps the model resident) |
| Force CPU, or try a small / integrated GPU | `[semantic] gpu_min_vram_mb` (default 4096; set above your VRAM to force CPU, 0 to always try) and `allow_integrated_gpu` (default false) |
| Faster GPU embeddings, less coverage | `[semantic] onnx_seq_len = 512`. Halves the padded window, but also halves the largest unit that can be embedded — units above the new ceiling are excluded from selection and counted as `exceeds_max_input_bytes`, not embedded. Leave unset (1024) unless you have measured the trade |
| Keep the laptop responsive | `[resources] mode = "low"`, or lower `[indexing] analysis_threads` |
| Index many repositories faster | `[resources] mode = "performance"` or a higher `scheduler_workers` |
| Smaller semantic database | `[semantic] dimension = 512` (re-embeds once) |
| Lexical-only (no model at all) | `ATTIC_SEMANTIC=0` or `[semantic] enabled = false` |

The `status` tool reports semantic progress (`semantic_progress`) and a
plain-language bottleneck diagnosis (`diagnostics.why_slow`).

## Benchmark your own repositories

Both benchmarks are opt-in and read-only: they never modify the directory
they measure.

```powershell
# Indexing: cold, warm and incremental timings with a per-stage breakdown
$env:ATTIC_BENCH_INDEX = '1'
$env:ATTIC_BENCH_ROOT  = 'C:\code\my-repo'      # omit for the synthetic corpus
cargo test --release -p attic-indexing --test index_throughput_bench -- --ignored --nocapture

# Embeddings: real-model CPU throughput and batch equivalence (needs the cached model)
$env:ATTIC_BENCH_QWEN        = '1'
$env:ATTIC_BENCH_QWEN_CORPUS = 'C:\code\my-repo' # omit to sample Attic's own sources
cargo test --release -p attic-semantic --test qwen3_throughput_bench -- --ignored --nocapture
```

On macOS/Linux use `export ATTIC_BENCH_INDEX=1` and so on. Further knobs:
`ATTIC_BENCH_THREADS`, `ATTIC_BENCH_REPLICAS`, `ATTIC_BENCH_INCR_FILES`,
`ATTIC_BENCH_QWEN_ITEMS`, `ATTIC_BENCH_QWEN_BATCH` and
`ATTIC_BENCH_QWEN_CHUNK_BYTES` (see the header of each benchmark file).

## Not yet measured

- A complete GPU embedding drain of a large workspace end to end through the
  MCP server — the GPU times above are computed from measured throughput.
- p95 query latency under concurrent MCP load while indexing.
