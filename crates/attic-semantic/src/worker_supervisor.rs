//! SemanticProvider adapter over the supervised inference worker (r06).
//!
//! Neural inference (Candle Qwen3, ORT/DirectML) runs in a child process so
//! a hung or crashed native runtime is killable and never wedges the MCP
//! server. This adapter preserves the `SemanticProvider` contract: typed
//! errors map from the worker's error classes, and a worker timeout kills
//! the child and surfaces as a typed failure the enrichment layer can retry
//! against a freshly restarted worker.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use attic_inference_protocol::supervisor::{
    LoadParams, SupervisorError, WorkerLaunch, WorkerSupervisor,
};
use attic_inference_protocol::{EmbedItem, WorkerErrorClass};

use crate::error::SemanticError;
use crate::provider::{
    CancelFlag, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput, ProviderConcurrencyContract,
    ResourceUsage, SemanticProvider,
};

/// How long a single embedding batch may run inside the worker before the
/// supervisor kills it. Generous for cold starts; the resource governor
/// (r08) tightens effective behavior via batch size, not this bound.
///
/// Derived from [`crate::diagnostics::EMBED_DEADLINE_SECS`] so stall
/// diagnostics and the actual kill deadline can never drift apart. They did:
/// status declared "STALLED — restart the embedding worker" at 120s while the
/// supervisor would not kill the hung worker until 300s, so for three minutes
/// operators were told to intervene manually in a situation that recovers on
/// its own.
const EMBED_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(crate::diagnostics::EMBED_DEADLINE_SECS);

/// The batch deadline the supervisor actually enforces.
///
/// Exposed so diagnostics can assert that the number quoted to operators is
/// the number that governs the kill, rather than trusting two constants to
/// stay in sync by convention.
pub(crate) fn embed_deadline() -> std::time::Duration {
    EMBED_DEADLINE
}

pub struct SupervisedWorkerProvider {
    supervisor: WorkerSupervisor,
    fingerprint: EmbeddingFingerprint,
    max_input_bytes: usize,
    ready: AtomicBool,
    /// Set once a load attempt has actually failed (handshake, model load,
    /// or identity mismatch) — distinct from "never attempted yet". Without
    /// this, gating `available()` purely on `ready` deadlocks retrieval: the
    /// query path's cheap availability probe (`SemanticCandidateGenerator`)
    /// runs BEFORE ever calling `embed_batch`, which is the only place
    /// `ready` gets set, so a freshly-restarted-but-never-yet-queried
    /// worker would report unavailable forever and never get the one real
    /// attempt that would actually set `ready = true`.
    load_failed: AtomicBool,
    /// Queue items per `embed_batch` for backends that bucket internally.
    claim_items: Option<usize>,
}

/// Items claimed per GPU batch call. The DirectML provider packs these into
/// length buckets under a token budget (up to 64 short items per forward
/// pass), so it needs a claim wide enough to fill several full passes.
/// Host cost is only the claimed texts; device memory is bounded by the
/// provider's token budget, not by this number.
pub const GPU_CLAIM_ITEMS: usize = 128;

/// Worker env: padded tokens per GPU forward pass (`[semantic] gpu_batch_tokens`).
pub const ENV_GPU_BATCH_TOKENS: &str = "ATTIC_GPU_BATCH_TOKENS";
/// Worker env: GPU pause temperature in °C (`[semantic] gpu_temp_pause_c`).
pub const ENV_GPU_TEMP_PAUSE_C: &str = "ATTIC_GPU_TEMP_PAUSE_C";
/// Worker env: GPU resume temperature in °C (`[semantic] gpu_temp_resume_c`).
pub const ENV_GPU_TEMP_RESUME_C: &str = "ATTIC_GPU_TEMP_RESUME_C";

impl SupervisedWorkerProvider {
    /// Create the provider; spawns nothing yet (lazy start on first embed so
    /// server startup never blocks on the worker or model load).
    pub fn new(
        launch: WorkerLaunch,
        load: LoadParams,
        fingerprint: EmbeddingFingerprint,
        max_input_bytes: usize,
    ) -> Self {
        let claim_items = (load.backend == "ort-directml").then_some(GPU_CLAIM_ITEMS);
        let supervisor = WorkerSupervisor::new(launch);
        // Remember load params immediately so lazy restart works.
        supervisor.load_model_params_only(load);
        // The worker reports its ACTUAL identity on every load (first load
        // and every lazy restart); reject any mismatch against what this
        // provider was constructed to expect rather than assuming the
        // worker loaded what was asked for.
        let expected = fingerprint.clone();
        supervisor.set_identity_verifier(move |caps: &[String]| {
            verify_identity_capabilities(&expected, caps)
        });
        Self {
            supervisor,
            fingerprint,
            max_input_bytes,
            ready: AtomicBool::new(false),
            load_failed: AtomicBool::new(false),
            claim_items,
        }
    }

