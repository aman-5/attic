//! Opt-in authoritative spec workflow, separate from disposable source indexes.

pub mod boundary;
pub mod citations;
pub mod error;
pub mod git;
pub mod hooks;
pub mod pack;
pub mod review;
pub mod service;
pub mod state;
pub mod store;
pub mod verifier;
pub mod waves;

pub use error::{Result, SpecError};

/// SHA-256 identity of raw bytes, without normalization.
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// Checked UTC microseconds used by every persisted workflow timestamp.
pub fn now_micros() -> Result<i64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| SpecError::new("ERR_CLOCK", "clock precedes Unix epoch"))?;
    i64::try_from(elapsed.as_micros())
        .map_err(|_| SpecError::new("ERR_CLOCK", "clock exceeds timestamp range"))
}
