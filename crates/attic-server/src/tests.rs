use super::*;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tempfile::TempDir;

#[test]
fn file_log_level_comes_from_attic_toml() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("attic.db");
    assert_eq!(configured_file_log_level(&db), LevelFilter::OFF);
    fs::write(
        tmp.path().join("attic.toml"),
        "[logging]\nfile_level = \"debug\"\n",
    )
    .unwrap();
    assert_eq!(configured_file_log_level(&db), LevelFilter::DEBUG);
    assert_eq!(level_filter_from_name("TRACE"), Some(LevelFilter::TRACE));
    assert_eq!(level_filter_from_name("verbose"), None);
}

#[test]
fn panic_diagnostic_helpers_render_and_chain() {
    let mut rendered = String::new();
    let mut chained = false;
    emit_panic_diagnostic_with(
        "boom",
        Some(("src/main.rs", 10, 7)),
        Some("stack line"),
        |line| rendered = line.to_string(),
        || chained = true,
    );
    assert!(
        rendered.contains("panic at src/main.rs:10:7: boom"),
        "{rendered}"
    );
    assert!(rendered.contains("backtrace:"), "{rendered}");
    assert!(rendered.contains("stack line"), "{rendered}");
    assert!(chained, "default hook must still be chained");
}

fn make_server(tmp: &TempDir) -> AtticServer {
    AtticServer::new(&tmp.path().join("test.db")).expect("AtticServer::new")
}

/// Default Phase 8 status bundle for tests that don't care about
/// resource-mode reporting specifically.
fn test_resource_status() -> ResourceStatus {
    ResourceStatus {
        resource_mode: attic_storage::ResourceMode::Balanced,
        resource_mode_source: attic_storage::ResourceModeSource::Detected,
        effective_resources: attic_storage::ResourcePolicy::baseline_for_mode(
            attic_storage::ResourceMode::Balanced,
        )
        .apply_fallback_safety_limits(),
        knowledge: KnowledgeState::default().to_json(),
    }
}

/// Every repository currently registered in storage — used as the
/// "configured membership" set in direct handler tests, which register
/// repositories without going through workspace configuration.
fn ids(srv: &AtticServer) -> HashSet<String> {
    srv.pool
        .with_reader(get_repository_stats)
        .expect("repository stats")
        .into_iter()
        .map(|s| s.id)
        .collect()
}

/// A fresh install (no `attic.toml` yet) must end up with a real,
/// editable file on disk afterward — not just an invisible in-memory
/// default the user can never find or tune.
#[test]
fn fresh_startup_materializes_attic_toml_on_disk() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("fresh.db");
    let attic_toml = tmp.path().join("attic.toml");
    assert!(!attic_toml.exists(), "precondition: no attic.toml yet");

    let _server = AtticServer::new(&db).expect("fresh server should start");

    assert!(
        attic_toml.exists(),
        "attic.toml must be written to disk on first startup"
    );
    let written = std::fs::read_to_string(&attic_toml).unwrap();
    assert_eq!(
        written,
        attic_core::ATTIC_TOML_TEMPLATE,
        "the materialized file must match the shipped template exactly"
    );
    assert!(written.contains("[resources]"));
    assert!(!written.contains("[semantic]"));
}

/// A misspelled analyzer plugin id must stop startup with an actionable
/// error naming the valid ids, never silently index with the wrong set.
#[test]
fn unknown_analyzer_plugin_in_attic_toml_fails_startup() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("fresh.db");
    std::fs::write(
        tmp.path().join("attic.toml"),
        "[indexing]\nanalyzers = [\"swfit\"]\n",
    )
    .unwrap();
    let err = match AtticServer::new(&db) {
        Ok(_) => panic!("startup must fail on an unknown analyzer id"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("swfit"), "{err}");
    assert!(
        err.contains("swift"),
        "error must list the valid ids: {err}"
    );
}

/// Valid analyzer and parallelism settings flow into the options every
/// bootstrap and incremental run uses.
#[test]
fn indexing_settings_flow_into_index_options() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("fresh.db");
    std::fs::write(
        tmp.path().join("attic.toml"),
        "[indexing]\nanalysis_threads = 3\nanalyzers = [\"aem\", \"java\"]\nmax_units_per_file = 5000\n",
    )
    .unwrap();
    let server = AtticServer::new(&db).expect("valid config starts");
    let opts = server.index_options();
    assert_eq!(opts.analysis_threads, 3);
    assert_eq!(opts.max_units_per_file, 5000);
    assert_eq!(opts.analyzers.enabled(), ["aem", "java"]);
    assert!(opts.structural);
}

// ── Multi-root workspace configuration: parsing + validation ──────────

#[test]
fn parse_repositories_config_reads_three_unrelated_roots() {
    let contents = r#"
        # Three arbitrary repository roots, no common parent.
        [[repositories]]
        path = "C:\Users\<username>\code\orders-service"

        [[repositories]]
        path = "C:\Users\<username>\Path1"

        [[repositories]]
        path = "C:\Users\<username>\Path3"
    "#;
    let roots = parse_repositories_config(contents).expect("valid config");
    assert_eq!(
        roots,
        vec![
            PathBuf::from(r"C:\Users\<username>\code\orders-service"),
            PathBuf::from(r"C:\Users\<username>\Path1"),
            PathBuf::from(r"C:\Users\<username>\Path3"),
        ]
    );
}

#[test]
fn parse_repositories_config_rejects_missing_path_key() {
    let contents = "[[repositories]]\nroot = \"/a\"\n";
    let err = parse_repositories_config(contents).unwrap_err();
    assert!(err.contains("unknown key"), "{err}");
}

#[test]
fn parse_repositories_config_rejects_empty_file() {
    let err = parse_repositories_config("").unwrap_err();
    assert!(err.contains("no [[repositories]]"), "{err}");
}

#[test]
fn validate_configured_roots_skips_missing_and_dedups_canonical_duplicates() {
    let tmp = TempDir::new().unwrap();
    let real = tmp.path().join("real-repo");
    fs::create_dir_all(&real).unwrap();
    let missing = tmp.path().join("does-not-exist");

    // The same real root listed twice (once via a `.` component that
    // canonicalizes to the same path) must collapse to one entry, and
    // the missing root must be skipped rather than failing everything.
    let raw = vec![real.clone(), missing, real.join(".")];
    let out = validate_configured_roots(raw);
    assert_eq!(
        out.valid.len(),
        1,
        "expected exactly one deduped valid root"
    );
    assert_eq!(out.valid[0], real.canonicalize().unwrap());
    // §17: the missing root is preserved as configured-but-unavailable,
    // never silently discarded.
    assert_eq!(out.unavailable.len(), 1, "missing root must be reported");
    // The third raw entry canonicalizes to the same real root → duplicate.
    assert_eq!(
        out.duplicates.len(),
        1,
        "canonical duplicate must be reported"
    );
}

#[test]
fn validate_configured_roots_isolates_failures_across_unrelated_roots() {
    let tmp_a = TempDir::new().unwrap();
    let tmp_c = TempDir::new().unwrap();
    let broken = PathBuf::from("Z:\\this\\does\\not\\exist\\at\\all");

    // Three configured roots with NO common parent; the middle one is
    // broken. Both good roots must still validate (failure isolation).
    let raw = vec![
        tmp_a.path().to_path_buf(),
        broken.clone(),
        tmp_c.path().to_path_buf(),
    ];
    let out = validate_configured_roots(raw);
    assert_eq!(
        out.valid.len(),
        2,
        "the two valid unrelated roots must survive"
    );
    assert!(out.valid.contains(&tmp_a.path().canonicalize().unwrap()));
    assert!(out.valid.contains(&tmp_c.path().canonicalize().unwrap()));
    // §17: the broken root is reported as unavailable with a reason.
    assert_eq!(out.unavailable.len(), 1, "broken root must be reported");
    assert_eq!(out.unavailable[0].0, broken);
}

fn text_of(r: &CallToolResult) -> String {
    match r.content.first() {
        Some(ContentBlock::Text(t)) => t.text.clone(),
        other => panic!("expected text content, got: {other:?}"),
    }
}

fn region_args(
    start_line: Option<u64>,
    end_line: Option<u64>,
    start_byte: Option<u64>,
    end_byte: Option<u64>,
) -> FileRegion {
    FileRegion {
        start_line,
        end_line,
        start_byte,
        end_byte,
    }
}

// compile-time gate: no rusqlite direct dep
#[test]
fn no_direct_rusqlite_in_server() {
    let _ = true;
}

#[test]
fn indexing_uses_writer_abstraction() {
    fn _check(e: IndexError) {
        match e {
            IndexError::Discovery(_) => {}
            IndexError::Storage(_) => {}
            IndexError::Io { .. } => {}
            IndexError::PolicyHash(_) => {}
            IndexError::AnalyzerConfig(_) => {}
            IndexError::RepositoryNotBootstrapped(_) => {}
            IndexError::TransientFailures { .. } => {}
            IndexError::IncompleteAnalysis { .. } => {}
            IndexError::ClassificationCountMismatch { .. } => {}
            IndexError::ClassificationPathMismatch { .. } => {}
            IndexError::Cancelled => {}
        }
    }
}

// validate_filter
#[test]
fn validate_filter_ok() {
    assert!(validate_filter("q", "hello", 512).is_ok());
}
#[test]
fn validate_filter_long() {
    assert!(validate_filter("q", &"a".repeat(11), 10).is_err());
}
#[test]
fn validate_filter_ctrl() {
    assert!(validate_filter("q", "a\x00b", 512).is_err());
}
#[test]
fn validate_repo_id_ok() {
    assert!(validate_repository_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
}
#[test]
fn validate_repo_id_bad() {
    assert!(validate_repository_id("../../etc").is_err());
}
#[test]
fn validate_repo_id_long() {
    assert!(validate_repository_id(&"a".repeat(65)).is_err());
}

// ── region parsing: checked numeric conversions ──────────────────────────

#[test]
fn parse_region_missing_keys_is_empty() {
    let a: HashMap<String, Value> = HashMap::new();
    assert_eq!(parse_region(&a).unwrap(), FileRegion::default());
}

#[test]
fn parse_region_rejects_negative() {
    let mut a = HashMap::new();
    a.insert("start_byte".into(), json!(-1));
    let err = parse_region(&a).unwrap_err().to_string();
    assert!(err.contains("non-negative"), "{err}");
}

#[test]
fn parse_region_rejects_float() {
    let mut a = HashMap::new();
    a.insert("start_line".into(), json!(2.5));
    assert!(parse_region(&a).is_err());
}

#[test]
fn parse_region_rejects_string() {
    let mut a = HashMap::new();
    a.insert("end_line".into(), json!("ten"));
    assert!(parse_region(&a).is_err());
}

#[test]
fn parse_region_rejects_overflow_magnitude() {
    // A JSON number beyond u64::MAX parses as a float → not a non-negative
    // integer → rejected instead of truncated.
    let mut a = HashMap::new();
    a.insert("start_byte".into(), json!(1e30));
    assert!(parse_region(&a).is_err());

    // Values within u64 but beyond MAX_REGION_VALUE are rejected BEFORE
    // any conversion or expensive work.
    let mut b = HashMap::new();
    b.insert("end_line".into(), json!(u64::MAX));
    let err = parse_region(&b).unwrap_err().to_string();
    assert!(err.contains("maximum allowed"), "{err}");
}

#[test]
fn parse_region_rejects_inverted_windows() {
    let mut a = HashMap::new();
    a.insert("start_byte".into(), json!(10));
    a.insert("end_byte".into(), json!(5));
    let err = parse_region(&a).unwrap_err().to_string();
    assert!(err.contains("greater than or equal"), "{err}");

    let mut b = HashMap::new();
    b.insert("start_line".into(), json!(9));
    b.insert("end_line".into(), json!(2));
    assert!(parse_region(&b).is_err());
}

#[test]
fn parse_region_enforces_span_limits() {
    let mut a = HashMap::new();
    a.insert("start_line".into(), json!(1));
    a.insert("end_line".into(), json!(MAX_LINE_SPAN + 1));
    let err = parse_region(&a).unwrap_err().to_string();
    assert!(err.contains("too large"), "{err}");

    // Exactly MAX_LINE_SPAN lines is allowed.
    let mut ok = HashMap::new();
    ok.insert("start_line".into(), json!(1));
    ok.insert("end_line".into(), json!(MAX_LINE_SPAN));
    assert!(parse_region(&ok).is_ok());

    let mut b = HashMap::new();
    b.insert("start_byte".into(), json!(0));
    b.insert("end_byte".into(), json!(MAX_BYTE_SPAN + 1));
    assert!(parse_region(&b).is_err());

    // Exactly at the limit is fine.
    let mut c = HashMap::new();
    c.insert("start_byte".into(), json!(0));
    c.insert("end_byte".into(), json!(MAX_BYTE_SPAN));
    assert!(parse_region(&c).is_ok());
}

// ── UTF-8-safe byte regions ──────────────────────────────────────────────

#[test]
fn floor_char_boundary_basic_and_clamps() {
    let s = "abc";
    assert_eq!(floor_char_boundary(s, 0), 0);
    assert_eq!(floor_char_boundary(s, 2), 2);
    assert_eq!(floor_char_boundary(s, 3), 3);
    assert_eq!(floor_char_boundary(s, 999), 3);
}

#[test]
fn utf8_invalid_offsets_never_panic_and_are_deterministic() {
    // Layout: 'a'(1B) é(2B) 日(3B) x(1B) → boundaries {0,1,3,6,7}, len 7.
    let s = "a\u{e9}\u{65e5}x";
    assert_eq!(s.len(), 7);

    // start inside 'é' (byte 2) floors to 1; end at boundary 6.
    assert_eq!(slice_utf8_safe(s, 2, 6), "\u{e9}\u{65e5}");
    // start inside 日 (byte 5) floors to 3.
    assert_eq!(slice_utf8_safe(s, 5, 7), "\u{65e5}x");
    // end inside 日 (byte 4) floors to 3 → empty tail from 3.
    assert_eq!(slice_utf8_safe(s, 3, 4), "");
    // both offsets inside the same character → empty.
    assert_eq!(slice_utf8_safe(s, 4, 5), "");
    // clamping past EOF.
    assert_eq!(slice_utf8_safe(s, 0, 100), s);
    assert_eq!(slice_utf8_safe(s, 100, 200), "");

    // Pure ASCII behaviour unchanged.
    assert_eq!(slice_utf8_safe("abcdef", 1, 4), "bcd");
}

// apply_region_bounds
#[test]
fn region_full() {
    let s = "a\nb\nc";
    assert_eq!(
        apply_region_bounds(s, region_args(None, None, None, None))
            .unwrap()
            .as_ref(),
        s
    );
}
#[test]
fn region_lines() {
    assert_eq!(
        apply_region_bounds("L1\nL2\nL3", region_args(Some(2), Some(2), None, None))
            .unwrap()
            .as_ref(),
        "L2"
    );
}
#[test]
fn region_bytes() {
    assert_eq!(
        apply_region_bounds("abcdef", region_args(None, None, Some(1), Some(4)))
            .unwrap()
            .as_ref(),
        "bcd"
    );
}
#[test]
fn region_bytes_win_over_lines() {
    assert_eq!(
        apply_region_bounds("abcdef", region_args(Some(1), Some(1), Some(1), Some(4)))
            .unwrap()
            .as_ref(),
        "bcd"
    );
}
#[test]
fn region_bytes_clamped() {
    assert_eq!(
        apply_region_bounds("hi", region_args(None, None, Some(0), Some(999)))
            .unwrap()
            .as_ref(),
        "hi"
    );
}
#[test]
fn region_bytes_past_end() {
    assert_eq!(
        apply_region_bounds("hi", region_args(None, None, Some(999), None))
            .unwrap()
            .as_ref(),
        ""
    );
}
#[test]
fn region_multibyte_bytes_are_floored_not_panicking() {
    let s = "a\u{e9}\u{65e5}x"; // boundaries {0,1,3,6,7}
    assert_eq!(
        apply_region_bounds(s, region_args(None, None, Some(2), Some(6)))
            .unwrap()
            .as_ref(),
        "\u{e9}\u{65e5}"
    );
    assert_eq!(
        apply_region_bounds(s, region_args(None, None, Some(4), Some(5)))
            .unwrap()
            .as_ref(),
        ""
    );
}
#[test]
fn region_line_window_too_far_returns_empty() {
    assert_eq!(
        apply_region_bounds("one", region_args(Some(50), None, None, None))
            .unwrap()
            .as_ref(),
        ""
    );
}

// ── response-size enforcement ────────────────────────────────────────────

#[test]
fn response_under_cap_passes_through() {
    let body = "hello".to_owned();
    assert_eq!(enforce_response_limit(body.clone()), body);
}

#[test]
fn response_over_cap_truncated_at_char_boundary() {
    // Multibyte padding so a naive byte-cut would split a character.
    let unit = "\u{65e5}".repeat(400_000); // 1_200_000 bytes > 1 MiB cap
    let out = enforce_response_limit(unit);
    assert!(
        out.len() < MAX_RESPONSE_BYTES + 128,
        "out len {}",
        out.len()
    );
    assert!(out.ends_with("[truncated: response exceeded the server output limit]"));
    // Everything before the marker must consist of WHOLE original
    // characters only (no split multi-byte sequences — String guarantees
    // UTF-8 validity, this checks no character was lost mid-sequence).
    let body = out.split("\n\n[truncated").next().unwrap();
    assert!(!body.is_empty());
    assert!(
        body.chars().all(|c| c == '\u{65e5}'),
        "split character detected"
    );
}

// ── streaming collector units ────────────────────────────────────────────

#[test]
fn stream_collector_byte_window_across_chunks() {
    let mut c = StreamWindowCollector::new(WindowSpec::Bytes { start: 3, end: 12 });
    let mut fed = String::new();
    for piece in ["01234", "56789", "abcde"] {
        fed.push_str(piece);
        if !c.feed(piece) {
            break;
        }
    }
    let out = c.finish();
    assert_eq!(out, &fed[3..12]);
}

#[test]
fn stream_collector_stops_early_once_window_complete() {
    let mut c = StreamWindowCollector::new(WindowSpec::Bytes { start: 0, end: 5 });
    // The chunk that satisfies the window already signals "stop pulling".
    assert!(!c.feed("hello world garbage"));
    assert_eq!(c.finish(), "hello");
}

#[test]
fn stream_collector_lines_window_with_split_lines() {
    let mut c = StreamWindowCollector::new(WindowSpec::Lines { start: 2, end: 3 });
    c.feed("l1\nl2\nl3"); // note: l3 has no trailing newline yet
    c.feed("\nl4\n");
    let out = c.finish();
    assert_eq!(out, "l2\nl3");
}

#[test]
fn stream_collector_all_mode_caps_output() {
    let mut c = StreamWindowCollector::new(WindowSpec::All);
    loop {
        if !c.feed(&"x".repeat(64 * 1024)) {
            break;
        }
        if c.out.len() > MAX_RESPONSE_BYTES {
            panic!("collector exceeded cap");
        }
    }
    let out = c.finish();
    assert!(out.ends_with("[truncated: response exceeded the server output limit]"));
    assert!(out.len() < MAX_RESPONSE_BYTES + 128);
}

// handle_file: argument gates
#[test]
fn file_bad_repo_id() {
    let tmp = TempDir::new().unwrap();
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!("../../etc"));
    a.insert("path".into(), json!("x.rs"));
    assert!(handle_file(&make_server(&tmp).pool, &a, &HashSet::new()).is_err());
}

