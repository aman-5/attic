//! Shared daemon per database, thin relay clients (Fix 1 in
//! `docs/Plan-1.md`).
//!
//! Only the first `attic-server` launch for a given database wins
//! `attic.lock` and becomes the **daemon**: it alone owns the SQLite writer,
//! the filesystem watcher, and startup recovery (the same single-owner
//! invariants documented in `docs/ARCHITECTURE.md`, just relocated to "per
//! daemon" instead of "per launch"). It binds a local socket
//! (`interprocess::local_socket`, Unix domain socket on Linux/macOS, named
//! pipe on Windows) and, only after that bind succeeds, publishes the
//! socket's name to a sibling `attic.ipc` file next to `attic.lock` — this
//! ordering closes the race where a client reads an address nobody is
//! listening on yet.
//!
//! Every later launch for the same database fails `try_lock()`, discovers
//! the daemon's address via `attic.ipc`, connects, and becomes a thin
//! **relay**: it splices its own stdin/stdout to the socket byte-for-byte.
//! No per-tool forwarding logic is needed — `AtticServer` is already
//! `Clone`+`Arc`-backed, so the daemon just runs `server.clone().serve(..)`
//! per accepted connection, identical to today's stdio call.
//!
//! `ATTIC_NO_DAEMON=1` (or `true`) skips all of this and reproduces the
//! exact legacy single-process behavior (see `daemon::no_daemon_mode`),
//! preserving existing test behavior and serving as an upgrade-compat
//! fallback if an old pre-daemon binary is holding the lock.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

// ── Phase 6/7 recovery constants ────────────────────────────────────────────

/// Backoff steps (ms) used between successive re-election attempts when the
/// relay loses its daemon connection. Capped at the last value once all steps
/// are exhausted.
const RELAY_RECOVERY_BACKOFFS_MS: &[u64] = &[100, 250, 500, 1_000];

/// Total wall-clock budget a relay is willing to spend trying to recover from
/// a daemon-side disconnect before giving up and exiting. Kept well below
/// `CLIENT_TOTAL_RETRY_BUDGET` (40s) so the relay itself times out first
/// rather than letting the outer election loop spin endlessly.
const RELAY_RECOVERY_BUDGET: Duration = Duration::from_secs(30);

use interprocess::local_socket::{
    GenericNamespaced, ListenerOptions,
    tokio::{Listener as IpcListener, Stream as IpcStream, prelude::*},
};
use rmcp::ServiceExt;
use tracing::{debug, info, warn};

use crate::{AtticServer, ShutdownHandles, run_shutdown_sequence};

/// How long the daemon waits at zero active connections before shutting
/// down. Single constant, easy to find/change. Overridable via
/// `ATTIC_DAEMON_IDLE_TIMEOUT_MS` (milliseconds) so tests don't have to wait
/// out a real 90-second timeout.
pub(crate) const DAEMON_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Bounded time a relay spends discovering `attic.ipc` in one phase before
/// re-checking the overall retry budget below.
const IPC_DISCOVERY_PHASE: Duration = Duration::from_secs(2);
/// Poll interval while waiting for `attic.ipc` to appear/update.
const IPC_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(75);
/// Total time a relay spends trying to discover a usable daemon before
/// giving up with a clear, distinct error (see `elect`). Deliberately kept
/// comfortably above `GRACEFUL_SHUTDOWN_TIMEOUT_MS` (30s): a healthy
/// daemon's own shutdown sequence (bounded task-join up to that timeout,
/// plus an unbounded WAL checkpoint/backup) can legitimately take close to
/// that long, and a client racing a good-faith shutdown must keep retrying
/// through it rather than timing out with a misleading "old pre-daemon
/// build" error.
const CLIENT_TOTAL_RETRY_BUDGET: Duration = Duration::from_secs(40);

/// Returns `true` if `ATTIC_NO_DAEMON` is set to `1` or `true`
/// (case-insensitive) — the escape hatch that skips the whole election flow
/// and reproduces the exact legacy single-process stdio behavior.
pub(crate) fn no_daemon_mode() -> bool {
    match std::env::var("ATTIC_NO_DAEMON") {
        Ok(v) => {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true")
        }
        Err(_) => false,
    }
}

fn idle_timeout() -> Duration {
    std::env::var("ATTIC_DAEMON_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(DAEMON_IDLE_TIMEOUT)
}

/// Everything the winning process needs to run as the daemon: the still-held
/// `attic.lock` guard (kept alive for the daemon's entire lifetime, same
/// contract as the legacy single-instance lock), the bound IPC listener, and
/// the path of the address-discovery file it published (removed best-effort
/// on shutdown).
pub(crate) struct DaemonHandle {
    _lock_guard: std::fs::File,
    listener: IpcListener,
    ipc_path: PathBuf,
}

impl DaemonHandle {
    /// Returns the IPC discovery file path (e.g. `attic.ipc`).
    #[allow(dead_code)]
    pub(crate) fn ipc_path(&self) -> &Path {
        &self.ipc_path
    }

    /// Derive the database path from the IPC path (sibling file convention:
    /// `attic.ipc` lives next to `attic.db`).
    #[allow(dead_code)]
    pub(crate) fn db_path(&self) -> PathBuf {
        self.ipc_path.with_file_name("attic.db")
    }
}

/// A connected IPC stream, ready to be spliced to this process's own
/// stdin/stdout.
pub(crate) struct RelayHandle {
    stream: IpcStream,
}

/// Outcome of [`elect`]: this process either won the race and must now run
/// as the daemon, lost it and should relay to whoever won, or won the lock
/// but couldn't stand up the socket/IPC side of the daemon role and should
/// fall back to legacy single-process serving while still holding the lock
/// it already won (see `ElectAttempt::BindOrIpcFailed`).
pub(crate) enum ElectionResult {
    Daemon(DaemonHandle),
    Relay(RelayHandle),
    Fallback(std::fs::File),
}

/// Outcome of [`run_relay_supervised`]: explicitly describes why relay
/// supervision ended so the caller can take the correct action without
/// guessing from an `Option`.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RelaySupervisionOutcome {
    /// The MCP client (AI agent) closed its stdin. The relay is no longer
    /// needed — normal clean exit.
    ClientClosed,

    /// The daemon died and this relay won the replacement-daemon election.
    /// The caller must:
    ///   1. Construct an `AtticServer` through the normal production path.
    ///   2. Spawn the daemon accept loop concurrently (via [`spawn_daemon`]).
    ///   3. Call [`resume_relay_after_promotion`] with the recovery state
    ///      so that the existing stdin/stdout MCP session continues through
    ///      a new local IPC connection to the replacement daemon.
    PromoteToDaemon {
        daemon_handle: DaemonHandle,
        recovery_state: RelayRecoveryState,
    },

    /// An unrecoverable error occurred (recovery budget exhausted, IPC
    /// setup failed during fallback, etc.).
    Fatal { error: anyhow::Error },
}

/// State preserved from a relay session that is being promoted to a
/// daemon+relay dual role. Carries everything needed to restore the
/// existing MCP client's session on a new daemon connection without
/// losing the stdin/stdout pipe.
pub(crate) struct RelayRecoveryState {
    /// Cached MCP initialization frames for replay.
    session_cache: RelaySessionCache,
    /// The request that was in-flight (sent to the old daemon but whose
    /// response was never received) at the moment of daemon disconnect.
    interrupted_request: Option<InFlightRequest>,
    /// Persistent stdin handle (retained across recovery so Windows pipe state is preserved).
    stdin: tokio::io::Stdin,
    /// Persistent stdout handle (retained across recovery).
    stdout: tokio::io::Stdout,
    /// Unparsed or pending stdin bytes from before promotion.
    pending_stdin: Vec<u8>,
}

