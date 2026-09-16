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

pub mod cpu_isolation;
pub mod deferred_provider;
pub mod diagnostics;
pub mod disk_safety;
pub mod enrich;
pub mod error;
pub mod generation;
pub mod identity;
pub mod instruction;
pub mod invalidate;
pub mod learned_tuning;
pub mod model_assets;
pub mod model_lifecycle;
#[cfg(feature = "ort-directml")]
pub mod ort_directml;
pub mod provider;
pub mod qwen3_model;
pub mod qwen3_provider;
pub mod scheduler;
pub mod selection;
pub mod store;
pub mod testing;
pub mod throughput_controller;
pub mod worker_supervisor;

pub use cpu_isolation::CpuIsolationPlan;
pub use deferred_provider::{DeferredProvider, ModelLifecycle};
pub use diagnostics::{
    DiagnosticContext, SemanticLatencyBreakdown, SemanticProgressSnapshot, WhySlowDiagnostic,
    diagnose_why_slow,
};
pub use disk_safety::{DiskClearance, DiskFootprintSummary, DiskSafetyConfig, DiskSafetyGuard};
pub use enrich::{BackgroundEnricher, EnrichStats, EnrichmentConfig, drive};
pub use error::SemanticError;
pub use generation::{GenerationManager, GenerationRecord, GenerationStatus};
pub use identity::{SemanticUnitIdentity, content_hash};
pub use instruction::{CODE_RETRIEVAL_V1_ID, CODE_RETRIEVAL_V1_TEMPLATE, format_query_instruction};
pub use invalidate::{ReconcileReport, reconcile};
pub use learned_tuning::{LearnedTuningManager, LearnedTuningRecord, TuningKey};
pub use model_assets::{
    ModelAssetError, ModelAssetManager, ModelAssetStatus, ModelFileSpec, ModelManifest,
};
pub use model_lifecycle::{ModelLifecycleState, SharedModelHandle};
#[cfg(feature = "ort-directml")]
pub use ort_directml::{ORT_PROVIDER_ID, OrtDirectMlProvider};
pub use provider::{
    CancelFlag, EmbeddingExecutionBudget, EmbeddingFingerprint, EmbeddingInput, EmbeddingOutput,
    EmbeddingProvider, ExecutionBackend, ProviderConcurrencyContract, ResourceUsage,
    SemanticProvider, UnavailableProvider, cosine,
};
pub use qwen3_provider::{QWEN_MODEL_ID, QWEN_PROVIDER_ID, Qwen3Embedder, QwenPooling};
pub use scheduler::{
    HierarchicalFairnessScheduler, QueueBackpressure, ScheduledUnit, SchedulerConfig,
};
pub use selection::{
    EX_BELOW_THRESHOLD, EX_CAP_REPO, EX_CAP_TOTAL, EX_DUPLICATE, EX_EXCLUDED_GLOB, EX_FILE_TOO_BIG,
    EX_GENERATED_PATH, EX_GENERATED_TYPE, EX_TOO_LARGE, SEMANTIC_SELECTION_VERSION, SelectedUnit,
    SelectionConfig, SelectionReport, SelectionSignals, select_units,
};
pub use store::{EmbeddingRecord, KnnResult, NearestHit, QueueItem, ScanBudget, SemanticStore};
pub use throughput_controller::{
    CandidateAllocation, ControllerAction, ControllerPhase, ThroughputController,
    ThroughputControllerConfig,
};
pub use worker_supervisor::{
    SupervisedWorkerProvider, expected_fingerprint, expected_max_input_bytes,
};

/// Re-exported canonical read types the layer consumes.
pub use attic_storage::{SemanticUnitRow, UnitAnchor};
