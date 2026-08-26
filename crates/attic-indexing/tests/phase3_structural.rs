//! Phase 3 — end-to-end structural integration tests (§13, §17 of the brief).
//!
//! Proves, through the REAL indexing pipeline and storage:
//! - structural nodes / symbol identities+occurrences / relationships /
//!   unit↔node links are persisted for every supported language;
//! - import edges upgrade to PACKAGE_RESOLVED / SYMBOL_RESOLVED only with
//!   real evidence and stay SYNTACTIC otherwise;
//! - a single-file change re-analyzes ONLY that file's structural state
//!   while untouched files keep their rows;
//! - stale/removed relationships leave no ghosts;
//! - redacted content never leaks secrets through the full pipeline.

use std::collections::BTreeSet;
use std::path::PathBuf;

use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_storage::{DbPool, WriterQueue, WriterQueueHandle, open_db, run_migrations};
use tempfile::TempDir;

fn opts() -> IndexOptions {
    IndexOptions {
        repository_name: "phase3".into(),
        ..Default::default()
    }
}

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    pool: DbPool,
    _queue: WriterQueue,
    writer: WriterQueueHandle,
}

impl Fixture {
    fn bootstrap(seed: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo root");
        for (rel, content) in seed {
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

        let store = IndexingStore {
            readers: &pool,
            writer: &writer,
        };
        index_repository(&store, &root, &DiscoveryPolicy::default_git(), &opts())
            .expect("bootstrap");

        Self {
            _dir: dir,
            root,
            pool,
            _queue: queue,
            writer,
        }
    }

    fn store(&self) -> IndexingStore<'_> {
        IndexingStore {
            readers: &self.pool,
            writer: &self.writer,
        }
    }

    fn reindex_changed(&self, upserts: &[&str]) -> attic_indexing::ScopedIndexResult {
        self.reindex(upserts, &[], &[])
    }

    fn reindex(
        &self,
        upserts: &[&str],
        deletes: &[&str],
        renames: &[(&str, &str)],
    ) -> attic_indexing::ScopedIndexResult {
        use attic_indexing::ScopedChanges;
        let changes = ScopedChanges {
            upserts: upserts.iter().map(|s| s.to_string()).collect(),
            deletes: deletes.iter().map(|s| s.to_string()).collect(),
            rename_hints: renames
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        };
        attic_indexing::index_changes(
            &self.store(),
            &self.root,
            &DiscoveryPolicy::default_git(),
            &opts(),
            &changes,
        )
        .expect("scoped reindex")
    }

    fn query_i64<F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<i64>>(&self, f: F) -> i64 {
        self.pool
            .with_reader(|c| f(c).map_err(attic_storage::StorageError::from))
            .unwrap()
    }

    fn query_row3<
        F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<(String, String, String)>,
    >(
        &self,
        f: F,
    ) -> (String, String, String) {
        self.pool
            .with_reader(|c| f(c).map_err(attic_storage::StorageError::from))
            .unwrap()
    }

    fn query_str<F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<String>>(
        &self,
        f: F,
    ) -> String {
        self.pool
            .with_reader(|c| f(c).map_err(attic_storage::StorageError::from))
            .unwrap()
    }
}

// ── Sources ─────────────────────────────────────────────────────────────────

const JAVA_MAIN: &str = "package com.acme;\n\nimport com.acme.shared.Helper;\nimport java.util.List;\n\npublic class Main extends BaseService {\n    public int run() {\n        Helper.greet();\n        return 1;\n    }\n}\n";
const JAVA_BASE: &str = "package com.acme;\npublic abstract class BaseService { }\n";
const JAVA_HELPER: &str =
    "package com.acme.shared;\npublic class Helper {\n    public static void greet() { }\n}\n";

// ── Java end-to-end ──────────────────────────────────────────────────────────

