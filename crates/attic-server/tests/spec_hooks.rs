use attic_spec::hooks;
use std::{path::Path, process::Command};
fn fixture() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("egm-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap()
            .success()
    );
}
#[test]
fn explicit_hooks_path_chaining_and_uninstall_preserve_custom_hook() {
    let dir = fixture();
    git(dir.path(), &["init", "--quiet"]);
    git(dir.path(), &["config", "core.hooksPath", ".hooks"]);
    std::fs::create_dir(dir.path().join(".hooks")).unwrap();
    let path = dir.path().join(".hooks").join("pre-commit");
    let custom = "#!/bin/sh\necho original\n";
    std::fs::write(&path, custom).unwrap();
    assert_eq!(
        hooks::install(dir.path(), false, false).unwrap_err().code,
        "ERR_HOOK_CONFLICT"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), custom);
    hooks::install(dir.path(), true, false).unwrap();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains(hooks::MARKER)
    );
    hooks::uninstall(dir.path()).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), custom);
}
#[test]
fn hook_managers_are_print_only_and_host_mapping_does_not_expand_tool_access() {
    let dir = fixture();
    git(dir.path(), &["init", "--quiet"]);
    std::fs::create_dir(dir.path().join(".husky")).unwrap();
    let report = hooks::install(dir.path(), true, true).unwrap();
    assert!(report.contains("manually"));
    assert!(
        !dir.path()
            .join(".git")
            .join("hooks")
            .join("pre-commit")
            .exists()
    );
    let host = hooks::host_config(true);
    assert_eq!(
        host["hooks"]["PreToolUse"][0]["matcher"],
        "Edit|Write|MultiEdit|NotebookEdit|Bash"
    );
    assert!(host.get("mcpServers").is_none());
    assert!(hooks::GUIDANCE.contains("dea7761de06ce5b51c177ebb9bba0469f24af5a4"));
}

fn shell() -> std::path::PathBuf {
    if cfg!(windows) {
        let output = Command::new("git").arg("--exec-path").output().unwrap();
        let path = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
        path.ancestors()
            .map(|p| p.join("usr").join("bin").join("sh.exe"))
            .find(|p| p.exists())
            .expect("Git for Windows shell required")
    } else {
        std::path::PathBuf::from("/bin/sh")
    }
}
#[test]
fn rendered_hook_missing_binary_policy_and_disabled_binary_are_executable() {
    let dir = fixture();
    let shell = shell();
    let shell_dir = shell.parent().unwrap();
    let hook = dir.path().join("pre-commit");
    for (closed, exit) in [(false, 0), (true, 1)] {
        std::fs::write(&hook, hooks::git_script(closed)).unwrap();
        let output = Command::new(&shell)
            .arg("pre-commit")
            .current_dir(dir.path())
            .env("PATH", shell_dir)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("attic unavailable"));
    }
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let binary = bin.join(if cfg!(windows) { "attic.exe" } else { "attic" });
    std::fs::hard_link(env!("CARGO_BIN_EXE_attic"), binary).unwrap();
    let path = std::env::join_paths([bin.as_path(), shell_dir]).unwrap();
    let output = Command::new(&shell)
        .arg("pre-commit")
        .current_dir(dir.path())
        .env("PATH", path)
        .env("ATTIC_HOME", dir.path().join("home"))
        .env("ATTIC_DB_PATH", dir.path().join("data").join("catalog.db"))
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join("data").exists());
}
