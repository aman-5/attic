//! Model asset management and atomic activation (Master Plan V2 §47–§50, CP8).
//!
//! Enforces:
//! - Pinned model IDs, revisions, checksums, and license provenance.
//! - Two-phase atomic activation: download to staging → checksum/probe → atomic promote to snapshot.
//! - Failure isolation: corrupted or partial downloads are pruned; active models remain untouched.
//! - Non-blocking semantics: canonical indexing proceeds even if assets are downloading or unavailable.

use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors arising from model asset operations.
#[derive(Debug, Error)]
pub enum ModelAssetError {
    #[error("I/O error during model asset operation: {0}")]
    Io(#[from] std::io::Error),
    #[error("checksum mismatch for {filename}: expected {expected}, computed {computed}")]
    ChecksumMismatch {
        filename: String,
        expected: String,
        computed: String,
    },
    #[error("file missing from model staging: {0}")]
    MissingFile(String),
    #[error("model validation failed: {0}")]
    ValidationFailed(String),
    #[error("download failed: {0}")]
    DownloadFailed(String),
    #[error("model assets not found in local cache (offline mode)")]
    Offline,
}

/// Specification for one required model asset file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFileSpec {
    pub filename: String,
    pub expected_sha256: Option<String>,
    pub expected_size_bytes: Option<u64>,
}

/// Pinned immutable manifest for a specific model revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    pub model_id: String,
    pub repo_owner: String,
    pub repo_name: String,
    pub pinned_revision: String,
    pub files: Vec<ModelFileSpec>,
    pub license: String,
    pub provenance: String,
}

impl ModelManifest {
    /// Canonical manifest for Qwen3-Embedding-0.6B.
    pub fn qwen3_default() -> Self {
        Self {
            model_id: "qwen3-embedding-0.6b".to_string(),
            repo_owner: "Qwen".to_string(),
            repo_name: "Qwen3-Embedding-0.6B".to_string(),
            pinned_revision: "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3".to_string(), // Stable pinned commit hash
            // SHA-256 pinned from the official HF revision above (integrity
            // against corruption; first download trusts HF+TLS like any
            // lockfile bootstrap). Verification is now real SHA-256 (r05).
            files: vec![
                ModelFileSpec {
                    filename: "config.json".to_string(),
                    expected_sha256: Some(
                        "b5bf1f51fc45be473a54718cef92448d90a1be001bf9b9a44b8c7f10a19feaa9"
                            .to_string(),
                    ),
                    expected_size_bytes: None,
                },
                ModelFileSpec {
                    filename: "tokenizer.json".to_string(),
                    expected_sha256: Some(
                        "def76fb086971c7867b829c23a26261e38d9d74e02139253b38aeb9df8b4b50a"
                            .to_string(),
                    ),
                    expected_size_bytes: None,
                },
                ModelFileSpec {
                    filename: "model.safetensors".to_string(),
                    expected_sha256: Some(
                        "0437e45c94563b09e13cb7a64478fc406947a93cb34a7e05870fc8dcd48e23fd"
                            .to_string(),
                    ),
                    expected_size_bytes: None,
                },
            ],
            license: "Apache-2.0".to_string(),
            provenance: "https://huggingface.co/Qwen/Qwen3-Embedding-0.6B".to_string(),
        }
    }
}

/// Status of model assets on local disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelAssetStatus {
    /// Assets are not present locally.
    NotPresent,
    /// Assets are currently staging.
    Staging,
    /// Assets are verified and active in the local snapshot.
    Active { path: PathBuf, revision: String },
    /// Asset operation failed.
    Failed { reason: String },
}

/// Manager for staging, verifying, and atomically activating model assets.
pub struct ModelAssetManager {
    base_dir: PathBuf,
    manifest: ModelManifest,
}

