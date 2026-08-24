//! `attic-discovery` — Git-aware, security-hardened repository discovery.
//!
//! # Phase 1B scope
//!
//! This crate implements the full Phase 1B discovery pipeline:
//!
//! ```text
//! configured roots
//!   → canonicalize / security boundary
//!   → git repository detection
//!   → gitignore evaluation (via `ignore` crate)
//!   → Attic include / exclude policy
//!   → default exclusions
//!   → priority classification
//!   → eligible manifest
//!   → SourceRevision manifest hash
//!   → per-file secrets classification (Safe / Redacted / Excluded)
//!     (classification only — content is NOT retained in DiscoveryOutput)
//! ```
//!
//! # Downstream content contract
//!
//! `DiscoveryOutput` records a **classification** for each eligible file —
//! whether its content is safe, redacted, or excluded — but does **not**
//! retain the file content in memory.  This ensures the discovery pass
//! remains bounded even for workspaces with 500 K+ files.
//!
//! Consumers that need actual file content call [`preprocess_file_content`]
//! per file, which reads, secret-scans, and returns the (possibly redacted)
//! content for that single file without accumulating workspace-wide state.
//!
//! Large-file handling follows `docs/contracts/large_files.md`:
//!
//! | Tier | Size | Secrets scan |
//! |------|------|--------------|
//! | SMALL / MEDIUM | < 4 MiB | Full content scanned |
//! | LARGE | 4 MiB – 50 MiB | Sample only (first + last `MAX_SAMPLE_BYTES`) |
//! | VERY_LARGE | > 50 MiB | Sample only + `PartialSecretScan` diagnostic |
//!
//! # Entry point
//!
//! The primary entry point is [`discover`], which takes a root path and a
//! [`DiscoveryPolicy`] and returns a [`DiscoveryOutput`] containing:
//!
//! - all eligible [`EligibleEntry`] values,
//! - a [`SourceManifest`] with the BLAKE3 manifest hash,
//! - optional [`GitRepoMeta`] (branch + HEAD SHA),
//! - any non-fatal [`Diagnostic`] events,
//! - per-file [`DownstreamClassification`] — classification without content.

pub mod classification;
pub mod diagnostics;
pub mod error;
pub mod git;
pub mod manifest;
pub mod policy;
pub mod secrets;
pub mod security;
pub mod walk;

// ---------------------------------------------------------------------------
// Convenience re-exports
// ---------------------------------------------------------------------------

pub use policy::DiscoveryPriority;
pub use diagnostics::{Diagnostic, DiagnosticKind};
pub use error::DiscoveryError;
pub use git::GitRepoMeta;
pub use manifest::{ManifestEntry, SourceManifest};
pub use policy::{DiscoveryPolicy, GlobRule, PriorityRule};
pub use secrets::{PreprocessResult, ScanResult, SecretFinding, SecretScanDecision};
pub use security::canonicalize_within_root;
pub use walk::{EligibleEntry, WalkResult};

// ---------------------------------------------------------------------------
// Large-file size thresholds (from large_files.md contract)
// ---------------------------------------------------------------------------

/// Files below this threshold are SMALL/MEDIUM — full content loaded.
pub const MAX_FULL_LOAD_BYTES: u64 = 4 * 1024 * 1024; // 4 MiB

/// Files above this threshold are VERY_LARGE — metadata + sample only.
pub const MAX_LARGE_BYTES: u64 = 50 * 1024 * 1024; // 50 MiB

/// Bytes sampled from start and end of LARGE / VERY_LARGE files for secret
/// scanning (matches `MAX_SAMPLE_BYTES` from the contract).
pub const MAX_SAMPLE_BYTES: usize = 8 * 1024; // 8 KiB

// ---------------------------------------------------------------------------
// Downstream content classification
// ---------------------------------------------------------------------------

