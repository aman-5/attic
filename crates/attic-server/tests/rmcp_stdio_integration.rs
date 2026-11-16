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
        // Each `connect()` owns a private database, so this process always
        // wins the election and serves its own stdio. A zero idle timeout
        // makes it exit as soon as the client disconnects instead of
        // lingering for relays that will never come.
        .env("ATTIC_DAEMON_IDLE_TIMEOUT_MS", "0")
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

#[tokio::test]
async fn egm_is_available_by_default_over_official_client() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_attic"));
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("catalog.db");
    let mut srv = connect(&bin, &db, None).await;
    let tools = tokio::time::timeout(IO_TIMEOUT, srv.service.peer().list_all_tools())
        .await
        .unwrap()
        .unwrap();
    assert!(tools.iter().any(|tool| tool.name == "spec"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let response = call_tool_text(&mut srv, "egm", serde_json::json!({}))
            .await
            .unwrap();
        let status: Value = serde_json::from_str(&response).unwrap();
        if status["status"] != "initializing" {
            assert_eq!(status["enabled"], true, "{status}");
            assert_eq!(status["mode"], "manual");
            assert_eq!(status["enforce"], false);
            assert_eq!(status["roots"], serde_json::json!([]));
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{status}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn egm_spec_off_and_full_enabled_flow_over_official_client() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_attic"));
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("egm-tests");
    std::fs::create_dir_all(&fixtures).unwrap();
    let dir = tempfile::tempdir_in(fixtures).unwrap();
    let disabled_db = dir.path().join("disabled").join("catalog.db");
    std::fs::create_dir_all(disabled_db.parent().unwrap()).unwrap();
    std::fs::write(
        disabled_db.parent().unwrap().join("attic.toml"),
        "[planning]\nenabled = false\n",
    )
    .unwrap();
    {
        let mut srv = connect(&bin, &disabled_db, None).await;
        let tools = tokio::time::timeout(IO_TIMEOUT, srv.service.peer().list_all_tools())
            .await
            .unwrap()
            .unwrap();
        assert!(!tools.iter().any(|t| t.name == "spec"));
        assert!(tools.iter().any(|t| t.name == "egm"));
        let activation = call_tool_text(&mut srv, "egm", serde_json::json!({}))
            .await
            .unwrap();
        let activation: Value = serde_json::from_str(&activation).unwrap();
        assert_eq!(activation["enabled"], false);
        assert!(
            activation["activation"]["enable"]
                .as_str()
                .unwrap()
                .contains("attic egm enable")
        );
        let guessed = call_tool_text(&mut srv, "spec", serde_json::json!({"action":"status"}))
            .await
            .unwrap();
        assert!(guessed.contains("ERR_PLANNING_DISABLED"), "{guessed}");
        assert!(!disabled_db.parent().unwrap().join("spec_state.db").exists());
    }
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let root = root.canonicalize().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(root.join("src").join("lib.rs"), "pub fn original() {}\n").unwrap();
    std::fs::create_dir_all(root.join("docs").join("designs")).unwrap();
    std::fs::write(
        root.join("docs").join("designs").join("one.md"),
        "# One\n```attic-citations\nTARGET_MODIFY repo src/lib.rs\n```\n",
    )
    .unwrap();
    let db = dir.path().join("enabled").join("catalog.db");
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let activation = std::process::Command::new(&bin)
        .args(["egm", "enable", "--root"])
        .arg(&root)
        .args(["--mode", "autopilot"])
        .env("ATTIC_HOME", dir.path().join("attic-home"))
        .env("ATTIC_DB_PATH", &db)
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .output()
        .unwrap();
    assert!(
        activation.status.success(),
        "{}",
        String::from_utf8_lossy(&activation.stderr)
    );
    assert!(!db.exists());
    assert!(!db.parent().unwrap().join("spec_state.db").exists());
    let mut srv = connect(&bin, &db, Some(&root)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let ready = loop {
        let text = call_tool_text(&mut srv, "status", serde_json::json!({}))
            .await
            .unwrap();
        let status: Value = serde_json::from_str(&text).unwrap();
        if status["workspace"]["current_repository_count"].as_u64() == Some(1) {
            break text;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fixture indexing did not finish: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let tools = tokio::time::timeout(IO_TIMEOUT, srv.service.peer().list_all_tools())
        .await
        .unwrap()
        .unwrap();
    assert!(tools.iter().any(|t| t.name == "spec"));
    let activation = call_tool_text(&mut srv, "egm", serde_json::json!({"action":"status"}))
        .await
        .unwrap();
    let activation: Value = serde_json::from_str(&activation).unwrap();
    assert_eq!(activation["enabled"], true);
    assert_eq!(activation["mode"], "autopilot");
    let guidance = call_tool_text(&mut srv, "egm", serde_json::json!({"action":"guidance"}))
        .await
        .unwrap();
    let guidance: Value = serde_json::from_str(&guidance).unwrap();
    assert!(
        guidance["workflow"]["advance"]
            .as_str()
            .unwrap()
            .contains("automatically")
    );
    let call = |action: &str| serde_json::json!({"action":action,"spec_id":"one","root":root});
    let registered = call_tool_text(&mut srv, "spec", call("register"))
        .await
        .unwrap();
    let registered: Value = serde_json::from_str(&registered).unwrap();
    assert_eq!(
        registered["status"], "UNDER_REVIEW",
        "{registered}; ready={ready}"
    );
    let verified = call_tool_text(&mut srv, "spec", call("verify"))
        .await
        .unwrap();
    let verified: Value = serde_json::from_str(&verified).unwrap();
    assert_eq!(verified["status"], "VERIFIED", "{verified}");
    let mut pack = call("pack");
    pack["persona"] = "zoom_out".into();
    pack["delivery"] = "inline".into();
    let packed = call_tool_text(&mut srv, "spec", pack).await.unwrap();
    let packed: Value = serde_json::from_str(&packed).unwrap();
    assert!(
        packed["data"]["markdown"]
            .as_str()
            .unwrap()
            .contains("Existing repo_map summary")
    );
    let mut pack_paths = Vec::new();
    for persona in ["zoom_out", "codebase_details", "implementation_readiness"] {
        let mut pack = call("pack");
        pack["persona"] = persona.into();
        pack["delivery"] = "file".into();
        let packed = call_tool_text(&mut srv, "spec", pack).await.unwrap();
        let packed: Value = serde_json::from_str(&packed).unwrap();
        let handle = &packed["data"];
        let path = PathBuf::from(handle["path"].as_str().expect("file-delivered pack handle"));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(handle["bytes"].as_u64(), Some(bytes.len() as u64));
        assert_eq!(handle["sha256"], attic_spec::sha256(&bytes));
        assert_eq!(handle["persona"], persona);
        assert!(!path.starts_with(&root));
        assert!(handle.get("markdown").is_none());
        pack_paths.push(path);
    }
    let mut unknown = call("verify");
    unknown["extra"] = true.into();
    assert!(
        call_tool_text(&mut srv, "spec", unknown)
            .await
            .unwrap()
            .contains("ERR_INVALID_ARGUMENT")
    );
    assert!(
        call_tool_text(&mut srv, "spec", call("approve"))
            .await
            .unwrap()
            .contains("ERR_INVALID_ARGUMENT")
    );
    let mut outside = call("register");
    outside["root"] = serde_json::json!(dir.path());
    assert!(
        call_tool_text(&mut srv, "spec", outside)
            .await
            .unwrap()
            .contains("ERR_ROOT_NOT_ACTIVE")
    );
    struct HumanFixture;
    impl attic_spec::service::ApprovalPrompt for HumanFixture {
        fn is_interactive(&self) -> bool {
            true
        }
        fn confirm(
            &mut self,
            _: &attic_spec::state::Document,
            _: &attic_spec::verifier::Run,
            _: &str,
        ) -> attic_spec::Result<bool> {
            Ok(true)
        }
    }
    let service = attic_spec::service::SpecService::open(
        &db,
        attic_core::config::PlanningConfig {
            enabled: true,
            ..Default::default()
        },
    )
    .unwrap()
    .unwrap();
    let doc = service.resolve("one", None).unwrap();
    service.approve(&doc, &mut HumanFixture).unwrap();
    let begun = call_tool_text(&mut srv, "spec", call("begin"))
        .await
        .unwrap();
    let begun: Value = serde_json::from_str(&begun).unwrap();
    assert_eq!(begun["status"], "IMPLEMENTING", "{begun}");
    let document = &begun["data"]["document"];
    let mut heartbeat = call("heartbeat");
    heartbeat["lease_id"] = document["lease_id"].clone();
    heartbeat["fencing_token"] = document["fencing_token"].clone();
    let response = call_tool_text(&mut srv, "spec", heartbeat.clone())
        .await
        .unwrap();
    assert!(response.contains("IMPLEMENTING"), "{response}");
    heartbeat["action"] = "wave_complete".into();
    heartbeat["wave_number"] = 1.into();
    heartbeat["repository"] = "repo".into();
    let response = call_tool_text(&mut srv, "spec", heartbeat.clone())
        .await
        .unwrap();
    assert!(response.contains("COMPLETED"), "{response}");
    heartbeat["action"] = "drift".into();
    heartbeat.as_object_mut().unwrap().remove("wave_number");
    heartbeat.as_object_mut().unwrap().remove("repository");
    let response = call_tool_text(&mut srv, "spec", heartbeat).await.unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["status"], "IMPLEMENTED", "{response}");
    let mut reopen = call("reopen");
    reopen["reason"] = "fixture new scope".into();
    let response = call_tool_text(&mut srv, "spec", reopen).await.unwrap();
    assert!(response.contains("UNDER_REVIEW"), "{response}");
    assert!(pack_paths.iter().all(|path| !path.exists()));
    let before_disable = service.store.documents().unwrap();
    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close())
        .await
        .unwrap();
    drop(srv);
    let disabled = std::process::Command::new(&bin)
        .args(["egm", "disable"])
        .env("ATTIC_HOME", dir.path().join("attic-home"))
        .env("ATTIC_DB_PATH", &db)
        .output()
        .unwrap();
    assert!(disabled.status.success());
    let mut srv = connect(&bin, &db, Some(&root)).await;
    let tools = tokio::time::timeout(IO_TIMEOUT, srv.service.peer().list_all_tools())
        .await
        .unwrap()
        .unwrap();
    assert!(!tools.iter().any(|t| t.name == "spec"));
    let activation = call_tool_text(&mut srv, "egm", serde_json::json!({}))
        .await
        .unwrap();
    let activation: Value = serde_json::from_str(&activation).unwrap();
    assert_eq!(activation["enabled"], false);
    assert_eq!(service.store.documents().unwrap(), before_disable);
    let denied = call_tool_text(&mut srv, "spec", call("status"))
        .await
        .unwrap();
    assert!(denied.contains("ERR_PLANNING_DISABLED"));
}

/// Call a tool through the real rmcp client and return its first text block.
async fn call_tool_text(
    srv: &mut ServerHandle,
    tool: &str,
    arguments: Value,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + IO_TIMEOUT;
    loop {
        let mut params = CallToolRequestParams::new(tool.to_owned());
        params.arguments = arguments.as_object().cloned();
        let fut = srv.service.call_tool(params);
        let outcome: Result<String, String> = match tokio::time::timeout_at(deadline, fut).await {
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
        if let Ok(text) = &outcome
            && serde_json::from_str::<Value>(text)
                .ok()
                .is_some_and(|state| state["status"] == "initializing")
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(25)).await;
            continue;
        }
        return outcome;
    }
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
    for expected in ["file", "repo_map", "status", "context"] {
        assert!(
            names.iter().any(|n| n == expected),
            "missing tool `{expected}` in {names:?}"
        );
    }
    // One-door merge: standalone `search` is gone; raw hybrid retrieval is
    // `context` with mode="SEARCH".
    assert!(
        !names.iter().any(|n| n == "search"),
        "search tool must not be listed after the one-door merge: {names:?}"
    );

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
            "context",
            serde_json::json!({ "query": "rmcp_e2e_unique_token", "mode": "SEARCH" }),
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

#[tokio::test]
async fn rmcp_structured_workflow_search_expand_and_references() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let primary = tmp.path().join("primary");
    let secondary = tmp.path().join("secondary");
    for root in [&primary, &secondary] {
        std::fs::create_dir_all(root.join("flows")).unwrap();
    }
    std::fs::write(
        primary.join("flows").join("workflow.json"),
        r#"{
  "onVerify": {
    "handler": "otp.json",
    "condition": "checkHdbFlow and existingCustomer",
    "failure": {"errorCode": "VERIFY_FAILURE", "effect": "showError"}
  },
  "marker": "primaryWorkflowProbe"
}"#,
    )
    .unwrap();
    std::fs::write(
        secondary.join("flows").join("workflow.json"),
        r#"{"onVerify":{"condition":"checkHdbFlow","errorCode":"VERIFY_FAILURE"},"marker":"secondaryWorkflowProbe"}"#,
    )
    .unwrap();
    std::fs::write(
        primary.join("other.json"),
        r#"{"condition":"checkHdbFlow","errorCode":"VERIFY_FAILURE"}"#,
    )
    .unwrap();

    let mut srv = connect(&bin, &tmp.path().join("workflow.db"), Some(&primary)).await;
    let added = call_tool_text(
        &mut srv,
        "workspace",
        serde_json::json!({"action":"add", "path":secondary}),
    )
    .await
    .expect("add second repository");
    assert!(added.contains("config.toml"), "{added}");

    let mut repo_ids = Vec::new();
    for token in ["primaryWorkflowProbe", "secondaryWorkflowProbe"] {
        let mut found = None;
        for _ in 0..30 {
            let text = call_tool_text(
                &mut srv,
                "context",
                serde_json::json!({"query":token, "mode":"SEARCH"}),
            )
            .await
            .expect("wait for indexed fixture");
            let value: Value = serde_json::from_str(&text).expect("search JSON");
            if let Some(hit) = value["results"].as_array().and_then(|items| items.first())
                && (token == "primaryWorkflowProbe" || value["crossrepo_ready"] == true)
            {
                found = hit["repository_id"].as_str().map(str::to_owned);
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        repo_ids.push(found.unwrap_or_else(|| panic!("{token} never became searchable")));
    }
    assert_ne!(repo_ids[0], repo_ids[1]);
    let repo = &repo_ids[0];
    let path = "flows/workflow.json";
    let mut measurements = Vec::new();

    async fn measured(
        srv: &mut ServerHandle,
        measurements: &mut Vec<Value>,
        tool: &str,
        arguments: Value,
    ) -> String {
        let started = std::time::Instant::now();
        let text = call_tool_text(srv, tool, arguments)
            .await
            .expect("workflow call");
        let decoded = serde_json::from_str::<Value>(&text).ok();
        let server_ms = decoded
            .as_ref()
            .and_then(|value| value["telemetry"]["server_processing_ms"].as_f64());
        if tool == "context" {
            let value = decoded.as_ref().expect("context response JSON");
            assert_eq!(value["telemetry"]["response_bytes"], text.len());
            assert!(server_ms.is_some(), "missing server timing: {text}");
        }
        measurements.push(serde_json::json!({
            "tool": tool,
            "response_bytes": text.len(),
            "client_wall_ms": started.elapsed().as_secs_f64() * 1000.0,
            "server_processing_ms": server_ms,
        }));
        text
    }

    let search = measured(
        &mut srv,
        &mut measurements,
        "context",
        serde_json::json!({
            "query":"checkHdbFlow NeverIndexedSearchWord",
            "mode":"SEARCH", "repository_id":repo, "path":path
        }),
    )
    .await;
    let search: Value = serde_json::from_str(&search).unwrap();
    let hits = search["results"].as_array().unwrap();
    assert!(!hits.is_empty(), "ranked OR must find known term: {search}");
    assert!(
        hits.iter()
            .all(|hit| hit["repository_id"] == *repo && hit["path"] == path),
        "{search}"
    );
    let unit = hits[0]["retrieval_unit_id"]
        .as_str()
        .expect("indexed JSON unit");
    assert!(hits[0]["json_pointer"].is_string(), "{search}");

    let strict = measured(
        &mut srv,
        &mut measurements,
        "context",
        serde_json::json!({
            "query":"checkHdbFlow NeverIndexedSearchWord",
            "mode":"SEARCH", "match_mode":"all", "repository_id":repo, "path":path
        }),
    )
    .await;
    let strict: Value = serde_json::from_str(&strict).unwrap();
    assert!(strict["results"].as_array().unwrap().is_empty(), "{strict}");
    assert!(!strict["search_diagnostics"].is_null(), "{strict}");

    let scoped = measured(
        &mut srv,
        &mut measurements,
        "context",
        serde_json::json!({
            "query":"VERIFY_FAILURE", "mode":"SEARCH",
            "repository_id":repo, "path_prefix":"flows"
        }),
    )
    .await;
    let scoped: Value = serde_json::from_str(&scoped).unwrap();
    let scoped_hits = scoped["results"].as_array().unwrap();
    assert!(!scoped_hits.is_empty(), "{scoped}");
    assert!(
        scoped_hits
            .iter()
            .all(|hit| hit["repository_id"] == *repo && hit["path"] == path),
        "{scoped}"
    );

    let expanded = measured(
        &mut srv,
        &mut measurements,
        "file",
        serde_json::json!({"repository_id":repo, "retrieval_unit_id":unit}),
    )
    .await;
    assert!(expanded.contains("checkHdbFlow"), "{expanded}");
    assert!(
        expanded.contains("indexed selection may have changed"),
        "{expanded}"
    );

    let failure = measured(
        &mut srv,
        &mut measurements,
        "file",
        serde_json::json!({"repository_id":repo, "path":path, "json_pointer":"/onVerify/failure"}),
    )
    .await;
    assert!(
        failure.contains("VERIFY_FAILURE") && failure.contains("showError"),
        "{failure}"
    );

    let answer = measured(
        &mut srv,
        &mut measurements,
        "context",
        serde_json::json!({
            "query":"What does checkHdbFlow do in workflow.json?",
            "mode":"NORMAL", "repository_id":repo, "path":path
        }),
    )
    .await;
    let answer: Value = serde_json::from_str(&answer).unwrap();
    assert!(
        !answer["evidence"].as_array().unwrap().is_empty(),
        "{answer}"
    );
    assert!(
        answer["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["repository_id"] == *repo && hit["path"] == path),
        "{answer}"
    );
    assert!(
        answer["repository_answers"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["scope"] != "workspace_relationships"),
        "{answer}"
    );

    let references = measured(
        &mut srv,
        &mut measurements,
        "context",
        serde_json::json!({
            "query":"VERIFY_FAILURE", "mode":"REFERENCES", "repository_id":repo, "path":path
        }),
    )
    .await;
    let references: Value = serde_json::from_str(&references).unwrap();
    assert_eq!(references["coverage"]["complete"], true, "{references}");
    assert!(
        references["matches"].as_array().unwrap().iter().any(|hit| {
            hit["json_pointer"] == "/onVerify/failure/errorCode"
                && hit["kind"] == "reference"
                && hit["repository_id"] == *repo
        }),
        "{references}"
    );

    let absent = measured(
        &mut srv,
        &mut measurements,
        "context",
        serde_json::json!({
            "query":"UnseenOtpError", "mode":"REFERENCES", "repository_id":repo, "path":path
        }),
    )
    .await;
    let absent: Value = serde_json::from_str(&absent).unwrap();
    assert_eq!(absent["coverage"]["absence_proven"], true, "{absent}");
    assert!(measurements.len() <= 8, "{measurements:?}");
    eprintln!(
        "synthetic workflow measurements: {}",
        serde_json::json!(measurements)
    );

    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
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
            // See the comment in `connect()` above.
            .env("ATTIC_DAEMON_IDLE_TIMEOUT_MS", "0")
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
        "context",
        serde_json::json!({ "query": "anything", "mode": "SEARCH" }),
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
    let cfg = std::fs::read_to_string(home.join("config").join("config.toml"))
        .expect("persistent config");
    for p in &root_paths {
        assert!(cfg.contains(p), "config.toml must contain {p}: {cfg}");
    }

    // Each root is independently indexed and searchable through the SAME
    // process/DB (bootstrap is synchronous inside the workspace call).
    for token in &tokens {
        let mut found = false;
        for _ in 0..30 {
            let text = call_tool_text(
                &mut srv,
                "context",
                serde_json::json!({ "query": token, "mode": "SEARCH" }),
            )
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

    let cfg = std::fs::read_to_string(home.join("config").join("config.toml"))
        .expect("config after remove");
    assert!(
        !cfg.contains(&root_paths[1]),
        "removed root must leave config: {cfg}"
    );

    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
}
