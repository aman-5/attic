# Master Architecture Audit (CP21 — Master Gate)

**Date**: 2026-09-09
**Status**: **PASS (45 / 45 Invariants Verified)**
**Reference**: `ATTIC_FINAL_MASTER_PLAN_V2.md` §65

This audit verifies all 45 architectural invariants defined in Master Plan V2 §65 before authorizing the final quality and embedding benchmarks (CP22) and final release integration (CP23).

---

## Audit Checklist & Verification Matrix

| # | Architectural Invariant | Code Evidence / Implementation Path | Audit Verdict |
|---|:---|:---|:---:|
| 1 | **One normal resource authority** | `ResourceOrchestrator` in `crates/attic-storage/src/resource_orchestrator.rs` is the single authority owning proactive resource distribution between canonical indexing, semantic inference, and interactive MCP. Exposed via `shared_allocation()`. | **PASS** |
| 2 | **ResourceMonitor emergency authority** | `ResourceMonitor` (`crates/attic-storage/src/resource_monitor.rs`) remains the emergency brake. Safety clamps bound downward; emergency halt (`semantic_batch_size == 0`) halts inference immediately regardless of mode. | **PASS** |
| 3 | **Modes are intent policies** | `ModePolicy` in `crates/attic-core/src/config.rs` defines Low, Balanced, Performance, and Auto as intent policies with target headroom, fairness weights, and aggressiveness ratios, not static worker counts. | **PASS** |
| 4 | **Auto real and stable** | `ResourceOrchestrator` evaluates real machine telemetry (RAM, smoothed CPU, power) and employs hysteresis windows to prevent flapping. `mode = "auto"` is never a static Balanced alias. | **PASS** |
| 5 | **Modes remain distinct** | Verified via CP5 unit tests (`test_mode_policy_distinctions`). Low, Balanced, and Performance generate strictly distinct resource envelopes and allocations. | **PASS** |
| 6 | **Explicit precedence rules** | Precedence chain strictly enforced: Emergency Halt (0) > ResourceMonitor Safety Clamps > User Advanced Caps > Orchestrator Allocation. Verified in `EnrichmentConfig::effective_batch_size()`. | **PASS** |
| 7 | **Scale up and down** | Downscales immediately upon high external workload or battery power; gradually upscales during idle periods via conservative hill climbing in `ThroughputController`. | **PASS** |
| 8 | **Runtime mode behavior defined** | `ResourceOrchestrator::set_mode` updates policy and recalculates active allocation dynamically at runtime without restarting database connections or daemon processes. | **PASS** |
| 9 | **Available RAM/CPU/disk used** | `MachineTelemetrySampler` (`crates/attic-storage/src/machine_telemetry.rs`) samples system available RAM, smoothed CPU load, process RSS, and free disk space. | **PASS** |
| 10 | **Multi-process behavior defined** | Telemetry monitors external system consumption. When external developer workloads (IDEs, Docker, builds) consume RAM/CPU, Attic yields resources back immediately. | **PASS** |
| 11 | **Minimum canonical share** | Canonical indexing is guaranteed at least 1 lane / thread even during 100% semantic backlogs, preventing starvation of core file indexing. | **PASS** |
| 12 | **Bounded MCP reserve** | Interactive MCP operations receive reserved thread headroom. If interactive query latency exceeds threshold, background batching rolls back immediately. | **PASS** |
| 13 | **Resource redistribution** | When canonical backlog is empty, idle canonical capacity is dynamically shifted to semantic backlog, and vice versa. | **PASS** |
| 14 | **Model baseline memory accounted** | `ModelLifecycleManager` verifies resident memory requirements before spawning inference lanes, ensuring model weight overhead does not breach process memory budgets. | **PASS** |
| 15 | **Shared model strategy** | Single shared model instance per process; all workspaces and repositories share the same provider instance. | **PASS** |
| 16 | **Provider concurrency defined** | `ProviderConcurrencyContract` (`SharedConcurrent`, `Serialized`, `PooledLanes`) explicitly declared on `EmbeddingProvider`. | **PASS** |
| 17 | **Candle cannot bypass CPU budget** | `CpuIsolationPlan` (`crates/attic-semantic/src/cpu_isolation.rs`) divides granted CPU threads across lanes and exports thread bounds to math backends (`RAYON_NUM_THREADS`, `OMP_NUM_THREADS`). | **PASS** |
| 18 | **Startup/warm-up policy** | `ThroughputController` requires model tensor warm-up before entering high-throughput batching. | **PASS** |
| 19 | **Throughput controller bounded** | Hill-climbing candidate exploration is bounded by stabilization windows, cooldown periods, and rollback on diminishing returns (`min_meaningful_gain_ratio`). | **PASS** |
| 20 | **Tuning persistence/invalidation** | `LearnedTuningManager` with `0004_learned_tuning.sql` persists optimal parameters keyed by `(cpu_arch, os, model_id, revision, dimension, runtime_version)` with BLAKE3 composite hash; automatically invalidates on environment change. | **PASS** |
| 21 | **Shared semantic engine** | Single global `SemanticStore` and `BackgroundEnricher` handle all repositories in the workspace. | **PASS** |
| 22 | **Deterministic hierarchical fairness** | `SemanticFairScheduler` orders work deterministically by `(priority_class -> workspace -> repo -> deterministic FIFO)`. | **PASS** |
| 23 | **Bounded queue** | Queue depth bounded by high watermark (10,000) and low watermark (5,000) hysteresis. | **PASS** |
| 24 | **Upstream backpressure** | Reaching high watermark throttles generation of new semantic jobs while canonical indexing proceeds unblocked. | **PASS** |
| 25 | **Crash-safe jobs** | Durable SQLite queue tracks `PENDING -> INFLIGHT -> DONE`. Process restart resets uncommitted `INFLIGHT` jobs to `PENDING`. | **PASS** |
| 26 | **Idempotent vector commits** | Commits use `ON CONFLICT(retrieval_unit_id, provider_id, model_id) DO UPDATE` with deterministic fingerprinting. | **PASS** |
| 27 | **Stale jobs cannot commit** | Pre-commit content hash validation in scheduler discards embeddings if the underlying source file changed during inference. | **PASS** |
| 28 | **Bad jobs cannot stall queue** | Poison pill detection and retry quarantine after `max_attempts` mark bad units as `FAILED` without blocking the queue. | **PASS** |
| 29 | **Disk protected** | `DiskSafetyGuard` enforces 2048 MiB emergency threshold, halting queue expansion and downloads when disk space is critically low. | **PASS** |
| 30 | **Model assets pinned/atomic** | `ModelAssetManager` downloads assets to staging, validates SHA-256 and pinned revisions, and promotes atomically via filesystem rename. | **PASS** |
| 31 | **Canonical indexing independent from model download** | Canonical indexing runs to completion even if model weights are downloading or offline. | **PASS** |
| 32 | **Model unload/cancel safe** | Cooperative cancellation via `CancelFlag`; model unloading frees memory safely without leaving dangling handles or corrupted state. | **PASS** |
| 33 | **Fingerprint complete** | `EmbeddingFingerprint` encapsulates provider, model ID, revision, dimension, pooling, normalization, tokenizer, chunking, and query instruction version. | **PASS** |
| 34 | **Generations prevent vector mixing** | `sem_generations` table and `generation_id` column strictly isolate vector spaces across different model architectures or revisions. | **PASS** |
| 35 | **Vector index generation-safe** | SQLite schema and kNN queries are explicitly scoped to `generation_id`. | **PASS** |
| 36 | **Rollback available** | `GenerationManager::rollback_generation` atomically reactivates previous complete generation without re-embedding vectors. | **PASS** |
| 37 | **Incremental semantic indexing** | Unchanged units reuse existing embeddings; modified units re-embed; deleted files purge vectors. | **PASS** |
| 38 | **Dedup policy defined** | Identical content hash with same fingerprint avoids duplicate embedding forward passes. | **PASS** |
| 39 | **Canonical readiness independent** | System reports canonical `READY` even while semantic generation is `BUILDING`. | **PASS** |
| 40 | **Semantic failures isolated** | Inference failures gracefully degrade semantic candidate generation without crashing the MCP daemon or affecting canonical search. | **PASS** |
| 41 | **Vector search scalability measured** | Verified in CP20: 30k, 100k, and 500k→1M+ deadline benchmarks pass with `0005_vector_index_scale.sql` composite indexing. | **PASS** |
| 42 | **Progress/ETA/why-slow available** | Verified in CP18: `SemanticProgressSnapshot` and `diagnose_why_slow` integrated into server `status` output. | **PASS** |
| 43 | **External MCP remains stdio** | `attic-server` communicates strictly over standard input/output using JSON-RPC. | **PASS** |
| 44 | **Phase 100 daemon recovery preserved** | Daemon lifecycle, PID files, lockfiles, and stdio recovery mechanisms preserved intact. | **PASS** |
| 45 | **CI/release preserved** | Pure Rust implementation with zero external runtime dependencies (no Python, PyTorch, CUDA, or ONNX Runtime required). | **PASS** |

---

## Conclusion
All 45 architectural invariants in `ATTIC_FINAL_MASTER_PLAN_V2.md` §65 are verified and green. CP21 Master Architecture Gate is **APPROVED**.
