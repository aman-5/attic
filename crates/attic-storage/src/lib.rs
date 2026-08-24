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
pub mod migration;
pub mod repository;
pub mod writer;

pub use connection::{open_db, DbPool};
pub use error::StorageError;
pub use migration::run_migrations;
pub use writer::{WriterQueue, WriterQueueHandle};