impl ModelAssetManager {
    pub fn new(base_dir: &Path, manifest: ModelManifest) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
            manifest,
        }
    }

    /// Snapshot directory path for the pinned revision:
    /// `<base_dir>/models--<owner>--<repo>/snapshots/<revision>/`
    pub fn snapshot_dir(&self) -> PathBuf {
        self.base_dir
            .join(format!(
                "models--{}--{}",
                self.manifest.repo_owner, self.manifest.repo_name
            ))
            .join("snapshots")
            .join(&self.manifest.pinned_revision)
    }

    /// Staging directory path for in-progress preparation:
    /// `<base_dir>/staging/<owner>--<repo>--<revision>/`
    pub fn staging_dir(&self) -> PathBuf {
        self.base_dir.join("staging").join(format!(
            "{}--{}--{}",
            self.manifest.repo_owner, self.manifest.repo_name, self.manifest.pinned_revision
        ))
    }

    /// Check current local asset status without network operations.
    pub fn check_status(&self) -> ModelAssetStatus {
        let snapshot = self.snapshot_dir();
        if !snapshot.is_dir() {
            return ModelAssetStatus::NotPresent;
        }

        for file_spec in &self.manifest.files {
            let p = snapshot.join(&file_spec.filename);
            if !p.is_file() {
                return ModelAssetStatus::NotPresent;
            }
        }

        ModelAssetStatus::Active {
            path: snapshot,
            revision: self.manifest.pinned_revision.clone(),
        }
    }

    /// Prepare a clean staging directory.
    pub fn prepare_staging(&self) -> Result<PathBuf, ModelAssetError> {
        let staging = self.staging_dir();
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;
        Ok(staging)
    }

    /// Compute the real SHA-256 hex digest of a file (streamed; multi-GB
    /// weights must never load whole). r05 fix: this previously computed
    /// BLAKE3 despite the name, so no pinned SHA-256 could ever match.
    pub fn compute_file_sha256(path: &Path) -> Result<String, ModelAssetError> {
        use sha2::Digest;
        let mut file = File::open(path)?;
        let mut hasher = sha2::Sha256::new();
        let mut buffer = vec![0u8; 1024 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        Ok(hex::encode(hasher.finalize()))
    }

    /// Validate all files in the staging directory against the manifest.
    pub fn validate_staging(&self, staging: &Path) -> Result<(), ModelAssetError> {
        for file_spec in &self.manifest.files {
            let path = staging.join(&file_spec.filename);
            if !path.is_file() {
                return Err(ModelAssetError::MissingFile(file_spec.filename.clone()));
            }

            if let Some(expected_size) = file_spec.expected_size_bytes {
                let meta = fs::metadata(&path)?;
                if meta.len() != expected_size {
                    return Err(ModelAssetError::ValidationFailed(format!(
                        "size mismatch for {}: expected {}, got {}",
                        file_spec.filename,
                        expected_size,
                        meta.len()
                    )));
                }
            }

            if let Some(ref expected_hash) = file_spec.expected_sha256 {
                let computed = Self::compute_file_sha256(&path)?;
                if &computed != expected_hash {
                    return Err(ModelAssetError::ChecksumMismatch {
                        filename: file_spec.filename.clone(),
                        expected: expected_hash.clone(),
                        computed,
                    });
                }
            }
        }

        // Validate basic integrity of config.json if present
        let config_path = staging.join("config.json");
        if config_path.is_file() {
            let content = fs::read_to_string(&config_path)?;
            let _val: serde_json::Value = serde_json::from_str(&content).map_err(|e| {
                ModelAssetError::ValidationFailed(format!("invalid JSON in config.json: {e}"))
            })?;
        }

        Ok(())
    }

    /// Atomically activate model files from staging into the active snapshot directory.
    pub fn activate_staging(&self, staging: &Path) -> Result<PathBuf, ModelAssetError> {
        self.validate_staging(staging)?;

        let target_dir = self.snapshot_dir();
        if let Some(parent) = target_dir.parent() {
            fs::create_dir_all(parent)?;
        }

        // Never delete the active snapshot before its replacement is in
        // place: move it aside, rename staging in, then drop the backup. A
        // failed rename restores the backup; a crash in between leaves the
        // previous snapshot recoverable beside the target (`.previous-*`).
        let backup = if target_dir.exists() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let backup = target_dir.with_file_name(format!(
                "{}.previous-{stamp}",
                self.manifest.pinned_revision
            ));
            fs::rename(&target_dir, &backup)?;
            Some(backup)
        } else {
            None
        };

        if let Err(e) = fs::rename(staging, &target_dir) {
            if let Some(b) = &backup {
                let _ = fs::rename(b, &target_dir);
            }
            return Err(e.into());
        }
        if let Some(b) = backup {
            let _ = fs::remove_dir_all(b);
        }

        // Update refs/main pointer
        let repo_root = self.base_dir.join(format!(
            "models--{}--{}",
            self.manifest.repo_owner, self.manifest.repo_name
        ));
        let refs_dir = repo_root.join("refs");
        fs::create_dir_all(&refs_dir)?;
        let mut main_ref = File::create(refs_dir.join("main"))?;
        writeln!(main_ref, "{}", self.manifest.pinned_revision)?;

        Ok(target_dir)
    }

    /// Verify the ACTIVE snapshot against the pinned manifest (r05).
    /// `MissingFile`/`Offline` are transient (partial or absent download);
    /// `ChecksumMismatch`/`ValidationFailed` are permanent for this content —
    /// the caller must quarantine and re-download, never load or retry the
    /// same bytes.
    pub fn verify_active_snapshot(&self) -> Result<PathBuf, ModelAssetError> {
        let snapshot = self.snapshot_dir();
        if !snapshot.is_dir() {
            return Err(ModelAssetError::Offline);
        }
        self.validate_staging(&snapshot)?;
        Ok(snapshot)
    }

    /// Move a corrupt snapshot aside so the next download starts clean.
    /// Never deletes — the evidence stays for operators. Returns the
    /// quarantine path when a snapshot existed.
    pub fn quarantine_snapshot(&self) -> Result<Option<PathBuf>, ModelAssetError> {
        let snapshot = self.snapshot_dir();
        if !snapshot.exists() {
            return Ok(None);
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dest =
            snapshot.with_file_name(format!("{}.corrupt-{stamp}", self.manifest.pinned_revision));
        fs::rename(&snapshot, &dest)?;
        Ok(Some(dest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_status_not_present_on_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = ModelManifest::qwen3_default();
        let mgr = ModelAssetManager::new(tmp.path(), manifest);
        assert_eq!(mgr.check_status(), ModelAssetStatus::NotPresent);
    }

    #[test]
    fn atomic_activation_promotes_staging_and_updates_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = ModelManifest::qwen3_default();
        let mgr = ModelAssetManager::new(tmp.path(), manifest.clone());

        let staging = mgr.prepare_staging().unwrap();
        fs::write(staging.join("config.json"), "{\"vocab_size\": 1000}").unwrap();
        fs::write(staging.join("tokenizer.json"), "{}").unwrap();
        fs::write(staging.join("model.safetensors"), "binary-weights").unwrap();
        // Pin from the staged content (production pins come from the official
        // revision; tests pin what they staged).
        for f in manifest.files.iter_mut() {
            f.expected_sha256 =
                Some(ModelAssetManager::compute_file_sha256(&staging.join(&f.filename)).unwrap());
        }
        let mgr = ModelAssetManager::new(tmp.path(), manifest.clone());

        let active_path = mgr
            .activate_staging(&staging)
            .expect("activation must succeed");
        assert!(active_path.is_dir());
        assert!(!staging.exists(), "staging dir should be moved");

        // Verify status now reports Active
        match mgr.check_status() {
            ModelAssetStatus::Active { revision, .. } => {
                assert_eq!(revision, manifest.pinned_revision);
            }
            other => panic!("expected Active status, got {other:?}"),
        }

        // Verify refs/main
        let refs_main = tmp
            .path()
            .join(format!(
                "models--{}--{}",
                manifest.repo_owner, manifest.repo_name
            ))
            .join("refs")
            .join("main");
        assert_eq!(
            fs::read_to_string(refs_main).unwrap().trim(),
            manifest.pinned_revision
        );
    }

    #[test]
    fn validation_fails_on_missing_required_file() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = ModelManifest::qwen3_default();
        // Unpinned: this test exercises the MISSING-file check, which runs
        // before any checksum comparison.
        for f in manifest.files.iter_mut() {
            f.expected_sha256 = None;
        }
        let mgr = ModelAssetManager::new(tmp.path(), manifest);

        let staging = mgr.prepare_staging().unwrap();
        // Only write config.json, leaving out tokenizer and safetensors
        fs::write(staging.join("config.json"), "{}").unwrap();

        let err = mgr.activate_staging(&staging).unwrap_err();
        match err {
            ModelAssetError::MissingFile(f) => assert_eq!(f, "tokenizer.json"),
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(
            staging.exists(),
            "staging remains for diagnostic inspection"
        );
    }

    #[test]
    fn validation_fails_on_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = ModelManifest::qwen3_default();
        // Unpinned: this test exercises the config.json JSON-validity check,
        // which runs after checksum comparison.
        for f in manifest.files.iter_mut() {
            f.expected_sha256 = None;
        }
        let mgr = ModelAssetManager::new(tmp.path(), manifest);

        let staging = mgr.prepare_staging().unwrap();
        fs::write(staging.join("config.json"), "NOT_JSON").unwrap();
        fs::write(staging.join("tokenizer.json"), "{}").unwrap();
        fs::write(staging.join("model.safetensors"), "binary").unwrap();

        let err = mgr.activate_staging(&staging).unwrap_err();
        match err {
            ModelAssetError::ValidationFailed(msg) => {
                assert!(msg.contains("invalid JSON"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn qwen3_pinned_manifest_integrity() {
        let manifest = ModelManifest::qwen3_default();
        assert_eq!(manifest.model_id, "qwen3-embedding-0.6b");
        assert_eq!(manifest.repo_owner, "Qwen");
        assert_eq!(manifest.repo_name, "Qwen3-Embedding-0.6B");
        assert_eq!(
            manifest.pinned_revision,
            "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3"
        );
        assert_eq!(manifest.license, "Apache-2.0");

        let filenames: Vec<&str> = manifest.files.iter().map(|f| f.filename.as_str()).collect();
        assert!(filenames.contains(&"config.json"));
        assert!(filenames.contains(&"tokenizer.json"));
        assert!(filenames.contains(&"model.safetensors"));
    }

    /// r05: the default manifest pins REAL sha256 values, and the verifier
    /// is real SHA-256 (a BLAKE3 mislabel previously made pinning
    /// impossible).
    #[test]
    fn qwen3_manifest_has_real_sha256_pins() {
        let manifest = ModelManifest::qwen3_default();
        for f in &manifest.files {
            let pin = f
                .expected_sha256
                .as_ref()
                .unwrap_or_else(|| panic!("{} must be sha256-pinned", f.filename));
            assert_eq!(pin.len(), 64, "{} pin must be sha256 hex", f.filename);
            assert!(pin.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    /// r05: a corrupt active snapshot fails verification permanently and is
    /// quarantined (evidence preserved), so the next run re-downloads clean.
    #[test]
    fn corrupt_active_snapshot_fails_and_quarantines() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = ModelManifest::qwen3_default();
        let mgr = ModelAssetManager::new(tmp.path(), manifest);

        // Stage files with WRONG content, then force-activate by creating the
        // snapshot layout directly (bypassing validation).
        let snapshot = mgr.snapshot_dir();
        fs::create_dir_all(&snapshot).unwrap();
        fs::write(snapshot.join("config.json"), "{}").unwrap();
        fs::write(snapshot.join("tokenizer.json"), "{}").unwrap();
        fs::write(snapshot.join("model.safetensors"), "not-the-real-weights").unwrap();

        let err = mgr.verify_active_snapshot().unwrap_err();
        assert!(
            matches!(err, ModelAssetError::ChecksumMismatch { .. }),
            "corrupt snapshot must fail with checksum mismatch, got {err:?}"
        );

        let q = mgr
            .quarantine_snapshot()
            .unwrap()
            .expect("snapshot existed, must quarantine");
        assert!(!snapshot.exists(), "corrupt snapshot moved aside");
        assert!(q.exists(), "quarantine preserves the evidence");
        assert_eq!(
            mgr.check_status(),
            ModelAssetStatus::NotPresent,
            "after quarantine the model is cleanly absent for re-download"
        );
    }

    /// r05: a correctly pinned snapshot verifies. Uses a manifest whose pins
    /// are computed from the staged content (same code path as production).
    #[test]
    fn active_snapshot_with_matching_pins_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        let mut manifest = ModelManifest::qwen3_default();
        let mgr_probe = ModelAssetManager::new(tmp.path(), manifest.clone());
        let snapshot = mgr_probe.snapshot_dir();
        fs::create_dir_all(&snapshot).unwrap();
        fs::write(snapshot.join("config.json"), "{\"a\":1}").unwrap();
        fs::write(snapshot.join("tokenizer.json"), "{}").unwrap();
        fs::write(snapshot.join("model.safetensors"), "weights").unwrap();
        for f in manifest.files.iter_mut() {
            f.expected_sha256 =
                Some(ModelAssetManager::compute_file_sha256(&snapshot.join(&f.filename)).unwrap());
        }
        let mgr = ModelAssetManager::new(tmp.path(), manifest);
        let verified = mgr.verify_active_snapshot().unwrap();
        assert_eq!(verified, snapshot);
    }
}