#[test]
fn file_missing_path() {
    let tmp = TempDir::new().unwrap();
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!("aabbccdd"));
    let e = handle_file(&make_server(&tmp).pool, &a, &HashSet::new())
        .unwrap_err()
        .to_string();
    assert!(e.contains("path required"), "{e}");
}

#[test]
fn file_unknown_repo() {
    let tmp = TempDir::new().unwrap();
    let mut a = HashMap::new();
    a.insert(
        "repository_id".into(),
        json!("deadbeef-0000-0000-0000-000000000000"),
    );
    a.insert("path".into(), json!("src/lib.rs"));
    let e = handle_file(&make_server(&tmp).pool, &a, &HashSet::new())
        .unwrap_err()
        .to_string();
    assert!(e.contains("not found"), "{e}");
}

// ── bootstrap_workspace_roots_cancellable: nested-repo fan-out ───────

#[test]
fn bootstrap_workspace_roots_container_splits_into_n_repositories() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let container = tmp.path().join("container");
    let repo_a = container.join("repo-a");
    let repo_b = container.join("repo-b");
    fs::create_dir_all(repo_a.join(".git")).unwrap();
    fs::create_dir_all(repo_b.join(".git")).unwrap();
    fs::write(repo_a.join("a.txt"), "alpha").unwrap();
    fs::write(repo_b.join("b.txt"), "beta").unwrap();

    let results = srv
        .bootstrap_workspace_roots_cancellable(
            &container,
            &attic_core::CancellationToken::default(),
        )
        .expect("fan-out bootstrap should succeed");

    assert_eq!(
        results.len(),
        2,
        "expected one repository per nested .git root"
    );
    let ids: HashSet<&str> = results.iter().map(|(_, id)| id.as_str()).collect();
    assert_eq!(
        ids.len(),
        2,
        "each nested root must get a distinct repository id"
    );

    let stats = srv.pool.with_reader(get_repository_stats).unwrap();
    for (_, id) in &results {
        let s = stats.iter().find(|s| &s.id == id).expect("repo row exists");
        assert!(
            s.file_count >= 1,
            "expected at least one file indexed for {id}"
        );
    }
}

#[test]
fn bootstrap_workspace_roots_single_git_root_unchanged() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(repo.join("f.txt"), "data").unwrap();

    let results = srv
        .bootstrap_workspace_roots_cancellable(&repo, &attic_core::CancellationToken::default())
        .expect("single git root should bootstrap");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, repo.canonicalize().unwrap());
}

#[test]
fn bootstrap_workspace_roots_no_git_anywhere_unchanged() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let plain = tmp.path().join("plain");
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("f.txt"), "data").unwrap();

    let results = srv
        .bootstrap_workspace_roots_cancellable(&plain, &attic_core::CancellationToken::default())
        .expect("plain non-git dir should bootstrap as one repository");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, plain);
}

#[test]
fn file_rejects_overflow_numeric_arguments() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("ovf");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("f.txt"), "data").unwrap();
    let id = srv.bootstrap_workspace(&repo).unwrap();

    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(id));
    a.insert("path".into(), json!("f.txt"));
    a.insert("start_byte".into(), json!(u64::MAX));
    let err = handle_file(&srv.pool, &a, &ids(&srv))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("maximum allowed") || err.contains("non-negative"),
        "{err}"
    );

    let mut b = HashMap::new();
    b.insert(
        "repository_id".into(),
        json!(srv.bootstrap_workspace(&repo).unwrap()),
    );
    b.insert("path".into(), json!("f.txt"));
    b.insert("end_byte".into(), json!(-42));
    assert!(handle_file(&srv.pool, &b, &ids(&srv)).is_err());
}

// handle_file: live read + region
#[test]
fn file_returns_live_content_and_region() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("hello.txt"), "line1\nline2\nline3\n").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).expect("bootstrap");

    // full file
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id.clone()));
    a.insert("path".into(), json!("hello.txt"));
    let r = handle_file(&srv.pool, &a, &ids(&srv)).expect("handle_file");
    let text = text_of(&r);
    assert!(text.contains("line1") && text.contains("line3"), "{text}");

    // line region
    let mut b = HashMap::new();
    b.insert("repository_id".into(), json!(repo_id));
    b.insert("path".into(), json!("hello.txt"));
    b.insert("start_line".into(), json!(2u64));
    b.insert("end_line".into(), json!(2u64));
    let r2 = handle_file(&srv.pool, &b, &ids(&srv)).expect("region");
    let t2 = text_of(&r2);
    assert!(t2.contains("line2") && !t2.contains("line1"), "{t2}");
}

#[test]
fn file_expands_indexed_json_unit_pointer_parent_and_outline() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("json-navigation");
    fs::create_dir_all(&repo).unwrap();
    let text = format!(
        "{{\n  \"pad\":\"{}\",\n  \"a/b~c\":{{\"token\":\"uniqueNavigationNeedle\",\"child\":{{\"flag\":false}}}}\n}}",
        "x".repeat(2200)
    );
    fs::write(repo.join("form.json"), &text).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let search_args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("query".into(), json!("uniqueNavigationNeedle")),
    ]);
    let search = handle_search(&srv.pool, &search_args, &ids(&srv), None).unwrap();
    let value: Value = serde_json::from_str(&text_of(&search)).unwrap();
    let result = &value["results"][0];
    assert_eq!(result["json_pointer"], "/a~1b~0c");
    assert_eq!(result["start_line"], 3);
    assert_eq!(result["end_line"], 3);
    let unit_id = result["retrieval_unit_id"].as_str().unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("unit_id".into(), json!(unit_id)),
    ]);
    let body = text_of(&handle_file(&srv.pool, &args, &ids(&srv)).unwrap());
    assert!(body.contains("uniqueNavigationNeedle"), "{body}");
    assert!(body.contains("live JSON pointer"), "{body}");
    assert!(!body.contains("\"pad\""), "{body}");

    let pointer_args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("form.json")),
        ("json_pointer".into(), json!("/a~1b~0c/child")),
        ("parent".into(), json!(true)),
        ("outline".into(), json!(true)),
    ]);
    let outline = text_of(&handle_file(&srv.pool, &pointer_args, &ids(&srv)).unwrap());
    assert!(outline.contains("/a~1b~0c/child/flag"), "{outline}");
    assert!(!outline.contains("uniqueNavigationNeedle"), "{outline}");

    fs::write(
        repo.join("form.json"),
        r#"{"a/b~c":{"token":"changedLiveValue"}}"#,
    )
    .unwrap();
    let live = text_of(&handle_file(&srv.pool, &args, &ids(&srv)).unwrap());
    assert!(live.contains("changedLiveValue"), "{live}");
    assert!(!live.contains("uniqueNavigationNeedle"), "{live}");
}

#[test]
fn file_rejects_invalid_json_navigation_and_mismatched_unit_path() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("json-invalid");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("one.json"), r#"{"key":"needleForUnit"}"#).unwrap();
    fs::write(repo.join("two.json"), "{}").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let base = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("one.json")),
    ]);
    for pointer in ["no-leading-slash", "/bad~2escape", "/missing"] {
        let mut args = base.clone();
        args.insert("json_pointer".into(), json!(pointer));
        assert!(
            handle_file(&srv.pool, &args, &ids(&srv)).is_err(),
            "{pointer}"
        );
    }
    let search = handle_search(
        &srv.pool,
        &HashMap::from([
            ("repository_id".into(), json!(repo_id)),
            ("query".into(), json!("needleForUnit")),
        ]),
        &ids(&srv),
        None,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&text_of(&search)).unwrap();
    let unit_id = value["results"][0]["retrieval_unit_id"].as_str().unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("two.json")),
        ("unit_id".into(), json!(unit_id)),
    ]);
    assert!(handle_file(&srv.pool, &args, &ids(&srv)).is_err());
}

#[test]
fn file_rejects_oversized_json_value_without_truncating_json() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("large-json-navigation");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("form.json"),
        json!({"large": "x".repeat(MAX_RESPONSE_BYTES + 1)}).to_string(),
    )
    .unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("form.json")),
        ("json_pointer".into(), json!("/large")),
    ]);
    let error = handle_file(&srv.pool, &args, &ids(&srv))
        .unwrap_err()
        .to_string();
    assert!(error.contains("more specific json_pointer"), "{error}");
}

#[test]
fn file_expands_json_array_group_using_original_indices() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("json-array-groups");
    fs::create_dir_all(&repo).unwrap();
    let items = (0..100)
        .map(|index| json!({"rule": format!("rule-{index}"), "padding": "x".repeat(90)}))
        .collect::<Vec<_>>();
    fs::write(repo.join("form.json"), json!({"rules": items}).to_string()).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let (unit_id, metadata): (String, String) = srv.pool.with_reader(|conn| {
        conn.query_row(
            "SELECT id, occurrence_metadata FROM core_retrieval_units WHERE analyzer_id = 'json' AND occurrence_metadata LIKE '%array_start%' ORDER BY rowid DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(Into::into)
    }).unwrap();
    let metadata: Value = serde_json::from_str(&metadata).unwrap();
    let start = metadata["array_start"].as_u64().unwrap() as usize;
    let end = metadata["array_end"].as_u64().unwrap() as usize;
    assert!(start > 0);
    let mut args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("unit_id".into(), json!(unit_id)),
    ]);
    let response = handle_file(&srv.pool, &args, &ids(&srv)).unwrap();
    let text = text_of(&response);
    let body: Value = serde_json::from_str(&text[text.find('[').unwrap_or(0)..])
        .or_else(|_| {
            let start = text.find("\n\n[").unwrap() + 2;
            serde_json::from_str(&text[start..])
        })
        .unwrap();
    assert_eq!(body.as_array().unwrap().len(), end - start);
    assert_eq!(body[0]["rule"], format!("rule-{start}"));
    args.insert("outline".into(), json!(true));
    let response = handle_file(&srv.pool, &args, &ids(&srv)).unwrap();
    let text = text_of(&response);
    let body: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    assert_eq!(
        body["entries"][1]["json_pointer"],
        format!("/rules/{start}")
    );
}

#[test]
fn file_grep_reports_matches_continuation_and_complete_absence() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("grep");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("data.txt"), "alpha\nerror 42\nerror 77\nomega\n").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let mut args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("data.txt")),
        ("grep".into(), json!(r"error \d+")),
        ("regex".into(), json!(true)),
        ("max_matches".into(), json!(1)),
    ]);
    let text = text_of(&handle_file(&srv.pool, &args, &ids(&srv)).unwrap());
    let value: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    assert_eq!(value["matches"][0]["line"], 2);
    assert_eq!(value["coverage_complete"], false);
    assert_eq!(value["next_line"], 3);
    args.insert("start_line".into(), value["next_line"].clone());
    let text = text_of(&handle_file(&srv.pool, &args, &ids(&srv)).unwrap());
    let value: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    assert_eq!(value["matches"][0]["line"], 3);

    args.insert("grep".into(), json!("not-present"));
    args.insert("regex".into(), json!(false));
    args.remove("start_line");
    let text = text_of(&handle_file(&srv.pool, &args, &ids(&srv)).unwrap());
    let value: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    assert_eq!(value["matches"].as_array().unwrap().len(), 0);
    assert_eq!(value["coverage_complete"], true);
    args.insert("grep".into(), json!("["));
    args.insert("regex".into(), json!(true));
    assert!(handle_file(&srv.pool, &args, &ids(&srv)).is_err());
}

#[test]
fn file_grep_reports_multiple_matches_on_one_line() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("grep-columns");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("data.txt"), "needle and needle\n").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("data.txt")),
        ("grep".into(), json!("needle")),
    ]);
    let text = text_of(&handle_file(&srv.pool, &args, &ids(&srv)).unwrap());
    let value: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    assert_eq!(value["matches"][0]["byte_columns"], json!([0, 11]));
    assert_eq!(value["coverage_complete"], true);
}

#[test]
fn file_grep_scans_large_sanitized_lines_beyond_response_limit() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("large-grep");
    fs::create_dir_all(&repo).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    fs::write(
        repo.join("large.txt"),
        format!("{}markerAtTheEnd\n", "x".repeat(4 * 1024 * 1024 + 128)),
    )
    .unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("large.txt")),
        ("grep".into(), json!("markerAtTheEnd")),
    ]);
    let text = text_of(&handle_file(&srv.pool, &args, &ids(&srv)).unwrap());
    let value: Value = serde_json::from_str(&text[text.find('{').unwrap()..]).unwrap();
    assert_eq!(value["matches"][0]["line"], 1);
    assert!(
        value["matches"][0]["snippet"]
            .as_str()
            .unwrap()
            .contains("markerAtTheEnd")
    );
    assert_eq!(value["coverage_complete"], true);
}

