//! Worker-side engine abstraction and the stdin/stdout serving loop (r06).
//!
//! The loop is transport-complete and engine-agnostic: `attic-server`'s
//! `inference-worker` subcommand supplies the real neural engine; tests use
//! the deterministic `MockEngine` (echo / hang / corrupt / crash modes) so
//! supervisor fault-injection needs no GPU, no model, and no network.

use crate::{
    EmbedItem, PROTOCOL_VERSION, WorkerErrorClass, WorkerRequest, WorkerResponse, read_request,
    write_response,
};
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// What the worker process can actually do once a model is loaded.
pub struct EngineInfo {
    pub backend: String,
    pub capabilities: Vec<String>,
    pub dimension: usize,
    pub max_input_bytes: usize,
}

/// The provider-agnostic boundary the serving loop drives.
pub trait WorkerEngine {
    /// Human/machine backend label reported in the Hello handshake.
    fn backend_name(&self) -> &str;
    /// Load the model; returns engine info for the supervisor.
    fn load(&mut self, req: &LoadSpec) -> Result<EngineInfo, WorkerFail>;
    /// Embed one batch under a cooperative wall-clock deadline.
    fn embed(
        &mut self,
        items: &[EmbedItem],
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, WorkerFail>;
}

/// Load parameters from the supervisor's LoadModel request.
pub struct LoadSpec {
    pub cache_dir: String,
    pub batch_size: usize,
    pub dimension: Option<usize>,
    pub onnx_model_dir: Option<String>,
    pub seq_len: Option<usize>,
    /// Backend selector: "candle-cpu" | "ort-directml".
    pub backend: String,
}

/// A typed engine failure — maps 1:1 onto the wire error classes.
#[derive(Debug)]
pub struct WorkerFail {
    pub class: WorkerErrorClass,
    pub message: String,
}

impl WorkerFail {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            class: WorkerErrorClass::Internal,
            message: msg.into(),
        }
    }
    pub fn artifact(msg: impl Into<String>) -> Self {
        Self {
            class: WorkerErrorClass::Artifact,
            message: msg.into(),
        }
    }
}

/// How often the worker sends a heartbeat while an embed runs (only when
/// the engine ticked or is paused since the last one).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(1000);

/// Serve requests on `input`/`output` until Shutdown or EOF. Returns the
/// process exit code. All panics inside the engine are caught per request
/// and reported as Internal errors — a worker must never die silently while
/// the supervisor still believes it is healthy.
pub fn run_worker_loop(
    mut engine: impl WorkerEngine,
    input: impl Read,
    output: impl Write + Send,
) -> i32 {
    let mut reader = BufReader::new(input);
    let writer = Mutex::new(BufWriter::new(output));
    let send = |resp: &WorkerResponse| {
        let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
        write_response(&mut *w, resp)
    };
    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => return 0, // clean EOF: supervisor closed the pipe
            Err(e) => {
                eprintln!("worker: protocol read failed: {e}");
                return 2;
            }
        };
        let resp = match req {
            WorkerRequest::Hello { id } => WorkerResponse::HelloOk {
                id,
                protocol: PROTOCOL_VERSION,
                backend: engine.backend_name().to_string(),
                capabilities: vec![],
            },
            WorkerRequest::LoadModel {
                id,
                cache_dir,
                batch_size,
                dimension,
                backend,
                onnx_model_dir,
                seq_len,
            } => {
                let spec = LoadSpec {
                    cache_dir,
                    batch_size,
                    dimension,
                    onnx_model_dir,
                    seq_len,
                    backend,
                };
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| engine.load(&spec)))
                {
                    Ok(Ok(info)) => WorkerResponse::HelloOk {
                        id,
                        protocol: PROTOCOL_VERSION,
                        backend: info.backend,
                        capabilities: {
                            let mut c = info.capabilities;
                            c.push(format!("dim:{}", info.dimension));
                            c.push(format!("max_input_bytes:{}", info.max_input_bytes));
                            c
                        },
                    },
                    Ok(Err(f)) => WorkerResponse::Error {
                        id,
                        class: f.class,
                        message: f.message,
                    },
                    Err(_) => WorkerResponse::Error {
                        id,
                        class: WorkerErrorClass::Internal,
                        message: "engine panicked during model load".into(),
                    },
                }
            }
            WorkerRequest::EmbedBatch {
                id,
                items,
                deadline_ms,
            } => {
                let started = Instant::now();
                let deadline = Duration::from_millis(deadline_ms);
                let done = AtomicBool::new(false);
                let result = std::thread::scope(|s| {
                    let beat = s.spawn(|| {
                        let (mut last_ticks, _) = crate::progress::snapshot();
                        while !done.load(Ordering::Acquire) {
                            std::thread::park_timeout(HEARTBEAT_INTERVAL);
                            if done.load(Ordering::Acquire) {
                                break;
                            }
                            let (ticks, paused) = crate::progress::snapshot();
                            if ticks != last_ticks || paused {
                                last_ticks = ticks;
                                if send(&WorkerResponse::Progress { id, ticks, paused }).is_err() {
                                    break;
                                }
                            }
                        }
                    });
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        engine.embed(&items, deadline)
                    }));
                    done.store(true, Ordering::Release);
                    // Wake the heartbeat now so the response is not held
                    // back by up to one heartbeat interval.
                    beat.thread().unpark();
                    r
                });
                match result {
                    Ok(Ok(vectors)) => WorkerResponse::Embeddings {
                        id,
                        items_embedded: vectors.len() as u64,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        vectors,
                    },
                    Ok(Err(f)) => WorkerResponse::Error {
                        id,
                        class: f.class,
                        message: f.message,
                    },
                    Err(_) => WorkerResponse::Error {
                        id,
                        class: WorkerErrorClass::Internal,
                        message: "engine panicked during embed".into(),
                    },
                }
            }
            WorkerRequest::Shutdown { id } => {
                let _ = send(&WorkerResponse::Ok { id });
                return 0;
            }
        };
        if send(&resp).is_err() {
            return 3; // supervisor's pipe is gone; nothing left to do
        }
    }
}

