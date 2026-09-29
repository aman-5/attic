//! Supervisor: owns the inference worker child process (r06).
//!
//! Safety properties:
//! * The worker is a separate process — a hung native `run()` cannot block
//!   the MCP server. Every request carries a wall-clock deadline; on timeout
//!   the supervisor KILLS the child and answers with a typed
//!   [`SupervisorError::WorkerTimeout`].
//! * A dead/corrupt worker is detected (EOF, bad frame, protocol mismatch)
//!   and restarted lazily on the next request; the model reloads there.
//! * Request ids are monotone per supervisor and never reset on restart, so
//!   no response can be misattributed across worker generations.
//! * One in-flight request at a time (the worker's native runtimes are
//!   serialized anyway); concurrency policy lives in the SemanticProvider
//!   adapter above this type.

use crate::{
    EmbedItem, PROTOCOL_VERSION, ProtocolError, WorkerErrorClass, WorkerRequest, WorkerResponse,
    read_response, write_request,
};
use std::io::{BufReader, BufWriter};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("worker protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    #[error("worker process failed to spawn: {0}")]
    Spawn(String),
    #[error("worker timed out after {0:?} and was killed")]
    WorkerTimeout(Duration),
    #[error("worker stalled (no progress for {0:?}) and was killed")]
    WorkerStalled(Duration),
    #[error("worker died mid-request (EOF)")]
    WorkerDied,
    #[error("worker reported {class:?}: {message}")]
    Engine {
        class: WorkerErrorClass,
        message: String,
    },
    #[error("worker protocol version mismatch")]
    VersionMismatch,
}

/// How to launch a worker process.
#[derive(Debug, Clone)]
pub struct WorkerLaunch {
    pub program: std::path::PathBuf,
    pub args: Vec<String>,
    /// Extra environment (e.g. ATTIC_MOCK_WORKER for tests).
    pub env: Vec<(String, String)>,
}

struct LiveChild {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    /// Reader thread forwards parsed responses here; supervisor never blocks
    /// on a raw read longer than the request deadline.
    responses: mpsc::Receiver<Result<Option<WorkerResponse>, ProtocolError>>,
}

struct State {
    live: Option<LiveChild>,
    model_loaded: bool,
    next_id: u64,
}

/// Load parameters remembered for lazy restarts.
#[derive(Debug, Clone, Default)]
pub struct LoadParams {
    pub cache_dir: String,
    pub batch_size: usize,
    pub dimension: Option<usize>,
    pub backend: String,
    pub onnx_model_dir: Option<String>,
    pub seq_len: Option<usize>,
}

/// Verifies the actual loaded worker identity (carried in the LoadModel
/// response's capabilities) against whatever identity was assumed before
/// the worker existed. Returns `Err(reason)` on any mismatch — every
/// mismatch is rejected explicitly, on first load AND every lazy restart,
/// so a differently-identified worker (wrong provider, backend, dimension,
/// quantization, ...) can never be silently accepted.
pub type IdentityVerifier = dyn Fn(&[String]) -> Result<(), String> + Send + Sync;

pub struct WorkerSupervisor {
    launch: WorkerLaunch,
    load: Mutex<Option<LoadParams>>,
    state: Mutex<State>,
    identity_verifier: Mutex<Option<Arc<IdentityVerifier>>>,
    /// Workers killed by the stall watchdog, and when the last one was.
    stall_kills: AtomicU64,
    last_stall: Mutex<Option<SystemTime>>,
}

impl WorkerSupervisor {
    pub fn new(launch: WorkerLaunch) -> Self {
        Self {
            launch,
            load: Mutex::new(None),
            state: Mutex::new(State {
                live: None,
                model_loaded: false,
                next_id: 1,
            }),
            identity_verifier: Mutex::new(None),
            stall_kills: AtomicU64::new(0),
            last_stall: Mutex::new(None),
        }
    }

    /// `(workers killed by the stall watchdog, time of the last kill)`.
    pub fn stall_stats(&self) -> (u64, Option<SystemTime>) {
        (
            self.stall_kills.load(Ordering::Relaxed),
            *self.last_stall.lock().unwrap_or_else(|e| e.into_inner()),
        )
    }