/// Derive a deterministic local-socket name from the resolved database path,
/// so repeated launches against the same database always compute the same
/// name. Prefers a canonicalized parent directory + file name (stable across
/// non-canonical path spellings of the same file); falls back to the raw
/// path if canonicalization fails (should not happen in practice since
/// `AtticPaths::resolve()` already creates the home directory).
fn derive_socket_name(db_path: &Path) -> String {
    let key = db_path
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .map(|canon_parent| canon_parent.join(db_path.file_name().unwrap_or_default()))
        .unwrap_or_else(|| db_path.to_path_buf());

    let hash = fnv1a_hash(key.as_os_str().as_encoded_bytes());
    format!("attic-{hash:016x}.sock")
}

/// FNV-1a (64-bit): a small, manually-implemented, deterministic hash with a
/// documented-stable algorithm, unlike `std::collections::hash_map::DefaultHasher`
/// — whose own docs state its algorithm is an unspecified implementation
/// detail NOT guaranteed stable across compiler/std releases. A daemon and
/// relay built with different Rust toolchain versions must compute the
/// exact same socket name for the same database path, or they can never
/// find each other.
fn fnv1a_hash(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn bind_listener(socket_name: &str) -> io::Result<IpcListener> {
    let name = socket_name.to_ns_name::<GenericNamespaced>()?;
    ListenerOptions::new().name(name).create_tokio()
}

async fn connect_stream(socket_name: &str) -> io::Result<IpcStream> {
    let name = socket_name.to_ns_name::<GenericNamespaced>()?;
    IpcStream::connect(name).await
}

/// Outcome of one [`try_become_daemon`] attempt.
enum ElectAttempt {
    /// Another process already holds `attic.lock`.
    NotElected,
    /// Won `attic.lock`, bound the listener, and published `attic.ipc`.
    Daemon(DaemonHandle),
    /// Won `attic.lock`, but binding the local socket/named-pipe listener or
    /// writing `attic.ipc` failed. This process already safely holds
    /// `attic.lock` — it can fall back to legacy single-process serving
    /// with this same guard rather than hard-failing, since nothing else
    /// can be holding the lock at the same time.
    BindOrIpcFailed(std::fs::File, anyhow::Error),
}

/// Attempt to become the daemon: `try_lock()` the (already-open-or-opened)
/// `attic.lock`, and on success, bind the IPC listener and publish
/// `attic.ipc` — in that order, which is what closes the "client reads an
/// address nobody is listening on yet" race. Returns
/// `Ok(ElectAttempt::NotElected)` (not an error) when another process
/// already holds the lock, and `Ok(ElectAttempt::BindOrIpcFailed(..))` (also
/// not an error — see that variant's doc) when the lock was won but the
/// socket/IPC setup failed. Only a failure to even open/lock the file
/// itself propagates as `Err`.
fn try_become_daemon(
    db_path: &Path,
    lock_path: &Path,
    ipc_path: &Path,
) -> anyhow::Result<ElectAttempt> {
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .map_err(|e| anyhow::anyhow!("failed to open lock file '{}': {e}", lock_path.display()))?;

    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(ElectAttempt::NotElected),
        Err(std::fs::TryLockError::Error(e)) => {
            // A genuine I/O failure (locking unsupported on this filesystem,
            // permissions) is not the same as ordinary contention and must
            // not be silently treated as "someone else is the daemon" — see
            // this function's doc comment and the same distinction made at
            // the legacy-mode lock site in `main.rs`.
            return Err(anyhow::anyhow!(
                "failed to lock '{}': {e}",
                lock_path.display()
            ));
        }
    }

    let socket_name = derive_socket_name(db_path);
    let listener = match bind_listener(&socket_name) {
        Ok(listener) => listener,
        Err(e) => {
            return Ok(ElectAttempt::BindOrIpcFailed(
                lock_file,
                anyhow::anyhow!("daemon failed to bind IPC listener '{socket_name}': {e}"),
            ));
        }
    };
    if let Err(e) = std::fs::write(ipc_path, &socket_name) {
        return Ok(ElectAttempt::BindOrIpcFailed(
            lock_file,
            anyhow::anyhow!(
                "daemon failed to write IPC address file '{}': {e}",
                ipc_path.display()
            ),
        ));
    }

    Ok(ElectAttempt::Daemon(DaemonHandle {
        _lock_guard: lock_file,
        listener,
        ipc_path: ipc_path.to_path_buf(),
    }))
}

/// Poll `ipc_path` for a non-empty address, bounded by
/// `min(overall_deadline, now + IPC_DISCOVERY_PHASE)`. Returns `None` if
/// nothing usable appeared within that bound (the caller decides whether to
/// retry or give up based on the overall retry budget).
async fn read_ipc_with_backoff(ipc_path: &Path, overall_deadline: Instant) -> Option<String> {
    let phase_deadline = std::cmp::min(Instant::now() + IPC_DISCOVERY_PHASE, overall_deadline);
    loop {
        if let Ok(contents) = std::fs::read_to_string(ipc_path) {
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if Instant::now() >= phase_deadline {
            return None;
        }
        tokio::time::sleep(IPC_DISCOVERY_POLL_INTERVAL).await;
    }
}

/// Common handling for one `try_become_daemon` outcome, shared between the
/// initial election attempt and the re-election retry after a stale-socket
/// reconnect failure in `elect()` below — both used to repeat this three-arm
/// match verbatim. `Some(result)` means the caller should immediately return
/// that from `elect()`; `None` means `NotElected`, i.e. fall through to the
/// caller's own retry/re-election logic.
fn handle_elect_attempt(attempt: ElectAttempt) -> Option<ElectionResult> {
    match attempt {
        ElectAttempt::Daemon(handle) => Some(ElectionResult::Daemon(handle)),
        ElectAttempt::BindOrIpcFailed(lock_file, e) => {
            warn!(
                "attic: daemon socket/IPC setup failed ({e}); falling back to legacy \
                 single-process mode for this launch (attic.lock is still held)"
            );
            Some(ElectionResult::Fallback(lock_file))
        }
        ElectAttempt::NotElected => None,
    }
}

/// Run the full election protocol for `db_path`: `try_lock()` first; on
/// success this process is the daemon. On failure, discover and connect to
/// the incumbent daemon's IPC address, retrying election (in case the
/// incumbent crashed and left a stale `attic.ipc`) within a bounded total
/// budget before giving up with a clear, distinct error.
pub(crate) async fn elect(db_path: &Path) -> anyhow::Result<ElectionResult> {
    let lock_path = db_path.with_file_name("attic.lock");
    let ipc_path = db_path.with_file_name("attic.ipc");

    if let Some(result) = handle_elect_attempt(try_become_daemon(db_path, &lock_path, &ipc_path)?) {
        return Ok(result);
    }

    let overall_deadline = Instant::now() + CLIENT_TOTAL_RETRY_BUDGET;
    loop {
        if let Some(socket_name) = read_ipc_with_backoff(&ipc_path, overall_deadline).await {
            match connect_stream(&socket_name).await {
                Ok(stream) => return Ok(ElectionResult::Relay(RelayHandle { stream })),
                Err(e) => {
                    // `try_lock()` is inherently crash-safe: the OS releases
                    // the advisory lock automatically when the holding
                    // process exits or is killed. A stale `attic.ipc` that
                    // names a socket nobody is listening on anymore is
                    // therefore recoverable — try to become the new daemon
                    // ourselves.
                    debug!(
                        "attic.ipc at '{}' named a socket that refused connection ({e}); \
                         retrying election in case the previous daemon crashed",
                        ipc_path.display()
                    );
                    if let Some(result) =
                        handle_elect_attempt(try_become_daemon(db_path, &lock_path, &ipc_path)?)
                    {
                        return Ok(result);
                    }
                    // Someone else won the re-election race; fall through
                    // and read `attic.ipc` again, bounded by the deadline
                    // check below.
                }
            }
        }

        if Instant::now() >= overall_deadline {
            anyhow::bail!(
                "another attic-server instance holds the lock for database '{}' (lock '{}') \
                 but never published a usable IPC address at '{}' within {CLIENT_TOTAL_RETRY_BUDGET:?} \
                 — this usually means an old pre-daemon attic-server build is running against \
                 this database; upgrade or stop it, or set ATTIC_NO_DAEMON=1 to use the legacy \
                 single-process mode",
                db_path.display(),
                lock_path.display(),
                ipc_path.display(),
            );
        }

        tokio::time::sleep(IPC_DISCOVERY_POLL_INTERVAL).await;
    }
}

/// Why [`run_relay`] stopped splicing — tells the caller whether to exit the
/// whole process or retry election against a (possibly new) daemon.
pub(crate) enum RelayExit {
    /// This process's own caller (the MCP client on the other end of our
    /// stdin/stdout) is done. There is nothing left to relay for — exit.
    StdinClosed,
    /// The daemon side of the connection ended, cleanly or with an error, or
    /// writing to it failed. The client we're relaying for is still there
    /// (we haven't seen stdin EOF); the caller should retry
    /// `elect()` — reconnecting to a new daemon, or becoming the new daemon
    /// itself if none has taken over yet — rather than exiting.
    DaemonClosed,
}

// ── Phase 7: MCP session cache + in-flight request tracking ─────────────────

/// Delivery state of one MCP request forwarded to the daemon.
///
/// Currently only `Sent` is used; stored so future phases can distinguish
/// "queued but not yet written" from "bytes flushed to the socket" without a
/// schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryState {
    /// The framed request bytes were successfully written to the daemon socket.
    Sent,
}

