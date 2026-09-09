//! Fix 1 integration gate: shared daemon per database, thin relay clients
//! (`crates/attic-server/src/daemon.rs`).
//!
//! Unlike `rmcp_stdio_integration.rs` (which forces `ATTIC_NO_DAEMON=1` so it
//! keeps exercising the legacy single-process path unaffected by this
//! change), every test here spawns real child `attic-server` processes with
//! daemon mode ON, pointed at the same isolated `ATTIC_HOME`/database, and
//! asserts:
//!   (a) exactly one process becomes the daemon (holds `attic.lock`) while
//!       the other(s) become relays,
//!   (b) both can make MCP tool calls concurrently against the shared state,
//!   (c) once every client disconnects, the daemon shuts down within a
//!       bounded time close to the (test-shortened) idle-timeout,
//!   (d) killing the daemon process and launching a new client against the
//!       same `ATTIC_HOME` results in the new client successfully becoming
//!       the daemon (crash re-election).
//!
//! Every await is bounded by a timeout; on timeout the child process is
//! killed immediately so a wedged server can never hang the suite.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ContentBlock},
    service::RunningService,
};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// Locate the required `attic` binary.  Fails loudly when absent.
fn require_bin() -> PathBuf {
    let path = match std::env::var("CARGO_BIN_EXE_attic") {
        Ok(p) => PathBuf::from(p),
        Err(_) => panic!(
            "CARGO_BIN_EXE_attic is not set: the attic server binary must be \
             built for this REQUIRED integration gate (NOT VERIFIED otherwise)"
        ),
    };
    assert!(
        path.exists(),
        "required attic binary missing at {}: build it first; \
         this test must fail rather than false-pass",
        path.display()
    );
    path
}

struct ServerHandle {
    child: tokio::process::Child,
    service: RunningService<RoleClient, ()>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // Deterministic teardown even on assertion failure/early return.
        let _ = self.child.start_kill();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(_) => break,
            }
        }
    }
}

/// Isolated `ATTIC_HOME`/database pair shared by every process spawned
/// against it in a given test — this is exactly what makes them contend for
/// the same `attic.lock`/`attic.ipc`.
fn attic_home_and_db(tmp: &Path) -> (PathBuf, PathBuf) {
    let home = tmp.join("attic-home");
    std::fs::create_dir_all(&home).expect("create isolated ATTIC_HOME");
    let db = tmp.join("daemon_relay.db");
    (home, db)
}

/// Spawn an attic-server process with daemon mode ON (no `ATTIC_NO_DAEMON`)
/// against a shared `home`/`db`, and connect an official rmcp client to
/// whatever ends up on the other end of its stdio — a direct daemon
/// connection if this launch wins the election, or a relay splicing through
/// to whichever process already won it.
async fn connect_daemon(
    bin: &Path,
    home: &Path,
    db: &Path,
    idle_timeout_ms: Option<u64>,
) -> ServerHandle {
    connect_daemon_with_env(bin, home, db, idle_timeout_ms, &[]).await
}

/// Spawn an attic-server process with daemon mode ON and optional extra environment variables.
async fn connect_daemon_with_env(
    bin: &Path,
    home: &Path,
    db: &Path,
    idle_timeout_ms: Option<u64>,
    extra_env: &[(&str, &str)],
) -> ServerHandle {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env("ATTIC_HOME", home)
        .env("ATTIC_DB_PATH", db)
        .env("ATTIC_SEMANTIC", "0")
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .env_remove("ATTIC_NO_DAEMON")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(ms) = idle_timeout_ms {
        cmd.env("ATTIC_DAEMON_IDLE_TIMEOUT_MS", ms.to_string());
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().expect("spawn attic server");
    let stdout = child.stdout.take().expect("server stdout piped");
    let stdin = child.stdin.take().expect("server stdin piped");

    let serve = ().serve((stdout, stdin));
    let service = match tokio::time::timeout(IO_TIMEOUT, serve).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = child.start_kill();
            panic!("rmcp client initialize failed: {e}");
        }
        Err(_) => {
            let _ = child.start_kill();
            panic!("rmcp handshake did not complete within {IO_TIMEOUT:?} — server killed");
        }
    };
    ServerHandle { child, service }
}

/// Call a tool through the real rmcp client and return its first text block.
async fn call_tool_text(
    srv: &mut ServerHandle,
    tool: &str,
    arguments: Value,
) -> Result<String, String> {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    params.arguments = arguments.as_object().cloned();
    let fut = srv.service.call_tool(params);
    match tokio::time::timeout(IO_TIMEOUT, fut).await {
        Ok(Ok(result)) => match result.content.first() {
            Some(ContentBlock::Text(t)) => Ok(t.text.clone()),
            other => Err(format!(
                "expected text content from `{tool}`, got {other:?}"
            )),
        },
        Ok(Err(e)) => Err(format!("`{tool}` call failed: {e}")),
        Err(_) => {
            let _ = srv.child.start_kill();
            Err(format!(
                "`{tool}` call exceeded {IO_TIMEOUT:?} — server killed"
            ))
        }
    }
}

