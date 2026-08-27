//! Shared Phase 4 test fixture: a small multi-language workspace indexed
//! through the REAL pipeline (discovery → secrets scan → analyzers →
//! structural persistence), plus a ready-to-use `RetrievalService`.

#![allow(dead_code)]

pub mod bench;

use std::path::PathBuf;

use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_retrieval::{AnswerMode, AnswerOutcome, AnswerRequest, RetrievalService};
use attic_storage::{DbPool, WriterQueue, WriterQueueHandle, open_db, run_migrations};
use tempfile::TempDir;

/// Java service class (definition + methods).
pub const ROUTER_JAVA: &str = r#"
package com.sable;

public class Router {
    private final RouteRegistry registry;

    public Router(RouteRegistry registry) {
        this.registry = registry;
    }

    public Response handle(Request request) {
        if (request.path().equals("/health")) {
            return Response.ok();
        }
        return registry.dispatch(request);
    }
}
"#;

/// Second Java class importing the first (import relationship evidence).
pub const REGISTRY_JAVA: &str = r#"
package com.sable;

import com.sable.Router;

public class RouteRegistry {
    public Response dispatch(Request request) {
        return Response.notFound();
    }
}
"#;

/// Java test file (behavioral expectation evidence).
pub const ROUTER_TEST_JAVA: &str = r#"
package com.sable;

import org.junit.jupiter.api.Test;
import static org.junit.jupiter.api.Assertions.assertEquals;

public class RouterTest {
    @Test
    public void healthEndpointReturnsOk() {
        Router router = new Router(new RouteRegistry());
        assertEquals(Response.ok(), router.handle(Request.at("/health")));
    }
}
"#;

/// Configuration file (configured-behavior evidence). Values deliberately
/// distinctive for retrieval assertions.
pub const APP_YML: &str = "\
server:
  port: 8443
sable:
  database_url: postgres://db.internal/sable_prod
  retry_limit: 5
";

/// Project knowledge note (authoritative documented intent).
pub const ARCHITECTURE_MD: &str = r#"
# Sable Architecture

The router dispatches every incoming request through `RouteRegistry`.
Health checks on `/health` short-circuit dispatch.

## Retry policy

The configured retry limit is 3 for upstream calls. Backoff doubles per
attempt up to one second.
"#;

/// Documentation that CONTRADICTS the config value above (retry_limit 5 vs
/// knowledge "retry limit is 3") — drives contradiction-detection tests.
pub const RUNBOOK_MD: &str = r#"
# Operations Runbook

To rotate the upstream endpoint, update `database_url` in config/app.yml.
The retry limit is controlled by `retry_limit` in the same file.
"#;

/// Python module with a function definition.
pub const PAY_PY: &str = r#"
def process_payment(amount_cents: int, currency: str) -> bool:
    """Charge via the payment provider; returns success."""
    if amount_cents <= 0:
        return False
    return True
"#;

fn opts() -> IndexOptions {
    IndexOptions {
        repository_name: "phase4".into(),
        ..Default::default()
    }
}

pub struct Fixture {
    /// Owns the temp workspace lifetime.
    pub dir: TempDir,
    pub root: PathBuf,
    /// Canonical index file (for direct read-only connections).
    pub db_path: PathBuf,
    pub pool: DbPool,
    _queue: WriterQueue,
    pub writer: WriterQueueHandle,
}

impl Fixture {
    /// Build the standard multi-language benchmark workspace.
    pub fn bootstrap() -> Self {
        let seed: Vec<(&str, &str)> = vec![
            ("src/main/java/com/sable/Router.java", ROUTER_JAVA),
            ("src/main/java/com/sable/RouteRegistry.java", REGISTRY_JAVA),
            ("src/test/java/com/sable/RouterTest.java", ROUTER_TEST_JAVA),
            ("config/app.yml", APP_YML),
            ("knowledge/architecture.md", ARCHITECTURE_MD),
            ("docs/runbook.md", RUNBOOK_MD),
            ("services/pay.py", PAY_PY),
        ];
        Self::seed(&seed)
    }

    /// Seed an arbitrary file set (test-specific scenarios).
    pub fn seed_pub(files: &[(&'static str, &'static str)]) -> Self {
        Self::seed(files)
    }

