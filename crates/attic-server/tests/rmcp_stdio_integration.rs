//! REQUIRED Phase 1D gate — genuine MCP integration over stdio using the
//! official `rmcp` client/service APIs against a spawned Attic server binary.
//!
//! Unlike the supplemental manual JSON-RPC tests in `main.rs`, this file
//! drives the server with the real rmcp client service stack
//! (`ServiceExt::serve` over the `(AsyncRead, AsyncWrite)` transport), so the
//! full official handshake, framing, and request/response correlation are
//! exercised end-to-end.
//!
//! Every await is bounded by a 10-second timeout; on timeout the child
//! process is killed immediately so a wedged server can never hang the test
//! suite.  If the server binary cannot be located this test FAILS — it never
//! silently passes.

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

/// Spawn the attic stdio server and connect an official rmcp client to it.
async fn connect(bin: &Path, db: &Path, workspace_root: Option<&Path>) -> ServerHandle {
    let attic_home = db
        .parent()
        .expect("test database must have a parent directory")
        .join("attic-home");

    std::fs::create_dir_all(&attic_home).expect("create isolated ATTIC_HOME");

    let mut cmd = tokio::process::Command::new(bin);

    cmd.env("ATTIC_HOME", &attic_home)
        .env("ATTIC_DB_PATH", db)
        .env("ATTIC_SEMANTIC", "0")
        // This suite drives the server directly over its own stdio pipes,
        // one process per `connect()` call. `ATTIC_NO_DAEMON=1` keeps every
        // test here exercising exactly the legacy single-process path,
        // unaffected by the daemon/relay election added alongside it (see
        // `crates/attic-server/src/daemon.rs` and
        // `tests/daemon_relay_integration.rs` for daemon-mode coverage).
        .env("ATTIC_NO_DAEMON", "1")
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    if let Some(ws) = workspace_root {
        cmd.env("ATTIC_WORKSPACE_ROOT", ws);
    }
    let mut child = cmd.spawn().expect("spawn attic server");
    let stdout = child.stdout.take().expect("server stdout piped");
    let stdin = child.stdin.take().expect("server stdin piped");

    // Official rmcp client service: `()` implements ClientHandler with a
    // default get_info(), and the (read, write) tuple is an IntoTransport.
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

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // Deterministic teardown even on assertion failure.
        let _ = self.child.start_kill();
        // [FIX] `start_kill()` only signals termination — it does not wait
        // for the OS to actually reclaim the process (and release its
        // `attic.lock`). Several tests reuse the same database path across
        // sequential `connect()` calls (e.g. pre-seeding multiple repos, or
        // simulating a restart); without this bounded wait, the next
        // `connect()` can race the still-exiting previous process and be
        // correctly refused by the single-instance lock, surfacing as a
        // spurious "connection closed" failure rather than a real bug.
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

/// Call a tool through the real rmcp client and return its first text block.
async fn call_tool_text(
    srv: &mut ServerHandle,
    tool: &str,
    arguments: Value,
) -> Result<String, String> {
    let mut params = CallToolRequestParams::new(tool.to_owned());
    params.arguments = arguments.as_object().cloned();
    let fut = srv.service.call_tool(params);
    let outcome: Result<String, String> = match tokio::time::timeout(IO_TIMEOUT, fut).await {
        Ok(Ok(result)) => match result.content.first() {
            Some(ContentBlock::Text(t)) => Ok(t.text.clone()),
            other => Err(format!(
                "expected text content from `{tool}`, got {other:?}"
            )),
        },
        Ok(Err(e)) => Err(format!("`{tool}` call failed: {e}")),
        // Timed out: kill the wedged server immediately.
        Err(_) => {
            let _ = srv.child.start_kill();
            return Err(format!(
                "`{tool}` call exceeded {IO_TIMEOUT:?} — server killed"
            ));
        }
    };
    outcome
}

#[tokio::test]
async fn rmcp_client_full_lifecycle_over_stdio() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = tmp.path().join("rmcp_gate.db");

    let mut srv = connect(&bin, &db, None).await;

    // 1. The negotiated peer info identifies the Attic server.
    let peer_info = tokio::time::timeout(IO_TIMEOUT, async {
        loop {
            if let Some(info) = srv.service.peer().peer_info() {
                return info;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("peer_info wait timed out");
    let impl_name = peer_info
        .server_info
        .as_ref()
        .map(|i| i.name.clone())
        .unwrap_or_default();
    assert!(impl_name.contains("attic"), "serverInfo.name = {impl_name}");

    // 2. tools/list via the high-level paginated helper.
    let tools = tokio::time::timeout(IO_TIMEOUT, srv.service.peer().list_all_tools())
        .await
        .expect("tools/list timed out")
        .expect("list_all_tools failed");
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    for expected in ["file", "search", "repo_map", "status"] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing tool `{expected}` in {names:?}"
        );
    }

    // 3. tools/call status returns valid JSON with status=ok.
    let status_text = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status tool");
    let v: Value = serde_json::from_str(&status_text).expect("status payload is JSON");
    // No workspace configured in this test: status must succeed but report
    // UNCONFIGURED rather than fabricate an authoritative empty workspace.
    assert_eq!(v["status"], "unconfigured", "{status_text}");

    // 4. Unknown tool yields error content through normal MCP results.
    let unknown = call_tool_text(&mut srv, "does_not_exist", serde_json::json!({}))
        .await
        .expect("unknown tool call should return error content as a result");
    assert!(
        unknown.contains("unknown tool"),
        "expected unknown-tool error text, got: {unknown}"
    );

    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close())
        .await
        .expect("graceful client shutdown timed out");
}

#[tokio::test]
async fn rmcp_client_workspace_index_search_and_file_e2e() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = tmp.path().join("rmcp_e2e.db");

    // A tiny workspace with one uniquely-tokenized source file.
    let ws = tmp.path().join("workspace");
    std::fs::create_dir_all(&ws).expect("create workspace");
    std::fs::write(
        ws.join("e2e_probe.rs"),
        "pub fn rmcp_e2e_unique_token() {}\n",
    )
    .expect("write probe file");

    // ATTIC_WORKSPACE_ROOT makes the server index the workspace on startup
    // through the coordinated writer queue.
    let mut srv = connect(&bin, &db, Some(&ws)).await;

    // Poll search until the startup indexing run commits and becomes visible
    // (bounded — never hangs).
    let mut search_payload = String::new();
    let mut found = None;
    for _ in 0..30 {
        search_payload = call_tool_text(
            &mut srv,
            "search",
            serde_json::json!({ "query": "rmcp_e2e_unique_token" }),
        )
        .await
        .expect("search tool");
        let v: Value = serde_json::from_str(&search_payload).expect("search payload is JSON");
        if let Some(results) = v["results"].as_array()
            && !results.is_empty()
        {
            found = Some(results[0].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let hit = found.unwrap_or_else(|| {
        panic!("indexed token never became searchable; last payload: {search_payload}")
    });

    // The hit belongs to our repository and points at the indexed path.
    assert!(
        hit["path"]
            .as_str()
            .unwrap_or("")
            .replace('\\', "/")
            .ends_with("e2e_probe.rs"),
        "unexpected hit path: {hit}"
    );
    let repo_id = hit["repository_id"]
        .as_str()
        .expect("repository_id in hit")
        .to_owned();

    // Retrieve live file content through the `file` tool over real stdio.
    let file_text = call_tool_text(
        &mut srv,
        "file",
        serde_json::json!({ "repository_id": repo_id, "path": "e2e_probe.rs" }),
    )
    .await
    .expect("file tool");
    assert!(
        file_text.contains("rmcp_e2e_unique_token"),
        "file tool response must contain live content: {file_text}"
    );

    // Bounded region retrieval over the same channel.
    let region_text = call_tool_text(
        &mut srv,
        "file",
        serde_json::json!({
            "repository_id": repo_id,
            "path": "e2e_probe.rs",
            "start_line": 1,
            "end_line": 1
        }),
    )
    .await
    .expect("file region tool");
    assert!(
        region_text.contains("rmcp_e2e_unique_token"),
        "{region_text}"
    );

    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close())
        .await
        .expect("graceful shutdown timed out");
}

/// REQUIRED first-run workspace-lifecycle gate (spec §8/§10/§11/§36):
///
/// A pristine Attic install with an empty ATTIC_HOME must start over stdio,
/// report UNCONFIGURED, refuse retrieval until configured, then be configured
/// ENTIRELY at runtime through the `workspace` MCP tool with three arbitrary
/// UNRELATED roots (no common parent). The membership must be persisted to
/// <ATTIC_HOME>/config.toml, survive a full process restart, and support
/// runtime removal.
#[tokio::test]
async fn rmcp_first_run_unconfigured_then_workspace_tool_configure_and_restart() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("attic-home");

    // Spawn with ONLY an explicit ATTIC_HOME: no ATTIC_DB_PATH, no
    // ATTIC_CONFIG, no ATTIC_WORKSPACE_ROOT.
    fn spawn(bin: &Path, home: &Path) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.env("ATTIC_HOME", home)
            .env("ATTIC_SEMANTIC", "0")
            // See the comment in `connect()` above: this gate is stdio-only.
            .env("ATTIC_NO_DAEMON", "1")
            .env_remove("ATTIC_DB_PATH")
            .env_remove("ATTIC_CONFIG")
            .env_remove("ATTIC_WORKSPACE_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        cmd
    }
    async fn connect_home(bin: &Path, home: &Path) -> ServerHandle {
        let mut child = spawn(bin, home).spawn().expect("spawn attic server");
        let stdout = child.stdout.take().expect("stdout piped");
        let stdin = child.stdin.take().expect("stdin piped");
        let service = match tokio::time::timeout(IO_TIMEOUT, ().serve((stdout, stdin))).await {
            Ok(Ok(s)) => s,
            other => panic!("rmcp handshake failed: {other:?}"),
        };
        ServerHandle { child, service }
    }

    // Three arbitrary, deliberately UNRELATED roots with no common parent.
    let roots = ["repoalpha", "repobeta", "repogamma"];
    let mut tokens = Vec::new();
    for (i, name) in roots.iter().enumerate() {
        let dir = tmp.path().join(format!("root{i}_{name}"));
        std::fs::create_dir_all(&dir).expect("create root");
        let token = format!("first_run_token_{name}");
        std::fs::write(
            dir.join(format!("{name}_probe.rs")),
            format!("pub fn {token}() {{}}\n"),
        )
        .expect("write probe");
        tokens.push(token);
    }
    // Server-side validation canonicalizes roots (on Windows this adds the
    // \?\ extended-length prefix), so the persisted config holds canonical
    // paths. Mirror that here for the on-disk assertions.
    let root_paths: Vec<String> = (0..3)
        .map(|i| {
            std::fs::canonicalize(tmp.path().join(format!("root{i}_{}", roots[i])))
                .expect("canonicalize root")
                .display()
                .to_string()
        })
        .collect();

    // ── First run: UNCONFIGURED ──
    let mut srv = connect_home(&bin, &home).await;

    let status_text = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status on pristine install");
    let v: Value = serde_json::from_str(&status_text).expect("status JSON");
    assert_eq!(
        v["status"], "unconfigured",
        "pristine install: {status_text}"
    );

    let search_err = call_tool_text(
        &mut srv,
        "search",
        serde_json::json!({ "query": "anything" }),
    )
    .await
    .unwrap_or_else(|e| e);
    assert!(
        search_err.contains("workspace not configured"),
        "search must refuse while UNCONFIGURED, got {search_err:?}"
    );

    // ── Configure entirely at runtime through the `workspace` tool ──
    for p in &root_paths {
        let resp = call_tool_text(
            &mut srv,
            "workspace",
            serde_json::json!({ "action": "add", "path": p }),
        )
        .await
        .expect("workspace add");
        assert!(resp.contains("config.toml"), "add response: {resp}");
    }

    let inspect = call_tool_text(
        &mut srv,
        "workspace",
        serde_json::json!({ "action": "inspect" }),
    )
    .await
    .expect("workspace inspect");
    let v: Value = serde_json::from_str(&inspect).expect("inspect JSON");
    assert_eq!(v["membership_count"], 3, "{inspect}");
    assert_eq!(v["configured"], true, "{inspect}");

    // Membership was durably persisted to <ATTIC_HOME>/config.toml.
    let cfg = std::fs::read_to_string(home.join("config.toml")).expect("persistent config");
    for p in &root_paths {
        assert!(cfg.contains(p), "config.toml must contain {p}: {cfg}");
    }

    // Each root is independently indexed and searchable through the SAME
    // process/DB (bootstrap is synchronous inside the workspace call).
    for token in &tokens {
        let mut found = false;
        for _ in 0..30 {
            let text = call_tool_text(&mut srv, "search", serde_json::json!({ "query": token }))
                .await
                .unwrap_or_else(|e| e);
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => panic!("search returned non-JSON: {text}"),
            };
            if v["results"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                found = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert!(found, "token {token} never became searchable");
    }

    let status_text = call_tool_text(&mut srv, "status", serde_json::json!({}))
        .await
        .expect("status after configure");
    let v: Value = serde_json::from_str(&status_text).unwrap();
    assert_eq!(
        v["workspace"]["configured_repository_count"], 3,
        "{status_text}"
    );

    // Graceful shutdown, then restart from the SAME ATTIC_HOME.
    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
    drop(srv);

    let mut srv = connect_home(&bin, &home).await;
    let inspect = call_tool_text(
        &mut srv,
        "workspace",
        serde_json::json!({ "action": "inspect" }),
    )
    .await
    .expect("inspect after restart");
    let v: Value = serde_json::from_str(&inspect).expect("inspect JSON");
    assert_eq!(
        v["membership_count"], 3,
        "membership must persist: {inspect}"
    );

    // Runtime removal through MCP, verified both live and on disk.
    let removed = call_tool_text(
        &mut srv,
        "workspace",
        serde_json::json!({ "action": "remove", "path": root_paths[1] }),
    )
    .await
    .expect("workspace remove");
    let v: Value = serde_json::from_str(&removed).unwrap();
    assert_eq!(v["membership_count"], 2, "{removed}");

    let cfg = std::fs::read_to_string(home.join("config.toml")).expect("config after remove");
    assert!(
        !cfg.contains(&root_paths[1]),
        "removed root must leave config: {cfg}"
    );

    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
}