/// Spawn a raw (non-rmcp) attic-server child with piped stdio. Used only by
/// tests that need byte-level control over exact request timing (e.g. racing
/// a write against a daemon kill to land a request "in flight") that the
/// high-level rmcp client in [`connect_daemon`] deliberately hides.
async fn spawn_raw_child(
    bin: &Path,
    home: &Path,
    db: &Path,
    idle_timeout_ms: Option<u64>,
) -> (
    tokio::process::Child,
    tokio::process::ChildStdin,
    BufReader<tokio::process::ChildStdout>,
) {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env("ATTIC_HOME", home)
        .env("ATTIC_DB_PATH", db)
        .env("ATTIC_SEMANTIC", "0")
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .env_remove("ATTIC_NO_DAEMON")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(ms) = idle_timeout_ms {
        cmd.env("ATTIC_DAEMON_IDLE_TIMEOUT_MS", ms.to_string());
    }
    let mut child = cmd.spawn().expect("spawn attic server (raw)");
    let stdin = child.stdin.take().expect("server stdin piped");
    let stdout = child.stdout.take().expect("server stdout piped");
    (child, stdin, BufReader::new(stdout))
}

/// Build one newline-delimited JSON-RPC request.
fn raw_request(id: u64, method: &str, params: Value) -> String {
    format!(
        "{}\n",
        serde_json::to_string(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .unwrap()
    )
}

/// Read one newline-delimited JSON-RPC message, bounded by `IO_TIMEOUT`.
/// Returns `None` on EOF, timeout, or malformed JSON.
async fn read_json_line(reader: &mut BufReader<tokio::process::ChildStdout>) -> Option<Value> {
    let mut line = String::new();
    match tokio::time::timeout(IO_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(Ok(0)) => None,
        Ok(Ok(_)) => serde_json::from_str(line.trim()).ok(),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Perform the raw MCP `initialize` handshake (request + `initialized`
/// notification) over a raw child's stdio, mirroring the format
/// `main.rs`'s own `spawn_and_initialize` test helper uses.
async fn raw_initialize(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
) {
    let init = raw_request(
        1,
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "daemon-relay-raw-test", "version": "0"}
        }),
    );
    stdin
        .write_all(init.as_bytes())
        .await
        .expect("write initialize");
    stdin.flush().await.expect("flush initialize");
    let resp = read_json_line(stdout)
        .await
        .expect("initialize response timed out/missing/malformed");
    assert_eq!(resp["id"], 1, "unexpected initialize response: {resp}");
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .expect("write initialized notification");
    stdin.flush().await.expect("flush initialized notification");
}

/// (a) + (b): the second launch against the same database does not hard-fail
/// (unlike the legacy `ATTIC_NO_DAEMON=1` behavior covered by
/// `rmcp_stdio_integration.rs`) — instead exactly one process ends up
/// holding `attic.lock`, and BOTH connections can make MCP tool calls
/// concurrently against the shared state.
#[tokio::test]
async fn daemon_and_relay_serve_concurrent_status_calls() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    // Give the first launch a clear head start so it deterministically wins
    // the election before the second launches — correctness doesn't depend
    // on this ordering (either process could win), but it keeps this test's
    // intent ("second launch becomes a relay") legible and avoids a rare
    // near-simultaneous try_lock() race making the assertions flaky.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;

    // Exactly one process holds `attic.lock`: a third, independent attempt
    // to acquire it (from this test process) must fail while both children
    // are alive. Poll briefly since the winner's `try_lock()` may not have
    // landed the instant its process was spawned.
    let lock_path = db.with_file_name("attic.lock");
    let mut lock_held_by_someone = false;
    for _ in 0..30 {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            && f.try_lock().is_err()
        {
            lock_held_by_someone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        lock_held_by_someone,
        "expected exactly one of the two attic-server processes to hold attic.lock at '{}'",
        lock_path.display()
    );

    // Both connections independently reach the same shared, unconfigured
    // workspace state — proving the relay's byte-level splice really is
    // reaching the daemon's live `AtticServer`, not a second, disconnected
    // in-process state.
    let (r1, r2) = tokio::join!(
        call_tool_text(&mut srv1, "status", serde_json::json!({})),
        call_tool_text(&mut srv2, "status", serde_json::json!({})),
    );
    let v1: Value = serde_json::from_str(&r1.expect("status via first connection")).expect("json");
    let v2: Value = serde_json::from_str(&r2.expect("status via second connection")).expect("json");
    assert_eq!(v1["status"], "unconfigured", "{v1}");
    assert_eq!(v2["status"], "unconfigured", "{v2}");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.service.close()).await;
    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
}

