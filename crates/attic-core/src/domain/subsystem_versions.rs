//! Typed wrapper for the `subsystem_versions_json` column in `core_index_generations`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// A map of subsystem-name → version-string stored as a JSON blob.
///
/// The well-known key names are in [`crate::constants::subsystem_keys`].
/// The map is always explicitly provided — there is no `DEFAULT` in the schema
/// (ADR-004).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SubsystemVersions(HashMap<String, String>);

impl SubsystemVersions {
    /// Create an empty map.
    pub fn new() -> Self {
        Self(HashMap::new())
    }

    /// Insert or update a subsystem version entry.
    pub fn set(&mut self, key: impl Into<String>, version: impl Into<String>) {
        self.0.insert(key.into(), version.into());
    }

    /// Retrieve the version string for a subsystem key, if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// Number of entries in the map.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Return `true` when the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Serialise to the JSON string stored in the `subsystem_versions_json` column.
    pub fn to_json(&self) -> Result<String, CoreError> {
        serde_json::to_string(&self.0).map_err(|e| CoreError::SubsystemVersionsJson {
            message: e.to_string(),
        })
    }

    /// Deserialise from the JSON string read from the `subsystem_versions_json` column.
    pub fn from_json(s: &str) -> Result<Self, CoreError> {
        let map: HashMap<String, String> =
            serde_json::from_str(s).map_err(|e| CoreError::SubsystemVersionsJson {
                message: e.to_string(),
            })?;
        Ok(Self(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::subsystem_keys;

    #[test]
    fn round_trip_json() {
        let mut sv = SubsystemVersions::new();
        sv.set(subsystem_keys::SCHEMA, "1.0.0");
        sv.set(subsystem_keys::INDEXER, "2.3.1");
        sv.set(subsystem_keys::SECRET_DETECTOR, "1");

        let json = sv.to_json().expect("serialise");
        let decoded = SubsystemVersions::from_json(&json).expect("deserialise");

        assert_eq!(decoded.get(subsystem_keys::SCHEMA), Some("1.0.0"));
        assert_eq!(decoded.get(subsystem_keys::INDEXER), Some("2.3.1"));
        assert_eq!(decoded.get(subsystem_keys::SECRET_DETECTOR), Some("1"));
        assert_eq!(decoded.len(), 3);
    }

    #[test]
    fn from_json_bad_input_is_error() {
        let result = SubsystemVersions::from_json("not valid json {{");
        assert!(result.is_err());
        match result.unwrap_err() {
            crate::error::CoreError::SubsystemVersionsJson { .. } => {}
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn empty_map_serialises_to_empty_object() {
        let sv = SubsystemVersions::new();
        assert!(sv.is_empty());
        let json = sv.to_json().unwrap();
        assert_eq!(json, "{}");
    }
}
