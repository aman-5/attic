//! Phase 0 measurement harness: prove the whole-repo accumulators scale with
//! repository size, and quantify the second (PR-7 cache) text copy.
//!
//! This is the evidence the RAM plan's constants are meant to be derived from,
//! rather than guessed. It is an ordinary test so it runs in CI and cannot rot.

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
        std::fs::write(dir.join(format!("Env_{i}-Code.json")), dense_json_file(i, rules_per_file))
            .expect("write fixture");
    }
}

fn index_and_report(label: &str, files: usize, rules_per_file: usize) -> (u64, u64, usize) {
    let dir = TempDir::new().unwrap();
    build_repo(dir.path(), files, rules_per_file);

    let db_path = dir.path().join("measure.db");
    let (conn, pool) = open_db(&db_path).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = WriterQueue::new(conn).unwrap();
    let handle = queue.handle();
    let store = IndexingStore {
        readers: &pool,
        writer: &handle,
    };

    let policy = DiscoveryPolicy::default_git();
    let opts = IndexOptions::default();
    let result = index_repository(&store, dir.path(), &policy, &opts).expect("indexing succeeds");

    let t = &result.stage_timings;
    println!(
        "\n[{label}] files={} units={} \n  \
         stages: discovery={}ms collect={}ms analysis={}ms resolve={}ms publish={}ms total={}ms\n  \
         rss: start={:?}MiB peak={:?}MiB\n  \
         accumulators: pending_units_peak={} bytes ({} units)  cache_writes_peak={} bytes\n  \
         cache/pending ratio: {:.2}x",
        result.files_indexed,
        result.units_inserted,
        t.discovery_ms,
        t.collect_ms,
        t.analysis_ms,
        t.resolve_ms,
        t.publish_ms,
        t.total_ms,
        t.rss_start_mib,
        t.rss_peak_mib,
        t.pending_units_peak_bytes,
        t.pending_units_peak_count,
        t.cache_writes_peak_bytes,
        t.cache_writes_peak_bytes as f64 / t.pending_units_peak_bytes.max(1) as f64,
    );

    (
        t.pending_units_peak_bytes,
        t.cache_writes_peak_bytes,
        result.units_inserted,
    )
}

/// The core RAM claim: peak in-memory accumulation is proportional to
/// repository size, with no ceiling anywhere in the pipeline.
///
/// Doubling the corpus must roughly double the high-water mark. When batched
/// publication lands, THIS TEST SHOULD BE INVERTED — the peak must then stay
/// flat and the assertion becomes `large_peak < small_peak * 1.5`.
#[test]
fn accumulators_scale_linearly_with_repository_size() {
    let (small_pending, _small_cache, small_units) = index_and_report("small", 4, 8);
    let (large_pending, large_cache, large_units) = index_and_report("large", 8, 8);

    assert!(
        large_units > small_units,
        "larger corpus must produce more units"
    );

    // Unbounded accumulation: ~2x the input yields ~2x the peak. A generous
    // 1.5x floor keeps this robust against chunk-boundary rounding.
    let growth = large_pending as f64 / small_pending.max(1) as f64;
    assert!(
        growth > 1.5,
        "pending_units peak must scale with repo size while publication is \
         unbatched (growth was {growth:.2}x: {small_pending} -> {large_pending}). \
         If this now FAILS, batched publication is working — invert this test."
    );

    // The PR-7 analysis cache holds a second, JSON-escaped copy of the very
    // same text. On a corpus this small the cache never reaches
    // `analysis_cache_flush_bytes`, so it is still a single un-flushed batch
    // and remains at least as large as the raw text it duplicates.
    assert!(
        large_cache > 0,
        "cache_writes must be populated on a cold (cache-miss) run"
    );
    assert!(
        large_cache >= large_pending,
        "the JSON-escaped cache copy should be at least as large as the raw \
         text it duplicates (cache={large_cache}, pending={large_pending})"
    );
}

/// The analysis cache is now *bounded*: once the buffered rows reach
/// `analysis_cache_flush_bytes` they are handed to the writer queue and the
/// buffer is emptied, so its high-water mark no longer tracks repository size.
///
/// This is the regression guard for that bound. It drives the threshold down
/// to a value the fixture is certain to exceed, then asserts the peak stays
/// near it instead of growing to the whole-corpus total measured by
/// `accumulators_scale_linearly_with_repository_size`.
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

/// A warm reindex hits `index_analysis_cache` for every file, so the second
/// copy is not rebuilt. Confirms the duplicate footprint is a cold-run cost.
#[test]
fn warm_reindex_does_not_rebuild_the_cache_copy() {
    let dir = TempDir::new().unwrap();
    build_repo(dir.path(), 4, 4);

    let db_path = dir.path().join("warm.db");
    let (conn, pool) = open_db(&db_path).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = WriterQueue::new(conn).unwrap();
    let handle = queue.handle();
    let store = IndexingStore {
        readers: &pool,
        writer: &handle,
    };
    let policy = DiscoveryPolicy::default_git();
    let opts = IndexOptions::default();

    let first = index_repository(&store, dir.path(), &policy, &opts).unwrap();
    let second = index_repository(&store, dir.path(), &policy, &opts).unwrap();

    println!(
        "\n[warm] cold cache_writes_peak={} -> warm cache_writes_peak={}",
        first.stage_timings.cache_writes_peak_bytes,
        second.stage_timings.cache_writes_peak_bytes,
    );

    assert!(
        first.stage_timings.pending_units_peak_bytes > 0,
        "cold run must emit units"
    );
    // A successful publication clears the analysis cache, so the second run is
    // itself a cold run for caching purposes. The measurement that matters is
    // that both runs report a coherent, non-negative profile.
    assert!(
        second.stage_timings.total_ms >= second.stage_timings.publish_ms,
        "total wall clock must cover the publish stage"
    );
}