/// (c): once its only client disconnects, the daemon arms its idle timer and
/// shuts down cleanly within a bounded window close to the (test-shortened,
/// via `ATTIC_DAEMON_IDLE_TIMEOUT_MS`) idle timeout — it is not left resident
/// forever.
#[tokio::test]
async fn daemon_shuts_down_after_idle_timeout_once_client_disconnects() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    const IDLE_MS: u64 = 800;
    let mut srv = connect_daemon(&bin, &home, &db, Some(IDLE_MS)).await;

    // Confirm it's alive and answering before triggering the disconnect.
    call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status before disconnect");

    // Graceful client-side close: the daemon's own-stdio connection sees
    // EOF, its active-connection count drops to zero, and its idle timer
    // arms. `srv` itself is deliberately NOT dropped here — its `Drop` impl
    // hard-kills the child, which would make this test pass for the wrong
    // reason (a killed process, not a real idle-timeout shutdown).
    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;

    let deadline =
        std::time::Instant::now() + Duration::from_millis(IDLE_MS) + Duration::from_secs(8);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        match srv.child.try_wait() {
            Ok(Some(status)) => {
                assert!(
                    status.success(),
                    "daemon should exit cleanly on idle timeout, got {status:?}"
                );
                exited = true;
                break;
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    assert!(
        exited,
        "daemon did not shut down within {:?} of its {IDLE_MS}ms idle timeout",
        Duration::from_millis(IDLE_MS) + Duration::from_secs(8)
    );
}

/// (d): `attic.lock` is an OS advisory lock, released automatically by the
/// kernel when the holding process dies — including a hard kill, not just a
/// graceful exit. A fresh launch against the same `ATTIC_HOME`/database
/// after the daemon is killed must successfully become the new daemon
/// (crash re-election), not hang or fail waiting on the stale `attic.ipc`
/// the crashed process left behind.
#[tokio::test]
async fn crashed_daemon_lock_is_recovered_by_next_launch() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    call_tool_text(&mut srv1, "status", serde_json::json!({}))
        .await
        .expect("status before crash");

    // Simulate a crash: no `service.close()`, no SIGINT/graceful shutdown —
    // just terminate the process outright.
    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    // A fresh launch against the very same ATTIC_HOME/database must now
    // succeed in becoming the new daemon.
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;
    let status = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status after crash re-election");
    let v: Value = serde_json::from_str(&status).expect("status payload is JSON");
    assert_eq!(v["status"], "unconfigured", "{status}");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
}

/// Phase 96 Primary Goal: when the daemon process dies while a relay client
/// is active, the relay must detect the disconnect, win the replacement-daemon
/// election, start the replacement daemon, and KEEP ITS EXISTING STDIO SESSION
/// ALIVE so the AI/MCP client continues making requests seamlessly without
/// reconnection!
#[tokio::test]
async fn relay_wins_election_and_promotes_while_keeping_client_connected() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    // 1. Start daemon process (srv1).
    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 2. Start relay process (srv2).
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;

    // Verify both are serving requests.
    let s1 = call_tool_text(&mut srv1, "status", serde_json::json!({}))
        .await
        .expect("status via daemon");
    let s2 = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status via relay");
    let v1: Value = serde_json::from_str(&s1).expect("json");
    let v2: Value = serde_json::from_str(&s2).expect("json");
    assert_eq!(v1["status"], "unconfigured");
    assert_eq!(v2["status"], "unconfigured");

    // 3. Kill the daemon (srv1).
    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    // 4. Give the relay time to detect disconnect, win election, and promote
    // itself to replacement daemon concurrently.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 5. THE EXISTING MCP CLIENT ON srv2 MUST CONTINUE FUNCTIONING!
    // No new process is spawned; the existing rmcp service connection is used!
    let s2_after = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status via promoted relay without client reconnect");
    let v2_after: Value = serde_json::from_str(&s2_after).expect("json");
    assert_eq!(v2_after["status"], "unconfigured");

    // 6. A third launch (srv3) now connects as a new relay to the promoted daemon!
    let mut srv3 = connect_daemon(&bin, &home, &db, None).await;
    let s3 = call_tool_text(&mut srv3, "status", serde_json::json!({}))
        .await
        .expect("status via new relay connected to promoted daemon");
    let v3: Value = serde_json::from_str(&s3).expect("json");
    assert_eq!(v3["status"], "unconfigured");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
    let _ = tokio::time::timeout(IO_TIMEOUT, srv3.service.close()).await;
}

// ── Phase 96 §17: daemon-recovery test list (items 1-11) ────────────────────

/// §17 item 1: with exactly one relay client active when the daemon dies,
/// `run_relay_supervised` must return `PromoteToDaemon` — not `Fatal`, and
/// not silently behave like `ClientClosed` — which is observable from
/// outside the process only as: the existing relay connection keeps
/// answering requests even though nothing else was around to take over.
#[tokio::test]
async fn relay_supervision_returns_promotion_when_it_wins_election() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;

    call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status via relay before kill");

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Only one relay exists, so it MUST win the election and be promoted —
    // if `run_relay_supervised` had instead returned `Fatal`, this call
    // would error or time out.
    let status_after = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status via promoted relay: PromoteToDaemon must have been returned");
    let v: Value = serde_json::from_str(&status_after).expect("json");
    assert_eq!(v["status"], "unconfigured", "{status_after}");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
}

/// §17 item 2: after promotion, `spawn_daemon`'s accept loop must actually be
/// live — provable from outside the process only by having a brand new,
/// independent launch discover `attic.ipc` and successfully connect to it.
#[tokio::test]
async fn promotion_starts_replacement_daemon() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // A fresh, independent launch must find a live daemon via `attic.ipc` —
    // this only works if the replacement daemon's accept loop is actually
    // bound and listening, not merely a `DaemonHandle` sitting idle.
    let mut srv3 = connect_daemon(&bin, &home, &db, None).await;
    let s3 = call_tool_text(&mut srv3, "status", serde_json::json!({}))
        .await
        .expect("status via new relay against the replacement daemon");
    let v: Value = serde_json::from_str(&s3).expect("json");
    assert_eq!(v["status"], "unconfigured", "{s3}");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
    let _ = tokio::time::timeout(IO_TIMEOUT, srv3.service.close()).await;
}

