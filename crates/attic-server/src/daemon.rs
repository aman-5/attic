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

    if lock_file.try_lock().is_err() {
        return Ok(ElectAttempt::NotElected);
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

/// Run the full election protocol for `db_path`: `try_lock()` first; on
/// success this process is the daemon. On failure, discover and connect to
/// the incumbent daemon's IPC address, retrying election (in case the
/// incumbent crashed and left a stale `attic.ipc`) within a bounded total
/// budget before giving up with a clear, distinct error.
pub(crate) async fn elect(db_path: &Path) -> anyhow::Result<ElectionResult> {
    let lock_path = db_path.with_file_name("attic.lock");
    let ipc_path = db_path.with_file_name("attic.ipc");

    match try_become_daemon(db_path, &lock_path, &ipc_path)? {
        ElectAttempt::Daemon(handle) => return Ok(ElectionResult::Daemon(handle)),
        ElectAttempt::BindOrIpcFailed(lock_file, e) => {
            warn!(
                "attic: daemon socket/IPC setup failed ({e}); falling back to legacy \
                 single-process mode for this launch (attic.lock is still held)"
            );
            return Ok(ElectionResult::Fallback(lock_file));
        }
        ElectAttempt::NotElected => {}
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
                    match try_become_daemon(db_path, &lock_path, &ipc_path)? {
                        ElectAttempt::Daemon(handle) => return Ok(ElectionResult::Daemon(handle)),
                        ElectAttempt::BindOrIpcFailed(lock_file, e) => {
                            warn!(
                                "attic: daemon socket/IPC setup failed ({e}); falling back to \
                                 legacy single-process mode for this launch (attic.lock is \
                                 still held)"
                            );
                            return Ok(ElectionResult::Fallback(lock_file));
                        }
                        ElectAttempt::NotElected => {}
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

/// Relay path: splice this process's own stdin/stdout to the connected IPC
/// stream. No per-tool forwarding logic — this is a byte-level splice of the
/// same rmcp stdio protocol the daemon side already speaks. stdin and stdout
/// are different concrete tokio types, so a single `copy_bidirectional`
/// doesn't apply; instead two independent directional copies are raced so
/// that EOF/disconnect on EITHER side ends the relay cleanly.
pub(crate) async fn run_relay(relay: RelayHandle) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;

    let (mut recv_half, mut send_half) = relay.stream.split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut send_half) => {
            if let Err(e) = result {
                warn!("relay: error copying stdin to daemon: {e}");
            }
            eprintln!("attic: stdin closed; relay exiting");
        }
        result = tokio::io::copy(&mut recv_half, &mut stdout) => {
            match result {
                Ok(_) => eprintln!("attic: daemon connection closed; relay exiting"),
                Err(e) => eprintln!("attic: lost connection to daemon: {e}"),
            }
        }
    }

    let _ = stdout.flush().await;
    Ok(())
}

type CancelTokens =
    Arc<std::sync::Mutex<HashMap<u64, rmcp::service::RunningServiceCancellationToken>>>;

/// RAII guard for one per-connection task's contribution to the live-
/// connection counter. Constructed before `.serve()`/`.waiting()` is ever
/// awaited and held for the task's entire body, so `Drop` runs the exact
/// same decrement+signal on every exit path — normal return, early return
/// on a `serve()` error, AND a panic unwinding through the awaited future —
/// instead of only on the explicit-decrement paths a plain function body
/// would cover. Without this, a panic mid-session permanently leaked the
/// counter and prevented idle-timeout from ever re-arming.
struct ActiveGuard {
    active: Arc<AtomicUsize>,
    conn_done: tokio::sync::mpsc::UnboundedSender<()>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        let _ = self.conn_done.send(());
    }
}

/// Drives an already-started MCP session to completion, tracked in the same
/// cancel-token bookkeeping regardless of which transport started it (own
/// stdio vs. an accepted IPC connection) — shared tail for
/// [`handle_connection`] and [`handle_stdio_connection`]. The live-
/// connection counter itself is handled by the caller's [`ActiveGuard`], not
/// here.
async fn run_session(
    running: rmcp::service::RunningService<rmcp::RoleServer, AtticServer>,
    cancel_tokens: CancelTokens,
    conn_id: u64,
) {
    cancel_tokens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(conn_id, running.cancellation_token());

    let reason = running.waiting().await;

    cancel_tokens
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&conn_id);

    match reason {
        Ok(r) => debug!("daemon: connection {conn_id} closed: {r:?}"),
        Err(e) => warn!("daemon: connection {conn_id} ended with error: {e}"),
    }
}