/// One MCP request that was forwarded to the daemon but whose response has not
/// yet been observed. Kept until the daemon sends back a matching `id`.
struct InFlightRequest {
    /// Raw JSON-RPC id value (number literal or `"quoted string"`) extracted
    /// from the request JSON. Used to match the daemon response so the entry
    /// can be cleared.
    id_raw: String,
    /// JSON-RPC `method` string — used to classify retry safety after a
    /// daemon disconnect.
    method: String,
    /// Complete Content-Length–framed bytes of the original request, stored
    /// so an eligible read-only request can be retried exactly once against
    /// the replacement daemon.
    framed: Vec<u8>,
    /// Delivery state at the moment the daemon disconnected.
    #[allow(dead_code)]
    delivery: DeliveryState,
}

/// Methods that are **explicitly known to be safe, read-only, and
/// idempotent** — the only ones the relay will automatically retry (at most
/// once) after a daemon disconnect with an ambiguous in-flight request.
///
/// **Design:** this is an explicit opt-in allowlist, NOT a blocklist of known
/// mutations. Unknown or future methods are therefore treated as unsafe by
/// default — they never get auto-retried. Any new tool that performs
/// side-effects is automatically protected without needing to be added here.
/// Any new read-only tool must be explicitly listed here before it becomes
/// eligible for auto-retry.
///
/// This mirrors the MCP work-class philosophy: unknown tools default to
/// `Expensive` (fail-safe). Here, unknown methods default to never-retry
/// (fail-safe). Both are opt-in for the permissive path.
fn is_safe_readonly_method(method: &str) -> bool {
    matches!(
        method,
        // MCP lifecycle — initialize is safe to replay; it is how session
        // state is restored after reconnect and is explicitly handled by cache
        "initialize"
            | "ping"
            // Tool/resource/prompt discovery
            | "tools/list"
            | "resources/list"
            | "resources/read"
            | "prompts/list"
            | "prompts/get"
            // Attic search and retrieval
            | "search"
            | "search/semantic"
            | "search/keyword"
            | "search/hybrid"
            | "context"
            | "symbols"
            | "definition"
            | "hover"
            | "references"
            // Attic status / diagnostics
            | "status"
            | "health"
            | "diagnostics"
            | "index/status"
            | "workspace/list"
            | "workspace/status"
    )
}

/// Extract the raw JSON-RPC `id` value from a request/response body as an
/// owned string. Returns the literal text — a number like `"42"` or a quoted
/// string like `"\"abc\""` — so number vs string ids are distinguished.
fn extract_jsonrpc_id(body: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(body).ok()?;
    let key = "\"id\"";
    let key_pos = s.find(key)?;
    let after_key = s[key_pos + key.len()..].trim_start_matches([' ', '\t', ':']);
    if after_key.is_empty() {
        return None;
    }
    if let Some(inner) = after_key.strip_prefix('"') {
        let end = inner.find('"')?;
        Some(format!("\"{}\"", &inner[..end]))
    } else {
        let end = after_key
            .find(|c: char| c == ',' || c == '}' || c.is_whitespace())
            .unwrap_or(after_key.len());
        let raw = after_key[..end].trim();
        if raw.is_empty() {
            None
        } else {
            Some(raw.to_string())
        }
    }
}

/// Extract the JSON body slice from either a Content-Length framed message
/// (`Content-Length: ...\r\n\r\n{...}`) or a newline-delimited JSON message (`{...}\n`).
fn extract_body_slice(msg: &[u8]) -> &[u8] {
    if let Some(hdr_end) = msg.windows(4).position(|w| w == b"\r\n\r\n") {
        &msg[hdr_end + 4..]
    } else {
        // Trim trailing newline / whitespace
        let mut end = msg.len();
        while end > 0 && (msg[end - 1] == b'\n' || msg[end - 1] == b'\r' || msg[end - 1] == b' ') {
            end -= 1;
        }
        let mut start = 0;
        while start < end && (msg[start] == b' ' || msg[start] == b'\t') {
            start += 1;
        }
        &msg[start..end]
    }
}

/// Build a JSON-RPC error response suitable for writing directly to the MCP
/// client's stdout. Standard MCP stdio transports use newline-delimited JSON.
fn make_jsonrpc_error_response(id_raw: &str, code: i32, message: &str) -> Vec<u8> {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id_raw},\"error\":{{\"code\":{code},\
         \"message\":{message:?}}}}}\n",
    )
    .into_bytes()
}

/// Minimal cache of MCP initialization state captured by intercepting the
/// client-to-daemon byte stream. Used by [`run_relay_supervised`] to replay
/// the initialization handshake against a newly connected daemon after a
/// daemon-side disconnect, enabling transparent MCP session recovery for the
/// common case where the client connection is still alive.
///
/// Only the `initialize` request and the subsequent `notifications/initialized`
/// notification are cached; in-flight non-initialization requests are tracked
/// separately via [`InFlightRequest`].
#[derive(Default)]
struct RelaySessionCache {
    /// Raw bytes of the client's `initialize` request, exactly as received
    /// from stdin. Replayed verbatim to a fresh daemon socket before resuming
    /// the normal byte-level splice. `None` until the first `initialize` message
    /// has been intercepted.
    initialize_request: Option<Vec<u8>>,
    /// Raw bytes of the `notifications/initialized` notification, if observed.
    /// Sent to the new daemon immediately after the `initialize` replay.
    initialized_notification: Option<Vec<u8>>,
}

