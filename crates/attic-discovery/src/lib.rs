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
//! | Tier | Size | Secrets scan | Content returned |
//! |------|------|--------------|-----------------|
//! | SMALL / MEDIUM | < 4 MiB | Full content scanned | Present (redacted if needed) |
//! | LARGE | 4 MiB – 50 MiB | **Full streaming scan** (bounded chunks) | `None` — caller streams separately |
//! | VERY_LARGE | > 50 MiB | Head + tail sample only | Sample (partial) |
//!
//! VERY_LARGE files are classified as [`DownstreamClassification::PartialScan`]
//! (never `Safe`) because the body between the head and tail samples is not
//! inspected.  Consumers **must not** treat `PartialScan` as equivalent to
//! `Safe`.
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

/// Bytes sampled from start and end of VERY_LARGE files for secret scanning
/// (matches `MAX_SAMPLE_BYTES` from the contract).
pub const MAX_SAMPLE_BYTES: usize = 8 * 1024; // 8 KiB

/// Chunk size for the bounded streaming secret scan of LARGE files.
///
/// Each chunk is read into memory independently; only two chunks (current +
/// overlap tail) are live at once, so peak allocation is
/// `LARGE_SCAN_CHUNK_BYTES + CHUNK_OVERLAP_BYTES`, not the full file size.
pub const LARGE_SCAN_CHUNK_BYTES: usize = 128 * 1024; // 128 KiB

/// Overlap between consecutive scan windows to catch secrets that span a
/// chunk boundary.  Must be >= the length of the longest V1 secret pattern.
/// 256 bytes comfortably covers all V1 patterns.
const CHUNK_OVERLAP_BYTES: usize = 256;

// ---------------------------------------------------------------------------
// Downstream content classification
// ---------------------------------------------------------------------------

