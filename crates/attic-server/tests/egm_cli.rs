use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_attic"));
    command
        .arg("egm")
        .env("ATTIC_HOME", home)
        .env_remove("ATTIC_DB_PATH")
        .env_remove("ATTIC_CONFIG")
        .env_remove("ATTIC_WORKSPACE_ROOT");
    command
}

fn json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn controls_preserve_settings_and_toggle_without_starting_runtime_or_state() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let initial = json(command(&home).arg("status").output().unwrap());
    assert_eq!(initial["enabled"], false);
    assert!(!home.exists());
    let config_dir = home.join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    let path = config_dir.join("attic.toml");
    std::fs::write(
        &path,
        "# keep this comment\n[logging]\nfile_level = \"warn\"\n[planning]\nenabled=false\n[planning.fast_path]\nmax_files=7\n",
    )
    .unwrap();
    let enabled = json(
        command(&home)
            .args(["enable", "--root"])
            .arg(&root)
            .args(["--mode", "autopilot"])
            .output()
            .unwrap(),
    );
    assert_eq!(enabled["enabled"], true);
    assert_eq!(enabled["mode"], "autopilot");
    assert_eq!(enabled["enforce"], false);
    let guidance = json(command(&home).arg("guidance").output().unwrap());
    assert!(
        guidance["workflow"]["advance"]
            .as_str()
            .unwrap()
            .contains("automatically")
    );
    assert_eq!(
        guidance["workflow"]["reviewers"].as_array().unwrap().len(),
        3
    );
    assert_eq!(guidance["approval"], "human_only");
    assert_eq!(
        enabled["roots"][0],
        root.canonicalize().unwrap().to_str().unwrap()
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep this comment"));
    let config = attic_core::AtticConfig::parse_str(&text).unwrap();
    assert_eq!(config.logging.file_level.as_deref(), Some("warn"));
    assert_eq!(config.planning.fast_path.max_files, 7);
    let disabled = json(command(&home).arg("disable").output().unwrap());
    assert_eq!(disabled["enabled"], false);
    assert_eq!(disabled["mode"], "autopilot");
    assert!(!home.join("data").exists());
    assert!(!home.join("run").exists());
    assert!(!root.join(".git").exists());
    let enabled = json(
        command(&home)
            .args(["enable", "--all-roots", "--enforce"])
            .output()
            .unwrap(),
    );
    assert_eq!(enabled["mode"], "manual");
    assert_eq!(enabled["enforce"], true);
    assert_eq!(enabled["roots"], serde_json::json!([]));
    let guidance = json(command(&home).arg("guidance").output().unwrap());
    assert!(
        guidance["workflow"]["advance"]
            .as_str()
            .unwrap()
            .starts_with("Ask the user")
    );
}

#[test]
fn invalid_controls_never_overwrite_config_or_create_state() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    for args in [
        vec!["enable", "--mode", "invalid"],
        vec!["enable", "--root"],
        vec!["enable", "--root", "missing", "--all-roots"],
        vec!["disable", "--enforce"],
        vec!["status", "--unexpected"],
    ] {
        let output = command(&home).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!home.exists());
    }
    let path = home.join("config").join("attic.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let malformed = "[planning]\nenabled = \"not boolean\"\n";
    std::fs::write(&path, malformed).unwrap();
    assert_eq!(
        command(&home)
            .arg("disable")
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
    assert!(!home.join("data").exists());
}

#[test]
fn explicit_database_override_uses_flat_configuration_without_migration() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("unrelated-home");
    let database = dir.path().join("flat").join("custom.db");
    let mut command = command(&home);
    command.env("ATTIC_DB_PATH", &database);
    let status = json(command.args(["enable", "--all-roots"]).output().unwrap());
    assert_eq!(
        status["config_path"],
        database.with_file_name("attic.toml").to_str().unwrap()
    );
    assert!(!database.exists());
    assert!(!home.exists());
}
