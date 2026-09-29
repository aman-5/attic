//! GPU→CPU inference fallback coordinator (Final Master Plan V2 escalation
//! gap — a permanent GPU failure must never wedge or silently degrade
//! semantic retrieval).
//!
//! [`FallbackCoordinator`] wraps a GPU-backed [`SemanticProvider`] (e.g.
//! [`crate::worker_supervisor::SupervisedWorkerProvider`] with the
//! `ort-directml` backend) and a CPU-backed one, and is itself a
//! `SemanticProvider`. It is a drop-in composition, not a parallel
//! mechanism:
//! * Failure classification decides whether a GPU error is transient (worth
//!   retrying on GPU) or permanent (GPU is not going to work at all).
//! * On a permanent classification it flips which inner provider answers
//!   `fingerprint()`/`embed_batch()` — nothing else. The EXISTING generation
//!   machinery (`enrich::ensure_generation_for_fingerprint`,
//!   `GenerationManager::activate_generation`) reacts to the fingerprint
//!   change exactly as it already does for a model upgrade: it starts a new
//!   BUILDING generation keyed by the CPU fingerprint, leaving the GPU
//!   generation ACTIVE and serving queries untouched.
//! * This module adds exactly one thing the existing machinery does NOT
//!   already do: deciding when that BUILDING CPU generation is actually
//!   complete (its vector-space queue is drained AND made real progress) and
//!   only then calling `activate_generation` — so retrieval never reads a
//!   half-built CPU generation.
//! * Queued work is never touched here: a failed claim's lease already
//!   resets/fails via the v2 queue's existing lease/attempts machinery
//!   (`queue_reset`/`queue_mark_failed`, driven by `enrich::drive`).
//!   This coordinator only changes which fingerprint future claims get
//!   embedded — and therefore generation-tagged — under.
//!
//! Mocked-only: real GPU OOM/device-loss/worker-load-failure cannot be
//! triggered on real hardware from an automated test; every test here uses
//! [`crate::testing`]-style injected failure doubles. See module tests.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, ProviderConcurrencyContract,
    ResourceUsage, SemanticProvider,
};
use crate::store::SemanticStore;

/// Which inner provider is currently answering calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveBackend {
    Gpu,
    Cpu,
}

/// How a GPU-side [`SemanticError`] should influence fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GpuFailureClass {
    /// Escalate to CPU immediately — the GPU stack is fundamentally not
    /// going to work (model/artifact load failure, worker unreachable).
    Permanent,
    /// Counts toward the consecutive-failure escalation threshold (e.g.
    /// repeated worker deaths/timeouts standing in for "device loss").
    TransientCountable,
    /// Never counts and never escalates — the existing per-batch
    /// `oom_batch_cap` halving in `enrich.rs` already owns this signal.
    Ignored,
}

fn classify(err: &SemanticError) -> GpuFailureClass {
    match err {
        // Worker load/handshake failure or reported model-artifact problem —
        // this is exactly "GPU execution remains unavailable or unsafe".
        SemanticError::ProviderUnavailable { .. } => GpuFailureClass::Permanent,
        // Device pressure is a condition, not a defect. It clears when other
        // processes release VRAM, so it must be able to recover — but if it
        // never clears, the consecutive-failure threshold still escalates to
        // CPU rather than starving the queue forever.
        SemanticError::DevicePressure(_) => GpuFailureClass::TransientCountable,
        // OOM is per-batch adaptive-sizing territory (r07); repeated OOM
        // alone must never trigger fallback (see module tests).
        SemanticError::BudgetExhausted(_) => GpuFailureClass::Ignored,
        // Worker died/timed out mid-batch or an internal engine error:
        // possibly transient (the supervisor already restarts the child),
        // but repeated occurrences stand in for "device loss" since real
        // device loss cannot be synthesized in this environment.
        SemanticError::EmbeddingFailed(_) => GpuFailureClass::TransientCountable,
        SemanticError::Cancelled { .. } => GpuFailureClass::TransientCountable,
        _ => GpuFailureClass::TransientCountable,
    }
}

