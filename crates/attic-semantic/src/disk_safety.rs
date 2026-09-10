//! Disk safety reserve and disk pressure guards (Master Plan V2 §41, CP13).
//!
//! Enforces:
//! - Protected disk reserve: guarantees minimum free disk space for canonical indexing and OS operations.
//! - Emergency threshold: when available disk falls below safety limits, pauses semantic operations
//!   (model downloads, generation rebuilds, queue processing) while allowing canonical operations to continue.
//! - Disk footprint accounting: tracks disk usage across model assets, semantic DB, and vector storage.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Operating clearance status based on available host disk space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiskClearance {
    /// Ample disk headroom: all semantic indexing and model downloading permitted.
    Clear,
    /// Warning headroom: semantic background batching should be throttled.
    LowSpaceWarning,
    /// Emergency low disk: semantic writes and downloads HALTED to preserve canonical indexing.
    EmergencyHalt,
}

/// Configuration for disk safety thresholds in MiB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSafetyConfig {
    /// Minimum headroom below which semantic downloads/rebuilds are halted (default: 2048 MiB).
    pub emergency_reserve_mib: u64,
    /// Headroom below which warning/throttling begins (default: 5120 MiB).
    pub warning_threshold_mib: u64,
}

impl Default for DiskSafetyConfig {
    fn default() -> Self {
        Self {
            emergency_reserve_mib: 2_048,
            warning_threshold_mib: 5_120,
        }
    }
}

/// Accounting summary of disk usage across Attic storage components.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DiskFootprintSummary {
    pub model_assets_bytes: u64,
    pub semantic_db_bytes: u64,
    pub staging_bytes: u64,
    pub total_semantic_bytes: u64,
}

/// Controller guarding disk operations against disk exhaustion.
pub struct DiskSafetyGuard {
    config: DiskSafetyConfig,
}

impl DiskSafetyGuard {
    pub fn new(config: DiskSafetyConfig) -> Self {
        Self { config }
    }

    /// Evaluate current disk clearance based on available disk space in MiB.
    pub fn evaluate_clearance(&self, available_disk_mib: u64) -> DiskClearance {
        if available_disk_mib < self.config.emergency_reserve_mib {
            DiskClearance::EmergencyHalt
        } else if available_disk_mib < self.config.warning_threshold_mib {
            DiskClearance::LowSpaceWarning
        } else {
            DiskClearance::Clear
        }
    }

    /// Whether semantic indexing and model downloads are permitted under current disk space.
    pub fn is_semantic_permitted(&self, available_disk_mib: u64) -> bool {
        self.evaluate_clearance(available_disk_mib) != DiskClearance::EmergencyHalt
    }

    /// Calculate recursive directory size in bytes.
    pub fn calculate_dir_size(path: &Path) -> u64 {
        if !path.exists() {
            return 0;
        }
        if path.is_file() {
            return std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    total += Self::calculate_dir_size(&p);
                } else if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
        total
    }

    /// Compute footprint summary across attic directories.
    pub fn compute_footprint(base_dir: &Path) -> DiskFootprintSummary {
        let models_dir = base_dir.join("models");
        let staging_dir = base_dir.join("staging");
        let semantic_db = base_dir.join("semantic.db");

        let model_assets_bytes = Self::calculate_dir_size(&models_dir);
        let staging_bytes = Self::calculate_dir_size(&staging_dir);
        let semantic_db_bytes = if semantic_db.is_file() {
            std::fs::metadata(&semantic_db)
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        let total_semantic_bytes = model_assets_bytes + staging_bytes + semantic_db_bytes;

        DiskFootprintSummary {
            model_assets_bytes,
            semantic_db_bytes,
            staging_bytes,
            total_semantic_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluate_disk_clearance_thresholds() {
        let guard = DiskSafetyGuard::new(DiskSafetyConfig {
            emergency_reserve_mib: 1_000,
            warning_threshold_mib: 3_000,
        });

        // Plentiful space
        assert_eq!(guard.evaluate_clearance(5_000), DiskClearance::Clear);
        assert!(guard.is_semantic_permitted(5_000));

        // Low space warning
        assert_eq!(
            guard.evaluate_clearance(2_000),
            DiskClearance::LowSpaceWarning
        );
        assert!(guard.is_semantic_permitted(2_000));

        // Emergency halt: strictly protects canonical operation
        assert_eq!(guard.evaluate_clearance(500), DiskClearance::EmergencyHalt);
        assert!(!guard.is_semantic_permitted(500));
    }

    #[test]
    fn calculate_dir_size_measures_files() {
        let tmp = tempfile::tempdir().unwrap();
        let f1 = tmp.path().join("f1.bin");
        let f2 = tmp.path().join("f2.bin");
        std::fs::write(&f1, vec![0u8; 1024]).unwrap();
        std::fs::write(&f2, vec![0u8; 2048]).unwrap();

        let size = DiskSafetyGuard::calculate_dir_size(tmp.path());
        assert_eq!(size, 3072);
    }
}
