//! Freshness benchmark (plan Phase 0 / Phase 9). Ignored by default; run:
//!
//! ```powershell
//! $env:ATTIC_BENCH_WORKSPACE = "C:\path\repo1;C:\path\repo2"
//! $env:ATTIC_BENCH_OUT = "docs\benchmarks\freshness.md"   # optional
//! cargo test -p attic-incremental --release --test freshness_bench -- --ignored --nocapture
//! ```
//!
//! Read-only for the listed repositories: indexes live in temporary
//! databases; the edit benchmark works on a temporary copy.

use std::path::{Path, PathBuf};
use std::time::Instant;

use attic_discovery::DiscoveryPolicy;
use attic_incremental::{GateConfig, ReconcileOptions, ensure_current, reconcile_repository_with};
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_storage::{WriterQueue, open_db, run_migrations};

struct Row {
    repo: String,
    files: usize,
    full_index_ms: u128,
    check_fast_ms: u128,
    check_hash_all_ms: u128,
    edit10_check_ms: u128,
}

fn bench_repo(root: &Path) -> Row {
    let dir = tempfile::TempDir::new().unwrap();
    let (conn, pool) = open_db(dir.path().join("bench.db")).unwrap();
    run_migrations(&conn).unwrap();
    let queue = WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let store = IndexingStore {
        readers: &pool,
        writer: &writer,
    };
    let policy = DiscoveryPolicy::default_git();
    let opts = IndexOptions::default();
    let t = Instant::now();
    let result = index_repository(&store, root, &policy, &opts).expect("full index");
    let full_index_ms = t.elapsed().as_millis();
    // Let the racy window pass, then re-index so capture times are trusted.
    std::thread::sleep(std::time::Duration::from_millis(2_200));
    index_repository(&store, root, &policy, &opts).expect("settle index");

    let t = Instant::now();
    let r = reconcile_repository_with(&pool, root, &policy, &ReconcileOptions::default()).unwrap();
    let check_fast_ms = t.elapsed().as_millis();
    let t = Instant::now();
    reconcile_repository_with(
        &pool,
        root,
        &policy,
        &ReconcileOptions { always_hash: true },
    )
    .unwrap();
    let check_hash_all_ms = t.elapsed().as_millis();
    let _ = r;
    drop(queue);

    Row {
        repo: root
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
        files: result.files_indexed,
        full_index_ms,
        check_fast_ms,
        check_hash_all_ms,
        edit10_check_ms: bench_edit_copy(root),
    }
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name();
        if name == ".git" || name == "node_modules" || name == "target" {
            continue;
        }
        let src = entry.path();
        let dst = to.join(&name);
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            copy_tree(&src, &dst);
        } else {
            let _ = std::fs::copy(&src, &dst);
        }
    }
}

/// Gate time after editing 10 source files in a temporary copy.
fn bench_edit_copy(root: &Path) -> u128 {
    let dir = tempfile::TempDir::new().unwrap();
    let copy = dir.path().join("repo");
    copy_tree(root, &copy);
    let (conn, pool) = open_db(dir.path().join("bench.db")).unwrap();
    run_migrations(&conn).unwrap();
    let queue = WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let store = IndexingStore {
        readers: &pool,
        writer: &writer,
    };
    let policy = DiscoveryPolicy::default_non_git();
    index_repository(&store, &copy, &policy, &IndexOptions::default()).unwrap();
    let mut edited = 0;
    let mut stack = vec![copy.clone()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if edited < 10
                && p.extension()
                    .is_some_and(|x| x == "js" || x == "java" || x == "ts")
            {
                let mut s = std::fs::read_to_string(&p).unwrap_or_default();
                s.push_str("\n// bench edit\n");
                std::fs::write(&p, s).unwrap();
                edited += 1;
            }
        }
    }
    let t = Instant::now();
    ensure_current(&pool, &writer, &copy, &policy, &GateConfig::default()).unwrap();
    let ms = t.elapsed().as_millis();
    drop(queue);
    ms
}

fn peak_rss_mib() -> Option<u64> {
    // Best effort; Windows only via tasklist would need a process API.
    None
}

#[test]
#[ignore = "benchmark: set ATTIC_BENCH_WORKSPACE and run with --ignored"]
fn freshness_benchmark() {
    let Ok(list) = std::env::var("ATTIC_BENCH_WORKSPACE") else {
        eprintln!("ATTIC_BENCH_WORKSPACE not set; skipping");
        return;
    };
    let roots: Vec<PathBuf> = list
        .split(';')
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .collect();
    let mut rows = Vec::new();
    for root in &roots {
        eprintln!("benchmarking {}", root.display());
        rows.push(bench_repo(root));
    }
    let mut md = String::from(
        "| Repository | Files | Full index (ms) | Check, nothing changed (ms) | Check, hash everything (ms) | Check after 10 edits (ms) |\n|---|---:|---:|---:|---:|---:|\n",
    );
    let mut total = [0u128; 4];
    let mut files = 0usize;
    for r in &rows {
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            r.repo,
            r.files,
            r.full_index_ms,
            r.check_fast_ms,
            r.check_hash_all_ms,
            r.edit10_check_ms
        ));
        files += r.files;
        total[0] += r.full_index_ms;
        total[1] += r.check_fast_ms;
        total[2] += r.check_hash_all_ms;
        total[3] += r.edit10_check_ms;
    }
    md.push_str(&format!(
        "| **Total** | {files} | {} | {} | {} | {} |\n",
        total[0], total[1], total[2], total[3]
    ));
    if let Some(rss) = peak_rss_mib() {
        md.push_str(&format!("\nPeak RSS: {rss} MiB\n"));
    }
    println!("{md}");
    if let Ok(out) = std::env::var("ATTIC_BENCH_OUT") {
        std::fs::write(out, &md).unwrap();
    }
}