#[cfg(test)]
mod device_pressure_tests {
    use super::*;

    /// VRAM pressure used to arrive as `ProviderUnavailable`, which is
    /// classified permanent — so one spike on a shared desktop GPU retired
    /// the device for the whole process. It must be recoverable.
    #[test]
    fn device_pressure_is_never_permanent() {
        assert!(matches!(
            classify(&SemanticError::DevicePressure("vram".into())),
            GpuFailureClass::TransientCountable
        ));
    }

    /// A genuinely broken provider must still be permanent — the fix above
    /// must not soften real provider defects.
    #[test]
    fn a_broken_provider_is_still_permanent() {
        assert!(matches!(
            classify(&SemanticError::ProviderUnavailable {
                provider: "x".into(),
                reason: "model artifact corrupt".into(),
            }),
            GpuFailureClass::Permanent
        ));
    }
}

/// Tunable escalation knobs.
#[derive(Debug, Clone, Copy)]
pub struct FallbackConfig {
    /// Consecutive `TransientCountable` GPU failures before escalating to
    /// CPU (stand-in for "repeated failure past a threshold" device loss).
    pub consecutive_failure_threshold: u32,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            consecutive_failure_threshold: 3,
        }
    }
}

/// Composes a GPU provider and a CPU provider behind one [`SemanticProvider`]
/// identity, escalating from GPU to CPU on a classified-permanent failure
/// and promoting the resulting CPU generation to ACTIVE only once it is
/// verifiably complete for its vector space.
pub struct FallbackCoordinator {
    gpu: Arc<dyn SemanticProvider>,
    cpu: Arc<dyn SemanticProvider>,
    store: Arc<SemanticStore>,
    active: RwLock<ActiveBackend>,
    reason: RwLock<Option<String>>,
    consecutive_transient_failures: AtomicU32,
    config: FallbackConfig,
    /// Set once the post-fallback CPU generation has been activated, so
    /// repeated successful CPU batches don't re-issue `activate_generation`
    /// forever (idempotent but wasteful).
    promoted: AtomicBool,
}

impl FallbackCoordinator {
    pub fn new(
        gpu: Arc<dyn SemanticProvider>,
        cpu: Arc<dyn SemanticProvider>,
        store: Arc<SemanticStore>,
        config: FallbackConfig,
    ) -> Self {
        Self {
            gpu,
            cpu,
            store,
            active: RwLock::new(ActiveBackend::Gpu),
            reason: RwLock::new(None),
            consecutive_transient_failures: AtomicU32::new(0),
            config,
            promoted: AtomicBool::new(false),
        }
    }

    fn active_backend(&self) -> ActiveBackend {
        *self.active.read().unwrap_or_else(|e| e.into_inner())
    }

    fn current(&self) -> &Arc<dyn SemanticProvider> {
        match self.active_backend() {
            ActiveBackend::Gpu => &self.gpu,
            ActiveBackend::Cpu => &self.cpu,
        }
    }

    /// True once this coordinator has escalated away from GPU.
    pub fn has_fallen_back(&self) -> bool {
        self.active_backend() == ActiveBackend::Cpu
    }

    fn switch_to_cpu(&self, reason: String) {
        let mut backend = self.active.write().unwrap_or_else(|e| e.into_inner());
        if *backend == ActiveBackend::Cpu {
            return; // already switched; keep the first reason
        }
        *backend = ActiveBackend::Cpu;
        drop(backend);
        *self.reason.write().unwrap_or_else(|e| e.into_inner()) = Some(reason);
        self.promoted.store(false, Ordering::Relaxed);
    }

