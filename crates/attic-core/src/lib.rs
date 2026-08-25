#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! `attic-core` — domain types and pure-logic contracts.
//!
//! MUST NOT depend on tokio, rusqlite, rmcp, tree-sitter, notify, or any I/O crate.

pub mod constants;
pub mod domain;
pub mod error;

pub use constants::{CURRENT_SCHEMA_VERSION, SECRET_PATTERN_VERSION, subsystem_keys};
pub use domain::{
    enums::{
        ArtifactType, Authority, CompatibilityClass, DependencyBasis, DiscoveryClass,
        ExistenceState, FileType, FreshnessState, InvalidationReason, LexicalState, RelType,
        Resolution, SecretScanState, SecurityState, SemanticState, SourceType, SymbolKind,
        TaskState, TaskType, VerificationState,
    },
    ids::{
        EvidenceId, FileIdentityId, FileOccurrenceId, IndexGenerationId, OpsAuditId, RepositoryId,
        RetrievalUnitId, SchemaMigrationId, SourceRevisionId, StructuralNodeId, SymbolIdentityId,
        SymbolOccurrenceId,
    },
    subsystem_versions::SubsystemVersions,
    value_types::SourceSpan,
};
pub use error::CoreError;
