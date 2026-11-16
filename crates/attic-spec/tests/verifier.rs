mod common;
use attic_spec::verifier;
use common::Fixture;

#[test]
fn empty_analysis_receipt_proves_absence_but_legacy_indexes_remain_unproven() {
    let f = Fixture::new("", "TARGET_CREATE_SYMBOL repo src/lib.rs Fresh", true);
    let run = verifier::verify(
        &attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap(),
        1,
        false,
        std::slice::from_ref(&f.repo),
        &[],
        &f.service.config.validation,
    )
    .unwrap();
    assert!(run.passed, "{:#?}", run.citations);
    assert_eq!(run.citations[0].tier, "PRE_CREATE_ABSENT_OK");
    assert!(run.citations[0].occurrence.is_some());
    let f = Fixture::new("", "TARGET_CREATE_SYMBOL repo src/lib.rs Fresh", false);
    let run = verifier::verify(
        &attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap(),
        1,
        false,
        std::slice::from_ref(&f.repo),
        &[],
        &f.service.config.validation,
    )
    .unwrap();
    assert!(!run.passed);
    assert_eq!(run.citations[0].tier, "UNSUPPORTED_CONSTRUCT");
    assert!(!f.pool.with_reader(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='spec_index_analysis_receipts')",[],|r|r.get::<_,bool>(0))?)).unwrap());
}

#[test]
fn current_bytes_qualified_symbols_collisions_and_pre_post_truth_table() {
    let f = Fixture::new(
        "pub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs original\nTARGET_CREATE_FILE repo src/new.rs\nTARGET_CREATE_SYMBOL repo src/lib.rs Fresh",
        true,
    );
    let bytes = attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap();
    let run = verifier::verify(
        &bytes,
        1,
        false,
        std::slice::from_ref(&f.repo),
        &[],
        &f.service.config.validation,
    )
    .unwrap();
    assert!(run.passed, "{:#?}", run.citations);
    assert_eq!(run.citations[0].tier, "AST_VERIFIED");
    assert_eq!(run.citations[1].tier, "PRE_CREATE_ABSENT_OK");
    let post = verifier::verify(
        &bytes,
        1,
        true,
        std::slice::from_ref(&f.repo),
        &[],
        &f.service.config.validation,
    )
    .unwrap();
    assert!(!post.passed);
    let wrong = bytes
        .windows(b"original".len())
        .position(|s| s == b"original")
        .unwrap();
    let mut invalid = bytes.clone();
    invalid.splice(wrong..wrong + 8, b"wrong::original".iter().copied());
    assert!(
        !verifier::verify(
            &invalid,
            1,
            false,
            std::slice::from_ref(&f.repo),
            &[],
            &f.service.config.validation
        )
        .unwrap()
        .passed
    );
    std::fs::write(f.root.join("src").join("lib.rs"), "pub fn modified() {}\n").unwrap();
    assert_eq!(
        verifier::verify(
            &bytes,
            1,
            false,
            std::slice::from_ref(&f.repo),
            &[],
            &f.service.config.validation
        )
        .unwrap_err()
        .code,
        "ERR_INDEX_NOT_FRESH"
    );
}

#[test]
fn excluded_creation_paths_and_receipt_completeness_are_not_negative_evidence() {
    let f = Fixture::new("", "TARGET_CREATE_FILE repo node_modules/new.js", true);
    assert_eq!(
        f.service
            .verify(
                &f.registered(),
                false,
                std::slice::from_ref(&f.repo),
                &[],
                None,
                false
            )
            .err()
            .map(|e| e.code),
        Some("ERR_CITATION_PATH_NOT_INDEXABLE".into())
    );
}

#[test]
fn incomplete_fallback_error_and_misbound_receipts_remain_unproven() {
    let f = Fixture::new("", "TARGET_CREATE_SYMBOL repo src/lib.rs Fresh", true);
    let bytes = attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap();
    let conn = rusqlite::Connection::open(f.dir.path().join("data").join("index.db")).unwrap();
    for (field, bad, good) in [
        ("structurally_complete", "0", "1"),
        ("fallback_used", "1", "0"),
        ("has_errors", "1", "0"),
        ("symbols_supported", "0", "1"),
        (
            "content_hash",
            "'wrong'",
            "(SELECT content_hash FROM core_file_occurrences WHERE id=file_occurrence_id)",
        ),
        (
            "source_revision_id",
            "'wrong'",
            "(SELECT source_revision_id FROM core_file_occurrences WHERE id=file_occurrence_id)",
        ),
        (
            "index_generation_id",
            "'wrong'",
            "(SELECT index_generation_id FROM core_file_occurrences WHERE id=file_occurrence_id)",
        ),
    ] {
        conn.execute(
            &format!("UPDATE spec_index_analysis_receipts SET {field}={bad}"),
            [],
        )
        .unwrap();
        let run = verifier::verify(
            &bytes,
            1,
            false,
            std::slice::from_ref(&f.repo),
            &[],
            &f.service.config.validation,
        )
        .unwrap();
        assert!(!run.passed, "{field} must not prove absence");
        assert_eq!(run.citations[0].tier, "UNSUPPORTED_CONSTRUCT");
        conn.execute(
            &format!("UPDATE spec_index_analysis_receipts SET {field}={good}"),
            [],
        )
        .unwrap();
    }
}

#[test]
fn duplicate_qualified_definitions_are_ambiguous_and_fingerprints_exclude_locations() {
    let f = Fixture::new(
        "pub fn original() {}\npub fn original() {}\n",
        "TARGET_MODIFY repo src/lib.rs original",
        true,
    );
    let bytes = attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap();
    let run = verifier::verify(
        &bytes,
        1,
        false,
        std::slice::from_ref(&f.repo),
        &[],
        &f.service.config.validation,
    )
    .unwrap();
    assert!(!run.passed);
    assert_eq!(run.citations[0].tier, "COLLISION_ERROR");
    let mut findings = run.citations;
    let first = verifier::fingerprint(&findings);
    findings[0].content_hash = Some("changed-body".into());
    findings[0].start_line = Some(999);
    findings[0].source_revision = Some("new-source".into());
    assert_eq!(verifier::fingerprint(&findings), first);
    findings[0].signature_hash = Some("changed-signature".into());
    assert_ne!(verifier::fingerprint(&findings), first);
    findings[0].repository_root = "root-provider".into();
    findings[0].tier = "AST_VERIFIED".into();
    findings[0].signature_hash = Some("abc".into());
    assert_eq!(
        verifier::fingerprint(&findings),
        "7be80127e1e9e8f4de9c92697cace479a8090bf9eff89958265c346694c7024e"
    );
}

#[test]
fn syntax_error_warning_is_not_a_complete_negative_symbol_analysis() {
    let f = Fixture::new(
        "pub fn broken( {\n",
        "TARGET_CREATE_SYMBOL repo src/lib.rs Fresh",
        true,
    );
    let bytes = attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap();
    let run = verifier::verify(
        &bytes,
        1,
        false,
        std::slice::from_ref(&f.repo),
        &[],
        &f.service.config.validation,
    )
    .unwrap();
    assert!(!run.passed);
    assert_eq!(run.citations[0].tier, "UNSUPPORTED_CONSTRUCT");
}

#[test]
fn exhausted_reader_pool_returns_bounded_busy_without_leaking_a_reader() {
    let f = Fixture::new("", "TARGET_MODIFY repo src/lib.rs", true);
    let bytes = attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap();
    fn hold(pool: &attic_storage::DbPool, depth: usize, callback: &mut dyn FnMut()) {
        assert!(depth < 256, "unexpectedly unbounded pool");
        let result = pool.with_reader(|_| {
            hold(pool, depth + 1, callback);
            Ok(())
        });
        match result {
            Err(attic_storage::StorageError::PoolExhausted) => callback(),
            other => other.unwrap(),
        }
    }
    let mut called = false;
    hold(&f.pool, 0, &mut || {
        let start = std::time::Instant::now();
        let error = verifier::verify(
            &bytes,
            1,
            false,
            std::slice::from_ref(&f.repo),
            &[],
            &f.service.config.validation,
        )
        .unwrap_err();
        assert_eq!(error.code, "ERR_BUSY");
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        called = true;
    });
    assert!(called);
    assert!(
        verifier::verify(
            &bytes,
            1,
            false,
            std::slice::from_ref(&f.repo),
            &[],
            &f.service.config.validation
        )
        .unwrap()
        .passed
    );
}