    fn ensure_ready(&self) -> Result<(), SemanticError> {
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = self
            .supervisor
            .handshake()
            .map_err(map_supervisor_error)
            .and_then(|_| {
                self.supervisor
                    .load_model_remembered()
                    .map_err(map_supervisor_error)
            });
        match result {
            Ok(()) => {
                self.load_failed.store(false, Ordering::Release);
                self.ready.store(true, Ordering::Release);
                Ok(())
            }
            Err(e) => {
                self.load_failed.store(true, Ordering::Release);
                Err(e)
            }
        }
    }
}

fn map_supervisor_error(e: SupervisorError) -> SemanticError {
    match e {
        SupervisorError::WorkerTimeout(d) => SemanticError::EmbeddingFailed(format!(
            "inference worker exceeded {d:?} deadline and was killed; it will restart on the next batch"
        )),
        SupervisorError::WorkerDied => SemanticError::EmbeddingFailed(
            "inference worker died mid-batch; it will restart on the next batch".into(),
        ),
        SupervisorError::Engine { class, message } => match class {
            WorkerErrorClass::OutOfMemory => SemanticError::BudgetExhausted(message),
            WorkerErrorClass::ResourcePressure => SemanticError::DevicePressure(message),
            WorkerErrorClass::InvalidInput => SemanticError::EmbeddingFailed(message),
            WorkerErrorClass::Artifact => SemanticError::ProviderUnavailable {
                provider: "inference-worker".into(),
                reason: message,
            },
            _ => SemanticError::EmbeddingFailed(message),
        },
        other => SemanticError::ProviderUnavailable {
            provider: "inference-worker".into(),
            reason: other.to_string(),
        },
    }
}

