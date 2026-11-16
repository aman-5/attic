mod common;
use common::Fixture;
#[test]
fn packs_are_private_bounded_redacted_and_bound_to_run_and_bytes() {
    let secret = ["ghp_", "1234567890abcdefghijklmnopqrstuvwxyzABCD"].concat();
    let source = format!(
        "const TOKEN: &str = \"{secret}\";\n{}",
        "// evidence line\n".repeat(300)
    );
    let mut f = Fixture::new(&source, "TARGET_MODIFY repo src/lib.rs", true);
    f.service.config.packs.byte_budget = 1024;
    let doc = f.verified();
    let inline = f
        .service
        .pack(
            &doc,
            "codebase_details",
            "inline",
            std::slice::from_ref(&f.repo),
        )
        .unwrap();
    let markdown = inline["markdown"].as_str().unwrap();
    assert!(!f.dir.path().join("data").join("packs").exists());
    assert!(!markdown.contains(&secret));
    assert!(markdown.len() <= 1024);
    assert!(inline["context_cost"].is_string());
    let handle = f
        .service
        .pack(
            &doc,
            "codebase_details",
            "file",
            std::slice::from_ref(&f.repo),
        )
        .unwrap();
    assert!(handle.get("markdown").is_none());
    let path = std::path::Path::new(handle["path"].as_str().unwrap());
    assert!(!attic_discovery::security::path_is_within(path, &f.root));
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(attic_spec::sha256(&bytes), handle["sha256"]);
    assert_eq!(bytes.len(), handle["bytes"]);
    assert!(!String::from_utf8_lossy(&bytes).contains(&secret));
    assert_eq!(
        handle["run_id"],
        doc.latest_run_id.as_ref().unwrap().as_str()
    );
    f.service.invalidate_packs(&doc).unwrap();
    assert!(!path.exists());
    std::fs::write(f.root.join("src").join("lib.rs"), "changed").unwrap();
    assert_eq!(
        f.service
            .pack(
                &doc,
                "codebase_details",
                "inline",
                std::slice::from_ref(&f.repo)
            )
            .unwrap_err()
            .code,
        "ERR_INDEX_NOT_FRESH"
    );
}
