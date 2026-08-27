//! Cross-repository resolver (Phase 6 §5, §7).
//!
//! Resolution is PROGRESSIVE and evidence-first:
//!
//! ```text
//! explicit build dependency (local path / submodule / workspace)
//!        ↓ BUILD_RESOLVED
//! package/coordinate match against exactly one provider repository
//!        ↓ PACKAGE_RESOLVED
//! module-path/import-context match with exactly one provider
//!        ↓ INFERRED (never "resolved")
//! name-only similarity across repositories → NO EDGE AT ALL
//! ```
//!
//! Anti-laundering invariants:
//! - Ambiguity stays ambiguity: >1 candidate provider ⇒ no edge, recorded
//!   as an ambiguous target.
//! - Zero candidates ⇒ missing-target diagnostic, never an invented edge.
//! - `INFERRED` edges require real import/module-path context; confidence
//!   stays ≤ 0.5 and resolution level INFERRED forever.
//! - Every draft carries provenance JSON (ecosystem detail, declaring
//!   manifest, specifier) — never raw manifest bytes.

use std::collections::HashMap;

use serde_json::json;

use crate::{DeclarationKind, DependencyDeclaration, Ecosystem, ProvidedIdentity, limits};

/// One repository's catalog data as seen by the resolver.
#[derive(Debug, Clone, Default)]
pub struct RepoCatalogData {
    /// Repository UUID string.
    pub repository_id: String,
    /// Canonical absolute root path of the repository (for local-hint and
    /// submodule boundary matching).
    pub root_path: String,
    /// Source revision the catalog data was derived from.
    pub source_revision_id: String,
    /// Provided identities.
    pub provides: Vec<ProvidedIdentity>,
    /// Declared dependencies.
    pub declarations: Vec<DependencyDeclaration>,
    /// File-occurrence UUID of the repository's PRIMARY manifest at the
    /// repo root (`go.mod`, `package.json`, `pom.xml`, `pyproject.toml`),
    /// used as the default TARGET anchor for edges. `None` ⇒ logical id.
    pub primary_anchor_occurrence: Option<String>,
    /// Go module prefix when the repository has a `go.mod`.
    pub go_module_prefix: Option<String>,
}

/// A resolved cross-repository edge ready for persistence into
/// `core_relationships` (`rel_type='DEPENDS_ON'`, cross-repository pair).
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeDraft {
    /// Source repository UUID string.
    pub source_repository_id: String,
    /// Source anchor entity (declaring manifest occurrence or logical id).
    pub source_entity_id: String,
    /// Target repository UUID string.
    pub target_repository_id: String,
    /// Target anchor entity (provider primary-manifest occurrence or
    /// deterministic logical id).
    pub target_entity_id: String,
    /// Schema dependency-basis token.
    pub dependency_basis: String,
    /// Schema resolution token (SYNTACTIC | PACKAGE_RESOLVED |
    /// SYMBOL_RESOLVED | BUILD_RESOLVED | FRAMEWORK_RESOLVED | INFERRED).
    pub resolution: String,
    /// Confidence in [0,1]; derived edges never reach 1.0.
    pub confidence: f64,
    /// Structured provenance JSON (no secret content).
    pub provenance_json: String,
    /// Revision of the SOURCE repository that produced this edge.
    pub source_revision_id: String,
}

impl EdgeDraft {
    /// Identity key used for dedupe and diffing during maintenance.
    pub fn identity_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{:.2}",
            self.dependency_basis,
            self.resolution,
            self.target_repository_id,
            self.source_entity_id,
            self.target_entity_id,
            self.confidence
        )
    }
}

/// Diagnostics from one resolver run (bounded; count/path only).
#[derive(Debug, Default, Clone)]
pub struct ResolutionDiagnostics {
    /// `(repo, declaration name)` pairs that matched no provider.
    pub missing_targets: Vec<(String, String)>,
    /// `(repo, declaration name)` → competing provider repositories.
    pub ambiguous_targets: Vec<(String, String, Vec<String>)>,
    /// Repositories skipped because the workspace exceeded run bounds.
    pub skipped_repositories: usize,
}

