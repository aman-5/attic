//! §35 workspace lifecycle E2E tests — restart behaviour, identity stability,
//! and unavailable-member degraded reporting.
//!
//! These tests exercise the full binary lifecycle over the official rmcp
//! client transport, using an explicit `ATTIC_HOME` to isolate each test's
//! persistent state from the developer's real `~/.attic` and from each other.
//!
//! Every await is bounded by a 10-second timeout identical to the main
//! rmcp_stdio_integration.rs gate; wedged servers are killed immediately.

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

/// Locate the required `attic` binary; fail loudly when absent.
fn require_bin() -> PathBuf {
    let path = match std::env::var("CARGO_BIN_EXE_attic") {
        Ok(p) => PathBuf::from(p),
        Err(_) => panic!(
            "CARGO_BIN_EXE_attic is not set: the attic server binary must be \
             built for this REQUIRED integration gate"
        ),
    };
    assert!(
        path.exists(),
        "required attic binary missing at {}: build it first",
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
        let _ = self.child.start_kill();
    }
}

/// Spawn an `attic` server using ONLY `ATTIC_HOME`; strip all other
/// workspace/DB env vars so the server uses `<ATTIC_HOME>/config.toml` as its
/// persistent workspace and `<ATTIC_HOME>/attic.db` as its database.
async fn connect_home(bin: &Path, home: &Path) -> ServerHandle {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.env("ATTIC_HOME", home)
        .env_remove("ATTIC_DB_PATH")
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().expect("spawn attic server");
    let stdout = child.stdout.take().expect("server stdout piped");
    let stdin = child.stdin.take().expect("server stdin piped");
    let service = match tokio::time::timeout(IO_TIMEOUT, ().serve((stdout, stdin))).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let _ = child.start_kill();
            panic!("rmcp client initialize failed: {e}");
        }
        Err(_) => {
            let _ = child.start_kill();
            panic!("rmcp handshake timed out after {IO_TIMEOUT:?}");
        }
    };
    ServerHandle { child, service }
}

/// Call a tool and return its first text block, or an Err string.
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
            other => Err(format!("expected text content from `{tool}`, got {other:?}")),
        },
        Ok(Err(e)) => Err(format!("`{tool}` call failed: {e}")),
        Err(_) => {
            let _ = srv.child.start_kill();
            Err(format!("`{tool}` call exceeded {IO_TIMEOUT:?} — server killed"))
        }
    }
}

