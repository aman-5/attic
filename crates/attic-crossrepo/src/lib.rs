//! `attic-crossrepo` — Phase 6: bounded cross-repository intelligence.
//!
//! Pipeline:
//!
//! ```text
//! repository-local intelligence
//!          ↓
//! package/build dependency basis      [manifest]
//!          ↓
//! workspace catalog                   [catalog]
//!          ↓
//! cross-repository resolution         [resolver]
//!          ↓
//! confidence/provenance               [resolver → core_relationships]
//!          ↓
//! bounded traversal                   [traversal]
//!          ↓
//! impact analysis                     [impact]
//!          ↓
//! Phase 4 Evidence Manager            (edges live in core_relationships)
//! ```
//!
//! Critical principle: **a name match across repositories is not a resolved
//! dependency.** Only explicit build/package/module evidence resolves an
//! edge; ambiguity stays ambiguity and is never persisted as truth.
//!
//! Repository isolation is preserved:
//!
//! ```text
//! Repository → Repository Index → Workspace Catalog → Cross-Repo Edges
//! ```
//!
//! Recomputation is per-repository and incremental: a change in repo-17 only
//! invalidates/recomputes edges touching repo-17 ([`maintenance`]).
//!
//! Security: build/package metadata is parsed as UNTRUSTED data. No build
//! scripts, package managers, or network access are ever executed; parser
//! outputs never carry raw manifest bytes so secret material cannot leak
//! into derived state.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod catalog;
pub mod error;
pub mod impact;
pub mod maintenance;
pub mod manifest;
pub mod resolver;
pub mod traversal;

pub use error::CrossRepoError;

/// Build/package ecosystem of a dependency declaration or catalog identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum Ecosystem {
    /// Maven coordinates (`groupId:artifactId`, pom.xml).
    Maven,
    /// Gradle project/module dependencies.
    Gradle,
    /// Go module require/replace (`go.mod`).
    Go,
    /// npm package name (`package.json`, workspaces included).
    Npm,
    /// Python project/package name (`pyproject.toml`/requirements).
    Python,
    /// Git submodule (`.gitmodules`).
    Submodule,
    /// Generated API/schema relationship (protobuf imports).
    GeneratedApi,
}

impl Ecosystem {
    /// Canonical TEXT stored in SQLite.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Maven => "MAVEN",
            Self::Gradle => "GRADLE",
            Self::Go => "GO",
            Self::Npm => "NPM",
            Self::Python => "PYTHON",
            Self::Submodule => "SUBMODULE",
            Self::GeneratedApi => "GENERATED_API",
        }
    }

    /// Parse from SQLite TEXT.
    pub fn from_db_str(s: &str) -> Result<Self, CrossRepoError> {
        match s {
            "MAVEN" => Ok(Self::Maven),
            "GRADLE" => Ok(Self::Gradle),
            "GO" => Ok(Self::Go),
            "NPM" => Ok(Self::Npm),
            "PYTHON" => Ok(Self::Python),
            "SUBMODULE" => Ok(Self::Submodule),
            "GENERATED_API" => Ok(Self::GeneratedApi),
            other => Err(CrossRepoError::UnknownVariant {
                type_name: "Ecosystem",
                value: other.to_owned(),
            }),
        }
    }
}

impl std::fmt::Display for Ecosystem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a declaration relates to its target within the declaring repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeclarationKind {
    /// External coordinate with no local path hint.
    External,
    /// Declared via an explicit local relative path (go replace, file:,
    /// project(:...), path dependency).
    LocalPath,
    /// Member of a multi-package/workspace build.
    WorkspaceMember,
}

impl DeclarationKind {
    /// Canonical TEXT stored in SQLite.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::External => "external",
            Self::LocalPath => "local_path",
            Self::WorkspaceMember => "workspace_member",
        }
    }

    /// Parse from SQLite TEXT.
    pub fn from_db_str(s: &str) -> Result<Self, CrossRepoError> {
        match s {
            "external" => Ok(Self::External),
            "local_path" => Ok(Self::LocalPath),
            "workspace_member" => Ok(Self::WorkspaceMember),
            other => Err(CrossRepoError::UnknownVariant {
                type_name: "DeclarationKind",
                value: other.to_owned(),
            }),
        }
    }
}

/// One parsed dependency target from a manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct DependencyDeclaration {
    /// Repo-relative manifest path the declaration came from.
    pub path: String,
    /// Ecosystem of the declaration.
    pub ecosystem: Ecosystem,
    /// Normalized target identity (group:artifact, module path, npm name,
    /// python distribution name, submodule name).
    pub name: String,
    /// Version requirement string when present.
    pub version_req: Option<String>,
    /// Local/workspace/path classification.
    pub kind: DeclarationKind,
    /// Repo-relative local hint (never escapes the repository root).
    pub local_hint: Option<String>,
}

/// An identity a repository PROVIDES to the workspace catalog.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProvidedIdentity {
    /// Ecosystem of the identity.
    pub ecosystem: Ecosystem,
    /// Normalized provided name (module path, group:artifact, npm name,
    /// python distribution name).
    pub name: String,
}

// ---------------------------------------------------------------------------
// Bounds, deadlines, cancellation
// ---------------------------------------------------------------------------

/// Default caps applied unless the caller overrides them.  Sized for the
/// canonical 25–30 repository workspace; every value is a hard bound.
pub mod limits {
    /// Maximum manifest file size parsed (bytes); larger manifests are
    /// refused with an observable diagnostic rather than truncated silently.
    pub const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
    /// Maximum declarations retained per repository.
    pub const MAX_DECLARATIONS_PER_REPO: usize = 2_000;
    /// Maximum provided identities retained per repository.
    pub const MAX_PROVIDES_PER_REPO: usize = 512;
    /// Maximum repositories considered in one resolver run.
    pub const MAX_REPOSITORIES_PER_RUN: usize = 64;
    /// Maximum candidates inspected per single lookup key.
    pub const MAX_CANDIDATES_PER_KEY: usize = 32;
}

/// Wall-clock deadline used by long-running Phase 6 operations.
#[derive(Debug, Clone)]
pub struct Deadline(pub std::time::Instant);

impl Deadline {
    /// Deadline `dur` away from now.
    pub fn after(dur: std::time::Duration) -> Self {
        Self(std::time::Instant::now() + dur)
    }

    /// Whether the deadline has passed.
    pub fn expired(&self) -> bool {
        std::time::Instant::now() >= self.0
    }
}

/// Cooperative cancellation token for bounded operations.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(pub std::sync::Arc<std::sync::atomic::AtomicBool>);

impl CancelToken {
    /// A token that is never cancelled.
    pub fn never() -> Self {
        Self::default()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        use std::sync::atomic::Ordering;
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.0.load(Ordering::SeqCst)
    }
}
