use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, index_repository};
use attic_storage::{DbPool, WriterQueue, open_db, run_migrations};
use tempfile::TempDir;

fn opts() -> IndexOptions {
    IndexOptions {
        repository_name: "tier1-resolvers".into(),
        ..Default::default()
    }
}

struct Fixture {
    pool: DbPool,
    _queue: WriterQueue,
    _dir: TempDir,
}

impl Fixture {
    fn bootstrap(seed: &[(&str, &str)]) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("repo root");
        for (rel, content) in seed {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }

        let db_path = dir.path().join("attic.db");
        let (conn, pool) = open_db(&db_path).expect("open db");
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
            pool,
            _queue: queue,
            _dir: dir,
        }
    }

    fn query_str<F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<String>>(
        &self,
        f: F,
    ) -> String {
        self.pool
            .with_reader(|c| f(c).map_err(attic_storage::StorageError::from))
            .unwrap()
    }

    fn query_i64<F: FnOnce(&rusqlite::Connection) -> rusqlite::Result<i64>>(&self, f: F) -> i64 {
        self.pool
            .with_reader(|c| f(c).map_err(attic_storage::StorageError::from))
            .unwrap()
    }
}

#[test]
fn csharp_imports_resolve_to_namespace_suffix_matches() {
    let fx = Fixture::bootstrap(&[
        (
            "src/Program.cs",
            "global using WidgetAlias = Acme.Models.Widget;\nnamespace Acme.App { public class Program { public Program() { } } }\n",
        ),
        (
            "src/Acme/Models/Widget.cs",
            "namespace Acme.Models { public class Widget { } }\n",
        ),
    ]);

    let resolution = fx.query_str(|c| {
        c.query_row(
            "SELECT resolution FROM core_relationships
              WHERE rel_type='IMPORT' AND dependency_basis='CSHARP_NAMESPACE'
                AND provenance_json LIKE '%Acme.Models.Widget%'
              LIMIT 1",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(resolution, "PACKAGE_RESOLVED");

    let target = fx.query_str(|c| {
        c.query_row(
            "SELECT target_entity_id FROM core_relationships
              WHERE rel_type='IMPORT' AND dependency_basis='CSHARP_NAMESPACE'
                AND provenance_json LIKE '%Acme.Models.Widget%'
              LIMIT 1",
            [],
            |r| r.get(0),
        )
    });
    let widget_occ = fx.query_str(|c| {
        c.query_row(
            "SELECT id FROM core_file_occurrences WHERE path = 'src/Acme/Models/Widget.cs'
              ORDER BY rowid DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(target, widget_occ);
}

#[test]
fn rust_mod_and_use_imports_resolve_to_local_module_files() {
    let fx = Fixture::bootstrap(&[
        (
            "src/lib.rs",
            "mod util;\nuse crate::util::helper;\npub fn boot() { helper(); }\n",
        ),
        ("src/util.rs", "pub fn helper() {}\n"),
    ]);

    let util_occ = fx.query_str(|c| {
        c.query_row(
            "SELECT id FROM core_file_occurrences WHERE path = 'src/util.rs'
              ORDER BY rowid DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
    });
    let count = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_relationships
              WHERE rel_type='IMPORT' AND dependency_basis='RUST_MODULE'
                AND resolution='PACKAGE_RESOLVED' AND target_entity_id = ?1",
            [util_occ],
            |r| r.get(0),
        )
    });
    assert!(
        count >= 2,
        "expected both `mod util;` and `use crate::util::helper;` to resolve"
    );
}

#[test]
fn dockerfile_copy_inputs_resolve_and_stage_edges_remain_local() {
    let fx = Fixture::bootstrap(&[
        (
            "docker/Dockerfile",
            "FROM alpine AS builder\nCOPY app.sh /usr/local/bin/app.sh\nFROM builder AS runner\nCOPY --from=builder /usr/local/bin/app.sh /usr/local/bin/app.sh\nADD config.json /etc/app/config.json\n",
        ),
        ("docker/app.sh", "#!/bin/sh\necho ok\n"),
        ("docker/config.json", "{\"ok\":true}\n"),
    ]);

    let import_count = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_relationships
              WHERE rel_type='IMPORT' AND dependency_basis='DOCKER_CONTEXT'
                AND resolution='PACKAGE_RESOLVED'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(import_count, 2, "COPY + ADD should resolve to repo files");
    let rels = fx.query_str(|c| {
        c.query_row(
            "SELECT COALESCE(group_concat(rel_type || ':' || target_entity_id || ':' || resolution, '|'), '')
               FROM core_relationships",
            [],
            |r| r.get(0),
        )
    });

    let extends_count = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_relationships
              WHERE rel_type='EXTENDS' AND resolution='SYMBOL_RESOLVED'
                AND provenance_json LIKE '%\"target_name\":\"builder\"%'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(
        extends_count, 1,
        "runner should extend the builder stage; got {rels}"
    );

    let copy_from_count = fx.query_i64(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM core_relationships
              WHERE rel_type='REFERENCES' AND resolution='SYMBOL_RESOLVED'
                AND provenance_json LIKE '%\"target_name\":\"builder\"%'",
            [],
            |r| r.get(0),
        )
    });
    assert_eq!(
        copy_from_count, 1,
        "COPY --from=builder should point at the prior stage; got {rels}"
    );
}
