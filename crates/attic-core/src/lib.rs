#![forbid(unsafe_code)]
#![deny(missing_docs)]

//! `attic-core` — domain types and pure-logic contracts.
//!
//! MUST NOT depend on tokio, rusqlite, rmcp, tree-sitter, notify, or any I/O crate.

/// Cooperative cancellation primitives shared by long-running Attic operations.
pub mod cancellation;

/// `attic.toml` configuration model (resource/embedding tunables). Pure
/// parsing only — see `attic_storage::resource_policy` for hardware
/// detection, baseline resolution, and env-var override layering.
pub mod config;

/// Shared Attic constants and resource configuration values.
pub mod constants;

/// Core Attic domain types.
pub mod domain;

/// Core Attic error types.
pub mod error;

/// Phase 7 runtime path policy for data roots, backups, and temporary files.
pub mod paths;

pub use config::{
    ATTIC_TOML_TEMPLATE, AtticConfig, ConfigError, EmbeddingOverride, IndexingOverride,
    ResourceModeSetting, ResourceOverrides,
};
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
