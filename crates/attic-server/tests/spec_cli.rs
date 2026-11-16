use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};
fn fixture() -> tempfile::TempDir {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("egm-tests");
    std::fs::create_dir_all(&path).unwrap();
    tempfile::tempdir_in(path).unwrap()
}
fn command(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_attic"));
    cmd.args(["spec"])
        .current_dir(dir)
        .env("ATTIC_HOME", dir.join("home"))
        .env("ATTIC_DB_PATH", dir.join("data").join("catalog.db"))
        .env_remove("ATTIC_CONFIG")
        .env("ATTIC_WORKSPACE_ROOT", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}
fn run(dir: &Path, args: &[&str], input: &[u8]) -> std::process::Output {
    let mut child = command(dir).args(args).spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}
fn config(dir: &Path, fail_closed: bool) {
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data").join("attic.toml"),format!("[planning]\nenabled=true\n[planning.fast_path]\nenforce=true\nmax_files=1\n[planning.hooks]\nfail_closed={fail_closed}\n")).unwrap();
}
#[test]
fn disabled_hooks_ignore_bad_payload_and_create_no_state_or_runtime() {
    let dir = fixture();
    std::fs::create_dir_all(dir.path().join("data")).unwrap();
    std::fs::write(
        dir.path().join("data").join("attic.toml"),
        "[planning]\nenabled = false\n",
    )
    .unwrap();
    for action in ["check-write", "baseline-start", "check-commit"] {
        let out = run(dir.path(), &[action, "--host", "claude"], b"not json");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert!(!dir.path().join("data").join("spec_state.db").exists());
    assert!(!dir.path().join("data").join("catalog.db").exists());
    assert!(!dir.path().join("home").exists());
    assert!(run(dir.path(), &["--help"], b"").status.success());
}
#[test]
fn production_approval_rejects_all_piped_input_without_opening_state_or_daemon() {
    let dir = fixture();
    config(dir.path(), true);
    let out = run(dir.path(), &["approve", "one"], b"Y\nABC123\n");
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("ERR_APPROVAL_REQUIRES_TTY"));
    assert!(!dir.path().join("data").join("spec_state.db").exists());
    assert!(!dir.path().join("data").join("catalog.db").exists());
    assert!(!dir.path().join("home").exists());
}
#[test]
fn malformed_host_payload_obeys_error_policy_and_notebook_path_is_enforced() {
    let dir = fixture();
    config(dir.path(), false);
    assert!(
        run(dir.path(), &["check-write", "--host", "claude"], b"broken")
            .status
            .success()
    );
    config(dir.path(), true);
    assert_eq!(
        run(dir.path(), &["check-write", "--host", "claude"], b"broken")
            .status
            .code(),
        Some(2)
    );
    let initialized = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(initialized.success());
    std::fs::write(dir.path().join("one.rs"), "dirty").unwrap();
    std::fs::write(dir.path().join("two.rs"), "dirty").unwrap();
    let payload = serde_json::json!({"hook_event_name":"PreToolUse","session_id":"fixture","cwd":dir.path(),"tool_name":"NotebookEdit","tool_input":{"notebook_path":"analysis.ipynb"}});
    let out = run(
        dir.path(),
        &["check-write", "--host", "claude"],
        payload.to_string().as_bytes(),
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let design = serde_json::json!({"hook_event_name":"PreToolUse","session_id":"fixture","cwd":dir.path(),"tool_name":"Write","tool_input":{"file_path":"docs/designs/one.md","content":"# design"}});
    assert!(
        run(
            dir.path(),
            &["check-write", "--host", "claude"],
            design.to_string().as_bytes()
        )
        .status
        .success()
    );
    assert!(!dir.path().join("data").join("spec_state.db").exists());
}
#[test]
fn sessionstart_resume_does_not_replace_baseline() {
    let dir = fixture();
    config(dir.path(), true);
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(dir.path().join("existing.rs"), "before").unwrap();
    let payload=serde_json::json!({"hook_event_name":"SessionStart","session_id":"fixture","cwd":dir.path()}).to_string();
    let first = run(
        dir.path(),
        &["baseline-start", "--host", "claude"],
        payload.as_bytes(),
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let db = dir.path().join("data").join("spec_state.db");
    let read = || {
        let conn =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        conn.query_row(
            "SELECT payload FROM spec_session_baselines WHERE session_id='fixture'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap()
    };
    let original = read();
    std::fs::write(dir.path().join("existing.rs"), "changed").unwrap();
    let second = run(
        dir.path(),
        &["baseline-start", "--host", "claude"],
        payload.as_bytes(),
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(read(), original);
    assert!(!dir.path().join("data").join("catalog.db").exists());
}

#[test]
fn absolute_target_in_other_active_root_is_enforced_independently_of_cwd() {
    let dir = fixture();
    config(dir.path(), true);
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    for root in [&a, &b] {
        std::fs::create_dir_all(root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
    let workspace = dir.path().join("data").join("config.toml");
    std::fs::write(
        &workspace,
        format!(
            "[[repositories]]\npath=\"{}\"\n[[repositories]]\npath=\"{}\"\n",
            a.display(),
            b.display()
        ),
    )
    .unwrap();
    std::fs::write(b.join("dirty.rs"), "dirty").unwrap();
    let payload = serde_json::json!({"hook_event_name":"PreToolUse","session_id":"fixture",
        "cwd":a,"tool_name":"Write","tool_input":{"file_path":b.join("new.rs"),"content":"unapproved"}});
    let mut child = command(dir.path())
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .env("ATTIC_CONFIG", &workspace)
        .args(["check-write", "--host", "claude"])
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("uncovered files: 2"),
        "must evaluate B's budget: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.path().join("data").join("spec_state.db").exists());
}

#[test]
fn abandon_of_deleted_spec_succeeds_and_cleans_packs_without_runtime_startup() {
    let dir = fixture();
    config(dir.path(), true);
    let mut cfg = attic_core::config::PlanningConfig {
        enabled: true,
        ..Default::default()
    };
    cfg.fast_path.enforce = true;
    let service =
        attic_spec::service::SpecService::open(&dir.path().join("data").join("catalog.db"), cfg)
            .unwrap()
            .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let doc = attic_spec::state::Document::new(
        "one",
        &attic_spec::verifier::identity(&root),
        "docs/designs/one.md",
        "one",
        attic_spec::now_micros().unwrap(),
    );
    service
        .store
        .write(|tx| attic_spec::store::insert(tx, &doc))
        .unwrap();
    let packs = service.data_dir.join("packs").join(&doc.spec_uid);
    std::fs::create_dir_all(&packs).unwrap();
    std::fs::write(packs.join("old.md"), "fixture").unwrap();
    let out = run(
        dir.path(),
        &["abandon", "one", "--reason", "deleted document"],
        b"",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        service.resolve("one", None).unwrap().status,
        attic_spec::state::Status::Abandoned
    );
    assert!(!packs.exists());
    assert!(!dir.path().join("data").join("catalog.db").exists());
}
