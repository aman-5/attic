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
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env("ATTIC_HOME", home)
        .env("ATTIC_DB_PATH", db)
        .env("ATTIC_SEMANTIC", "0")
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .env_remove("ATTIC_NO_DAEMON")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(ms) = idle_timeout_ms {
        cmd.env("ATTIC_DAEMON_IDLE_TIMEOUT_MS", ms.to_string());
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
