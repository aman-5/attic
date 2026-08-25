//! Domain-level errors for `attic-core`.

use thiserror::Error;

/// Errors that can arise in pure domain logic (no I/O).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoreError {
    /// A required field was absent when constructing a domain object.
    #[error("missing required field: {field}")]
    MissingField {
        /// Name of the missing field.
        field: &'static str,
    },

    /// An unknown string variant was encountered when converting from DB storage.
    #[error("unknown variant '{value}' for type '{type_name}'")]
    UnknownVariant {
        /// The Rust type name.
        type_name: &'static str,
        /// The raw string value read from storage.
        value: String,
    },

    /// The `subsystem_versions_json` field could not be serialised or deserialised.
    #[error("subsystem versions JSON error: {message}")]
    SubsystemVersionsJson {
        /// Human-readable description of the error.
        message: String,
    },
}
