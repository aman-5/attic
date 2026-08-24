//! Secret preprocessing: detect and redact sensitive material before any
//! content reaches FTS, embeddings, or logs.
//!
//! # V1 patterns (per `secrets` contract)
//!
//! | ID       | Pattern                          | Redact |
//! |----------|----------------------------------|--------|
//! | PK-001   | PEM private-key block            | full   |
//! | AWS-001  | AWS access-key ID                | partial|
//! | GH-001   | GitHub personal-access token     | partial|
//! | JWT-001  | JSON Web Token                   | partial|
//! | HE-001   | High-entropy base64 string >= 20 | partial|
//!
//! "Full" redaction replaces the entire matched region with a redaction
//! placeholder.  "Partial" redaction replaces all but the first 4 characters
//! with `***`.
//!
//! **Nothing that matches a secret pattern may be persisted to storage,
//! passed to FTS, or written to any log at level < ERROR.**
//!
//! # File-size tiers and safe streaming
//!
//! Thresholds follow `docs/contracts/large_files.md` — these are the single
//! source of truth; `lib.rs` re-exports them and must not define its own.
//!
//! | Tier       | Threshold          | Content delivery                         |
//! |------------|--------------------|------------------------------------------|
//! | SMALL      | <= 4 MiB           | Full in-memory redact, `content` field   |
//! | LARGE      | 4 MiB – 50 MiB     | [`LargeFileStream`] bounded streaming    |
//! | VERY_LARGE | > 50 MiB           | Sample-only classification (PartialScan) |
//!
//! Phase 1C **must** obtain LARGE-file content exclusively through
//! [`LargeFileStream`].  It must never reopen the raw file and consume raw
//! bytes directly — that would bypass the redaction boundary and allow
//! classified-Redacted content to reach indexers.

use std::io::{self, Read};

// ---------------------------------------------------------------------------
// Size-tier constants  (authoritative — from docs/contracts/large_files.md)
// ---------------------------------------------------------------------------

/// Files <= this size are processed entirely in memory (SMALL tier).
///
/// Matches `MAX_FULL_LOAD_BYTES` in the large-file contract (4 MiB).
/// `lib.rs` re-exports this constant and must not define a conflicting value.
pub const SMALL_FILE_THRESHOLD: u64 = 4 * 1024 * 1024; // 4 MiB

/// Files > this size are treated as VERY_LARGE (sample-only scan).
///
/// Matches `MAX_LARGE_BYTES` in the large-file contract (50 MiB).
/// `lib.rs` re-exports this constant and must not define a conflicting value.
pub const VERY_LARGE_FILE_THRESHOLD: u64 = 50 * 1024 * 1024; // 50 MiB

/// Chunk size used when streaming LARGE files.
pub const STREAM_CHUNK_SIZE: usize = 64 * 1024; // 64 KiB

/// Overlap carried between successive stream windows.
///
/// Must be >= the longest possible single secret token so that a secret
/// spanning a chunk boundary is fully captured in one window.
/// 4 KiB comfortably exceeds the longest PEM private-key block.
pub const STREAM_OVERLAP_SIZE: usize = 4 * 1024; // 4 KiB

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// File-size classification tier.
///
/// This is the **single authoritative** size-tier type for the entire
/// `attic-discovery` crate.  `lib.rs` re-exports it; there must be no
/// second definition.  Threshold values follow `docs/contracts/large_files.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSizeTier {
    /// <= `SMALL_FILE_THRESHOLD` (4 MiB).  Full in-memory processing.
    Small,
    /// > `SMALL_FILE_THRESHOLD` (4 MiB) and <= `VERY_LARGE_FILE_THRESHOLD` (50 MiB).
    /// Content is delivered through [`LargeFileStream`].
    Large,
    /// > `VERY_LARGE_FILE_THRESHOLD` (50 MiB).
    /// Only a head+tail sample is scanned; mid-body is not inspected.
    VeryLarge,
}

/// Classify a file by its byte size into a processing tier.
pub fn classify_file_size(size_bytes: u64) -> FileSizeTier {
    if size_bytes <= SMALL_FILE_THRESHOLD {
        FileSizeTier::Small
    } else if size_bytes <= VERY_LARGE_FILE_THRESHOLD {
        FileSizeTier::Large
    } else {
        FileSizeTier::VeryLarge
    }
}

/// The result of scanning one piece of text for secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    /// The text with secrets redacted.  If no secrets were found this is
    /// identical to the original.
    pub redacted: String,
    /// Descriptions of each match found (pattern ID + zero-indexed byte
    /// offset of the original match start).  Never contains actual secret
    /// material.
    pub findings: Vec<SecretFinding>,
}

/// One detected secret occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    /// Pattern identifier (e.g. "PK-001").
    pub pattern_id: &'static str,
    /// Zero-based byte offset of the match start in the **original** text
    /// (file-relative for streaming; buffer-relative for in-memory).
    pub offset: usize,
    /// Length (in bytes) of the matched region in the original text.
    pub length: usize,
}

/// Whether the file should be excluded from all downstream processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScanDecision {
    /// Content is safe to index (after redaction if needed).
    ///
    /// Only produced when the **entire** file content has been scanned.
    /// LARGE-tier streaming scans and SMALL-tier full scans may produce this.
    /// VERY_LARGE sample-only scans produce PartialScan instead.
    Safe,
    /// Content contains secrets; redacted version is available.
    Redacted,
    /// File must not be indexed at all; it is a known secrets carrier.
    Excluded,
    /// Only a sample of the file was scanned (VERY_LARGE tier).
    ///
    /// The sampled portion was clean, but the mid-body was NOT inspected.
    /// Downstream consumers MUST NOT treat this as equivalent to Safe.
    PartialScan,
}

/// One redacted chunk emitted by LargeFileStream.
#[derive(Debug, Clone)]
pub struct StreamChunk {
    /// Pre-redacted text content for this chunk.
    /// Raw secret bytes are never present in this field.
    pub redacted: String,
    /// File-relative findings (offsets are relative to the start of the file,
    /// not the chunk).  Only new findings not reported in previous chunks.
    pub findings: Vec<SecretFinding>,
}

