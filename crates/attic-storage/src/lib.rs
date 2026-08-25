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

pub mod connection;
pub mod error;
pub mod fts;
pub mod indexing_publication;
pub mod migration;
pub mod repository;
pub mod writer;

pub use connection::{DbPool, open_db};
pub use error::StorageError;
pub use fts::{
    FtsSearchParams, FtsSearchResult, MAX_SEARCH_RESULTS, NewRetrievalUnit,
    delete_retrieval_unit_with_fts, delete_retrieval_units_for_file, fts_path_lookup, fts_search,
    insert_retrieval_unit_with_fts,
};
pub use indexing_publication::{
    IndexPublication, IndexPublicationStats, PublicationFile, PublicationOccurrence,
    PublicationRetrievalUnit, submit_index_publication,
};
pub use migration::run_migrations;
pub use writer::{WriterQueue, WriterQueueHandle};

// Repository sub-module re-exports for use by attic-indexing and attic-server.
pub use repository::file_occurrence::{
    NewFileOccurrence, insert_file_occurrence, lookup_file_identity_by_basis,
    lookup_latest_file_occurrence_for_path, upsert_file_identity,
};
pub use repository::index_generation::insert_index_generation;
pub use repository::publication::{PublicationItem, publish_file_batch};
pub use repository::repository::{
    DbStats, RepositoryStats, get_db_stats, get_repository_path, get_repository_stats,
    lookup_repository_by_root_path, upsert_repository,
};
pub use repository::source_revision::{
    exists_source_revision, insert_source_revision, insert_source_revision_with_hashes,
};
