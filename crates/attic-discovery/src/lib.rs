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
//!   → per-file secrets classification (Safe / Redacted / Excluded / PartialScan)
//!     (classification only — content is NOT retained in DiscoveryOutput)
//! ```
//!
//! # Downstream content contract
//!
//! `DiscoveryOutput` records a **classification** for each eligible file —
//! whether its content is safe, redacted, excluded, or only partially scanned —
//! but does **not** retain the file content in memory.  This ensures the
//! discovery pass remains bounded even for workspaces with 500 K+ files.
//!
//! Consumers that need actual file content call [`preprocess_file_content`]
//! per file, which reads, secret-scans, and returns the (possibly redacted)
//! content for that single file without accumulating workspace-wide state.
//!
//! Large-file handling follows `docs/contracts/large_files.md`:
//!
//! | Tier       | Size             | Secrets scan              | Content returned                |
//! |------------|------------------|---------------------------|---------------------------------|
//! | SMALL      | ≤ 4 MiB          | Full content scanned      | `content=Some(redacted)` (SMALL)|
//! | LARGE      | 4 MiB – 50 MiB   | Full streaming scan       | `stream=Some(LargeFileStream)`  |
//! | VERY_LARGE | > 50 MiB         | Head + tail sample only   | `content=Some(sample)`, PartialScan|
//!
//! LARGE files MUST be consumed through the returned [`secrets::LargeFileStream`].
//! Phase 1C MUST NOT reopen the raw file path — doing so would bypass the
//! redaction boundary.
//!
//! VERY_LARGE files are classified as [`DownstreamClassification::PartialScan`]
//! (never `Safe`) because the body between the two samples is not inspected.
//! Consumers **must not** treat `PartialScan` as equivalent to `Safe`.
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

pub use diagnostics::{Diagnostic, DiagnosticKind};
pub use error::DiscoveryError;
pub use git::GitRepoMeta;
pub use manifest::{ManifestEntry, SourceManifest};
pub use policy::DiscoveryPriority;
pub use policy::{DiscoveryPolicy, GlobRule, PriorityRule};
pub use secrets::{
    FileSizeTier, LargeFileStream, PreprocessResult, SMALL_FILE_THRESHOLD as MAX_FULL_LOAD_BYTES,
    ScanResult, SecretFinding, SecretScanDecision, VERY_LARGE_FILE_THRESHOLD as MAX_LARGE_BYTES,
};
pub use security::canonicalize_within_root;
pub use walk::{EligibleEntry, WalkResult};

// ---------------------------------------------------------------------------
// Large-file constants
// ---------------------------------------------------------------------------
//
// Size-tier thresholds are defined in `secrets` (the authoritative module)
// and re-exported above as `MAX_FULL_LOAD_BYTES` / `MAX_LARGE_BYTES`.
//
// The constants below are supplementary processing parameters used only by
// the VERY_LARGE sample reader and are NOT duplicated in `secrets`.

/// Bytes sampled from start and end of VERY_LARGE files for secret scanning
/// (matches `MAX_SAMPLE_BYTES` from the contract).
pub const MAX_SAMPLE_BYTES: usize = 8 * 1024; // 8 KiB

// ---------------------------------------------------------------------------
// Downstream content classification
// ---------------------------------------------------------------------------