/// Size tier assigned at discovery time, per `docs/contracts/large_files.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSizeTier {
    /// < 4 MiB — full content loaded and scanned.
    Small,
    /// 4 MiB to 50 MiB — full streaming secret scan; content streamed on demand
    /// by later pipeline stages (not buffered during discovery).
    Large,
    /// Above 50 MiB — only a head + tail sample is scanned; a
    /// `PartialSecretScan` diagnostic is recorded.  Classified as
    /// [`DownstreamClassification::PartialScan`], never `Safe`.
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
    ///
    /// Only produced when the **entire** file has been scanned:
    /// - SMALL/MEDIUM: full text scan.
    /// - LARGE: complete streaming chunk scan.
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
///    - SMALL/MEDIUM: full content scan.
///    - LARGE: bounded streaming chunk scan (no full-file allocation).
///    - VERY_LARGE: head + tail sample only; classified as `PartialScan`.
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

    // 3-4. Walk + security + classification.
    let walk_result = walk::walk(&canonical_root, policy)?;

    let mut all_diagnostics = walk_result.diagnostics;

    // 5. Build the BLAKE3 manifest using raw (unredacted) bytes.
    let manifest = manifest::build_manifest(&walk_result.entries, &canonical_root);

    // Collect manifest read errors and unstable capture diagnostics.
    all_diagnostics.extend(manifest.read_errors.clone());
    all_diagnostics.extend(manifest.unstable_captures.clone());

    // 6. Classify each file's content for downstream use.
    //    SMALL/MEDIUM: full scan.
    //    LARGE: bounded streaming scan (no full-file allocation).
    //    VERY_LARGE: head+tail sample → PartialScan classification.
    //    Content is NOT retained — only the classification is stored.
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
/// tier and applying the appropriate secret-scanning strategy.
///
/// **No content is retained after this function returns.**
///
/// - SMALL/MEDIUM (< `MAX_FULL_LOAD_BYTES`): full content scanned.
/// - LARGE (`MAX_FULL_LOAD_BYTES`..=`MAX_LARGE_BYTES`): the entire file is
///   scanned in bounded `LARGE_SCAN_CHUNK_BYTES` chunks; at most two chunks
///   are live in memory simultaneously.
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

    let size_tier = if size_bytes > MAX_LARGE_BYTES {
        FileSizeTier::VeryLarge
    } else if size_bytes > MAX_FULL_LOAD_BYTES {
        FileSizeTier::Large
    } else {
        FileSizeTier::Small
    };

    match size_tier {
        FileSizeTier::Small => {
            // Full content — bounded by MAX_FULL_LOAD_BYTES.
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
            // Streaming chunk scan — the ENTIRE file is scanned but only
            // LARGE_SCAN_CHUNK_BYTES + CHUNK_OVERLAP_BYTES bytes are live
            // at any one time.  No full-file allocation.
            stream_scan_large_file_classify(abs_path, repo_relative)
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

/// Scan a LARGE file for secrets using a bounded streaming approach.
///
/// The file is read in `LARGE_SCAN_CHUNK_BYTES` chunks.  To catch secrets
/// that span a chunk boundary, the last `CHUNK_OVERLAP_BYTES` bytes of the
/// previous chunk are prepended to each new chunk before scanning.
///
/// At most `LARGE_SCAN_CHUNK_BYTES + CHUNK_OVERLAP_BYTES` bytes are live at
/// any one time — the full file is **never** loaded into memory.
///
/// Returns `Safe { Large }` if no secrets are found across the entire file,
/// `Redacted { Large, findings }` if secrets were detected, or `ScanSkipped`
/// if the file cannot be opened or contains non-UTF-8 content.
fn stream_scan_large_file_classify(abs_path: &Path, repo_relative: &str) -> DownstreamClassification {
    use std::fs::File;
    use std::io::BufReader;

    // Check path-based exclusion first (e.g. `.env`, `*.pem`).
    if secrets::is_known_secrets_file(repo_relative) {
        return DownstreamClassification::Excluded;
    }

    let file = match File::open(abs_path) {
        Ok(f) => f,
        Err(e) => {
            return DownstreamClassification::ScanSkipped {
                reason: format!("open failed: {e}"),
            };
        }
    };

    let mut reader = BufReader::new(file);
    let mut all_findings: Vec<SecretFinding> = Vec::new();
    // Overlap tail carried forward from the previous window.
    let mut overlap: Vec<u8> = Vec::new();
    // Cumulative byte offset of the start of the *overlap* window within
    // the file — used to adjust finding offsets to be file-relative.
    let mut window_file_offset: usize = 0;

    let mut raw_chunk = vec![0u8; LARGE_SCAN_CHUNK_BYTES];

    loop {
        let n = match reader.read(&mut raw_chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                return DownstreamClassification::ScanSkipped {
                    reason: format!("read failed: {e}"),
                };
            }
        };

        // Build the scan window: overlap from previous chunk + new bytes.
        let mut window_bytes = Vec::with_capacity(overlap.len() + n);
        window_bytes.extend_from_slice(&overlap);
        window_bytes.extend_from_slice(&raw_chunk[..n]);

        // Convert to str for the scanner (lossy — replace invalid UTF-8 with
        // the replacement character so we never miss an ASCII secret).
        let window_str = String::from_utf8_lossy(&window_bytes).into_owned();

        let scan_result = secrets::scan_and_redact(&window_str);

        // Adjust finding offsets to be file-relative and collect.
        for mut finding in scan_result.findings {
            finding.offset += window_file_offset;
            all_findings.push(finding);
        }

        // Prepare overlap for the next iteration:
        // keep the last CHUNK_OVERLAP_BYTES of the *new* bytes only (not the
        // full window) so the overlap window doesn't grow unboundedly.
        let overlap_start = n.saturating_sub(CHUNK_OVERLAP_BYTES);
        overlap = raw_chunk[overlap_start..n].to_vec();
        // Advance the file offset by the new bytes consumed this iteration.
        window_file_offset += n;
    }

    if all_findings.is_empty() {
        DownstreamClassification::Safe {
            size_tier: FileSizeTier::Large,
        }
    } else {
        DownstreamClassification::Redacted {
            size_tier: FileSizeTier::Large,
            findings: all_findings,
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
/// This is the **lazy content accessor** for downstream consumers.  It reads
/// the file, scans it for secrets, and returns a [`PreprocessResult`] whose
/// `content` field is:
///
/// - **SMALL/MEDIUM** (< `MAX_FULL_LOAD_BYTES`): the full (possibly redacted)
///   content as a `String`.  Decision is `Safe`, `Redacted`, or `Excluded`.
/// - **LARGE** (`MAX_FULL_LOAD_BYTES`..=`MAX_LARGE_BYTES`): `content` is
///   `None`.  The entire file was scanned via the streaming chunked scanner
///   but is **not buffered** here — callers must stream the file themselves
///   for content delivery.  Decision is `Safe`, `Redacted`, or `Excluded`.
/// - **VERY_LARGE** (> `MAX_LARGE_BYTES`): `content` contains the head + tail
///   sample only (not the full file).  Decision is always `PartialScan` —
///   never `Safe` — because the mid-body was not inspected.
///
/// # Errors
///
/// Returns `Err` if the file cannot be opened or stat'd.
pub fn preprocess_file_content(
    abs_path: &Path,
    repo_relative: &str,
) -> io::Result<PreprocessResult> {
    let size_bytes = std::fs::metadata(abs_path)?.len();

    if size_bytes > MAX_LARGE_BYTES {
        // VERY_LARGE: sample-only scan, PartialScan decision.
        let sample = read_sample(abs_path, MAX_SAMPLE_BYTES)?;
        let scan = secrets::scan_and_redact(&sample);
        // Return the sample as content so callers can at least display it;
        // but the decision is unambiguously PartialScan.
        return Ok(PreprocessResult {
            decision: SecretScanDecision::PartialScan,
            content: Some(scan.redacted),
            findings: scan.findings,
        });
    }

    if size_bytes > MAX_FULL_LOAD_BYTES {
        // LARGE: streaming scan of entire file; content NOT buffered.
        let classification = stream_scan_large_file_classify(abs_path, repo_relative);
        let decision = match &classification {
            DownstreamClassification::Safe { .. } => SecretScanDecision::Safe,
            DownstreamClassification::Redacted { .. } => SecretScanDecision::Redacted,
            DownstreamClassification::Excluded => SecretScanDecision::Excluded,
            DownstreamClassification::PartialScan { .. } => SecretScanDecision::PartialScan,
            DownstreamClassification::ScanSkipped { reason } => {
                return Err(io::Error::other(reason.clone()));
            }
        };
        let findings = match classification {
            DownstreamClassification::Redacted { findings, .. } => findings,
            DownstreamClassification::PartialScan { findings } => findings,
            _ => Vec::new(),
        };
        // content is None — caller must stream the file for actual content.
        return Ok(PreprocessResult {
            decision,
            content: None,
            findings,
        });
    }

    // SMALL/MEDIUM: full content, full scan.
    let raw = std::fs::read_to_string(abs_path)?;
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
            }
            other => panic!(
                "expected Redacted classification for file with AWS key; got {other:?}"
            ),
        }
    }

    /// A known secrets carrier (`.netrc`-style content) must produce `Excluded`.
    #[test]
    fn known_secret_carrier_produces_excluded_classification() {
        let tmp = TempDir::new().unwrap();

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

    /// `preprocess_file_content` for a clean small file returns `Safe` with content.
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
        let returned = secret.content.expect("Redacted result must carry redacted content");
        assert!(
            !returned.contains("AKIAIOSFODNN7EXAMPLE"),
            "raw secret must not survive in the returned content"
        );
    }

    // ── New focused tests for streaming LARGE scan and PartialScan ────────

    /// A LARGE file whose AWS secret exists **only in the middle** must be
    /// detected by the streaming scanner and classified as `Redacted`.
    ///
    /// This verifies that the chunked scan covers the full file, not just
    /// head+tail samples.
    #[test]
    fn large_file_secret_in_middle_is_detected_by_streaming_scan() {
        let tmp = TempDir::new().unwrap();

        // Build a file that is just over MAX_FULL_LOAD_BYTES.
        // Place the secret in the exact middle so head+tail sampling would miss it.
        let half = MAX_FULL_LOAD_BYTES as usize / 2;
        // "a" * half  +  AWS key  +  "b" * half  → total > 4 MiB
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let mut content = String::with_capacity(half * 2 + secret.len() + 10);
        content.push_str(&"a".repeat(half));
        content.push(' ');
        content.push_str(secret);
        content.push(' ');
        content.push_str(&"b".repeat(half));

        let path = tmp.path().join("large_with_middle_secret.txt");
        fs::write(&path, &content).unwrap();

        // Confirm the file is actually LARGE tier.
        assert!(
            content.len() as u64 > MAX_FULL_LOAD_BYTES,
            "test setup: file must exceed MAX_FULL_LOAD_BYTES"
        );
        assert!(
            content.len() as u64 <= MAX_LARGE_BYTES,
            "test setup: file must not exceed MAX_LARGE_BYTES"
        );

        let result = preprocess_file_content(&path, "large_with_middle_secret.txt")
            .expect("preprocess_file_content must not fail");

        assert_eq!(
            result.decision,
            SecretScanDecision::Redacted,
            "streaming scan must detect AWS key in middle of LARGE file; got {:?}",
            result.decision
        );
        assert!(
            !result.findings.is_empty(),
            "must have at least one finding for the mid-file AWS key"
        );
        // content must be None for LARGE files (not buffered).
        assert!(
            result.content.is_none(),
            "LARGE file preprocess must return content=None (not buffered)"
        );
    }

    /// A clean LARGE file (no secrets anywhere) must be classified `Safe`
    /// with `content=None` (not buffered).  This proves that the streaming
    /// scanner completes without allocating the full file.
    #[test]
    fn large_clean_file_classifies_safe_with_no_content() {
        let tmp = TempDir::new().unwrap();

        let size = MAX_FULL_LOAD_BYTES as usize + 512;
        // All lowercase letters — no secret pattern matches.
        let content = "x".repeat(size);
        let path = tmp.path().join("large_clean.txt");
        fs::write(&path, &content).unwrap();

        let result = preprocess_file_content(&path, "large_clean.txt")
            .expect("preprocess_file_content must not fail for clean large file");

        assert_eq!(
            result.decision,
            SecretScanDecision::Safe,
            "clean LARGE file must be Safe; got {:?}",
            result.decision
        );
        assert!(
            result.content.is_none(),
            "LARGE file preprocess must return content=None (not buffered); got Some(...)"
        );
        assert!(result.findings.is_empty(), "clean file must have no findings");
    }

    /// A clean VERY_LARGE file must be classified as `PartialScan`, never `Safe`.
    /// The content field must be `Some(sample)` (the head+tail portion).
    #[test]
    fn very_large_clean_file_classified_as_partial_scan_not_safe() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        // Write a file just over MAX_LARGE_BYTES using sparse writing.
        // We only need the file to be large on-disk; content can be minimal.
        // Use a file with clean head + tail but we don't control the middle —
        // the point is that PartialScan must always be returned.
        let path = root.join("data").join("very_large_clean.bin");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            use std::io::{Seek, Write};
            let mut f = fs::File::create(&path).unwrap();
            // Write 1 byte at offset MAX_LARGE_BYTES + 1 to create a sparse file
            // of the required size without allocating 50 MiB of RAM.
            let target = MAX_LARGE_BYTES + 1;
            f.seek(io::SeekFrom::Start(target)).unwrap();
            f.write_all(b"z").unwrap();
        }

        // Via discover() the classification must be PartialScan.
        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        let entry = output
            .downstream_classifications
            .iter()
            .find(|(p, _)| p == "data/very_large_clean.bin");

        // The file may be excluded by the walk policy (binary / gitignore).
        // If it is present, it must be PartialScan.
        if let Some((_, classification)) = entry {
            match classification {
                DownstreamClassification::PartialScan { .. } => {
                    // Correct — VERY_LARGE → always PartialScan.
                }
                DownstreamClassification::ScanSkipped { .. } => {
                    // Acceptable if the file is binary/unreadable.
                }
                other => panic!(
                    "VERY_LARGE file must be PartialScan or ScanSkipped; got {other:?}"
                ),
            }
        }

        // Also test via preprocess_file_content directly.
        let result = preprocess_file_content(&path, "data/very_large_clean.bin")
            .expect("preprocess_file_content must not fail");

        assert_eq!(
            result.decision,
            SecretScanDecision::PartialScan,
            "VERY_LARGE clean file must return PartialScan, not Safe; got {:?}",
            result.decision
        );
        assert!(
            result.content.is_some(),
            "VERY_LARGE preprocess must return content=Some(sample)"
        );
    }

    /// A VERY_LARGE file whose sample contains a secret must be classified
    /// as `PartialScan` with non-empty findings — not `Safe`.
    #[test]
    fn very_large_file_with_secret_in_sample_classifies_partial_scan_with_findings() {
        let tmp = TempDir::new().unwrap();

        // Write a file > MAX_LARGE_BYTES.  Place an AWS key at the very start
        // so it falls within the head sample.
        let path = tmp.path().join("very_large_secret.txt");
        {
            use std::io::{Seek, Write};
            let mut f = fs::File::create(&path).unwrap();
            // Head: AWS key followed by padding to fill head sample.
            let head_secret = b"AKIAIOSFODNN7EXAMPLE padding_padding_padding_padding_pad\n";
            f.write_all(head_secret).unwrap();
            // Seek past MAX_LARGE_BYTES to make it VERY_LARGE.
            let target = MAX_LARGE_BYTES + 1;
            f.seek(io::SeekFrom::Start(target)).unwrap();
            f.write_all(b"end").unwrap();
        }

        let result = preprocess_file_content(&path, "very_large_secret.txt")
            .expect("preprocess_file_content must not fail");

        assert_eq!(
            result.decision,
            SecretScanDecision::PartialScan,
            "VERY_LARGE file must always return PartialScan; got {:?}",
            result.decision
        );
        assert!(
            !result.findings.is_empty(),
            "secret in VERY_LARGE head sample must produce findings"
        );
    }

    /// The PARTIAL_SECRET_SCAN diagnostic must be emitted for every VERY_LARGE
    /// file encountered during discovery.
    #[test]
    fn very_large_file_emits_partial_secret_scan_diagnostic() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        setup_git_repo(root);

        let path = root.join("data").join("huge.bin");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            use std::io::{Seek, Write};
            let mut f = fs::File::create(&path).unwrap();
            let target = MAX_LARGE_BYTES + 1;
            f.seek(io::SeekFrom::Start(target)).unwrap();
            f.write_all(b"z").unwrap();
        }

        let policy = DiscoveryPolicy::default_git();
        let output = discover(root, &policy).unwrap();

        // If the file made it through the walk, there must be a
        // PARTIAL_SECRET_SCAN diagnostic for it.
        let was_classified = output
            .downstream_classifications
            .iter()
            .any(|(p, _)| p == "data/huge.bin");

        if was_classified {
            let has_partial_diag = output.diagnostics.iter().any(|d| {
                d.message.contains("PARTIAL_SECRET_SCAN")
                    && d.path.to_string_lossy().contains("huge.bin")
            });
            assert!(
                has_partial_diag,
                "a PARTIAL_SECRET_SCAN diagnostic must be emitted for every VERY_LARGE file; \
                 diagnostics: {:#?}",
                output.diagnostics
            );
        }
        // If the walk filtered the file out, the test passes trivially —
        // the invariant only applies to files that enter classification.
    }

    /// No raw detected secret value may be returned through the `findings`
    /// field of any `DownstreamClassification` or `PreprocessResult`.
    /// Findings carry only metadata (pattern_id, offset, length).
    #[test]
    fn no_raw_secret_value_in_findings() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("secret.rs");
        let raw_key = "AKIAIOSFODNN7EXAMPLE";
        fs::write(&path, format!("const K: &str = \"{raw_key}\";\n")).unwrap();

        let result = preprocess_file_content(&path, "secret.rs")
            .expect("preprocess_file_content must not fail");

        for finding in &result.findings {
            // SecretFinding has: pattern_id (&'static str), offset (usize),
            // length (usize). None of these fields carry raw secret bytes.
            // Verify the pattern_id is a known label, not raw secret material.
            assert!(
                !finding.pattern_id.contains(raw_key),
                "pattern_id must not contain raw secret: {:?}",
                finding.pattern_id
            );
        }
        // The content (if present) must not contain the raw key.
        if let Some(content) = &result.content {
            assert!(
                !content.contains(raw_key),
                "returned content must not contain the raw secret value"
            );
        }
    }
}
