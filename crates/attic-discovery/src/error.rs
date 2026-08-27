//! Discovery-specific error type.

use std::path::PathBuf;

use thiserror::Error;

/// Errors that can occur during the discovery pipeline.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// The configured workspace root path does not exist or is not a directory.
    #[error("workspace root is not a directory: {0}")]
    RootNotDirectory(PathBuf),

    /// Path canonicalization failed.
    #[error("cannot canonicalize path {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// A path traversal or symlink-escape attempt was detected.
    #[error("path escapes allowed root: {0}")]
    PathEscape(PathBuf),

    /// An I/O error occurred during the walk.
    #[error("I/O error during discovery walk: {source}")]
    Io {
        #[source]
        source: std::io::Error,
    },

    /// The discovery policy could not be serialized (required for hashing).
    #[error("discovery policy serialization failed: {0}")]
    PolicySerialize(String),

    /// A scan-exempt path references a security-forbidden prefix.
    #[error("scan-exempt path '{0}' matches a security-forbidden prefix — configuration rejected")]
    ScanExemptForbiddenPath(String),

    /// Configuration is invalid for another reason.
    #[error("invalid discovery configuration: {0}")]
    InvalidConfig(String),

    /// `include_untracked = false` was requested but the Git tracked-file set
    /// could not be obtained.  The walk did not proceed — refusing to silently
    /// broaden discovery scope.
    #[error("include_untracked=false requested but git tracked-file set unavailable: {reason}")]
    TrackedFileSetUnavailable { reason: String },

    /// File exceeds the configured maximum size.
    #[error("file exceeds maximum size of {max_bytes} bytes: {path}")]
    FileTooLarge {
        path: PathBuf,
        max_bytes: u64,
    },
}

impl From<std::io::Error> for DiscoveryError {
    fn from(e: std::io::Error) -> Self {
        DiscoveryError::Io { source: e }
    }
}