impl SemanticProvider for SupervisedWorkerProvider {
    fn id(&self) -> &'static str {
        "qwen3-supervised"
    }

    fn model_id(&self) -> &str {
        "qwen3-embedding-0.6b"
    }

    fn dimensions(&self) -> usize {
        self.fingerprint.dimension
    }

    fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }

    fn available(&self) -> bool {
        // Truthful readiness (Phase 4): "available" means "not known to be
        // broken", not "has already loaded". A worker that hasn't been
        // tried yet gets the benefit of the doubt so the first real query
        // or drive() call can actually attempt the one load that proves it
        // one way or the other; a worker whose load has genuinely failed
        // must never be reported as active.
        !self.load_failed.load(Ordering::Acquire)
    }

    fn concurrency_contract(&self) -> ProviderConcurrencyContract {
        // One worker process owns the model/device session.
        ProviderConcurrencyContract::Serialized
    }

    fn fingerprint(&self) -> Option<EmbeddingFingerprint> {
        Some(self.fingerprint.clone())
    }

    fn preferred_claim_items(&self) -> Option<usize> {
        self.claim_items
    }

    fn embed_batch(
        &self,
        inputs: &[EmbeddingInput],
        cancel: &CancelFlag,
        usage: &mut ResourceUsage,
        deadline: Option<Instant>,
    ) -> Result<Vec<EmbeddingOutput>, SemanticError> {
        if cancel.is_cancelled() {
            return Err(SemanticError::Cancelled {
                completed: 0,
                total: inputs.len(),
            });
        }
        let warmup_started = Instant::now();
        self.ensure_ready()?;
        let warmup_cost = warmup_started.elapsed();

        let items: Vec<EmbedItem> = inputs
            .iter()
            .map(|i| EmbedItem {
                key: i.unit_key.clone(),
                text: i.text.clone(),
            })
            .collect();
        let keys: Vec<String> = inputs.iter().map(|i| i.unit_key.clone()).collect();

        let effective_deadline = match deadline {
            Some(d) => {
                // Refund whatever a COLD MODEL LOAD just consumed.
                //
                // The caller's deadline starts before `ensure_ready()`, which
                // is where a cold worker loads the model — for a 1.1 GB fp16
                // ONNX export on DirectML that can take minutes. The remaining
                // budget then reached zero before a single item was embedded,
                // so the batch returned `Cancelled { completed: 0 }`. Three of
                // those trip the fallback coordinator's consecutive-failure
                // threshold and the GPU is abandoned for CPU permanently —
                // observed as "gpu provider failed 3 consecutive times:
                // embedding batch cancelled after 0 of 4 items" on a machine
                // whose GPU was working perfectly.
                //
                // This deadline exists to catch a HUNG INFERENCE CALL. Warm-up
                // is a legitimate one-time cost with its own separate budget
                // (the supervisor's load roundtrip), so charging it here
                // punished the GPU for being cold rather than for being stuck.
                // Refunding it leaves the hang detection intact — the budget
                // is still capped at EMBED_DEADLINE and a warm worker is
                // completely unaffected, since `warmup_cost` is then ~zero.
                let remaining = d.saturating_duration_since(Instant::now());
                match inference_budget(remaining, warmup_cost) {
                    Some(b) => b,
                    None => {
                        return Err(SemanticError::Cancelled {
                            completed: 0,
                            total: inputs.len(),
                        });
                    }
                }
            }
            None => EMBED_DEADLINE,
        };

        let vectors = match self.supervisor.embed_batch(items, effective_deadline) {
            Ok(v) => v,
            Err(e) => {
                // `ready` only reflects the state as of the last successful
                // `ensure_ready()` and is otherwise never touched — without
                // this, one successful load at startup would make
                // `available()` report healthy forever, even through a
                // later worker crash/identity-mismatch-on-restart that
                // keeps failing every subsequent call. An OOM is a
                // per-batch resource signal the adaptive batch-cap halving
                // in `enrich.rs` already owns, not evidence the worker
                // itself is broken, so it alone does not flip readiness.
                if !matches!(
                    e,
                    SupervisorError::Engine {
                        class: WorkerErrorClass::OutOfMemory,
                        ..
                    }
                ) {
                    self.load_failed.store(true, Ordering::Release);
                    self.ready.store(false, Ordering::Release);
                }
                return Err(map_supervisor_error(e));
            }
        };

        if vectors.len() != keys.len() {
            return Err(SemanticError::EmbeddingFailed(format!(
                "worker returned {} vectors for {} inputs",
                vectors.len(),
                keys.len()
            )));
        }

        usage.input_bytes += inputs.iter().map(|i| i.text.len() as u64).sum::<u64>();
        usage.items_embedded += keys.len() as u64;
        Ok(keys
            .into_iter()
            .zip(vectors)
            .map(|(unit_key, vector)| EmbeddingOutput { unit_key, vector })
            .collect())
    }
}

/// The fingerprint the worker's provider WILL have once loaded — needed
/// before the child exists so enrichment can key generations. Must match
/// the in-worker providers exactly (qwen3_provider / ort_directml); the
/// real provider reports its fingerprint after load and a mismatch would
/// produce a new vector space rather than silent mixing.
pub fn expected_fingerprint(backend: &str, dimension: Option<usize>) -> EmbeddingFingerprint {
    use crate::provider::ExecutionBackend;
    let base = EmbeddingFingerprint {
        provider: crate::qwen3_provider::QWEN_PROVIDER_ID.into(),
        model_id: crate::qwen3_provider::QWEN_MODEL_ID.into(),
        model_revision: String::new(),
        dimension: dimension.unwrap_or(1024),
        pooling_version: "last_token_v1".into(),
        normalization_version: "l2_unit_v1".into(),
        tokenizer_version: "qwen_bpe_v1".into(),
        chunking_version: attic_core::constants::CHUNKING_VERSION.into(),
        query_instruction_version: crate::instruction::CODE_RETRIEVAL_V1_ID.into(),
        execution_backend: ExecutionBackend::Unknown,
        quantization: String::new(),
    };
    match backend {
        "ort-directml" => EmbeddingFingerprint {
            // Must match `ort_directml::ORT_PROVIDER_ID`; kept as a literal
            // here because that module is `#[cfg(feature = "ort-directml")]`
            // and this function must resolve regardless of feature flags.
            provider: "qwen3-ort".into(),
            model_revision: "onnx-community-fp16".into(),
            execution_backend: ExecutionBackend::OrtDirectMl,
            quantization: "fp16-onnx".into(),
            ..base
        },
        _ => EmbeddingFingerprint {
            model_revision: crate::model_assets::ModelManifest::qwen3_default().pinned_revision,
            // Telemetry only (not identity-checked — see
            // `verify_identity_capabilities`). Reported accurately for the
            // requested device so status output is truthful, while still
            // permitting a CPU fallback at load time.
            execution_backend: match backend {
                "candle-cuda" => ExecutionBackend::CandleCuda,
                "candle-metal" => ExecutionBackend::CandleMetal,
                _ => ExecutionBackend::CandleCpu,
            },
            quantization: "fp32-safetensors".into(),
            ..base
        },
    }
}

