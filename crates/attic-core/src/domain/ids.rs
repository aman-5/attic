//! Strongly-typed UUID wrappers for every first-class entity in the domain model.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CoreError;

/// Generates a newtype UUID wrapper with `new_v4`, `as_uuid`, `to_string_repr`,
/// `Display`, `FromStr`, `Serialize`, and `Deserialize`.
macro_rules! define_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generate a fresh random v4 ID.
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }

            /// Return the underlying [`Uuid`].
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }

            /// Return the hyphenated string representation used for DB storage.
            pub fn to_string_repr(&self) -> String {
                self.0.hyphenated().to_string()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0.hyphenated())
            }
        }

        impl FromStr for $name {
            type Err = CoreError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self).map_err(|_| CoreError::UnknownVariant {
                    type_name: stringify!($name),
                    value: s.to_owned(),
                })
            }
        }
    };
}

define_id!(
    /// Identifies a source-code repository root.
    RepositoryId
);

define_id!(
    /// Identifies a point-in-time snapshot of a repository (one commit / revision).
    SourceRevisionId
);

define_id!(
    /// Identifies a completed indexing run over a `SourceRevision`.
    IndexGenerationId
);

define_id!(
    /// Identifies the canonical identity of a file (content-hash + path).
    FileIdentityId
);

define_id!(
    /// Identifies a file as it appeared in a specific `IndexGeneration`.
    FileOccurrenceId
);

define_id!(
    /// Identifies the canonical identity of a symbol (qualified name + kind).
    SymbolIdentityId
);

define_id!(
    /// Identifies a symbol occurrence within a specific file occurrence.
    SymbolOccurrenceId
);

define_id!(
    /// Identifies a node in the structural (AST/outline) hierarchy.
    StructuralNodeId
);

define_id!(
    /// Identifies a retrieval unit — the atom of search results.
    RetrievalUnitId
);

define_id!(
    /// Identifies a piece of evidence attached to a retrieval unit.
    EvidenceId
);

define_id!(
    /// Identifies a schema migration record.
    SchemaMigrationId
);

define_id!(
    /// Identifies an operations audit log entry.
    OpsAuditId
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_id_round_trips_via_string() {
        let id = RepositoryId::new_v4();
        let s = id.to_string_repr();
        let parsed: RepositoryId = s.parse().expect("parse should succeed");
        assert_eq!(id, parsed);
    }

    #[test]
    fn bad_uuid_string_returns_error() {
        let result = RepositoryId::from_str("not-a-uuid");
        assert!(result.is_err());
    }

    #[test]
    fn display_matches_to_string_repr() {
        let id = IndexGenerationId::new_v4();
        assert_eq!(id.to_string(), id.to_string_repr());
    }
}