/// The secrets-scan classification for one eligible file.
///
/// **Content is NOT stored here.**  `DiscoveryOutput` is bounded regardless
/// of workspace size.  Consumers that need file content call
/// [`preprocess_file_content`] per file.
#[derive(Debug, Clone)]
pub enum DownstreamClassification {
    /// No secrets found.  Content is safe for downstream indexing.
    ///
    /// Only produced when the **entire** file has been scanned:
    /// - SMALL: full text scan.
    /// - LARGE: complete streaming chunk scan via [`secrets::LargeFileStream`].
    ///
    /// VERY_LARGE files are **never** classified as `Safe`; they produce
    /// [`DownstreamClassification::PartialScan`] instead.
    Safe {
        /// Size tier at discovery time.
        size_tier: FileSizeTier,
    },
    /// Secrets were found; content must be redacted before indexing.
    /// `findings` lists pattern IDs and byte offsets only;
    /// no raw secret values are stored.
    Redacted {
        /// Size tier at discovery time.
        size_tier: FileSizeTier,
        /// Descriptions of findings (no raw secret values).
        findings: Vec<SecretFinding>,
    },
    /// The file is a known secrets carrier (e.g. `.netrc`, `id_rsa`) and must
    /// not be indexed at all.
    Excluded,
    /// **VERY_LARGE file**: only a head + tail sample was scanned.
    ///
    /// The body between the two samples was **not** inspected.  Downstream
    /// consumers **must not** treat this as equivalent to [`Self::Safe`].
    /// A `PARTIAL_SECRET_SCAN` diagnostic is recorded alongside this result.
    ///
    /// `findings` contains any secrets detected in the sample portion.
    PartialScan {
        /// Secrets detected in the sampled portion (no raw secret values).
        findings: Vec<SecretFinding>,
    },
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
/// (Safe / Redacted / Excluded / PartialScan / ScanSkipped) for each eligible
/// file, but does **not** retain file content.  Callers retrieve content
/// lazily via [`preprocess_file_content`].
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
/// 6. For each eligible file: determine size tier, scan content for secrets,
///    and record the [`DownstreamClassification`].
///    - SMALL (≤ 4 MiB): full content scan via [`secrets::preprocess`].
///    - LARGE (4 MiB – 50 MiB): bounded streaming scan via
///      [`secrets::stream_scan_large_file_classify`].  No full-file allocation.
///    - VERY_LARGE (> 50 MiB): head + tail sample only; classified as
///      `PartialScan`.  A `PARTIAL_SECRET_SCAN` diagnostic is recorded.
///      **File content is not retained in the returned `DiscoveryOutput`.**
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
    let canonical_root = root
        .canonicalize()
        .map_err(|source| DiscoveryError::Canonicalize {
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

    // 3-4. Walk + security + classification.
    let walk_result = walk::walk(&canonical_root, policy)?;

    let mut all_diagnostics = walk_result.diagnostics;

    // 5. Build the BLAKE3 manifest using raw (unredacted) bytes.
    let manifest = manifest::build_manifest(&walk_result.entries, &canonical_root);

    // Collect manifest read errors and unstable capture diagnostics.
    all_diagnostics.extend(manifest.read_errors.clone());
    all_diagnostics.extend(manifest.unstable_captures.clone());

    // 6. Classify each file's content for downstream use.
    //    SMALL: full scan via secrets::preprocess.
    //    LARGE: bounded streaming scan via secrets::stream_scan_large_file_classify.
    //    VERY_LARGE: head+tail sample → PartialScan classification.
    //    Content is NOT retained — only the classification is stored.
    let mut downstream_classifications: Vec<(String, DownstreamClassification)> =
        Vec::with_capacity(walk_result.entries.len());

    for entry in &walk_result.entries {
        let classification = classify_file_for_downstream(
            &entry.abs_path,
            &entry.repo_relative,
            &mut all_diagnostics,
        );
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
/// tier and applying the appropriate secret-scanning strategy.
///
/// **No content is retained after this function returns.**
///
/// - SMALL (≤ `MAX_FULL_LOAD_BYTES`): full content scanned via
///   [`secrets::preprocess`].
/// - LARGE (`MAX_FULL_LOAD_BYTES`..`MAX_LARGE_BYTES`): the entire file is
///   scanned via [`secrets::stream_scan_large_file_classify`] — the single
///   authoritative streaming scanner.  At most
///   `secrets::STREAM_CHUNK_SIZE + secrets::SAFETY_WINDOW_SIZE` bytes are
///   live at any one time.
/// - VERY_LARGE (> `MAX_LARGE_BYTES`): only the first + last `MAX_SAMPLE_BYTES`
///   are scanned; a `PARTIAL_SECRET_SCAN` diagnostic is recorded; the
///   classification is always [`DownstreamClassification::PartialScan`], never
///   `Safe`, because the mid-body was not inspected.
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

    let size_tier = secrets::classify_file_size(size_bytes);

    match size_tier {
        FileSizeTier::Small => {
            // Full content — bounded by MAX_FULL_LOAD_BYTES (4 MiB).
            let content = match std::fs::read_to_string(abs_path) {
                Ok(s) => s,
                Err(_) => {
                    return DownstreamClassification::ScanSkipped {
                        reason: "file unreadable or binary".to_string(),
                    };
                }
            };
            let preprocess = secrets::preprocess(&content, repo_relative);
            // Drop content immediately — do not propagate to caller.
            drop(content);
            match preprocess.decision {
                SecretScanDecision::Excluded => DownstreamClassification::Excluded,
                SecretScanDecision::Safe => DownstreamClassification::Safe { size_tier },
                SecretScanDecision::Redacted => DownstreamClassification::Redacted {
                    size_tier,
                    findings: preprocess.findings,
                },
                // PartialScan is never returned by `secrets::preprocess` (which
                // always has access to the full text it was given). Handle
                // defensively so the match is exhaustive.
                SecretScanDecision::PartialScan => DownstreamClassification::ScanSkipped {
                    reason: "unexpected PartialScan from full-content preprocess".to_string(),
                },
            }
        }

        FileSizeTier::Large => {
            // Delegate to the single authoritative streaming classifier in secrets.
            // This is the ONLY LARGE-file scanner; there is no duplicate in lib.rs.
            if secrets::is_known_secrets_file(repo_relative) {
                return DownstreamClassification::Excluded;
            }
            match secrets::stream_scan_large_file_classify(abs_path) {
                Ok((SecretScanDecision::Safe, _, _)) => DownstreamClassification::Safe {
                    size_tier: FileSizeTier::Large,
                },
                Ok((SecretScanDecision::Redacted, findings, _)) => {
                    DownstreamClassification::Redacted {
                        size_tier: FileSizeTier::Large,
                        findings,
                    }
                }
                Ok((SecretScanDecision::Excluded, _, _)) => DownstreamClassification::Excluded,
                Ok((SecretScanDecision::PartialScan, findings, _)) => {
                    DownstreamClassification::PartialScan { findings }
                }
                Err(e) => DownstreamClassification::ScanSkipped {
                    reason: format!("streaming scan failed: {e}"),
                },
            }
        }

        FileSizeTier::VeryLarge => {
            // Sample-only scan: head + tail.  The mid-body is NOT inspected.
            // Classification is ALWAYS PartialScan, never Safe.
            let sample = match read_sample(abs_path, MAX_SAMPLE_BYTES) {
                Ok(s) => s,
                Err(e) => {
                    return DownstreamClassification::ScanSkipped {
                        reason: format!("sample read failed: {e}"),
                    };
                }
            };

            // Record that mid-body was not scanned.
            diagnostics.push(Diagnostic {
                kind: DiagnosticKind::IoError,
                path: abs_path.to_path_buf(),
                message: format!(
                    "PARTIAL_SECRET_SCAN: '{}' is {size_bytes} bytes \
                     (VERY_LARGE); only {MAX_SAMPLE_BYTES}-byte head/tail \
                     samples were scanned for secrets; mid-body NOT inspected",
                    repo_relative
                ),
            });

            // Scan the sample for secrets.
            let result = secrets::scan_and_redact(&sample);
            drop(sample);

            // Regardless of whether findings were detected in the sample,
            // this is always PartialScan — never Safe.
            DownstreamClassification::PartialScan {
                findings: result.findings,
            }
        }
    }
}

/// Read the first `sample_bytes` and last `sample_bytes` of a file into a
/// `String`, discarding any non-UTF-8 bytes.  Used for secret scanning of
/// VERY_LARGE files without loading the full content.
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
/// result appropriate for its size tier, following the large-file contract.
///
/// This is the **lazy content accessor** for downstream consumers (Phase 1C).
///
/// | Tier       | `decision`  | `content`      | `stream`              |
/// |------------|-------------|----------------|-----------------------|
/// | SMALL      | Safe/Redacted/Excluded | `Some(redacted)` | `None` |
/// | LARGE      | Safe/Redacted/Excluded | `None`         | `Some(LargeFileStream)` |
/// | VERY_LARGE | PartialScan | `Some(sample)` | `None`                |
///
/// **Phase 1C MUST consume LARGE file content exclusively through the `stream`
/// field.**  It must NOT reopen the original file path — that would bypass
/// the redaction boundary.
///
/// # Errors
///
/// Returns `Err` if the file cannot be opened or stat'd.
pub fn preprocess_file_content(
    abs_path: &Path,
    repo_relative: &str,
) -> io::Result<PreprocessResult> {
    let size_bytes = std::fs::metadata(abs_path)?.len();
    let size_tier = secrets::classify_file_size(size_bytes);

    match size_tier {
        FileSizeTier::VeryLarge => {
            // VERY_LARGE: sample-only scan, always PartialScan decision.
            let sample = read_sample(abs_path, MAX_SAMPLE_BYTES)?;
            let scan = secrets::scan_and_redact(&sample);
            Ok(PreprocessResult {
                decision: SecretScanDecision::PartialScan,
                content: Some(scan.redacted),
                stream: None,
                findings: scan.findings,
            })
        }

        FileSizeTier::Large => {
            // LARGE: delegate entirely to the safe secrets API.
            // preprocess_large_file() runs the authoritative streaming
            // classifier and returns stream=Some(LargeFileStream).
            // Phase 1C MUST consume content through that stream.
            secrets::preprocess_large_file(abs_path, repo_relative)
        }

        FileSizeTier::Small => {
            // SMALL: full content, full scan.
            let raw = std::fs::read_to_string(abs_path)?;
            Ok(secrets::preprocess(&raw, repo_relative))
        }
    }
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

        let paths: Vec<&str> = output
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
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
        assert!(
            output
                .manifest
                .manifest_hash
                .chars()
                .all(|c| c.is_ascii_hexdigit())
        );
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

        let paths: Vec<&str> = output
            .entries
            .iter()
            .map(|e| e.repo_relative.as_str())
            .collect();
        assert!(!paths.contains(&"private.key"));
        assert!(!paths.contains(&".env"));
        assert!(paths.contains(&"src/main.rs"));
    }

    // =========================================================================
    // Integration tests: preprocess_file_content public API
    // =========================================================================

    /// (1) LARGE file returns stream=Some, content=None through the public API.
    #[test]
    fn preprocess_file_content_large_returns_stream_some() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // Write a file slightly above SMALL_FILE_THRESHOLD (4 MiB).
        let size = MAX_FULL_LOAD_BYTES as usize + 1024;
        let abs_path = root.join("big.rs");
        fs::write(&abs_path, "x".repeat(size)).unwrap();

        let result = preprocess_file_content(&abs_path, "big.rs").unwrap();

        assert!(
            result.stream.is_some(),
            "LARGE file must return stream=Some; got stream=None"
        );
        assert!(
            result.content.is_none(),
            "LARGE file must return content=None; got content=Some"
        );
    }

    /// (2) Consuming the returned LargeFileStream never exposes the raw secret.
    #[test]
    fn preprocess_file_content_large_stream_never_exposes_raw_secret() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        let pad = "x".repeat(MAX_FULL_LOAD_BYTES as usize + 200);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let content = format!("{pad} {secret} end");
        let abs_path = root.join("secrets_config.rs");
        fs::write(&abs_path, &content).unwrap();

        let result = preprocess_file_content(&abs_path, "secrets_config.rs").unwrap();

        assert_eq!(
            result.decision,
            SecretScanDecision::Redacted,
            "must be classified Redacted"
        );
        assert!(result.stream.is_some(), "LARGE must return a stream");
        assert!(result.content.is_none(), "LARGE must NOT return content");

        // Consume the stream and verify raw secret is absent.
        let mut stream = result.stream.unwrap();
        let scan_result = secrets::collect_all(&mut stream).unwrap();
        let full_redacted = scan_result.redacted;

        assert!(
            !full_redacted.contains(secret),
            "raw secret must NEVER appear in the streamed output; \
             got near end: {:?}",
            &full_redacted[full_redacted.len().saturating_sub(100)..]
        );
        assert!(
            full_redacted.contains("AKIA***"),
            "redacted placeholder must appear in streamed output"
        );
    }

