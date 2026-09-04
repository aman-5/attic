//! Attic semantic intelligence (Phase 5) — the DISPOSABLE derived layer.
//!
//! Architecture (ADR-013/ADR-014):
//!
//! ```text
//! canonical source → retrieval units
//!   → SemanticUnitSelection (explicit, versioned policy)
//!   → SemanticProvider      (provider-neutral embedding contract)
//!   → SemanticStore         (separate, deletable SQLite; kNN)
//!   → semantic candidate generator (inside attic-retrieval)
//!   → EXISTING Phase 4 fusion / ranking / validation
//! ```
//!
//! Critical invariant: `semantic failure != canonical-index failure`.
//! Every type here is safe to delete; FTS, symbols, structure, evidence and
//! verification continue to work untouched.

pub mod bge_embedder;
pub mod embedding_policy;
pub mod embedding_profile;
pub mod enrich;
pub mod error;
pub mod identity;
pub mod invalidate;
pub mod provider;
pub mod providers;
pub mod selection;
pub mod store;

pub use bge_embedder::BgeEmbedder;
pub use embedding_policy::{EmbeddingPolicy, EmbeddingRecommendation};
pub use embedding_profile::{
    ClaimOutcome, EmbeddingIntentSource, EmbeddingProfile, EmbeddingSpaceDescriptor,
    PoolingStrategy, ProfileCheck, TruncationPolicy, check_requested_profile,
};
pub use enrich::{BackgroundEnricher, EnrichStats, EnrichmentConfig, drive};
pub use error::SemanticError;
pub use identity::{SemanticUnitIdentity, content_hash};
pub use invalidate::{ReconcileReport, reconcile};
pub use provider::{
    CancelFlag, EmbeddingInput, EmbeddingOutput, ResourceUsage, SemanticProvider, cosine,
};
pub use providers::{
    FailingProvider, HashingEmbedder, RecordingProvider, SlowProvider, UnavailableProvider,
};
pub use selection::{
    EX_BELOW_THRESHOLD, EX_CAP_REPO, EX_CAP_TOTAL, EX_DUPLICATE, EX_GENERATED_PATH,
    EX_GENERATED_TYPE, EX_TOO_LARGE, SEMANTIC_SELECTION_VERSION, SelectedUnit, SelectionConfig,
    SelectionReport, SelectionSignals, select_units,
};
pub use store::{EmbeddingRecord, KnnResult, NearestHit, QueueItem, ScanBudget, SemanticStore};

/// Re-exported canonical read types the layer consumes.
pub use attic_storage::{SemanticUnitRow, UnitAnchor};