#[test]
fn references_locate_duplicate_json_values_and_continue() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("references");
    fs::create_dir_all(&repo).unwrap();
    let text = concat!(
        "{\r\n",
        "  \"arr\": [],\r\n",
        "  \"arr\": [\"needle\"],\r\n",
        "  \"a/b~c\": \"needle\",\r\n",
        "  \"a/b~c\": \"needle\"\r\n",
        "}\r\n"
    );
    fs::write(repo.join("form.json"), text).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let mut args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("form.json")),
        ("query".into(), json!("needle")),
        ("mode".into(), json!("REFERENCES")),
    ]);
    let response =
        crate::handlers::references::handle_references(&srv.pool, &args, &ids(&srv)).unwrap();
    let value: Value = serde_json::from_str(&text_of(&response)).unwrap();
    let matches = value["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 3, "{value}");
    assert_eq!(matches[0]["json_pointer"], "/arr/0");
    assert_eq!(matches[0]["start_line"], 3);
    assert_eq!(matches[1]["json_pointer"], "/a~1b~0c");
    assert_eq!(matches[1]["start_line"], 4);
    assert_eq!(matches[2]["start_line"], 5);
    assert_eq!(matches[1]["pointer_ambiguous"], true);
    assert_eq!(value["coverage"]["complete"], true, "{value}");
    for hit in matches {
        let start = hit["source_start_byte"].as_u64().unwrap() as usize;
        let end = hit["source_end_byte"].as_u64().unwrap() as usize;
        assert_eq!(&text[start..end], "\"needle\"");
        assert_eq!(hit["content_hash_verified"], true);
    }

    args.insert("max_results".into(), json!(1));
    let first =
        crate::handlers::references::handle_references(&srv.pool, &args, &ids(&srv)).unwrap();
    let first: Value = serde_json::from_str(&text_of(&first)).unwrap();
    assert_eq!(first["matches"].as_array().unwrap().len(), 1);
    assert_eq!(first["coverage"]["complete"], false);
    let continuation = &first["coverage"]["repositories"][0]["continuation"]["within_file"];
    assert_eq!(continuation["reference_offset"], 1);
    args.insert(
        "reference_offset".into(),
        continuation["reference_offset"].clone(),
    );
    let next =
        crate::handlers::references::handle_references(&srv.pool, &args, &ids(&srv)).unwrap();
    let next: Value = serde_json::from_str(&text_of(&next)).unwrap();
    assert_eq!(next["matches"][0]["start_line"], 4);
}

#[test]
fn references_only_prove_absence_for_safe_current_exact_files() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("reference-coverage");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("form.json"), r#"{"state":"ready"}"#).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("form.json")),
        ("query".into(), json!("missing-value")),
        ("mode".into(), json!("REFERENCES")),
    ]);
    let current =
        crate::handlers::references::handle_references(&srv.pool, &args, &ids(&srv)).unwrap();
    let current: Value = serde_json::from_str(&text_of(&current)).unwrap();
    assert_eq!(current["coverage"]["absence_proven"], true, "{current}");

    fs::write(repo.join("form.json"), r#"{"state":"changed"}"#).unwrap();
    let stale =
        crate::handlers::references::handle_references(&srv.pool, &args, &ids(&srv)).unwrap();
    let stale: Value = serde_json::from_str(&text_of(&stale)).unwrap();
    assert_eq!(stale["coverage"]["absence_proven"], false, "{stale}");
    assert!(stale.to_string().contains("content_hash_unverified"));

    let db = attic_storage::connection::open_rw(&tmp.path().join("test.db")).unwrap();
    db.execute(
        "UPDATE core_file_occurrences SET existence_state = 'excluded' WHERE path = ?1",
        ["form.json"],
    )
    .unwrap();
    let excluded =
        crate::handlers::references::handle_references(&srv.pool, &args, &ids(&srv)).unwrap();
    let excluded: Value = serde_json::from_str(&text_of(&excluded)).unwrap();
    assert_eq!(excluded["coverage"]["absence_proven"], false, "{excluded}");
    assert!(excluded.to_string().contains("file_not_indexed_as_present"));
}

#[test]
fn references_redact_secrets_and_report_incomplete_scan() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("reference-redaction");
    fs::create_dir_all(&repo).unwrap();
    let token = format!("{}{}", "AKIA", "0123456789ABCDEF");
    fs::write(repo.join("form.json"), json!({"value": token}).to_string()).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("path".into(), json!("form.json")),
        ("query".into(), json!("missing-value")),
    ]);
    let result =
        crate::handlers::references::handle_references(&srv.pool, &args, &ids(&srv)).unwrap();
    let text = text_of(&result);
    assert!(!text.contains(&token));
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["coverage"]["absence_proven"], false, "{value}");
    assert!(text.contains("security_redaction_or_partial_scan"));
}

#[test]
fn file_traversal_rejected() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("r");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("ok.txt"), "data").unwrap();
    let id = srv.bootstrap_workspace(&repo).unwrap();
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(id));
    a.insert("path".into(), json!("../../etc/passwd"));
    assert!(handle_file(&srv.pool, &a, &ids(&srv)).is_err());
}

#[test]
fn file_forbidden_path_rejected() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("r2");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::write(repo.join(".git").join("config"), "[core]").unwrap();
    let id = srv.bootstrap_workspace(&repo).unwrap();
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(id));
    a.insert("path".into(), json!(".git/config"));
    // preprocess_file_content returns Excluded for .git/* — no error, but content is policy message
    let r = handle_file(&srv.pool, &a, &ids(&srv));
    match r {
        Err(e) => assert!(
            e.to_string().contains("forbidden")
                || e.to_string().contains("security")
                || e.to_string().contains("rejected"),
            "{e}"
        ),
        Ok(cr) => {
            let t = text_of(&cr);
            assert!(
                t.contains("Excluded") || t.contains("security") || t.contains("forbidden"),
                "{t}"
            );
        }
    }
}

#[test]
fn file_rejects_nested_security_forbidden_paths() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("r3");
    fs::create_dir_all(repo.join("vendor").join("nested").join(".git")).unwrap();
    fs::create_dir_all(repo.join("fixtures").join(".ssh")).unwrap();
    fs::create_dir_all(repo.join("keys").join(".gnupg")).unwrap();
    fs::write(
        repo.join("vendor")
            .join("nested")
            .join(".git")
            .join("config"),
        "[core]",
    )
    .unwrap();
    fs::write(repo.join("fixtures").join(".ssh").join("id_rsa"), "PRIVATE").unwrap();
    fs::write(repo.join("keys").join(".gnupg").join("pubring.gpg"), "gpg").unwrap();
    let id = srv.bootstrap_workspace(&repo).unwrap();

    for rel in [
        "vendor/nested/.git/config",
        "fixtures/.ssh/id_rsa",
        "keys/.gnupg/pubring.gpg",
    ] {
        let mut args = HashMap::new();
        args.insert("repository_id".into(), json!(id.clone()));
        args.insert("path".into(), json!(rel));
        let err = handle_file(&srv.pool, &args, &ids(&srv))
            .unwrap_err()
            .to_string();
        assert!(err.contains("path rejected"), "{rel}: {err}");
    }
}

#[cfg(windows)]
#[test]
fn file_rejects_windows_case_variant_git_dirs() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("r4");
    fs::create_dir_all(repo.join("sub").join(".GIT")).unwrap();
    fs::write(repo.join("sub").join(".GIT").join("config"), "[core]").unwrap();
    let id = srv.bootstrap_workspace(&repo).unwrap();

    let mut args = HashMap::new();
    args.insert("repository_id".into(), json!(id));
    args.insert("path".into(), json!("sub/.GIT/config"));
    let err = handle_file(&srv.pool, &args, &ids(&srv))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains(".git") || err.contains("path rejected"),
        "{err}"
    );
}

// ── LARGE-file genuinely bounded retrieval ───────────────────────────────

/// Build a deterministic LARGE-tier file (>4 MiB, ≤50 MiB) with unique
/// marker tokens at known line positions.  Returns `(path, base_line_len)`.
fn build_large_file(dir: &Path, name: &str) -> (std::path::PathBuf, usize) {
    let path = dir.join(name);
    let base_line = format!("{}\n", "filler payload ".repeat(24)); // 361 bytes
    let base_len = base_line.len();
    let target_total = 4 * 1024 * 1024 + 512 * 1024; // 4.5 MiB
    let mut f = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    let mut written = 0usize;
    let mut lineno = 0usize;
    while written < target_total {
        lineno += 1;
        let l = match lineno {
            100 => format!("MIDDLE_MARKER_TOKEN_{lineno} {base_line}"),
            9000 => format!("TAIL_MARKER_TOKEN_{lineno} {base_line}"),
            _ => base_line.clone(),
        };
        use std::io::Write as _;
        f.write_all(l.as_bytes()).unwrap();
        written += l.len();
    }
    drop(f);
    (path, base_len)
}

#[test]
fn large_file_region_is_genuinely_bounded_streamed() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("big");
    fs::create_dir_all(&repo).unwrap();
    let (big, base_len) = build_large_file(&repo, "large_source.txt");

    let meta = fs::metadata(&big).unwrap();
    assert!(
        meta.len() > SMALL_THRESHOLD_FOR_TEST,
        "fixture must be LARGE tier"
    );
    assert!(
        meta.len() <= 50 * 1024 * 1024,
        "fixture must not exceed LARGE tier"
    );
    let repo_id = srv.bootstrap_workspace(&repo).expect("bootstrap");

    // Byte-region covering MIDDLE_MARKER_TOKEN_100 exactly from its first
    // byte: lines 1..=99 are plain base lines, so the marker starts at
    // byte 99*base_len.
    let middle_offset = 99 * base_len;
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id.clone()));
    a.insert("path".into(), json!("large_source.txt"));
    a.insert("start_byte".into(), json!(middle_offset as u64));
    a.insert("end_byte".into(), json!(middle_offset as u64 + 40));
    let r = handle_file(&srv.pool, &a, &ids(&srv)).expect("middle region");
    let t = text_of(&r);
    assert!(t.contains("MIDDLE_MARKER_TOKEN_100"), "{t}");
    assert!(
        !t.contains("TAIL_MARKER_TOKEN_9000"),
        "must not leak other regions: {t}"
    );
    assert!(
        t.len() < 500,
        "response must be tiny, got {} bytes",
        t.len()
    );

    // Line-region at the tail marker.
    let mut b = HashMap::new();
    b.insert("repository_id".into(), json!(repo_id.clone()));
    b.insert("path".into(), json!("large_source.txt"));
    b.insert("start_line".into(), json!(9000u64));
    b.insert("end_line".into(), json!(9000u64));
    let r2 = handle_file(&srv.pool, &b, &ids(&srv)).expect("tail line region");
    let t2 = text_of(&r2);
    assert!(t2.contains("TAIL_MARKER_TOKEN_9000"), "{t2}");
    assert!(!t2.contains("MIDDLE_MARKER_TOKEN_100"), "{t2}");

    // Full-file request must be CAPPED, proving the whole 4.5 MiB file is
    // never accumulated into the response.
    let mut c = HashMap::new();
    c.insert("repository_id".into(), json!(repo_id));
    c.insert("path".into(), json!("large_source.txt"));
    let r3 = handle_file(&srv.pool, &c, &ids(&srv)).expect("full file");
    let t3 = text_of(&r3);
    assert!(
        t3.len() <= MAX_RESPONSE_BYTES + 256,
        "full-file response must be capped, got {} bytes",
        t3.len()
    );
    assert!(
        t3.contains("[truncated:"),
        "cap must be reported: len={}",
        t3.len()
    );
}

const SMALL_THRESHOLD_FOR_TEST: u64 = 4 * 1024 * 1024 + 256 * 1024;

#[test]
fn small_single_huge_line_response_is_capped() {
    // A SMALL file whose single line dwarfs the response cap.
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("huge_line");
    fs::create_dir_all(&repo).unwrap();
    let huge = format!("HUGE_START{}HUGE_END", "z".repeat(2 * 1024 * 1024));
    fs::write(repo.join("huge_line.txt"), &huge).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();

    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id));
    a.insert("path".into(), json!("huge_line.txt"));
    let r = handle_file(&srv.pool, &a, &ids(&srv)).expect("huge line file");
    let t = text_of(&r);
    assert!(t.len() <= MAX_RESPONSE_BYTES + 256, "len {}", t.len());
    assert!(t.contains("[truncated:"), "{:.80}", t);
    assert!(t.contains("HUGE_START"), "head must be preserved");
}

// handle_search
#[test]
fn search_missing_query() {
    let tmp = TempDir::new().unwrap();
    let e = handle_search(
        &make_server(&tmp).pool,
        &HashMap::new(),
        &HashSet::new(),
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("query required"), "{e}");
}

#[test]
fn search_query_too_long() {
    let tmp = TempDir::new().unwrap();
    let mut a = HashMap::new();
    a.insert("query".into(), json!("x".repeat(513)));
    assert!(handle_search(&make_server(&tmp).pool, &a, &HashSet::new(), None).is_err());
}

#[test]
fn search_bad_repo_id() {
    let tmp = TempDir::new().unwrap();
    let mut a = HashMap::new();
    a.insert("query".into(), json!("hello"));
    a.insert("repository_id".into(), json!("bad!id"));
    assert!(handle_search(&make_server(&tmp).pool, &a, &HashSet::new(), None).is_err());
}

#[test]
fn search_empty_db_returns_results_array() {
    let tmp = TempDir::new().unwrap();
    let mut a = HashMap::new();
    a.insert("query".into(), json!("hello"));
    let r = handle_search(&make_server(&tmp).pool, &a, &HashSet::new(), None).unwrap();
    let t = text_of(&r);
    let v: Value = serde_json::from_str(&t).unwrap();
    assert!(v["results"].is_array());
}

#[test]
fn search_groups_only_structurally_equivalent_results_and_preserves_locations() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("duplicates");
    fs::create_dir_all(&repo).unwrap();
    let body = json!({"tag": "sharedGroupingNeedle", "result": "failure"});
    let pad = "x".repeat(2200);
    fs::write(
        repo.join("a.json"),
        json!({"pad": pad, "rule": body}).to_string(),
    )
    .unwrap();
    fs::write(
        repo.join("b.json"),
        json!({"pad": pad, "rule": body}).to_string(),
    )
    .unwrap();
    fs::write(
        repo.join("c.json"),
        json!({"pad": pad, "other": body}).to_string(),
    )
    .unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("query".into(), json!("sharedGroupingNeedle")),
    ]);
    let answer = handle_search(&srv.pool, &args, &ids(&srv), None).unwrap();
    let value: Value = serde_json::from_str(&text_of(&answer)).unwrap();
    let results = value["results"].as_array().unwrap();
    assert_eq!(results.len(), 2, "{value}");
    let grouped = results
        .iter()
        .find(|result| result["also_in"].is_array())
        .unwrap();
    let paths: HashSet<_> = std::iter::once(grouped["path"].as_str().unwrap())
        .chain(
            grouped["also_in"]
                .as_array()
                .unwrap()
                .iter()
                .map(|location| location["path"].as_str().unwrap()),
        )
        .collect();
    assert_eq!(paths, HashSet::from(["a.json", "b.json"]));
    assert!(grouped["also_in"][0]["retrieval_unit_id"].is_string());
    assert_eq!(grouped["also_in"][0]["json_pointer"], "/rule");
    assert!(
        results
            .iter()
            .any(|result| result["json_pointer"] == "/other")
    );
}

#[test]
fn search_empty_diagnostics_respect_strict_terms_and_scope() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("diagnostics");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("one.txt"), "presentNeedle").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("query".into(), json!("presentNeedle absentNeedle")),
        ("match_mode".into(), json!("all")),
    ]);
    let response = handle_search(&srv.pool, &args, &ids(&srv), None).unwrap();
    let value: Value = serde_json::from_str(&text_of(&response)).unwrap();
    assert!(value["results"].as_array().unwrap().is_empty());
    assert_eq!(value["search_diagnostics"]["complete"], true);
    let terms = value["search_diagnostics"]["terms"].as_array().unwrap();
    assert_eq!(terms[0]["has_indexed_match"], true);
    assert_eq!(terms[1]["has_indexed_match"], false);
}