/// Poll `search` until `token` appears in results (up to `max_attempts × 500 ms`).
/// Returns the `repository_id` of the first hit, or panics on timeout.
async fn wait_for_token(
    srv: &mut ServerHandle,
    token: &str,
    max_attempts: u32,
) -> String {
    for attempt in 0..max_attempts {
        let text = call_tool_text(srv, "search", serde_json::json!({ "query": token }))
            .await
            .unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if let Some(results) = v["results"].as_array()
            && let Some(first) = results.first()
        {
            let repo_id = first["repository_id"].as_str().unwrap_or("").to_owned();
            if !repo_id.is_empty() {
                return repo_id;
            }
        }
        if attempt + 1 < max_attempts {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    panic!("token '{token}' never became searchable after {max_attempts} attempts");
}

// ─────────────────────────────────────────────────────────────────────────────
// §35 test 1: reordering config must NOT change repository identities
// ─────────────────────────────────────────────────────────────────────────────

/// §35 `test_restart_reorder_preserves_ids`:
///
/// 1. Create a temporary `ATTIC_HOME` and three temp repository dirs A/B/C
///    (no common parent).
/// 2. Write `<ATTIC_HOME>/config.toml` with order A, B, C.
/// 3. Spawn the server; wait for all three probe tokens to be indexed;
///    record `{ root_path → repository_id }` from search results.
/// 4. Kill the server.
/// 5. Rewrite `config.toml` with order C, A, B.
/// 6. Spawn a fresh server from the SAME `ATTIC_HOME`.
/// 7. Assert that every root has the same `repository_id` as before —
///    identity must not depend on config order.
#[tokio::test]
async fn test_restart_reorder_preserves_ids() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("attic-home");
    std::fs::create_dir_all(&home).expect("create ATTIC_HOME");

    // ── Three independent repo dirs with unique probe tokens ──────────────
    let roots: Vec<(&str, &str)> = vec![
        ("repoA", "REORDER_TOKEN_ALPHA_XQ7"),
        ("repoB", "REORDER_TOKEN_BETA_XQ7"),
        ("repoC", "REORDER_TOKEN_GAMMA_XQ7"),
    ];
    let mut dirs: Vec<PathBuf> = Vec::new();
    for (name, token) in &roots {
        let dir = tmp.path().join(name);
        std::fs::create_dir_all(&dir).expect("create repo dir");
        std::fs::write(
            dir.join(format!("{name}_probe.rs")),
            format!("// {token}\npub fn marker() {{}}\n"),
        )
        .expect("write probe");
        // canonicalize so the path matches what the server persists
        dirs.push(dir.canonicalize().expect("canonicalize dir"));
    }

    // ── Write config.toml: order A, B, C ─────────────────────────────────
    fn write_config(home: &Path, ordered: &[&PathBuf]) {
        let mut body = String::from(
            "# Attic workspace configuration (generated by the `workspace` MCP tool)\n",
        );
        for d in ordered {
            body.push_str("[[repositories]]\n");
            body.push_str(&format!("path = \"{}\"\n", d.display()));
        }
        std::fs::write(home.join("config.toml"), body).expect("write config.toml");
    }
    write_config(&home, &[&dirs[0], &dirs[1], &dirs[2]]);

    // ── First run: index all three repos, collect repo-id per token ───────
    let mut srv = connect_home(&bin, &home).await;

    // Wait until all probe tokens are searchable.
    let id_a_run1 =
        wait_for_token(&mut srv, roots[0].1, 40).await;
    let id_b_run1 =
        wait_for_token(&mut srv, roots[1].1, 40).await;
    let id_c_run1 =
        wait_for_token(&mut srv, roots[2].1, 40).await;

    assert_ne!(id_a_run1, id_b_run1, "repos must have distinct IDs");
    assert_ne!(id_b_run1, id_c_run1, "repos must have distinct IDs");
    assert_ne!(id_a_run1, id_c_run1, "repos must have distinct IDs");

    // Graceful shutdown.
    let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
    drop(srv);

    // ── Rewrite config with order C, A, B ────────────────────────────────
    write_config(&home, &[&dirs[2], &dirs[0], &dirs[1]]);

    // ── Second run: verify identities are order-independent ──────────────
    let mut srv2 = connect_home(&bin, &home).await;

    // All tokens must still be searchable after restart.
    let id_a_run2 = wait_for_token(&mut srv2, roots[0].1, 40).await;
    let id_b_run2 = wait_for_token(&mut srv2, roots[1].1, 40).await;
    let id_c_run2 = wait_for_token(&mut srv2, roots[2].1, 40).await;

    // §18 hard property: same root → same repository_id regardless of order.
    assert_eq!(
        id_a_run1, id_a_run2,
        "repoA repository_id must be stable across config reorder: \
         run1={id_a_run1} run2={id_a_run2}"
    );
    assert_eq!(
        id_b_run1, id_b_run2,
        "repoB repository_id must be stable across config reorder: \
         run1={id_b_run1} run2={id_b_run2}"
    );
    assert_eq!(
        id_c_run1, id_c_run2,
        "repoC repository_id must be stable across config reorder: \
         run1={id_c_run1} run2={id_c_run2}"
    );

    let _ = tokio::time::timeout(IO_TIMEOUT, srv2.service.close()).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// §35 test 2: configured-but-unavailable repo shows degraded; others usable
// ─────────────────────────────────────────────────────────────────────────────

/// §35 `test_restart_unavailable_repo_shows_degraded`:
///
/// 1. Create a temporary `ATTIC_HOME` and three repo dirs A/B/C.
/// 2. Write `config.toml`; spawn server; confirm 3 configured, 0 unavailable.
/// 3. Search for probe tokens in A and C → each returns 1 result.
/// 4. Kill the server; rename B's directory so it becomes unavailable.
/// 5. Spawn fresh server → status must report `unavailable_repository_count == 1`,
///    `degraded == true`, and B's canonical path in `unavailable_repositories`.
/// 6. Search for A's probe token → still returns 1 result (A/C usable).
/// 7. Kill the server; rename B's directory back.
/// 8. Spawn fresh server → status must report `unavailable_repository_count == 0`,
///    `degraded == false`.
#[tokio::test]
async fn test_restart_unavailable_repo_shows_degraded() {
    let bin = require_bin();
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let home = tmp.path().join("attic-home");
    std::fs::create_dir_all(&home).expect("create ATTIC_HOME");

    // ── Three independent repo dirs ───────────────────────────────────────
    let dir_a = tmp.path().join("unavail_repoA");
    let dir_b = tmp.path().join("unavail_repoB");
    let dir_c = tmp.path().join("unavail_repoC");
    std::fs::create_dir_all(&dir_a).expect("create repoA");
    std::fs::create_dir_all(&dir_b).expect("create repoB");
    std::fs::create_dir_all(&dir_c).expect("create repoC");

    let token_a = "UNAVAIL_TOKEN_AREPOTOKEN_ZZ9";
    let token_b = "UNAVAIL_TOKEN_BREPOTOKEN_ZZ9";
    let token_c = "UNAVAIL_TOKEN_CREPOTOKEN_ZZ9";
    std::fs::write(dir_a.join("a_probe.txt"), format!("{token_a}\n")).expect("write A probe");
    std::fs::write(dir_b.join("b_probe.txt"), format!("{token_b}\n")).expect("write B probe");
    std::fs::write(dir_c.join("c_probe.txt"), format!("{token_c}\n")).expect("write C probe");

    // Canonicalize paths so they match what the server stores.
    let canon_a = dir_a.canonicalize().expect("canonicalize A");
    let canon_b = dir_b.canonicalize().expect("canonicalize B");
    let canon_c = dir_c.canonicalize().expect("canonicalize C");

    // ── Write initial config: A, B, C ────────────────────────────────────
    let config_body = format!(
        "# Attic workspace configuration (generated by the `workspace` MCP tool)\n\
         [[repositories]]\npath = \"{}\"\n\
         [[repositories]]\npath = \"{}\"\n\
         [[repositories]]\npath = \"{}\"\n",
        canon_a.display(),
        canon_b.display(),
        canon_c.display(),
    );
    std::fs::write(home.join("config.toml"), &config_body).expect("write config.toml");

    // ── Run 1: all three repos configured ────────────────────────────────
    {
        let mut srv = connect_home(&bin, &home).await;

        // Wait for all probe tokens to be indexed.
        wait_for_token(&mut srv, token_a, 40).await;
        wait_for_token(&mut srv, token_b, 40).await;
        wait_for_token(&mut srv, token_c, 40).await;

        // Status: 3 configured, 0 unavailable, not degraded.
        let status_text = call_tool_text(&mut srv, "status", serde_json::json!({}))
            .await
            .expect("status run1");
        let sv: Value = serde_json::from_str(&status_text).expect("status JSON run1");
        assert_eq!(sv["status"], "ok", "run1 status: {status_text}");
        assert_eq!(
            sv["workspace"]["configured_repository_count"], 3,
            "run1: expected 3 configured repos; {status_text}"
        );
        assert_eq!(
            sv["workspace"]["unavailable_repository_count"], 0,
            "run1: expected 0 unavailable; {status_text}"
        );
        assert_eq!(
            sv["workspace"]["degraded"], false,
            "run1: must not be degraded; {status_text}"
        );

        let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
    }

    // ── Make B unavailable by renaming its directory ──────────────────────
    let dir_b_hidden = tmp.path().join("unavail_repoB_hidden");
    std::fs::rename(&canon_b, &dir_b_hidden).expect("rename B to hidden");

    // ── Run 2: B's path no longer exists ─────────────────────────────────
    {
        let mut srv = connect_home(&bin, &home).await;

        // Give the server a moment to start (status is immediate).
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Status: B is unavailable, degraded == true.
        let status_text = call_tool_text(&mut srv, "status", serde_json::json!({}))
            .await
            .expect("status run2");
        let sv: Value = serde_json::from_str(&status_text).expect("status JSON run2");

        assert_eq!(sv["status"], "ok", "run2 status must be ok (not unconfigured): {status_text}");
        assert_eq!(
            sv["workspace"]["unavailable_repository_count"], 1,
            "run2: B must be reported unavailable; {status_text}"
        );
        assert_eq!(
            sv["workspace"]["degraded"], true,
            "run2: workspace must be degraded when a member is unavailable; {status_text}"
        );

        // B must be listed in unavailable_repositories.
        let unavail = sv["workspace"]["unavailable_repositories"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        assert_eq!(unavail.len(), 1, "run2: exactly one unavailable entry: {status_text}");
        let unavail_path = unavail[0]["path"].as_str().unwrap_or("");
        // The path reported must include the canonical B path components.
        // On Windows canon_b may have \\?\ prefix; compare display() for robustness.
        let canon_b_str = canon_b.display().to_string();
        // Strip \\?\ prefix if present for comparison.
        let canon_b_norm = canon_b_str.trim_start_matches(r"\\?\");
        let unavail_norm = unavail_path.trim_start_matches(r"\\?\");
        assert!(
            unavail_norm.contains("unavail_repoB")
                || unavail_path.contains(canon_b_norm)
                || unavail_norm == canon_b_norm,
            "run2: unavailable path must identify B; got '{unavail_path}', expected to contain '{canon_b_norm}'"
        );

        // A and C are still usable — search for token_a must still work.
        let a_results_text =
            call_tool_text(&mut srv, "search", serde_json::json!({ "query": token_a }))
                .await
                .expect("search token_a run2");
        let av: Value =
            serde_json::from_str(&a_results_text).expect("search JSON run2");
        let a_hits = av["results"].as_array().map(Vec::len).unwrap_or(0);
        assert_eq!(
            a_hits, 1,
            "run2: token_a must still be searchable (A/C usable); {a_results_text}"
        );

        let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
    }

    // ── Restore B ─────────────────────────────────────────────────────────
    std::fs::rename(&dir_b_hidden, &canon_b).expect("rename B back");

    // ── Run 3: B is available again — workspace fully healthy ─────────────
    {
        let mut srv = connect_home(&bin, &home).await;

        // Wait for B's probe token to be re-indexed after the reappearance.
        wait_for_token(&mut srv, token_b, 40).await;

        let status_text = call_tool_text(&mut srv, "status", serde_json::json!({}))
            .await
            .expect("status run3");
        let sv: Value = serde_json::from_str(&status_text).expect("status JSON run3");

        assert_eq!(sv["status"], "ok", "run3 status: {status_text}");
        assert_eq!(
            sv["workspace"]["unavailable_repository_count"], 0,
            "run3: B restored — no unavailable repos; {status_text}"
        );
        assert_eq!(
            sv["workspace"]["degraded"], false,
            "run3: workspace must not be degraded once B is restored; {status_text}"
        );

        let _ = tokio::time::timeout(IO_TIMEOUT, srv.service.close()).await;
    }
}
