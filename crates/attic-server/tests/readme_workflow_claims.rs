//! Guards the README's process-model claims: many launches against one
//! database are safe because exactly one becomes the daemon (single writer,
//! watchers, recovery) and the rest relay to it.

fn readme() -> String {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md"))
        .expect("README.md must be readable");
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn readme_describes_the_single_daemon_process_model() {
    let readme = readme();
    assert!(
        readme.contains("becomes the **daemon**: it owns one catalog writer and one writer per open repository store"),
        "README must describe daemon-owned catalog and repository writers"
    );
    assert!(
        readme.contains("becomes a thin **relay**"),
        "README must say later launches become relays"
    );
    assert!(
        !readme.contains("ATTIC_NO_DAEMON"),
        "README must not document the removed single-process mode"
    );
}