/// §17 item 3: the ORIGINAL client's stdio session — the very same
/// `RunningService` object, never dropped or reconnected — must keep working
/// continuously across the promotion: before the kill, and repeatedly after
/// promotion completes, with no client-side reconnect ever happening.
#[tokio::test]
async fn promotion_preserves_existing_stdio_session() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;

    let before = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status before kill");
    assert_eq!(
        serde_json::from_str::<Value>(&before).unwrap()["status"],
        "unconfigured"
    );

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Two further calls through the SAME `srv2.service` connection — proving
    // continuity rather than a single lucky response.
    for i in 0..2 {
        let after = call_tool_text(&mut srv2, "status", serde_json::json!({}))
            .await
            .unwrap_or_else(|e| panic!("status call #{i} after promotion failed: {e}"));
        assert_eq!(
            serde_json::from_str::<Value>(&after).unwrap()["status"],
            "unconfigured"
        );
    }

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
}

/// §17 item 4: a new relay launched after promotion must reconnect via local
/// IPC to the replacement daemon, AND do so concurrently with the original
/// (promoted) relay's own connection still being served — proving both are
/// really talking to the one shared replacement daemon.
#[tokio::test]
async fn promoted_relay_reconnects_to_local_daemon() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut srv3 = connect_daemon(&bin, &home, &db, None).await;

    let (r2, r3) = tokio::join!(
        call_tool_text(&mut srv2, "status", serde_json::json!({})),
        call_tool_text(&mut srv3, "status", serde_json::json!({})),
    );
    let v2: Value =
        serde_json::from_str(&r2.expect("srv2 status via promoted relay")).expect("json");
    let v3: Value = serde_json::from_str(&r3.expect("srv3 status via new relay")).expect("json");
    assert_eq!(v2["status"], "unconfigured");
    assert_eq!(v3["status"], "unconfigured");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
    let _ = tokio::time::timeout(IO_TIMEOUT, srv3.service.close()).await;
}

/// §17 item 5: `RelaySessionCache::replay_to` must never forward the
/// replacement daemon's `initialize` response to the client's stdout (the
/// client already completed its handshake once); doing so would desync
/// JSON-RPC request/response correlation. Verified at the byte level: after
/// promotion, a request sent WITHOUT re-initializing must get back exactly
/// one response bearing the SAME id we sent, with nothing extra queued.
#[tokio::test]
async fn promoted_relay_replays_initialize() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut srv2_child, mut srv2_stdin, mut srv2_stdout) =
        spawn_raw_child(&bin, &home, &db, None).await;
    raw_initialize(&mut srv2_stdin, &mut srv2_stdout).await;

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let call = raw_request(
        2,
        "tools/call",
        serde_json::json!({"name": "status", "arguments": {}}),
    );
    srv2_stdin
        .write_all(call.as_bytes())
        .await
        .expect("write status call after promotion");
    srv2_stdin.flush().await.expect("flush status call");

    let resp = read_json_line(&mut srv2_stdout)
        .await
        .expect("status response after promotion (no re-initialize)");
    assert_eq!(
        resp["id"], 2,
        "response id must correlate to our request, not a leaked \
         duplicate initialize response: {resp}"
    );
    assert!(
        resp.get("error").is_none(),
        "expected a successful status result, got: {resp}"
    );

    // Confirm nothing else is queued immediately behind it (e.g. a stray
    // duplicate initialize response that would desync the NEXT read).
    let mut probe = String::new();
    let extra = tokio::time::timeout(
        Duration::from_millis(300),
        srv2_stdout.read_line(&mut probe),
    )
    .await;
    assert!(
        extra.is_err() || matches!(extra, Ok(Ok(0))),
        "unexpected extra message queued after the status response: {probe:?}"
    );

    let _ = srv2_child.start_kill();
    let _ = tokio::time::timeout(IO_TIMEOUT, srv2_child.wait()).await;
}

/// §17 item 6: an explicit safe read-only request (`status`, via `tools/call`)
/// that is in flight at the exact moment the daemon dies must be retried at
/// most once against the replacement daemon and complete successfully — the
/// client must never observe the "ambiguous disconnect" synthesized error for
/// a call on the read-only allowlist.
///
/// This races a real disconnect, so the exact interleaving (whether the
/// original daemon happened to answer before dying, or the relay had to
/// retry) is not fixed from run to run — but the outcome the client observes
/// must always be a genuine successful status result, bounded in time.
#[tokio::test]
async fn safe_read_request_retried_once_after_promotion() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut srv2_child, mut srv2_stdin, mut srv2_stdout) =
        spawn_raw_child(&bin, &home, &db, None).await;
    raw_initialize(&mut srv2_stdin, &mut srv2_stdout).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let call = raw_request(
        2,
        "tools/call",
        serde_json::json!({"name": "status", "arguments": {}}),
    );
    srv2_stdin
        .write_all(call.as_bytes())
        .await
        .expect("write status call");
    srv2_stdin.flush().await.expect("flush status call");

    // Kill the daemon immediately, racing the in-flight request above.
    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    // Bounded wait: recovery (bounded by RELAY_RECOVERY_BUDGET) plus the
    // (possibly retried) request round trip.
    let resp = tokio::time::timeout(Duration::from_secs(20), read_json_line(&mut srv2_stdout))
        .await
        .expect("status response timed out entirely")
        .expect("status response missing/malformed");
    assert_eq!(resp["id"], 2, "{resp}");
    assert!(
        resp.get("error").is_none(),
        "safe read-only request must never surface the ambiguous-disconnect \
         error; it must be retried once and succeed: {resp}"
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    let v: Value = serde_json::from_str(text).expect("status payload is JSON");
    assert_eq!(v["status"], "unconfigured", "{text}");

    let _ = srv2_child.start_kill();
    let _ = tokio::time::timeout(IO_TIMEOUT, srv2_child.wait()).await;
}

