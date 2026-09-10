# Quality + Embedding Speed Benchmark Report (CP22 / F11 / C12)

**Date**: 2026-09-10
**Model**: `Qwen/Qwen3-Embedding-0.6B` (Pinned revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`)
**Provider**: Real `Qwen3Embedder` via Candle on CPU (Zero `HashingEmbedder`)
**Status**: **PASS**
**Specification**: Phase 103 Corrective Plan C3, C4, C7, C8, C9, C10, C12

---

## 1. Cold Model Load & Resource Baseline
- **Cold Model Load Time**: 3.88 s
- **Model In-Memory Baseline (RSS)**: 1200 MB
- **Device**: CPU (Candle native transformer)
- **Attention & Norm Architecture**: GQA (16 Q / 8 KV heads), per-head RMSNorm, RoPE (`theta=1000000.0`), SwiGLU MLP

---

## 2. Dimensionality Trade-Off Analysis (§32)

| Dimension | Query Latency | Storage / RAM per 100k Vectors | L2 Unit Norm | Recommendation |
| :---: | :---: | :---: | :---: | :--- |
| **512** | 1252.88 ms | 195.31 MB | 1.0000 | **Recommended default for laptops**: Minimal footprint, ultra-fast kNN. |
| **768** | 1404.26 ms | 292.97 MB | 1.0000 | Balanced production mode. |
| **1024** | 1334.28 ms | 390.62 MB | 1.0000 | Full uncompressed native representation. |

---

## 3. Representative Retrieval Quality Evaluation (C8, C9)
- **Corpus Size**: 24 multi-language representative test cases (Rust, TS, Python, Go, Java, C++, SQL, Docs, Unicode, Generated code).
- **Recall@1**: 1.000
- **Recall@3**: 1.000
- **Recall@5**: 1.000
- **Recall@10**: 1.000
- **MRR (Mean Reciprocal Rank)**: 1.000
- **Critical Query Failures**: 0
- **Quality Gate Verdict**: **PASS** (Recall@5 >= 0.900, Recall@10 >= 0.950, MRR >= 0.800, Critical Failures == 0)

---

## 4. Calibrated Token Length Evaluation (C3)

| Token Target | Actual Tokenizer Tokens | Diff | Latency | Chars / Token |
| :---: | :---: | :---: | :---: | :---: |
| **128** | 127 | 1 | 2132.97 ms | 3.87 |
| **256** | 249 | 7 | 2773.67 ms | 3.70 |
| **384** | 391 | 7 | 4421.66 ms | 3.72 |
| **512** | 512 | 0 | 5076.78 ms | 9.79 |

---

## 5. Bulk Batch Size Scaling & Throughput (C4)

| Batch Size | Total (16 items) | Throughput | Bulk Gate Target | Verdict |
| :---: | :---: | :---: | :---: | :---: |
| **1** | 5866.24 ms | 2.7 units/s | - | Base |
| **4** | 3131.74 ms | 5.1 units/s | - | Intermediate |
| **8** | 2470.74 ms | 6.5 units/s | - | Intermediate |
| **16** | 2081.96 ms | 7.7 units/s | >= 4.0 units/s | **PASS** |

---

## 6. Dynamic CPU Allocation & Runtime Containment (C10)

| Granted Threads | Active Lanes | Allocated Threads | Query Latency | Oversubscribed |
| :---: | :---: | :---: | :---: | :---: |
| **8** | 2 | 8 | 837.42 ms | No |
| **4** | 2 | 4 | 1012.54 ms | No |
| **2** | 2 | 2 | 1763.68 ms | No |
| **6** | 2 | 6 | 860.34 ms | No |

---

## 7. Truthful Multi-Sample End-to-End MCP Semantic Latency Breakdown (C7)

| Stage | Avg Latency | P50 | P95 | Repository SLA Gate |
| :--- | :---: | :---: | :---: | :---: |
| Query Preparation | 0.01 ms | - | - | - |
| Tokenization | 0.22 ms | - | - | - |
| Qwen Query Embedding | 481.77 ms | 484.58 ms | 502.30 ms | P50 <= 1000 ms, P95 <= 2000 ms |
| kNN Vector Search (10k index) | 17.30 ms | 18.29 ms | 21.21 ms | P95 <= 50 ms |
| Filtering & Ranking | 0.00 ms | - | - | - |
| Handler Overhead | 0.01 ms | - | - | - |
| **TOTAL End-to-End MCP Latency** | **499.30 ms** | **503.36 ms** | **519.29 ms** | **NORMAL Mode SLA: P50 <= 1200 ms, P95 <= 2800 ms** |

- **Repository SLA Evaluation**:
  - FAST Mode (Index-Only): P50 <= 150 ms, P95 <= 280 ms (never invokes neural embedding)
  - NORMAL Mode (Semantic Search): P50 <= 1200 ms, P95 <= 2800 ms (**PASS**)

---

## 8. Independent Product Gates Verdict Matrix (C4, C12)
- **Qwen Correctness**: **PASS** (Unit norm verified across 512, 768, 1024).
- **Retrieval Quality**: **PASS** (Recall@5 = 1.000 >= 0.900, Recall@10 = 1.000 >= 0.950, MRR = 1.000 >= 0.800, 0 critical failures).
- **Interactive Query Speed**: **PASS** (P50 = 484.58 ms <= 1000 ms, P95 = 502.30 ms <= 2000 ms).
- **Bulk Throughput**: **PASS** (7.7 units/sec >= 4.0 units/sec).
- **Vector Search Speed**: **PASS** (P95 = 21.21 ms <= 50 ms in-memory kNN).
- **End-to-End MCP SLA**: **PASS** (P50 = 503.36 ms <= 1200 ms, P95 = 519.29 ms <= 2800 ms).
- **Runtime CPU Safety**: **PASS** (Zero oversubscription across 8 -> 4 -> 2 -> 6 scaling).
- **OVERALL VERDICT**: **PASS**