/// Wire encoding of a worker's ACTUAL loaded [`EmbeddingFingerprint`],
/// carried in the LoadModel response's `capabilities` (a free-form
/// `Vec<String>` — the protocol layer stays free of any provider-identity
/// type). One `"fp:<field>:<value>"` entry per fingerprint field.
pub fn fingerprint_capabilities(fp: &EmbeddingFingerprint) -> Vec<String> {
    vec![
        format!("fp:provider:{}", fp.provider),
        format!("fp:model_id:{}", fp.model_id),
        format!("fp:model_revision:{}", fp.model_revision),
        format!("fp:dimension:{}", fp.dimension),
        format!("fp:pooling_version:{}", fp.pooling_version),
        format!("fp:normalization_version:{}", fp.normalization_version),
        format!("fp:tokenizer_version:{}", fp.tokenizer_version),
        format!("fp:chunking_version:{}", fp.chunking_version),
        format!(
            "fp:query_instruction_version:{}",
            fp.query_instruction_version
        ),
        format!("fp:execution_backend:{}", fp.execution_backend.as_str()),
        format!("fp:quantization:{}", fp.quantization),
    ]
}

fn capability_value<'a>(caps: &'a [String], field: &str) -> Option<&'a str> {
    let prefix = format!("fp:{field}:");
    caps.iter().find_map(|c| c.strip_prefix(prefix.as_str()))
}

/// Compare the actual identity a worker reported on load (via
/// [`fingerprint_capabilities`]) against the identity expected before the
/// worker existed ([`expected_fingerprint`]). Every mismatched or missing
/// field is collected and rejected explicitly — persistence must never be
/// keyed off an assumed identity the loaded worker didn't actually report.
pub fn verify_identity_capabilities(
    expected: &EmbeddingFingerprint,
    caps: &[String],
) -> Result<(), String> {
    let mut mismatches = Vec::new();
    let mut check = |field: &str, expected_val: &str| match capability_value(caps, field) {
        Some(actual) if actual == expected_val => {}
        Some(actual) => mismatches.push(format!(
            "{field}: expected '{expected_val}', got '{actual}'"
        )),
        None => mismatches.push(format!("{field}: missing from worker response")),
    };
    check("provider", &expected.provider);
    check("model_id", &expected.model_id);
    check("model_revision", &expected.model_revision);
    check("dimension", &expected.dimension.to_string());
    check("pooling_version", &expected.pooling_version);
    check("normalization_version", &expected.normalization_version);
    check("tokenizer_version", &expected.tokenizer_version);
    check("chunking_version", &expected.chunking_version);
    check(
        "query_instruction_version",
        &expected.query_instruction_version,
    );
    // `execution_backend` is deliberately NOT checked.
    //
    // It is telemetry (see `provider::ExecutionBackend`), and comparing it
    // here actively breaks GPU fallback: a host configured for `candle-cuda`
    // that legitimately degrades to CPU (no NVIDIA device, or a binary built
    // without the `candle-cuda` feature) reports `candle-cpu`, which would be
    // rejected as an identity mismatch — turning a graceful, intended
    // fallback into a total semantic-embedding outage.
    //
    // Nothing is weakened by dropping it. The vector spaces that genuinely
    // must not mix are already separated by fields that ARE checked:
    //   - CPU/CUDA/Metal all run identical F32 safetensors  -> same space,
    //     differing only by float non-determinism (a parity question).
    //   - DirectML is fp16 ONNX                             -> already split
    //     by `provider` (qwen3-ort), `quantization` (fp16-onnx) and
    //     `model_revision` (onnx-community-fp16).
    check("quantization", &expected.quantization);
    if mismatches.is_empty() {
        Ok(())
    } else {
        Err(mismatches.join("; "))
    }
}