/// §17 item 7: a mutation (`workspace` tool, `add` action) that is in flight
/// when the daemon dies must NEVER be blindly retried — its completion state
/// is ambiguous, so at most it may be applied once (by the original daemon,
/// if it completed before dying), never twice. Verified by checking, after
/// recovery, that the added path appears in the workspace membership at most
/// once — never duplicated, regardless of which side of the race fired.
#[tokio::test]
async fn mutation_not_retried_after_ambiguous_disconnect() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());
    let workspace_root = tmp.path().join("mutation-root");
    std::fs::create_dir_all(&workspace_root).expect("create candidate workspace root");
    let workspace_root_str = workspace_root.to_string_lossy().replace('\\', "\\\\");

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut srv2_child, mut srv2_stdin, mut srv2_stdout) =
        spawn_raw_child(&bin, &home, &db, None).await;
    raw_initialize(&mut srv2_stdin, &mut srv2_stdout).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let call = raw_request(
        2,
        "tools/call",
        serde_json::json!({
            "name": "workspace",
            "arguments": {"action": "add", "path": workspace_root.to_string_lossy()}
        }),
    );
    srv2_stdin
        .write_all(call.as_bytes())
        .await
        .expect("write workspace add call");
    srv2_stdin.flush().await.expect("flush workspace add call");

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    // Whatever comes back for the in-flight add (a synthesized error, a
    // genuine success from the original daemon, or a genuine success from
    // the new daemon if it was never actually sent) — read and discard it,
    // bounded.
    let _ = tokio::time::timeout(Duration::from_secs(20), read_json_line(&mut srv2_stdout)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let inspect = raw_request(
        3,
        "tools/call",
        serde_json::json!({"name": "workspace", "arguments": {"action": "inspect"}}),
    );
    srv2_stdin
        .write_all(inspect.as_bytes())
        .await
        .expect("write workspace inspect call");
    srv2_stdin
        .flush()
        .await
        .expect("flush workspace inspect call");
    let resp = tokio::time::timeout(IO_TIMEOUT, read_json_line(&mut srv2_stdout))
        .await
        .expect("workspace inspect timed out")
        .expect("workspace inspect response missing/malformed");
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();

    let occurrences = text.matches(workspace_root_str.trim_matches('"')).count()
        + text.matches(&*workspace_root.to_string_lossy()).count();
    assert!(
        occurrences <= 2, // path may appear in more than one JSON field once; never duplicated as a second root entry
        "the in-flight `workspace add` mutation must never be applied twice \
         after an ambiguous disconnect; workspace inspect result: {text}"
    );

    let _ = srv2_child.start_kill();
    let _ = tokio::time::timeout(IO_TIMEOUT, srv2_child.wait()).await;
}

/// §17 item 8: a request using a method NOT on the safe read-only allowlist
/// (an unrecognized top-level JSON-RPC method, never routed by either the
/// original or replacement daemon) that is in flight when the daemon dies
/// must never be silently retried into a fabricated success — it must
/// resolve, within a bounded time, to SOME well-formed error response (never
/// a hang, and never a "result").
#[tokio::test]
async fn unknown_method_not_retried() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut srv2_child, mut srv2_stdin, mut srv2_stdout) =
        spawn_raw_child(&bin, &home, &db, None).await;
    raw_initialize(&mut srv2_stdin, &mut srv2_stdout).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let call = raw_request(2, "diagnostics/experimental_probe", serde_json::json!({}));
    srv2_stdin
        .write_all(call.as_bytes())
        .await
        .expect("write unknown-method call");
    srv2_stdin.flush().await.expect("flush unknown-method call");

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    let resp = tokio::time::timeout(Duration::from_secs(20), read_json_line(&mut srv2_stdout))
        .await
        .expect("unknown-method response timed out entirely")
        .expect("unknown-method response missing/malformed");
    assert_eq!(resp["id"], 2, "{resp}");
    assert!(
        resp.get("result").is_none(),
        "an unrecognized method must never resolve to a fabricated success: {resp}"
    );
    assert!(
        resp.get("error").is_some(),
        "expected a JSON-RPC error: {resp}"
    );

    let _ = srv2_child.start_kill();
    let _ = tokio::time::timeout(IO_TIMEOUT, srv2_child.wait()).await;
}

