//! Delete / rename / restore / edit scenarios (plan §7, Phase 6): files
//! that import a changed file keep correct edges after every change.

mod common;

use attic_incremental::{GateConfig, ensure_current};
use common::{Fixture, write_file};

fn gate(fx: &Fixture) {
    ensure_current(
        &fx.pool,
        &fx.writer,
        fx.root(),
        &fx.policy(),
        &GateConfig::default(),
    )
    .expect("ensure_current");
}

/// `(target_entity_id, provenance)` of the latest `importer`'s IMPORT edges.
fn import_edges(fx: &Fixture, importer: &str) -> Vec<(String, serde_json::Value)> {
    fx.pool
        .with_reader(|c| {
            let mut stmt = c.prepare(
                "SELECT r.target_entity_id, r.provenance_json
                   FROM core_relationships r
                   JOIN core_file_occurrences fo ON r.source_entity_id = fo.id
                  WHERE r.rel_type = 'IMPORT' AND fo.path = ?1
                    AND fo.occurrence_seq = (SELECT MAX(occurrence_seq)
                                               FROM core_file_occurrences WHERE path = ?1)",
            )?;
            let rows = stmt.query_map([importer], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default(),
                ))
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
}

fn latest_occurrence(fx: &Fixture, path: &str) -> String {
    fx.occurrence(path).expect("occurrence").id
}

const IMPORTER: &str =
    "import { helper } from './helper.js';\nexport function run() { return helper(); }\n";

#[test]
fn importer_edges_follow_edit_delete_restore_and_rename() {
    let fx = Fixture::new(&[
        ("app.js", IMPORTER),
        ("helper.js", "export function helper() { return 1; }\n"),
    ]);
    gate(&fx);
    let edges = import_edges(&fx, "app.js");
    assert_eq!(edges.len(), 1, "{edges:?}");
    assert_eq!(edges[0].0, latest_occurrence(&fx, "helper.js"));

    // S30: editing the imported file keeps the importer linked.
    write_file(
        fx.root(),
        "helper.js",
        "export function helper() { return 2; }\n",
    );
    gate(&fx);
    let edges = import_edges(&fx, "app.js");
    assert_eq!(edges[0].0, latest_occurrence(&fx, "helper.js"), "{edges:?}");

    // S8: deleting it turns the edge into "imports missing file".
    std::fs::remove_file(fx.root().join("helper.js")).unwrap();
    gate(&fx);
    let edges = import_edges(&fx, "app.js");
    assert!(edges[0].0.starts_with("logical:"), "{edges:?}");
    assert_eq!(edges[0].1["target_missing"], "helper.js", "{edges:?}");

    // S10: restoring it reconnects the importer (re-resolved this check).
    write_file(
        fx.root(),
        "helper.js",
        "export function helper() { return 3; }\n",
    );
    gate(&fx);
    let edges = import_edges(&fx, "app.js");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].0, latest_occurrence(&fx, "helper.js"), "{edges:?}");

    // S11: renaming it away leaves the importer pointing at nothing.
    std::fs::rename(fx.root().join("helper.js"), fx.root().join("helpers2.js")).unwrap();
    gate(&fx);
    let edges = import_edges(&fx, "app.js");
    assert!(edges[0].0.starts_with("logical:"), "{edges:?}");
}