    /// (3) Boundary-spanning secrets are redacted when consumed through the
    ///     public preprocess_file_content API.
    #[test]
    fn preprocess_file_content_large_boundary_spanning_secret_redacted() {
        use secrets::{SAFETY_WINDOW_SIZE, STREAM_CHUNK_SIZE};

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Place the AWS key so it straddles the first STREAM_CHUNK_SIZE boundary.
        // Put the file in LARGE territory (> 4 MiB) by making the pad longer
        // than MAX_FULL_LOAD_BYTES.
        let boundary = MAX_FULL_LOAD_BYTES as usize + STREAM_CHUNK_SIZE;
        let before_boundary = boundary - 10; // key starts 10 bytes before boundary
        let secret = "AKIAIOSFODNN7EXAMPLE"; // 20 bytes; 10 in first chunk, 10 in next
        let filler_after = "y".repeat(500);
        let content = format!("{} {secret} {filler_after}", "x".repeat(before_boundary));
        let abs_path = root.join("boundary.rs");
        fs::write(&abs_path, &content).unwrap();

        let result = preprocess_file_content(&abs_path, "boundary.rs").unwrap();

        assert!(result.stream.is_some(), "LARGE must return a stream");

        let mut stream = result.stream.unwrap();
        let scan_result = secrets::collect_all(&mut stream).unwrap();
        let full_redacted = scan_result.redacted;

        assert!(
            !full_redacted.contains(secret),
            "boundary-spanning secret must be redacted through public API"
        );
        assert!(
            full_redacted.contains("AKIA***"),
            "redacted replacement must appear for boundary-spanning secret"
        );

        let _ = SAFETY_WINDOW_SIZE; // suppress unused warning
    }