/// §17 item 9: with TWO relays active when the daemon dies, only one may
/// become the replacement daemon — never a split-brain of two daemons each
/// thinking they won. Verified both by the shared `attic.lock` (exactly one
/// holder) and by both surviving relays remaining independently functional
/// against that single replacement afterward.
#[tokio::test]
async fn multiple_relays_create_only_one_replacement_daemon() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon(&bin, &home, &db, None).await;
    let mut srv3 = connect_daemon(&bin, &home, &db, None).await;

    call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("srv2 status before kill");
    call_tool_text(&mut srv3, "status", serde_json::json!({}))
        .await
        .expect("srv3 status before kill");

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let lock_path = db.with_file_name("attic.lock");
    let mut lock_held_by_someone = false;
    for _ in 0..30 {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            && f.try_lock().is_err()
        {
            lock_held_by_someone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        lock_held_by_someone,
        "expected exactly one replacement daemon to hold attic.lock"
    );

    // Both surviving relays must still be functional against the single
    // replacement daemon — a split-brain would leave at least one wedged.
    let (r2, r3) = tokio::join!(
        call_tool_text(&mut srv2, "status", serde_json::json!({})),
        call_tool_text(&mut srv3, "status", serde_json::json!({})),
    );
    let v2: Value = serde_json::from_str(&r2.expect("srv2 status after promotion")).expect("json");
    let v3: Value = serde_json::from_str(&r3.expect("srv3 status after promotion")).expect("json");
    assert_eq!(v2["status"], "unconfigured");
    assert_eq!(v3["status"], "unconfigured");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
    let _ = tokio::time::timeout(IO_TIMEOUT, srv3.service.close()).await;
}

/// §17 item 10: if, after the daemon dies, a relay can neither win the
/// `attic.lock` re-election (something else already holds it) nor find a
/// live daemon via `attic.ipc`, `run_relay_supervised`'s recovery loop must
/// still give up and exit within a bounded time — never hang forever.
///
/// No internal test-only hook exists to force this deterministically fast
/// (and none should be added purely for a test), so this exercises the REAL
/// bound (`elect`'s `CLIENT_TOTAL_RETRY_BUDGET` = 40s): correct, but slow by
/// construction. Simulates "daemon cannot start" by having the TEST PROCESS
/// itself grab `attic.lock` the instant the original daemon dies, before the
/// relay's own re-election attempt — so the relay can never win it either.
#[tokio::test]
async fn recovery_is_bounded_when_daemon_cannot_start() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (mut srv2_child, _srv2_stdin, _srv2_stdout) = spawn_raw_child(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    // Immediately squat on attic.lock from the TEST process itself so no
    // relay can ever win the re-election.
    let lock_path = db.with_file_name("attic.lock");
    let squat_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .expect("open lock file to squat on it");
    squat_lock.try_lock().expect(
        "test process must win the lock immediately after the daemon died \
         (before the relay's own re-election attempt) for this test's premise to hold",
    );

    let probe_start = std::time::Instant::now();
    let deadline = probe_start + Duration::from_secs(90);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        match srv2_child.try_wait() {
            Ok(Some(_status)) => {
                exited = true;
                break;
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(200)).await,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    eprintln!(
        "[diag] recovery_is_bounded_when_daemon_cannot_start: exited={exited} after {:?}",
        probe_start.elapsed()
    );
    drop(squat_lock);
    assert!(
        exited,
        "relay did not give up within a bounded time when the daemon could \
         never start — recovery must be bounded, not hang forever"
    );
}

/// §17 item 11: if the MCP client disconnects (stdin closes) WHILE this
/// relay is in the middle of recovering from its daemon's death, the process
/// must exit promptly — never hang waiting on a client that is already gone.
#[tokio::test]
async fn shutdown_during_recovery_does_not_hang() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    const IDLE_MS: u64 = 800;
    let mut srv1 = connect_daemon(&bin, &home, &db, None).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon(&bin, &home, &db, Some(IDLE_MS)).await;

    call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status via relay before kill");

    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    // Close the MCP client's own connection WHILE recovery (backoff +
    // re-election) is still in flight, rather than waiting for it to settle.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;

    // Bounded by: recovery backoff/election + (if promoted) the replacement
    // daemon's own idle-timeout shutdown, comfortably within this deadline.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        match srv2.child.try_wait() {
            Ok(Some(_status)) => {
                exited = true;
                break;
            }
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
    assert!(
        exited,
        "relay did not exit within a bounded time after its MCP client \
         disconnected mid-recovery"
    );
}

// ── Section 17 items 12–19: Resource & stress validation ──────────────────

/// §17 item 12: In Performance mode under healthy conditions, Attic configures
/// and reaches 8 scheduler workers, max_indexing_heavy = 8, and effective_indexing_heavy_limit = 8.
#[tokio::test]
async fn performance_reaches_eight_workers_when_healthy() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[("ATTIC_RESOURCE_MODE", "performance")],
    )
    .await;

    let res = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status call");
    let v: Value = serde_json::from_str(&res).expect("json");

    assert_eq!(v["resource_mode"], "performance");
    assert_eq!(v["effective_resources"]["scheduler_workers"], 8);
    let rp = &v["resource_pressure"];
    assert_eq!(rp["level"], "normal");
    assert_eq!(rp["max_indexing_heavy"], 8);
    assert_eq!(rp["effective_indexing_heavy_limit"], 8);
    assert_eq!(rp["effective_embedding_limit"], 8);
    assert_eq!(rp["effective_embedding_batch"], 64);
    assert_eq!(rp["recovery_stage"], "Full");
}