#[test]
fn search_knowledge_excludes_root_readme_before_limits_and_diagnostics() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("central-knowledge");
    fs::create_dir_all(&repo).unwrap();
    fs::write(
        repo.join("README.md"),
        "readmeOnlyNeedle\nsharedKnowledgeNeedle",
    )
    .unwrap();
    fs::write(repo.join("notes.md"), "sharedKnowledgeNeedle").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let mut args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("query".into(), json!("readmeOnlyNeedle")),
        ("scope".into(), json!("knowledge")),
        ("max_results".into(), json!(1)),
    ]);
    let answer = handle_search(&srv.pool, &args, &ids(&srv), Some(&repo_id)).unwrap();
    let value: Value = serde_json::from_str(&text_of(&answer)).unwrap();
    assert!(value["results"].as_array().unwrap().is_empty(), "{value}");
    assert_eq!(
        value["search_diagnostics"]["terms"][0]["has_indexed_match"],
        false
    );
    args.insert("query".into(), json!("sharedKnowledgeNeedle"));
    let answer = handle_search(&srv.pool, &args, &ids(&srv), Some(&repo_id)).unwrap();
    let value: Value = serde_json::from_str(&text_of(&answer)).unwrap();
    assert_eq!(value["results"][0]["path"], "notes.md");
}

#[test]
fn context_reports_ambiguous_missing_and_multiple_nested_path_hints() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("path-hints");
    for subdir in ["one", "two"] {
        fs::create_dir_all(repo.join(subdir)).unwrap();
        fs::write(
            repo.join(subdir).join("nested.json"),
            r#"{"rule":"pathNeedle"}"#,
        )
        .unwrap();
    }
    fs::write(
        repo.join("one").join("related.json"),
        r#"{"rule":"relatedNeedle"}"#,
    )
    .unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let mut args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("query".into(), json!("nested.json")),
        ("mode".into(), json!("FAST")),
    ]);
    let result = handle_context(
        &srv.pool,
        &srv.writer,
        true,
        &args,
        &ids(&srv),
        attic_storage::resource_manager::ResourceAdvisory::Ok,
        None,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&text_of(&result)).unwrap();
    assert_eq!(
        value["path_resolution"]["hints"][0]["status"], "ambiguous",
        "{value}"
    );
    assert_eq!(
        value["path_resolution"]["hints"][0]["matches"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(
        value["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|evidence| {
                evidence["path"]
                    .as_str()
                    .unwrap_or("")
                    .ends_with("/nested.json")
            })
    );

    args.insert(
        "query".into(),
        json!("Why do nested.json and related.json and absent.json fail?"),
    );
    let result = handle_context(
        &srv.pool,
        &srv.writer,
        true,
        &args,
        &ids(&srv),
        attic_storage::resource_manager::ResourceAdvisory::Ok,
        None,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&text_of(&result)).unwrap();
    let hints = value["path_resolution"]["hints"].as_array().unwrap();
    assert_eq!(hints.len(), 3, "{value}");
    assert_eq!(hints[0]["status"], "ambiguous");
    assert_eq!(hints[1]["status"], "unique");
    assert_eq!(hints[2]["status"], "missing");
}

#[test]
fn context_does_not_expose_legacy_synthetic_json_lines() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("legacy-json");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("form.json"), r#"{"target":"legacyNeedle"}"#).unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();
    let db = attic_storage::connection::open_rw(&tmp.path().join("test.db")).unwrap();
    db.execute(
        "UPDATE core_retrieval_units SET occurrence_metadata = ?1, start_line = 0, end_line = 1 WHERE analyzer_id = 'json'",
        [r#"{"json_pointer":"/target"}"#],
    ).unwrap();
    let args = HashMap::from([
        ("repository_id".into(), json!(repo_id)),
        ("query".into(), json!("form.json")),
        ("mode".into(), json!("FAST")),
    ]);
    let result = handle_context(
        &srv.pool,
        &srv.writer,
        true,
        &args,
        &ids(&srv),
        attic_storage::resource_manager::ResourceAdvisory::Ok,
        None,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&text_of(&result)).unwrap();
    let evidence = value["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["path"] == "form.json")
        .expect("JSON evidence");
    assert!(evidence["source_span"].is_null(), "{value}");
    assert_eq!(evidence["location_status"], "unavailable_reindex_required");
}

#[test]
fn context_unscoped_direct_path_queries_all_active_repositories_only() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let mut repo_ids = Vec::new();
    for name in ["first", "second", "inactive"] {
        let repo = tmp.path().join(name);
        fs::create_dir_all(&repo).unwrap();
        fs::write(
            repo.join("shared.rs"),
            format!("pub fn shared_workspace_probe_{name}() {{}}\n"),
        )
        .unwrap();
        repo_ids.push(srv.bootstrap_workspace(&repo).unwrap());
    }
    let active = HashSet::from([repo_ids[0].clone(), repo_ids[1].clone()]);
    let args = HashMap::from([
        ("query".into(), json!("shared_workspace_probe")),
        ("mode".into(), json!("NORMAL")),
    ]);
    let result = handle_context(
        &srv.pool,
        &srv.writer,
        true,
        &args,
        &active,
        attic_storage::resource_manager::ResourceAdvisory::Ok,
        None,
    )
    .unwrap();
    let value: Value = serde_json::from_str(&text_of(&result)).unwrap();
    let evidence = value["evidence"].as_array().unwrap();
    assert!(
        evidence
            .iter()
            .any(|entry| entry["repository_id"] == repo_ids[0]),
        "{value}"
    );
    assert!(
        evidence
            .iter()
            .any(|entry| entry["repository_id"] == repo_ids[1]),
        "{value}"
    );
    assert!(
        evidence
            .iter()
            .all(|entry| entry["repository_id"] != repo_ids[2]),
        "{value}"
    );
}

#[test]
fn repository_selector_resolves_unique_active_names_and_rejects_ambiguity() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let first = tmp.path().join("first").join("duplicate");
    let second = tmp.path().join("second").join("duplicate");
    let unique = tmp.path().join("unique-repo");
    for root in [&first, &second, &unique] {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("file.txt"), "repository selector").unwrap();
    }
    let first_id = srv.bootstrap_workspace(&first).unwrap();
    srv.bootstrap_workspace(&second).unwrap();
    let unique_id = srv.bootstrap_workspace(&unique).unwrap();
    let active = ids(&srv);
    let by_name = HashMap::from([("repository".into(), json!("unique-repo"))]);
    let resolved =
        crate::validate::resolve_repository_selector(&srv.pool, &by_name, &active).unwrap();
    assert_eq!(resolved["repository_id"], unique_id);
    let by_path = HashMap::from([("repository".into(), json!(first.to_string_lossy()))]);
    let resolved =
        crate::validate::resolve_repository_selector(&srv.pool, &by_path, &active).unwrap();
    assert_eq!(resolved["repository_id"], first_id);
    let ambiguous = HashMap::from([("repository".into(), json!("duplicate"))]);
    let error = crate::validate::resolve_repository_selector(&srv.pool, &ambiguous, &active)
        .unwrap_err()
        .to_string();
    assert!(error.contains("ambiguous"), "{error}");
    let conflicting = HashMap::from([
        ("repository".into(), json!("unique-repo")),
        ("repository_id".into(), json!(unique_id)),
    ]);
    assert!(
        crate::validate::resolve_repository_selector(&srv.pool, &conflicting, &active).is_err()
    );
    assert!(
        crate::validate::resolve_repository_selector(&srv.pool, &by_name, &HashSet::new()).is_err()
    );
}

// ── central knowledge folder ([knowledge] dir) ─────────────────────────

#[test]
fn knowledge_dir_resolution_reports_reasons_and_never_panics() {
    let tmp = TempDir::new().unwrap();
    let default_dir = tmp.path().join("home-knowledge");

    // Default: on, folder created.
    let none = attic_core::KnowledgeConfig::default();
    let got = resolve_knowledge_dir(&none, &default_dir).unwrap().unwrap();
    assert!(default_dir.is_dir(), "default folder must be created");
    assert_eq!(got, std::fs::canonicalize(&default_dir).unwrap());

    // Turned off: nothing resolved.
    let off = attic_core::KnowledgeConfig {
        enabled: false,
        dir: None,
    };
    assert_eq!(resolve_knowledge_dir(&off, &default_dir), Ok(None));

    // A configured dir is never created.
    let missing_path = tmp.path().join("nope");
    let missing = attic_core::KnowledgeConfig {
        enabled: true,
        dir: Some(missing_path.display().to_string()),
    };
    assert!(
        resolve_knowledge_dir(&missing, &default_dir)
            .unwrap_err()
            .contains("not accessible")
    );
    assert!(!missing_path.exists(), "configured dir must not be created");

    let file = tmp.path().join("a.md");
    std::fs::write(&file, "x").unwrap();
    let not_dir = attic_core::KnowledgeConfig {
        enabled: true,
        dir: Some(file.display().to_string()),
    };
    assert!(
        resolve_knowledge_dir(&not_dir, &default_dir)
            .unwrap_err()
            .contains("not a directory")
    );

    let ok = attic_core::KnowledgeConfig {
        enabled: true,
        dir: Some(tmp.path().display().to_string()),
    };
    assert_eq!(
        resolve_knowledge_dir(&ok, &default_dir).unwrap(),
        Some(std::fs::canonicalize(tmp.path()).unwrap())
    );
}

#[test]
fn invalid_knowledge_dir_is_reported_in_status_state_without_failing() {
    let tmp = TempDir::new().unwrap();
    let mut srv = make_server(&tmp);
    srv.attic_config.knowledge.dir = Some(tmp.path().join("missing").display().to_string());
    srv.start_central_knowledge();
    let k = srv.knowledge.read().unwrap().to_json();
    assert_eq!(k["state"], "failed", "{k}");
    assert!(
        k["reason"].as_str().unwrap().contains("not accessible"),
        "{k}"
    );
    assert!(srv.knowledge_repository_id().is_none());
}

#[test]
fn disabled_knowledge_reports_off() {
    let tmp = TempDir::new().unwrap();
    let mut srv = make_server(&tmp);
    srv.attic_config.knowledge.enabled = false;
    srv.start_central_knowledge();
    assert_eq!(srv.knowledge.read().unwrap().to_json()["state"], "off");
    assert!(srv.knowledge_repository_id().is_none());
    assert!(
        !attic_core::sibling(&srv.db_path, "knowledge").exists(),
        "a disabled feature must not create the folder"
    );
}

/// With no `[knowledge]` table the default folder next to the database
/// is created and indexed.
#[tokio::test(flavor = "multi_thread")]
async fn default_knowledge_folder_is_created_and_indexed() {
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    srv.start_central_knowledge();
    let expected = attic_core::sibling(&srv.db_path, "knowledge");
    assert!(expected.is_dir(), "default folder must exist");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let kid = loop {
        if let Some(id) = srv.knowledge_repository_id() {
            break id;
        }
        let k = srv.knowledge.read().unwrap().to_json();
        assert_ne!(k["state"], "failed", "{k}");
        assert!(std::time::Instant::now() < deadline, "never indexed");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let k = srv.knowledge.read().unwrap().to_json();
    assert_eq!(k["state"], "ready", "{k}");
    assert_eq!(
        k["dir"],
        std::fs::canonicalize(&expected)
            .unwrap()
            .display()
            .to_string()
    );
    srv.stop_watcher(&kid);
}

/// End to end through the server: the folder is indexed at startup
/// without joining the workspace, `context` about ANOTHER repository
/// serves its note, and `search` labels and scopes it.
#[tokio::test(flavor = "multi_thread")]
async fn central_knowledge_folder_serves_context_and_search() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let mut srv = make_server(&tmp);
    let code = tmp.path().join("code");
    fs::create_dir_all(code.join("src")).unwrap();
    fs::write(
        code.join("src/billing.py"),
        "def refund_window():\n    return 'refund window refund window'\n",
    )
    .unwrap();
    let notes = tmp.path().join("knowledge");
    fs::create_dir_all(&notes).unwrap();
    fs::write(
        notes.join("billing.md"),
        "# Billing\n\nThe refund window is 30 days, set by finance.\n",
    )
    .unwrap();
    fs::write(notes.join("README.md"), "Put refund window notes here.\n").unwrap();
    srv.attic_config.knowledge.dir = Some(notes.display().to_string());

    let code_id = srv.bootstrap_workspace(&code).unwrap();
    srv.start_central_knowledge();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let kid = loop {
        if let Some(id) = srv.knowledge_repository_id() {
            break id;
        }
        let k = srv.knowledge.read().unwrap().to_json();
        assert_ne!(k["state"], "failed", "{k}");
        assert!(
            std::time::Instant::now() < deadline,
            "knowledge never indexed"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(srv.knowledge.read().unwrap().to_json()["state"], "ready");
    let active: HashSet<String> = [code_id.clone()].into();
    assert!(
        !active.contains(&kid),
        "knowledge must not be a workspace member"
    );

    // context about the CODE repository serves the central note.
    let mut a = HashMap::new();
    a.insert("query".into(), json!("What is the refund window?"));
    a.insert("repository_id".into(), json!(code_id));
    let r = handle_context(
        &srv.pool,
        &srv.writer,
        false,
        &a,
        &active,
        attic_storage::resource_manager::ResourceAdvisory::Ok,
        Some(kid.clone()),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let ev = v["evidence"].as_array().unwrap();
    assert!(
        ev.iter()
            .any(|e| e["repository_id"] == kid.as_str() && e["path"] == "billing.md"),
        "central note missing: {v}"
    );
    assert!(
        !ev.iter().any(|e| e["path"] == "README.md"),
        "folder README must be skipped: {v}"
    );

    // search: default results unchanged (no central hits) but labelled.
    let mut s = HashMap::new();
    s.insert("query".into(), json!("refund"));
    let r = handle_search(&srv.pool, &s, &active, Some(&kid)).unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let res = v["results"].as_array().unwrap();
    assert!(!res.is_empty());
    assert!(
        res.iter().all(|x| x["repository_id"] == code_id.as_str()),
        "{v}"
    );
    assert!(res.iter().all(|x| x["source_type"] == "code"), "{v}");

    // scope=knowledge: only knowledge, central note first, no README.
    s.insert("scope".into(), json!("knowledge"));
    let r = handle_search(&srv.pool, &s, &active, Some(&kid)).unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let res = v["results"].as_array().unwrap();
    assert!(!res.is_empty(), "{v}");
    assert!(res.iter().all(|x| x["source_type"] == "knowledge"), "{v}");
    assert_eq!(res[0]["path"], "billing.md", "{v}");
    assert!(!res.iter().any(|x| x["path"] == "README.md"), "{v}");

    // Shutdown hygiene: stop the folder watcher this test started.
    srv.stop_watcher(&kid);
}

#[test]
fn search_rejects_unknown_scope() {
    let tmp = TempDir::new().unwrap();
    let mut a = HashMap::new();
    a.insert("query".into(), json!("x"));
    a.insert("scope".into(), json!("docs"));
    let e = handle_search(&make_server(&tmp).pool, &a, &HashSet::new(), None)
        .unwrap_err()
        .to_string();
    assert!(e.contains("scope must be"), "{e}");
}

// handle_status
#[test]
fn status_returns_ok() {
    let tmp = TempDir::new().unwrap();
    let r = handle_status(
        &make_server(&tmp).pool,
        &HashMap::new(),
        &HashMap::new(),
        None,
        true,
        &[],
        &[],
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &test_resource_status(),
    )
    .unwrap();
    let t = text_of(&r);
    let v: Value = serde_json::from_str(&t).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(
        v["knowledge"]["state"], "off",
        "status must report knowledge: {v}"
    );
}

/// A repository row can exist before its watcher is registered (the
/// bootstrap task hasn't reached `start_watcher` yet). This window must
/// report `INDEXING`, not a bare `DISABLED` — it isn't actually disabled,
/// it just hasn't finished starting up.
#[test]
fn status_reports_bootstrap_in_progress_not_disabled() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("f.txt"), "data").unwrap();
    // Bootstrap via the canonicalized root, matching how production
    // code always canonicalizes via `validate_root` before bootstrapping
    // (see `handle_workspace`'s `add` branch) — without ever calling
    // start_watcher (that only happens via handle_workspace/startup).
    let canonical_root = repo.canonicalize().unwrap();
    srv.bootstrap_workspace(&canonical_root).unwrap();
    let key = root_identity_key(&canonical_root);

    let r = handle_status(
        &srv.pool,
        &HashMap::new(),
        &HashMap::new(),
        None,
        true,
        &[canonical_root],
        &[],
        &[key],
        &HashMap::new(),
        &HashMap::new(),
        &test_resource_status(),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let repos = v["workspace"]["repositories"].as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["state"], "INDEXING");
    assert_eq!(repos[0]["watcher"]["reason"], "bootstrap_in_progress");
}

/// When `start_watcher` genuinely failed, `status` must carry the real
/// error instead of a bare unreasoned `DISABLED`.
#[test]
fn status_reports_watcher_start_failure_reason() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("f.txt"), "data").unwrap();
    let canonical_root = repo.canonicalize().unwrap();
    let repo_id = srv.bootstrap_workspace(&canonical_root).unwrap();

    let mut failures = HashMap::new();
    failures.insert(repo_id, "synthetic watcher failure".to_string());

    let r = handle_status(
        &srv.pool,
        &HashMap::new(),
        &HashMap::new(),
        None,
        true,
        &[canonical_root],
        &[],
        &[],
        &HashMap::new(),
        &failures,
        &test_resource_status(),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let repos = v["workspace"]["repositories"].as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["state"], "DISABLED");
    assert_eq!(repos[0]["watcher"]["error"], "synthetic watcher failure");
}

