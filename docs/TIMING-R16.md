# Timing Rebaseline (r16) — measured, not promised

All numbers below were produced on the acceptance machine: Intel i7-1370P
(14C/20T), 31.66 GB RAM, NVIDIA RTX A500 Laptop GPU (4 GB), Windows.
Everything here is either a direct measurement or a computed estimate whose
inputs are direct measurements (labeled ESTIMATE where so).

## Measured — lexical + structural indexing (production pipeline, cold DB)

| Corpus | Content | Runs | Result |
|---|---|---|---|
| Dump (`C:\Users\amanbansal\Desktop\Dump`) | 18 files / 21 MB (5 env JSON exports ~4.5 MB each, 2 DOCX + 1 PDF unsupported-reported) | 8.8s, 16.4s, 13.1s, 11.7s | **median ≈ 13 s**, 45,105 units, 10,587 distinct canonical bodies (76.5% dedup collapse) |
| HDFC workspace (`C:\Adobe-Projects\HDFC-Bank-on-prem\HDFC Repo`) | 224k raw files, 20 nested git repos indexed as separate roots | 54.2s, 39.3s | **≈ 40–54 s**, 7,382 eligible files, 71,004 units |

Original targets: Dump lexical ≤ 5 min and HDFC lexical ≤ 5 min — **both met
by an order of magnitude** (seconds, not minutes).

## Measured — embedding throughput (Qwen3-Embedding-0.6B)

| Backend | Throughput | Parity vs HF reference |
|---|---|---|
| ORT/DirectML fp16 (RTX A500) | **3,130 tok/s** | min cosine 0.99996 |
| ORT/DirectML official Q8 (`model_quantized.onnx`) | 740 tok/s | min cosine 0.752 — **rejected** |
| ORT/DirectML official int8 QOperator (`model_int8.onnx`) | (aborted — slower) | min cosine 0.721 — **rejected** |
| Candle Q8 GGUF (CPU) | 16.3 tok/s | min cosine 0.9992 |
| Candle fp32 safetensors (CPU fallback) | ≤ Q8 GGUF class | reference-exact |

Quantized GPU variants of this model are both lower quality and slower under
DirectML — fp16 is the correct GPU artifact; Q8 belongs to the CPU path.

## Computed — full semantic coverage (ESTIMATE from measured throughput)

- **Dump** (10,587 unique canonical units ≈ 1.08 M unique tokens):
  - GPU (DirectML fp16): ~6 min pure inference → **≈ 8–12 min end-to-end
    warm-model** (tokenization, queue, vector commit included).
    Original ≤ 3 min target: **not met**; the honest target is ≈ 10 min.
  - CPU: ~18 h — CPU semantic on the full Dump is not a practical target.
- **HDFC** (~42 M semantic tokens at full coverage):
  - GPU: **≈ 3.8–4.5 h**. Original ≤ 20 min target: not achievable on a 4 GB
    RTX A500; it would require ~11× the measured throughput.
  - CPU: days — not a target.

## Excluded from indexing time (per contract)

Model download/verification is reported separately and never counted in
warm-model indexing time. On first run it is a one-time ~1.2 GB download
plus SHA-256 verification.

## Reproduce

```powershell
# Dump / HDFC lexical acceptance + timings
$env:ATTIC_ACCEPTANCE_DUMP='1'; cargo test -p attic-indexing --test real_corpus_acceptance dump_corpus -- --nocapture
$env:ATTIC_ACCEPTANCE_HDFC='1'; cargo test -p attic-indexing --test real_corpus_acceptance hdfc_workspace -- --nocapture
# DirectML provider throughput/parity harness (spike methodology):
# files\qwen-ort-spike in the session workspace, --target x86_64-pc-windows-msvc
```

## What remains unmeasured

- A live full semantic drain (GPU) of Dump/HDFC through the MCP server — the
  per-phase estimates above are computed from the measured 3,130 tok/s and
  the measured unique-content counts, not a single end-to-end run.
- p95 latency under concurrent MCP query load during indexing.
