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
pub mod cpu_isolation;
pub mod diagnostics;
pub mod disk_safety;
pub mod embedding_policy;
pub mod embedding_profile;
pub mod enrich;
pub mod error;
pub mod generation;
pub mod identity;
pub mod instruction;
pub mod invalidate;
pub mod learned_tuning;
pub mod model_assets;
pub mod model_lifecycle;
pub mod provider;
pub mod providers;
pub mod qwen3_provider;
pub mod scheduler;
pub mod selection;
pub mod store;
pub mod throughput_controller;

pub use bge_embedder::BgeEmbedder;
pub use cpu_isolation::CpuIsolationPlan;
pub use diagnostics::{
    DiagnosticContext, SemanticProgressSnapshot, WhySlowDiagnostic, diagnose_why_slow,
};
pub use disk_safety::{DiskClearance, DiskFootprintSummary, DiskSafetyConfig, DiskSafetyGuard};
pub use generation::{GenerationManager, GenerationRecord, GenerationStatus};
pub use instruction::{format_query_instruction, CODE_RETRIEVAL_V1_ID, CODE_RETRIEVAL_V1_TEMPLATE};
pub use learned_tuning::{LearnedTuningManager, LearnedTuningRecord, TuningKey};
pub use model_assets::{ModelAssetError, ModelAssetManager, ModelAssetStatus, ModelFileSpec, ModelManifest};
pub use model_lifecycle::{ModelLifecycleState, SharedModelHandle};
pub use qwen3_provider::{Qwen3Embedder, QwenPooling, QWEN_MODEL_ID, QWEN_PROVIDER_ID};
pub use scheduler::{
    HierarchicalFairnessScheduler, QueueBackpressure, ScheduledUnit, SchedulerConfig,
};
pub use throughput_controller::{
    CandidateAllocation, ControllerAction, ControllerPhase, ThroughputController,
    ThroughputControllerConfig,
};
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
    CancelFlag, EmbeddingExecutionBudget, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput,
    EmbeddingProvider, ProviderConcurrencyContract, ResourceUsage, SemanticProvider, cosine,
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
