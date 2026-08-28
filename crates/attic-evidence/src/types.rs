//! Canonical `Evidence` object and its supporting enums
//! (`docs/ARCHITECTURE.md`).

use serde::{Deserialize, Serialize};

use attic_core::FreshnessState;
use attic_core::SourceSpan;

use crate::ranking::RankingSignals;
use crate::str_enum;

str_enum! {
    /// What kind of artifact the evidence was derived from.
    EvidenceSourceType {
        /// Implementation source file.
        SourceCode => "SOURCE_CODE",
        /// Test file (behavioral expectation).
        Test => "TEST",
        /// Configuration file (configured behavior).
        Configuration => "CONFIGURATION",
        /// General documentation (non-knowledge).
        Documentation => "DOCUMENTATION",
        /// knowledge/*.md project documentation.
        Knowledge => "KNOWLEDGE",
        /// Derived from relationship/graph traversal.
        Relationship => "RELATIONSHIP",
        /// Auto-generated source file.
        GeneratedSource => "GENERATED_SOURCE",
    }
}

str_enum! {
    /// How authoritative this source type is for the claim being evaluated.
    ///
    /// Authority is NOT a total order across query types; the Query Evidence
    /// Contract decides which authority applies (evidence.md §Authority).
    AuthorityLevel {
        /// Source code; highest for behavioral/correctness claims.
        Implementation => "IMPLEMENTATION",
        /// Test; authoritative for expected behavior.
        TestExpectation => "TEST_EXPECTATION",
        /// Configuration; authoritative for configured behavior.
        Configured => "CONFIGURED",
        /// Knowledge docs; authoritative for documented intent.
        ProjectKnowledge => "PROJECT_KNOWLEDGE",
        /// General docs; medium authority.
        Doc => "DOCUMENTATION",
        /// Graph traversal; confidence-dependent, never IMPLEMENTATION.
        Derived => "DERIVED",
    }
}

str_enum! {
    /// Per-evidence verification state (evidence.md VerificationState).
    VerificationStatus {
        /// Not yet checked against current source.
        Unverified => "UNVERIFIED",
        /// Confirmed against the current source revision.
        Verified => "VERIFIED",
        /// Source revision changed since last verification.
        Stale => "STALE",
        /// Other evidence contradicts this item.
        Contradicted => "CONTRADICTED",
    }
}

str_enum! {
    /// Relationship resolution level carried by graph-derived evidence
    /// (mirrors `core_relationships.resolution`, ADR-011 vocabulary).
    ResolutionLevel {
        /// Pure syntax-level match, unresolved target possible.
        Syntactic => "SYNTACTIC",
        /// Target resolved to a package/module layout.
        PackageResolved => "PACKAGE_RESOLVED",
        /// Target resolved to a concrete symbol occurrence.
        SymbolResolved => "SYMBOL_RESOLVED",
        /// Resolved through a build system.
        BuildResolved => "BUILD_RESOLVED",
        /// Resolved through framework conventions.
        FrameworkResolved => "FRAMEWORK_RESOLVED",
        /// Heuristic inference only.
        Inferred => "INFERRED",
    }
}

/// Provenance for evidence derived from a relationship edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelationshipProvenance {
    /// `core_relationships.id`.
    pub edge_id: String,
    /// Edge type token (IMPORT | CALL | EXTENDS | ... ).
    pub rel_type: String,
    /// Resolution level of the edge.
    pub resolution: ResolutionLevel,
    /// Confidence of the edge [0.0, 1.0]; always < 1.0 for derived evidence.
    pub confidence: f64,
    /// Hop depth from the seed entity during expansion (0 = direct edge).
    pub hop_depth: u32,
}

/// Which retriever produced a candidate and with what raw score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalSource {
    /// FTS | SYMBOL | STRUCTURAL | RELATIONSHIP | KNOWLEDGE | PATH | GRAPH
    pub retriever_type: String,
    /// Raw retrieval score from that retriever (higher = better).
    pub score: f64,
    /// The query or subquery fragment that produced this result.
    pub query_fragment: String,
}