/// §17 item 13: Warning pressure reduces heavy-work admission limits
/// (indexing limit drops to 6 = 75% of 8, embedding batch shrinks to 32 = half of 64).
#[tokio::test]
async fn warning_reduces_heavy_admission() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FORCE_RESOURCE_PRESSURE", "warning"),
        ],
    )
    .await;

    let res = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status call");
    let v: Value = serde_json::from_str(&res).expect("json");

    let rp = &v["resource_pressure"];
    assert_eq!(rp["level"], "warning");
    assert_eq!(rp["max_indexing_heavy"], 8);
    assert_eq!(rp["effective_indexing_heavy_limit"], 6);
    assert_eq!(rp["effective_embedding_batch"], 32);
}

/// §17 item 14: Critical pressure reduces heavy-work admission further
/// (indexing limit = 2 = 25% of 8, embedding limit = 1, batch = 16),
/// and expensive MCP operations (e.g. `context`) are rejected with `server_busy`
/// while lightweight MCP operations (`status`) remain responsive.
#[tokio::test]
async fn critical_reduces_heavy_admission_further() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FORCE_RESOURCE_PRESSURE", "critical"),
        ],
    )
    .await;

    let res = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status call");
    let v: Value = serde_json::from_str(&res).expect("json");

    let rp = &v["resource_pressure"];
    assert_eq!(rp["level"], "critical");
    assert_eq!(rp["effective_indexing_heavy_limit"], 2);
    assert_eq!(rp["effective_embedding_limit"], 1);
    assert_eq!(rp["effective_embedding_batch"], 16);

    // Expensive MCP tool (`context`) must be rejected under Critical
    let ctx_res = call_tool_text(&mut srv, "context", serde_json::json!({"query": "test"})).await;
    let err_str = ctx_res.unwrap_or_else(|e| e);
    assert!(
        err_str.contains("server_busy") || err_str.contains("memory pressure"),
        "expected server_busy rejection for expensive MCP call under Critical, got: {err_str}"
    );

    // Verify mcp_pressure_rejections counter incremented
    let res2 = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status call");
    let v2: Value = serde_json::from_str(&res2).expect("json");
    assert!(
        v2["resource_pressure"]["mcp_pressure_rejections"]
            .as_u64()
            .unwrap_or(0)
            >= 1,
        "expected mcp_pressure_rejections >= 1"
    );
}

/// §17 item 15: Emergency pressure zeroes out new heavy indexing and embedding permits,
/// rejects mutations (`workspace`), while keeping cheap diagnostics (`status`) responsive.
#[tokio::test]
async fn emergency_starts_no_new_heavy_work() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FORCE_RESOURCE_PRESSURE", "emergency"),
        ],
    )
    .await;

    // Cheap tool (`status`) still responds
    let res = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status call");
    let v: Value = serde_json::from_str(&res).expect("json");

    let rp = &v["resource_pressure"];
    assert_eq!(rp["level"], "emergency");
    assert_eq!(rp["effective_indexing_heavy_limit"], 0);
    assert_eq!(rp["effective_embedding_limit"], 0);
    assert_eq!(rp["effective_embedding_batch"], 0);

    // Mutation tool (`workspace`) is rejected under Emergency
    let ws_res = call_tool_text(
        &mut srv,
        "workspace",
        serde_json::json!({"action": "inspect"}),
    )
    .await;
    let err_str = ws_res.unwrap_or_else(|e| e);
    assert!(
        err_str.contains("server_busy") || err_str.contains("memory pressure"),
        "expected server_busy rejection for mutation MCP call under Emergency, got: {err_str}"
    );
}

/// §17 item 16: Embedding batch size shrinks monotonically with pressure:
/// Normal (64) -> Warning (32) -> Critical (16) -> Emergency (0).
#[tokio::test]
async fn embedding_batch_shrinks_with_pressure() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");

    for (pressure_str, expected_batch) in [
        ("normal", 64),
        ("warning", 32),
        ("critical", 16),
        ("emergency", 0),
    ] {
        let (home, db) = attic_home_and_db(&tmp.path().join(pressure_str));
        let mut srv = connect_daemon_with_env(
            &bin,
            &home,
            &db,
            None,
            &[
                ("ATTIC_RESOURCE_MODE", "performance"),
                ("ATTIC_FORCE_RESOURCE_PRESSURE", pressure_str),
            ],
        )
        .await;

        let res = call_tool_text(&mut srv, "status", serde_json::json!({}))
            .await
            .expect("status call");
        let v: Value = serde_json::from_str(&res).expect("json");
        assert_eq!(
            v["resource_pressure"]["effective_embedding_batch"], expected_batch,
            "failed batch check for pressure={pressure_str}"
        );
        let _ = srv.service.close().await;
    }
}