impl RelaySessionCache {
    /// Attempt to replay cached initialization state onto `stream`. Returns
    /// `true` if replay succeeded (or there was nothing to replay), `false`
    /// if the write failed or the daemon's initialize response could not be read.
    ///
    /// # Critical Safety Rule (Phase 4 / §9.4):
    /// The daemon's `initialize` response MUST be read and consumed from `stream`
    /// right here. It must NEVER be forwarded to the client's stdout because
    /// the client already completed its handshake originally. Delivering a
    /// duplicate `initialize` response to the client breaks the JSON-RPC
    /// request/response correlation.
    async fn replay_to(&self, stream: &mut IpcStream) -> bool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        if let Some(init_bytes) = &self.initialize_request {
            if stream.write_all(init_bytes).await.is_err() || stream.flush().await.is_err() {
                return false;
            }

            // Read the daemon's initialize response from the stream until we
            // have parsed the complete message (either Content-Length or newline-delimited).
            let mut resp_buf = Vec::with_capacity(2048);
            let mut tmp = [0u8; 1024];
            let mut got_response = false;
            while !got_response {
                match stream.read(&mut tmp).await {
                    Ok(0) | Err(_) => return false,
                    Ok(n) => {
                        resp_buf.extend_from_slice(&tmp[..n]);
                        if let Some((framed, _next)) = parse_one_framed_message(&resp_buf, 0) {
                            let body = extract_body_slice(&framed);
                            if let Ok(s) = std::str::from_utf8(body)
                                && s.contains("\"error\"")
                                && !s.contains("\"result\"")
                            {
                                warn!("replay_to: daemon returned error on initialize replay: {s}");
                                return false;
                            }
                            got_response = true;
                        }
                    }
                }
            }
        }

        if let Some(notif_bytes) = &self.initialized_notification
            && (stream.write_all(notif_bytes).await.is_err() || stream.flush().await.is_err())
        {
            return false;
        }
        true
    }
}

/// Parse one message from `buf` starting at `offset`. Supports both
/// standard MCP newline-delimited JSON (`{...}\n`) and LSP-style
/// `Content-Length: <N>\r\n\r\n<JSON>` framing.
///
/// Returns `Some((message_bytes, next_offset))` where `message_bytes` is the
/// *complete* chunk exactly as received, or `None` if `buf` doesn't yet contain
/// a complete message.
fn parse_one_framed_message(buf: &[u8], offset: usize) -> Option<(Vec<u8>, usize)> {
    let data = &buf[offset..];
    if data.is_empty() {
        return None;
    }

    // LSP-style Content-Length header framing:
    if data.starts_with(b"Content-Length:") || data.starts_with(b"content-length:") {
        let header_end = data.windows(4).position(|w| w == b"\r\n\r\n")?;
        let headers = &data[..header_end];
        let cl_prefix = b"Content-Length: ";
        let cl_pos = headers
            .windows(cl_prefix.len())
            .position(|w| w.eq_ignore_ascii_case(cl_prefix))?;
        let after_cl = &headers[cl_pos + cl_prefix.len()..];
        let line_end = after_cl
            .iter()
            .position(|&b| b == b'\r' || b == b'\n')
            .unwrap_or(after_cl.len());
        let content_length: usize = std::str::from_utf8(&after_cl[..line_end])
            .ok()?
            .trim()
            .parse()
            .ok()?;

        let body_start = header_end + 4;
        let body_end = body_start + content_length;
        if data.len() < body_end {
            return None;
        }
        return Some((data[..body_end].to_vec(), offset + body_end));
    }

    // Standard MCP line-delimited JSON (newline terminated):
    let newline_pos = data.iter().position(|&b| b == b'\n')?;
    let end = newline_pos + 1;
    Some((data[..end].to_vec(), offset + end))
}

/// Look for the JSON-RPC `method` field value in a raw UTF-8 body slice.
/// Returns `Some(method)` on a best-effort parse without pulling in a full
/// JSON library at this layer. Only called on the tiny subset of messages
/// needed for cache decisions.
fn extract_jsonrpc_method(body: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(body).ok()?;
    let key = "\"method\"";
    let key_pos = s.find(key)?;
    let after_key = s[key_pos + key.len()..].trim_start_matches([' ', '\t', ':']);
    if !after_key.starts_with('"') {
        return None;
    }
    let inner = &after_key[1..];
    let end = inner.find('"')?;
    Some(&inner[..end])
}

/// Look for a `"params":{"name": "..."}` tool name in a raw UTF-8 body slice
/// — the shape of every real MCP `tools/call` request. Same best-effort,
/// no-JSON-library parsing style as [`extract_jsonrpc_method`].
fn extract_tools_call_name(body: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(body).ok()?;
    let params_pos = s.find("\"params\"")?;
    let after_params = &s[params_pos..];
    let name_key = "\"name\"";
    let key_pos = after_params.find(name_key)?;
    let after_key = after_params[key_pos + name_key.len()..].trim_start_matches([' ', '\t', ':']);
    if !after_key.starts_with('"') {
        return None;
    }
    let inner = &after_key[1..];
    let end = inner.find('"')?;
    Some(&inner[..end])
}

/// Resolve the *effective* method used for retry-safety classification
/// ([`is_safe_readonly_method`]): every real MCP tool invocation is sent as
/// the JSON-RPC method `"tools/call"` with the actual tool name inside
/// `params.name` — not as a bare top-level method matching the tool's name.
/// `is_safe_readonly_method`'s allowlist is expressed in terms of tool names
/// (`"status"`, `"search"`, ...), so `tools/call` requests must be reclassified
/// by their inner tool name or the allowlist can never match real traffic.
/// Direct JSON-RPC methods (`"initialize"`, `"ping"`, `"tools/list"`, ...)
/// pass through unchanged. Falls back to `"tools/call"` itself (never on the
/// allowlist, i.e. fail-safe/never-retried) if the tool name can't be parsed.
fn effective_method_for_classification<'a>(method: &'a str, body: &'a [u8]) -> &'a str {
    if method == "tools/call" {
        extract_tools_call_name(body).unwrap_or(method)
    } else {
        method
    }
}

