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
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env("ATTIC_DB_PATH", db)
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
    assert_eq!(v["status"], "ok", "{status_text}");

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
