//! Deterministic semantic-unit lineage (Phase 5 §5).
//!
//! Every stored embedding carries the FULL identity chain required by the
//! phase contract:
//!
//! ```text
//! RetrievalUnit id
//! × SourceRevision id
//! × IndexGeneration id
//! × semantic-selection version
//! × provider id + model id
//! × semantic-content hash (BLAKE3 of the exact embedded text)
//! ```
//!
//! Any change to any component invalidates exactly the affected artifacts —
//! never the canonical index, FTS, structural nodes, symbols, or
//! relationships (the disposable-layer invariant).

/// Semantic-content hash of the EXACT text handed to the provider.
pub fn content_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

/// Identity components recorded alongside every embedding row. Provider and
/// model are store-level columns (they key the active model set), so this
/// struct carries the per-unit portion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticUnitIdentity {
    /// `core_retrieval_units.id`.
    pub retrieval_unit_id: String,
    /// `core_source_revisions.id` of the backing file occurrence.
    pub source_revision_id: String,
    /// `core_index_generations.id` that produced the unit.
    pub index_generation_id: String,
    /// Selection policy version that chose this unit (`SEMANTIC_SELECTION_VERSION`).
    pub selection_version: String,
    /// BLAKE3 of the embedded text — detects any content change.
    pub content_hash: String,
}

impl SemanticUnitIdentity {
    pub fn new(
        retrieval_unit_id: impl Into<String>,
        source_revision_id: impl Into<String>,
        index_generation_id: impl Into<String>,
        selection_version: impl Into<String>,
        text: &str,
    ) -> Self {
        Self {
            retrieval_unit_id: retrieval_unit_id.into(),
            source_revision_id: source_revision_id.into(),
            index_generation_id: index_generation_id.into(),
            selection_version: selection_version.into(),
            content_hash: content_hash(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_deterministic_and_sensitive() {
        let a = content_hash("hello world");
        let b = content_hash("hello world");
        let c = content_hash("hello worlds");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn identity_equality_follows_all_components() {
        let base = SemanticUnitIdentity::new("u1", "r1", "g1", "v1", "text");
        let same = SemanticUnitIdentity::new("u1", "r1", "g1", "v1", "text");
        let diff_rev = SemanticUnitIdentity::new("u1", "r2", "g1", "v1", "text");
        let diff_text = SemanticUnitIdentity::new("u1", "r1", "g1", "v1", "other");
        assert_eq!(base, same);
        assert_ne!(base, diff_rev);
        assert_ne!(base, diff_text);
    }
}