/// Bidirectional splice of `stdin → daemon` with session-cache interception
/// and in-flight request tracking (Phase 7), plus `daemon → stdout`
/// pass-through. Unlike the plain [`run_relay`], this variant:
///
/// 1. Buffers bytes arriving from stdin and parses Content-Length frames.
/// 2. When it sees an `initialize` or `notifications/initialized` message it
///    stores a copy in `cache` **before** forwarding to the daemon.
/// 3. For every other request that carries an `id` field it records an
///    [`InFlightRequest`] entry in `in_flight`, replacing any prior entry
///    (the MCP stdio model issues requests serially so at most one is
///    in-flight at any instant).
/// 4. When the daemon returns a response whose `id` matches the tracked
///    entry it clears `in_flight`.
/// 5. Forwards all bytes to the daemon and all bytes back to stdout.
///
/// Returns the same [`RelayExit`] semantics as [`run_relay`].
async fn run_relay_with_cache(
    relay: RelayHandle,
    cache: &mut RelaySessionCache,
    in_flight: &mut Option<InFlightRequest>,
    stdin: &mut tokio::io::Stdin,
    stdout: &mut tokio::io::Stdout,
    stdin_buf: &mut Vec<u8>,
) -> anyhow::Result<RelayExit> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut recv_half, mut send_half) = relay.stream.split();

    // If there were any buffered bytes from before (e.g. read before previous daemon died),
    // flush them to the daemon now.
    if !stdin_buf.is_empty() {
        let mut parse_offset = 0usize;
        while let Some((framed, next)) = parse_one_framed_message(stdin_buf, parse_offset) {
            let body = extract_body_slice(&framed);
            let method = extract_jsonrpc_method(body).map(str::to_owned);
            match method.as_deref() {
                Some("initialize") => {
                    if cache.initialize_request.is_none() {
                        cache.initialize_request = Some(framed.clone());
                    }
                }
                Some("notifications/initialized") => {
                    if cache.initialized_notification.is_none() {
                        cache.initialized_notification = Some(framed.clone());
                    }
                }
                Some(m) => {
                    if let Some(id_raw) = extract_jsonrpc_id(body) {
                        *in_flight = Some(InFlightRequest {
                            id_raw,
                            method: effective_method_for_classification(m, body).to_owned(),
                            framed: framed.clone(),
                            delivery: DeliveryState::Sent,
                        });
                    }
                }
                None => {}
            }
            parse_offset = next;
        }

        if send_half.write_all(stdin_buf).await.is_err() || send_half.flush().await.is_err() {
            eprintln!("attic: lost connection to daemon; will reconnect");
            return Ok(RelayExit::DaemonClosed);
        }
        stdin_buf.clear();
    }

    // Two separate read buffers — tokio::select! evaluates both future
    // expressions before polling, which means both `stdin.read(&mut buf)`
    // and `recv_half.read(&mut buf)` would be constructed simultaneously.
    // Rust rejects two simultaneous `&mut` borrows of the same array
    // (E0499), so each arm needs its own buffer.
    let mut stdin_tmp = [0u8; 4096];
    let mut daemon_tmp = [0u8; 4096];

    let exit = loop {
        tokio::select! {
            // stdin → daemon (with cache interception + in-flight tracking)
            n = stdin.read(&mut stdin_tmp) => {
                match n {
                    Err(e) => {
                        warn!("relay: error reading stdin: {e}");
                        break RelayExit::StdinClosed;
                    }
                    Ok(0) => {
                        eprintln!("attic: stdin closed; relay exiting");
                        break RelayExit::StdinClosed;
                    }
                    Ok(n) => {
                        stdin_buf.extend_from_slice(&stdin_tmp[..n]);
                        info!(len = n, raw = %String::from_utf8_lossy(&stdin_tmp[..n]), "relay: stdin read");

                        // Parse and cache any complete frames before forwarding.
                        let mut parse_offset = 0usize;
                        while let Some((framed, next)) =
                            parse_one_framed_message(stdin_buf, parse_offset)
                        {
                            let body = extract_body_slice(&framed);
                            let method = extract_jsonrpc_method(body).map(str::to_owned);
                            match method.as_deref() {
                                Some("initialize") => {
                                    if cache.initialize_request.is_none() {
                                        cache.initialize_request =
                                            Some(framed.clone());
                                    }
                                }
                                Some("notifications/initialized") => {
                                    if cache.initialized_notification.is_none() {
                                        cache.initialized_notification =
                                            Some(framed.clone());
                                    }
                                }
                                Some(m) => {
                                    // Phase 7: track in-flight request.
                                    if let Some(id_raw) = extract_jsonrpc_id(body) {
                                        *in_flight = Some(InFlightRequest {
                                            id_raw,
                                            method: effective_method_for_classification(m, body).to_owned(),
                                            framed: framed.clone(),
                                            delivery: DeliveryState::Sent,
                                        });
                                    }
                                }
                                None => {}
                            }
                            parse_offset = next;
                        }

                        // Forward the full accumulated buffer to the daemon.
                        if send_half.write_all(stdin_buf).await.is_err()
                            || send_half.flush().await.is_err()
                        {
                            eprintln!(
                                "attic: lost connection to daemon; will reconnect"
                            );
                            break RelayExit::DaemonClosed;
                        }
                        stdin_buf.clear();
                    }
                }
            }

            // daemon → stdout (phase 7: clear in-flight on matching response id)
            n = recv_half.read(&mut daemon_tmp) => {
                match n {
                    Err(e) => {
                        warn!("relay: error reading from daemon: {e}");
                        break RelayExit::DaemonClosed;
                    }
                    Ok(0) => {
                        eprintln!("attic: daemon closed connection; will reconnect");
                        break RelayExit::DaemonClosed;
                    }
                    Ok(n) => {
                        // Phase 7: if the daemon response carries an id that
                        // matches our tracked in-flight request, clear it —
                        // the operation completed successfully.
                        if let Some(req) = in_flight.as_ref() {
                            let body = extract_body_slice(&daemon_tmp[..n]);
                            if let Some(resp_id) = extract_jsonrpc_id(body)
                                && resp_id == req.id_raw
                            {
                                *in_flight = None;
                            }
                        }
                        if stdout.write_all(&daemon_tmp[..n]).await.is_err()
                            || stdout.flush().await.is_err()
                        {
                            break RelayExit::StdinClosed;
                        }
                    }
                }
            }
        }
    };

    Ok(exit)
}

