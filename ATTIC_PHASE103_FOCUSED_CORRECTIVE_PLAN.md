Repository-first. Inspect the actual Phase 105 code and trace the real Qwen/Candle execution path before changing anything. Do not assume the previous review is correct; verify it against the repository.

Fix only the remaining **real production issues** related to Qwen performance and resource control.

### 1. Qwen/Candle performance

* Profile the actual release-build Qwen path.
* Check whether Attic’s custom Qwen attention implementation is leaving current Candle CPU optimizations unused.
* Check `repeat_kv`, attention-score materialization, tensor copies/allocations, sequence length/padding, tokenizer reuse, and model reuse.
* Verify model/tokenizer are loaded once and reused.
* Benchmark **single inference lane first**, then controlled concurrency.
* Keep the implementation simple and use the fastest correct Candle path supported by the current dependency/version.
* Do not change the model or relax performance requirements just to make the benchmark pass.

### 2. CPU/concurrency control

* Verify how Rayon/BLAS/tokenizer/Candle threads are actually initialized.
* Do not rely on changing environment variables after thread pools already exist.
* Keep runtime elasticity through actual admission/concurrency control.
* Verify allocation changes such as `8 -> 4 -> 2 -> 6` using real Qwen inference.
* Ensure inference lanes cannot multiply into uncontrolled backend threads.

### 3. Benchmark correctness

* Use real Qwen only for performance/quality evidence.
* Use actual tokenizer lengths.
* Measure real p50/p95 with multiple samples.
* Separate:

  * query embedding latency;
  * bulk embedding throughput;
  * vector-search latency;
  * end-to-end MCP latency.
* Keep FAST/NORMAL SLA definitions from the repository; do not invent new thresholds.
* Regenerate reports from actual runs. Never hard-code PASS.

### 4. Production cleanup

* Hashing must not be production-selectable or a fallback.
* Remove any remaining obsolete BGE/legacy/provider-selection surface encountered in this work.
* Delete obsolete tests/reports/config/docs.
* **Do not add dummy code, fake providers, synthetic production paths, placeholder implementations, or test-only code that is presented as production functionality.**
* Every change must be connected to the actual production code path that will be demonstrated.

### 5. Final verification

Run the relevant real production tests/benchmarks in release mode, then verify CI/release compatibility.

Do not mark anything PASS unless the actual implementation and measured evidence satisfy the existing repository requirements.

At the end, report only:

* files changed/deleted;
* actual production behavior changed;
* benchmarks before/after;
* remaining failures;
* final status.
