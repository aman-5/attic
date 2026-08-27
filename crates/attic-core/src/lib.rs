#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! `attic-core` — domain types and pure-logic contracts.
//!
//! MUST NOT depend on tokio, rusqlite, rmcp, tree-sitter, notify, or any I/O crate.

pub mod constants;
pub mod domain;
/// Configuration model for the Attic MCP server, reading defaults from
/// `crates/attic-core/src/constants.rs::resources` and allowing override via
/// environment variables.
pub mod config;
pub mod error;

pub use constants::{
    ANALYZER_REGISTRY_VERSION, CURRENT_SCHEMA_VERSION, SECRET_PATTERN_VERSION, subsystem_keys,
    resources,
};
pub use domain::{
    enums::{
        ArtifactType, Authority, CompatibilityClass, DependencyBasis, DiscoveryClass,
        ExistenceState, FileType, FreshnessState, InvalidationArtifactType, InvalidationCause,
        InvalidationReason, LexicalState, RelType, Resolution, SecretScanState, SecurityState,
        SemanticState, SourceType, SymbolKind, TaskState, TaskType, VerificationState, ResourcePressure,
    },
    ids::{
        EvidenceId, FileIdentityId, FileOccurrenceId, IndexGenerationId, OpsAuditId, RepositoryId,
        RetrievalUnitId, SchemaMigrationId, SourceRevisionId, StructuralNodeId, SymbolIdentityId,
        SymbolOccurrenceId,
    },
    subsystem_versions::SubsystemVersions,
    value_types::{
        ResourceBudgets, SourceSpan,
    },
};
pub use error::CoreError;
pub use config::ProductionConfig;