// ── Mock engine for supervisor fault-injection tests ───────────────────────

/// Deterministic worker behavior for tests, selected by `ATTIC_MOCK_WORKER`:
///   echo    — embed = deterministic hash-ish vector, instant
///   hang    — never answer EmbedBatch (supervisor deadline must kill us)
///   slow    — tick progress every 100 ms for 3 s, then answer
///   paused  — report a 3 s deliberate pause, then answer
///   corrupt — answer with a malformed frame (protocol violation)
///   crash   — exit(4) immediately on EmbedBatch (supervisor sees dead child)
#[derive(Default)]
pub struct MockEngine {
    pub mode: String,
    pub loaded: bool,
}

impl WorkerEngine for MockEngine {
    fn backend_name(&self) -> &str {
        "mock"
    }

    fn load(&mut self, _req: &LoadSpec) -> Result<EngineInfo, WorkerFail> {
        self.loaded = true;
        Ok(EngineInfo {
            backend: "mock".into(),
            capabilities: vec!["mock".into()],
            dimension: 4,
            max_input_bytes: 4096,
        })
    }

    fn embed(
        &mut self,
        items: &[EmbedItem],
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, WorkerFail> {
        match self.mode.as_str() {
            "hang" => {
                // Sleep well beyond any test deadline; the supervisor must
                // kill the process rather than wait.
                std::thread::sleep(deadline.max(Duration::from_secs(600)));
                Err(WorkerFail::internal("unreachable"))
            }
            // Busy but alive: ticks every 100 ms for 3 s. A stall watchdog
            // shorter than 3 s must NOT kill this.
            "slow" => {
                for _ in 0..30 {
                    std::thread::sleep(Duration::from_millis(100));
                    crate::progress::tick();
                }
                Ok(items.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
            }
            // Deliberately paused (thermal) for 3 s without ticking; paused
            // time must count neither as a stall nor against the budget.
            "paused" => {
                {
                    let _p = crate::progress::PauseGuard::enter();
                    std::thread::sleep(Duration::from_secs(3));
                }
                crate::progress::tick();
                Ok(items.iter().map(|_| vec![1.0, 0.0, 0.0, 0.0]).collect())
            }
            "crash" => std::process::exit(4),
            _ => Ok(items
                .iter()
                .map(|i| {
                    // Deterministic pseudo-embedding from the text bytes.
                    let h = blake3ish(&i.text);
                    vec![h as f32, (h >> 8) as f32, i.text.len() as f32, 1.0]
                })
                .collect()),
        }
    }
}

fn blake3ish(s: &str) -> u32 {
    // Tiny deterministic mixer — no real dependency needed for a mock.
    let mut h: u32 = 2166136261;
    for b in s.as_bytes() {
        h = h.wrapping_mul(16777619) ^ (*b as u32);
    }
    h
}

/// Entry point used by the `attic-mock-worker` test binary AND by
/// `attic inference-worker` (which passes the real engine instead).
pub fn run_mock_worker_stdio() -> i32 {
    let mode = std::env::var("ATTIC_MOCK_WORKER").unwrap_or_else(|_| "echo".to_string());
    let engine = MockEngine {
        mode,
        loaded: false,
    };
    // Corrupt mode writes garbage framing on the FIRST response.
    if engine.mode == "corrupt" {
        let mut out = BufWriter::new(std::io::stdout());
        // Read and discard the hello frame, then emit a malformed length.
        let mut input = BufReader::new(std::io::stdin());
        let _ = read_request(&mut input);
        let bogus = (crate::MAX_FRAME_BYTES + 1).to_le_bytes();
        let _ = out.write_all(&bogus);
        let _ = out.flush();
        return 0;
    }
    run_worker_loop(engine, std::io::stdin(), std::io::stdout())
}