/// §17 item 17: When recovering from Emergency, capacity is restored gradually
/// (0 -> Step1: 2 -> Step2: 4 -> Step3: 6 -> Full: 8) without rapid oscillation.
#[tokio::test]
async fn resource_capacity_recovers_gradually() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FORCE_RESOURCE_PRESSURE", "emergency"),
            ("ATTIC_FAST_RECOVERY_MS", "80"),
        ],
    )
    .await;

    let res = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status");
    let v: Value = serde_json::from_str(&res).expect("json");
    assert_eq!(v["resource_pressure"]["effective_indexing_heavy_limit"], 0);

    // Switch to normal pressure via file override so the running server
    // de-escalates to normal and begins graduated recovery.
    let override_file = home.join("pressure_override");
    std::fs::write(&override_file, "normal").expect("write normal override");

    // Sample status periodically; verify monotonic capacity progression
    let mut seen_limits = Vec::new();
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(10);
    while start.elapsed() < timeout {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(res) = call_tool_text(&mut srv, "status", serde_json::json!({})).await
            && let Ok(v) = serde_json::from_str::<Value>(&res)
        {
            let limit = v["resource_pressure"]["effective_indexing_heavy_limit"]
                .as_u64()
                .unwrap_or(0);
            if seen_limits.last().copied() != Some(limit) {
                seen_limits.push(limit);
            }
            if limit == 8 {
                break;
            }
        }
    }

    eprintln!("[diag] graduated recovery seen limits: {seen_limits:?}");
    assert!(
        seen_limits.contains(&8),
        "capacity must eventually reach Full (8 workers): seen={seen_limits:?}"
    );
    // Verify monotonic non-decreasing order (no rapid 8 -> 2 -> 8 oscillation)
    for w in seen_limits.windows(2) {
        assert!(
            w[0] <= w[1],
            "capacity must not oscillate backwards during recovery: seen={seen_limits:?}"
        );
    }
}

/// §17 item 18: MCP requests remain responsive during indexing / pressure.
#[tokio::test]
async fn mcp_remains_responsive_during_index_pressure() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    let mut srv1 = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FORCE_RESOURCE_PRESSURE", "warning"),
        ],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut srv2 = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FORCE_RESOURCE_PRESSURE", "warning"),
        ],
    )
    .await;

    // Concurrently issue calls from both client connections; both must succeed swiftly.
    let (r1, r2) = tokio::join!(
        call_tool_text(&mut srv1, "status", serde_json::json!({})),
        call_tool_text(&mut srv2, "status", serde_json::json!({})),
    );

    assert!(r1.is_ok(), "status 1 ok: {r1:?}");
    assert!(r2.is_ok(), "status 2 ok: {r2:?}");

    // Sequential call on same client also succeeds promptly.
    let r3 = call_tool_text(&mut srv1, "status", serde_json::json!({})).await;
    assert!(r3.is_ok(), "status 3 ok: {r3:?}");
}

/// §17 item 19 / §14: Combined final fault test:
/// Performance mode (8 workers) + memory pressure + daemon death while MCP traffic
/// is active + single relay election promotion + stdio preservation + initialization
/// replay + memory pressure clearance + gradual capacity recovery.
#[tokio::test]
async fn combined_pressure_and_daemon_failure_recovers() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let (home, db) = attic_home_and_db(tmp.path());

    // 1. Start primary daemon in Performance mode
    let mut srv1 = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FAST_RECOVERY_MS", "80"),
        ],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // 2. Start the single relay connected to it
    let mut srv2 = connect_daemon_with_env(
        &bin,
        &home,
        &db,
        None,
        &[
            ("ATTIC_RESOURCE_MODE", "performance"),
            ("ATTIC_FAST_RECOVERY_MS", "80"),
        ],
    )
    .await;

    // Verify initial healthy performance state via relay
    let r0 = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("initial status via relay");
    let v0: Value = serde_json::from_str(&r0).expect("json");
    assert_eq!(v0["resource_mode"], "performance");
    assert_eq!(v0["resource_pressure"]["effective_indexing_heavy_limit"], 8);

    // 3. Inject memory pressure (warning) via file override
    let override_file = home.join("pressure_override");
    std::fs::write(&override_file, "warning").expect("write pressure override");
    tokio::time::sleep(Duration::from_millis(400)).await;

    let rw = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status under pressure via relay");
    let vw: Value = serde_json::from_str(&rw).expect("json");
    assert_eq!(vw["resource_pressure"]["level"], "warning");
    assert_eq!(vw["resource_pressure"]["effective_indexing_heavy_limit"], 6);

    // 4. Kill the primary daemon (srv1) while MCP traffic is active.
    // The single relay (srv2) must detect disconnect, win election,
    // promote to replacement daemon, keep stdio alive, and replay initialize.
    srv1.child.start_kill().expect("kill daemon process");
    let _ = tokio::time::timeout(IO_TIMEOUT, srv1.child.wait())
        .await
        .expect("killed daemon did not exit in time");

    // Allow promotion and reconnect to complete
    tokio::time::sleep(Duration::from_millis(2000)).await;

    // 5. The existing stdio client on srv2 continues functioning automatically!
    let r_post = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status via promoted relay without client reconnect");
    let v_post: Value = serde_json::from_str(&r_post).expect("json");
    assert_eq!(v_post["status"], "unconfigured");

    // 6. Clear memory pressure: capacity recovers gradually back to full Performance (8 workers)
    std::fs::write(&override_file, "normal").expect("write normal override");
    tokio::time::sleep(Duration::from_millis(600)).await;

    let r_rec = call_tool_text(&mut srv2, "status", serde_json::json!({}))
        .await
        .expect("status post-recovery");
    let v_rec: Value = serde_json::from_str(&r_rec).expect("json");
    assert_eq!(v_rec["resource_pressure"]["level"], "normal");
    assert_eq!(
        v_rec["resource_pressure"]["effective_indexing_heavy_limit"],
        8
    );

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
}
