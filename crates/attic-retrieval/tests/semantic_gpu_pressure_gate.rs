//! Backend-aware host-RAM pressure gate for background embedding.
//!
//! Host-RAM Emergency used to park ALL embedding, including a dedicated GPU
//! whose work lives in VRAM — leaving the GPU idle for nothing. These tests
//! drive the real `BackgroundEnricher` loop under a forced Emergency tier and
//! injected system-memory samples, with a provider whose reported execution
//! backend can be switched live (as a GPU->CPU demotion does).

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use attic_core::ResourcePressure;
use attic_retrieval::semantic::SemanticStack;
use attic_semantic::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, EnrichmentConfig,
    ExecutionBackend, ResourceUsage, SemanticError, SemanticProvider, testing::HashingEmbedder,
};
use attic_storage::resource_manager::{
    DEDICATED_GPU_HOST_FLOOR_MIB, ResourceMonitor, UNIFIED_GPU_HOST_FLOOR_MIB,
};
use common::Fixture;

/// Hashing embedder that reports a switchable execution backend.
struct BackendAs {
    inner: HashingEmbedder,
    backend: Mutex<ExecutionBackend>,
}

impl BackendAs {
    fn new(backend: ExecutionBackend) -> Arc<Self> {
        Arc::new(Self {
            inner: HashingEmbedder::new(),
            backend: Mutex::new(backend),
        })
    }
    fn set(&self, backend: ExecutionBackend) {
        *self.backend.lock().unwrap() = backend;
    }
}

impl SemanticProvider for BackendAs {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    fn max_input_bytes(&self) -> usize {
        self.inner.max_input_bytes()
    }
    fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
        let mut fp = self.inner.fingerprint()?;
        fp.execution_backend = *self.backend.lock().unwrap();
        Some(fp)
    }
    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        // Slow enough that the queue cannot drain before a pause is checked.
        std::thread::sleep(Duration::from_millis(120));
        self.inner.embed_batch(inputs, cancel, usage, deadline)
    }
}

struct Rig {
    _fx: Fixture,
    stack: Arc<SemanticStack>,
    monitor: Arc<ResourceMonitor>,
    bg: Option<attic_semantic::BackgroundEnricher>,
}

