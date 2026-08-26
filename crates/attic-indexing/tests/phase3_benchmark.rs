//! Phase 3 — structural benchmark slice (§18 of the brief).
//!
//! Indexes a deterministic multi-language fixture repository twice through
//! the REAL pipeline against fresh databases:
//!
//! - **baseline**   — `IndexOptions { structural: false }` (Phase 1D
//!   behaviour: GenericAnalyzer only, no structural artifacts)
//! - **structural** — default options (GenericAnalyzer + five language
//!   analyzers)
//!
//! then compares committed capability metrics. Gate assertions keep
//! structural complexity ONLY where it measurably improves definition /
//! symbol / import / reference / navigation answers without regressing
//! plain FTS coverage.

use std::path::PathBuf;

use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_storage::{
    FtsSearchParams, WriterQueue, WriterQueueHandle, fts_search, open_db, run_migrations,
};
use tempfile::TempDir;

const SEED: &[(&str, &str)] = &[
    // Java cluster: cross-file extends + import + call.
    (
        "src/com/acme/Main.java",
        "package com.acme;\nimport com.acme.shared.Helper;\npublic class Main extends BaseService {\n    public int run() { Helper.greet(); return 1; }\n}\n",
    ),
    (
        "src/com/acme/BaseService.java",
        "package com.acme;\npublic abstract class BaseService { }\n",
    ),
    (
        "src/com/acme/shared/Helper.java",
        "package com.acme.shared;\npublic class Helper {\n    public static void greet() { }\n}\n",
    ),
    // Python cluster: relative import + local call.
    ("pkg/__init__.py", ""),
    (
        "pkg/core.py",
        "from .helpers import load\n\ndef boot():\n    return load(1)\n",
    ),
    ("pkg/helpers.py", "def load(x):\n    return x\n"),
    // Go cluster: module-path import + intra-file constructor call.
    ("go.mod", "module github.com/acme/wire\n\ngo 1.22\n"),
    (
        "cmd/app.go",
        "package main\n\nimport (\n\t\"github.com/acme/wire/internal/parts\"\n)\n\nfunc main() {\n\tparts.Use()\n}\n",
    ),
    (
        "internal/parts/parts.go",
        "package parts\n\nfunc Use() { }\n",
    ),
    // JS/TS cluster: relative imports.
    (
        "web/app.ts",
        "import { cfg } from './config';\nexport function start() { return cfg; }\n",
    ),
    ("web/config.ts", "export const cfg = 42;\n"),
];

struct Bench {
    _dir: TempDir,
    #[allow(dead_code)]
    root: PathBuf,
    pool: attic_storage::DbPool,
    _queue: WriterQueue,
    #[allow(dead_code)]
    writer: WriterQueueHandle,
    #[allow(dead_code)]
    repo_id: String,
}

impl Bench {
    fn new(structural: bool) -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        for (rel, content) in SEED {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        let db = dir.path().join("bench.db");
        let (conn, pool) = open_db(&db).unwrap();
        run_migrations(&conn).unwrap();
        let queue = WriterQueue::new(conn).unwrap();
        let writer = queue.handle();

        let opts = IndexOptions {
            repository_name: "bench".into(),
            structural,
            ..Default::default()
        };
        let store = IndexingStore {
            readers: &pool,
            writer: &writer,
        };
        let result =
            index_repository(&store, &root, &DiscoveryPolicy::default_git(), &opts).unwrap();
        Self {
            _dir: dir,
            root,
            pool,
            _queue: queue,
            writer,
            repo_id: result.repository_id,
        }
    }
}

#[derive(Debug, PartialEq)]
struct Metrics {
    /// Distinct symbol definitions with a definition occurrence — powers
    /// definition/symbol lookup questions.
    symbol_definitions: i64,
    /// IMPORT edges resolved beyond SYNTACTIC — dependency questions.
    resolved_imports: i64,
    /// EXTENDS/IMPLEMENTS/CALL edges at SYMBOL_RESOLVED — reference questions.
    resolved_references: i64,
    /// Unit↔node navigation links — navigation questions.
    navigation_links: i64,
    /// Total FTS hits across representative queries — coverage guard.
    fts_hits: i64,
}

