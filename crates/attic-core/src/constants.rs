//! Global compile-time constants for schema versioning and secret scanning.

/// Current schema version string, embedded in every index generation.
pub const CURRENT_SCHEMA_VERSION: &str = "1.0.0";

/// Analyzer registry implementation version (compatibility contract:
/// recorded per index generation under `analyzer_registry`; bumped when the
/// bundled analyzer set changes so operators can detect stale generations).
/// 0.1.x = Phase 1C generic-only; 0.2.0 = Phase 3 structural languages.
pub const ANALYZER_REGISTRY_VERSION: &str = "0.2.0";

/// Version of the secret-pattern ruleset used during scanning.
/// Increment this whenever the ruleset changes to trigger re-scanning.
pub const SECRET_PATTERN_VERSION: i64 = 1;

/// Well-known keys used in the `subsystem_versions_json` map stored in
/// `core_index_generations`. Keep in sync with the migration SQL.
pub mod subsystem_keys {
    /// Schema migration version.
    pub const SCHEMA: &str = "schema";
    /// Analyzer registry version.
    pub const ANALYZER_REGISTRY: &str = "analyzer_registry";
    /// Segmentation algorithm version.
    pub const SEGMENTATION: &str = "segmentation";
    /// Indexer pipeline version.
    pub const INDEXER: &str = "indexer";
    /// Ranking algorithm version.
    pub const RANKING: &str = "ranking";
    /// Embedding model identifier.
    pub const EMBEDDING_MODEL: &str = "embedding_model";
    /// Secret-detector ruleset version (mirrors `SECRET_PATTERN_VERSION`).
    pub const SECRET_DETECTOR: &str = "secret_detector";
    /// General configuration version.
    pub const CONFIGURATION: &str = "configuration";
}
