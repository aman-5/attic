//! S3/S4 — Repository CRUD and publication batch sub-modules.

pub mod file_occurrence;
pub mod index_generation;
pub mod publication;
#[allow(clippy::module_inception)]
pub mod repository;
pub mod source_revision;