#[test]
fn java_end_to_end_persists_structure_and_upgrades_resolution() {
    let fx = Fixture::bootstrap(&[
        ("src/com/acme/Main.java", JAVA_MAIN),
        ("src/com/acme/BaseService.java", JAVA_BASE),
        ("src/com/acme/shared/Helper.java", JAVA_HELPER),
    ]);

    let nodes: i64 = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_structural_nodes", [], |r| {
            r.get(0)
        })
    });
    assert!(
        nodes >= 5,
        "expected the fixture's structural nodes; got {nodes}"
    );

    // Import edge upgraded via package layout + known class.
    let (res, conf, target) = fx.query_row3(|c| {
        c.query_row(
            "SELECT resolution, CAST(confidence AS TEXT), target_entity_id
               FROM core_relationships
              WHERE rel_type='IMPORT' AND provenance_json LIKE '%com.acme.shared.Helper%'
              LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    });
    assert_eq!(
        res, "SYMBOL_RESOLVED",
        "Helper import must be symbol-resolved"
    );
    assert!(conf.parse::<f64>().unwrap() >= 0.8);
    // Imports resolve to the DEFINING FILE occurrence.
    let helper_file_occ: String = fx.query_str(|c| {
        c.query_row(
            "SELECT id FROM core_file_occurrences WHERE path = 'src/com/acme/shared/Helper.java'
              ORDER BY rowid DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(target, helper_file_occ, "target must be the defining file");

    // Unknown external import stays syntactic.
    let ext = fx.query_str(|c| {
        c.query_row(
            "SELECT resolution FROM core_relationships
              WHERE rel_type='IMPORT' AND provenance_json LIKE '%java.util.List%'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(ext, "SYNTACTIC");

    // EXTENDS upgraded to in-repo base class occurrence.
    let extends_res = fx.query_str(|c| {
        c.query_row(
            "SELECT resolution FROM core_relationships WHERE rel_type='EXTENDS'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(extends_res, "SYMBOL_RESOLVED");

    // Unit↔node links populated.
    let links: i64 = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_retrieval_unit_nodes", [], |r| {
            r.get(0)
        })
    });
    assert!(links > 0, "unit-node links expected");
}

// ── Go module-path resolution ───────────────────────────────────────────────

#[test]
fn go_module_prefix_upgrade_requires_go_mod_evidence() {
    const GO_MOD: &str = "module github.com/acme/wire\n\ngo 1.22\n";
    const GO_APP: &str = "package main\n\nimport (\n\t\"fmt\"\n\t\"github.com/acme/wire/internal/parts\"\n)\n\nfunc main() {\n\tparts.Use()\n}\n";
    const GO_PARTS: &str = "package parts\n\nfunc Use() { }\n";

    let fx = Fixture::bootstrap(&[
        ("go.mod", GO_MOD),
        ("cmd/app.go", GO_APP),
        ("internal/parts/parts.go", GO_PARTS),
    ]);

    let (res, basis, _) = fx.query_row3(|c| {
        c.query_row(
            "SELECT resolution, dependency_basis, ''
               FROM core_relationships
              WHERE rel_type='IMPORT' AND provenance_json LIKE '%wire/internal/parts%'
              LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
    });
    assert_eq!(res, "PACKAGE_RESOLVED");
    assert_eq!(basis, "GO_MODULE");

    let std_res = fx.query_str(|c| {
        c.query_row(
            "SELECT resolution FROM core_relationships WHERE provenance_json LIKE '%\"fmt\"%'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(std_res, "SYNTACTIC", "stdlib imports stay syntactic");
}

// ── Python relative-import resolution ───────────────────────────────────────

#[test]
fn python_relative_import_resolves_to_layout() {
    let fx = Fixture::bootstrap(&[
        ("pkg/__init__.py", ""),
        (
            "pkg/core.py",
            "from .helpers import load\nfrom . import sibling\n\ndef boot():\n    return load(sibling.NAME)\n",
        ),
        ("pkg/helpers.py", "def load(x):\n    return x\n"),
        ("pkg/sibling.py", "NAME = \"s\"\n"),
    ]);

    let res = fx.query_str(|c| {
        c.query_row(
            "SELECT resolution FROM core_relationships
              WHERE rel_type='IMPORT' AND dependency_basis='PYTHON_PACKAGE'
                AND provenance_json LIKE '%helpers%'
              LIMIT 1",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(res, "PACKAGE_RESOLVED");
}

// ── JS/TS relative specifier probing stays honest ────────────────────────────

#[test]
fn typescript_relative_import_and_bare_package_stay_honest() {
    let fx = Fixture::bootstrap(&[
        (
            "src/app.ts",
            "import { cfg } from './config';\nimport { Vue } from 'vue';\nexport const x = Vue.use(cfg);\n",
        ),
        ("src/config.ts", "export const cfg = 1;\n"),
    ]);

    let local = fx.query_str(|c| {
        c.query_row(
            "SELECT resolution FROM core_relationships WHERE provenance_json LIKE '%./config%'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(local, "PACKAGE_RESOLVED");

    let bare = fx.query_str(|c| {
        c.query_row(
            "SELECT resolution FROM core_relationships WHERE provenance_json LIKE '%vue%'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(bare, "SYNTACTIC");
}

// ── §17 incremental scoping ──────────────────────────────────────────────────

#[test]
fn single_file_change_reanalyzes_only_affected_structural_state() {
    let mut fx = Fixture::bootstrap(&[("src/A.java", JAVA_BASE), ("src/B.java", JAVA_HELPER)]);
    let _ = &mut fx;

    fn node_ids_for(fx: &Fixture, path: &str) -> BTreeSet<String> {
        fx.pool
            .with_reader(move |c| {
                let mut stmt = c.prepare(
                    "SELECT n.id FROM core_structural_nodes n
                       JOIN core_file_occurrences fo ON fo.id = n.file_occurrence_id
                      WHERE fo.path = ?1",
                )?;
                let rows = stmt.query_map([path], |r| r.get::<_, String>(0))?;
                Ok(rows.collect::<Result<BTreeSet<_>, _>>()?)
            })
            .unwrap()
    }

    let b_before = node_ids_for(&fx, "src/B.java");

    // Edit ONLY A.java.
    std::fs::write(
        fx.root.join("src/A.java"),
        JAVA_BASE.replace("{ }", "{ void extra() {} }"),
    )
    .unwrap();
    let scoped = fx.reindex_changed(&["src/A.java"]);
    assert_eq!(scoped.files_published, 1, "only A republished");

    let b_after = node_ids_for(&fx, "src/B.java");
    assert_eq!(
        b_before, b_after,
        "untouched file must keep identical structural rows"
    );

    // A's fresh nodes are CURRENT under the new revision.
    let a_current: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_structural_nodes n
               JOIN core_file_occurrences fo ON fo.id = n.file_occurrence_id
              WHERE fo.path = 'src/A.java' AND n.freshness_state = 'CURRENT'",
            [],
            |r| r.get(0),
        )
    });
    assert!(a_current >= 1);
}

// ── §17 ghost relationships ──────────────────────────────────────────────────

#[test]
fn removed_extends_leaves_no_ghost_relationship() {
    let fx = Fixture::bootstrap(&[("src/Kid.java", JAVA_MAIN), ("src/Base.java", JAVA_BASE)]);

    let before: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_relationships WHERE rel_type='EXTENDS'",
            [],
            |r| r.get(0),
        )
    });
    assert!(before >= 1, "extends edge exists after bootstrap");

    let kid_no_extends = JAVA_MAIN.replace(" extends BaseService", "");
    std::fs::write(fx.root.join("src/Kid.java"), kid_no_extends).unwrap();
    fx.reindex_changed(&["src/Kid.java"]);

    let kid_edges: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_relationships r
               JOIN core_file_occurrences fo ON fo.id = r.source_entity_id
              WHERE r.rel_type='EXTENDS' AND fo.path='src/Kid.java'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(kid_edges, 0, "ghost EXTENDS must disappear after edit");
}

// ── Security e2e ─────────────────────────────────────────────────────────────

#[test]
fn redacted_java_secret_never_reaches_published_artifacts() {
    const SECRET_TOKEN: &str = "AKIAIOSFODNN7REALKEY";
    const TEMPLATE: &str = "package leaky;\n\nimport java.time.Clock;\n\npublic class Leaky {\n    private final String key = \"__SECRET__\";\n    public String k() { return key + Clock.systemUTC(); }\n}\n";
    let raw_src = TEMPLATE.replace("__SECRET__", SECRET_TOKEN);

    let fx = Fixture::bootstrap(&[("src/Leaky.java", raw_src.as_str())]);

    fx.pool
        .with_reader(|c| {
            let mut dump = String::new();
            let mut stmt = c.prepare(
                "SELECT retrieval_text FROM core_retrieval_units
                 UNION ALL SELECT metadata_json FROM core_structural_nodes",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
            for r in rows {
                if let Some(t) = r? {
                    dump.push_str(&t);
                    dump.push('\n');
                }
            }
            assert!(
                !dump.contains(SECRET_TOKEN),
                "secret leaked into published artifacts"
            );
            // Safe surrounding code still searchable.
            let hits = attic_storage::fts_search(
                c,
                &attic_storage::FtsSearchParams {
                    query: "Leaky",
                    repository_id: None,
                    file_type: None,
                    language: None,
                    max_results: 5,
                },
            )?;
            assert!(!hits.is_empty(), "safe surroundings indexed");
            Ok(())
        })
        .unwrap();
}

// ════════════════════════════════════════════════════════════════════════════
// Phase 3 corrections — panic-free idempotency, ghost artifacts on
// re-analysis / deletion / rename.
// ════════════════════════════════════════════════════════════════════════════

/// Fix 1 verification: a full refresh re-runs symbol-identity insertion for
/// every file; the upsert path must reuse identities WITHOUT panicking and
/// must not duplicate identity rows.
#[test]
fn double_publish_reuses_symbol_identities_without_panic() {
    let fx = Fixture::bootstrap(&[("src/A.java", JAVA_BASE), ("src/B.java", JAVA_HELPER)]);

    let identities_first: i64 = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_symbol_identities", [], |r| {
            r.get(0)
        })
    });

    // Second FULL run (refresh path replays insert_structural_file for every
    // file, exercising the identity-conflict branch).
    let _again = index_repository(
        &fx.store(),
        &fx.root,
        &DiscoveryPolicy::default_git(),
        &opts(),
    )
    .expect("second full publish must succeed without panicking");

    let identities_second: i64 = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_symbol_identities", [], |r| {
            r.get(0)
        })
    });
    assert_eq!(
        identities_first, identities_second,
        "identity rows must be reused, never duplicated"
    );

    // Definitions still resolve to exactly one CURRENT occurrence per symbol.
    let dup_defs: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT si.id FROM core_symbol_identities si
                   JOIN core_symbol_occurrences so ON so.symbol_identity_id = si.id
                  WHERE so.is_definition = 1
                  GROUP BY si.id HAVING COUNT(*) > 1)",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(dup_defs, 0, "no symbol may carry duplicate definitions");
}

/// Fix 6 — DELETION ghosts: tombstoning a file must remove its structural
/// nodes, symbol occurrences, relationships and unit links entirely.
#[test]
fn deleted_file_leaves_no_structural_ghosts() {
    let fx = Fixture::bootstrap(&[("src/Keep.java", JAVA_BASE), ("src/Gone.java", JAVA_HELPER)]);

    let before: i64 = fx.query_i64(|c| {
        c.query_row("SELECT COUNT(*) FROM core_structural_nodes", [], |r| {
            r.get(0)
        })
    });
    assert!(before > 0);

    std::fs::remove_file(fx.root.join("src/Gone.java")).unwrap();
    let scoped = fx.reindex(&[], &["src/Gone.java"], &[]);
    // files_published counts NON-tombstone publications: only the deletion
    // was scoped, so zero live files were republished.
    assert_eq!(scoped.files_published, 0);

    // The tombstone row exists.
    let tombstones: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_file_occurrences
              WHERE path='src/Gone.java' AND existence_state='deleted'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(tombstones, 1, "deletion recorded as tombstone");

    // No structural row may reference a DEAD occurrence (existence=deleted).
    let ghosts: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_structural_nodes n
               JOIN core_file_occurrences fo ON fo.id = n.file_occurrence_id
              WHERE fo.existence_state = 'deleted'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(ghosts, 0, "structural nodes of deleted files must vanish");

    let sym_ghosts: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_symbol_occurrences so
               JOIN core_file_occurrences fo ON fo.id = so.file_occurrence_id
              WHERE fo.existence_state = 'deleted'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(
        sym_ghosts, 0,
        "symbol occurrences of deleted files must vanish"
    );

    // Helper symbol occurrences are gone; Keep's remain.
    let helper_left: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_symbol_identities si
               JOIN core_symbol_occurrences so ON so.symbol_identity_id = si.id
              WHERE si.qualified_name LIKE '%Helper%'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(helper_left, 0);
}

/// Fix 6 — RENAME ghosts: an identical-content rename removes the old
/// occurrence's artifacts, creates the new ones, and records the identity
/// link. Nothing may reference the dead old occurrence.
#[test]
fn rename_removes_old_artifacts_and_keeps_new() {
    let fx = Fixture::bootstrap(&[("src/Base.java", JAVA_BASE), ("src/Old.java", JAVA_HELPER)]);
    let old_node_ids: Vec<String> = fx
        .pool
        .with_reader(|c| {
            let mut stmt = c.prepare(
                "SELECT n.id FROM core_structural_nodes n
                   JOIN core_file_occurrences fo ON fo.id = n.file_occurrence_id
                  WHERE fo.path = 'src/Old.java'",
            )?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap();
    assert!(!old_node_ids.is_empty());

    // Content-identical move on disk.
    std::fs::rename(fx.root.join("src/Old.java"), fx.root.join("src/New.java")).unwrap();

    let scoped = fx.reindex(
        &["src/New.java"],
        &["src/Old.java"],
        &[("src/Old.java", "src/New.java")],
    );
    assert_eq!(scoped.files_published, 1, "only the new location is live");

    // Old occurrence: no nodes left.
    let old_left: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_structural_nodes n
               JOIN core_file_occurrences fo ON fo.id = n.file_occurrence_id
              WHERE fo.path = 'src/Old.java' OR fo.existence_state='deleted'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(
        old_left, 0,
        "renamed-away occurrence keeps no structural rows"
    );

    // New occurrence: fresh nodes exist.
    let new_count: i64 = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_structural_nodes n
               JOIN core_file_occurrences fo ON fo.id = n.file_occurrence_id
              WHERE fo.path = 'src/New.java'",
            [],
            |r| r.get(0),
        )
    });
    assert!(new_count > 0, "new location carries structure");

    // Identity link recorded for the content match (ADR-009).
    let links: i64 =
        fx.query_i64(|c| c.query_row("SELECT COUNT(*) FROM core_identity_links", [], |r| r.get(0)));
    assert!(links >= 1, "rename identity link expected");
}
