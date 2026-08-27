//! Guards the README's multi-repository workflow claim against drifting back
//! to an unsupported architecture (multiple processes sharing one SQLite
//! writer). See README.md "Configure a workspace".

#[test]
fn readme_does_not_recommend_unsupported_multi_process_db_sharing() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
        .expect("README.md must be readable");

    assert!(
        !readme.contains("point additional\nAttic instances"),
        "README must not recommend running multiple attic-server processes \
         against a shared ATTIC_DB_PATH — the writer/watcher/startup-recovery \
         architecture assumes single-process ownership per database"
    );
    assert!(
        readme.contains("does **not** support multiple `attic-server` processes"),
        "README must explicitly state that multi-process DB sharing is unsupported"
    );
}