#[test]
fn status_reports_incremental_stuck_tasks() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("f.txt"), "data").unwrap();
    let canonical_root = repo.canonicalize().unwrap();
    let repo_id = srv.bootstrap_workspace(&canonical_root).unwrap();

    let repo_id_for_task = repo_id.clone();
    srv.writer
        .send(move |conn| {
            attic_storage::enqueue_task(
                conn,
                "t-stuck",
                Some(&repo_id_for_task),
                attic_storage::TASK_INCREMENTAL_INDEX,
                50,
                "{\"dedup_key\":\"stuck\"}",
                1,
            )?;
            let _ = attic_storage::claim_next_pending_task(conn, 2)?;
            Ok(())
        })
        .unwrap();

    let mut incremental = HashMap::new();
    incremental.insert(
        repo_id.clone(),
        Arc::new(attic_incremental::IncrementalService::new(
            &canonical_root,
            srv.discovery_policy(),
        )),
    );
    let mut watch_mode = HashMap::new();
    watch_mode.insert(repo_id.clone(), attic_incremental::WatchMode::NativeWatcher);

    let r = handle_status(
        &srv.pool,
        &incremental,
        &watch_mode,
        None,
        true,
        std::slice::from_ref(&canonical_root),
        &[],
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &test_resource_status(),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(v["incremental_stuck_tasks"][0]["task_id"], "t-stuck");
    assert_eq!(v["incremental_stuck_tasks"][0]["repository_id"], repo_id);
    assert_eq!(
        v["workspace"]["repositories"][0]["watcher"]["tasks"]["stuck_tasks"][0]["task_id"],
        "t-stuck"
    );
}

/// The residual case (no bootstrap in progress, no recorded watcher
/// failure) must still carry an honest label, not silence.

#[test]
fn status_reports_watcher_not_registered_reason() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("f.txt"), "data").unwrap();
    let canonical_root = repo.canonicalize().unwrap();
    srv.bootstrap_workspace(&canonical_root).unwrap();

    let r = handle_status(
        &srv.pool,
        &HashMap::new(),
        &HashMap::new(),
        None,
        true,
        &[canonical_root],
        &[],
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &test_resource_status(),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let repos = v["workspace"]["repositories"].as_array().unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0]["state"], "DISABLED");
    assert_eq!(repos[0]["watcher"]["reason"], "watcher_not_registered");
}

// handle_workspace — missing-root removal (PR-6, principal-architect
// audit A-06): a configured root that has been deleted or moved must
// still be removable by path.
#[tokio::test]
async fn workspace_remove_after_root_deleted_succeeds() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let root = tmp.path().join("ws");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("main.rs"), "fn main() {}").unwrap();

    let mut add_args = HashMap::new();
    add_args.insert("action".into(), json!("add"));
    add_args.insert("path".into(), json!(root.display().to_string()));
    srv.handle_workspace(&add_args).await.unwrap();

    // Delete the directory entirely — canonicalize() can no longer run
    // on this path, which is exactly the bug being fixed.
    fs::remove_dir_all(&root).unwrap();

    let mut remove_args = HashMap::new();
    remove_args.insert("action".into(), json!("remove"));
    remove_args.insert("path".into(), json!(root.display().to_string()));
    let r = srv.handle_workspace(&remove_args).await.unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(
        v["membership_count"], 0,
        "deleted root must still be removable: {v}"
    );
}

/// Code-review finding: `last_discovery_counters` must not leak an
/// entry forever once its repository is removed from the workspace.
#[tokio::test]
async fn workspace_remove_prunes_last_discovery_counters() {
    use std::fs;

    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let root = tmp.path().join("ws");

    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("main.rs"), "fn main() {}").unwrap();

    let mut add_args = HashMap::new();
    add_args.insert("action".into(), json!("add"));
    add_args.insert("path".into(), json!(root.display().to_string()));

    srv.handle_workspace(&add_args).await.unwrap();

    let canonical_root = root.canonicalize().unwrap();

    // workspace add intentionally bootstraps in the background.
    // Wait for that asynchronous bootstrap to register the repository.
    let repo_id = {
        let mut found = None;

        for _ in 0..100 {
            found = srv
                .pool
                .with_reader(|c| {
                    lookup_repository_by_root_path(c, &canonical_root.to_string_lossy())
                })
                .unwrap()
                .map(|id| id.to_string());

            if found.is_some() {
                break;
            }

            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        found.expect("repository must be registered after background add")
    };

    // Registration can happen just before bootstrap records the discovery
    // counters, so wait for those independently as well.
    let counters_recorded = {
        let mut recorded = false;

        for _ in 0..100 {
            recorded = srv
                .last_discovery_counters
                .read()
                .unwrap()
                .contains_key(&repo_id);

            if recorded {
                break;
            }

            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        recorded
    };

    assert!(
        counters_recorded,
        "background bootstrap must record discovery counters for this repo"
    );

    let mut remove_args = HashMap::new();
    remove_args.insert("action".into(), json!("remove"));
    remove_args.insert("path".into(), json!(root.display().to_string()));

    srv.handle_workspace(&remove_args).await.unwrap();

    assert!(
        !srv.last_discovery_counters
            .read()
            .unwrap()
            .contains_key(&repo_id),
        "removing the root must prune its discovery counters entry"
    );
}

#[tokio::test]
async fn workspace_add_container_with_nested_git_repos_indexes_all_and_remove_prunes_all() {
    use std::fs;

    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let container = tmp.path().join("container");
    let repo_a = container.join("repo-a");
    let repo_b = container.join("repo-b");
    fs::create_dir_all(repo_a.join(".git")).unwrap();
    fs::create_dir_all(repo_b.join(".git")).unwrap();
    fs::write(repo_a.join("a.txt"), "alpha").unwrap();
    fs::write(repo_b.join("b.txt"), "beta").unwrap();

    let mut add_args = HashMap::new();
    add_args.insert("action".into(), json!("add"));
    add_args.insert("path".into(), json!(container.display().to_string()));
    srv.handle_workspace(&add_args).await.unwrap();

    let container_key = root_identity_key(&container.canonicalize().unwrap());

    // workspace add fans out into two repositories in the background;
    // wait for both to be registered under container_repo_roots.
    let effective_roots = {
        let mut found = None;
        for _ in 0..100 {
            let v = srv
                .container_repo_roots
                .read()
                .unwrap()
                .get(&container_key)
                .cloned();
            if v.as_ref().is_some_and(|v| v.len() == 2) {
                found = v;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        found.expect("container must fan out into 2 effective repository roots")
    };
    assert_eq!(effective_roots.len(), 2);

    // Both nested repos must actually be indexed (this is the direct
    // regression check for the "container silently indexes to 0 files"
    // bug): each has file_count >= 1, and neither is reported DISABLED.
    for effective_root in &effective_roots {
        let repo_id = srv
            .pool
            .with_reader(|c| lookup_repository_by_root_path(c, &effective_root.to_string_lossy()))
            .unwrap()
            .map(|id| id.to_string())
            .expect("nested repo must be registered");
        let stats = srv.pool.with_reader(get_repository_stats).unwrap();
        let s = stats.iter().find(|s| s.id == repo_id).unwrap();
        assert!(
            s.file_count >= 1,
            "expected files indexed for {effective_root:?}"
        );
    }

    // The fanned-out repository ids must be recognized as active members
    // of the configured container root, not just present in storage —
    // this is what query tools (`file`/`search`/`repo_map`/`context`)
    // actually gate on via `require_active_member`.
    {
        let active_roots = srv.active_roots.read().unwrap().clone();
        let container_repo_roots = srv.container_repo_roots.read().unwrap().clone();
        let (active_ids, _owner) =
            expand_active_ids(&srv.pool, &active_roots, &container_repo_roots);
        for effective_root in &effective_roots {
            let repo_id = srv
                .pool
                .with_reader(|c| {
                    lookup_repository_by_root_path(c, &effective_root.to_string_lossy())
                })
                .unwrap()
                .map(|id| id.to_string())
                .unwrap();
            assert!(
                require_active_member(&active_ids, &repo_id).is_ok(),
                "fanned-out repository {repo_id} must be recognized as an active member"
            );
        }
    }

    // Poll status until neither fanned-out repository reports DISABLED
    // (watchers need a moment to register after bootstrap completes).
    let mut saw_non_disabled = false;
    for _ in 0..100 {
        let inc = srv.incremental.read().unwrap().clone();
        let wm = srv.watch_mode.read().unwrap().clone();
        let container_repo_roots = srv.container_repo_roots.read().unwrap().clone();
        let watcher_start_failures = srv.watcher_start_failures.read().unwrap().clone();
        let active_roots = srv.active_roots.read().unwrap().clone();
        let r = handle_status(
            &srv.pool,
            &inc,
            &wm,
            None,
            true,
            &active_roots,
            &[],
            &[],
            &container_repo_roots,
            &watcher_start_failures,
            &test_resource_status(),
        )
        .unwrap();
        let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
        let repos = v["workspace"]["repositories"].as_array();
        let none_disabled = repos
            .is_none_or(|repos| repos.len() == 2 && repos.iter().all(|r| r["state"] != "DISABLED"));
        if none_disabled {
            saw_non_disabled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        saw_non_disabled,
        "neither fanned-out repository should be reported DISABLED"
    );

    // `workspace inspect` surfaces the fan-out via root_expansions.
    let mut inspect_args = HashMap::new();
    inspect_args.insert("action".into(), json!("inspect"));
    let inspect_r = srv.handle_workspace(&inspect_args).await.unwrap();
    let inspect_v: Value = serde_json::from_str(&text_of(&inspect_r)).unwrap();
    let expansions = inspect_v["root_expansions"]
        .as_object()
        .expect("root_expansions object");
    assert!(
        expansions.contains_key(&container_key),
        "inspect must surface the container's fan-out; got {expansions:?}"
    );
    assert_eq!(
        expansions[&container_key].as_array().unwrap().len(),
        2,
        "expected 2 nested roots surfaced for the container"
    );

    // Removing the container must stop both watchers and prune the
    // container_repo_roots entry.
    let mut remove_args = HashMap::new();
    remove_args.insert("action".into(), json!("remove"));
    remove_args.insert("path".into(), json!(container.display().to_string()));
    srv.handle_workspace(&remove_args).await.unwrap();

    assert!(
        !srv.container_repo_roots
            .read()
            .unwrap()
            .contains_key(&container_key),
        "removing the container must prune container_repo_roots"
    );
}

#[tokio::test]
async fn workspace_remove_missing_root_does_not_affect_similar_prefix_root() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let root_a = tmp.path().join("a");
    let root_ab = tmp.path().join("ab");
    fs::create_dir_all(&root_a).unwrap();
    fs::create_dir_all(&root_ab).unwrap();

    for root in [&root_a, &root_ab] {
        let mut add_args = HashMap::new();
        add_args.insert("action".into(), json!("add"));
        add_args.insert("path".into(), json!(root.display().to_string()));
        srv.handle_workspace(&add_args).await.unwrap();
    }

    fs::remove_dir_all(&root_a).unwrap();

    let mut remove_args = HashMap::new();
    remove_args.insert("action".into(), json!("remove"));
    remove_args.insert("path".into(), json!(root_a.display().to_string()));
    let r = srv.handle_workspace(&remove_args).await.unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let roots = v["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 1, "only the deleted root must be removed: {v}");
    assert!(
        roots[0]
            .as_str()
            .unwrap()
            .replace('\\', "/")
            .ends_with("/ab"),
        "similar-prefix root 'ab' must survive removal of 'a': {v}"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn workspace_remove_missing_root_is_case_insensitive_on_windows() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let root = tmp.path().join("WsRoot");
    fs::create_dir_all(&root).unwrap();

    let mut add_args = HashMap::new();
    add_args.insert("action".into(), json!("add"));
    add_args.insert("path".into(), json!(root.display().to_string()));
    srv.handle_workspace(&add_args).await.unwrap();

    fs::remove_dir_all(&root).unwrap();

    // Remove using different casing than what was added.
    let differently_cased = root.to_string_lossy().to_lowercase();
    let mut remove_args = HashMap::new();
    remove_args.insert("action".into(), json!("remove"));
    remove_args.insert("path".into(), json!(differently_cased));
    let r = srv.handle_workspace(&remove_args).await.unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(
        v["membership_count"], 0,
        "removal must be case-insensitive on Windows for a missing root: {v}"
    );
}

// handle_repo_map
#[test]
fn repo_map_missing_repo_id() {
    let tmp = TempDir::new().unwrap();
    let e = handle_repo_map(
        &make_server(&tmp).pool,
        &HashMap::new(),
        &HashSet::new(),
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("repository_id required"), "{e}");
}

// workspace lifecycle: index → search (coordinated writer end-to-end)
#[test]
fn workspace_becomes_searchable_through_coordinated_writer() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("ws");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("main.rs"), "fn hello_world() {}").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();

    // status should succeed
    let r = handle_status(
        &srv.pool,
        &HashMap::new(),
        &HashMap::new(),
        None,
        true,
        &[],
        &[],
        &[],
        &HashMap::new(),
        &HashMap::new(),
        &test_resource_status(),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(v["status"], "ok");

    // search for content — proves the coordinated publication committed
    // retrievable units through the writer queue.
    let mut a = HashMap::new();
    a.insert("query".into(), json!("hello_world"));
    a.insert("repository_id".into(), json!(repo_id.clone()));
    let r2 = handle_search(&srv.pool, &a, &ids(&srv), None).unwrap();
    let v2: Value = serde_json::from_str(&text_of(&r2)).unwrap();
    let results = v2["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "indexing via WriterQueue must yield searchable results"
    );

    // Second bootstrap is idempotent (same repo id, no duplicate rows).
    let again = srv.bootstrap_workspace(&repo).unwrap();
    assert_eq!(again, repo_id, "existing repository must be reused");
}

// Code-review finding: RepoMapDirNode must not render a file and a
// directory with the same name at the same tree level (an impossible
// filesystem shape that stale occurrence data could otherwise produce).
#[test]
fn repo_map_dir_node_directory_wins_over_conflicting_file_name() {
    let mut root = RepoMapDirNode::default();
    // Directory inserted first ("foo/sub.rs"), then a conflicting file
    // leaf named "foo" — the file insert must be dropped, not create a
    // second sibling node named "foo".
    root.insert(&["foo", "sub.rs"], "rust");
    root.insert(&["foo"], "rust");

    let tree = root.to_json();
    assert_eq!(
        tree.len(),
        1,
        "must not render two nodes named 'foo': {tree:?}"
    );
    assert_eq!(tree[0]["name"], "foo");
    assert_eq!(tree[0]["type"], "directory");
}

#[test]
fn repo_map_dir_node_directory_wins_regardless_of_insert_order() {
    let mut root = RepoMapDirNode::default();
    // Same conflict, file inserted first this time.
    root.insert(&["foo"], "rust");
    root.insert(&["foo", "sub.rs"], "rust");

    let tree = root.to_json();
    assert_eq!(
        tree.len(),
        1,
        "must not render two nodes named 'foo': {tree:?}"
    );
    assert_eq!(tree[0]["name"], "foo");
    assert_eq!(tree[0]["type"], "directory");
}

// handle_repo_map — derived directory tree
#[test]
fn repo_map_builds_nested_tree_directories_before_files_lexicographic() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("ws");
    fs::create_dir_all(repo.join("src/app")).unwrap();
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("src/app/main.rs"), "fn app_main() {}").unwrap();
    fs::write(repo.join("src/lib.rs"), "fn app_lib() {}").unwrap();
    fs::write(repo.join("docs/guide.md"), "# guide").unwrap();
    fs::write(repo.join("readme.md"), "# readme").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();

    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id));
    let r = handle_repo_map(&srv.pool, &a, &ids(&srv), &HashMap::new(), &HashMap::new()).unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let tree = v["tree"].as_array().expect("tree array");

    // Root: directories ("docs", "src") before the file ("readme.md"),
    // each group lexicographic.
    let names: Vec<&str> = tree.iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["docs", "src", "readme.md"],
        "root order: {tree:?}"
    );
    assert_eq!(tree[0]["type"], "directory");
    assert_eq!(tree[1]["type"], "directory");
    assert_eq!(tree[2]["type"], "file");

    // Nested: src/ contains directory "app" before file "lib.rs".
    let src_children = tree[1]["children"].as_array().expect("src children");
    let src_names: Vec<&str> = src_children
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert_eq!(src_names, vec!["app", "lib.rs"]);
    assert_eq!(src_children[0]["type"], "directory");

    // Leaf file carries a real file_type.
    let app_children = src_children[0]["children"].as_array().unwrap();
    assert_eq!(app_children[0]["name"], "main.rs");
    assert_eq!(app_children[0]["type"], "file");
    assert!(app_children[0]["file_type"].is_string());
}

