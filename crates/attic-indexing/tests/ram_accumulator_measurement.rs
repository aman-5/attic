//! Regression guards for RAM accumulator bounds in the indexing pipeline.

use std::path::Path;

use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_storage::{WriterQueue, open_db};
use tempfile::TempDir;

/// Roughly the shape of an AEM form-code export: a few hundred lines of
/// pretty-printed JSON where individual rule expressions run to multiple KB on
/// a single line. Deliberately the content class that exposed the truncation
/// bug — long single lines inside an otherwise ordinary file.
fn dense_json_file(index: usize, rules: usize) -> String {
    let mut s = String::from("{\n  \"items\": {\n");
    for r in 0..rules {
        // One long single-line expression, comfortably over the per-unit cap.
        let expr = format!(
            "value != null && value != '' && (function(){{ return {} }})() && \
             somePredicate_{r}(input) && anotherPredicate_{r}(input)",
            "x".repeat(2_000)
        );
        s.push_str(&format!(
            "    \"custom:Rule_{index}_{r}\": {{ \"rule\": \"{expr}\" }},\n"
        ));
    }
    s.push_str("    \"_end\": true\n  }\n}\n");
    s
}

fn build_repo(dir: &Path, files: usize, rules_per_file: usize) {
    for i in 0..files {
        std::fs::write(
            dir.join(format!("Env_{i}-Code.json")),
            dense_json_file(i, rules_per_file),
        )
        .expect("write fixture");
    }
}

/// The analysis cache is now *bounded*: once the buffered rows reach
/// `analysis_cache_flush_bytes` they are handed to the writer queue and the
/// buffer is emptied, so its high-water mark no longer tracks repository size.
///
/// This is the regression guard for that bound. It drives the threshold down
/// to a value the fixture is certain to exceed, then asserts the peak stays
/// near it instead of growing to the whole-corpus total.
#[test]
fn analysis_cache_peak_is_bounded_by_the_flush_threshold() {
    let dir = TempDir::new().unwrap();
    build_repo(dir.path(), 8, 8);

    let db_path = dir.path().join("bounded.db");
    let (conn, pool) = open_db(&db_path).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = WriterQueue::new(conn).unwrap();
    let handle = queue.handle();
    let store = IndexingStore {
        readers: &pool,
        writer: &handle,
    };
    let policy = DiscoveryPolicy::default_git();

    let unbounded = index_repository(&store, dir.path(), &policy, &IndexOptions::default())
        .expect("unbounded run succeeds");
    let unbounded_peak = unbounded.stage_timings.cache_writes_peak_bytes;

    // One file's cache row is far larger than this, so every single file
    // crosses the threshold and flushes.
    let flush_bytes = 64 * 1024;
    let bounded_opts = IndexOptions {
        analysis_cache_flush_bytes: flush_bytes,
        ..IndexOptions::default()
    };
    let dir2 = TempDir::new().unwrap();
    build_repo(dir2.path(), 8, 8);
    let db2 = dir2.path().join("bounded2.db");
    let (conn2, pool2) = open_db(&db2).unwrap();
    attic_storage::run_migrations(&conn2).unwrap();
    let queue2 = WriterQueue::new(conn2).unwrap();
    let handle2 = queue2.handle();
    let store2 = IndexingStore {
        readers: &pool2,
        writer: &handle2,
    };
    let bounded = index_repository(&store2, dir2.path(), &policy, &bounded_opts)
        .expect("bounded run succeeds");
    let bounded_peak = bounded.stage_timings.cache_writes_peak_bytes;

    println!(
        "\n[bounded] unbounded cache peak={unbounded_peak} bytes -> bounded peak={bounded_peak} \
         bytes across {} flushes",
        bounded.stage_timings.cache_writes_flushes,
    );

    assert!(
        bounded.stage_timings.cache_writes_flushes > 0,
        "a threshold below one file's cache row must trigger mid-run flushes"
    );
    assert_eq!(
        unbounded.units_inserted, bounded.units_inserted,
        "flushing the cache earlier must not change what gets indexed"
    );
    assert!(
        bounded_peak < unbounded_peak,
        "bounding the cache must lower its high-water mark \
         (bounded={bounded_peak}, unbounded={unbounded_peak})"
    );
    // The buffer is checked after each file is merged, so the peak can exceed
    // the threshold by at most one file's worth of rows — never by the corpus.
    let one_file_slack = unbounded_peak / 8 + flush_bytes;
    assert!(
        bounded_peak <= flush_bytes + one_file_slack,
        "the peak must stay within one file of the threshold \
         (peak={bounded_peak}, threshold={flush_bytes}, slack={one_file_slack})"
    );
}

/// The striped analysis pipeline bounds *unmerged* analysis output: at most
/// one stripe of files' results (retrieval text plus structural captures) is
/// live at once, instead of the whole corpus. Regression guard for the
/// whole-corpus `Vec<AnalyzedFile>` peak that parallel analysis introduced.
#[test]
fn analysis_inflight_peak_is_bounded_by_the_stripe() {
    // 128 files with analysis forced sequential: stripe = max(1*8, 32) = 32
    // files, so the run is 4 stripes and the unmerged peak should sit near a
    // quarter of the corpus text, not at 100% of it.
    let dir = TempDir::new().unwrap();
    build_repo(dir.path(), 128, 8);

    let db_path = dir.path().join("striped.db");
    let (conn, pool) = open_db(&db_path).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = WriterQueue::new(conn).unwrap();
    let handle = queue.handle();
    let store = IndexingStore {
        readers: &pool,
        writer: &handle,
    };
    let policy = DiscoveryPolicy::default_git();
    let opts = IndexOptions {
        analysis_threads: 1,
        ..IndexOptions::default()
    };
    let result = index_repository(&store, dir.path(), &policy, &opts).expect("indexing succeeds");

    let inflight = result.stage_timings.analysis_inflight_peak_bytes;
    let total = result.stage_timings.pending_units_peak_bytes;
    println!(
        "\n[striped] analysis_inflight_peak={inflight} bytes vs whole-corpus pending peak={total} bytes"
    );

    assert!(
        inflight > 0,
        "analysis produced units, so a stripe held them"
    );
    assert!(
        inflight.saturating_mul(2) <= total,
        "with 4 stripes the unmerged peak must stay well under the whole-corpus \
         total (inflight={inflight}, total={total})"
    );
}