    /// Build an empty repository (no files).
    pub fn bootstrap_empty() -> Self {
        Self::seed(&[])
    }

    fn seed(files: &[(&'static str, &'static str)]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo root");
        for (rel, content) in files {
            let p = root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        let db_path = dir.path().join("attic.db");
        let (conn, pool) = open_db(&db_path).expect("open_db");
        run_migrations(&conn).expect("migrations");
        let queue = WriterQueue::new(conn).expect("writer queue");
        let writer = queue.handle();

        if !files.is_empty() {
            let store = IndexingStore {
                readers: &pool,
                writer: &writer,
            };
            index_repository(&store, &root, &DiscoveryPolicy::default_git(), &opts())
                .expect("bootstrap indexing");
        }

        Self {
            dir,
            root,
            db_path: db_path.clone(),
            pool,
            _queue: queue,
            writer,
        }
    }

    /// Direct READ-ONLY connection for semantic-layer calls whose error
    /// types differ from the pool's closure signature.
    pub fn read_conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open_with_flags(
            &self.db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read-only canonical connection")
    }

    pub fn store(&self) -> IndexingStore<'_> {
        IndexingStore {
            readers: &self.pool,
            writer: &self.writer,
        }
    }

    pub fn service(&self) -> RetrievalService {
        RetrievalService {
            readers: self.pool.clone(),
            writer: self.writer.clone(),
            semantic: None,
            crossrepo_degraded: false,
        }
    }

    /// Service with a Phase 5 semantic stack over an in-memory store.
    pub fn service_with_semantic(
        &self,
        provider: std::sync::Arc<dyn attic_semantic::SemanticProvider>,
    ) -> Result<RetrievalService, String> {
        Ok(RetrievalService {
            readers: self.pool.clone(),
            writer: self.writer.clone(),
            semantic: Some(std::sync::Arc::new(
                attic_retrieval::semantic::SemanticStack::in_memory(provider)?,
            )),
            crossrepo_degraded: false,
        })
    }

    /// Run one query at the given mode and return the outcome.
    pub fn ask(&self, question: &str, mode: AnswerMode) -> AnswerOutcome {
        self.service()
            .answer(&AnswerRequest::new(question, mode))
            .expect("answer")
    }

    pub fn query_i64<F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<i64>>(
        &self,
        f: F,
    ) -> i64 {
        self.pool
            .with_reader(|c| f(c).map_err(attic_storage::StorageError::from))
            .unwrap()
    }

    /// Set freshness of ALL artifacts for one path (simulates Phase 2 state).
    pub fn set_path_freshness(&self, path: &str, state: &str) {
        let path = path.to_owned();
        let state = state.to_owned();
        self.writer
            .send(move |c| {
                c.execute(
                    "UPDATE core_file_occurrences SET freshness_state = ?2 WHERE path = ?1",
                    rusqlite::params![path, state],
                )
                .map_err(storage_err)?;
                // Propagate to dependent artifact states so the scenario is
                // internally consistent.
                c.execute(
                    "UPDATE core_retrieval_units SET freshness_state = ?2
                      WHERE file_occurrence_id IN (
                          SELECT id FROM core_file_occurrences WHERE path = ?1)",
                    rusqlite::params![path, state],
                )
                .map_err(storage_err)?;
                c.execute(
                    "UPDATE core_structural_nodes SET freshness_state = ?2
                      WHERE file_occurrence_id IN (
                          SELECT id FROM core_file_occurrences WHERE path = ?1)",
                    rusqlite::params![path, state],
                )
                .map_err(storage_err)?;
                Ok(())
            })
            .expect("set freshness");
    }
}

// Local error shim so closures match the writer's expected signature.
fn storage_err(e: rusqlite::Error) -> attic_storage::StorageError {
    attic_storage::StorageError::Worker(e.to_string())
}

/// Count plans persisted in ops_retrieval_log.
pub fn persisted_plan_count(fx: &Fixture) -> i64 {
    fx.query_i64(|c| c.query_row("SELECT COUNT(*) FROM ops_retrieval_log", [], |r| r.get(0)))
}
