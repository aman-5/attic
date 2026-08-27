#![forbid(unsafe_code)]
#![deny(missing_docs)]
//! `attic-storage` — SQLite-backed persistence layer for the Attic MCP server.
//!
//! Phase 1A implements:
//! - S1: connection configuration (WAL, PRAGMAs, pool)
//! - S2: migration runner
//! - S3: core repository/file CRUD
//! - S4: publication batch
//! - S5: FTS helpers (external-content tables)
//! - S6: bounded writer queue
//! - S7: production resource manager (Phase 7 hardening)

pub mod connection;
pub mod crossrepo_ops;
pub mod error;
pub mod fts;
pub mod indexing_publication;
pub mod invalidation_ops;
pub mod migration;
pub mod ops_tasks;
pub mod repository;
pub mod resource_manager;
pub mod retrieval_reads;
pub mod semantic_reads;
pub mod server_state;
pub mod writer;

pub use connection::{DbPool, open_db};
pub use crossrepo_ops::{
    CatalogRow, DeclarationRow, WorkspaceSnapshotRevision, WorkspaceSnapshotRow, XrepoEdge,
    all_catalog_entries, all_repository_ids, catalog_entry, create_workspace_snapshot,
    cross_edges_all, cross_edges_between, cross_edges_for_entities, cross_edges_touching,
    declarations_for_repository, delete_all_xrepo_edges_touching,
    delete_declarations_for_repository, delete_xrepo_edges_between, insert_declaration,
    insert_xrepo_edge, latest_workspace_snapshot, providers_of_identity,
    remove_repository_crossrepo_data, snapshot_revisions, upsert_catalog_row,
};
pub use error::StorageError;
pub use fts::{
    FtsSearchParams, FtsSearchResult, MAX_SEARCH_RESULTS, NewRetrievalUnit,
    delete_retrieval_unit_with_fts, delete_retrieval_units_for_file, fts_path_lookup, fts_search,
    insert_retrieval_unit_with_fts,
};
pub use indexing_publication::{
    IndexPublication, IndexPublicationStats, PublicationFile, PublicationNode,
    PublicationOccurrence, PublicationRelationship, PublicationRetrievalUnit,
    PublicationStructuralFile, PublicationSymbolDef, PublicationUnitLink, submit_index_publication,
};
pub use invalidation_ops::{
    FreshnessTotals, InvalidationCounts, close_pending_records_for_occurrence,
    get_freshness_totals, invalidate_for_occurrences, record_invalidation, record_recomputed,
};
pub use migration::run_migrations;
pub use ops_tasks::{
    ClaimedTask, EnqueueOutcome, IncrementalTaskPayload, TASK_INCREMENTAL_INDEX,
    TASK_RECONCILIATION, TaskCounts, TaskOutcome, cancel_pending_task, claim_next_pending_task,
    enqueue_task, finish_task, get_task_counts, recover_interrupted_tasks, set_task_checkpoint,
};
pub use resource_manager::{ResourceAdvisory, ResourceConfig, ResourceMonitor};
pub use server_state::{ServerState, get_server_state, record_clean_shutdown, record_startup};
pub use writer::{WriterQueue, WriterQueueHandle};

// Repository sub-module re-exports for use by attic-indexing and attic-server.
pub use repository::file_occurrence::{
    NewFileOccurrence, OccurrenceSnapshot, current_path_hashes_for_repository,
    insert_file_occurrence, lookup_file_identity_by_basis, lookup_latest_file_occurrence_for_path,
    lookup_occurrence_snapshot, upsert_file_identity,
};
pub use repository::identity_links::{
    NewIdentityLink, basis as identity_basis, confidence as identity_confidence,
    insert_identity_link, latest_link_for_identity,
};
pub use repository::index_generation::insert_index_generation;
pub use repository::publication::{PublicationItem, publish_file_batch};
pub use repository::repository::{
    DbStats, RepositoryStats, get_db_stats, get_repository_path, get_repository_stats,
    lookup_repository_by_root_path, upsert_repository,
};
pub use repository::source_revision::{
    exists_source_revision, insert_source_revision, insert_source_revision_with_hashes,
    latest_source_revision_for_repository,
};
pub use repository::structural::{StructuralCounts, lookup_symbol_definition_occurrence};
pub use retrieval_reads::{
    FileHeader, NewRetrievalPlanRecord, NodeRow, RelationshipEdge, SymbolHit, file_header_by_id,
    get_retrieval_plan_json, insert_retrieval_plan, latest_occurrence_for_path,
    lookup_symbol_exact, relationships_for_entity, search_symbols, structural_nodes_by_type,
    structural_nodes_for_file,
};
pub use semantic_reads::{
    SemanticUnitRow, UnitAnchor, retrieval_unit_anchor, semantic_unit_rows, semantic_units_by_ids,
};
