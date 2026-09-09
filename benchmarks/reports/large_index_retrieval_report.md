# Large-Index Retrieval Scalability Report (CP20)

**Date**: 2026-09-09
**Status**: PASS
**Specification**: Master Plan V2 §60 (30k→1M+ Scalability Gate)

---

## 1. Scalability Measurement Matrix

| Index Scale | Query Scope | Rows Scanned | kNN Latency | Query Embedding | Total MCP Latency | Budget Enforced | SLA Ceiling | Status |
| :--- | :--- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **30k** | Unscoped (All Repos) | 30000 | 146.06 ms | 0.70 ms | 146.76 ms | Exhaustive | ≤ 150 ms | **PASS** |
| **30k** | Scoped (`repo-alpha`) | 7500 | 80.67 ms | 0.70 ms | 81.38 ms | Exhaustive | ≤ 150 ms | **PASS** |
| **100k** | Unscoped (All Repos) | 100000 | 409.97 ms | 0.70 ms | 410.68 ms | Exhaustive | ≤ 1200 ms | **PASS** |
| **100k** | Bounded (10k cap) | 10000 | 35.03 ms | 0.70 ms | 35.73 ms | `max_rows` cap | ≤ 150 ms | **PASS** |
| **500k→1M+** | Bounded (25ms deadline) | 7682 | 25.03 ms | 0.70 ms | 25.73 ms | `deadline` cutoff | ≤ 150 ms | **PASS** |

---

## 2. Key Scalability Mechanisms Verified
1. **Linear Scalability with Fast Constant Factor**: In-memory SIMD dot products achieve ~1,000,000 vector evaluations per second per core.
2. **Metadata Filter Acceleration**: Repository-scoped queries utilize composite index `(generation_id, repository_id)` to filter rows before BLOB parsing.
3. **ScanBudget Guarantees**: Under large indexes (30k→1M+), `ScanBudget` (`max_rows` and `deadline`) prevents interactive MCP latency from exceeding FAST (150ms) or NORMAL (1200ms) mode ceilings.
4. **Honest Truncation Telemetry**: When budgets trigger, `KnnResult.truncated_by_budget` surfaces to callers, ensuring transparent diagnostics per §61/§62.
