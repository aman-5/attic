//! Guards the README's multi-repository workflow claim against drifting back
//! to an unsupported architecture (multiple processes each running their own
//! writer/watcher/startup-recovery against a shared database). Since the
//! daemon/relay self-election model landed, multiple `attic-server`
//! *processes* ARE supported (one daemon + N thin relays) — what remains
//! genuinely unsupported is multiple concurrent *daemons* writing to the
//! same database. See README.md "Workspaces & Multiple Repositories".

#[test]
fn readme_does_not_recommend_unsupported_multi_process_db_sharing() {
    let readme = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
        .expect("README.md must be readable");

    assert!(
        !readme.contains("point additional\nAttic instances"),
        "README must not recommend running multiple independent attic-server \
         processes (each with its own writer/watcher) against a shared \
         ATTIC_DB_PATH outside of the daemon/relay architecture"
    );
    assert!(
        readme.contains("self-elects as its **daemon**") && readme.contains("thin **relay**"),
        "README must explain the daemon/relay self-election model that makes \
         multiple attic-server processes against the same database safe"
    );
    assert!(
        readme.contains(
            "does **not** support\nmultiple *daemons* concurrently writing to the same database"
        ),
        "README must still explicitly state that multiple concurrent DAEMONS \
         writing to the same database is unsupported — the safe part is \
         one daemon + N relays, not multiple independent daemons"
    );
}