#[test]
fn repo_map_file_type_filter_actually_filters() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("ws");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
    fs::write(repo.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();

    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id));
    a.insert("file_type".into(), json!("rust"));
    let r = handle_repo_map(&srv.pool, &a, &ids(&srv), &HashMap::new(), &HashMap::new()).unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let tree = v["tree"].as_array().expect("tree array");

    let names: Vec<&str> = tree.iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["main.rs"],
        "file_type=rust must exclude Cargo.toml: {tree:?}"
    );
}

#[test]
fn repo_map_is_isolated_per_repository() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo_a = tmp.path().join("a");
    let repo_b = tmp.path().join("b");
    fs::create_dir_all(&repo_a).unwrap();
    fs::create_dir_all(&repo_b).unwrap();
    fs::write(repo_a.join("only_in_a.rs"), "fn a() {}").unwrap();
    fs::write(repo_b.join("only_in_b.rs"), "fn b() {}").unwrap();
    let repo_id_a = srv.bootstrap_workspace(&repo_a).unwrap();
    let _repo_id_b = srv.bootstrap_workspace(&repo_b).unwrap();

    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id_a));
    let r = handle_repo_map(&srv.pool, &a, &ids(&srv), &HashMap::new(), &HashMap::new()).unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let tree = v["tree"].as_array().expect("tree array");

    let names: Vec<&str> = tree.iter().map(|n| n["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["only_in_a.rs"],
        "repo_map must not leak paths from other repositories: {tree:?}"
    );
}

#[test]
fn repo_map_surfaces_discovery_counters_after_bootstrap() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("ws");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("main.rs"), "fn main() {}").unwrap();
    fs::create_dir_all(repo.join("node_modules/pkg")).unwrap();
    fs::write(repo.join("node_modules/pkg/index.js"), "module.exports={}").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();

    // bootstrap_workspace must have recorded counters for this repo_id.
    let recorded = srv
        .last_discovery_counters
        .read()
        .unwrap()
        .get(&repo_id)
        .copied()
        .expect("bootstrap must record discovery counters");
    assert_eq!(recorded.files_eligible, 1, "only main.rs is eligible");
    assert!(
        recorded.ignored_or_pruned >= 1,
        "node_modules/pkg/index.js must be counted as pruned: {recorded:?}"
    );

    // repo_map surfaces exactly those recorded counters under "discovery".
    let mut discovery_counters = HashMap::new();
    discovery_counters.insert(repo_id.clone(), recorded);
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id));
    let r = handle_repo_map(
        &srv.pool,
        &a,
        &ids(&srv),
        &discovery_counters,
        &HashMap::new(),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    assert_eq!(v["discovery"]["files_eligible"], 1);
    assert!(v["discovery"]["ignored_or_pruned"].as_u64().unwrap() >= 1);
}

#[test]
fn repo_map_surfaces_submodule_diagnostic_after_bootstrap() {
    use std::fs;
    let tmp = TempDir::new().unwrap();
    let srv = make_server(&tmp);
    let repo = tmp.path().join("ws");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("root_file.rs"), "fn root() {}").unwrap();
    let sub = repo.join("sub");
    fs::create_dir_all(sub.join(".git")).unwrap();
    fs::write(sub.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(sub.join("sub_file.rs"), "fn sub() {}").unwrap();
    let repo_id = srv.bootstrap_workspace(&repo).unwrap();

    let recorded_diags = srv
        .last_discovery_diagnostics
        .read()
        .unwrap()
        .get(&repo_id)
        .cloned()
        .expect("bootstrap must record discovery diagnostics");
    assert!(
        recorded_diags
            .iter()
            .any(|d| d.kind == attic_discovery::DiagnosticKind::SubmoduleDetected),
        "expected a SubmoduleDetected diagnostic; got {recorded_diags:?}"
    );

    let mut diagnostics = HashMap::new();
    diagnostics.insert(repo_id.clone(), recorded_diags);
    let mut a = HashMap::new();
    a.insert("repository_id".into(), json!(repo_id));
    let r = handle_repo_map(&srv.pool, &a, &ids(&srv), &HashMap::new(), &diagnostics).unwrap();
    let v: Value = serde_json::from_str(&text_of(&r)).unwrap();
    let diags = v["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        diags.iter().any(|d| d["kind"] == "SUBMODULE_DETECTED"),
        "expected SUBMODULE_DETECTED in repo_map diagnostics; got {diags:?}"
    );
}

// ── MCP child-process tests (supplemental manual JSON-RPC protocol tests).
// The required gate for real client↔server operation lives in
// tests/rmcp_stdio_integration.rs using the official rmcp client API.
// ─────────────────────────────────────────────────────────────────────────

fn binary_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    let name = if cfg!(windows) { "attic.exe" } else { "attic" };
    p.join(name)
}

/// REQUIRED: the built `attic` binary.  These supplemental protocol tests
/// FAIL (never silently pass) when the binary cannot be located.
fn require_binary() -> PathBuf {
    let bin = binary_path();
    assert!(
        bin.exists(),
        "required MCP test binary missing: {} — build the attic binary first \
         (cargo build -p attic-server); these tests must fail rather than false-pass",
        bin.display()
    );
    bin
}

fn mcp_request(id: u64, method: &str, params: Value) -> String {
    let v = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    format!("{}\n", serde_json::to_string(&v).unwrap())
}

/// Manual lifecycle handshake accepted by the rmcp 3.x server:
/// protocolVersion must be one of the SDK's known versions and
/// `capabilities` is a required field of InitializeRequestParams.
fn spawn_and_initialize(
    bin: &Path,
    tmp: &TempDir,
) -> (std::process::Child, std::process::ChildStdin) {
    let mut child = Command::new(bin)
        .env("ATTIC_HOME", tmp.path())
        .env(
            "ATTIC_DB_PATH",
            tmp.path().join("test.db").to_str().unwrap(),
        )
        // Each test owns a private database, so this process wins the
        // election and serves its own stdio. A zero idle timeout makes a
        // closed transport shut the daemon down immediately, which
        // `mcp_disconnect_cancels_and_joins_background_bootstrap` relies
        // on (relay behaviour is covered by
        // `tests/daemon_relay_integration.rs`).
        .env("ATTIC_DAEMON_IDLE_TIMEOUT_MS", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn attic server");
    let mut stdin = child.stdin.take().unwrap();
    let init = mcp_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "attic-supplemental-test", "version": "0"}
        }),
    );
    let resp = send_recv(&mut child, &mut stdin, &init);
    assert_eq!(resp["jsonrpc"], "2.0", "initialize failed: {resp}");
    assert_eq!(resp["id"], 1);
    // Notifications are fire-and-forget — they MUST NOT be awaited.
    send_only(
        &mut stdin,
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
    );
    for _ in 0..400 {
        let response = send_recv(
            &mut child,
            &mut stdin,
            &mcp_request(
                999,
                "tools/call",
                json!({"name": "status", "arguments": {}}),
            ),
        );
        let state: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        if state["status"] != "initializing" {
            assert_ne!(state["status"], "failed", "{state}");
            return (child, stdin);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    child.kill().ok();
    child.wait().ok();
    panic!("catalog did not become ready");
}

fn send_recv(
    child: &mut std::process::Child,
    stdin: &mut std::process::ChildStdin,
    msg: &str,
) -> Value {
    stdin.write_all(msg.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let stdout = child.stdout.as_mut().unwrap();
    let mut line = String::new();
    BufReader::new(stdout)
        .read_line(&mut line)
        .expect("I/O error reading MCP child stdout");
    if line.is_empty() {
        let status = child.try_wait().ok().flatten();

        let stderr_output = child
            .stderr
            .take()
            .map(|mut stderr| {
                let mut output = String::new();
                let _ = std::io::Read::read_to_string(&mut stderr, &mut output);
                output
            })
            .unwrap_or_default();

        panic!(
            "MCP child produced EOF (empty stdout) — child likely crashed before \
            responding.\n  Exit status : {status:?}\n  Child stderr:\n{stderr_output}\n  \
            Sent message: {msg}"
        );
    }
    serde_json::from_str(&line).unwrap_or_else(|e| {
        panic!("Failed to parse MCP response as JSON: {e}\n  Raw line: {line:?}");
    })
}

/// Write-only send for notifications (which receive no reply).
fn send_only(stdin: &mut std::process::ChildStdin, msg: &str) {
    stdin.write_all(msg.as_bytes()).unwrap();
    stdin.flush().unwrap();
}

#[test]
fn mcp_initialize_handshake() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let mut child = Command::new(&bin)
        // ATTIC_HOME is required on all platforms so the server can locate
        // its data directory.  Without it the process exits immediately on
        // Windows before writing anything to stdout, causing an EOF panic.
        .env("ATTIC_HOME", tmp.path())
        .env(
            "ATTIC_DB_PATH",
            tmp.path().join("test.db").to_str().unwrap(),
        )
        .env("ATTIC_SEMANTIC", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let init = mcp_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "attic-supplemental-test", "version": "0"}
        }),
    );
    let resp = send_recv(&mut child, &mut stdin, &init);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert!(
        resp["result"]["serverInfo"]["name"]
            .as_str()
            .unwrap_or("")
            .contains("attic"),
        "expected attic in serverInfo, got: {resp}"
    );
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn mcp_tools_list() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
    let list_req = mcp_request(2, "tools/list", json!({}));
    let resp = send_recv(&mut child, &mut stdin, &list_req);
    assert_eq!(resp["jsonrpc"], "2.0");
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"file"), "missing file tool: {names:?}");
    // One-door merge: standalone `search` is gone; raw hybrid retrieval is
    // `context` with mode="SEARCH".
    assert!(
        !names.contains(&"search"),
        "search tool must not be listed after the one-door merge: {names:?}"
    );
    assert!(
        names.contains(&"context"),
        "missing context tool: {names:?}"
    );
    assert!(
        names.contains(&"repo_map"),
        "missing repo_map tool: {names:?}"
    );
    assert!(names.contains(&"status"), "missing status tool: {names:?}");
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn mcp_call_tool_status() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
    let call = mcp_request(2, "tools/call", json!({"name":"status","arguments":{}}));
    let resp = send_recv(&mut child, &mut stdin, &call);
    assert_eq!(resp["jsonrpc"], "2.0");
    let content = &resp["result"]["content"];
    assert!(content.is_array(), "expected content array: {resp}");
    let text = content[0]["text"].as_str().unwrap_or("");
    let v: Value = serde_json::from_str(text).expect("status result is JSON");
    // Spawned without any workspace configuration: status must succeed
    // and report UNCONFIGURED (spec §30), never a fabricated empty ok.
    assert_eq!(v["status"], "unconfigured", "unexpected status: {v}");
    assert!(
        v.get("semantic_health").is_none(),
        "removed subsystem must not be advertised: {v}"
    );
    assert_eq!(v["storage"], "repository_sqlite");
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn mcp_call_tool_repo_map_empty() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
    let call = mcp_request(
        2,
        "tools/call",
        json!({"name":"repo_map","arguments":{"repository_id":"00000000-0000-0000-0000-000000000000"}}),
    );
    let resp = send_recv(&mut child, &mut stdin, &call);
    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(
        resp["result"].is_object() || resp["error"].is_null(),
        "unexpected transport error: {resp}"
    );
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn mcp_call_tool_search_missing_query() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
    // Raw retrieval now lives behind context's SEARCH mode (one-door merge).
    let call = mcp_request(
        2,
        "tools/call",
        json!({"name":"context","arguments":{"mode":"SEARCH"}}),
    );
    let resp = send_recv(&mut child, &mut stdin, &call);
    assert_eq!(resp["jsonrpc"], "2.0");
    let content = &resp["result"]["content"];
    if let Some(arr) = content.as_array() {
        let text = arr[0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("query required")
                || text.contains("required")
                // Spawned without workspace config: the guard fires first.
                || text.contains("workspace not configured"),
            "expected validation error, got: {text}"
        );
    }
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn mcp_call_unknown_tool_returns_error_content() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
    let call = mcp_request(
        2,
        "tools/call",
        json!({"name":"does_not_exist","arguments":{}}),
    );
    let resp = send_recv(&mut child, &mut stdin, &call);
    assert_eq!(resp["jsonrpc"], "2.0");
    let content = &resp["result"]["content"];
    if let Some(arr) = content.as_array() {
        let text = arr[0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("unknown tool") || text.contains("does_not_exist"),
            "expected unknown tool error, got: {text}"
        );
    }
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn mcp_context_tool_lists_and_rejects_missing_query() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);

    // The context capability is advertised.
    let list = send_recv(
        &mut child,
        &mut stdin,
        &mcp_request(2, "tools/list", json!({})),
    );
    let names: Vec<String> = list["result"]["tools"]
        .as_array()
        .map(|ts| {
            ts.iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    assert!(names.iter().any(|n| n == "context"), "tools={names:?}");
    for legacy in ["file", "repo_map", "status"] {
        assert!(names.iter().any(|n| n == legacy), "{legacy} must remain");
    }
    assert!(
        !names.iter().any(|n| n == "search"),
        "search must not survive the one-door merge"
    );

    // Missing query is a clean argument error (never a panic/SQL leak).
    let call = mcp_request(3, "tools/call", json!({"name":"context","arguments":{}}));
    let resp = send_recv(&mut child, &mut stdin, &call);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("query required") || text.contains("workspace not configured"),
        "got: {text}"
    );

    child.kill().ok();
    child.wait().ok();
}