/// Supervised relay loop (Phase 6 + 7): run [`run_relay_with_cache`] and, on
/// a [`RelayExit::DaemonClosed`], attempt bounded recovery:
///
/// 1. Re-run [`elect`]: if another process has already become the new daemon
///    we just `Relay`-connect to it.  If no daemon exists and this relay wins
///    the election we return [`RelaySupervisionOutcome::PromoteToDaemon`] so
///    that the higher-level lifecycle layer (in `main.rs`) can start a
///    replacement daemon through the same normal production initialization
///    path while **keeping the existing relay alive**.
/// 2. Replay the cached MCP `initialize` / `notifications/initialized`
///    frames onto the new connection.
/// 3. Phase 7: handle the interrupted in-flight request — retry read-only
///    requests once; synthesize a JSON-RPC error for mutations.
/// 4. Resume normal splicing.
///
/// Returns [`RelaySupervisionOutcome::ClientClosed`] when the MCP client
/// exits normally, [`RelaySupervisionOutcome::PromoteToDaemon`] when this
/// relay won the daemon election and the caller must start a replacement
/// daemon while keeping the relay alive, or
/// [`RelaySupervisionOutcome::Fatal`] on unrecoverable error.
pub(crate) async fn run_relay_supervised(
    relay: RelayHandle,
    db_path: &Path,
) -> RelaySupervisionOutcome {
    use tokio::io::AsyncWriteExt;

    let mut cache = RelaySessionCache::default();
    // Phase 7: one in-flight request slot (MCP stdio is serial).
    let mut in_flight: Option<InFlightRequest> = None;
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_buf: Vec<u8> = Vec::with_capacity(4096);

    // First run — use the relay handle we were given.
    match run_relay_with_cache(
        relay,
        &mut cache,
        &mut in_flight,
        &mut stdin,
        &mut stdout,
        &mut stdin_buf,
    )
    .await
    {
        Ok(RelayExit::StdinClosed) => return RelaySupervisionOutcome::ClientClosed,
        Ok(RelayExit::DaemonClosed) => {} // fall through to recovery loop
        Err(e) => {
            return RelaySupervisionOutcome::Fatal {
                error: e.context("relay initial connection failed"),
            };
        }
    }

    // Recovery loop — bounded by RELAY_RECOVERY_BUDGET.
    let recovery_deadline = Instant::now() + RELAY_RECOVERY_BUDGET;
    let mut attempt = 0usize;

    loop {
        if Instant::now() >= recovery_deadline {
            return RelaySupervisionOutcome::Fatal {
                error: anyhow::anyhow!(
                    "attic: relay could not reconnect to a daemon within \
                     {RELAY_RECOVERY_BUDGET:?}; giving up"
                ),
            };
        }

        // Back-off before retrying election.
        let backoff_ms = RELAY_RECOVERY_BACKOFFS_MS
            .get(attempt)
            .copied()
            .unwrap_or(*RELAY_RECOVERY_BACKOFFS_MS.last().unwrap());
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        attempt += 1;

        info!(
            attempt,
            "relay: daemon disconnected; attempting recovery (budget remaining: {:?})",
            recovery_deadline.saturating_duration_since(Instant::now()),
        );

        match elect(db_path).await {
            Err(e) => {
                return RelaySupervisionOutcome::Fatal {
                    error: e.context("relay recovery: election failed"),
                };
            }

            // ── relay wins election: return promotion outcome with recovery state ──
            Ok(ElectionResult::Daemon(handle)) => {
                info!(
                    "relay: won daemon election; returning PromoteToDaemon \
                     (relay will remain alive for existing MCP client)"
                );
                return RelaySupervisionOutcome::PromoteToDaemon {
                    daemon_handle: handle,
                    recovery_state: RelayRecoveryState {
                        session_cache: cache,
                        interrupted_request: in_flight,
                        stdin,
                        stdout,
                        pending_stdin: stdin_buf,
                    },
                };
            }

            // ── fallback: no IPC, serve inline — relay cannot do this ──
            Ok(ElectionResult::Fallback(_lock)) => {
                return RelaySupervisionOutcome::Fatal {
                    error: anyhow::anyhow!(
                        "attic: relay won the daemon lock but IPC setup failed; \
                         cannot continue in relay mode"
                    ),
                };
            }

            // ── connected to a (new) daemon: replay session + resume ──
            Ok(ElectionResult::Relay(new_relay)) => {
                let mut stream = new_relay.stream;

                // Replay MCP initialization onto the new daemon connection.
                if !cache.replay_to(&mut stream).await {
                    warn!("relay: session replay failed on new daemon connection; will retry");
                    continue;
                }

                // Phase 7: handle the interrupted in-flight request.
                //
                // Safety rule (opt-in allowlist): only methods explicitly
                // listed in `is_safe_readonly_method` may be retried once.
                // Everything else — mutations, unknown/future tools, and
                // anything with unclear idempotency — receives a synthesized
                // JSON-RPC error. Unknown methods default to never-retry,
                // which is the correct safe failure mode.
                if let Some(req) = in_flight.take() {
                    if is_safe_readonly_method(&req.method) {
                        // Explicitly known read-only: retry at most once.
                        info!(
                            method = %req.method,
                            id    = %req.id_raw,
                            "relay: retrying in-flight read-only request \
                             against new daemon (max 1 retry)"
                        );
                        if stream.write_all(&req.framed).await.is_err()
                            || stream.flush().await.is_err()
                        {
                            warn!(
                                "relay: retry write failed; new daemon connection \
                                 already broken"
                            );
                            // in_flight was already taken; the retry simply
                            // won't produce a response. Loop and try again.
                            continue;
                        }
                        // Re-arm the in-flight tracker so the response clears it.
                        in_flight = Some(InFlightRequest {
                            id_raw: req.id_raw,
                            method: req.method,
                            framed: req.framed,
                            delivery: DeliveryState::Sent,
                        });
                    } else {
                        // Mutation, unknown tool, or anything not on the
                        // read-only allowlist: synthesize a JSON-RPC error —
                        // never replay.
                        let err_bytes = make_jsonrpc_error_response(
                            &req.id_raw,
                            -32603,
                            "The daemon disconnected while this operation was in \
                             flight. Its completion state is unknown. The operation \
                             was not automatically retried.",
                        );
                        let _ = stdout.write_all(&err_bytes).await;
                        let _ = stdout.flush().await;
                        warn!(
                            method = %req.method,
                            id    = %req.id_raw,
                            "relay: in-flight request had ambiguous delivery; \
                             synthesized error to client (method not on read-only allowlist)"
                        );
                    }
                }

                // Resume normal splicing against the new daemon connection.
                match run_relay_with_cache(
                    RelayHandle { stream },
                    &mut cache,
                    &mut in_flight,
                    &mut stdin,
                    &mut stdout,
                    &mut stdin_buf,
                )
                .await
                {
                    Ok(RelayExit::StdinClosed) => return RelaySupervisionOutcome::ClientClosed,
                    Ok(RelayExit::DaemonClosed) => {
                        // Another disconnect — keep looping within the budget.
                        warn!("relay: new daemon connection also closed; continuing recovery loop");
                    }
                    Err(e) => {
                        return RelaySupervisionOutcome::Fatal {
                            error: e.context("relay: connection error during recovery"),
                        };
                    }
                }
            }
        }
    }
}