    /// Install the identity check run against every LoadModel response
    /// (initial load and lazy restart alike). Caller-supplied so this
    /// protocol-layer crate stays free of any provider-identity type.
    pub fn set_identity_verifier(
        &self,
        f: impl Fn(&[String]) -> Result<(), String> + Send + Sync + 'static,
    ) {
        if let Ok(mut v) = self.identity_verifier.lock() {
            *v = Some(Arc::new(f));
        }
    }

    /// Run the installed identity verifier (if any) against a LoadModel
    /// response's capabilities. On mismatch the worker is killed — an
    /// unverified worker must never be reused for embedding.
    fn verify_capabilities(
        &self,
        state: &mut State,
        capabilities: &[String],
    ) -> Result<(), SupervisorError> {
        let verifier = self.identity_verifier.lock().ok().and_then(|v| v.clone());
        if let Some(verify) = verifier
            && let Err(reason) = verify(capabilities)
        {
            Self::kill_child(state);
            return Err(SupervisorError::Engine {
                class: WorkerErrorClass::Artifact,
                message: format!("worker identity mismatch, worker killed: {reason}"),
            });
        }
        Ok(())
    }

    /// Process id of the current worker (for tests/diagnostics).
    pub fn worker_pid(&self) -> Option<u32> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.live.as_ref().map(|l| l.child.id()))
    }

    /// Start (or restart) the worker process.
    fn spawn_child(&self) -> Result<LiveChild, SupervisorError> {
        let mut cmd = Command::new(&self.launch.program);
        cmd.args(&self.launch.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &self.launch.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| SupervisorError::Spawn(format!("{e}")))?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err(SupervisorError::Spawn(
                "worker stdin/stdout were not captured".into(),
            ));
        };
        let stdin = BufWriter::new(stdin);
        let mut stdout = BufReader::new(stdout);

        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("attic-worker-reader".into())
            .spawn(move || {
                loop {
                    let r = read_response(&mut stdout);
                    let stop = matches!(r, Ok(None));
                    if tx.send(r).is_err() || stop {
                        return;
                    }
                }
            })
            .map_err(|e| SupervisorError::Spawn(format!("reader thread: {e}")))?;

        Ok(LiveChild {
            child,
            stdin,
            responses: rx,
        })
    }

    /// Kill the current worker if any. The OS reaps the pipes; the reader
    /// thread exits on EOF.
    fn kill_child(state: &mut State) {
        if let Some(mut live) = state.live.take() {
            let _ = live.child.kill();
            let _ = live.child.wait();
        }
        state.model_loaded = false;
    }

    fn next_id(state: &mut State) -> u64 {
        let id = state.next_id;
        state.next_id += 1;
        id
    }

    /// Send a request and wait for its response with a hard deadline.
    /// On timeout the child is killed — the native stack cannot be trusted
    /// to honor anything once it hangs.
    ///
    /// `deadline` bounds WORK time: intervals the worker reports as paused
    /// (thermal, VRAM headroom) are not charged. `stall`, when set, kills a
    /// worker that sends no heartbeat for that long — a hang is caught in
    /// seconds instead of at the end of the whole batch budget.
    fn roundtrip(
        &self,
        state: &mut State,
        req: WorkerRequest,
        deadline: Duration,
        stall: Option<Duration>,
    ) -> Result<WorkerResponse, SupervisorError> {
        let id = match &req {
            WorkerRequest::Hello { id }
            | WorkerRequest::LoadModel { id, .. }
            | WorkerRequest::EmbedBatch { id, .. }
            | WorkerRequest::Shutdown { id } => *id,
        };

        // Ensure a live child exists.
        if state.live.is_none() {
            state.live = Some(self.spawn_child()?);
            state.model_loaded = false;
        }
        let Some(live) = state.live.as_mut() else {
            return Err(SupervisorError::Spawn("worker is not running".into()));
        };

        write_request(&mut live.stdin, &req)?;

        let started = Instant::now();
        let mut last_beat = started;
        let mut paused_total = Duration::ZERO;
        let mut paused_since: Option<Instant> = None;
        loop {
            let now = Instant::now();
            let paused_now = paused_since.map_or(Duration::ZERO, |p| now - p);
            let worked = (now - started).saturating_sub(paused_total + paused_now);
            let remaining = deadline.saturating_sub(worked);
            if remaining.is_zero() {
                Self::kill_child(state);
                return Err(SupervisorError::WorkerTimeout(deadline));
            }
            let mut wait = remaining.min(Duration::from_millis(100));
            if let Some(limit) = stall {
                let silent = now - last_beat;
                if silent >= limit {
                    Self::kill_child(state);
                    self.stall_kills.fetch_add(1, Ordering::Relaxed);
                    *self.last_stall.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(SystemTime::now());
                    return Err(SupervisorError::WorkerStalled(limit));
                }
                wait = wait.min(limit - silent);
            }
            let Some(live) = state.live.as_mut() else {
                return Err(SupervisorError::WorkerDied);
            };
            match live.responses.recv_timeout(wait) {
                Ok(Ok(Some(WorkerResponse::Progress { id: pid, paused, .. }))) => {
                    if pid == id {
                        let now = Instant::now();
                        last_beat = now;
                        match (paused, paused_since) {
                            (true, None) => paused_since = Some(now),
                            (false, Some(p)) => {
                                paused_total += now - p;
                                paused_since = None;
                            }
                            _ => {}
                        }
                    }
                    continue;
                }
                Ok(Ok(Some(resp))) => {
                    let resp_id = match &resp {
                        WorkerResponse::HelloOk { id, .. }
                        | WorkerResponse::ModelReady { id, .. }
                        | WorkerResponse::Embeddings { id, .. }
                        | WorkerResponse::Ok { id }
                        | WorkerResponse::Progress { id, .. }
                        | WorkerResponse::Error { id, .. } => *id,
                    };
                    if resp_id != id {
                        // Misattributed response — worker state is untrustworthy.
                        Self::kill_child(state);
                        return Err(ProtocolError::Malformed(format!(
                            "response id {resp_id} != request id {id}"
                        ))
                        .into());
                    }
                    return Ok(resp);
                }
                Ok(Ok(None)) => {
                    Self::kill_child(state);
                    return Err(SupervisorError::WorkerDied);
                }
                Ok(Err(e)) => {
                    Self::kill_child(state);
                    return Err(e.into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    Self::kill_child(state);
                    return Err(SupervisorError::WorkerDied);
                }
            }
        }
    }

    /// Hello handshake; validates the protocol version.
    pub fn handshake(&self) -> Result<Vec<String>, SupervisorError> {
        let mut state = self.state.lock().map_err(|_| {
            SupervisorError::Protocol(ProtocolError::Malformed("state lock poisoned".into()))
        })?;
        let id = Self::next_id(&mut state);
        match self.roundtrip(
            &mut state,
            WorkerRequest::Hello { id },
            Duration::from_secs(10),
            None,
        )? {
            WorkerResponse::HelloOk {
                protocol,
                capabilities,
                ..
            } => {
                if protocol != PROTOCOL_VERSION {
                    Self::kill_child(&mut state);
                    return Err(SupervisorError::VersionMismatch);
                }
                Ok(capabilities)
            }
            other => {
                Err(ProtocolError::Malformed(format!("expected HelloOk, got {other:?}")).into())
            }
        }
    }

    /// Load the model in the worker; remembered for lazy restarts.
    pub fn load_model(&self, params: LoadParams) -> Result<(), SupervisorError> {
        let mut state = self.state.lock().map_err(|_| {
            SupervisorError::Protocol(ProtocolError::Malformed("state lock poisoned".into()))
        })?;
        let id = Self::next_id(&mut state);
        let req = WorkerRequest::LoadModel {
            id,
            cache_dir: params.cache_dir.clone(),
            batch_size: params.batch_size,
            dimension: params.dimension,
            backend: params.backend.clone(),
            onnx_model_dir: params.onnx_model_dir.clone(),
            seq_len: params.seq_len,
        };
        match self.roundtrip(&mut state, req, Duration::from_secs(600), None)? {
            WorkerResponse::HelloOk { capabilities, .. } => {
                self.verify_capabilities(&mut state, &capabilities)?;
                state.model_loaded = true;
                if let Ok(mut l) = self.load.lock() {
                    *l = Some(params);
                }
                Ok(())
            }
            WorkerResponse::Error { class, message, .. } => {
                Err(SupervisorError::Engine { class, message })
            }
            other => {
                Err(ProtocolError::Malformed(format!("expected load result, got {other:?}")).into())
            }
        }
    }

    /// Remember load params without loading (lazy-start providers call this
    /// at construction so a later restart can always reload).
    pub fn load_model_params_only(&self, params: LoadParams) {
        if let Ok(mut l) = self.load.lock() {
            *l = Some(params);
        }
    }

    /// Load using the remembered params (first use or after a restart).
    pub fn load_model_remembered(&self) -> Result<(), SupervisorError> {
        let params =
            self.load
                .lock()
                .ok()
                .and_then(|l| l.clone())
                .ok_or(SupervisorError::Engine {
                    class: WorkerErrorClass::Artifact,
                    message: "no remembered load params".into(),
                })?;
        self.load_model(params)
    }

    /// Embed a batch with a hard deadline. If the worker had died/restarted
    /// since load, the model reloads first (lazy recovery).
    pub fn embed_batch(
        &self,
        items: Vec<EmbedItem>,
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, SupervisorError> {
        self.embed_batch_watched(items, deadline, None)
    }

    /// [`Self::embed_batch`] plus a stall watchdog: the worker is killed if
    /// it sends no progress heartbeat for `stall`. Use only with engines
    /// that report progress (see [`crate::progress`]).
    pub fn embed_batch_watched(
        &self,
        items: Vec<EmbedItem>,
        deadline: Duration,
        stall: Option<Duration>,
    ) -> Result<Vec<Vec<f32>>, SupervisorError> {
        let mut state = self.state.lock().map_err(|_| {
            SupervisorError::Protocol(ProtocolError::Malformed("state lock poisoned".into()))
        })?;

        // Lazy recovery: no live child, but we had a model before → reload.
        if !state.model_loaded || state.live.is_none() {
            let params =
                self.load
                    .lock()
                    .ok()
                    .and_then(|l| l.clone())
                    .ok_or(SupervisorError::Engine {
                        class: WorkerErrorClass::Artifact,
                        message: "no model loaded and no remembered load params".into(),
                    })?;
            let id = Self::next_id(&mut state);
            let req = WorkerRequest::LoadModel {
                id,
                cache_dir: params.cache_dir,
                batch_size: params.batch_size,
                dimension: params.dimension,
                backend: params.backend,
                onnx_model_dir: params.onnx_model_dir,
                seq_len: params.seq_len,
            };
            match self.roundtrip(&mut state, req, Duration::from_secs(600), None)? {
                WorkerResponse::HelloOk { capabilities, .. } => {
                    self.verify_capabilities(&mut state, &capabilities)?;
                    state.model_loaded = true;
                }
                WorkerResponse::Error { class, message, .. } => {
                    return Err(SupervisorError::Engine { class, message });
                }
                other => {
                    return Err(ProtocolError::Malformed(format!(
                        "expected load result, got {other:?}"
                    ))
                    .into());
                }
            }
        }

        let id = Self::next_id(&mut state);
        match self.roundtrip(
            &mut state,
            WorkerRequest::EmbedBatch {
                id,
                items,
                deadline_ms: deadline.as_millis() as u64,
            },
            deadline,
            stall,
        )? {
            WorkerResponse::Embeddings { vectors, .. } => Ok(vectors),
            WorkerResponse::Error { class, message, .. } => {
                Err(SupervisorError::Engine { class, message })
            }
            other => {
                Err(ProtocolError::Malformed(format!("expected Embeddings, got {other:?}")).into())
            }
        }
    }

    /// Graceful shutdown; tolerates an already-dead worker.
    pub fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            if state.live.is_some() {
                let id = Self::next_id(&mut state);
                let _ = self.roundtrip(
                    &mut state,
                    WorkerRequest::Shutdown { id },
                    Duration::from_secs(5),
                    None,
                );
            }
            Self::kill_child(&mut state);
        }
    }
}

impl Drop for WorkerSupervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}