/// Runs one accepted IPC connection's MCP session to completion: identical
/// to today's single stdio call (`server.serve(stdio())`), just with the
/// accepted IPC stream instead of stdio, mirroring the spec's "no per-tool
/// forwarding logic" design — `AtticServer` is already `Clone`+`Arc`-backed.
async fn handle_connection(
    server: AtticServer,
    stream: IpcStream,
    active: Arc<AtomicUsize>,
    conn_done: tokio::sync::mpsc::UnboundedSender<()>,
    cancel_tokens: CancelTokens,
    conn_id: u64,
) {
    let _guard = ActiveGuard { active, conn_done };
    let running = match server.serve(stream).await {
        Ok(running) => running,
        Err(e) => {
            warn!("daemon: failed to start MCP session on accepted connection: {e}");
            return;
        }
    };
    run_session(running, cancel_tokens, conn_id).await;
}

/// Runs the MCP session for the daemon's OWN stdio to completion. The
/// process that wins the election is still, from its own caller's point of
/// view, an ordinary attic-server launch talking over its own stdin/stdout
/// — it must keep serving that caller in addition to running the accept
/// loop for future relays. Sharing `run_session` means this session is
/// tracked in the exact same active-connection/idle-timeout/Ctrl+C
/// accounting as any accepted IPC relay connection.
async fn handle_stdio_connection(
    server: AtticServer,
    active: Arc<AtomicUsize>,
    conn_done: tokio::sync::mpsc::UnboundedSender<()>,
    cancel_tokens: CancelTokens,
    conn_id: u64,
) {
    let _guard = ActiveGuard { active, conn_done };
    let running = match server.serve(rmcp::transport::stdio()).await {
        Ok(running) => running,
        Err(e) => {
            warn!("daemon: failed to start MCP session on own stdio: {e}");
            return;
        }
    };
    run_session(running, cancel_tokens, conn_id).await;
}