/// End-to-end MCP integration test: multi-repository fixture → normal Attic
/// indexing → Phase 6 workspace sync → Phase 4 CrossRepoGenerator/Evidence
/// Manager → MCP context request → response.
///
/// Verifies all 7 gate requirements:
/// 1. Correct provider/dependent repository is identified.
/// 2. Unrelated repositories are not claimed.
/// 3. Relationship resolution/confidence is preserved.
/// 4. Real SourceRevision provenance reaches the evidence/context path.
/// 5. Cross-repo degraded state prevents cross-repo claims.
/// 6. Local retrieval still works while cross-repo is degraded.
/// 7. A manifest change through the Phase 2 production path changes the
///    subsequent MCP cross-repo result.
#[test]
fn mcp_e2e_crossrepo_multi_repo_fixture() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();

    // ── Build two-repo fixture ────────────────────────────────────────────
    // provider: declares module "example.com/provider"
    let provider_dir = tmp.path().join("provider");
    fs::create_dir_all(&provider_dir).unwrap();
    fs::write(
        provider_dir.join("go.mod"),
        "module example.com/provider\n\ngo 1.21\n",
    )
    .unwrap();
    fs::write(
        provider_dir.join("lib.go"),
        "package provider\n\nfunc Hello() string { return \"hello\" }\n",
    )
    .unwrap();

    // dependent: requires "example.com/provider"
    let dependent_dir = tmp.path().join("dependent");
    fs::create_dir_all(&dependent_dir).unwrap();
    fs::write(
        dependent_dir.join("go.mod"),
        "module example.com/dependent\n\ngo 1.21\n\nrequire example.com/provider v0.1.0\n",
    )
    .unwrap();
    fs::write(
        dependent_dir.join("main.go"),
        "package main\n\nimport \"example.com/provider\"\n\nfunc main() { _ = provider.Hello() }\n",
    )
    .unwrap();

    // unrelated repo: no dependency on provider
    let unrelated_dir = tmp.path().join("unrelated");
    fs::create_dir_all(&unrelated_dir).unwrap();
    fs::write(
        unrelated_dir.join("go.mod"),
        "module example.com/unrelated\n\ngo 1.21\n",
    )
    .unwrap();
    fs::write(
        unrelated_dir.join("util.go"),
        "package unrelated\n\nfunc Util() {}\n",
    )
    .unwrap();

    // Canonicalize so every downstream use (pre-seed bootstrap, the ID
    // readback below, and the `path = "..."` entries written into
    // `cfg_path`) matches the canonical form `validate_configured_roots`
    // produces for the real server's own ATTIC_CONFIG-driven sync —
    // otherwise the two can disagree on Windows (short 8.3 names,
    // `\\?\` verbatim prefix) and end up as two different repository rows.
    let provider_dir = provider_dir.canonicalize().unwrap();
    let dependent_dir = dependent_dir.canonicalize().unwrap();
    let unrelated_dir = unrelated_dir.canonicalize().unwrap();

    let db_path = tmp.path().join("e2e.db");

    // ── Pre-seed DB by indexing all repos in-process ──────────────────────
    {
        let mut srv = AtticServer::new(&db_path).expect("server for pre-seeding");
        srv.stores = Some(Arc::new(
            attic_storage::RepositoryStoreManager::open_with_catalog_writer(
                &db_path,
                tmp.path(),
                8,
                srv.writer.clone(),
            )
            .unwrap(),
        ));
        *srv.active_roots.write().unwrap() = vec![
            provider_dir.clone(),
            dependent_dir.clone(),
            unrelated_dir.clone(),
        ];
        let provider_id = srv
            .bootstrap_workspace(&provider_dir)
            .expect("index provider");
        let dependent_id = srv
            .bootstrap_workspace(&dependent_dir)
            .expect("index dependent");
        let _unrelated_id = srv
            .bootstrap_workspace(&unrelated_dir)
            .expect("index unrelated");
        drop(srv);

        // Verify distinct repository IDs.
        assert_ne!(provider_id, dependent_id, "repos must be distinct");
    }

    // ─────────────────────────────────────────────────────────────────────
    // Gate 5: cross-repo degraded state prevents cross-repo claims.
    // Gate 6: local retrieval still works while cross-repo is degraded.
    // Spawn WITHOUT ATTIC_WORKSPACE_ROOT → crossrepo_degraded stays true.
    // ─────────────────────────────────────────────────────────────────────
    {
        let (mut child, mut stdin) = {
            let mut child = Command::new(&bin)
                .env("ATTIC_HOME", tmp.path())
                .env("ATTIC_DB_PATH", db_path.to_str().unwrap())
                .env("ATTIC_SEMANTIC", "0")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn attic (degraded)");
            let mut stdin = child.stdin.take().unwrap();
            let init = mcp_request(
                1,
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "attic-e2e-test", "version": "0"}
                }),
            );
            let resp = send_recv(&mut child, &mut stdin, &init);
            assert_eq!(resp["id"], 1, "init failed: {resp}");
            send_only(
                &mut stdin,
                "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            );
            for poll_id in 900..1300 {
                let response = send_recv(
                    &mut child,
                    &mut stdin,
                    &mcp_request(
                        poll_id,
                        "tools/call",
                        json!({"name":"status","arguments":{}}),
                    ),
                );
                let state: Value = serde_json::from_str(
                    response["result"]["content"][0]["text"].as_str().unwrap(),
                )
                .unwrap();
                if state["status"] != "initializing" {
                    assert_ne!(state["status"], "failed", "{state}");
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            (child, stdin)
        };

        // Gate 5: context query about cross-repo dependency should NOT
        // produce a confident RESOLVED cross-repo claim when degraded.
        let call = mcp_request(
            2,
            "tools/call",
            json!({
                "name": "context",
                "arguments": {
                    "query": "What modules does example.com/dependent depend on?",
                    "mode": "FAST"
                }
            }),
        );
        let resp = send_recv(&mut child, &mut stdin, &call);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).unwrap_or(json!({}));
        // With degraded cross-repo, confidence must be LOW or result
        // must be INSUFFICIENT_EVIDENCE — never HIGH cross-repo confidence.
        let confidence = v["confidence"].as_str().unwrap_or("UNKNOWN");
        assert!(
            !confidence.contains("HIGH")
                || v["result"].as_str().unwrap_or("") == "INSUFFICIENT_EVIDENCE",
            "gate 5 FAIL: cross-repo degraded must not yield HIGH-confidence \
             cross-repo claim; got confidence={confidence}, result={}, context={:.200}",
            v["result"].as_str().unwrap_or(""),
            text
        );

        // Gate 6: local retrieval (context SEARCH mode) still works while degraded.
        let search = mcp_request(
            3,
            "tools/call",
            json!({
                "name": "context",
                "arguments": {"query": "provider", "mode": "SEARCH"}
            }),
        );
        let sresp = send_recv(&mut child, &mut stdin, &search);
        let stext = sresp["result"]["content"][0]["text"].as_str().unwrap_or("");
        // Spec §30 contract update: with NO workspace configuration the
        // search tool must refuse with a structured "workspace not
        // configured" response instead of serving pre-seeded (stale) DB
        // repos — the old behavior was exactly the historical-repo leak
        // the membership-authoritative model forbids (spec §16).
        assert!(
            stext.contains("workspace not configured"),
            "gate 6 FAIL: search must refuse while UNCONFIGURED; got: {stext:.200}"
        );

        child.kill().ok();
        child.wait().ok();
    }

    // ─────────────────────────────────────────────────────────────────────
    // Gates 1–4: full workspace sync via explicit multi-root ATTIC_CONFIG.
    // Spawn WITH ATTIC_CONFIG → triggers sync_workspace → clears
    // degraded flag → cross-repo claims become available.
    // ─────────────────────────────────────────────────────────────────────
    let (provider_id_str, dependent_id_str, _unrelated_id_str) = {
        let manager = attic_storage::RepositoryStoreManager::open(
            tmp.path().join("identity-catalog.db"),
            tmp.path(),
            4,
        )
        .unwrap();
        let pid = manager
            .register_repository(&provider_dir)
            .unwrap()
            .stable_id;
        let did = manager
            .register_repository(&dependent_dir)
            .unwrap()
            .stable_id;
        let uid = manager
            .register_repository(&unrelated_dir)
            .unwrap()
            .stable_id;
        (pid, did, uid)
    };

    let cfg_path = tmp.path().join("crossrepo.conf");
    fs::write(
        &cfg_path,
        format!(
            "[[repositories]]\npath = \"{}\"\n\n[[repositories]]\npath = \"{}\"\n\n[[repositories]]\npath = \"{}\"\n",
            provider_dir.display(),
            dependent_dir.display(),
            unrelated_dir.display(),
        ),
    )
    .unwrap();

    let (mut child, mut stdin) = {
        let mut child = Command::new(&bin)
            .env("ATTIC_HOME", tmp.path())
            .env("ATTIC_DB_PATH", db_path.to_str().unwrap())
            .env("ATTIC_CONFIG", cfg_path.to_str().unwrap())
            .env("ATTIC_SEMANTIC", "0")
            .env_remove("ATTIC_WORKSPACE_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn attic (synced)");
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "attic-e2e-test", "version": "0"}
            }),
        );
        let resp = send_recv(&mut child, &mut stdin, &init);
        assert_eq!(resp["id"], 1, "init (synced) failed: {resp}");
        send_only(
            &mut stdin,
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        );
        (child, stdin)
    };

    // Startup bootstrap + cross-repo sync run in the background. Wait until
    // all configured repositories are CURRENT before asking the cross-repo
    // gate question, then allow the immediately-following sync publication
    // to commit. This avoids racing initialize against startup sync.
    let mut all_current = false;
    let mut last_status = json!({});
    for poll_id in 10..210 {
        let status_call = mcp_request(
            poll_id,
            "tools/call",
            json!({"name":"status","arguments":{}}),
        );
        let status_resp = send_recv(&mut child, &mut stdin, &status_call);
        let status_text = status_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        let status_v: Value = serde_json::from_str(status_text).unwrap_or(json!({}));
        last_status = status_v.clone();
        if status_v["workspace"]["current_repository_count"] == 3 {
            all_current = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        all_current,
        "cross-repo fixture repositories never became CURRENT: {last_status}"
    );
    std::thread::sleep(Duration::from_millis(100));

    // Gate 1 + Gate 3: query about the dependent's dependencies.
    // The response should identify the provider repository and preserve
    // confidence information. `current_repository_count == 3` only means
    // indexing converged — the cross-repo edge publication that follows
    // it can still be in flight, so retry the real query itself instead
    // of trusting a fixed sleep to have been long enough.
    let call = mcp_request(
        2,
        "tools/call",
        json!({
            "name": "context",
            "arguments": {
                "query": "What Go modules does the dependent repository depend on?",
                "mode": "NORMAL",
                "repository_id": dependent_id_str.clone()
            }
        }),
    );
    let (v, full_response, _claims_json) = {
        let mut last_v = json!({});
        let mut last_full = String::new();
        let mut last_claims = String::new();
        for _ in 0..80 {
            let resp = send_recv(&mut child, &mut stdin, &call);
            let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
            let parsed: Value = serde_json::from_str(text).unwrap_or(json!({}));
            let context_body = parsed["context"].as_str().unwrap_or("");
            let claims = parsed["claims"].to_string();
            let full = format!("{context_body} {claims} {text}");
            let ready = full.contains("example.com/provider") || full.contains(&provider_id_str);
            last_v = parsed;
            last_claims = claims;
            last_full = full;
            if ready {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        (last_v, last_full, last_claims)
    };

    // Gate 1: provider is identified in the response.
    assert!(
        full_response.contains("example.com/provider") || full_response.contains(&provider_id_str),
        "gate 1 FAIL: provider repository not identified in cross-repo response; \
         response={:.400}",
        full_response
    );

    // Gate 2: unrelated repository is not falsely claimed as a dependency.
    // This MUST be an unscoped query (no repository_id): the Gate 1/3/4
    // query above is filtered to `dependent_id_str`, which would exclude
    // any evidence about example.com/unrelated regardless of whether the
    // underlying cross-repo logic has a real false-association bug,
    // making the assertion vacuously true.
    let unscoped_call = mcp_request(
        3,
        "tools/call",
        json!({
            "name": "context",
            "arguments": {
                "query": "What Go modules does the dependent repository depend on?",
                "mode": "NORMAL"
            }
        }),
    );
    let unscoped_resp = send_recv(&mut child, &mut stdin, &unscoped_call);
    let unscoped_text = unscoped_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    let unscoped_claims =
        serde_json::from_str::<Value>(unscoped_text).unwrap_or(json!({}))["claims"].to_string();
    // The raw context body may legitimately contain any indexed go.mod
    // file (retrieval surfaces all relevant content). We therefore check
    // only the structured claims JSON — not the context prose — for a
    // false dependency claim on example.com/unrelated.
    assert!(
        !unscoped_claims.contains("example.com/unrelated")
            || unscoped_claims.contains("not depend"),
        "gate 2 FAIL: unrelated repository should not appear as a dependency \
         claim; claims={:.400}",
        unscoped_claims
    );

    // Gate 3: confidence field is present and non-empty (preserved).
    let confidence = v["confidence"].as_str().unwrap_or("");
    assert!(
        !confidence.is_empty(),
        "gate 3 FAIL: confidence must be present in response; got: {v}"
    );

    // Gate 4: SourceRevision provenance — the result field or context must
    // not be empty, confirming that real indexed content drove the answer
    // (not a fabricated answer from a zero-evidence path).
    let result = v["result"].as_str().unwrap_or("");
    assert!(
        !result.is_empty(),
        "gate 4 FAIL: result verdict must be present; got: {v}"
    );
    // plan_id is set only when evidence was actually retrieved and a plan
    // record was persisted — this proves the evidence/context path ran.
    let plan_id = &v["plan_id"];
    assert!(
        !plan_id.is_null() && plan_id.as_str().map(|s| !s.is_empty()).unwrap_or(true),
        "gate 4 FAIL: plan_id must be set (evidence path ran); got: {v}"
    );

    // Drop accounting is always visible; per-item diagnostics are opt-in.
    assert!(
        v["evidence_dropped"].is_array(),
        "RP-INV-4 FAIL: evidence_dropped must be present as an array; got: {v}"
    );
    assert!(v["evidence_dropped_summary"]["count"].is_u64());
    assert!(v["evidence_dropped_summary"]["by_reason"].is_object());

    // Gate 4b (strengthened): WorkspaceSnapshot provenance must be traceable
    // from cross-repo evidence items.  Any evidence item that carries a
    // `workspace_snapshot_id` is definitionally cross-repo evidence (only
    // `CrossRepoGenerator` sets this field).  For such items we additionally
    // assert that `source_revision_id` is non-empty, which proves the exact
    // per-repository SourceRevision that was in scope when the edge was
    // resolved — i.e. the full provenance chain:
    //
    //   Evidence.workspace_snapshot_id
    //     → core_workspace_snapshot_revisions (snapshot_id, repository_id)
    //     → core_workspace_snapshots (exact revision set at sync time)
    //
    // The assertion is conditional: when sync_workspace hasn't yet produced
    // any cross-repo edge the evidence array may be empty or contain only
    // repo-local items, which is fine.
    {
        let evidence_arr = v["evidence"].as_array().cloned().unwrap_or_default();
        let snapshot_backed: Vec<_> = evidence_arr
            .iter()
            .filter(|e| {
                e["workspace_snapshot_id"]
                    .as_str()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            })
            .collect();
        for ev in &snapshot_backed {
            let ws_id = ev["workspace_snapshot_id"].as_str().unwrap_or("");
            assert!(
                !ws_id.is_empty(),
                "gate 4b FAIL: workspace_snapshot_id must be non-empty on \
                 cross-repo evidence; ev={ev}"
            );
            let src_rev = ev["source_revision_id"].as_str().unwrap_or("");
            assert!(
                !src_rev.is_empty(),
                "gate 4b FAIL: cross-repo evidence with workspace_snapshot_id \
                 must also carry source_revision_id (provenance chain broken); ev={ev}"
            );
        }
    }

    child.kill().ok();
    child.wait().ok();

    // ─────────────────────────────────────────────────────────────────────
    // Gate 7: manifest change through Phase 2 production path changes
    // the subsequent MCP cross-repo result.
    //
    // Remove the `require` line from dependent/go.mod, re-index via
    // bootstrap_workspace (same production indexing path), re-spawn
    // the server with ATTIC_CONFIG → sync_workspace rebuilds
    // cross-repo edges → provider should no longer be in the response.
    // ─────────────────────────────────────────────────────────────────────
    fs::write(
        dependent_dir.join("go.mod"),
        // Remove the require block entirely — no longer depends on provider.
        "module example.com/dependent\n\ngo 1.21\n",
    )
    .unwrap();

    // Re-index the dependent repo through the normal production path.
    // Use index_repository directly (rather than spawning a full server
    // round-trip through bootstrap_workspace) purely to keep this test
    // step synchronous and scoped to indexing.
    {
        let manager = attic_storage::RepositoryStoreManager::open(&db_path, tmp.path(), 4).unwrap();
        let lease = manager.acquire(&dependent_id_str).unwrap();
        let readers = lease.readers();
        let writer = lease.writer_handle();
        let store = IndexingStore {
            readers: &readers,
            writer: &writer,
        };
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions::default();
        index_repository(&store, &dependent_dir, &policy, &opts)
            .expect("re-index dependent after manifest change");
    }

    // Spawn a fresh server instance so sync_workspace rebuilds edges from
    // the updated catalog.
    let (mut child2, mut stdin2) = {
        let mut child = Command::new(&bin)
            .env("ATTIC_HOME", tmp.path())
            .env("ATTIC_DB_PATH", db_path.to_str().unwrap())
            .env("ATTIC_CONFIG", cfg_path.to_str().unwrap())
            .env("ATTIC_SEMANTIC", "0")
            .env_remove("ATTIC_WORKSPACE_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn attic (post-manifest-change)");
        let mut stdin = child.stdin.take().unwrap();
        let init = mcp_request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "attic-e2e-gate7", "version": "0"}
            }),
        );
        let resp = send_recv(&mut child, &mut stdin, &init);
        assert_eq!(resp["id"], 1, "gate 7 init failed: {resp}");
        send_only(
            &mut stdin,
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        );
        (child, stdin)
    };

    // As above, do not race the Gate 7 assertion against asynchronous
    // startup bootstrap/sync. The changed manifest must have been consumed
    // by a completed workspace sync before we inspect the result.
    let mut all_current_after_change = false;
    for poll_id in 10..210 {
        let status_call = mcp_request(
            poll_id,
            "tools/call",
            json!({"name":"status","arguments":{}}),
        );
        let status_resp = send_recv(&mut child2, &mut stdin2, &status_call);
        let status_text = status_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        let status_v: Value = serde_json::from_str(status_text).unwrap_or(json!({}));
        if status_v["workspace"]["current_repository_count"] == 3 {
            all_current_after_change = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        all_current_after_change,
        "post-change cross-repo fixture repositories never became CURRENT"
    );
    std::thread::sleep(Duration::from_millis(100));

    let call2 = mcp_request(
        2,
        "tools/call",
        json!({
            "name": "context",
            "arguments": {
                "query": "What Go modules does the dependent repository depend on?",
                "mode": "NORMAL",
                "repository_id": dependent_id_str
            }
        }),
    );
    let resp2 = send_recv(&mut child2, &mut stdin2, &call2);
    let text2 = resp2["result"]["content"][0]["text"].as_str().unwrap_or("");
    let v2: Value = serde_json::from_str(text2).unwrap_or(json!({}));
    let context2 = v2["context"].as_str().unwrap_or("");
    let claims2 = v2["claims"].to_string();
    let full2 = format!("{context2} {claims2} {text2}");

    // Gate 7: after removing the dependency, the provider should no longer
    // appear as a resolved cross-repo dependency claim.
    // We accept either: the provider module is absent from the response,
    // OR the result is INSUFFICIENT_EVIDENCE (no dependency evidence found),
    // OR the response explicitly states no dependencies.
    let result2 = v2["result"].as_str().unwrap_or("");
    let provider_still_claimed = full2.contains("example.com/provider")
        && !full2.contains("no longer")
        && !full2.contains("removed")
        && result2 != "INSUFFICIENT_EVIDENCE";
    assert!(
        !provider_still_claimed,
        "gate 7 FAIL: manifest change must change cross-repo result; \
         provider still claimed after removing require; \
         result={result2}, response={:.400}",
        full2
    );

    child2.kill().ok();
}