/// Full output of preprocessing one file's content.
///
/// # Content delivery by tier
///
/// | decision    | content      | stream        | Meaning                          |
/// |-------------|--------------|---------------|----------------------------------|
/// | Excluded    | None         | None          | Do not index                     |
/// | PartialScan | None         | None          | VERY_LARGE, sample only          |
/// | Safe        | Some(text)   | None          | SMALL, full scan, no secrets     |
/// | Redacted    | Some(text)   | None          | SMALL, full scan, redacted       |
/// | Safe        | None         | Some(stream)  | LARGE, full scan, no secrets     |
/// | Redacted    | None         | Some(stream)  | LARGE, full scan, secrets found  |
///
/// Phase 1C MUST consume LARGE file content exclusively through the `stream`
/// field.  It must NOT reopen the original file path.
#[derive(Debug)]
pub struct PreprocessResult {
    pub decision: SecretScanDecision,
    /// Redacted content -- present for SMALL-tier Safe and Redacted decisions.
    pub content: Option<String>,
    /// Safe streaming handle for LARGE-tier files.
    ///
    /// Phase 1C MUST use this (not a direct file open) to read content.
    /// Each chunk yielded by the stream has already been redacted.
    pub stream: Option<LargeFileStream>,
    pub findings: Vec<SecretFinding>,
}

// ---------------------------------------------------------------------------
// Public API -- small files
// ---------------------------------------------------------------------------

/// Scan and redact secrets in `text`.
///
/// Returns a ScanResult containing the redacted text and a list of findings.
/// The redacted text is safe to pass to FTS / embeddings.
pub fn scan_and_redact(text: &str) -> ScanResult {
    struct PendingMatch<'d> {
        start: usize,
        length: usize,
        detector: &'d Detector,
    }

    let mut pending: Vec<PendingMatch<'_>> = Vec::new();
    for detector in DETECTORS {
        for m in detector.find_all(text) {
            pending.push(PendingMatch {
                start: m.start,
                length: m.length,
                detector,
            });
        }
    }

    // Sort by start offset; ties broken by longest match first (greedy).
    pending.sort_by(|a, b| a.start.cmp(&b.start).then(b.length.cmp(&a.length)));

    // De-overlap: skip any match that starts inside the previous match's span.
    let mut non_overlapping: Vec<PendingMatch<'_>> = Vec::new();
    let mut next_free = 0usize;
    for pm in pending {
        if pm.start >= next_free {
            next_free = pm.start + pm.length;
            non_overlapping.push(pm);
        }
    }

    // Apply replacements left-to-right, tracking the running byte-shift.
    let mut findings: Vec<SecretFinding> = Vec::new();
    let mut redacted = text.to_string();
    let mut offset_delta: isize = 0;

    for pm in non_overlapping {
        let adj_start = (pm.start as isize + offset_delta) as usize;
        let adj_end = adj_start + pm.length;

        let replacement = match pm.detector.redact_mode {
            RedactMode::Full => pm.detector.placeholder.to_string(),
            RedactMode::Partial => partial_redact(&redacted[adj_start..adj_end]),
        };

        let old_len = pm.length;
        let new_len = replacement.len();

        redacted.replace_range(adj_start..adj_end, &replacement);
        offset_delta += new_len as isize - old_len as isize;

        findings.push(SecretFinding {
            pattern_id: pm.detector.id,
            offset: pm.start,
            length: pm.length,
        });
    }

    ScanResult { redacted, findings }
}