/// Low-level relay that simply splices `stdin ↔ stream` byte-for-byte with
/// no session caching. Used only for the `ATTIC_NO_DAEMON` fast-path and in
/// tests that don't need Phase 6/7 recovery.
#[allow(dead_code)]
pub(crate) async fn run_relay(relay: RelayHandle) -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut recv_half, mut send_half) = relay.stream.split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut stdin_buf = [0u8; 4096];
    let mut daemon_buf = [0u8; 4096];

    loop {
        tokio::select! {
            n = stdin.read(&mut stdin_buf) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if send_half.write_all(&stdin_buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            n = recv_half.read(&mut daemon_buf) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout.write_all(&daemon_buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Handle one accepted IPC connection on the daemon side: run the MCP server
/// over this socket connection (reading from the socket, writing to the
/// socket) using `server.serve()`.
async fn handle_connection(
    stream: IpcStream,
    server: AtticServer,
    active: Arc<AtomicUsize>,
    handles: Arc<ShutdownHandles>,
) {
    let _guard = ConnectionGuard::new(active);
    debug!("daemon: new IPC connection accepted");
    let (read_half, write_half) = stream.split();
    match server.serve((read_half, write_half)).await {
        Ok(running) => {
            let _ = running.waiting().await;
        }
        Err(e) => {
            warn!("daemon: connection ended with error: {e}");
        }
    }
    debug!("daemon: IPC connection closed");
    // Wake up the idle-timeout check in run_daemon_accept_loop.
    drop(handles);
}

/// RAII guard that increments an atomic counter on construction and
/// decrements it on drop.  Used to track active IPC connections so the
/// daemon can detect when it is truly idle.
struct ConnectionGuard(Arc<AtomicUsize>);
impl ConnectionGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Handle one accepted stdio connection on the daemon side.  Used when the
/// daemon is invoked directly (no relay) — i.e. the first launch with
/// `ATTIC_NO_DAEMON=0` that wins election still reads from its own stdin.
#[allow(dead_code)]
pub(crate) async fn handle_stdio_connection(server: AtticServer) -> anyhow::Result<()> {
    let running = server.serve(rmcp::transport::stdio()).await?;
    let _ = running.waiting().await;
    Ok(())
}

/// Spawn the daemon accept loop on a background task so the caller retains
/// control of the current async flow. The returned `JoinHandle` completes
/// when the daemon accept loop exits (idle timeout, shutdown signal, or
/// error).
///
/// The `ready_tx` oneshot is sent `()` as soon as the daemon listener is
/// known to be ready for connections (which is immediately, since
/// `DaemonHandle` already holds a bound listener). This explicit readiness
/// signal avoids arbitrary sleeps.
///
/// # Arguments
/// * `server`  — fully constructed `AtticServer` (normal production path).
/// * `semantic_enricher` — optional background enrichment worker.
/// * `handle`  — the `DaemonHandle` from winning the election.
/// * `ready_tx` — oneshot sender signaling listener readiness.
pub(crate) fn spawn_daemon(
    server: AtticServer,
    semantic_enricher: Option<attic_semantic::BackgroundEnricher>,
    handle: DaemonHandle,
    ready_tx: tokio::sync::oneshot::Sender<()>,
) -> tokio::task::JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        // The listener in `handle` is already bound — it was created during
        // election (try_become_daemon). Signal readiness immediately.
        let _ = ready_tx.send(());
        info!("daemon: spawned replacement daemon accept loop");
        run_daemon_accept_loop(server, semantic_enricher, handle, false).await
    })
}

/// Resume the relay after this process was promoted to daemon during
/// recovery. Connects the relay to the replacement daemon via local IPC,
/// replays the MCP session, handles any interrupted request, and then
/// continues forwarding stdin/stdout traffic until the MCP client
/// disconnects.
///
/// This function blocks until the MCP client closes stdin (normal relay
/// exit). The caller should await both this and the daemon task's
/// `JoinHandle`.
///
/// # Arguments
/// * `db_path` — path to `attic.db` (used to derive the IPC socket name).
/// * `recovery` — session cache + interrupted request from the pre-promotion
///   relay session.
pub(crate) async fn resume_relay_after_promotion(
    db_path: &Path,
    recovery: RelayRecoveryState,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let RelayRecoveryState {
        mut session_cache,
        mut interrupted_request,
        mut stdin,
        mut stdout,
        mut pending_stdin,
    } = recovery;

    // Connect to the replacement daemon using the same IPC mechanism as
    // a normal relay. The daemon was already started and signaled readiness
    // before this function is called.
    let socket_name = derive_socket_name(db_path);
    info!(
        socket = %socket_name,
        "relay promotion: connecting to replacement daemon via local IPC"
    );

    let mut stream = connect_stream(&socket_name).await.map_err(|e| {
        anyhow::anyhow!(
            "relay promotion: failed to connect to replacement daemon \
             at socket '{socket_name}': {e}"
        )
    })?;

    info!("relay promotion: connected to replacement daemon");

    // Replay MCP initialization onto the new daemon connection.
    if !session_cache.replay_to(&mut stream).await {
        anyhow::bail!(
            "relay promotion: failed to replay MCP initialization \
             to replacement daemon"
        );
    }
    info!("relay promotion: MCP initialization replayed successfully");

    // Phase 7: handle the interrupted in-flight request.
    if let Some(req) = interrupted_request.take() {
        if is_safe_readonly_method(&req.method) {
            info!(
                method = %req.method,
                id    = %req.id_raw,
                "relay promotion: retrying safe read-only in-flight request"
            );
            if stream.write_all(&req.framed).await.is_err() || stream.flush().await.is_err() {
                warn!(
                    "relay promotion: retry write failed; \
                     replacement daemon connection broken"
                );
                anyhow::bail!(
                    "relay promotion: replacement daemon connection \
                     broke during in-flight retry"
                );
            }
            // Re-arm for completeness (the response will flow through
            // run_relay_supervised's own in-flight tracker below).
            #[allow(unused_assignments)]
            {
                interrupted_request = Some(InFlightRequest {
                    id_raw: req.id_raw,
                    method: req.method,
                    framed: req.framed,
                    delivery: DeliveryState::Sent,
                });
            }
        } else {
            // Mutation or unknown method — synthesize error, never replay.
            let err_bytes = make_jsonrpc_error_response(
                &req.id_raw,
                -32603,
                "The daemon disconnected while this operation was in \
                 flight. Its completion state is unknown. The operation \
                 was not automatically retried.",
            );
            let _ = stdout.write_all(&err_bytes).await;
            let _ = stdout.flush().await;
            warn!(
                method = %req.method,
                id    = %req.id_raw,
                "relay promotion: synthesized error for unsafe in-flight request"
            );
        }
    }

    info!("relay promotion: resuming normal relay forwarding");

    // Splice traffic between the existing stdio client and the replacement daemon.
    let relay = RelayHandle { stream };
    match run_relay_with_cache(
        relay,
        &mut session_cache,
        &mut interrupted_request,
        &mut stdin,
        &mut stdout,
        &mut pending_stdin,
    )
    .await?
    {
        RelayExit::StdinClosed => {
            info!("relay promotion: MCP client closed normally");
            Ok(())
        }
        RelayExit::DaemonClosed => {
            warn!("relay promotion: replacement daemon closed connection");
            Ok(())
        }
    }
}

/// Accept loop run by the daemon process.  Accepts IPC connections from relay
/// processes, services each one with a clone of `server`, then performs the
/// same graceful shutdown sequence as [`crate::serve_until_closed`] once the
/// loop exits (idle-timeout, Ctrl+C, or zero active connections).
///
/// Mirrors [`crate::serve_until_closed`]'s interface so `main` can dispatch
/// to either path with the same three arguments.
pub(crate) async fn run_daemon_accept_loop(
    server: AtticServer,
    semantic_enricher: Option<attic_semantic::BackgroundEnricher>,
    handle: DaemonHandle,
    serve_stdio: bool,
) -> anyhow::Result<()> {
    use std::time::Duration;

    // Phase 3: spawn the periodic RSS sampler before consuming `server`.
    let rss_sampler = server.resource_monitor.as_ref().map(|monitor| {
        let monitor = monitor.clone();
        let cancel = attic_core::CancellationToken::new();
        let cancel_for_task = cancel.clone();
        let h = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if cancel_for_task.is_cancelled() {
                    break;
                }
                monitor.refresh_process_memory();
                let _ = monitor.guidance_pressure();
            }
        });
        (cancel, h)
    });

    // Shutdown handles must be captured before `server` is consumed by
    // any of the spawned `handle_connection` tasks.
    let mut sh = ShutdownHandles::capture(&server);
    sh.rss_sampler = rss_sampler;
    let handles = Arc::new(sh);

    // Internal shutdown-signal channel — used to ask the loop to stop
    // cleanly (e.g. on Ctrl+C).
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Ctrl+C / SIGINT handler — mirrors serve_until_closed.
    let _ctrl_c = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("daemon: ctrl_c/SIGINT received – initiating graceful shutdown");
            let _ = shutdown_tx.send(true);
        }
    });

    info!("daemon: IPC accept loop started");
    let active_connections: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let idle_timeout = idle_timeout();
    let mut idle_since: Option<Instant> = None;

    // When launched directly as the primary daemon (serve_stdio = true),
    // this process's own caller is talking to it over stdin/stdout (e.g. AI
    // client or integration test runner). Serve that stdio connection
    // concurrently with the IPC accept loop, tracking it in active_connections
    // so idle-timeout triggers only after both stdio and all IPC relays close.
    if serve_stdio {
        let stdin_server = server.clone();
        let stdin_active = Arc::clone(&active_connections);
        let stdin_handles = Arc::clone(&handles);
        tokio::spawn(async move {
            let _guard = ConnectionGuard::new(stdin_active);
            debug!("daemon: starting own stdio MCP session");
            match stdin_server.serve(rmcp::transport::stdio()).await {
                Ok(running) => {
                    let _ = running.waiting().await;
                }
                Err(e) => {
                    debug!("daemon: own stdio connection ended: {e}");
                }
            }
            debug!("daemon: own stdio connection closed");
            drop(stdin_handles);
        });
    }

    loop {
        // Idle-timeout check.
        if active_connections.load(Ordering::Relaxed) == 0 {
            match idle_since {
                None => idle_since = Some(Instant::now()),
                Some(t) if t.elapsed() >= idle_timeout => {
                    info!(
                        "daemon: idle for {idle_timeout:?} with no active connections; \
                         shutting down"
                    );
                    break;
                }
                _ => {}
            }
        } else {
            idle_since = None;
        }

        let accept_future = handle.listener.accept();
        let timeout_future = tokio::time::sleep(Duration::from_millis(500));

        tokio::select! {
            result = accept_future => {
                match result {
                    Ok(stream) => {
                        idle_since = None;
                        let server_clone = server.clone();
                        let active_clone = Arc::clone(&active_connections);
                        let handles_clone = Arc::clone(&handles);
                        tokio::spawn(handle_connection(
                            stream,
                            server_clone,
                            active_clone,
                            handles_clone,
                        ));
                    }
                    Err(e) => {
                        warn!("daemon: accept error: {e}");
                    }
                }
            }
            _ = timeout_future => {
                // Just a tick to re-check idle/shutdown.
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("daemon: shutdown signal received; stopping accept loop");
                    break;
                }
            }
        }
    }

    // Remove the IPC address file so stale relays get a clean election.
    let _ = std::fs::remove_file(&handle.ipc_path);
    info!("daemon: accept loop exited");

    // Unwrap the Arc — every spawned connection task holds a clone, but by
    // the time the accept loop exits (idle timeout / shutdown signal) all
    // connections have had their tasks spawned. We cannot wait for them to
    // finish here (tokio::spawn detaches), but the shutdown sequence below
    // drains owned bootstrap jobs and writer state, which is the only safe
    // subset to deterministically drain at this point.
    //
    // `Arc::try_unwrap` may fail if a connection task is still running;
    // fall back to a clone so shutdown still proceeds rather than hanging.
    let sh = Arc::try_unwrap(handles).unwrap_or_else(|arc| ShutdownHandles {
        writer: arc.writer.clone(),
        db_path: arc.db_path.clone(),
        watches: arc.watches.clone(),
        bootstrap_jobs: arc.bootstrap_jobs.clone(),
        scheduler: arc.scheduler.clone(),
        rss_sampler: None, // already cancelled above via the Arc'd copy
    });

    run_shutdown_sequence(sh, semantic_enricher, "daemon accept loop exited").await;
    Ok(())
}