fn measure(bench: &Bench) -> Metrics {
    bench
        .pool
        .with_reader(|c| {
            let symbol_definitions: i64 = c.query_row(
                "SELECT COUNT(*) FROM core_symbol_occurrences WHERE is_definition = 1",
                [],
                |r| r.get(0),
            )?;
            let resolved_imports: i64 = c.query_row(
                "SELECT COUNT(*) FROM core_relationships
                  WHERE rel_type='IMPORT' AND resolution != 'SYNTACTIC'",
                [],
                |r| r.get(0),
            )?;
            let resolved_references: i64 = c.query_row(
                "SELECT COUNT(*) FROM core_relationships
                  WHERE rel_type IN ('EXTENDS','IMPLEMENTS','CALL')
                    AND resolution = 'SYMBOL_RESOLVED'",
                [],
                |r| r.get(0),
            )?;
            let navigation_links: i64 =
                c.query_row("SELECT COUNT(*) FROM core_retrieval_unit_nodes", [], |r| {
                    r.get(0)
                })?;
            let mut fts_hits = 0i64;
            for q in [
                "run", "boot", "main", "start", "Use", "load", "cfg", "Helper",
            ] {
                fts_hits += fts_search(
                    c,
                    &FtsSearchParams {
                        query: q,
                        repository_id: None,
                        file_type: None,
                        language: None,
                        max_results: 20,
                    },
                )?
                .len() as i64;
            }
            Ok(Metrics {
                symbol_definitions,
                resolved_imports,
                resolved_references,
                navigation_links,
                fts_hits,
            })
        })
        .unwrap()
}

#[test]
fn structural_benchmark_improves_capability_slices_without_fts_regression() {
    let baseline_bench = Bench::new(false);
    let structural_bench = Bench::new(true);

    let baseline = measure(&baseline_bench);
    let structural = measure(&structural_bench);

    println!("=== Phase 3 structural benchmark ===");
    println!("{:<26}{:>10}{:>12}", "metric", "baseline", "structural");
    println!(
        "{:<26}{:>10}{:>12}",
        "symbol definitions", baseline.symbol_definitions, structural.symbol_definitions
    );
    println!(
        "{:<26}{:>10}{:>12}",
        "resolved imports", baseline.resolved_imports, structural.resolved_imports
    );
    println!(
        "{:<26}{:>10}{:>12}",
        "resolved references", baseline.resolved_references, structural.resolved_references
    );
    println!(
        "{:<26}{:>10}{:>12}",
        "navigation links", baseline.navigation_links, structural.navigation_links
    );
    println!(
        "{:<26}{:>10}{:>12}",
        "fts hits (8 queries)", baseline.fts_hits, structural.fts_hits
    );

    // Baseline sanity: Phase 1D mode must produce ZERO structural artifacts.
    assert_eq!(baseline.symbol_definitions, 0);
    assert_eq!(baseline.resolved_imports, 0);
    assert_eq!(baseline.resolved_references, 0);
    assert_eq!(baseline.navigation_links, 0);
    assert!(baseline.fts_hits > 0, "generic search still answers");

    // Gate assertions — keep structure only where it measurably helps:
    assert!(
        structural.symbol_definitions > 0,
        "definition/symbol lookup must improve"
    );
    assert!(
        structural.resolved_imports > 0,
        "import resolution must improve"
    );
    assert!(
        structural.resolved_references > 0,
        "reference resolution must improve"
    );
    assert!(
        structural.navigation_links > 0,
        "navigation (unit↔node anchors) must improve"
    );
    assert!(
        structural.fts_hits >= baseline.fts_hits,
        "plain lexical coverage MUST NOT regress"
    );
}
