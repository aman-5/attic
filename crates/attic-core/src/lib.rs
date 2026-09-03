#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! `attic-core` — domain types and pure-logic contracts.
//!
//! MUST NOT depend on tokio, rusqlite, rmcp, tree-sitter, notify, or any I/O crate.

/// Cooperative cancellation primitives shared by long-running Attic operations.
pub mod cancellation;

/// Configuration model for the Attic MCP server, reading defaults from
/// `crates/attic-core/src/constants.rs::resources` and allowing override via
/// environment variables.
pub mod config;

/// Shared Attic constants and resource configuration values.
pub mod constants;

/// Core Attic domain types.
pub mod domain;

/// Core Attic error types.
pub mod error;

/// Phase 7 runtime path policy for data roots, backups, and temporary files.
pub mod paths;

pub use config::ProductionConfig;
pub use constants::{
    ANALYZER_REGISTRY_VERSION, CURRENT_SCHEMA_VERSION, SECRET_PATTERN_VERSION, resources,
    subsystem_keys,
};
pub use domain::{
    enums::{
        ArtifactType, Authority, CompatibilityClass, DependencyBasis, DiscoveryClass,
        ExistenceState, FileType, FreshnessState, InvalidationArtifactType, InvalidationCause,
        LexicalState, RelType, Resolution, ResourcePressure, SecretScanState, SecurityState,
        SemanticState, SourceType, SymbolKind, TaskState, TaskType, VerificationState,
    },
    ids::{
        EvidenceId, FileIdentityId, FileOccurrenceId, IndexGenerationId, OpsAuditId, RepositoryId,
        RetrievalUnitId, SchemaMigrationId, SourceRevisionId, StructuralNodeId, SymbolIdentityId,
        SymbolOccurrenceId,
    },
    subsystem_versions::SubsystemVersions,
    value_types::{ResourceBudgets, SourceSpan},
};
pub use error::CoreError;
pub use paths::{AtticPaths, PathResolutionError, resolve_data_root_from};

pub use cancellation::CancellationToken;