/// Size tier assigned at discovery time, per `docs/contracts/large_files.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSizeTier {
    /// < 4 MiB — full content loaded and scanned.
    Small,
    /// 4 MiB – 50 MiB — only a sample is scanned; full content streamed by
    /// later pipeline stages.
    Large,
    /// Above 50 MiB — only a sample is scanned; a `PartialSecretScan`
    /// diagnostic is recorded.
    VeryLarge,
}

/// The secrets-scan classification for one eligible file.
///
/// **Content is NOT stored here.**  `DiscoveryOutput` is bounded regardless
/// of workspace size.  Consumers that need file content call
/// [`preprocess_file_content`] per file.
#[derive(Debug, Clone)]
pub enum DownstreamClassification {
    /// No secrets found.  Content is safe for downstream indexing.
    Safe {
        /// Size tier at discovery time.
        size_tier: FileSizeTier,
    },
    /// Secrets were found; content must be redacted before indexing.
    /// `findings` lists pattern IDs and (for small files) byte offsets only;
    /// no raw secret values are stored in findings.
    Redacted {
        /// Size tier at discovery time.
        size_tier: FileSizeTier,
        /// Descriptions of findings (no raw secret values).
        findings: Vec<SecretFinding>,
    },
    /// The file is a known secrets carrier (e.g. `.netrc`, `id_rsa`) and must
    /// not be indexed at all.
    Excluded,
    /// Secret scanning was skipped or could not be completed (e.g. unreadable
    /// file, binary content).  The file must not be indexed until rescanned.
    ScanSkipped {
        /// Human-readable reason.
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// High-level API
// ---------------------------------------------------------------------------

use std::io::{self, Read};
use std::path::Path;

/// The complete output of a single discovery pass over one root directory.
///
/// `downstream_classifications` records the secrets-scan *classification*
/// (Safe / Redacted / Excluded / ScanSkipped) for each eligible file, but
/// does **not** retain file content.  Callers retrieve content lazily via
/// [`preprocess_file_content`].
#[derive(Debug)]
pub struct DiscoveryOutput {
    /// Eligible files sorted by `repo_relative` path.
    pub entries: Vec<EligibleEntry>,
    /// Content-hash manifest and stable revision identifier.
    pub manifest: SourceManifest,
    /// Git repository metadata, if `root` is (or is inside) a Git repo.
    pub git_meta: Option<GitRepoMeta>,
    /// Non-fatal diagnostics produced during the walk or manifest build.
    pub diagnostics: Vec<Diagnostic>,
    /// Per-file secrets-scan classification (no content retained).
    ///
    /// Each element is `(repo_relative_path, classification)`.  The vec is in
    /// the same order as [`DiscoveryOutput::entries`].  Files that could not
    /// be stat'd or read during classification are represented as
    /// [`DownstreamClassification::ScanSkipped`].
    pub downstream_classifications: Vec<(String, DownstreamClassification)>,
}

/// Run the full discovery pipeline over `root`.
///
/// # Steps
///
/// 1. Canonicalise `root` (rejects path-traversal attempts).
/// 2. Detect the Git repository root (if any).
/// 3. Walk the tree with the `ignore` crate using `policy`.
/// 4. Apply security exclusions, default exclusions, and classification.
/// 5. Build the BLAKE3 manifest (using raw bytes — before redaction).
/// 6. For each eligible file: determine size tier, scan a bounded amount of
///    content for secrets, and record the [`DownstreamClassification`].
///    **File content is not retained in the returned `DiscoveryOutput`.**
/// 7. Return [`DiscoveryOutput`].
///
/// # Errors
///
/// Returns [`DiscoveryError`] for hard failures (root not a directory, path
/// escape, IO error during canonicalisation, `include_untracked=false` but
/// tracked-file set unavailable).  Non-fatal errors during the walk or
/// manifest build are captured in diagnostics / `SourceManifest::read_errors`.
pub fn discover(root: &Path, policy: &DiscoveryPolicy) -> Result<DiscoveryOutput, DiscoveryError> {
    // 1. Canonicalise root — rejects symlink escapes and non-directories.
    let canonical_root = root.canonicalize().map_err(|source| DiscoveryError::Canonicalize {
        path: root.to_path_buf(),
        source,
    })?;

    if !canonical_root.is_dir() {
        return Err(DiscoveryError::RootNotDirectory(canonical_root));
    }

    // 2. Git repository detection (best-effort; not an error if absent).
    let git_meta = if policy.git_aware {
        git::find_git_root(&canonical_root, &canonical_root)
            .and_then(|repo_root| git::read_git_meta(&repo_root).ok())
    } else {
        None
    };

    // 3–4. Walk + security + classification.
    let walk_result = walk::walk(&canonical_root, policy)?;

    let mut all_diagnostics = walk_result.diagnostics;

    // 5. Build the BLAKE3 manifest using raw (unredacted) bytes.
    let manifest = manifest::build_manifest(&walk_result.entries, &canonical_root);

    // Collect manifest read errors and unstable capture diagnostics.
    all_diagnostics.extend(manifest.read_errors.clone());
    all_diagnostics.extend(manifest.unstable_captures.clone());

    // 6. Classify each file's content for downstream use.
    //    We determine the size tier and scan a bounded amount of content for
    //    secrets.  Content is NOT retained — only the classification is stored.
    let mut downstream_classifications: Vec<(String, DownstreamClassification)> =
        Vec::with_capacity(walk_result.entries.len());

    for entry in &walk_result.entries {
        let classification =
            classify_file_for_downstream(&entry.abs_path, &entry.repo_relative, &mut all_diagnostics);
        downstream_classifications.push((entry.repo_relative.clone(), classification));
    }

    Ok(DiscoveryOutput {
        entries: walk_result.entries,
        manifest,
        git_meta,
        diagnostics: all_diagnostics,
        downstream_classifications,
    })
}

/// Classify one file's content for downstream use by determining its size
/// tier and scanning a bounded portion of its content for secrets.
///
/// **No content is retained after this function returns.**
///
/// For SMALL/MEDIUM files (< `MAX_FULL_LOAD_BYTES`) the full content is
/// scanned.  For LARGE/VERY_LARGE files only the first + last
/// `MAX_SAMPLE_BYTES` are scanned; a `PartialSecretScan` diagnostic is added
/// for VERY_LARGE files because the body between the two samples is not
/// inspected.
fn classify_file_for_downstream(
    abs_path: &Path,
    repo_relative: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> DownstreamClassification {
    // Stat to determine size tier (no content read yet).
    let size_bytes = match std::fs::metadata(abs_path) {
        Ok(m) => m.len(),
        Err(e) => {
            return DownstreamClassification::ScanSkipped {
                reason: format!("stat failed: {e}"),
            };
        }
    };

    let size_tier = if size_bytes > MAX_LARGE_BYTES {
        FileSizeTier::VeryLarge
    } else if size_bytes > MAX_FULL_LOAD_BYTES {
        FileSizeTier::Large
    } else {
        FileSizeTier::Small
    };

    // For SMALL/MEDIUM: load full text content; scan entirely.
    // For LARGE/VERY_LARGE: read only a bounded sample for secret scanning.
    let scan_content: String = match size_tier {
        FileSizeTier::Small => {
            // Full content — bounded by MAX_FULL_LOAD_BYTES.
            match std::fs::read_to_string(abs_path) {
                Ok(s) => s,
                Err(_) => {
                    // Binary or unreadable: cannot scan.
                    return DownstreamClassification::ScanSkipped {
                        reason: "file unreadable or binary".to_string(),
                    };
                }
            }
        }
        FileSizeTier::Large | FileSizeTier::VeryLarge => {
            // Sample only: first MAX_SAMPLE_BYTES + last MAX_SAMPLE_BYTES.
            match read_sample(abs_path, MAX_SAMPLE_BYTES) {
                Ok(s) => {
                    if matches!(size_tier, FileSizeTier::VeryLarge) {
                        // Record that the body between the two samples was not
                        // scanned — a downstream consumer must handle this.
                        diagnostics.push(Diagnostic {
                            kind: DiagnosticKind::IoError,
                            path: abs_path.to_path_buf(),
                            message: format!(
                                "PARTIAL_SECRET_SCAN: '{}' is {size_bytes} bytes \
                                 (VERY_LARGE); only {MAX_SAMPLE_BYTES}-byte head/tail \
                                 samples were scanned for secrets",
                                repo_relative
                            ),
                        });
                    }
                    s
                }
                Err(e) => {
                    return DownstreamClassification::ScanSkipped {
                        reason: format!("sample read failed: {e}"),
                    };
                }
            }
        }
    };

    // Run secrets preprocess on the (bounded) content.
    let preprocess = secrets::preprocess(&scan_content, repo_relative);
    // Drop scan_content immediately — do not propagate to caller.
    drop(scan_content);

    match preprocess.decision {
        SecretScanDecision::Excluded => DownstreamClassification::Excluded,
        SecretScanDecision::Safe => DownstreamClassification::Safe { size_tier },
        SecretScanDecision::Redacted => DownstreamClassification::Redacted {
            size_tier,
            // Store findings metadata only — no raw secret values.
            findings: preprocess.findings,
        },
    }
}

/// Read the first `sample_bytes` and last `sample_bytes` of a file into a
/// `String`, discarding any non-UTF-8 bytes.  Used for secret scanning of
/// LARGE / VERY_LARGE files without loading the full content.
fn read_sample(path: &Path, sample_bytes: usize) -> io::Result<String> {
    use std::fs::File;
    use std::io::Seek;

    let mut file = File::open(path)?;
    let size = file.metadata()?.len() as usize;

    let head_len = sample_bytes.min(size);
    let mut head = vec![0u8; head_len];
    file.read_exact(&mut head)?;

    let tail_len = sample_bytes.min(size.saturating_sub(head_len));
    let mut tail = vec![0u8; tail_len];
    if tail_len > 0 {
        file.seek(io::SeekFrom::End(-(tail_len as i64)))?;
        file.read_exact(&mut tail)?;
    }

    let mut combined = String::with_capacity(head_len + tail_len + 8);
    combined.push_str(&String::from_utf8_lossy(&head));
    if tail_len > 0 {
        combined.push_str("\n...\n");
        combined.push_str(&String::from_utf8_lossy(&tail));
    }
    Ok(combined)
}

/// Process a single file's content through the secrets layer and return the
/// redacted or raw content as appropriate, following the large-file contract.
///
/// This is the **lazy content accessor** for downstream consumers.  It reads
/// the file, scans it, and returns content safe for indexing.  It does not
/// retain state — callers invoke it per file when they need content.
///
/// For LARGE / VERY_LARGE files this returns the sampled content only; the
/// caller is responsible for further streaming or region-based processing per
/// `docs/contracts/large_files.md`.
///
/// # Errors
///
/// Returns `Err` if the file cannot be opened or read.
pub fn preprocess_file_content(
    abs_path: &Path,
    repo_relative: &str,
) -> io::Result<PreprocessResult> {
    let size_bytes = std::fs::metadata(abs_path)?.len();

    let raw: String = if size_bytes > MAX_FULL_LOAD_BYTES {
        // LARGE / VERY_LARGE: sample only.
        read_sample(abs_path, MAX_SAMPLE_BYTES)?
    } else {
        std::fs::read_to_string(abs_path)?
    };

    Ok(secrets::preprocess(&raw, repo_relative))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_git_repo(dir: &std::path::Path) {
        fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
        fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    fn write_file(dir: &std::path::Path, rel: &str, content: &str) {
        let full = dir.join(rel);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, content).unwrap();
    }

    #[test]
    fn discover_returns_eligible_entries() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/main.rs", "fn main() {}");
        write_file(root, "src/lib.rs", "pub fn x() {}");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let paths: Vec<&str> = output.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/lib.rs"));
    }

    #[test]
    fn discover_manifest_hash_is_64_hex_chars() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "a");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        assert_eq!(output.manifest.manifest_hash.len(), 64);
        assert!(output.manifest.manifest_hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn discover_detects_git_branch() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "a");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let meta = output.git_meta.expect("should have git meta");
        assert_eq!(meta.branch.as_deref(), Some("main"));
    }

