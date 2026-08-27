//! Shared deterministic fixture for the Phase 2 integration suite.
//!
//! No network, no home directory, no global Git config, no machine-specific
//! paths: repositories and databases live in `tempfile::TempDir`s owned by
//! each test.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use attic_discovery::DiscoveryPolicy;
use attic_incremental::{
    CoalescedChange, FsEventKind, IncrementalService, NormalizedEvent, run_next_task_synchronously,
};
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_storage::{DbPool, WriterQueue, WriterQueueHandle, open_db, run_migrations};
use tempfile::TempDir;

pub struct Fixture {
    pub _dir: TempDir,
    pub repo_dir: PathBuf,
    pub db_path: PathBuf,
    pub pool: DbPool,
    pub _queue: WriterQueue,
    pub writer: WriterQueueHandle,
    /// Repository UUID string after bootstrap.
    pub repo_id: String,
}

impl Fixture {
    /// Create a fixture with an indexed repository containing the given
    /// `(path, content)` seed files.
    pub fn new(seed: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let repo_dir = dir.path().join("repo");
        std::fs::create_dir_all(&repo_dir).expect("repo dir");

        let db_path = dir.path().join("attic.db");
        let (conn, pool) = open_db(&db_path).expect("open_db");
        run_migrations(&conn).expect("migrations");
        let queue = WriterQueue::new(conn).expect("writer queue");
        let writer = queue.handle();

        for (rel, content) in seed {
            write_file(&repo_dir, rel, content);
        }

        let store = IndexingStore {
            readers: &pool,
            writer: &writer,
        };
        let policy = DiscoveryPolicy::default_git();
        let opts = IndexOptions {
            repository_name: "fixture".into(),
            ..Default::default()
        };
        let result =
            index_repository(&store, &repo_dir, &policy, &opts).expect("bootstrap full index");

        Fixture {
            _dir: dir,
            repo_dir,
            db_path,
            pool,
            _queue: queue,
            writer,
            repo_id: result.repository_id,
        }
    }

    pub fn root(&self) -> &Path {
        &self.repo_dir
    }

    pub fn policy(&self) -> DiscoveryPolicy {
        DiscoveryPolicy::default_git()
    }

    /// A service wired to this fixture's repository with a tiny debounce.
    pub fn service(&self) -> Arc<IncrementalService> {
        Arc::new(IncrementalService::new(&self.repo_dir, self.policy()).with_quiet_period_ms(1))
    }

    /// Drive one full pipeline step deterministically: ingest → drain →
    /// verify → invalidate → schedule → execute task synchronously.
    pub fn step(&self, svc: &IncrementalService, events: Vec<(String, FsEventKind)>) {
        let normalized: Vec<NormalizedEvent> = events
            .into_iter()
            .map(|(p, k)| NormalizedEvent {
                rel_path: p,
                kind: k,
            })
            .collect();
        svc.ingest(&normalized);
        // Deterministic virtual clock: quiet period is 1 ms; jump far ahead.
        let report = svc
            .apply_pending(&self.pool, &self.writer, Some(u64::MAX / 2))
            .expect("apply_pending");
        assert!(!report.queue_saturated, "fixture queue must never saturate");
        while run_next_task_synchronously(
            &self.pool,
            &self.writer,
            &self.repo_dir,
            &self.policy(),
            None,
        )
        .expect("task execution")
        {}
    }

    /// Direct coalesced-change application (bypasses the debouncer).
    pub fn apply_ops(&self, svc: &IncrementalService, ops: Vec<CoalescedChange>) {
        let report = svc
            .apply_operations(&self.pool, &self.writer, ops)
            .expect("apply ops");
        assert!(!report.queue_saturated);
        while run_next_task_synchronously(
            &self.pool,
            &self.writer,
            &self.repo_dir,
            &self.policy(),
            None,
        )
        .expect("task execution")
        {}
    }

    /// FTS search helper returning (path, freshness) pairs.
    pub fn search(&self, query: &str) -> Vec<(String, String)> {
        self.pool
            .with_reader(|c| {
                attic_storage::fts_search(
                    c,
                    &attic_storage::FtsSearchParams {
                        query,
                        repository_id: None,
                        file_type: None,
                        language: None,
                        max_results: 50,
                    },
                )
            })
            .expect("fts search")
            .into_iter()
            .map(|r| (r.path, r.freshness_state))
            .collect()
    }

    /// Latest occurrence snapshot for a path (if any).
    pub fn occurrence(&self, rel: &str) -> Option<attic_storage::OccurrenceSnapshot> {
        let repo: attic_core::RepositoryId = self.repo_id.parse().expect("repo uuid");
        self.pool
            .with_reader(|c| attic_storage::lookup_occurrence_snapshot(c, &repo, rel))
            .expect("snapshot lookup")
    }

    pub fn sql_count(&self, sql: &str) -> i64 {
        self.pool
            .with_reader(|c| {
                c.query_row(sql, rusqlite::params![], |r| r.get::<_, i64>(0))
                    .map_err(attic_storage::StorageError::from)
            })
            .expect("count query")
    }
}

/// Write (create or overwrite) a file inside the repository.
pub fn write_file(root: &Path, rel: &str, content: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).expect("parent dir");
    }
    std::fs::write(abs, content).expect("write file");
}

/// Initialize a Git repository at `root` with fully isolated configuration
/// (no network, no user/global/system config — determinism requirement).
pub fn git_init_isolated(root: &Path) {
    let scratch = tempfile::TempDir::new().expect("git config scratch");
    let global_cfg = scratch.path().join("gitconfig");
    std::fs::write(&global_cfg, b"").expect("empty gitconfig");

    let output = std::process::Command::new("git")
        .arg("init")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", &global_cfg)
        .env("GIT_CONFIG_SYSTEM", &global_cfg)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git init must be available for the .gitignore scenario");
    assert!(
        output.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub fn delete_file(root: &Path, rel: &str) {
    std::fs::remove_file(root.join(rel)).ok();
}

/// BLAKE3 hex of file bytes at `root/rel`.
pub fn hash_of(root: &Path, rel: &str) -> String {
    let bytes = std::fs::read(root.join(rel)).expect("read for hash");
    blake3::hash(&bytes).to_hex().to_string()
}