/// The canonical Evidence object (`docs/ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// UUID (lowercase hyphenated string), stable for the query lifetime.
    pub id: String,

    /// Owning repository UUID.
    pub repository_id: String,

    /// What the evidence is about.
    pub source_type: EvidenceSourceType,

    /// `file_occurrence_id` or `knowledge_item_id` (or relationship edge id
    /// for RELATIONSHIP evidence).
    pub source_id: String,

    /// Normalized repo-relative path.
    pub path: String,

    /// Revision that produced the indexed content. `None` is treated as
    /// INVALID at validation time (invariant 1).
    pub source_revision_id: Option<String>,

    /// Index generation that derived this evidence. `None` treated as
    /// INVALID as well.
    pub index_generation_id: Option<String>,

    /// Exact span when known.
    pub source_span: Option<SourceSpan>,

    /// BLAKE3 hex of the evidenced content (as stored in the index).
    pub content_hash: Option<String>,

    /// Freshness of the underlying artifact.
    pub freshness_state: FreshnessState,

    /// Authority level relative to the current query intent.
    pub authority: AuthorityLevel,

    /// Retrieval confidence in [0.0, 1.0].
    pub confidence: f64,

    /// Set exactly when the evidence came via graph/relationship traversal;
    /// always < 1.0 (invariant 7).
    pub relationship_confidence: Option<f64>,

    /// Structured relationship provenance when `source_type == RELATIONSHIP`.
    pub relationship: Option<RelationshipProvenance>,

    /// Which retrievers found this candidate (merged during fusion).
    pub retrieval_sources: Vec<RetrievalSource>,

    /// Observable per-dimension ranking signals.
    pub signals: RankingSignals,

    /// Per-evidence verification state at validation time.
    pub verification_state: VerificationStatus,

    /// True when this exact FACT was confirmed against the LIVE working
    /// tree through bounded direct-source verification during THIS query.
    ///
    /// This flag NEVER alters the indexed artifact's own freshness /
    /// source-revision / generation lineage: a STALE indexed occurrence
    /// remains STALE as an indexed artifact. Sufficiency rules may accept
    /// `live_source_verified` evidence under CURRENT_ONLY contracts because
    /// the verification itself establishes current truth (ADR-012 D3),
    /// without falsifying what the index knows.
    #[serde(default)]
    pub live_source_verified: bool,

    /// Bounded redacted snippet used for context assembly. Secret bytes are
    /// never allowed here (defense-in-depth scan runs before serving).
    #[serde(default)]
    pub snippet: Option<String>,

    /// `core_workspace_snapshots.id` for cross-repository evidence only.
    ///
    /// Set ONLY when this evidence was derived from a cross-repo edge whose
    /// provenance embeds a real WorkspaceSnapshot ID recorded by
    /// `sync_workspace`; it is never fabricated. Repository-local evidence
    /// legitimately has no workspace snapshot and keeps `None`.
    #[serde(default)]
    pub workspace_snapshot_id: Option<String>,
}

impl Evidence {
    /// Build an empty signal set for construction sites.
    pub fn new(id: impl Into<String>, repository_id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            repository_id: repository_id.into(),
            source_type: EvidenceSourceType::SourceCode,
            source_id: String::new(),
            path: String::new(),
            source_revision_id: None,
            index_generation_id: None,
            source_span: None,
            content_hash: None,
            freshness_state: FreshnessState::Current,
            authority: AuthorityLevel::Implementation,
            confidence: 0.0,
            relationship_confidence: None,
            relationship: None,
            retrieval_sources: Vec::new(),
            signals: RankingSignals::default(),
            verification_state: VerificationStatus::Unverified,
            live_source_verified: false,
            snippet: None,
            workspace_snapshot_id: None,
        }
    }

    /// Deterministic dedup key: same logical content discovered through
    /// different retrievers must fuse into ONE evidence item, never multiply
    /// into fake independent support.
    pub fn fusion_key(&self) -> (String, String, u32) {
        let span_start = self.source_span.map(|s| s.start_line).unwrap_or(u32::MAX);
        (
            self.source_type.as_str().to_owned(),
            self.source_id.clone(),
            span_start,
        )
    }
}

str_enum! {
    /// Kinds of meaningful contradictory evidence the manager detects.
    ContradictionKind {
        /// Two CURRENT items of the same source type disagree.
        ConflictingValues => "CONFLICTING_VALUES",
        /// Knowledge/documentation disagrees with implementation or config.
        KnowledgeVsImplementation => "KNOWLEDGE_VS_IMPLEMENTATION",
        /// A test expectation contradicts implementation evidence.
        TestVsImplementation => "TEST_VS_IMPLEMENTATION",
        /// Multiple incompatible definitions remain unresolved.
        AmbiguousDefinition => "AMBIGUOUS_DEFINITION",
        /// A stale duplicate of an item that also has CURRENT evidence.
        SupersededStale => "SUPERSEDED_STALE",
    }
}

/// One detected contradiction between two evidence items. Both items stay
/// surfaced — contradictions are disclosed, never silently resolved
/// (evidence.md invariant 6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contradiction {
    /// Contradiction kind.
    pub kind: ContradictionKind,
    /// First evidence id.
    pub evidence_a: String,
    /// Second evidence id.
    pub evidence_b: String,
    /// Human-readable description (no secret content).
    pub description: String,
}