/// Daemon accept loop and lifecycle: accept IPC connections, spawning
/// `server.clone().serve(ipc_stream)` per connection (tracked so shutdown
/// can wait for them); track the live connection count; when it hits zero,
/// arm an idle timer (cancelled by any new connection accepted before it
/// fires); on idle-timeout OR SIGINT, stop accepting, cancel any still-live
/// MCP sessions, bound-wait for them to finish, then run the existing
/// shutdown sequence exactly once.
pub(crate) async fn run_daemon_accept_loop(
    server: AtticServer,
    semantic_enricher: Option<attic_semantic::BackgroundEnricher>,
    handle: DaemonHandle,
) -> anyhow::Result<()> {
    let DaemonHandle {
        _lock_guard,
        listener,
        ipc_path,
    } = handle;
    let shutdown_handles = ShutdownHandles::capture(&server);

    let active: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    let cancel_tokens: CancelTokens = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let (conn_done_tx, mut conn_done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    let mut next_conn_id: u64 = 0;

    let (ctrlc_tx, mut ctrlc_rx) = tokio::sync::oneshot::channel::<()>();
    let ctrlc_task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            let _ = ctrlc_tx.send(());
        }
    });

    info!("attic daemon listening (ipc={})", ipc_path.display());

    // This process's OWN caller (whoever spawned it) is talking to it over
    // its own stdin/stdout, exactly like a legacy single-process launch —
    // winning the election doesn't change that. Serve it as connection 0,
    // sharing the same accounting as every IPC relay accepted below, so
    // idle-timeout/Ctrl+C treat "my own caller disconnected" identically to
    // "a relay's caller disconnected".
    {
        active.fetch_add(1, Ordering::SeqCst);
        let conn_id = next_conn_id;
        next_conn_id += 1;
        tasks.spawn(handle_stdio_connection(
            server.clone(),
            active.clone(),
            conn_done_tx.clone(),
            cancel_tokens.clone(),
            conn_id,
        ));
    }

    let shutdown_reason: String = loop {
        let count = active.load(Ordering::SeqCst);

        tokio::select! {
            biased;

            _ = &mut ctrlc_rx => {
                info!("attic daemon: ctrl_c/SIGINT received - initiating graceful shutdown");
                break "ctrl_c/SIGINT".to_string();
            }

            accept_res = listener.accept() => {
                match accept_res {
                    Ok(stream) => {
                        active.fetch_add(1, Ordering::SeqCst);
                        let conn_id = next_conn_id;
                        next_conn_id += 1;
                        tasks.spawn(handle_connection(
                            server.clone(),
                            stream,
                            active.clone(),
                            conn_done_tx.clone(),
                            cancel_tokens.clone(),
                            conn_id,
                        ));
                    }
                    Err(e) => {
                        warn!("attic daemon: error accepting IPC connection: {e}");
                    }
                }
                continue;
            }

            // Only armed while there are zero active connections; a new
            // connection accepted above naturally takes priority (`biased`)
            // and re-loops before this can fire, which is how a fresh
            // connection "cancels" the idle timer.
            _ = tokio::time::sleep(idle_timeout()), if count == 0 => {
                info!(
                    "attic daemon: idle timeout ({:?}) reached with no active connections - \
                     shutting down",
                    idle_timeout()
                );
                break "idle timeout".to_string();
            }

            _ = conn_done_rx.recv() => {
                // A connection just closed: loop back around to re-evaluate
                // the idle timer against the updated count.
                continue;
            }
        }
    };

    ctrlc_task.abort();

    // Stop accepting new connections right now: explicitly drop the
    // listener here rather than just letting the loop above stop polling
    // it. Merely stopping the poll leaves the socket/pipe open, so the OS
    // can still silently accept a connection that never gets serviced
    // during the shutdown window; a racing client would then hang instead
    // of getting an immediate, honest connection-refused. Dropping it here
    // closes it cleanly.
    drop(listener);

    // Drop the server template now that the accept loop has ended and
    // nothing will spawn further connections from it. Every already-
    // spawned connection task holds its own clone (including its own
    // `Arc<WriterQueue>` reference via `_queue`), so this alone doesn't
    // zero the refcount — the `tasks.join_next()` loop below does that as
    // each connection's own clone is dropped. This must happen before
    // `run_shutdown_sequence`'s WAL checkpoint runs: `WriterQueue`'s `Drop`
    // joins the writer thread, and the checkpoint assumes the writer is
    // already fully stopped — the same invariant the legacy stdio path
    // (`serve_until_closed`) already provides.
    drop(server);

    // Cancel every still-running MCP session so its rmcp service can close
    // gracefully, mirroring the single-process Ctrl+C path, then bound-wait
    // for all connection tasks to actually finish before touching shared DB
    // resources in `run_shutdown_sequence`.
    {
        let mut map = cancel_tokens.lock().unwrap_or_else(|e| e.into_inner());
        for (_, token) in map.drain() {
            token.cancel();
        }
    }

    let join_deadline = Duration::from_millis(attic_core::resources::GRACEFUL_SHUTDOWN_TIMEOUT_MS);
    let join_result = tokio::time::timeout(join_deadline, async {
        while let Some(res) = tasks.join_next().await {
            if let Err(e) = res {
                warn!("attic daemon: connection task panicked during shutdown: {e}");
            }
        }
    })
    .await;
    if join_result.is_err() {
        warn!(
            "attic daemon: {} connection task(s) did not finish within the shutdown timeout; \
             proceeding with database shutdown maintenance regardless",
            tasks.len()
        );
    }

    run_shutdown_sequence(shutdown_handles, semantic_enricher, &shutdown_reason).await;

    // Best-effort: remove the address-discovery file as LATE as possible —
    // right before `_lock_guard` drops at the end of this function (i.e.
    // right before this process actually stops being electable) — so a
    // client racing this shutdown that already read a still-present
    // `attic.ipc` keeps patiently retrying (this is a live, in-progress
    // shutdown, bounded by `CLIENT_TOTAL_RETRY_BUDGET`) instead of hitting
    // a misleading "old pre-daemon build" error.
    let _ = std::fs::remove_file(&ipc_path);

    Ok(())
}
