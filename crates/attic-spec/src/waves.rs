use crate::{Result, SpecError, citations::Kind, verifier::Finding};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wave {
    pub cycle: bool,
    pub ordering_evidence: Vec<String>,
    pub wave_number: u32,
    pub repository_id: String,
    pub root: String,
    pub allowed_paths: Vec<String>,
    pub allowed_globs: Vec<String>,
    pub ordering_basis: String,
    pub status: String,
}

/// Input edges are consumer -> provider. Reachability precedes write-set restriction.
pub fn plan(citations: &[Finding], edges: &[(String, String, String)]) -> Result<Vec<Wave>> {
    let writes: BTreeSet<_> = citations
        .iter()
        .filter(|f| f.citation.kind.writes())
        .map(|f| f.repository_id.clone())
        .collect();
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (a, b, _) in edges {
        graph.entry(a.clone()).or_default().insert(b.clone());
    }
    let manifest = graph.clone();
    for f in citations
        .iter()
        .filter(|f| f.citation.kind == Kind::CrossRepoDependency)
    {
        if let Some(dep) = &f.dependency_repository_id {
            graph
                .entry(f.repository_id.clone())
                .or_default()
                .insert(dep.clone());
        }
    }
    let mut reaches = BTreeMap::new();
    let mut manifest_reaches = BTreeMap::new();
    for node in &writes {
        let mut reached = BTreeSet::new();
        let mut queue = vec![node.clone()];
        while let Some(n) = queue.pop() {
            if reached.insert(n.clone()) {
                queue.extend(graph.get(&n).into_iter().flatten().cloned());
            }
        }
        reaches.insert(node.clone(), reached);
        let mut reached = BTreeSet::new();
        let mut queue = vec![node.clone()];
        while let Some(n) = queue.pop() {
            if reached.insert(n.clone()) {
                queue.extend(manifest.get(&n).into_iter().flatten().cloned());
            }
        }
        manifest_reaches.insert(node.clone(), reached);
    }
    let mut groups = Vec::<BTreeSet<String>>::new();
    let mut assigned = BTreeSet::new();
    for node in &writes {
        if assigned.contains(node) {
            continue;
        }
        let component: BTreeSet<_> = writes
            .iter()
            .filter(|other| reaches[node].contains(*other) && reaches[*other].contains(node))
            .cloned()
            .collect();
        assigned.extend(component.iter().cloned());
        groups.push(component);
    }
    let mut done = BTreeSet::new();
    let mut result = Vec::new();
    let mut number = 1;
    while done.len() < writes.len() {
        let ready: Vec<_> = groups
            .iter()
            .filter(|g| {
                g.is_disjoint(&done)
                    && g.iter().all(|n| {
                        reaches[n]
                            .intersection(&writes)
                            .all(|dep| g.contains(dep) || done.contains(dep))
                    })
            })
            .cloned()
            .collect();
        if ready.is_empty() {
            return Err(SpecError::new(
                "ERR_WAVE_GRAPH",
                "condensed dependency graph cannot be layered",
            ));
        }
        for component in ready {
            for repo in &component {
                let f = citations
                    .iter()
                    .find(|f| &f.repository_id == repo && f.citation.kind.writes())
                    .ok_or_else(|| SpecError::new("ERR_WAVE_GRAPH", "write scope missing"))?;
                let needs_explicit = writes.iter().any(|other| {
                    (reaches[repo].contains(other) && !manifest_reaches[repo].contains(other))
                        || (reaches[other].contains(repo)
                            && !manifest_reaches[other].contains(repo))
                });
                let basis = if needs_explicit {
                    "EXPLICIT_SPEC_DECLARATION"
                } else if component.len() > 1 {
                    "MANIFEST_DEPENDENCY_CYCLE"
                } else if reaches[repo].intersection(&writes).any(|p| p != repo)
                    || writes
                        .iter()
                        .any(|r| r != repo && reaches[r].contains(repo))
                {
                    "MANIFEST_DEPENDENCY"
                } else {
                    "UNLINKED_SINGLE_WAVE"
                };
                let mut paths: BTreeSet<_> = citations
                    .iter()
                    .filter(|f| {
                        &f.repository_id == repo
                            && f.citation.kind.writes()
                            && f.citation.kind != Kind::AllowGlob
                    })
                    .filter_map(|f| f.citation.path.clone())
                    .collect();
                let globs: BTreeSet<_> = citations
                    .iter()
                    .filter(|f| &f.repository_id == repo && f.citation.kind == Kind::AllowGlob)
                    .filter_map(|f| f.citation.path.clone())
                    .collect();
                result.push(Wave {
                    cycle: component.len() > 1,
                    ordering_evidence: edges
                        .iter()
                        .map(|(a, b, basis)| format!("{basis}: {a} -> {b}"))
                        .chain(
                            citations
                                .iter()
                                .filter(|f| f.citation.kind == Kind::CrossRepoDependency)
                                .filter_map(|f| {
                                    f.dependency_repository_id
                                        .as_ref()
                                        .map(|b| format!("EXPLICIT: {} -> {b}", f.repository_id))
                                }),
                        )
                        .collect(),
                    wave_number: number,
                    repository_id: repo.clone(),
                    root: f.repository_root.clone(),
                    allowed_paths: std::mem::take(&mut paths).into_iter().collect(),
                    allowed_globs: globs.into_iter().collect(),
                    ordering_basis: basis.into(),
                    status: if number == 1 {
                        "IN_PROGRESS"
                    } else {
                        "PENDING"
                    }
                    .into(),
                });
            }
            done.extend(component);
        }
        number += 1;
    }
    Ok(result)
}

pub fn covers(wave: &Wave, path: &str) -> bool {
    wave.allowed_paths
        .iter()
        .any(|p| p == path || (cfg!(windows) && p.to_lowercase() == path.to_lowercase()))
        || wave.allowed_globs.iter().any(|g| {
            globset::GlobBuilder::new(g)
                .case_insensitive(cfg!(windows))
                .build()
                .is_ok_and(|g| g.compile_matcher().is_match(path))
        })
}