/// How long this batch may spend in *inference*, given the caller's remaining
/// budget and what a cold model load just cost.
///
/// `warmup_cost` is refunded. The caller's deadline starts before the worker
/// loads its model, so a cold load can consume the entire budget and leave a
/// batch cancelled having embedded nothing — which the fallback coordinator
/// counts as a provider failure and, after three, abandons the GPU for good.
/// This deadline is a hang detector for inference; warm-up is a legitimate
/// one-time cost with its own budget, so it must not be charged here.
///
/// Returns `None` when the budget is genuinely exhausted.
fn inference_budget(remaining: Duration, warmup_cost: Duration) -> Option<Duration> {
    let budget = remaining.saturating_add(warmup_cost).min(EMBED_DEADLINE);
    (!budget.is_zero()).then_some(budget)
}

/// The max_input_bytes the worker's provider will enforce, without loading
/// the model (mirrors each provider's contract).
pub fn expected_max_input_bytes(backend: &str, seq_len: usize) -> usize {
    match backend {
        // DirectML: fixed sequence length x conservative bytes-per-token.
        "ort-directml" => seq_len * 2,
        // Candle: DEFAULT_MAX_TOKENS (1024) x MIN_BYTES_PER_TOKEN (2).
        _ => 1024 * 2,
    }
}

#[cfg(test)]
mod expected_fingerprint_tests {
    use super::*;

    /// A recoverable device condition must survive the IPC boundary as a
    /// recoverable condition. It previously arrived as `Internal` ->
    /// `EmbeddingFailed`, whose transient check is substring-based and did
    /// not match "resource pressure", so every occurrence burned a retry and
    /// four units were permanently quarantined by a passing VRAM spike.
    #[test]
    fn device_pressure_survives_the_worker_boundary() {
        let mapped = map_supervisor_error(SupervisorError::Engine {
            class: WorkerErrorClass::ResourcePressure,
            message: "sustained critical VRAM pressure".into(),
        });
        assert!(
            matches!(mapped, SemanticError::DevicePressure(_)),
            "resource pressure must stay typed, got {mapped:?}"
        );
    }

    #[test]
    fn candle_cpu_provider_matches_qwen3_provider_id() {
        let fp = expected_fingerprint("candle-cpu", Some(1024));
        assert_eq!(fp.provider, crate::qwen3_provider::QWEN_PROVIDER_ID);
        assert_eq!(
            fp.execution_backend,
            crate::provider::ExecutionBackend::CandleCpu
        );
    }

    #[test]
    fn ort_directml_provider_matches_real_worker_provider_id() {
        // Must match `ort_directml::ORT_PROVIDER_ID` ("qwen3-ort") exactly —
        // this is the value the real DirectML worker reports on load, and a
        // mismatch here previously caused every DirectML load to be rejected
        // as an identity mismatch.
        let fp = expected_fingerprint("ort-directml", Some(1024));
        assert_eq!(fp.provider, "qwen3-ort");
        assert_ne!(fp.provider, crate::qwen3_provider::QWEN_PROVIDER_ID);
        assert_eq!(
            fp.execution_backend,
            crate::provider::ExecutionBackend::OrtDirectMl
        );
    }

    #[test]
    fn provider_is_available_before_first_attempt_and_unavailable_after_a_real_failure() {
        // `available()` means "not known to be broken", not "has already
        // loaded" — a never-tried worker gets the benefit of the doubt (so
        // the first real query/drive() call can make the one attempt that
        // actually proves it works), but a genuinely failed load must never
        // be reported as active.
        let launch = WorkerLaunch {
            program: std::path::PathBuf::from("attic-inference-worker-does-not-exist"),
            args: vec![],
            env: vec![],
        };
        let load = LoadParams {
            cache_dir: "unused".into(),
            batch_size: 1,
            dimension: Some(4),
            backend: "candle-cpu".into(),
            onnx_model_dir: None,
            seq_len: None,
        };
        let fp = expected_fingerprint("candle-cpu", Some(4));
        let provider = SupervisedWorkerProvider::new(launch, load, fp, 4096);
        assert!(
            provider.available(),
            "a never-attempted worker must be optimistically available"
        );

        let cancel = CancelFlag::new();
        let mut usage = ResourceUsage::default();
        let inputs = [EmbeddingInput {
            unit_key: "u1".into(),
            text: "hello".into(),
        }];
        let err = provider.embed_batch(&inputs, &cancel, &mut usage, None);
        assert!(err.is_err(), "spawning a nonexistent program must fail");
        assert!(
            !provider.available(),
            "a worker whose load attempt genuinely failed must report unavailable"
        );
    }

