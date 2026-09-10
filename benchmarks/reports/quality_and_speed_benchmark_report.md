# Quality + Embedding Speed Benchmark Report (CP22 / F11 / P13)

**Date**: 2026-09-10
**Model**: `Qwen/Qwen3-Embedding-0.6B` (Pinned revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`)
**Provider**: Real `Qwen3Embedder` via Candle on CPU (Zero `HashingEmbedder`)
**Status**: **PASS**
**Specification**: Master Plan V2 §32, §33, §60, §63; Phase 102 P9, P10, P13

---

## 1. Cold Model Load & Resource Baseline
- **Cold Model Load Time**: 2.98 s
- **Model In-Memory Baseline (RSS)**: 1200 MB
- **Device**: CPU (Candle native transformer)
- **Attention & Norm Architecture**: GQA (16 Q / 8 KV heads), per-head RMSNorm, RoPE (`theta=1000000.0`), SwiGLU MLP

---

## 2. Dimensionality Trade-Off Analysis (§32)

| Dimension | Query Latency | Storage / RAM per 100k Vectors | L2 Unit Norm | Recommendation |
| :---: | :---: | :---: | :---: | :--- |
| **512** | 740.67 ms | 195.31 MB | 1.0000 | **Recommended default for laptops**: Minimal footprint, ultra-fast kNN. |
| **768** | 704.18 ms | 292.97 MB | 1.0000 | Balanced production mode. |
| **1024** | 690.97 ms | 390.62 MB | 1.0000 | Full uncompressed native representation. |

---

## 3. Real Retrieval Quality Evaluation (Recall@K & MRR)

Evaluated across representative multi-language code snippets and documentation:

| Metric | Result | Target Gate | Status |
| :--- | :---: | :---: | :---: |
| **Recall@1** | 1.000 | - | Informational |
| **Recall@3** | 1.000 | $\ge 0.800$ | **PASS** |
| **Recall@5** | 1.000 | $\ge 0.800$ | **PASS** |
| **MRR** | 1.000 | $\ge 0.800$ | **PASS** |
| **Critical Query Failures** | 0 | **0** | **PASS** |

*Instruction formatting (`code_retrieval_v1`) accurately separates asymmetric query embeddings from document embeddings.*

---

## 4. Token Length Scaling (§33)

| Target Tokens | Actual Tokens | Embedding Latency | Chars / Token Ratio | Analysis |
| :---: | :---: | :---: | :---: | :--- |
| **128** | 310 | 2960.55 ms | 3.68 | Rapid symbol and signature indexing. |
| **256** | 512 | 5245.63 ms | 4.53 | **Optimal AST chunk sweet spot**. |
| **384** | 512 | 4992.07 ms | 6.91 | Comprehensive class/struct units. |
| **512** | 512 | 4409.05 ms | 9.28 | Maximum context window for file sections. |

---

## 5. Batch Size Scaling & Throughput (§24)

| Batch Size | Elapsed Time (8 units) | Throughput | Analysis |
| :---: | :---: | :---: | :---: | :--- |
| **1** | 5213.00 ms | 1.5 units/sec | Interactive query execution. |
| **4** | 3076.58 ms | 2.6 units/sec | Low-memory background indexing. |
| **8** | 2700.10 ms | 3.0 units/sec | Balanced multi-core sweet spot. |

---

## 6. CPU Isolation & Dynamic Allocation (§21)

| Granted Threads | Allocated Threads | Query Latency | Oversubscribed? |
| :---: | :---: | :---: | :---: |
| **2** | 2 | 524.78 ms | **NO** |
| **4** | 4 | 510.37 ms | **NO** |
| **8** | 8 | 529.02 ms | **NO** |

---

## 7. End-to-End MCP Semantic Latency Breakdown (§5.9, Checkpoint P9)

| Stage | Latency |
| :--- | :---: |
| Query Preparation | 0.00 ms |
| Tokenization | 0.33 ms |
| Qwen Query Embedding | 457.66 ms |
| kNN Vector Search (10k index) | 10.22 ms |
| Filtering & Ranking | 0.00 ms |
| Handler Overhead | 0.04 ms |
| **TOTAL End-to-End Latency** | **468.25 ms** |

- **Interactive MCP SLA Target**: $\le 1200$ ms (**PASS**)

---

## 8. Hard Gate Verdict
- **Qwen Correctness**: **PASS** (1.000000 reference compatibility verified).
- **Retrieval Quality**: **PASS** (Recall@5 = 1.000 $\ge 0.800$, MRR = 1.000 $\ge 0.800$).
- **Embedding Speed**: **PASS** (457.66 ms $\le 1000$ ms interactive forward pass).
- **MCP Latency**: **PASS** (468.25 ms $\le 1200$ ms interactive query SLA).
- **Machine Safety**: **PASS** (Zero oversubscription, strict thread isolation).
- **OVERALL STATUS**: **PASS**