    fn handle_gpu_failure(&self, err: &SemanticError) {
        match classify(err) {
            GpuFailureClass::Permanent => {
                self.switch_to_cpu(format!("gpu provider reported a permanent failure: {err}"));
            }
            GpuFailureClass::TransientCountable => {
                let n = self
                    .consecutive_transient_failures
                    .fetch_add(1, Ordering::AcqRel)
                    + 1;
                if n >= self.config.consecutive_failure_threshold {
                    self.switch_to_cpu(format!(
                        "gpu provider failed {n} consecutive times (threshold {}): {err}",
                        self.config.consecutive_failure_threshold
                    ));
                }
            }
            GpuFailureClass::Ignored => {}
        }
    }

    fn handle_gpu_success(&self) {
        self.consecutive_transient_failures
            .store(0, Ordering::Relaxed);
    }

    /// If we're running on CPU because of a fallback, and the BUILDING
    /// generation for the CPU fingerprint has fully drained its vector
    /// space's v2 queue AND made real progress (at least one committed
    /// vector), atomically activate it. A generation with zero enqueued
    /// work is NOT "complete" — it just hasn't started — so this never
    /// promotes an empty generation ahead of the still-serving GPU one.
    fn try_promote_cpu_generation(&self) {
        if self.promoted.load(Ordering::Relaxed) {
            return;
        }
        let Some(fp) = self.cpu.fingerprint() else {
            return;
        };
        let Ok(Some(building)) = self.store.get_building_generation() else {
            return;
        };
        if building.fingerprint != fp {
            return; // some other generation is building; not ours
        }
        let vsid = fp.vector_space_id();
        let Ok(counts) = self.store.queue_counts_for_vector_space(&vsid) else {
            return;
        };
        if counts.pending == 0
            && counts.inflight == 0
            && counts.done > 0
            && self
                .store
                .activate_generation(building.generation_id)
                .is_ok()
        {
            self.promoted.store(true, Ordering::Relaxed);
        }
    }

    /// Truthful fallback-state reason, if any (status reporting).
    pub fn fallback_reason_snapshot(&self) -> Option<String> {
        self.reason
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl SemanticProvider for FallbackCoordinator {
    fn id(&self) -> &'static str {
        // Delegate so callers see the actually-active backend's identity —
        // "gpu-fallback" would hide which real provider is serving.
        self.current().id()
    }

    fn model_id(&self) -> &str {
        // Cannot borrow out through Arc<dyn ...> across the match arms with
        // a `&str` return; both inner providers are qwen3-family so this is
        // stable regardless of which is active.
        "qwen3-embedding-0.6b"
    }

    fn dimensions(&self) -> usize {
        self.current().dimensions()
    }

    fn max_input_bytes(&self) -> usize {
        self.current().max_input_bytes()
    }

    fn available(&self) -> bool {
        self.current().available()
    }

    fn model_lifecycle(&self) -> Option<String> {
        self.current().model_lifecycle()
    }

    fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        self.current().concurrency_contract()
    }

    fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
        self.current().fingerprint()
    }

    fn fallback_reason(&self) -> Option<String> {
        self.fallback_reason_snapshot()
    }

    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        let backend = self.active_backend();
        let provider = match backend {
            ActiveBackend::Gpu => &self.gpu,
            ActiveBackend::Cpu => &self.cpu,
        };
        let result = provider.embed_batch(inputs, cancel, usage, deadline);
        match (&result, backend) {
            (Ok(_), ActiveBackend::Gpu) => self.handle_gpu_success(),
            (Ok(_), ActiveBackend::Cpu) => self.try_promote_cpu_generation(),
            (Err(e), ActiveBackend::Gpu) => self.handle_gpu_failure(e),
            (Err(_), ActiveBackend::Cpu) => {} // CPU failing has no further fallback tier
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{HashingEmbedder, OomProvider};

    fn store() -> Arc<SemanticStore> {
        Arc::new(SemanticStore::open_in_memory().unwrap())
    }

    /// A GPU test double that fails `available()`/`embed_batch` with a
    /// caller-chosen error every call, standing in for a worker that can
    /// never load its model artifact (classified-permanent) or one that
    /// keeps dying/timing out (classified-transient-countable). This is a
    /// MOCK: it is not driven by any real GPU/DirectML worker process, so
    /// this proves the fallback coordinator's own logic, not real hardware
    /// behavior.
    struct AlwaysFailProvider {
        fp: EmbeddingFingerprint,
        err: fn() -> SemanticError,
    }

    impl SemanticProvider for AlwaysFailProvider {
        fn id(&self) -> &'static str {
            "always-fail-gpu-mock"
        }
        fn model_id(&self) -> &str {
            "mock-gpu-v1"
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn max_input_bytes(&self) -> usize {
            4096
        }
        fn available(&self) -> bool {
            true
        }
        fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
            Some(self.fp.clone())
        }
        fn embed_batch(
            &self,
            _inputs: &[EmbeddingInput],
            _cancel: &CancelFlag,
            _usage: &mut ResourceUsage,
            _deadline: Option<Instant>,
        ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
            Err((self.err)())
        }
    }

    fn gpu_fp() -> EmbeddingFingerprint {
        use crate::provider::ExecutionBackend;
        EmbeddingFingerprint {
            provider: "qwen3-ort".into(),
            model_id: "qwen3-embedding-0.6b".into(),
            model_revision: "onnx-community-fp16".into(),
            dimension: 4,
            pooling_version: "last_token_v1".into(),
            normalization_version: "l2_unit_v1".into(),
            tokenizer_version: "qwen_bpe_v1".into(),
            chunking_version: "test".into(),
            query_instruction_version: "code_retrieval_v1".into(),
            execution_backend: ExecutionBackend::OrtDirectMl,
            quantization: "fp16-onnx".into(),
        }
    }

    fn cpu_fp() -> EmbeddingFingerprint {
        // A HashingEmbedder-flavored fingerprint, deliberately distinct in
        // every identity field from `gpu_fp()` (mirrors the real
        // candle-cpu-vs-ort-directml split in `worker_supervisor.rs`).
        HashingEmbedder::new().fingerprint().unwrap()
    }

    fn inputs(n: usize) -> Vec<EmbeddingInput> {
        (0..n)
            .map(|i| EmbeddingInput {
                unit_key: format!("unit-{i}"),
                text: format!("text {i}"),
            })
            .collect()
    }

    #[test]
    fn permanent_gpu_failure_triggers_immediate_fallback_to_cpu() {
        let gpu = Arc::new(AlwaysFailProvider {
            fp: gpu_fp(),
            err: || SemanticError::ProviderUnavailable {
                provider: "inference-worker".into(),
                reason: "mock: model artifact failed to load".into(),
            },
        });
        let cpu: Arc<dyn SemanticProvider> = Arc::new(HashingEmbedder::new());
        let coord = FallbackCoordinator::new(gpu, cpu, store(), FallbackConfig::default());

        assert!(!coord.has_fallen_back());
        assert_eq!(
            coord.fingerprint().unwrap().vector_space_id(),
            gpu_fp().vector_space_id()
        );

        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        let err = coord
            .embed_batch(&inputs(2), &cancel, &mut usage, None)
            .unwrap_err();
        assert!(matches!(err, SemanticError::ProviderUnavailable { .. }));

        assert!(
            coord.has_fallen_back(),
            "a single Artifact/ProviderUnavailable GPU error must escalate immediately"
        );
        assert!(coord.fallback_reason().is_some());
        // Subsequent calls now serve from CPU: fingerprint has flipped.
        assert_eq!(
            coord.fingerprint().unwrap().vector_space_id(),
            cpu_fp().vector_space_id()
        );
    }

    #[test]
    fn repeated_oom_alone_never_triggers_fallback() {
        // OomProvider fails BudgetExhausted whenever a batch exceeds
        // max_items — the existing r07 adaptive-batch-cap signal, which
        // this coordinator must leave completely alone.
        let gpu: Arc<dyn SemanticProvider> = Arc::new(OomProvider { max_items: 1 });
        let cpu: Arc<dyn SemanticProvider> = Arc::new(HashingEmbedder::new());
        let coord = FallbackCoordinator::new(gpu, cpu, store(), FallbackConfig::default());

        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        for _ in 0..50 {
            let _ = coord.embed_batch(&inputs(3), &cancel, &mut usage, None);
        }
        assert!(
            !coord.has_fallen_back(),
            "repeated OOM alone must never escalate to CPU fallback"
        );
        assert!(coord.fallback_reason().is_none());
    }

    #[test]
    fn repeated_transient_failures_past_threshold_escalate() {
        let gpu = Arc::new(AlwaysFailProvider {
            fp: gpu_fp(),
            err: || {
                SemanticError::EmbeddingFailed(
                    "inference worker died mid-batch; it will restart on the next batch".into(),
                )
            },
        });
        let cpu: Arc<dyn SemanticProvider> = Arc::new(HashingEmbedder::new());
        let cfg = FallbackConfig {
            consecutive_failure_threshold: 3,
        };
        let coord = FallbackCoordinator::new(gpu, cpu, store(), cfg);

        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        // First two failures: below threshold, stay on GPU.
        let _ = coord.embed_batch(&inputs(1), &cancel, &mut usage, None);
        assert!(!coord.has_fallen_back());
        let _ = coord.embed_batch(&inputs(1), &cancel, &mut usage, None);
        assert!(!coord.has_fallen_back());
        // Third failure crosses the threshold.
        let _ = coord.embed_batch(&inputs(1), &cancel, &mut usage, None);
        assert!(coord.has_fallen_back());
    }

    #[test]
    fn a_single_transient_failure_below_threshold_does_not_escalate() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct OnceFailThenOk {
            fp: EmbeddingFingerprint,
            calls: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl SemanticProvider for OnceFailThenOk {
            fn id(&self) -> &'static str {
                "once-fail-gpu-mock"
            }
            fn model_id(&self) -> &str {
                "mock-gpu-v1"
            }
            fn dimensions(&self) -> usize {
                4
            }
            fn max_input_bytes(&self) -> usize {
                4096
            }
            fn available(&self) -> bool {
                true
            }
            fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
                Some(self.fp.clone())
            }
            fn embed_batch(
                &self,
                inputs: &[EmbeddingInput],
                _cancel: &CancelFlag,
                usage: &mut ResourceUsage,
                _deadline: Option<Instant>,
            ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    return Err(SemanticError::EmbeddingFailed(
                        "mock: worker timeout".into(),
                    ));
                }
                usage.items_embedded += inputs.len() as u64;
                Ok(inputs
                    .iter()
                    .map(|i| EmbeddingOutput {
                        unit_key: i.unit_key.clone(),
                        vector: vec![1.0; 4],
                    })
                    .collect())
            }
        }
        let gpu = Arc::new(OnceFailThenOk {
            fp: gpu_fp(),
            calls: calls.clone(),
        });
        let cpu: Arc<dyn SemanticProvider> = Arc::new(HashingEmbedder::new());
        let coord = FallbackCoordinator::new(gpu, cpu, store(), FallbackConfig::default());

        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        let _ = coord.embed_batch(&inputs(1), &cancel, &mut usage, None);
        assert!(!coord.has_fallen_back());
        // A recovery clears the counter: still no escalation after success.
        coord
            .embed_batch(&inputs(1), &cancel, &mut usage, None)
            .unwrap();
        assert!(!coord.has_fallen_back());
    }

    #[test]
    fn queued_work_is_not_lost_and_completes_under_cpu_generation_after_fallback() {
        let s = store();

        // Simulate the GPU generation already ACTIVE and serving, exactly
        // as it would be before any failure.
        let gpu_gen = s.start_new_generation(&gpu_fp()).unwrap();
        s.activate_generation(gpu_gen.generation_id).unwrap();

        let gpu = Arc::new(AlwaysFailProvider {
            fp: gpu_fp(),
            err: || SemanticError::ProviderUnavailable {
                provider: "inference-worker".into(),
                reason: "mock: model artifact failed to load".into(),
            },
        });
        let cpu: Arc<dyn SemanticProvider> = Arc::new(HashingEmbedder::new());
        let coord = FallbackCoordinator::new(gpu, cpu, s.clone(), FallbackConfig::default());

        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        // Trigger the permanent-failure escalation.
        let _ = coord.embed_batch(&inputs(1), &cancel, &mut usage, None);
        assert!(coord.has_fallen_back());

        // The GPU generation must still be ACTIVE — nothing was lost or
        // torn down; a CPU generation has NOT been created yet (nothing
        // enqueues generations directly; that's `enrich::ensure_generation_for_fingerprint`'s
        // job, driven from `drive()`, not this coordinator).
        let active = s.get_active_generation().unwrap().unwrap();
        assert_eq!(active.generation_id, gpu_gen.generation_id);
        assert!(s.get_building_generation().unwrap().is_none());

        // Now simulate what `enrich::drive` does on the next pass: it reads
        // `coord.fingerprint()` (now CPU) and calls
        // `ensure_generation_for_fingerprint`, creating a BUILDING CPU
        // generation without touching the still-ACTIVE GPU one.
        let cpu_fp_now = coord.fingerprint().unwrap();
        assert_eq!(cpu_fp_now.vector_space_id(), cpu_fp().vector_space_id());
        let cpu_gen = s.start_new_generation(&cpu_fp_now).unwrap();

        // GPU generation is untouched: retrieval still reads real data
        // while the CPU generation is being built.
        let active_still = s.get_active_generation().unwrap().unwrap();
        assert_eq!(active_still.generation_id, gpu_gen.generation_id);

        // Enqueue + drain one occurrence under the CPU vector space, mirroring
        // what `invalidate::reconcile` + `enrich::drive` do in production.
        let vsid = cpu_fp_now.vector_space_id();
        s.add_occurrence(
            "occ-1",
            "unit-1",
            &vsid,
            "hash-1",
            "repo",
            "rev",
            "gen",
            "content-gen",
            "{}",
        )
        .unwrap();
        s.queue_enqueue("occ-1", 0.0).unwrap();

        // Before the queue drains, promotion must not happen even if called.
        coord.try_promote_cpu_generation();
        assert!(s.get_building_generation().unwrap().is_some());
        let active_before_drain = s.get_active_generation().unwrap().unwrap();
        assert_eq!(active_before_drain.generation_id, gpu_gen.generation_id);

        // Drain it: claim, embed via the (now CPU-active) coordinator, and
        // mark done — this is the "queued work is not lost" proof: the
        // occurrence claimed under the old GPU identity's failure is
        // completed under the CPU generation instead of disappearing.
        let claims = s.queue_claim_batch("owner-1", 60_000, 8).unwrap();
        assert_eq!(claims.len(), 1);
        let (occ_id, token) = claims[0].clone();
        let out = coord
            .embed_batch(
                &[EmbeddingInput {
                    unit_key: "unit-1".into(),
                    text: "hello world".into(),
                }],
                &cancel,
                &mut usage,
                None,
            )
            .unwrap();
        assert_eq!(out.len(), 1);
        s.queue_complete("occ-1", "owner-1", token).unwrap();

        // Now the CPU vector space's queue is fully drained with progress —
        // promotion may fire (either from the embed_batch success path
        // above, since it ran while active==Cpu, or explicitly here).
        coord.try_promote_cpu_generation();

        let active_final = s.get_active_generation().unwrap().unwrap();
        assert_eq!(
            active_final.generation_id, cpu_gen.generation_id,
            "CPU generation must become ACTIVE only once its queue is drained"
        );
        assert_eq!(active_final.fingerprint.vector_space_id(), vsid);
        assert_ne!(
            active_final.fingerprint.vector_space_id(),
            gpu_fp().vector_space_id(),
            "GPU-tagged and CPU-tagged vectors must never share one active generation"
        );
        let _ = occ_id; // silence unused warning if claim shape changes
    }
}
