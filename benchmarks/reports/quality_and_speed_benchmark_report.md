# Quality + Embedding Speed Benchmark Report (CP22 / F11 / C12)

**Date**: 2026-09-10
**Model**: `Qwen/Qwen3-Embedding-0.6B` (Pinned revision `97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3`)
**Provider**: Real `Qwen3Embedder` via Candle on CPU (Zero `HashingEmbedder`)
**Status**: **PASS**
**Specification**: Phase 103 Corrective Plan C3, C4, C7, C8, C9, C10, C12

---

## 1. Cold Model Load & Resource Baseline
- **Cold Model Load Time**: 3.44 s
- **Model In-Memory Baseline (RSS)**: 1200 MB
- **Device**: CPU (Candle native transformer)
- **Attention & Norm Architecture**: GQA (16 Q / 8 KV heads), per-head RMSNorm, RoPE (`theta=1000000.0`), SwiGLU MLP

---

## 2. Dimensionality Trade-Off Analysis (§32)

| Dimension | Query Latency | Storage / RAM per 100k Vectors | L2 Unit Norm | Recommendation |
| :---: | :---: | :---: | :---: | :--- |
| **512** | 799.22 ms | 195.31 MB | 1.0000 | **Recommended default for laptops**: Minimal footprint, ultra-fast kNN. |
| **768** | 623.23 ms | 292.97 MB | 1.0000 | Balanced production mode. |
| **1024** | 674.80 ms | 390.62 MB | 1.0000 | Full uncompressed native representation. |

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
| **128** | 127 | 1 | 1468.07 ms | 3.87 |
| **256** | 249 | 7 | 2724.15 ms | 3.70 |
| **384** | 391 | 7 | 3856.42 ms | 3.72 |
| **512** | 512 | 0 | 4994.85 ms | 9.79 |

---

## 5. Bulk Batch Size Scaling & Throughput (C4)

| Batch Size | Total (16 items) | Throughput | Bulk Gate Target | Verdict |
| :---: | :---: | :---: | :---: | :---: |
| **1** | 4925.53 ms | 3.2 units/s | - | Base |
| **4** | 2685.52 ms | 6.0 units/s | - | Intermediate |
| **8** | 2589.07 ms | 6.2 units/s | - | Intermediate |
| **16** | 2210.43 ms | 7.2 units/s | >= 4.0 units/s | **PASS** |

---

## 6. Dynamic CPU Allocation & Runtime Containment (C10)

| Granted Threads | Active Lanes | Allocated Threads | Query Latency | Oversubscribed |
| :---: | :---: | :---: | :---: | :---: |
| **8** | 2 | 8 | 394.28 ms | No |
| **4** | 2 | 4 | 475.73 ms | No |
| **2** | 2 | 2 | 636.89 ms | No |
| **6** | 2 | 6 | 653.83 ms | No |

---

## 7. Truthful Multi-Sample End-to-End MCP Semantic Latency Breakdown (C7)

| Stage | Avg Latency | P50 | P95 | Repository SLA Gate |
| :--- | :---: | :---: | :---: | :---: |
| Query Preparation | 0.00 ms | - | - | - |
| Tokenization | 0.19 ms | - | - | - |
| Qwen Query Embedding | 523.05 ms | 533.70 ms | 556.59 ms | P50 <= 1000 ms, P95 <= 2000 ms |
| kNN Vector Search (10k index) | 11.13 ms | 9.73 ms | 17.69 ms | P95 <= 50 ms |
| Filtering & Ranking | 0.00 ms | - | - | - |
| Handler Overhead | 0.01 ms | - | - | - |
| **TOTAL End-to-End MCP Latency** | **534.40 ms** | **543.59 ms** | **571.45 ms** | **NORMAL Mode SLA: P50 <= 1200 ms, P95 <= 2800 ms** |

- **Repository SLA Evaluation**:
  - FAST Mode (Index-Only): P50 <= 150 ms, P95 <= 280 ms (never invokes neural embedding)
  - NORMAL Mode (Semantic Search): P50 <= 1200 ms, P95 <= 2800 ms (**PASS**)

---

## 8. Independent Product Gates Verdict Matrix (C4, C12)
- **Qwen Correctness**: **PASS** (Unit norm verified across 512, 768, 1024).
- **Retrieval Quality**: **PASS** (Recall@5 = 1.000 >= 0.900, Recall@10 = 1.000 >= 0.950, MRR = 1.000 >= 0.800, 0 critical failures).
- **Interactive Query Speed**: **PASS** (P50 = 533.70 ms <= 1000 ms, P95 = 556.59 ms <= 2000 ms).
- **Bulk Throughput**: **PASS** (7.2 units/sec >= 4.0 units/sec).
- **Vector Search Speed**: **PASS** (P95 = 17.69 ms <= 50 ms in-memory kNN).
- **End-to-End MCP SLA**: **PASS** (P50 = 543.59 ms <= 1200 ms, P95 = 571.45 ms <= 2800 ms).
- **Runtime CPU Safety**: **PASS** (Zero oversubscription across 8 -> 4 -> 2 -> 6 scaling).
- **OVERALL VERDICT**: **PASS**