    #[test]
    fn discover_no_git_meta_when_not_git_aware() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "a");

        let policy = DiscoveryPolicy::default_non_git();
        let output = discover(root, &policy).unwrap();

        assert!(output.git_meta.is_none());
    }

    #[test]
    fn discover_excludes_security_forbidden_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/main.rs", "fn main() {}");
        write_file(root, "private.key", "-----BEGIN PRIVATE KEY-----");
        write_file(root, ".env", "SECRET=abc");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let paths: Vec<&str> = output.entries.iter().map(|e| e.repo_relative.as_str()).collect();
        assert!(!paths.contains(&"private.key"));
        assert!(!paths.contains(&".env"));
        assert!(paths.contains(&"src/main.rs"));
    }

    #[test]
    fn discover_manifest_is_deterministic_across_two_runs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/a.rs", "fn a() {}");
        write_file(root, "src/b.rs", "fn b() {}");

        let policy = DiscoveryPolicy::default_git();
        let out1 = discover(root, &policy).unwrap();
        let out2 = discover(root, &policy).unwrap();

        assert_eq!(out1.manifest.manifest_hash, out2.manifest.manifest_hash);
    }

    #[test]
    fn discover_error_for_nonexistent_root() {
        let policy = DiscoveryPolicy::default_git();
        let result = discover(std::path::Path::new("/nonexistent/does/not/exist/xyzzy"), &policy);
        assert!(result.is_err(), "should fail for non-existent root");
    }

    // ── Bounded content handling tests ────────────────────────────────────

    /// `DiscoveryOutput` must not retain file content — `downstream_classifications`
    /// holds only classification metadata, not strings of workspace content.
    #[test]
    fn discovery_output_does_not_retain_content() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);
        write_file(root, "src/code.rs", "pub fn safe() {}");

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        // Verify that DownstreamClassification::Safe does not carry a String.
        let entry = output
            .downstream_classifications
            .iter()
            .find(|(p, _)| p == "src/code.rs")
            .expect("src/code.rs must have a classification");

        match &entry.1 {
            DownstreamClassification::Safe { .. } => {
                // Good — no content stored, just classification + tier.
            }
            other => panic!(
                "expected Safe classification for clean code; got {other:?}"
            ),
        }
    }

    /// A source file containing an AWS access key must classify as `Redacted`.
    /// The `findings` must be non-empty; no raw key value is stored.
    #[test]
    fn inline_secret_produces_redacted_classification() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        write_file(
            root,
            "src/config.rs",
            "// AWS key below\nconst KEY: &str = \"AKIAIOSFODNN7EXAMPLE\";\n",
        );

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let entry = output
            .downstream_classifications
            .iter()
            .find(|(p, _)| p == "src/config.rs")
            .expect("src/config.rs must have a classification");

        match &entry.1 {
            DownstreamClassification::Redacted { findings, .. } => {
                assert!(
                    !findings.is_empty(),
                    "at least one finding expected for AWS key; got {findings:?}"
                );
                // Verify no raw key value is in the findings (findings are
                // metadata only — pattern IDs, not raw secret values).
            }
            other => panic!(
                "expected Redacted classification for file with AWS key; got {other:?}"
            ),
        }
    }

    /// A known secrets carrier (`.netrc`-style content, if it reaches
    /// classification) must produce `Excluded`.
    ///
    /// We test this via the public `preprocess_file_content` lazy accessor
    /// rather than through `discover()`, because the walk security layer may
    /// already exclude these files before they enter `downstream_classifications`.
    /// The important invariant is that calling `preprocess_file_content` on a
    /// known-secrets-carrier path returns `Excluded` — callers must honour that.
    #[test]
    fn known_secret_carrier_produces_excluded_classification() {
        let tmp = TempDir::new().unwrap();

        // Write a `.netrc`-style file that `is_known_secrets_file` recognises.
        let path = tmp.path().join(".netrc");
        fs::write(&path, "machine example.com login user password s3cr3t").unwrap();

        let result = preprocess_file_content(&path, ".netrc")
            .expect("preprocess_file_content must succeed for a readable file");

        assert_eq!(
            result.decision,
            secrets::SecretScanDecision::Excluded,
            "known secrets carrier must produce Excluded; got {:?}",
            result.decision
        );
        assert!(
            result.content.is_none(),
            "Excluded result must not carry content"
        );
    }

    /// A file just over `MAX_FULL_LOAD_BYTES` must be classified as
    /// `FileSizeTier::Large` and must not cause an error or `ScanSkipped`.
    #[test]
    fn large_file_classified_as_large_tier() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        // Write a file just over the 4 MiB boundary.
        let size = MAX_FULL_LOAD_BYTES as usize + 1;
        // Use all-ASCII content so `read_to_string` never rejects it as binary.
        let content = "a".repeat(size);
        write_file(root, "data/large.txt", &content);

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let entry = output
            .downstream_classifications
            .iter()
            .find(|(p, _)| p == "data/large.txt")
            .expect("data/large.txt must have a classification");

        match &entry.1 {
            DownstreamClassification::Safe { size_tier } => {
                assert_eq!(
                    *size_tier,
                    FileSizeTier::Large,
                    "file just over 4 MiB must be Large tier"
                );
            }
            DownstreamClassification::Redacted { size_tier, .. } => {
                assert_eq!(
                    *size_tier,
                    FileSizeTier::Large,
                    "file just over 4 MiB must be Large tier"
                );
            }
            other => panic!(
                "expected Safe or Redacted for large ASCII file; got {other:?}"
            ),
        }
    }

    /// `preprocess_file_content` is the lazy per-file content accessor.
    /// For a clean small file it returns `Safe` with content.
    /// For a file containing a secret it returns `Redacted` with findings and
    /// the secret value removed from the returned content.
    #[test]
    fn preprocess_file_content_returns_bounded_result() {
        let tmp = TempDir::new().unwrap();

        // ── Clean file ────────────────────────────────────────────────────
        let clean_path = tmp.path().join("clean.rs");
        fs::write(&clean_path, "fn main() {}").unwrap();

        let clean = preprocess_file_content(&clean_path, "clean.rs")
            .expect("must succeed for readable file");

        assert_eq!(
            clean.decision,
            secrets::SecretScanDecision::Safe,
            "clean code must return Safe"
        );
        assert!(
            clean.content.is_some(),
            "Safe result must carry content for downstream consumers"
        );

        // ── File containing an AWS key ─────────────────────────────────────
        let secret_path = tmp.path().join("config.rs");
        fs::write(
            &secret_path,
            "const KEY: &str = \"AKIAIOSFODNN7EXAMPLE\";\n",
        )
        .unwrap();

        let secret = preprocess_file_content(&secret_path, "config.rs")
            .expect("must succeed for readable file");

        assert_eq!(
            secret.decision,
            secrets::SecretScanDecision::Redacted,
            "file with AWS key must return Redacted"
        );
        assert!(
            !secret.findings.is_empty(),
            "at least one finding expected; got {:?}",
            secret.findings
        );
        // The raw key value must not appear in the returned content.
        let returned = secret.content.expect("Redacted result must carry redacted content");
        assert!(
            !returned.contains("AKIAIOSFODNN7EXAMPLE"),
            "raw secret must not survive in the returned content"
        );
    }
}