impl Rig {
    /// Enqueue the fixture's units, force Emergency with `available_mib`
    /// free, and start the background enricher with one-item batches so
    /// progress is observable incrementally.
    fn start(provider: Arc<BackendAs>, available_mib: u64, unified: bool) -> Self {
        let fx = Fixture::bootstrap();
        let stack = Arc::new(
            SemanticStack::open(
                &fx.dir.path().join("semantic.db"),
                provider.clone() as Arc<dyn SemanticProvider>,
            )
            .expect("semantic stack"),
        );
        {
            let conn = fx.read_conn();
            let report = attic_semantic::reconcile(
                &conn,
                &stack.store,
                stack.provider.as_ref(),
                &attic_semantic::SelectionConfig {
                    min_score: 0.0,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(report.enqueued >= 4, "fixture must queue work: {report:?}");
        }

        let monitor = Arc::new(ResourceMonitor::new());
        monitor.apply_resource_policy(4, 1, 1);
        monitor.set_forced_pressure_for_testing(Some(ResourcePressure::Emergency));
        monitor.set_system_memory_for_testing(32_768, 32_768 - available_mib, available_mib);
        assert!(monitor.is_emergency());

        let bg = attic_semantic::BackgroundEnricher::spawn(
            fx.db_path.clone(),
            stack.store.clone(),
            stack.provider.clone(),
            EnrichmentConfig {
                gpu_unified_memory: unified,
                ..EnrichmentConfig::standalone(1, 3, 50, 1)
            },
            Some(monitor.clone()),
            Arc::new(std::sync::atomic::AtomicU64::new(0)),
        );
        Self {
            _fx: fx,
            stack,
            monitor,
            bg: Some(bg),
        }
    }

    fn done(&self) -> u64 {
        self.stack.store.queue_counts().unwrap().done
    }

    fn wait_for_progress(&self, from: u64, within: Duration) -> bool {
        let until = Instant::now() + within;
        while Instant::now() < until {
            if self.done() > from {
                return true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        false
    }

    /// No item completes during `window`, after letting any drive slice that
    /// was already admitted finish — and work is still pending, so the pause
    /// is real rather than an empty queue.
    fn stays_parked(&self, window: Duration) -> bool {
        std::thread::sleep(Duration::from_millis(600));
        let before = self.done();
        std::thread::sleep(window);
        let pending = self.stack.store.queue_counts().unwrap().pending;
        assert!(
            pending > 0,
            "queue drained; the pause check would be vacuous"
        );
        self.done() == before
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if let Some(bg) = self.bg.take() {
            assert!(bg.shutdown(Duration::from_secs(10)), "enricher must stop");
        }
    }
}

const ABOVE_BOTH_FLOORS: u64 = 3_000;

#[test]
fn dedicated_gpu_keeps_embedding_under_host_ram_emergency() {
    let rig = Rig::start(
        BackendAs::new(ExecutionBackend::OrtDirectMl),
        ABOVE_BOTH_FLOORS,
        false,
    );
    assert!(
        rig.wait_for_progress(0, Duration::from_secs(10)),
        "dedicated GPU must not be parked by host-RAM Emergency"
    );
}

#[test]
fn cpu_backend_still_parks_under_host_ram_emergency() {
    let rig = Rig::start(
        BackendAs::new(ExecutionBackend::CandleCpu),
        ABOVE_BOTH_FLOORS,
        false,
    );
    assert!(rig.stays_parked(Duration::from_millis(1_000)));
    assert_eq!(rig.done(), 0);
}

#[test]
fn dedicated_gpu_parks_below_its_host_floor() {
    let rig = Rig::start(
        BackendAs::new(ExecutionBackend::CandleCuda),
        DEDICATED_GPU_HOST_FLOOR_MIB - 1,
        false,
    );
    assert!(rig.stays_parked(Duration::from_millis(1_000)));
    assert_eq!(rig.done(), 0);
}

#[test]
fn unified_memory_gpu_runs_above_its_floor_and_parks_below_it() {
    // Apple Metal: unified memory, runs above the 700 MiB floor.
    let rig = Rig::start(
        BackendAs::new(ExecutionBackend::CandleMetal),
        ABOVE_BOTH_FLOORS,
        false,
    );
    assert!(rig.wait_for_progress(0, Duration::from_secs(10)));

    // Drop below the unified floor (still above the dedicated one): parks.
    rig.monitor.set_system_memory_for_testing(
        32_768,
        32_768 - (UNIFIED_GPU_HOST_FLOOR_MIB - 1),
        UNIFIED_GPU_HOST_FLOOR_MIB - 1,
    );
    assert!(rig.stays_parked(Duration::from_millis(1_000)));
}

#[test]
fn integrated_directml_adapter_is_gated_as_unified_memory() {
    let rig = Rig::start(
        BackendAs::new(ExecutionBackend::OrtDirectMl),
        UNIFIED_GPU_HOST_FLOOR_MIB - 1,
        true,
    );
    assert!(rig.stays_parked(Duration::from_millis(1_000)));
    assert_eq!(rig.done(), 0);
}

#[test]
fn runtime_backend_switch_is_honoured_live() {
    let provider = BackendAs::new(ExecutionBackend::CandleCpu);
    let rig = Rig::start(provider.clone(), ABOVE_BOTH_FLOORS, false);
    assert!(rig.stays_parked(Duration::from_millis(800)), "CPU parks");

    // GPU comes back (or was only just loaded): embedding resumes.
    provider.set(ExecutionBackend::OrtDirectMl);
    assert!(rig.wait_for_progress(rig.done(), Duration::from_secs(10)));

    // Demoted to CPU mid-run: the host-RAM gate applies again.
    provider.set(ExecutionBackend::CandleCpu);
    assert!(rig.stays_parked(Duration::from_millis(1_000)));
}

/// GPU backends commit batch N on a separate thread while batch N+1 embeds.
/// Every claimed item must be committed exactly once, nothing may be left
/// INFLIGHT, and the result must match the inline (CPU) path.
#[test]
fn pipelined_gpu_drive_commits_every_batch_exactly_once() {
    fn run(backend: ExecutionBackend) -> (u64, attic_semantic::QueueCounts, usize) {
        let fx = Fixture::bootstrap();
        let stack = SemanticStack::open(
            &fx.dir.path().join("semantic.db"),
            BackendAs::new(backend) as Arc<dyn SemanticProvider>,
        )
        .expect("semantic stack");
        let conn = fx.read_conn();
        let report = attic_semantic::reconcile(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &attic_semantic::SelectionConfig {
                min_score: 0.0,
                ..Default::default()
            },
        )
        .unwrap();
        let stats = attic_semantic::drive(
            &conn,
            &stack.store,
            stack.provider.as_ref(),
            &EnrichmentConfig::standalone(2, 3, 120_000, 1),
            &CancelFlag::new(),
        )
        .unwrap();
        (
            stats.embedded,
            stack.store.queue_counts().unwrap(),
            report.enqueued,
        )
    }

    let (gpu_embedded, gpu, enqueued) = run(ExecutionBackend::OrtDirectMl);
    assert!(enqueued >= 4);
    assert_eq!(gpu.inflight, 0, "pipelined commit leaked INFLIGHT items");
    assert_eq!(gpu.pending, 0, "{gpu:?}");
    assert_eq!(gpu.failed, 0, "{gpu:?}");
    assert_eq!(gpu.done, enqueued as u64, "{gpu:?}");
    assert_eq!(
        gpu_embedded, gpu.done,
        "stats must count committed items once"
    );

    let (cpu_embedded, cpu, _) = run(ExecutionBackend::CandleCpu);
    assert_eq!(cpu.done, gpu.done);
    assert_eq!(cpu_embedded, gpu_embedded);
}