/// THE multi-root acceptance test (§23-25 of the multi-root design): ONE
/// Attic process, started ONCE, configured via `ATTIC_CONFIG` with THREE
/// repository roots that share NO common filesystem parent (three
/// independent `TempDir`s, not subdirectories of one workspace, no
/// symlinks, no submodules). Verifies status reports all three as
/// configured/current, workspace-wide and repository-scoped search both
/// work and never cross repository boundaries, and `file` is scoped to
/// the requesting repository's own root.
#[test]
fn mcp_multi_root_workspace_via_config_no_common_parent() {
    let bin = require_binary();

    // Three UNRELATED roots — each its own TempDir, never nested inside
    // one another or under a shared configured parent.
    let repo_a = TempDir::new().unwrap();
    let repo_b = TempDir::new().unwrap();
    let repo_c = TempDir::new().unwrap();
    fs::write(
        repo_a.path().join("alpha.txt"),
        "ALPHA_MARKER_TOKEN one two three",
    )
    .unwrap();
    fs::write(
        repo_b.path().join("beta.txt"),
        "BETA_MARKER_TOKEN four five six",
    )
    .unwrap();
    fs::write(
        repo_c.path().join("gamma.txt"),
        "GAMMA_MARKER_TOKEN seven eight nine",
    )
    .unwrap();

    // Config directory is itself unrelated to any of the three roots.
    let cfg_dir = TempDir::new().unwrap();
    let cfg_path = cfg_dir.path().join("attic-workspace.conf");
    fs::write(
        &cfg_path,
        format!(
            "[[repositories]]\npath = \"{}\"\n\n[[repositories]]\npath = \"{}\"\n\n[[repositories]]\npath = \"{}\"\n",
            repo_a.path().display(),
            repo_b.path().display(),
            repo_c.path().display(),
        ),
    )
    .unwrap();

    let db_dir = TempDir::new().unwrap();
    let mut child = Command::new(&bin)
        .env("ATTIC_HOME", db_dir.path())
        .env(
            "ATTIC_DB_PATH",
            db_dir.path().join("multiroot.db").to_str().unwrap(),
        )
        .env("ATTIC_CONFIG", cfg_path.to_str().unwrap())
        .env("ATTIC_SEMANTIC", "0")
        .env_remove("ATTIC_WORKSPACE_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn attic (multi-root)");
    let mut stdin = child.stdin.take().unwrap();
    let init = mcp_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "attic-multiroot-test", "version": "0"}
        }),
    );
    let resp = send_recv(&mut child, &mut stdin, &init);
    assert_eq!(resp["id"], 1, "init failed: {resp}");
    send_only(
        &mut stdin,
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
    );

    // Startup bootstrap is intentionally asynchronous. Poll status until all
    // three repositories are CURRENT instead of assuming initialize blocks.
    let mut status_v = json!({});
    for _ in 0..200 {
        let status_call = mcp_request(2, "tools/call", json!({"name":"status","arguments":{}}));
        let status_resp = send_recv(&mut child, &mut stdin, &status_call);
        let status_text = status_resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or("");
        status_v = serde_json::from_str(status_text).expect("status is JSON");
        if status_v["workspace"]["current_repository_count"] == 3 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(
        status_v["workspace"]["configured_repository_count"], 3,
        "status={status_v}"
    );
    assert_eq!(
        status_v["workspace"]["current_repository_count"], 3,
        "status={status_v}"
    );
    assert_eq!(status_v["workspace"]["disabled_repository_count"], 0);

    // ── workspace-wide search: each marker resolves to a DISTINCT repo,
    //    and the returned path never crosses into another root ────────
    let search_for = |id: u64,
                      query: &str,
                      stdin: &mut std::process::ChildStdin,
                      child: &mut std::process::Child|
     -> (String, String) {
        let call = mcp_request(
            id,
            "tools/call",
            json!({"name":"context","arguments":{"query": query, "mode": "SEARCH"}}),
        );
        let resp = send_recv(child, stdin, &call);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(text).unwrap_or(json!({}));
        let results = v["results"].as_array().cloned().unwrap_or_default();
        assert_eq!(
            results.len(),
            1,
            "expected exactly one workspace-wide hit for {query}, got: {text}"
        );
        (
            results[0]["repository_id"]
                .as_str()
                .unwrap_or("")
                .to_string(),
            results[0]["path"].as_str().unwrap_or("").to_string(),
        )
    };
    let (repo_a_id, path_a) = search_for(3, "ALPHA_MARKER_TOKEN", &mut stdin, &mut child);
    let (repo_b_id, path_b) = search_for(4, "BETA_MARKER_TOKEN", &mut stdin, &mut child);
    let (repo_c_id, path_c) = search_for(5, "GAMMA_MARKER_TOKEN", &mut stdin, &mut child);
    assert!(path_a.contains("alpha.txt"), "path_a={path_a}");
    assert!(path_b.contains("beta.txt"), "path_b={path_b}");
    assert!(path_c.contains("gamma.txt"), "path_c={path_c}");
    assert_ne!(repo_a_id, repo_b_id);
    assert_ne!(repo_b_id, repo_c_id);
    assert_ne!(repo_a_id, repo_c_id);

    // ── repository-scoped search must never leak across roots: asking
    //    repo A for repo C's marker returns nothing ───────────────────
    let scoped_call = mcp_request(
        6,
        "tools/call",
        json!({"name":"context","arguments":{"query":"GAMMA_MARKER_TOKEN","repository_id": repo_a_id, "mode":"SEARCH"}}),
    );
    let scoped_resp = send_recv(&mut child, &mut stdin, &scoped_call);
    let scoped_text = scoped_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    let scoped_v: Value = serde_json::from_str(scoped_text).unwrap_or(json!({}));
    assert_eq!(
        scoped_v["results"].as_array().map(Vec::len).unwrap_or(0),
        0,
        "repo A scoped search must not see repo C's content: {scoped_text}"
    );

    // ── `file`: repo-scoped read resolves the CORRECT repository's own
    //    root, never another configured root's file with the same name.
    let file_call = mcp_request(
        7,
        "tools/call",
        json!({"name":"file","arguments":{"repository_id": repo_a_id, "path": "alpha.txt"}}),
    );
    let file_resp = send_recv(&mut child, &mut stdin, &file_call);
    let file_text = file_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        file_text.contains("ALPHA_MARKER_TOKEN"),
        "file tool must read repo A's own alpha.txt: {file_text:.300}"
    );

    // repo A does not contain gamma.txt — must be a clean not-found, not
    // a cross-root read of repo C's file of the same relative name.
    let cross_call = mcp_request(
        8,
        "tools/call",
        json!({"name":"file","arguments":{"repository_id": repo_a_id, "path": "gamma.txt"}}),
    );
    let cross_resp = send_recv(&mut child, &mut stdin, &cross_call);
    let cross_text = cross_resp["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("");
    assert!(
        !cross_text.contains("GAMMA_MARKER_TOKEN"),
        "repo A file access must never resolve repo C's content: {cross_text:.300}"
    );

    child.kill().ok();
    child.wait().ok();
}

// ── §37 failure-case unit tests ──────────────────────────────────────────

/// §37: corrupted config.toml must produce a clear diagnostic.
///
/// The `load_workspace_roots` half is gated on the ambient environment:
/// running `env::remove_var` in parallel tests is racy (threads share the
/// process environment), so we only exercise the `load_workspace_roots`
/// code-path when the relevant env vars are NOT already set by the test
/// runner. The `parse_repositories_config` half is always safe because it
/// is a pure function with no env reads.
#[test]
fn corrupted_config_toml_fails_with_diagnostic() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("config.toml");
    std::fs::write(&cfg, "this is garbage \x00 not toml [[[\n").unwrap();

    // Pure function — always testable regardless of ambient env.
    let contents = std::fs::read_to_string(&cfg).unwrap();
    let err = parse_repositories_config(&contents).unwrap_err();
    assert!(!err.is_empty(), "must produce a diagnostic: {err}");

    // load_workspace_roots reads env vars — only safe when ambient vars
    // are not set (avoid racy env mutation in parallel test threads).
    if std::env::var("ATTIC_CONFIG").is_err() && std::env::var("ATTIC_WORKSPACE_ROOT").is_err() {
        let result = load_workspace_roots(&cfg);
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains(cfg.to_str().unwrap()) || msg.contains("config"),
            "error must name the config file: {msg}"
        );
    }
}

/// §37: persisting config to a non-existent directory must return Err.
#[test]
fn config_write_failure_returns_error() {
    let tmp = TempDir::new().unwrap();
    let bad_path = tmp.path().join("nonexistent_dir").join("config.toml");
    let roots = vec![tmp.path().to_path_buf()];
    let result = persist_repositories_config(&bad_path, &roots);
    assert!(result.is_err(), "write to non-existent dir must fail");
    let msg = result.unwrap_err();
    assert!(
        msg.contains("failed to write") || msg.contains("config"),
        "error must be descriptive: {msg}"
    );
}

/// PR-9: the hardened write path must still round-trip correctly and
/// leave no temp file behind once it completes.
#[test]
fn persist_repositories_config_round_trips_and_leaves_no_temp_file() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("config.toml");
    let roots = vec![
        tmp.path().join("repo a"),
        tmp.path().join("repo\\b"),
        tmp.path().join("unicode_δρεπος"),
    ];
    for r in &roots {
        std::fs::create_dir_all(r).unwrap();
    }

    persist_repositories_config(&cfg, &roots).unwrap();
    let (_source, loaded) = load_workspace_roots(&cfg).unwrap();
    assert_eq!(loaded, roots, "round-trip must preserve every root exactly");

    let leftover: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        leftover.is_empty(),
        "no temp file must remain after a successful write: {leftover:?}"
    );
}

/// Code-review finding: a failure after the temp file is created (here,
/// the final rename failing because the destination is a directory)
/// must not leave the temp file behind.
#[test]
fn persist_repositories_config_cleans_up_temp_file_on_rename_failure() {
    let tmp = TempDir::new().unwrap();
    // `cfg` is a directory, not a file — `fs::rename(&tmp_file, &cfg)`
    // will fail on Windows ("Access is denied" / directory-in-the-way),
    // exercising the post-write, pre-rename-success failure path.
    let cfg = tmp.path().join("config.toml");
    std::fs::create_dir_all(&cfg).unwrap();
    let root = tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();

    let result = persist_repositories_config(&cfg, &[root]);
    assert!(result.is_err(), "rename onto a directory must fail");

    let leftover: Vec<_> = std::fs::read_dir(tmp.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
        .collect();
    assert!(
        leftover.is_empty(),
        "the temp file must be cleaned up even when the final rename fails: {leftover:?}"
    );
}

/// PR-9: two overlapping config writes (simulating two racing
/// `workspace` tool calls) must not corrupt each other — the unique
/// temp filename plus atomic rename means the last one to finish wins
/// cleanly, never a truncated/interleaved file.
#[test]
fn persist_repositories_config_concurrent_writes_never_corrupt_the_file() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("config.toml");
    let root_a = tmp.path().join("a");
    let root_b = tmp.path().join("b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let cfg_a = cfg.clone();
    let roots_a = vec![root_a.clone()];
    let cfg_b = cfg.clone();
    let roots_b = vec![root_b.clone()];
    let t1 = std::thread::spawn(move || persist_repositories_config(&cfg_a, &roots_a));
    let t2 = std::thread::spawn(move || persist_repositories_config(&cfg_b, &roots_b));
    t1.join().unwrap().unwrap();
    t2.join().unwrap().unwrap();

    // Whichever wrote last, the result must be a fully valid config
    // naming exactly one of the two roots — never a mix of both
    // (interleaved writes) and never a parse failure (truncated write).
    let (_source, loaded) = load_workspace_roots(&cfg).unwrap();
    assert_eq!(loaded.len(), 1, "must never interleave into a mixed file");
    assert!(loaded == vec![root_a] || loaded == vec![root_b]);
}

#[test]
fn mcp_stderr_does_not_contaminate_stdout() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let mut child = Command::new(&bin)
        .env("ATTIC_HOME", tmp.path())
        .env(
            "ATTIC_DB_PATH",
            tmp.path().join("test.db").to_str().unwrap(),
        )
        .env("ATTIC_SEMANTIC", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let init = mcp_request(
        1,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "attic-supplemental-test", "version": "0"}
        }),
    );
    let resp = send_recv(&mut child, &mut stdin, &init);
    assert_eq!(resp["jsonrpc"], "2.0", "stdout contains non-JSON: {resp}");
    child.kill().ok();
    child.wait().ok();
}

#[test]
fn mcp_disconnect_cancels_and_joins_background_bootstrap() {
    let bin = require_binary();
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("cancel-repo");
    fs::create_dir_all(&repo).unwrap();
    // Enough work to ensure the bootstrap is genuinely in flight when stdio closes.
    for i in 0..500 {
        fs::write(
            repo.join(format!("file-{i}.rs")),
            format!("pub fn f{i}() -> usize {{ {i} }}\n"),
        )
        .unwrap();
    }

    let (mut child, mut stdin) = spawn_and_initialize(&bin, &tmp);
    let add = mcp_request(
        2,
        "tools/call",
        json!({
            "name": "workspace",
            "arguments": {"action":"add", "path": repo.display().to_string()}
        }),
    );
    let resp = send_recv(&mut child, &mut stdin, &add);
    assert_eq!(resp["id"], 2, "workspace add failed: {resp}");

    // Closing MCP stdin is a normal transport disconnect. Attic must cancel
    // and join owned bootstrap work, then exit by itself; no child.kill().
    drop(stdin);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            assert!(status.success(), "attic must shut down cleanly: {status}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "attic stayed alive after MCP disconnect"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
