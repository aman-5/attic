#![allow(dead_code)]
use attic_core::config::PlanningConfig;
use attic_spec::{
    service::{ApprovalPrompt, Clock, SpecService},
    state::Document,
    verifier::{Repository, Run},
};
use attic_storage::{DbPool, WriterQueue};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

pub fn fixture() -> tempfile::TempDir {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("egm-tests");
    std::fs::create_dir_all(&path).unwrap();
    tempfile::tempdir_in(path).unwrap()
}
pub fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=EGM Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=.git/disabled-hooks",
            "-c",
            "core.autocrlf=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            root.join(".git").join("absent-global-config"),
        )
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}
pub struct TestClock(pub AtomicI64);
impl Clock for TestClock {
    fn now(&self) -> attic_spec::Result<i64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
pub struct Prompt(pub bool);
impl ApprovalPrompt for Prompt {
    fn is_interactive(&self) -> bool {
        true
    }
    fn confirm(&mut self, _: &Document, _: &Run, code: &str) -> attic_spec::Result<bool> {
        assert_eq!(code.len(), 6);
        Ok(self.0)
    }
}
pub struct Fixture {
    pub root: PathBuf,
    pub repo: Repository,
    pub pool: DbPool,
    pub queue: WriterQueue,
    pub service: SpecService,
    pub clock: Arc<TestClock>,
    pub dir: tempfile::TempDir,
}
impl Fixture {
    pub fn new(source: &str, citations: &str, receipts: bool) -> Self {
        let dir = fixture();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = root.canonicalize().unwrap();
        git(&root, &["init", "--quiet"]);
        std::fs::write(root.join("src").join("lib.rs"), source).unwrap();
        std::fs::create_dir_all(root.join("docs").join("designs")).unwrap();
        std::fs::write(
            root.join("docs").join("designs").join("one.md"),
            format!("# One\n```attic-citations\n{citations}\n```\n"),
        )
        .unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let (conn, pool) = attic_storage::open_db(data.join("index.db")).unwrap();
        attic_storage::run_migrations(&conn).unwrap();
        let queue = WriterQueue::new(conn).unwrap();
        let options = attic_indexing::IndexOptions {
            record_analysis_receipts: receipts,
            ..Default::default()
        };
        attic_indexing::index_repository(
            &attic_indexing::IndexingStore {
                readers: &pool,
                writer: &queue.handle(),
            },
            &root,
            &attic_discovery::DiscoveryPolicy::default_git(),
            &options,
        )
        .unwrap();
        let id = pool
            .with_reader(|conn| {
                Ok(
                    conn.query_row("SELECT id FROM core_repositories LIMIT 1", [], |r| {
                        r.get::<_, String>(0)
                    })?,
                )
            })
            .unwrap();
        let repo = Repository {
            id,
            root: root.clone(),
            name: "repo".into(),
            pool: pool.clone(),
            lease: None,
            policy: attic_discovery::DiscoveryPolicy::default_git(),
        };
        let mut cfg = PlanningConfig {
            enabled: true,
            ..Default::default()
        };
        cfg.validation.freshness_wait_ms = 0;
        cfg.fast_path.enforce = true;
        cfg.fast_path.max_files = 1;
        cfg.fast_path.max_lines = 1;
        let mut service = SpecService::open(&data.join("catalog.db"), cfg)
            .unwrap()
            .unwrap();
        let clock = Arc::new(TestClock(AtomicI64::new(attic_spec::now_micros().unwrap())));
        service.clock = clock.clone();
        Self {
            root,
            repo,
            pool,
            queue,
            service,
            clock,
            dir,
        }
    }
    pub fn registered(&self) -> Document {
        self.service.register("one", &self.repo).unwrap()
    }
    pub fn verified(&self) -> Document {
        self.service
            .verify(
                &self.registered(),
                false,
                std::slice::from_ref(&self.repo),
                &[],
                None,
                false,
            )
            .unwrap()
    }
    pub fn approved(&self) -> Document {
        self.service
            .approve(&self.verified(), &mut Prompt(true))
            .unwrap()
    }
    pub fn begun(&self) -> Document {
        self.service
            .verify(
                &self.approved(),
                false,
                std::slice::from_ref(&self.repo),
                &[],
                None,
                true,
            )
            .unwrap()
    }
    pub fn reindex(&self) {
        attic_indexing::index_repository(
            &attic_indexing::IndexingStore {
                readers: &self.pool,
                writer: &self.queue.handle(),
            },
            &self.root,
            &self.repo.policy,
            &attic_indexing::IndexOptions {
                record_analysis_receipts: true,
                ..Default::default()
            },
        )
        .unwrap();
    }
    pub fn advance(&self, micros: i64) {
        self.clock.0.fetch_add(micros, Ordering::SeqCst);
    }
}
