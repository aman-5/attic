mod common;
use attic_spec::{
    citations::Kind,
    verifier::{self, Finding},
    waves,
};
use common::Fixture;
fn scopes() -> Vec<Finding> {
    let f = Fixture::new("", "TARGET_MODIFY repo src/lib.rs", true);
    let run = verifier::verify(
        &attic_spec::service::SpecService::doc_bytes(&f.registered()).unwrap(),
        1,
        false,
        std::slice::from_ref(&f.repo),
        &[],
        &f.service.config.validation,
    )
    .unwrap();
    ["consumer", "provider", "unlinked"]
        .iter()
        .map(|name| {
            let mut c = run.citations[0].clone();
            c.repository_id = (*name).into();
            c.repository_root = format!("root-{name}");
            c
        })
        .collect()
}
#[test]
fn provider_first_reachability_through_nonwritten_repositories_and_scc() {
    let citations = scopes();
    let edges = vec![
        ("consumer".into(), "middle".into(), "MANIFEST".into()),
        ("middle".into(), "provider".into(), "MANIFEST".into()),
    ];
    let plan = waves::plan(&citations, &edges).unwrap();
    let n = |repo: &str| {
        plan.iter()
            .find(|w| w.repository_id == repo)
            .unwrap()
            .wave_number
    };
    assert_eq!(n("provider"), 1);
    assert_eq!(n("unlinked"), 1);
    assert_eq!(n("consumer"), 2);
    let mut cycle = edges;
    cycle.push(("provider".into(), "consumer".into(), "MANIFEST".into()));
    let plan = waves::plan(&citations, &cycle).unwrap();
    assert!(plan.iter().all(|w| w.wave_number == 1));
    assert!(
        plan.iter()
            .filter(|w| w.repository_id != "unlinked")
            .all(|w| w.cycle && w.ordering_basis == "MANIFEST_DEPENDENCY_CYCLE")
    );
}
#[test]
fn explicit_edges_do_not_relabel_unrelated_manifest_ordering() {
    let mut citations = scopes();
    let mut explicit = citations[0].clone();
    explicit.citation.kind = Kind::CrossRepoDependency;
    explicit.repository_id = "unrelated-a".into();
    explicit.dependency_repository_id = Some("unrelated-b".into());
    citations.push(explicit.clone());
    let edges = vec![("consumer".into(), "provider".into(), "MANIFEST".into())];
    let plan = waves::plan(&citations, &edges).unwrap();
    assert!(
        plan.iter()
            .filter(|w| w.repository_id != "unlinked")
            .all(|w| w.ordering_basis == "MANIFEST_DEPENDENCY")
    );
    explicit.repository_id = "provider".into();
    explicit.dependency_repository_id = Some("consumer".into());
    citations.push(explicit);
    let cycle = waves::plan(&citations, &edges).unwrap();
    assert!(
        cycle
            .iter()
            .filter(|w| w.repository_id != "unlinked")
            .all(|w| w.cycle && w.ordering_basis == "EXPLICIT_SPEC_DECLARATION")
    );
}