/// Returns a human-readable summary of the relay recovery backoff schedule
/// for use in log messages and diagnostics.
#[allow(dead_code)]
pub(crate) fn relay_recovery_info() -> HashMap<&'static str, String> {
    let mut m = HashMap::new();
    m.insert(
        "recovery_budget_secs",
        RELAY_RECOVERY_BUDGET.as_secs().to_string(),
    );
    m.insert(
        "backoff_steps_ms",
        RELAY_RECOVERY_BACKOFFS_MS
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(","),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_safe_readonly_method_allowlist() {
        // Safe read-only methods must return true
        assert!(is_safe_readonly_method("initialize"));
        assert!(is_safe_readonly_method("ping"));
        assert!(is_safe_readonly_method("tools/list"));
        assert!(is_safe_readonly_method("resources/list"));
        assert!(is_safe_readonly_method("resources/read"));
        assert!(is_safe_readonly_method("prompts/list"));
        assert!(is_safe_readonly_method("prompts/get"));
        assert!(is_safe_readonly_method("search"));
        assert!(is_safe_readonly_method("search/semantic"));
        assert!(is_safe_readonly_method("search/keyword"));
        assert!(is_safe_readonly_method("search/hybrid"));
        assert!(is_safe_readonly_method("context"));
        assert!(is_safe_readonly_method("symbols"));
        assert!(is_safe_readonly_method("definition"));
        assert!(is_safe_readonly_method("hover"));
        assert!(is_safe_readonly_method("references"));
        assert!(is_safe_readonly_method("status"));
        assert!(is_safe_readonly_method("health"));
        assert!(is_safe_readonly_method("diagnostics"));
        assert!(is_safe_readonly_method("index/status"));
        assert!(is_safe_readonly_method("workspace/list"));
        assert!(is_safe_readonly_method("workspace/status"));
    }

    #[test]
    fn test_mutation_and_unknown_methods_not_retried() {
        // Mutations must never be retried
        assert!(!is_safe_readonly_method("workspace"));
        assert!(!is_safe_readonly_method("workspace/add"));
        assert!(!is_safe_readonly_method("workspace/remove"));
        assert!(!is_safe_readonly_method("workspace/set"));
        assert!(!is_safe_readonly_method("logging"));

        // Unknown / arbitrary tools must never be retried (fail-safe opt-in allowlist)
        assert!(!is_safe_readonly_method("unknown_tool"));
        assert!(!is_safe_readonly_method("arbitrary/action"));
        assert!(!is_safe_readonly_method(""));
    }

    #[test]
    fn test_extract_jsonrpc_id_and_method() {
        let msg = b"{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"status\"}";
        assert_eq!(extract_jsonrpc_id(msg), Some("42".to_string()));
        assert_eq!(extract_jsonrpc_method(msg), Some("status"));

        let str_id = b"{\"jsonrpc\":\"2.0\",\"id\":\"req-abc-123\",\"method\":\"search\"}";
        assert_eq!(
            extract_jsonrpc_id(str_id),
            Some("\"req-abc-123\"".to_string())
        );
        assert_eq!(extract_jsonrpc_method(str_id), Some("search"));

        let no_id = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}";
        assert_eq!(extract_jsonrpc_id(no_id), None);
        assert_eq!(
            extract_jsonrpc_method(no_id),
            Some("notifications/initialized")
        );
    }

    #[test]
    fn test_make_jsonrpc_error_response() {
        let err = make_jsonrpc_error_response("42", -32603, "Daemon disconnected");
        let err_str = std::str::from_utf8(&err).expect("valid utf8");
        assert!(err_str.ends_with('\n'));
        assert!(err_str.contains("\"id\":42"));
        assert!(err_str.contains("\"code\":-32603"));
        assert!(err_str.contains("\"Daemon disconnected\""));
    }

    #[test]
    fn test_parse_one_framed_message() {
        // Content-Length framing (LSP style)
        let body = "{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}";
        let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let parsed = parse_one_framed_message(raw.as_bytes(), 0);
        assert!(parsed.is_some());
        let (framed, next) = parsed.unwrap();
        assert_eq!(framed, raw.as_bytes());
        assert_eq!(next, raw.len());

        // Newline-delimited framing (MCP style)
        let mcp_raw = format!("{body}\n");
        let parsed_mcp = parse_one_framed_message(mcp_raw.as_bytes(), 0);
        assert!(parsed_mcp.is_some());
        let (framed_mcp, next_mcp) = parsed_mcp.unwrap();
        assert_eq!(framed_mcp, mcp_raw.as_bytes());
        assert_eq!(next_mcp, mcp_raw.len());
    }

    #[test]
    fn test_relay_recovery_info() {
        let info = relay_recovery_info();
        assert_eq!(info.get("recovery_budget_secs").unwrap(), "30");
        assert_eq!(info.get("backoff_steps_ms").unwrap(), "100,250,500,1000");
    }

    #[test]
    fn test_derive_socket_name_deterministic() {
        let p1 = Path::new("C:/test/path/attic.db");
        let p2 = Path::new("C:/test/path/attic.db");
        assert_eq!(derive_socket_name(p1), derive_socket_name(p2));
    }
}