    /// The shipping bug this guards: a cold DirectML load ate the caller's
    /// whole embed deadline, so the batch was cancelled having embedded 0 of
    /// 4 items. Three of those tripped the fallback coordinator and the GPU
    /// was abandoned for CPU permanently — on a machine whose GPU was fine.
    #[test]
    fn a_cold_model_load_does_not_consume_the_inference_budget() {
        // Caller's budget fully consumed by warm-up.
        let budget = inference_budget(Duration::ZERO, Duration::from_secs(280))
            .expect("a cold load must not leave zero inference budget");
        assert_eq!(budget, Duration::from_secs(280));
    }

    #[test]
    fn a_warm_worker_is_unaffected_by_the_refund() {
        let remaining = Duration::from_secs(120);
        assert_eq!(
            inference_budget(remaining, Duration::ZERO),
            Some(remaining),
            "a warm worker pays no warm-up, so its budget must be unchanged"
        );
    }

    #[test]
    fn the_refund_is_still_capped_at_the_embed_deadline() {
        // The refund must not become an unbounded budget: a genuinely hung
        // inference call still has to be caught.
        let budget = inference_budget(EMBED_DEADLINE, EMBED_DEADLINE).unwrap();
        assert_eq!(budget, EMBED_DEADLINE);
    }

    #[test]
    fn an_exhausted_budget_with_no_warmup_is_still_exhausted() {
        assert_eq!(inference_budget(Duration::ZERO, Duration::ZERO), None);
    }

    #[test]
    fn gpu_request_that_falls_back_to_cpu_is_still_accepted() {
        // The exact shipping bug this guards: a host configured for CUDA
        // that has no NVIDIA device (or a binary built without the feature)
        // loads on CPU and reports `candle-cpu`. If identity verification
        // compared execution_backend, this would be rejected and semantic
        // embedding would fail completely rather than degrading.
        let expected = expected_fingerprint("candle-cuda", Some(1024));
        assert_eq!(
            expected.execution_backend,
            crate::provider::ExecutionBackend::CandleCuda,
            "the request should still be reported accurately as telemetry"
        );

        // What the worker actually reports after falling back to CPU.
        let actual_cpu = expected_fingerprint("candle-cpu", Some(1024));
        let caps = fingerprint_capabilities(&actual_cpu);

        verify_identity_capabilities(&expected, &caps).expect(
            "a CUDA request that degrades to CPU must remain identity-compatible: \
             same weights, same F32 dtype, same vector space",
        );
    }

    #[test]
    fn metal_fallback_to_cpu_is_also_accepted() {
        let expected = expected_fingerprint("candle-metal", Some(1024));
        assert_eq!(
            expected.execution_backend,
            crate::provider::ExecutionBackend::CandleMetal
        );
        let caps = fingerprint_capabilities(&expected_fingerprint("candle-cpu", Some(1024)));
        verify_identity_capabilities(&expected, &caps)
            .expect("an Apple Silicon request degrading to CPU must stay compatible");
    }

    #[test]
    fn directml_is_still_rejected_against_a_candle_vector_space() {
        // Dropping the execution_backend check must NOT let genuinely
        // incompatible spaces mix. fp16 ONNX vectors and fp32 safetensors
        // vectors are different spaces and are still separated by
        // provider/quantization/model_revision.
        let expected = expected_fingerprint("candle-cpu", Some(1024));
        let caps = fingerprint_capabilities(&expected_fingerprint("ort-directml", Some(1024)));
        let err = verify_identity_capabilities(&expected, &caps)
            .expect_err("fp16 ONNX must never be accepted into an fp32 candle space");
        assert!(err.contains("quantization"), "err: {err}");
        assert!(err.contains("provider"), "err: {err}");
    }

    #[test]
    fn every_fingerprint_field_differs_when_expected() {
        let cpu = expected_fingerprint("candle-cpu", Some(1024));
        let gpu = expected_fingerprint("ort-directml", Some(1024));
        assert_ne!(cpu.provider, gpu.provider);
        assert_ne!(cpu.model_revision, gpu.model_revision);
        assert_ne!(cpu.execution_backend, gpu.execution_backend);
        assert_ne!(cpu.quantization, gpu.quantization);
    }
}
