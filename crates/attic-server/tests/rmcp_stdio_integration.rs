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

/// Phase 6 cross-repo MCP E2E gate using the official rmcp client.
///
/// This test exercises the complete Phase 6 → Phase 4 evidence path through
/// the real MCP server binary:
///
/// 1. Creates a multi-repo fixture (provider + consumer + unrelated)
/// 2. Spawns the Attic server with ATTIC_WORKSPACE_ROOT → triggers
///    sync_workspace → creates WorkspaceSnapshot
/// 3. Calls the `context` tool with a cross-repo query
/// 4. Verifies:
///    - Provider repository is identified in the response
///    - Unrelated repository is NOT falsely claimed
///    - Evidence carries workspace_snapshot_id (provenance chain)
///    - Evidence carries source_revision_id (provenance chain)
///    - Result/plan_id is present (evidence path ran)
///    - Confidence is preserved
///
/// This is the REQUIRED product gate for Phase 6 cross-repo intelligence.
#[tokio::test]
async fn rmcp_client_crossrepo_multi_repo_fixture() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = tmp.path().join("rmcp_crossrepo.db");

    // ── Build three-repo fixture ──────────────────────────────────────────────
    // provider: declares module "example.com/rmcp/provider"
    let provider_dir = tmp.path().join("provider");
    std::fs::create_dir_all(&provider_dir).unwrap();
    std::fs::write(
        provider_dir.join("go.mod"),
        "module example.com/rmcp/provider\n\ngo 1.21\n",
    )
    .unwrap();
    std::fs::write(
        provider_dir.join("lib.go"),
        "package provider\n\nfunc Hello() string { return \"hello\" }\n",
    )
    .unwrap();

    // dependent: requires "example.com/rmcp/provider"
    let dependent_dir = tmp.path().join("dependent");
    std::fs::create_dir_all(&dependent_dir).unwrap();
    std::fs::write(
        dependent_dir.join("go.mod"),
        "module example.com/rmcp/dependent\n\ngo 1.21\n\nrequire example.com/rmcp/provider v0.1.0\n",
    )
    .unwrap();
    std::fs::write(
        dependent_dir.join("main.go"),
        "package main\n\nimport \"example.com/rmcp/provider\"\n\nfunc main() { _ = provider.Hello() }\n",
    )
    .unwrap();

    // unrelated repo: no dependency on provider
    let unrelated_dir = tmp.path().join("unrelated");
    std::fs::create_dir_all(&unrelated_dir).unwrap();
    std::fs::write(
        unrelated_dir.join("go.mod"),
        "module example.com/rmcp/unrelated\n\ngo 1.21\n",
    )
    .unwrap();
    std::fs::write(
        unrelated_dir.join("util.go"),
        "package unrelated\n\nfunc Util() {}\n",
    )
    .unwrap();

    // ── Pre-seed all three repos into the shared DB ───────────────────────────
    // We connect once per repo directory (each as its own ATTIC_WORKSPACE_ROOT)
    // so all three repos are indexed before we run sync_workspace.
    // We wait for each workspace to appear in the search index before closing,
    // to avoid a race between the startup indexing task and server shutdown.
    for (dir, token) in [
        (&provider_dir, "Hello"),
        (&dependent_dir, "provider"),
        (&unrelated_dir, "Util"),
    ] {
        let mut seed = connect(&bin, &db, Some(dir)).await;
        // Wait up to 5 s for the file to be indexed in this repo.
        for _ in 0..10 {
            let txt = call_tool_text(&mut seed, "search", serde_json::json!({ "query": token }))
                .await
                .unwrap_or_default();
            let sv: Value = serde_json::from_str(&txt).unwrap_or(Value::Null);
            if sv["results"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let _ = tokio::time::timeout(IO_TIMEOUT, seed.service.close()).await;
        // seed is dropped here, killing the child.
    }

    // ── Spawn server WITH ATTIC_WORKSPACE_ROOT → sync_workspace runs ──────────
    // All three repos are now in the DB; pointing ATTIC_WORKSPACE_ROOT at
    // provider_dir causes the server to run sync_workspace over all DB repos.
    let mut srv = connect(&bin, &db, Some(&provider_dir)).await;

    // Poll status until the watcher is running (cross-repo sync completed).
    // Accept either native-watcher or periodic-reconciliation as "ready".
    // Bounded poll: 10s total, 500ms intervals.
    let mut watcher_ready = false;
    for _ in 0..20 {
        let status_text = call_tool_text(&mut srv, "status", serde_json::json!({}))
            .await
            .expect("status tool");
        let v: Value = serde_json::from_str(&status_text).expect("status payload is JSON");
        // Multi-root `status` reports one watcher per repository under
        // `workspace.repositories[]`, not a single top-level `watcher`.
        let any_watcher_ready = v["workspace"]["repositories"]
            .as_array()
            .map(|repos| {
                repos.iter().any(|r| {
                    let mode = r["watcher"]["mode"].as_str().unwrap_or("");
                    mode == "native-watcher" || mode == "periodic-reconciliation"
                })
            })
            .unwrap_or(false);
        if any_watcher_ready {
            watcher_ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        watcher_ready,
        "cross-repo sync did not complete within 10s; watcher never started"
    );

    // ── Call context tool with cross-repo query ──────────────────────────────
    // Poll until at least one evidence item carries workspace_snapshot_id
    // (proves sync_workspace completed cross-repo edge resolution).  The watcher
    // being "ready" only means the watcher task started; sync_workspace may still
    // be computing cross-repo edges and creating the WorkspaceSnapshot.
    // Bounded: 20 attempts × 500 ms = 10 s.
    let mut response_text = String::new();
    let mut v = Value::Null;
    for _ in 0..20 {
        response_text = call_tool_text(
            &mut srv,
            "context",
            serde_json::json!({
                "query": "What Go modules does the dependent repository depend on?",
                "mode": "NORMAL"
            }),
        )
        .await
        .expect("context tool");
        v = serde_json::from_str(&response_text).expect("context payload is JSON");
        // Check if any evidence has workspace_snapshot_id set (cross-repo ready).
        let has_snapshot = v["evidence"].as_array().unwrap_or(&vec![]).iter().any(|e| {
            e["workspace_snapshot_id"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        });
        if has_snapshot {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Extract key fields from response.
    let context_body = v["context"].as_str().unwrap_or("");
    let claims_json = v["claims"].to_string();
    let evidence_json = v["evidence"].to_string();
    let full_response = format!("{context_body} {claims_json} {evidence_json} {response_text}");

    // GATE 1: Provider repository IS identified in the cross-repo response.
    // Check full_response — provider must appear somewhere (context or claims).
    assert!(
        full_response.contains("example.com/rmcp/provider"),
        "GATE 1 FAIL: provider module must be identified in cross-repo response; response={:.500}",
        full_response
    );

    // GATE 2: Unrelated repository is NOT falsely claimed as a dependency.
    // Only check claims_json — the prose context_body legitimately includes all
    // indexed go.mod files as CONFIGURATION evidence (that is correct behaviour);
    // the structured claims must not list the unrelated module as a dependency.
    assert!(
        !claims_json.contains("example.com/rmcp/unrelated"),
        "GATE 2 FAIL: unrelated module must NOT appear as dependency claim; claims={:.500}",
        claims_json
    );

    // GATE 3: Confidence field is present and non-empty.
    let confidence = v["confidence"].as_str().unwrap_or("");
    assert!(
        !confidence.is_empty(),
        "GATE 3 FAIL: confidence must be present; got: {v}"
    );

    // GATE 4 (STRENGTHENED): Real provenance must be present.
    // - result verdict must be present (not empty)
    let result = v["result"].as_str().unwrap_or("");
    assert!(
        !result.is_empty(),
        "GATE 4 FAIL: result verdict must be present; got: {v}"
    );
    // - plan_id must be set (evidence path ran)
    let plan_id = v["plan_id"].as_str().unwrap_or("");
    assert!(
        !plan_id.is_empty(),
        "GATE 4 FAIL: plan_id must be set (evidence path ran); got: {v}"
    );

    // GATE 4b: WorkspaceSnapshot provenance — any evidence item carrying a
    // non-null `workspace_snapshot_id` is definitionally a cross-repo evidence
    // item (only CrossRepoGenerator sets this field).  We assert:
    //   a) At least one such item exists (proves sync_workspace ran and produced
    //      a real WorkspaceSnapshot-backed edge).
    //   b) Every snapshot-backed item also carries `source_revision_id`
    //      (proves the full provenance chain is intact).
    let empty_vec: Vec<Value> = vec![];
    let evidence_arr = v["evidence"].as_array().unwrap_or(&empty_vec);
    let snapshot_backed: Vec<_> = evidence_arr
        .iter()
        .filter(|e| {
            e["workspace_snapshot_id"]
                .as_str()
                .map(|s| !s.is_empty())
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !snapshot_backed.is_empty(),
        "GATE 4b FAIL: must have at least one evidence item with workspace_snapshot_id \
         (cross-repo provenance); evidence={:.500}",
        evidence_json
    );
    for ev in &snapshot_backed {
        let ws_snap_id = ev["workspace_snapshot_id"].as_str().unwrap_or("");
        assert!(
            !ws_snap_id.is_empty(),
            "GATE 4b FAIL: snapshot-backed evidence must carry workspace_snapshot_id; evidence={ev}"
        );
        let src_rev = ev["source_revision_id"].as_str().unwrap_or("");
        assert!(
            !src_rev.is_empty(),
            "GATE 4b FAIL: snapshot-backed evidence must carry source_revision_id; evidence={ev}"
        );
    }

    // GATE 4c: workspace_snapshot_id must be a valid UUID.
    let first_ws_snap = snapshot_backed[0]["workspace_snapshot_id"]
        .as_str()
        .unwrap();
    let uuid_re =
        regex::Regex::new(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
            .unwrap();
    assert!(
        uuid_re.is_match(first_ws_snap),
        "GATE 4c FAIL: workspace_snapshot_id must be valid UUID; got {first_ws_snap}"
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
        std::fs::write(dir.join(format!("{name}_probe.rs")), format!("pub fn {token}() {{}}\n"))
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
    assert_eq!(v["status"], "unconfigured", "pristine install: {status_text}");

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

    let inspect = call_tool_text(&mut srv, "workspace", serde_json::json!({ "action": "inspect" }))
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
            if v["results"].as_array().map(|a| !a.is_empty()).unwrap_or(false) {
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
    assert_eq!(v["workspace"]["configured_repository_count"], 3, "{status_text}");

    // Graceful shutdown, then restart from the SAME ATTIC_HOME.
    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
    drop(srv);

    let mut srv = connect_home(&bin, &home).await;
    let inspect = call_tool_text(&mut srv, "workspace", serde_json::json!({ "action": "inspect" }))
        .await
        .expect("inspect after restart");
    let v: Value = serde_json::from_str(&inspect).expect("inspect JSON");
    assert_eq!(v["membership_count"], 3, "membership must persist: {inspect}");

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
    assert!(!cfg.contains(&root_paths[1]), "removed root must leave config: {cfg}");

    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
}