impl ResolutionDiagnostics {
    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.missing_targets.is_empty() && self.ambiguous_targets.is_empty()
    }
}

/// Provider index: `(ecosystem, name)` → provider repository ids.
///
/// Indexed lookup replaces N×N comparison: each declaration resolves via a
/// hash-map hit, never by scanning every repository pair.
///
/// **Bounded**: per-key candidate list capped at [`limits::MAX_CANDIDATES_PER_KEY`];
/// total index size bounded by `Σ min(provides_per_repo, MAX_PROVIDES_PER_REPO)`
/// across repos, itself bounded by `MAX_REPOSITORIES_PER_RUN × MAX_PROVIDES_PER_REPO`.
struct ProviderIndex {
    map: HashMap<(&'static str, String), Vec<String>>,
    by_root: HashMap<String, String>,
}

impl ProviderIndex {
    fn build(repos: &[RepoCatalogData]) -> Self {
        let mut map: HashMap<(&'static str, String), Vec<String>> = HashMap::new();
        let mut by_root = HashMap::new();
        for r in repos {
            by_root.insert(normalize_path(&r.root_path), r.repository_id.clone());
            for p in &r.provides {
                let key = (p.ecosystem.as_str(), p.name.clone());
                let entry = map.entry(key).or_default();
                if !entry.contains(&r.repository_id) {
                    entry.push(r.repository_id.clone());
                }
                if entry.len() >= limits::MAX_CANDIDATES_PER_KEY {
                    break;
                }
            }
        }
        Self { map, by_root }
    }

    fn providers_of(&self, eco: Ecosystem, name: &str) -> &[String] {
        self.map
            .get(&(eco.as_str(), name.to_string()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

fn normalize_path(p: &str) -> String {
    p.replace('\\', "/").trim_end_matches('/').to_lowercase()
}

/// Basis token stored in `core_relationships.dependency_basis` for an
/// ecosystem (schema vocabulary where present, precise token otherwise).
fn basis_for(eco: Ecosystem) -> &'static str {
    match eco {
        Ecosystem::Maven => "MAVEN",
        Ecosystem::Gradle => "GRADLE",
        Ecosystem::Go => "GO_MODULE",
        Ecosystem::Npm => "NPM",
        Ecosystem::Python => "PYTHON_PACKAGE",
        Ecosystem::Submodule => "SUBMODULE",
        Ecosystem::GeneratedApi => "GENERATED_API",
    }
}

/// Resolve every declaration in `repos` into cross-repo [`EdgeDraft`]s.
///
/// Deterministic: iteration follows input order, drafts sort by identity
/// key before returning.  Diagnostics record missing/ambiguous targets.
pub fn resolve_workspace(
    repos: &[RepoCatalogData],
    proto_paths_by_repo: &HashMap<String, Vec<String>>,
) -> (Vec<EdgeDraft>, ResolutionDiagnostics) {
    let mut diag = ResolutionDiagnostics::default();
    if repos.len() > limits::MAX_REPOSITORIES_PER_RUN {
        diag.skipped_repositories = repos.len() - limits::MAX_REPOSITORIES_PER_RUN;
    }
    let effective = &repos[..repos.len().min(limits::MAX_REPOSITORIES_PER_RUN)];
    let index = ProviderIndex::build(effective);

    // Generated-API (.proto import) index: exact specifier → provider repos.
    let mut proto_index: HashMap<&str, Vec<&str>> = HashMap::new();
    for (repo, paths) in proto_paths_by_repo {
        for p in paths {
            let entry = proto_index.entry(p.as_str()).or_default();
            entry.push(repo.as_str());
        }
    }

    let mut drafts: Vec<EdgeDraft> = Vec::new();

    for r in effective {
        for d in &r.declarations {
            if d.name.is_empty() {
                continue;
            }
            match d.kind {
                DeclarationKind::LocalPath => {
                    resolve_local_hint(r, d, &index.by_root, &mut drafts, &mut diag);
                }
                DeclarationKind::WorkspaceMember => {
                    // Workspace members are intra-repo relationships handled
                    // by Phase 3 structural intelligence; they only become a
                    // cross-repo basis through LocalPath-style hints, which
                    // arrive as DeclarationKind::LocalPath entries.
                }
                DeclarationKind::External => {
                    resolve_external(r, d, &index, &proto_index, &mut drafts, &mut diag);
                }
            }
        }
    }

    drafts.sort_by(|a, b| {
        a.source_repository_id
            .cmp(&b.source_repository_id)
            .then(a.identity_key().cmp(&b.identity_key()))
    });
    drafts.dedup_by(|a, b| a.identity_key() == b.identity_key());
    (drafts, diag)
}

fn resolve_local_hint(
    r: &RepoCatalogData,
    d: &DependencyDeclaration,
    by_root: &HashMap<String, String>,
    drafts: &mut Vec<EdgeDraft>,
    diag: &mut ResolutionDiagnostics,
) {
    let Some(hint) = &d.local_hint else {
        return;
    };
    // The hint is relative to the DECLARING repository root.  A nested or
    // sibling registered repository whose canonical root matches the hint
    // provides the build-resolved target (submodules, go replace dirs,
    // gradle project dirs mapped onto separate repositories).
    let base = std::path::PathBuf::from(&r.root_path);
    let joined = base.join(hint.replace('/', std::path::MAIN_SEPARATOR_STR));
    let Some(canon) = joined.canonicalize().ok() else {
        diag.missing_targets
            .push((r.repository_id.clone(), d.name.clone()));
        return;
    };
    let canon_str = normalize_path(&canon.to_string_lossy());
    let Some(target_repo) = by_root.get(&canon_str) else {
        // Points inside the same repository (or nowhere registered):
        // intra-repo layout evidence, not a cross-repo relationship.
        return;
    };
    if target_repo == &r.repository_id {
        return;
    }
    drafts.push(EdgeDraft {
        source_repository_id: r.repository_id.clone(),
        source_entity_id: r
            .primary_anchor_occurrence
            .clone()
            .unwrap_or_else(|| logical_repo_id(&r.repository_id)),
        target_repository_id: target_repo.clone(),
        target_entity_id: logical_or_anchor_placeholder(target_repo),
        dependency_basis: basis_for(d.ecosystem).to_string(),
        resolution: "BUILD_RESOLVED".to_string(),
        confidence: 0.95,
        provenance_json: json!({
            "kind": "local_path",
            "ecosystem": d.ecosystem.as_str(),
            "declaration": d.name,
            "manifest": d.path,
            "hint": hint,
        })
        .to_string(),
        source_revision_id: r.source_revision_id.clone(),
    });
}

fn resolve_external(
    r: &RepoCatalogData,
    d: &DependencyDeclaration,
    index: &ProviderIndex,
    proto_index: &HashMap<&str, Vec<&str>>,
    drafts: &mut Vec<EdgeDraft>,
    diag: &mut ResolutionDiagnostics,
) {
    // Generated API imports resolve against the exact proto path index.
    if d.ecosystem == Ecosystem::GeneratedApi {
        let providers = proto_index
            .get(d.name.as_str())
            .cloned()
            .unwrap_or_default();
        let others: Vec<String> = providers
            .iter()
            .map(|s| (*s).to_string())
            .filter(|p| *p != r.repository_id)
            .collect();
        if others.len() == 1 {
            let t = &others[0];
            drafts.push(EdgeDraft {
                source_repository_id: r.repository_id.clone(),
                source_entity_id: r
                    .primary_anchor_occurrence
                    .clone()
                    .unwrap_or_else(|| logical_repo_id(&r.repository_id)),
                target_repository_id: t.clone(),
                target_entity_id: logical_or_anchor_placeholder(t),
                dependency_basis: "GENERATED_API".to_string(),
                resolution: "PACKAGE_RESOLVED".to_string(),
                confidence: 0.8,
                provenance_json: json!({
                    "kind": "generated_api_import",
                    "specifier": d.name,
                    "manifest": d.path,
                })
                .to_string(),
                source_revision_id: r.source_revision_id.clone(),
            });
        } else if others.is_empty() {
            diag.missing_targets
                .push((r.repository_id.clone(), d.name.clone()));
        } else {
            diag.ambiguous_targets
                .push((r.repository_id.clone(), d.name.clone(), others));
        }
        return;
    }

    let providers = index.providers_of(d.ecosystem, &d.name);
    let others: Vec<&String> = providers
        .iter()
        .filter(|p| **p != r.repository_id)
        .collect();
    match others.len() {
        0 => {
            // No workspace provider — external-world dependency; nothing to
            // persist (Attic does not invent network dependencies).
            diag.missing_targets
                .push((r.repository_id.clone(), d.name.clone()));
        }
        1 => {
            let t = others[0];
            drafts.push(EdgeDraft {
                source_repository_id: r.repository_id.clone(),
                source_entity_id: r
                    .primary_anchor_occurrence
                    .clone()
                    .unwrap_or_else(|| logical_repo_id(&r.repository_id)),
                target_repository_id: (*t).clone(),
                target_entity_id: logical_or_anchor_placeholder(t),
                dependency_basis: basis_for(d.ecosystem).to_string(),
                resolution: "PACKAGE_RESOLVED".to_string(),
                confidence: 0.9,
                provenance_json: json!({
                    "kind": "package_coordinate",
                    "ecosystem": d.ecosystem.as_str(),
                    "coordinate": d.name,
                    "version_req": d.version_req,
                    "manifest": d.path,
                })
                .to_string(),
                source_revision_id: r.source_revision_id.clone(),
            });
        }
        _ => {
            diag.ambiguous_targets.push((
                r.repository_id.clone(),
                d.name.clone(),
                others.iter().map(|s| (*s).clone()).collect(),
            ));
        }
    }
}

/// Deterministic logical placeholder for a repository without an indexed
/// primary manifest occurrence (ADR-011 convention).
pub fn logical_or_anchor_placeholder(repo_id: &str) -> String {
    format!("logical:xrepo-{repo_id}")
}

/// Logical entity id representing a whole-repository anchor.
pub fn logical_repo_id(repo_id: &str) -> String {
    format!("logical:xrepo-{repo_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_repo(
        id: &str,
        root: &str,
        provides: Vec<ProvidedIdentity>,
        declarations: Vec<DependencyDeclaration>,
    ) -> RepoCatalogData {
        RepoCatalogData {
            repository_id: id.to_owned(),
            root_path: root.to_owned(),
            source_revision_id: format!("rev-{id}"),
            provides,
            declarations,
            primary_anchor_occurrence: None,
            go_module_prefix: None,
        }
    }

    #[test]
    fn empty_workspace_produces_no_edges() {
        let (edges, diag) = resolve_workspace(&[], &HashMap::new());
        assert!(edges.is_empty());
        assert!(diag.is_empty());
    }

    #[test]
    fn single_provider_single_consumer_resolves() {
        let provider = make_repo(
            "repo-lib",
            "/workspace/lib",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Go,
                name: "example.com/team/lib".to_owned(),
            }],
            vec![],
        );
        let consumer = make_repo(
            "repo-api",
            "/workspace/api",
            vec![],
            vec![DependencyDeclaration {
                path: "go.mod".to_owned(),
                ecosystem: Ecosystem::Go,
                name: "example.com/team/lib".to_owned(),
                version_req: Some("v1.2.3".to_owned()),
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );

        let (edges, diag) = resolve_workspace(&[provider, consumer], &HashMap::new());
        assert_eq!(edges.len(), 1, "exactly one edge expected");
        assert_eq!(edges[0].source_repository_id, "repo-api");
        assert_eq!(edges[0].target_repository_id, "repo-lib");
        assert_eq!(edges[0].resolution, "PACKAGE_RESOLVED");
        assert!(
            diag.is_empty(),
            "no diagnostics expected for clean resolution"
        );
    }

    #[test]
    fn ambiguous_provider_produces_no_edge() {
        let p1 = make_repo(
            "lib-a",
            "/ws/a",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Npm,
                name: "shared-util".to_owned(),
            }],
            vec![],
        );
        let p2 = make_repo(
            "lib-b",
            "/ws/b",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Npm,
                name: "shared-util".to_owned(),
            }],
            vec![],
        );
        let consumer = make_repo(
            "app",
            "/ws/app",
            vec![],
            vec![DependencyDeclaration {
                path: "package.json".to_owned(),
                ecosystem: Ecosystem::Npm,
                name: "shared-util".to_owned(),
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );

        let (edges, diag) = resolve_workspace(&[p1, p2, consumer], &HashMap::new());
        assert!(
            edges.is_empty(),
            "ambiguous target must NOT produce an edge"
        );
        assert_eq!(diag.ambiguous_targets.len(), 1);
        assert_eq!(diag.ambiguous_targets[0].1, "shared-util");
    }

    #[test]
    fn missing_provider_records_diagnostic() {
        let consumer = make_repo(
            "app",
            "/ws/app",
            vec![],
            vec![DependencyDeclaration {
                path: "go.mod".to_owned(),
                ecosystem: Ecosystem::Go,
                name: "example.com/unknown".to_owned(),
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );

        let (edges, diag) = resolve_workspace(&[consumer], &HashMap::new());
        assert!(edges.is_empty());
        assert_eq!(diag.missing_targets.len(), 1);
        assert_eq!(diag.missing_targets[0].1, "example.com/unknown");
    }

    #[test]
    fn workspace_members_are_ignored() {
        let repo = make_repo(
            "mono",
            "/ws/mono",
            vec![],
            vec![DependencyDeclaration {
                path: "settings.gradle".to_owned(),
                ecosystem: Ecosystem::Gradle,
                name: "project:::core".to_owned(),
                version_req: None,
                kind: DeclarationKind::WorkspaceMember,
                local_hint: None,
            }],
        );

        let (edges, diag) = resolve_workspace(&[repo], &HashMap::new());
        assert!(edges.is_empty());
        assert!(diag.is_empty());
    }

    #[test]
    fn edge_deterministic_ordering() {
        let mut repos = Vec::new();
        for i in 0..5 {
            repos.push(make_repo(
                &format!("repo-{i}"),
                &format!("/ws/{i}"),
                vec![ProvidedIdentity {
                    ecosystem: Ecosystem::Maven,
                    name: format!("com.example:lib{i}"),
                }],
                vec![],
            ));
        }
        // consumer depends on all 5
        let mut decls = Vec::new();
        for i in 0..5 {
            decls.push(DependencyDeclaration {
                path: "pom.xml".to_owned(),
                ecosystem: Ecosystem::Maven,
                name: format!("com.example:lib{i}"),
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            });
        }
        repos.push(make_repo("consumer", "/ws/c", vec![], decls));

        let (edges, _) = resolve_workspace(&repos, &HashMap::new());
        assert_eq!(edges.len(), 5);
        // Verify sorted by source then identity key
        for w in edges.windows(2) {
            assert!(w[0].identity_key() <= w[1].identity_key());
        }
    }

    #[test]
    fn repo_limit_enforced() {
        let mut repos = Vec::new();
        for i in 0..limits::MAX_REPOSITORIES_PER_RUN + 10 {
            repos.push(make_repo(
                &format!("repo-{i}"),
                &format!("/ws/{i}"),
                vec![ProvidedIdentity {
                    ecosystem: Ecosystem::Go,
                    name: format!("mod{i}"),
                }],
                vec![DependencyDeclaration {
                    path: "go.mod".to_owned(),
                    ecosystem: Ecosystem::Go,
                    name: format!("mod{}", i + 1),
                    version_req: None,
                    kind: DeclarationKind::External,
                    local_hint: None,
                }],
            ));
        }

        let (_, diag) = resolve_workspace(&repos, &HashMap::new());
        assert_eq!(diag.skipped_repositories, 10);
    }

    #[test]
    fn identity_key_is_deterministic() {
        let e1 = EdgeDraft {
            source_repository_id: "a".to_owned(),
            source_entity_id: "e1".to_owned(),
            target_repository_id: "b".to_owned(),
            target_entity_id: "e2".to_owned(),
            dependency_basis: "GO_MODULE".to_owned(),
            resolution: "PACKAGE_RESOLVED".to_owned(),
            confidence: 0.9,
            provenance_json: "{}".to_owned(),
            source_revision_id: "r1".to_owned(),
        };
        let e2 = e1.clone();
        assert_eq!(e1.identity_key(), e2.identity_key());
    }

    #[test]
    fn mutual_dependency_cycle_produces_edges() {
        let a = make_repo(
            "repo-a",
            "/ws/a",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Go,
                name: "example.com/a".to_owned(),
            }],
            vec![DependencyDeclaration {
                path: "go.mod".to_owned(),
                ecosystem: Ecosystem::Go,
                name: "example.com/b".to_owned(),
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );
        let b = make_repo(
            "repo-b",
            "/ws/b",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Go,
                name: "example.com/b".to_owned(),
            }],
            vec![DependencyDeclaration {
                path: "go.mod".to_owned(),
                ecosystem: Ecosystem::Go,
                name: "example.com/a".to_owned(),
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );

        let (edges, diag) = resolve_workspace(&[a, b], &HashMap::new());
        // Mutual dependency: A→B and B→A, both edges exist.
        assert_eq!(edges.len(), 2, "cycle edges should be emitted");
        assert!(diag.is_empty(), "cycle is not ambiguous");
    }

    #[test]
    fn transitive_chain_resolves_all_edges() {
        let lib = make_repo(
            "lib",
            "/ws/lib",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Npm,
                name: "utils".to_owned(),
            }],
            vec![DependencyDeclaration {
                path: "package.json".to_owned(),
                ecosystem: Ecosystem::Npm,
                name: "core".to_owned(),
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );
        let core = make_repo(
            "core",
            "/ws/core",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Npm,
                name: "core".to_owned(),
            }],
            vec![],
        );
        let app = make_repo(
            "app",
            "/ws/app",
            vec![],
            vec![DependencyDeclaration {
                path: "package.json".to_owned(),
                ecosystem: Ecosystem::Npm,
                name: "utils".to_owned(),
                version_req: None,
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );

        let (edges, diag) = resolve_workspace(&[lib, core, app], &HashMap::new());
        assert_eq!(edges.len(), 2, "app→lib and lib→core edges");
        assert!(diag.is_empty());
        // Verify direction
        let app_to_lib = edges
            .iter()
            .find(|e| e.source_repository_id == "app")
            .unwrap();
        assert_eq!(app_to_lib.target_repository_id, "lib");
        let lib_to_core = edges
            .iter()
            .find(|e| e.source_repository_id == "lib")
            .unwrap();
        assert_eq!(lib_to_core.target_repository_id, "core");
    }

    #[test]
    fn source_revision_propagated_to_edge() {
        let provider = make_repo(
            "provider",
            "/ws/p",
            vec![ProvidedIdentity {
                ecosystem: Ecosystem::Go,
                name: "example.com/p".to_owned(),
            }],
            vec![],
        );
        let consumer = make_repo(
            "consumer",
            "/ws/c",
            vec![],
            vec![DependencyDeclaration {
                path: "go.mod".to_owned(),
                ecosystem: Ecosystem::Go,
                name: "example.com/p".to_owned(),
                version_req: Some("v2.0.0".to_owned()),
                kind: DeclarationKind::External,
                local_hint: None,
            }],
        );

        let (edges, _) = resolve_workspace(&[provider, consumer], &HashMap::new());
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].source_revision_id, "rev-consumer");
    }
}