    /// (4) Finding offsets are correct through the public preprocess_file_content API.
    #[test]
    fn preprocess_file_content_large_finding_offsets_correct() {
        use secrets::STREAM_CHUNK_SIZE;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // Place a secret at a known file position within a LARGE file.
        // pad_len chosen so the file is in LARGE territory (> 4 MiB).
        let pad_len = MAX_FULL_LOAD_BYTES as usize + 100;
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let content = format!("{} {secret} end", "x".repeat(pad_len));
        // Expected file byte offset of the secret = pad_len + 1 (the space)
        let expected_offset = pad_len + 1;

        let abs_path = root.join("offset_check.rs");
        fs::write(&abs_path, &content).unwrap();

        let result = preprocess_file_content(&abs_path, "offset_check.rs").unwrap();

        assert_eq!(
            result.decision,
            SecretScanDecision::Redacted,
            "must detect secret"
        );

        let aws: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.pattern_id == "AWS-001")
            .collect();
        assert!(!aws.is_empty(), "must return AWS-001 findings");
        assert_eq!(
            aws[0].offset, expected_offset,
            "finding offset through preprocess_file_content must equal actual \
             file byte position; expected {expected_offset}, got {}",
            aws[0].offset
        );

        let _ = STREAM_CHUNK_SIZE; // suppress unused warning
    }

    /// (5) discover() and preprocess_file_content() agree on size tier for the
    ///     same file — both classify it as the same DownstreamClassification
    ///     variant (both Redacted when a secret is present, both Safe when clean).
    #[test]
    fn discovery_and_preprocess_agree_on_size_tier() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        // Write one SMALL file (safe) and one LARGE file (with a secret).
        write_file(root, "src/small_safe.rs", "fn main() {}");

        let large_content = format!(
            "{} AKIAIOSFODNN7EXAMPLE end",
            "x".repeat(MAX_FULL_LOAD_BYTES as usize + 100)
        );
        write_file(root, "src/large_secret.rs", &large_content);

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        // Verify discover() produced the expected classifications.
        let small_cls = output
            .downstream_classifications
            .iter()
            .find(|(p, _)| p == "src/small_safe.rs")
            .map(|(_, c)| c);
        let large_cls = output
            .downstream_classifications
            .iter()
            .find(|(p, _)| p == "src/large_secret.rs")
            .map(|(_, c)| c);

        assert!(
            matches!(small_cls, Some(DownstreamClassification::Safe { .. })),
            "discover() must classify small_safe.rs as Safe; got: {small_cls:?}"
        );
        assert!(
            matches!(large_cls, Some(DownstreamClassification::Redacted { .. })),
            "discover() must classify large_secret.rs as Redacted; got: {large_cls:?}"
        );

        // Verify preprocess_file_content() agrees.
        let small_abs = root.join("src/small_safe.rs");
        let large_abs = root.join("src/large_secret.rs");

        let small_prep = preprocess_file_content(&small_abs, "src/small_safe.rs").unwrap();
        let large_prep = preprocess_file_content(&large_abs, "src/large_secret.rs").unwrap();

        assert_eq!(
            small_prep.decision,
            SecretScanDecision::Safe,
            "preprocess_file_content must also classify small_safe.rs as Safe"
        );
        assert_eq!(
            large_prep.decision,
            SecretScanDecision::Redacted,
            "preprocess_file_content must also classify large_secret.rs as Redacted"
        );
        assert!(
            large_prep.stream.is_some(),
            "preprocess_file_content must return stream=Some for LARGE"
        );
        assert!(
            large_prep.content.is_none(),
            "preprocess_file_content must return content=None for LARGE"
        );
    }

    /// (6) Files at exact threshold boundary values go to the expected tier.
    ///     Verifies that MAX_FULL_LOAD_BYTES and MAX_LARGE_BYTES are used
    ///     consistently by both the secrets tier classifier and the public API.
    #[test]
    fn threshold_boundary_values_consistent() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();

        // File of exactly MAX_FULL_LOAD_BYTES should be SMALL.
        let small_exact = root.join("small_exact");
        fs::write(&small_exact, "x".repeat(MAX_FULL_LOAD_BYTES as usize)).unwrap();

        // File of MAX_FULL_LOAD_BYTES + 1 should be LARGE.
        let large_min = root.join("large_min");
        fs::write(&large_min, "x".repeat(MAX_FULL_LOAD_BYTES as usize + 1)).unwrap();

        // File of exactly MAX_LARGE_BYTES should be LARGE.
        let large_exact = root.join("large_exact");
        fs::write(&large_exact, "x".repeat(MAX_LARGE_BYTES as usize)).unwrap();

        // File of MAX_LARGE_BYTES + 1 should be VERY_LARGE.
        let very_large_min = root.join("very_large_min");
        fs::write(&very_large_min, "x".repeat(MAX_LARGE_BYTES as usize + 1)).unwrap();

        // Check via secrets::classify_file_size (the authoritative classifier).
        assert_eq!(
            secrets::classify_file_size(MAX_FULL_LOAD_BYTES),
            FileSizeTier::Small,
            "exactly MAX_FULL_LOAD_BYTES must be Small"
        );
        assert_eq!(
            secrets::classify_file_size(MAX_FULL_LOAD_BYTES + 1),
            FileSizeTier::Large,
            "MAX_FULL_LOAD_BYTES+1 must be Large"
        );
        assert_eq!(
            secrets::classify_file_size(MAX_LARGE_BYTES),
            FileSizeTier::Large,
            "exactly MAX_LARGE_BYTES must be Large"
        );
        assert_eq!(
            secrets::classify_file_size(MAX_LARGE_BYTES + 1),
            FileSizeTier::VeryLarge,
            "MAX_LARGE_BYTES+1 must be VeryLarge"
        );

        // Check that preprocess_file_content agrees with classify_file_size
        // at each boundary: SMALL returns content=Some, stream=None;
        // LARGE returns content=None, stream=Some;
        // VERY_LARGE returns content=Some (sample), stream=None, PartialScan.
        let small_prep = preprocess_file_content(&small_exact, "small_exact").unwrap();
        assert!(
            small_prep.content.is_some() && small_prep.stream.is_none(),
            "exact-boundary SMALL must have content=Some, stream=None"
        );

        let large_min_prep = preprocess_file_content(&large_min, "large_min").unwrap();
        assert!(
            large_min_prep.content.is_none() && large_min_prep.stream.is_some(),
            "LARGE (just over boundary) must have content=None, stream=Some"
        );

        let large_exact_prep = preprocess_file_content(&large_exact, "large_exact").unwrap();
        assert!(
            large_exact_prep.content.is_none() && large_exact_prep.stream.is_some(),
            "exact MAX_LARGE_BYTES must have content=None, stream=Some"
        );

        let very_large_prep = preprocess_file_content(&very_large_min, "very_large_min").unwrap();
        assert_eq!(
            very_large_prep.decision,
            SecretScanDecision::PartialScan,
            "VERY_LARGE must produce PartialScan decision"
        );
        assert!(
            very_large_prep.content.is_some() && very_large_prep.stream.is_none(),
            "VERY_LARGE must have content=Some (sample), stream=None"
        );
    }

    /// (7) Compile-time proof that there is no duplicate LARGE scanner: the
    ///     crate re-exports `FileSizeTier`, `LargeFileStream`,
    ///     `MAX_FULL_LOAD_BYTES`, and `MAX_LARGE_BYTES` from `secrets`.
    ///     If a duplicate type existed in lib.rs this test would fail to
    ///     compile (type mismatch).
    #[test]
    fn no_duplicate_large_scanner_path_reexport_roundtrip() {
        // FileSizeTier re-exported from secrets must be the same type as
        // secrets::FileSizeTier — verified by assignment without cast.
        let tier: FileSizeTier = secrets::classify_file_size(0);
        let _: secrets::FileSizeTier = tier; // would not compile if types differed

        // Threshold re-exports must alias the authoritative constants.
        assert_eq!(
            MAX_FULL_LOAD_BYTES,
            secrets::SMALL_FILE_THRESHOLD,
            "MAX_FULL_LOAD_BYTES must equal secrets::SMALL_FILE_THRESHOLD"
        );
        assert_eq!(
            MAX_LARGE_BYTES,
            secrets::VERY_LARGE_FILE_THRESHOLD,
            "MAX_LARGE_BYTES must equal secrets::VERY_LARGE_FILE_THRESHOLD"
        );

        // LargeFileStream must be the same type as secrets::LargeFileStream.
        // Open a temp file to construct one and assign it.
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("probe");
        fs::write(&f, "x").unwrap();
        let stream: LargeFileStream = secrets::LargeFileStream::open(&f).unwrap();
        let _: secrets::LargeFileStream = stream; // type identity check
    }
}
