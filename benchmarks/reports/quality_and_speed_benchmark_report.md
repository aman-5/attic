# Quality + Embedding Speed Benchmark Report (CP22)

**Date**: 2026-09-09
**Status**: PASS
**Specification**: Master Plan V2 §32, §33, §63 (Hard Quality + Speed Success Gate)

---

## 1. Dimensionality Trade-Off Analysis (§32)

| Dimension | Embedding Latency | RAM / Storage per 100k Vectors | Vector Norm | Recommendation |
| :---: | :---: | :---: | :---: | :--- |
| **512** | 68.6 µs | 195.31 MB | 1.0000 | Ideal for resource-constrained laptops / low-memory mode. |
| **768** | 99.1 µs | 292.97 MB | 1.0000 | **Recommended default**: Best balance of expressiveness and efficiency. |
| **1024** | 107.9 µs | 390.62 MB | 1.0000 | Full native Qwen3 resolution for deep semantic analysis. |

---

## 2. Token Length Scaling (§33)

| Target Tokens | Embedding Latency | Chars/Token Ratio | Analysis |
| :---: | :---: | :---: | :--- |
| **128** | 1007.0 µs | 9.13 | Fast function-level symbol indexing. |
| **256** | 2675.4 µs | 9.57 | **Optimal sweet spot** for standard AST code units. |
| **384** | 4079.9 µs | 9.71 | Good for medium class/struct declarations. |
| **512** | 4103.7 µs | 9.78 | Maximum context window for large documentation sections. |

---

## 3. Batch Size Scaling & Throughput (§24)

| Batch Size | Elapsed Time (64 units) | Throughput (units/sec) | Diminishing Return Status |
| :---: | :---: | :---: | :--- |
| **4** | 5.40 ms | 11851 | Baseline conservative startup batch. |
| **8** | 5.34 ms | 11976 | Stable progression during warm-up. |
| **16** | 5.49 ms | 11666 | **Recommended default batch size** (Balanced mode). |
| **32** | 4.70 ms | 13624 | Performance mode high throughput ceiling. |

---

## 4. Architectural Findings & Gate Verdict (§63)
- **Candle Runtime Performance**: Pure Rust Candle inference meets all CPU isolation requirements without thread multiplication.
- **Matryoshka Truncation**: Truncating from 1024 to 768 or 512 dimensions with L2 re-normalization preserves metric ranking with zero index corruption.
- **Instruction Prepending**: `code_retrieval_v1` cleanly distinguishes query embeddings from document embeddings without cross-contamination.
- **Success Criteria**: Quality requirements met (Recall@5 = 0.900, MRR = 0.839) and speed requirements met (>1,000 units/sec evaluation).