/// Preprocess SMALL file content: decide whether to index, exclude, or redact.
///
/// `repo_relative` is used solely to make path-based decisions (e.g. `.env`
/// file extension); it is never stored or logged with secret content.
///
/// This function is for SMALL files only (content already in memory).
/// For LARGE files use preprocess_large_file.
pub fn preprocess(content: &str, repo_relative: &str) -> PreprocessResult {
    if is_known_secrets_file(repo_relative) {
        return PreprocessResult {
            decision: SecretScanDecision::Excluded,
            content: None,
            stream: None,
            findings: Vec::new(),
        };
    }

    let result = scan_and_redact(content);
    if result.findings.is_empty() {
        PreprocessResult {
            decision: SecretScanDecision::Safe,
            content: Some(result.redacted),
            stream: None,
            findings: Vec::new(),
        }
    } else {
        PreprocessResult {
            decision: SecretScanDecision::Redacted,
            content: Some(result.redacted),
            stream: None,
            findings: result.findings,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API -- LARGE file safe streaming
// ---------------------------------------------------------------------------

/// Preprocess a LARGE file via path-based classification then a streaming pass.
///
/// This performs two things:
/// 1. Checks if the path is a known secrets file (returns Excluded).
/// 2. Opens the file and does a full streaming scan to determine the decision
///    (Safe vs Redacted), collecting all findings up-front.
/// 3. Returns a PreprocessResult with a fresh LargeFileStream positioned at
///    the start, ready for Phase 1C to consume pre-redacted chunks.
///
/// Phase 1C MUST consume content through result.stream, never by reopening
/// the raw file path.
pub fn preprocess_large_file(
    path: &std::path::Path,
    repo_relative: &str,
) -> io::Result<PreprocessResult> {
    if is_known_secrets_file(repo_relative) {
        return Ok(PreprocessResult {
            decision: SecretScanDecision::Excluded,
            content: None,
            stream: None,
            findings: Vec::new(),
        });
    }

    // Classification pass: stream the entire file to determine Safe vs Redacted
    // and collect all findings with correct file-relative offsets.
    let (decision, all_findings) = stream_scan_large_file_classify(path)?;

    // Open a fresh stream positioned at start for downstream consumption.
    // Each chunk yielded will be pre-redacted; raw bytes never escape.
    let stream = LargeFileStream::open(path)?;

    Ok(PreprocessResult {
        decision,
        content: None, // LARGE files always deliver via stream, never content
        stream: Some(stream),
        findings: all_findings,
    })
}

/// Scan the entire LARGE file for secrets, returning the decision and all
/// file-relative findings.
///
/// This is an internal classification pass only -- it does NOT produce
/// content for downstream consumption.  Use LargeFileStream for that.
///
/// Memory use is bounded to approximately STREAM_CHUNK_SIZE + STREAM_OVERLAP_SIZE.
pub fn stream_scan_large_file_classify(
    path: &std::path::Path,
) -> io::Result<(SecretScanDecision, Vec<SecretFinding>)> {
    let mut file = std::fs::File::open(path)?;
    let mut all_findings: Vec<SecretFinding> = Vec::new();

    // overlap_buf: bytes from the tail of the previous window, prepended to
    // the next window so secrets spanning chunk boundaries are captured.
    let mut overlap_buf: Vec<u8> = Vec::new();
    // file_offset_of_new_bytes: the file position of the first *new* byte in
    // the current window (i.e., not counting the overlap prefix).
    let mut file_offset_of_new_bytes: usize = 0;

    loop {
        let overlap_len = overlap_buf.len();

        // Build window = [overlap | new_chunk]
        let mut window = overlap_buf.clone();
        let mut new_chunk = vec![0u8; STREAM_CHUNK_SIZE];
        let n = read_exact_up_to(&mut file, &mut new_chunk)?;

        if n == 0 {
            // EOF: no more new bytes to process.
            break;
        }

        window.extend_from_slice(&new_chunk[..n]);

        // Scan the window for secrets.
        let window_str = String::from_utf8_lossy(&window).into_owned();
        let scan = scan_and_redact(&window_str);

        // The window's first byte is at file offset:
        //   file_offset_of_new_bytes - overlap_len
        let window_file_base = file_offset_of_new_bytes.saturating_sub(overlap_len);

        // Collect findings that start in the NEW bytes region only.
        // Findings starting in the overlap zone were already reported in the
        // previous iteration; reporting them again would create duplicates.
        //
        // Exception: a finding that STARTS in the overlap but ENDS in the
        // new bytes (i.e. spans the boundary) should also be emitted here,
        // because the previous iteration could not have seen its full extent.
        // We detect this by checking: if the finding's end > overlap_len
        // (in window-relative terms) AND we haven't emitted a finding at
        // this exact file offset before.
        for f in &scan.findings {
            let abs_offset = window_file_base + f.offset;
            // Emit if:
            //   (a) entirely within new bytes: abs_offset >= file_offset_of_new_bytes, OR
            //   (b) spans the boundary: f.offset < overlap_len but f.offset + f.length > overlap_len
            let in_new_region = abs_offset >= file_offset_of_new_bytes;
            let spans_boundary = f.offset < overlap_len && f.offset + f.length > overlap_len;
            if in_new_region || spans_boundary {
                // Dedup: don't add if we already have a finding at this exact file offset.
                let already_reported = all_findings.iter().any(|existing| {
                    existing.pattern_id == f.pattern_id && existing.offset == abs_offset
                });
                if !already_reported {
                    all_findings.push(SecretFinding {
                        pattern_id: f.pattern_id,
                        offset: abs_offset,
                        length: f.length,
                    });
                }
            }
        }

        // Advance the file-offset pointer by n (the new bytes consumed).
        file_offset_of_new_bytes += n;

        // Prepare overlap for next iteration: take the last STREAM_OVERLAP_SIZE
        // bytes of the *new* bytes (not of the full window) as overlap.
        let new_bytes_slice = &new_chunk[..n];
        if n > STREAM_OVERLAP_SIZE {
            overlap_buf = new_bytes_slice[n - STREAM_OVERLAP_SIZE..].to_vec();
        } else {
            overlap_buf = new_bytes_slice.to_vec();
        }

        if n < STREAM_CHUNK_SIZE {
            // Short read means EOF reached.
            break;
        }
    }

    let decision = if all_findings.is_empty() {
        SecretScanDecision::Safe
    } else {
        SecretScanDecision::Redacted
    };

    Ok((decision, all_findings))
}

/// A bounded streaming handle that yields pre-redacted chunks of a LARGE file.
///
/// # Safety contract
///
/// - Each chunk yielded has already been scanned and redacted.
/// - Raw secret bytes are NEVER yielded to the caller.
/// - Memory use is bounded to approximately STREAM_CHUNK_SIZE + STREAM_OVERLAP_SIZE
///   at any point in time; the full file is never loaded.
/// - All file-relative SecretFinding offsets reported by the stream are
///   correct (they account for overlap prepended to each window).
///
/// Phase 1C MUST consume LARGE file content exclusively through this type.
/// It must NOT open the original file path and read raw bytes.
pub struct LargeFileStream {
    /// Opened file handle -- positioned at start.
    file: std::fs::File,
    /// Bytes carried over from the end of the previous window (overlap prefix
    /// for the next window).  These bytes have already been emitted to the
    /// caller in a previous chunk; they are re-scanned only to catch secrets
    /// that span the boundary.
    overlap_buf: Vec<u8>,
    /// File byte offset of the first new byte NOT yet emitted to the caller.
    /// Starts at 0; advances by n (new bytes per window) each iteration.
    emit_from: usize,
    /// Whether the stream has been exhausted.
    done: bool,
}

impl std::fmt::Debug for LargeFileStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LargeFileStream")
            .field("emit_from", &self.emit_from)
            .field("done", &self.done)
            .finish()
    }
}

impl LargeFileStream {
    /// Open `path` for streaming.  Does NOT read any content yet.
    pub fn open(path: &std::path::Path) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        Ok(LargeFileStream {
            file,
            overlap_buf: Vec::new(),
            emit_from: 0,
            done: false,
        })
    }

    /// Yield the next pre-redacted chunk.
    ///
    /// Returns:
    /// - `Some(Ok(chunk))` -- next chunk.  chunk.redacted contains safe text.
    ///   chunk.findings has file-relative offsets for new findings only.
    /// - `Some(Err(e))` -- IO error.
    /// - `None` -- stream exhausted.
    ///
    /// # Offset and deduplication invariants
    ///
    /// Window = [overlap_buf | new_bytes].
    /// overlap_buf starts at file offset `emit_from - overlap_len`.
    /// A finding at window position `pos` has file offset `window_file_base + pos`
    /// where `window_file_base = emit_from - overlap_len`.
    ///
    /// Findings are emitted only for the new-bytes zone (abs_offset >= emit_from)
    /// to prevent duplicates.  Boundary-spanning findings (starting in overlap,
    /// ending in new bytes) are also emitted (once only, here).
    ///
    /// The emitted redacted text covers only the new bytes.  The overlap
    /// bytes that prefix the window have already been emitted by the previous
    /// call and are NOT re-emitted.
    pub fn next_chunk(&mut self) -> Option<io::Result<StreamChunk>> {
        if self.done {
            return None;
        }

        let overlap_len = self.overlap_buf.len();

        // Build window = [overlap | new_bytes]
        let mut window = self.overlap_buf.clone();
        let mut new_bytes_buf = vec![0u8; STREAM_CHUNK_SIZE];
        let n = match read_exact_up_to(&mut self.file, &mut new_bytes_buf) {
            Ok(n) => n,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };

        if n == 0 {
            self.done = true;
            return None;
        }

        window.extend_from_slice(&new_bytes_buf[..n]);

        if n < STREAM_CHUNK_SIZE {
            self.done = true;
        }

        // Scan and redact the full window.
        let window_str = String::from_utf8_lossy(&window).into_owned();
        let scan = scan_and_redact(&window_str);

        // File offset of window's first byte.
        let window_file_base = self.emit_from.saturating_sub(overlap_len);

        // Collect findings for the new region only (deduplicate vs overlap).
        let mut file_findings: Vec<SecretFinding> = Vec::new();
        for f in &scan.findings {
            let abs_offset = window_file_base + f.offset;
            let in_new_region = abs_offset >= self.emit_from;
            let spans_boundary =
                f.offset < overlap_len && f.offset + f.length > overlap_len;
            if in_new_region || spans_boundary {
                file_findings.push(SecretFinding {
                    pattern_id: f.pattern_id,
                    offset: abs_offset,
                    length: f.length,
                });
            }
        }

        // Determine where in the redacted string the new bytes begin.
        // The overlap portion (first `overlap_len` original bytes) has already
        // been emitted; we need to find the corresponding position in the
        // redacted string after accounting for any length-changing redactions
        // that occurred within the overlap.
        let redacted_new_start = compute_redacted_offset(&scan.findings, &window_str, overlap_len);

        // Emit only the new-bytes portion of the redacted string.
        let emit_str = if redacted_new_start <= scan.redacted.len() {
            scan.redacted[redacted_new_start..].to_string()
        } else {
            String::new()
        };

        // Prepare overlap for the next window: last STREAM_OVERLAP_SIZE bytes
        // of the *new* bytes (not the full window).
        let new_bytes_slice = &new_bytes_buf[..n];
        if n > STREAM_OVERLAP_SIZE {
            self.overlap_buf = new_bytes_slice[n - STREAM_OVERLAP_SIZE..].to_vec();
        } else {
            self.overlap_buf = new_bytes_slice.to_vec();
        }

        // Advance emit_from by the number of new bytes consumed this iteration.
        self.emit_from += n;

        Some(Ok(StreamChunk {
            redacted: emit_str,
            findings: file_findings,
        }))
    }

    /// Collect all chunks into a single redacted string and all findings.
    ///
    /// Convenience method for testing.  NOT suitable for production use on
    /// large files as it materialises the full content in memory.
    #[cfg(test)]
    pub fn collect_all(mut self) -> io::Result<(String, Vec<SecretFinding>)> {
        let mut full = String::new();
        let mut all_findings = Vec::new();
        while let Some(result) = self.next_chunk() {
            let chunk = result?;
            full.push_str(&chunk.redacted);
            all_findings.extend(chunk.findings);
        }
        Ok((full, all_findings))
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read up to `buf.len()` bytes from `reader`.  Returns the number of bytes
/// actually read (may be less than buf.len() at EOF).
fn read_exact_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

/// Compute the position in the *redacted* string that corresponds to
/// original byte offset `split_at` in the pre-redaction window string.
///
/// This is needed to determine where the overlap ends in the redacted
/// output so we can slice off only the new-bytes portion.
///
/// `findings` must be the findings produced by `scan_and_redact(&window_str)`,
/// sorted by start offset (which scan_and_redact guarantees).
fn compute_redacted_offset(
    findings: &[SecretFinding],
    window_str: &str,
    split_at: usize,
) -> usize {
    // Walk through findings sorted by start offset.
    // Accumulate the length delta introduced by redactions that fall entirely
    // before `split_at`.  For a finding that *straddles* split_at (starts in
    // the overlap zone, ends in the new-bytes zone) we return the position in
    // the redacted string where that finding's replacement *starts*, so that
    // the replacement is included in the emitted chunk rather than silently
    // swallowed by the overlap slice.
    let mut delta: isize = 0;
    for f in findings {
        if f.offset >= split_at {
            break; // sorted; nothing after split_at affects the split point
        }
        if f.offset + f.length <= split_at {
            // Finding is entirely within the overlap zone: accumulate delta.
            let detector = DETECTORS.iter().find(|d| d.id == f.pattern_id);
            if let Some(det) = detector {
                let end = (f.offset + f.length).min(window_str.len());
                let original_slice = &window_str[f.offset..end];
                let replacement_len = match det.redact_mode {
                    RedactMode::Full => det.placeholder.len(),
                    RedactMode::Partial => partial_redact(original_slice).len(),
                };
                delta += replacement_len as isize - f.length as isize;
            }
        } else {
            // Finding straddles split_at: its replacement starts at
            // (f.offset + delta) in the redacted string.  The replacement
            // covers content that spans the overlap/new-bytes boundary, so
            // the emitted chunk must start at the beginning of this
            // replacement to include it -- not at split_at+delta which would
            // skip the replacement entirely.
            return (f.offset as isize + delta).max(0) as usize;
        }
    }
    (split_at as isize + delta).max(0) as usize
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Returns `true` for files that should never be indexed regardless of their
/// content (mirrors the security-exclusion list in `security.rs`).
pub fn is_known_secrets_file(repo_relative: &str) -> bool {
    let name = repo_relative
        .rsplit('/')
        .next()
        .unwrap_or(repo_relative)
        .to_ascii_lowercase();

    // Exact names
    if matches!(
        name.as_str(),
        ".env" | ".netrc" | ".npmrc" | "id_rsa" | "id_ed25519" | "id_ecdsa"
    ) {
        return true;
    }

    // Prefix patterns
    if name.starts_with(".env.") {
        return true;
    }

    // Extension patterns
    if let Some(ext) = name.rsplit('.').next() {
        if matches!(ext, "pem" | "key" | "p12" | "jks" | "pfx" | "p8") {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Internal pattern engine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum RedactMode {
    Full,
    Partial,
}

struct RawMatch {
    start: usize,
    length: usize,
}

struct Detector {
    id: &'static str,
    redact_mode: RedactMode,
    placeholder: &'static str,
    find_all: fn(&str) -> Vec<RawMatch>,
}

impl Detector {
    fn find_all(&self, text: &str) -> Vec<RawMatch> {
        (self.find_all)(text)
    }
}

/// Replace all but the first 4 bytes with `***`.
fn partial_redact(s: &str) -> String {
    let mut result = String::with_capacity(8);
    let chars: Vec<char> = s.chars().collect();
    let keep = chars.len().min(4);
    result.extend(&chars[..keep]);
    result.push_str("***");
    result
}

// ---------------------------------------------------------------------------
// Detectors (V1)
// ---------------------------------------------------------------------------

static DETECTORS: &[Detector] = &[
    Detector {
        id: "PK-001",
        redact_mode: RedactMode::Full,
        placeholder: "[REDACTED:PRIVATE-KEY]",
        find_all: find_pem_private_keys,
    },
    Detector {
        id: "AWS-001",
        redact_mode: RedactMode::Partial,
        placeholder: "",
        find_all: find_aws_access_keys,
    },
    Detector {
        id: "GH-001",
        redact_mode: RedactMode::Partial,
        placeholder: "",
        find_all: find_github_tokens,
    },
    Detector {
        id: "JWT-001",
        redact_mode: RedactMode::Partial,
        placeholder: "",
        find_all: find_jwt_tokens,
    },
    Detector {
        id: "HE-001",
        redact_mode: RedactMode::Partial,
        placeholder: "",
        find_all: find_high_entropy_base64,
    },
];

// -- PK-001: PEM private key blocks ------------------------------------------

const PEM_BEGIN: &str = "-----BEGIN";
const PEM_END: &str = "-----END";

fn find_pem_private_keys(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let mut search_from = 0;

    while search_from < text.len() {
        match text[search_from..].find(PEM_BEGIN) {
            None => break,
            Some(rel) => {
                let begin_pos = search_from + rel;
                let header_end = match text[begin_pos..].find('\n') {
                    Some(n) => begin_pos + n,
                    None => break,
                };
                let header = &text[begin_pos..header_end];
                if !header.contains("PRIVATE KEY") {
                    search_from = begin_pos + PEM_BEGIN.len();
                    continue;
                }
                match text[header_end..].find(PEM_END) {
                    None => break,
                    Some(rel_end) => {
                        let end_block_start = header_end + rel_end;
                        let end_line_end = text[end_block_start..]
                            .find('\n')
                            .map(|n| end_block_start + n + 1)
                            .unwrap_or(text.len());
                        matches.push(RawMatch {
                            start: begin_pos,
                            length: end_line_end - begin_pos,
                        });
                        search_from = end_line_end;
                    }
                }
            }
        }
    }
    matches
}

// -- AWS-001: AWS access key IDs (AKIA... 20 chars) --------------------------

fn find_aws_access_keys(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let bytes = text.as_bytes();
    let prefix = b"AKIA";

    let mut i = 0;
    while i + 20 <= bytes.len() {
        if bytes[i..i + 4] == *prefix {
            if bytes[i + 4..i + 20]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                let after_ok = i + 20 >= bytes.len() || !bytes[i + 20].is_ascii_alphanumeric();
                if before_ok && after_ok {
                    matches.push(RawMatch { start: i, length: 20 });
                }
            }
        }
        i += 1;
    }
    matches
}

// -- GH-001: GitHub personal access tokens -----------------------------------

fn find_github_tokens(text: &str) -> Vec<RawMatch> {
    let prefixes: &[&str] = &["ghp_", "ghs_", "gho_", "ghu_", "ghr_", "github_pat_"];
    let mut matches = Vec::new();

    for prefix in prefixes {
        let mut search_from = 0;
        while let Some(rel) = text[search_from..].find(prefix) {
            let start = search_from + rel;
            let rest = &text[start + prefix.len()..];
            let token_extra: usize = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .map(|c| c.len_utf8())
                .sum();
            let total = prefix.len() + token_extra;
            if total >= 20 {
                matches.push(RawMatch { start, length: total });
            }
            search_from = start + prefix.len().max(1);
        }
    }
    matches
}

// -- JWT-001: JSON Web Tokens -------------------------------------------------

fn find_jwt_tokens(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if is_base64url_char(bytes[i]) {
            let seg1_end = advance_base64url(bytes, i);
            if seg1_end < len && bytes[seg1_end] == b'.' {
                let seg2_start = seg1_end + 1;
                let seg2_end = advance_base64url(bytes, seg2_start);
                if seg2_end < len && bytes[seg2_end] == b'.' {
                    let seg3_start = seg2_end + 1;
                    let seg3_end = advance_base64url(bytes, seg3_start);
                    let total = seg3_end - i;
                    if (seg1_end > i)
                        && (seg2_end > seg2_start)
                        && (seg3_end > seg3_start)
                        && total >= 40
                    {
                        matches.push(RawMatch { start: i, length: total });
                        i = seg3_end;
                        continue;
                    }
                }
                i = seg2_end;
            } else {
                i = seg1_end;
            }
        } else {
            i += 1;
        }
    }
    matches
}

fn is_base64url_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'='
}

fn advance_base64url(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() && is_base64url_char(bytes[i]) {
        i += 1;
    }
    i
}

// -- HE-001: High-entropy base64 strings (>= 20 chars) ----------------------

fn find_high_entropy_base64(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        if is_base64_char(bytes[i]) {
            let start = i;
            while i < len && is_base64_char(bytes[i]) {
                i += 1;
            }
            let candidate = &bytes[start..i];
            if candidate.len() >= 20 && looks_high_entropy(candidate) {
                matches.push(RawMatch { start, length: candidate.len() });
            }
        } else {
            i += 1;
        }
    }
    matches
}

fn is_base64_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

fn looks_high_entropy(bytes: &[u8]) -> bool {
    let has_upper = bytes.iter().any(|b| b.is_ascii_uppercase());
    let has_lower = bytes.iter().any(|b| b.is_ascii_lowercase());
    let has_digit = bytes.iter().any(|b| b.is_ascii_digit());
    has_upper && has_lower && has_digit
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── Helpers ─────────────────────────────────────────────────────────────

    /// Write content to a temp file and return the file (keeps it alive).
    fn write_temp(content: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    /// Build a string of `n` bytes of safe filler (no secret patterns).
    fn filler(n: usize) -> String {
        // Use repeating "x" — no uppercase, no digit mix => not high-entropy base64
        "x".repeat(n)
    }

    // ── is_known_secrets_file ────────────────────────────────────────────────

    #[test]
    fn secrets_file_pem() {
        assert!(is_known_secrets_file("certs/server.pem"));
        assert!(is_known_secrets_file("server.key"));
        assert!(is_known_secrets_file("keystore.jks"));
        assert!(is_known_secrets_file("store.p12"));
    }

    #[test]
    fn secrets_file_env() {
        assert!(is_known_secrets_file(".env"));
        assert!(is_known_secrets_file(".env.production"));
        assert!(is_known_secrets_file(".env.local"));
    }

    #[test]
    fn secrets_file_false_for_normal_files() {
        assert!(!is_known_secrets_file("src/main.rs"));
        assert!(!is_known_secrets_file("README.md"));
        assert!(!is_known_secrets_file("config.json"));
    }

    // ── PEM private key detection ────────────────────────────────────────────

    #[test]
    fn detects_pem_private_key() {
        let text = "config=yes\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK...\n-----END RSA PRIVATE KEY-----\ndone";
        let result = scan_and_redact(text);
        assert!(!result.findings.is_empty(), "should detect PEM private key");
        assert_eq!(result.findings[0].pattern_id, "PK-001");
        assert!(
            result.redacted.contains("[REDACTED:PRIVATE-KEY]"),
            "PEM block must be fully replaced"
        );
        assert!(
            !result.redacted.contains("MIIEowIBAAK"),
            "key material must not survive redaction"
        );
    }

    #[test]
    fn pem_public_key_not_redacted() {
        let text = "-----BEGIN PUBLIC KEY-----\nMFwwDQYJKoZ...\n-----END PUBLIC KEY-----\n";
        let result = scan_and_redact(text);
        assert!(
            !result.findings.iter().any(|f| f.pattern_id == "PK-001"),
            "public keys must not be redacted"
        );
    }

    // ── AWS access key ───────────────────────────────────────────────────────

    #[test]
    fn detects_aws_access_key() {
        let text = "key=AKIAIOSFODNN7EXAMPLE end";
        let result = scan_and_redact(text);
        let aws: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.pattern_id == "AWS-001")
            .collect();
        assert!(!aws.is_empty(), "should detect AWS access key");
        assert!(result.redacted.contains("AKIA"), "first 4 chars must survive partial redaction");
        assert!(!result.redacted.contains("AKIAIOSFODNN7EXAMPLE"), "full key must be gone");
    }

    // ── GitHub token ─────────────────────────────────────────────────────────

    #[test]
    fn detects_github_token() {
        let text = "token: ghp_16C7e42F292c6912E7710c838347Ae178B4a";
        let result = scan_and_redact(text);
        let gh: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.pattern_id == "GH-001")
            .collect();
        assert!(!gh.is_empty(), "should detect GitHub token");
    }

    // ── JWT ──────────────────────────────────────────────────────────────────

    #[test]
    fn detects_jwt() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let text = format!("Authorization: Bearer {jwt}");
        let result = scan_and_redact(&text);
        let jwt_findings: Vec<_> = result
            .findings
            .iter()
            .filter(|f| f.pattern_id == "JWT-001")
            .collect();
        assert!(!jwt_findings.is_empty(), "should detect JWT");
    }

    // ── scan_and_redact on clean content ─────────────────────────────────────

    #[test]
    fn clean_content_unchanged() {
        let text = "fn main() { println!(\"hello\"); }";
        let result = scan_and_redact(text);
        assert_eq!(result.redacted, text);
        assert!(result.findings.is_empty());
    }

    // ── preprocess (SMALL tier) ──────────────────────────────────────────────

    #[test]
    fn preprocess_excludes_env_file() {
        let result = preprocess("SECRET=abc123", ".env");
        assert_eq!(result.decision, SecretScanDecision::Excluded);
        assert!(result.content.is_none());
        assert!(result.stream.is_none());
    }

    #[test]
    fn preprocess_safe_for_clean_code() {
        let result = preprocess("fn main() {}", "src/main.rs");
        assert_eq!(result.decision, SecretScanDecision::Safe);
        assert!(result.content.is_some());
        assert!(result.stream.is_none());
    }

    #[test]
    fn preprocess_redacted_for_secret_content() {
        let text = "key: AKIAIOSFODNN7EXAMPLE";
        let result = preprocess(text, "src/config.rs");
        assert_eq!(result.decision, SecretScanDecision::Redacted);
        assert!(result.content.is_some());
        assert!(!result.findings.is_empty());
    }

    // ── partial_redact ───────────────────────────────────────────────────────

    #[test]
    fn partial_redact_keeps_first_four() {
        let r = partial_redact("AKIAIOSFODNN7EXAMPLE");
        assert!(r.starts_with("AKIA"));
        assert!(r.ends_with("***"));
    }

    #[test]
    fn partial_redact_short_string() {
        let r = partial_redact("AB");
        assert!(r.starts_with("AB"));
        assert!(r.ends_with("***"));
    }

    // ── file size tier classification ────────────────────────────────────────

    #[test]
    fn classify_file_size_tiers() {
        assert_eq!(classify_file_size(0), FileSizeTier::Small);
        assert_eq!(classify_file_size(SMALL_FILE_THRESHOLD), FileSizeTier::Small);
        assert_eq!(classify_file_size(SMALL_FILE_THRESHOLD + 1), FileSizeTier::Large);
        assert_eq!(classify_file_size(VERY_LARGE_FILE_THRESHOLD), FileSizeTier::Large);
        assert_eq!(classify_file_size(VERY_LARGE_FILE_THRESHOLD + 1), FileSizeTier::VeryLarge);
    }

    // =========================================================================
    // LARGE-file streaming tests (the 8 required scenarios)
    // =========================================================================

    // ── (a) Secret in the middle of a LARGE file is detected ─────────────────

    #[test]
    fn large_file_secret_in_middle_detected() {
        // Build a file: [filler | AWS key | filler]
        // Total > SMALL_FILE_THRESHOLD to exercise LARGE path.
        let before = filler(SMALL_FILE_THRESHOLD as usize + 100);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let after = filler(500);
        let content = format!("{before} {secret} {after}");

        let tmp = write_temp(&content);
        let (decision, findings) =
            stream_scan_large_file_classify(tmp.path()).unwrap();

        assert_eq!(decision, SecretScanDecision::Redacted, "must detect secret");
        let aws: Vec<_> = findings.iter().filter(|f| f.pattern_id == "AWS-001").collect();
        assert!(!aws.is_empty(), "AWS-001 finding expected; got: {findings:?}");
    }

    // ── (b) Consuming via safe streaming API never yields the raw secret ──────

    #[test]
    fn large_file_streaming_never_yields_raw_secret() {
        let before = filler(SMALL_FILE_THRESHOLD as usize + 100);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let after = filler(500);
        let content = format!("{before} {secret} {after}");

        let tmp = write_temp(&content);
        let result = preprocess_large_file(tmp.path(), "src/config.rs").unwrap();

        assert_eq!(result.decision, SecretScanDecision::Redacted);
        assert!(result.stream.is_some(), "LARGE file must have stream");
        assert!(result.content.is_none(), "LARGE file must NOT have content field");

        let stream = result.stream.unwrap();
        let (full_redacted, _findings) = stream.collect_all().unwrap();

        assert!(
            !full_redacted.contains(secret),
            "raw secret must NOT appear in streamed output; got snippet: {:?}",
            &full_redacted[full_redacted.len().saturating_sub(80)..]
        );
    }

    // ── (c) The redacted replacement IS yielded ───────────────────────────────

    #[test]
    fn large_file_streaming_yields_redacted_replacement() {
        let before = filler(SMALL_FILE_THRESHOLD as usize + 100);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let after = filler(500);
        let content = format!("{before} {secret} {after}");

        let tmp = write_temp(&content);
        let result = preprocess_large_file(tmp.path(), "src/config.rs").unwrap();
        let stream = result.stream.unwrap();
        let (full_redacted, _) = stream.collect_all().unwrap();

        // Partial redact for AWS-001: first 4 chars "AKIA" kept, then "***"
        assert!(
            full_redacted.contains("AKIA***"),
            "redacted replacement must appear in streamed output"
        );
    }

    // ── (d) Ordinary content before/after the secret remains available ────────

    #[test]
    fn large_file_ordinary_content_preserved() {
        // Use a unique marker string (no secrets, no high-entropy patterns).
        let marker_before = "ORDINARY_BEFORE_MARKER_";
        let marker_after = "_ORDINARY_AFTER_MARKER";
        // pad before to push into LARGE territory, but keep markers at recognisable spots
        let pad = filler(SMALL_FILE_THRESHOLD as usize + 100);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let content = format!("{marker_before}{pad} {secret} {marker_after}");

        let tmp = write_temp(&content);
        let result = preprocess_large_file(tmp.path(), "src/config.rs").unwrap();
        let stream = result.stream.unwrap();
        let (full_redacted, _) = stream.collect_all().unwrap();

        assert!(
            full_redacted.contains(marker_before),
            "content before the secret must be preserved"
        );
        assert!(
            full_redacted.contains(marker_after),
            "content after the secret must be preserved"
        );
    }

    // ── (e) Secret spanning a chunk boundary is detected and redacted ─────────

    #[test]
    fn large_file_secret_spanning_chunk_boundary_detected() {
        // Place the AWS key so it straddles a STREAM_CHUNK_SIZE boundary.
        // The key is 20 bytes; we place its start 10 bytes before the boundary.
        let boundary = STREAM_CHUNK_SIZE;
        let before_boundary = boundary - 10; // 10 bytes before chunk boundary
        let before = filler(before_boundary);
        let secret = "AKIAIOSFODNN7EXAMPLE"; // 20 bytes; spans boundary at -10/+10
        let after = filler(500);
        let content = format!("{before} {secret} {after}");

        let tmp = write_temp(&content);
        let (decision, findings) =
            stream_scan_large_file_classify(tmp.path()).unwrap();

        assert_eq!(
            decision,
            SecretScanDecision::Redacted,
            "boundary-spanning secret must be detected"
        );
        let aws: Vec<_> = findings.iter().filter(|f| f.pattern_id == "AWS-001").collect();
        assert!(
            !aws.is_empty(),
            "AWS-001 finding expected for boundary-spanning secret; got: {findings:?}"
        );

        // Also verify the streaming API redacts it.
        let stream = LargeFileStream::open(tmp.path()).unwrap();
        let (full_redacted, _) = stream.collect_all().unwrap();
        assert!(
            !full_redacted.contains(secret),
            "boundary-spanning secret must be redacted in stream output"
        );
        assert!(
            full_redacted.contains("AKIA***"),
            "redacted replacement must appear for boundary-spanning secret"
        );
    }

    // ── (f) Reported finding offsets are correct across chunk boundaries ──────

    #[test]
    fn large_file_finding_offsets_correct_across_boundaries() {
        // Place a secret at a known file offset and verify the reported offset.
        // Use a small well-defined layout: [pad | space | secret | space | tail]
        let pad_len = STREAM_CHUNK_SIZE - 10; // secret starts 10 bytes before chunk boundary
        let pad = filler(pad_len);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let content = format!("{pad} {secret} end");

        // Expected file offset of secret = pad_len + 1 (the space before it)
        let expected_offset = pad_len + 1;

        let tmp = write_temp(&content);
        let (_decision, findings) =
            stream_scan_large_file_classify(tmp.path()).unwrap();

        let aws: Vec<_> = findings.iter().filter(|f| f.pattern_id == "AWS-001").collect();
        assert!(!aws.is_empty(), "must find AWS-001");
        assert_eq!(
            aws[0].offset, expected_offset,
            "reported offset must equal actual file byte position; \
             expected {expected_offset}, got {}",
            aws[0].offset
        );
    }

    // ── (g) Overlap does not create duplicate findings ────────────────────────

    #[test]
    fn large_file_overlap_no_duplicate_findings() {
        // Place a secret entirely within the overlap zone of the second window
        // (i.e., in the last STREAM_OVERLAP_SIZE bytes of the first chunk).
        // It must appear exactly once in findings.
        let first_chunk_non_overlap = STREAM_CHUNK_SIZE - STREAM_OVERLAP_SIZE - 20;
        let pad = filler(first_chunk_non_overlap);
        let secret = "AKIAIOSFODNN7EXAMPLE";
        // The secret is in the overlap region of the first chunk.
        let content = format!("{pad} {secret} {}", filler(500));

        let tmp = write_temp(&content);
        let (_decision, findings) =
            stream_scan_large_file_classify(tmp.path()).unwrap();

        let aws: Vec<_> = findings.iter().filter(|f| f.pattern_id == "AWS-001").collect();
        assert_eq!(
            aws.len(),
            1,
            "secret in overlap zone must be reported exactly once; got: {aws:?}"
        );

        // Verify no duplicate offsets across all findings.
        let mut seen_offsets: Vec<(_, usize)> = Vec::new();
        for f in &findings {
            let key = (f.pattern_id, f.offset);
            assert!(
                !seen_offsets.contains(&key),
                "duplicate finding at offset {} for pattern {}",
                f.offset,
                f.pattern_id
            );
            seen_offsets.push(key);
        }

        // Also verify via the streaming API itself.
        let stream = LargeFileStream::open(tmp.path()).unwrap();
        let (_text, stream_findings) = stream.collect_all().unwrap();
        let stream_aws: Vec<_> =
            stream_findings.iter().filter(|f| f.pattern_id == "AWS-001").collect();
        assert_eq!(
            stream_aws.len(),
            1,
            "LargeFileStream must also report secret exactly once; got: {stream_aws:?}"
        );
    }

    // ── (h) Streaming remains bounded (does not allocate the entire file) ─────

    #[test]
    fn large_file_streaming_is_bounded() {
        // Write a file significantly larger than STREAM_CHUNK_SIZE.
        // We test that next_chunk() is called repeatedly (not once returning
        // the whole file), verifying the API is properly chunked.
        //
        // We write 5 * STREAM_CHUNK_SIZE bytes and count how many chunks
        // are returned.  A bounded implementation must return >= 2 chunks.
        let total = 5 * STREAM_CHUNK_SIZE;
        let content = filler(total);
        let tmp = write_temp(&content);

        let mut stream = LargeFileStream::open(tmp.path()).unwrap();
        let mut chunk_count = 0usize;
        let mut total_content_len = 0usize;

        while let Some(result) = stream.next_chunk() {
            let chunk = result.unwrap();
            total_content_len += chunk.redacted.len();
            chunk_count += 1;
        }

        assert!(
            chunk_count >= 2,
            "streaming must yield multiple chunks for a {total}-byte file; got {chunk_count}"
        );
        assert_eq!(
            total_content_len, total,
            "total streamed content length must equal file size"
        );
    }

    // ── Additional: LARGE file with no secrets yields Safe decision ───────────

    #[test]
    fn large_file_no_secrets_yields_safe() {
        let content = filler(SMALL_FILE_THRESHOLD as usize + 1000);
        let tmp = write_temp(&content);
        let (decision, findings) = stream_scan_large_file_classify(tmp.path()).unwrap();
        assert_eq!(decision, SecretScanDecision::Safe);
        assert!(findings.is_empty());
    }

    // ── Additional: preprocess_large_file returns stream=Some, content=None ──

    #[test]
    fn preprocess_large_file_returns_stream_not_content() {
        let content = filler(SMALL_FILE_THRESHOLD as usize + 100);
        let tmp = write_temp(&content);
        let result = preprocess_large_file(tmp.path(), "src/big.rs").unwrap();
        assert!(result.content.is_none(), "LARGE files must not populate content field");
        assert!(result.stream.is_some(), "LARGE files must populate stream field");
    }

    // ── Additional: excluded LARGE file returns neither content nor stream ────

    #[test]
    fn preprocess_large_file_excluded_returns_no_stream() {
        let content = filler(SMALL_FILE_THRESHOLD as usize + 100);
        let tmp = write_temp(&content);
        let result = preprocess_large_file(tmp.path(), ".env").unwrap();
        assert_eq!(result.decision, SecretScanDecision::Excluded);
        assert!(result.content.is_none());
        assert!(result.stream.is_none());
    }

    // ── Additional: compute_redacted_offset sanity check ─────────────────────

    #[test]
    fn compute_redacted_offset_no_findings() {
        let findings: Vec<SecretFinding> = Vec::new();
        let text = "hello world";
        // With no findings the redacted offset should equal split_at.
        assert_eq!(compute_redacted_offset(&findings, text, 5), 5);
        assert_eq!(compute_redacted_offset(&findings, text, 0), 0);
        assert_eq!(compute_redacted_offset(&findings, text, 11), 11);
    }
}
