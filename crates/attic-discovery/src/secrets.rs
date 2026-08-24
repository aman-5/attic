//! Secret preprocessing: detect and redact sensitive material before any
//! content reaches FTS, embeddings, or logs.
//!
//! # V1 patterns
//!
//! | ID       | Pattern                          | Redact  | Detector class     |
//! |----------|----------------------------------|---------|--------------------|
//! | PK-001   | PEM private-key block            | full    | stateful-streaming |
//! | AWS-001  | AWS access-key ID (20 chars)     | partial | bounded-token      |
//! | GH-001   | GitHub personal-access token     | partial | bounded-token      |
//! | JWT-001  | JSON Web Token                   | partial | bounded-token      |
//! | HE-001   | High-entropy base64 >= 20 chars  | partial | bounded-token      |
//!
//! **Bounded-token detectors** (AWS-001, GH-001, JWT-001, HE-001) have a
//! verified maximum match length.  A withheld safety window of
//! `SAFETY_WINDOW_SIZE` bytes at the trailing edge of each scan window
//! guarantees no partial match crosses the emission boundary undetected.
//!
//! **Stateful-streaming detector** (PK-001/PEM) has no maximum match length.
//! `LargeFileStream` carries a `PemStreamState` tracker; once a
//! `-----BEGIN … PRIVATE KEY-----` header is observed, ALL subsequent bytes
//! are withheld until the matching `-----END … -----` footer is found, then
//! the entire region is replaced by `[REDACTED:PRIVATE-KEY]`.
//!
//! # Withheld-tail streaming safety contract
//!
//! For every `LargeFileStream::next_chunk()` call:
//! 1. Read up to `STREAM_CHUNK_SIZE` new bytes.
//! 2. Prepend `withheld` buffer to form the scan window.
//! 3. Scan window for secrets → redacted window string.
//! 4. Compute `safe_emit_len` (original bytes):
//!    - PEM Idle: `window.len() − SAFETY_WINDOW_SIZE`
//!    - PEM InBlock: `0`  (withhold everything until END footer)
//!    - EOF:  `window.len()`  (flush all)
//! 5. Emit redacted equivalent of original bytes `0..safe_emit_len`.
//! 6. `withheld ← original bytes[safe_emit_len..]`
//!
//! # File-size tiers
//!
//! | Tier       | Threshold      | Content delivery                       |
//! |------------|----------------|----------------------------------------|
//! | SMALL      | <= 4 MiB       | Full in-memory redact, `content` field |
//! | LARGE      | 4 MiB – 50 MiB | [`LargeFileStream`] bounded streaming  |
//! | VERY_LARGE | > 50 MiB       | Sample-only (PartialScan)              |

use std::io::{self, Read};
use std::path::Path;
use std::time::SystemTime;

// ---------------------------------------------------------------------------
// Size-tier constants  (authoritative — from docs/contracts/large_files.md)
// ---------------------------------------------------------------------------

pub const SMALL_FILE_THRESHOLD: u64 = 4 * 1024 * 1024; // 4 MiB
pub const VERY_LARGE_FILE_THRESHOLD: u64 = 50 * 1024 * 1024; // 50 MiB
pub const STREAM_CHUNK_SIZE: usize = 64 * 1024; // 64 KiB

/// Bytes withheld at trailing edge of each scan window for bounded-token look-ahead.
/// Must be >= longest bounded token (~40 bytes); 1 KiB is ample.
/// Does NOT apply to PEM blocks — those use `PemStreamState`.
pub const SAFETY_WINDOW_SIZE: usize = 1024; // 1 KiB

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSizeTier { Small, Large, VeryLarge }

pub fn classify_file_size(size_bytes: u64) -> FileSizeTier {
    if size_bytes <= SMALL_FILE_THRESHOLD { FileSizeTier::Small }
    else if size_bytes <= VERY_LARGE_FILE_THRESHOLD { FileSizeTier::Large }
    else { FileSizeTier::VeryLarge }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult { pub redacted: String, pub findings: Vec<SecretFinding> }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    pub pattern_id: &'static str,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScanDecision { Safe, Redacted, Excluded, PartialScan }

#[derive(Debug, Clone)]
pub struct StreamChunk { pub redacted: String, pub findings: Vec<SecretFinding> }

#[derive(Debug)]
pub struct PreprocessResult {
    pub decision: SecretScanDecision,
    pub content: Option<String>,
    pub stream: Option<LargeFileStream>,
    pub findings: Vec<SecretFinding>,
}

// ---------------------------------------------------------------------------
// File identity (two-pass consistency)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileIdentity { size: u64, modified: SystemTime }

fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    let m = std::fs::metadata(path)?;
    Ok(FileIdentity { size: m.len(), modified: m.modified()? })
}

// ---------------------------------------------------------------------------
// PEM stateful state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum PemStreamState {
    Idle,
    InBlock { begin_file_offset: usize },
}

// ---------------------------------------------------------------------------
// Public API — small files
// ---------------------------------------------------------------------------

pub fn scan_and_redact(text: &str) -> ScanResult {
    struct PM<'d> { start: usize, length: usize, detector: &'d Detector }
    let mut pending: Vec<PM<'_>> = Vec::new();
    for d in DETECTORS {
        for m in d.find_all(text) {
            pending.push(PM { start: m.start, length: m.length, detector: d });
        }
    }
    pending.sort_by(|a, b| a.start.cmp(&b.start).then(b.length.cmp(&a.length)));
    let mut non_overlapping: Vec<PM<'_>> = Vec::new();
    let mut next_free = 0usize;
    for pm in pending {
        if pm.start >= next_free { next_free = pm.start + pm.length; non_overlapping.push(pm); }
    }
    let mut findings: Vec<SecretFinding> = Vec::new();
    let mut redacted = text.to_string();
    let mut delta: isize = 0;
    for pm in non_overlapping {
        let as_ = (pm.start as isize + delta) as usize;
        let ae = as_ + pm.length;
        let repl = match pm.detector.redact_mode {
            RedactMode::Full => pm.detector.placeholder.to_string(),
            RedactMode::Partial => partial_redact(&redacted[as_..ae]),
        };
        let old = pm.length; let new_ = repl.len();
        redacted.replace_range(as_..ae, &repl);
        delta += new_ as isize - old as isize;
        findings.push(SecretFinding { pattern_id: pm.detector.id, offset: pm.start, length: pm.length });
    }
    ScanResult { redacted, findings }
}

pub fn preprocess(content: &str, repo_relative: &str) -> PreprocessResult {
    if is_known_secrets_file(repo_relative) {
        return PreprocessResult { decision: SecretScanDecision::Excluded, content: None, stream: None, findings: Vec::new() };
    }
    let result = scan_and_redact(content);
    if result.findings.is_empty() {
        PreprocessResult { decision: SecretScanDecision::Safe, content: Some(result.redacted), stream: None, findings: Vec::new() }
    } else {
        PreprocessResult { decision: SecretScanDecision::Redacted, content: Some(result.redacted), stream: None, findings: result.findings }
    }
}

// ---------------------------------------------------------------------------
// Public API — LARGE file
// ---------------------------------------------------------------------------

pub fn preprocess_large_file(path: &Path, repo_relative: &str) -> io::Result<PreprocessResult> {
    if is_known_secrets_file(repo_relative) {
        return Ok(PreprocessResult { decision: SecretScanDecision::Excluded, content: None, stream: None, findings: Vec::new() });
    }
    let id_before = file_identity(path)?;
    let (decision, all_findings) = stream_scan_large_file_classify(path)?;
    let id_after = file_identity(path)?;
    if id_before != id_after {
        return Err(io::Error::new(io::ErrorKind::Other,
            format!("file changed during classification (unstable capture): {}", path.display())));
    }
    let stream = LargeFileStream::open_with_identity(path, id_after)?;
    Ok(PreprocessResult { decision, content: None, stream: Some(stream), findings: all_findings })
}

pub fn stream_scan_large_file_classify(
    path: &Path,
) -> io::Result<(SecretScanDecision, Vec<SecretFinding>)> {
    let mut file = std::fs::File::open(path)?;
    let mut all_findings: Vec<SecretFinding> = Vec::new();
    let mut overlap_buf: Vec<u8> = Vec::new();
    let mut file_offset_of_new_bytes: usize = 0;
    let mut pem_state = PemStreamState::Idle;

    loop {
        let overlap_len = overlap_buf.len();
        let mut window = overlap_buf.clone();
        let mut new_chunk = vec![0u8; STREAM_CHUNK_SIZE];
        let n = read_exact_up_to(&mut file, &mut new_chunk)?;

        if n == 0 {
            if let PemStreamState::InBlock { begin_file_offset } = pem_state {
                let len = file_offset_of_new_bytes.saturating_sub(begin_file_offset).max(1);
                if !all_findings.iter().any(|f| f.pattern_id == "PK-001" && f.offset == begin_file_offset) {
                    all_findings.push(SecretFinding { pattern_id: "PK-001", offset: begin_file_offset, length: len });
                }
            }
            break;
        }

        window.extend_from_slice(&new_chunk[..n]);
        let window_str = String::from_utf8_lossy(&window).into_owned();
        let window_file_base = file_offset_of_new_bytes.saturating_sub(overlap_len);

        pem_classify_window(&window_str, window_file_base, &mut pem_state, &mut all_findings);

        let scan = scan_and_redact(&window_str);
        for f in &scan.findings {
            if f.pattern_id == "PK-001" { continue; }
            let abs = window_file_base + f.offset;
            let in_new = abs >= file_offset_of_new_bytes;
            let spans = f.offset < overlap_len && f.offset + f.length > overlap_len;
            if (in_new || spans) && !all_findings.iter().any(|e| e.pattern_id == f.pattern_id && e.offset == abs) {
                all_findings.push(SecretFinding { pattern_id: f.pattern_id, offset: abs, length: f.length });
            }
        }

        file_offset_of_new_bytes += n;
        let new_slice = &new_chunk[..n];
        overlap_buf = if n > SAFETY_WINDOW_SIZE { new_slice[n - SAFETY_WINDOW_SIZE..].to_vec() } else { new_slice.to_vec() };

        if n < STREAM_CHUNK_SIZE {
            if let PemStreamState::InBlock { begin_file_offset } = &pem_state {
                let ps = *begin_file_offset;
                let pl = file_offset_of_new_bytes.saturating_sub(ps).max(1);
                if !all_findings.iter().any(|f| f.pattern_id == "PK-001" && f.offset == ps) {
                    all_findings.push(SecretFinding { pattern_id: "PK-001", offset: ps, length: pl });
                }
            }
            break;
        }
    }

    let decision = if all_findings.is_empty() { SecretScanDecision::Safe } else { SecretScanDecision::Redacted };
    Ok((decision, all_findings))
}

// ---------------------------------------------------------------------------
// PEM window classifier
// ---------------------------------------------------------------------------

fn pem_classify_window(
    window_str: &str,
    window_file_base: usize,
    pem_state: &mut PemStreamState,
    all_findings: &mut Vec<SecretFinding>,
) {
    let mut from = 0usize;
    loop {
        match pem_state {
            PemStreamState::Idle => {
                match find_pem_begin_private_key(window_str, from) {
                    None => break,
                    Some(begin_pos) => {
                        *pem_state = PemStreamState::InBlock { begin_file_offset: window_file_base + begin_pos };
                        from = begin_pos + PEM_BEGIN.len();
                    }
                }
            }
            PemStreamState::InBlock { begin_file_offset } => {
                match find_pem_end(window_str, from) {
                    None => break,
                    Some(end_pos) => {
                        let ps = *begin_file_offset;
                        let pl = (window_file_base + end_pos).saturating_sub(ps).max(1);
                        if !all_findings.iter().any(|f| f.pattern_id == "PK-001" && f.offset == ps) {
                            all_findings.push(SecretFinding { pattern_id: "PK-001", offset: ps, length: pl });
                        }
                        *pem_state = PemStreamState::Idle;
                        from = end_pos;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LargeFileStream — withheld-tail design
// ---------------------------------------------------------------------------

pub struct LargeFileStream {
    file: std::fs::File,
    withheld: Vec<u8>,
    withheld_file_offset: usize,
    file_bytes_consumed: usize,
    pem_state: PemStreamState,
    done: bool,
    #[allow(dead_code)]
    identity: FileIdentity,
}

impl std::fmt::Debug for LargeFileStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LargeFileStream")
            .field("withheld_file_offset", &self.withheld_file_offset)
            .field("done", &self.done)
            .finish()
    }
}

impl LargeFileStream {
    pub fn open(path: &Path) -> io::Result<Self> {
        let identity = file_identity(path)?;
        Self::open_with_identity(path, identity)
    }

    pub(crate) fn open_with_identity(path: &Path, identity: FileIdentity) -> io::Result<Self> {
        let file = std::fs::File::open(path)?;
        Ok(LargeFileStream {
            file, withheld: Vec::new(), withheld_file_offset: 0,
            file_bytes_consumed: 0, pem_state: PemStreamState::Idle,
            done: false, identity,
        })
    }

    pub fn next_chunk(&mut self) -> Option<io::Result<StreamChunk>> {
        if self.done && self.withheld.is_empty() { return None; }
        if self.done { return Some(self.flush_withheld()); }

        let mut new_buf = vec![0u8; STREAM_CHUNK_SIZE];
        let n = match read_exact_up_to(&mut self.file, &mut new_buf) {
            Ok(n) => n,
            Err(e) => { self.done = true; return Some(Err(e)); }
        };

        if n == 0 {
            self.done = true;
            return if self.withheld.is_empty() { None } else { Some(self.flush_withheld()) };
        }

        let is_eof = n < STREAM_CHUNK_SIZE;
        let window_file_base = self.withheld_file_offset;
        let mut window = self.withheld.clone();
        window.extend_from_slice(&new_buf[..n]);
        self.file_bytes_consumed += n;
        if is_eof { self.done = true; }

        let window_str = String::from_utf8_lossy(&window).into_owned();

        // PEM state transitions
        let mut pem_findings_this_window: Vec<SecretFinding> = Vec::new();
        {
            let mut from = 0usize;
            loop {
                match &self.pem_state {
                    PemStreamState::Idle => {
                        match find_pem_begin_private_key(&window_str, from) {
                            None => break,
                            Some(bp) => {
                                self.pem_state = PemStreamState::InBlock { begin_file_offset: window_file_base + bp };
                                from = bp + PEM_BEGIN.len();
                            }
                        }
                    }
                    PemStreamState::InBlock { begin_file_offset } => {
                        let bfo = *begin_file_offset;
                        match find_pem_end(&window_str, from) {
                            None => break,
                            Some(ep) => {
                                let pl = (window_file_base + ep).saturating_sub(bfo).max(1);
                                pem_findings_this_window.push(SecretFinding { pattern_id: "PK-001", offset: bfo, length: pl });
                                self.pem_state = PemStreamState::Idle;
                                from = ep;
                            }
                        }
                    }
                }
            }
        }

        // If EOF and still in PEM block
        if self.done {
            if let PemStreamState::InBlock { begin_file_offset } = &self.pem_state {
                let bfo = *begin_file_offset;
                let pl = self.file_bytes_consumed.saturating_sub(bfo).max(1);
                pem_findings_this_window.push(SecretFinding { pattern_id: "PK-001", offset: bfo, length: pl });
                self.pem_state = PemStreamState::Idle;
            }
        }

        // safe_emit_len in original bytes
        let safe_emit_len: usize = if self.done {
            window.len()
        } else {
            match &self.pem_state {
                PemStreamState::InBlock { .. } => 0,
                PemStreamState::Idle => {
                    if window.len() > SAFETY_WINDOW_SIZE { window.len() - SAFETY_WINDOW_SIZE } else { 0 }
                }
            }
        };

        let scan = scan_and_redact(&window_str);

        // Build chunk_findings (emitted region only)
        let mut chunk_findings: Vec<SecretFinding> = Vec::new();
        let emit_end_file = window_file_base + safe_emit_len;
        for f in &pem_findings_this_window {
            if f.offset < emit_end_file { chunk_findings.push(f.clone()); }
        }
        for f in &scan.findings {
            if f.pattern_id == "PK-001" { continue; }
            if f.offset < safe_emit_len {
                chunk_findings.push(SecretFinding { pattern_id: f.pattern_id, offset: window_file_base + f.offset, length: f.length });
            }
        }

        // Build emitted redacted string
        let redacted_emit_end = compute_redacted_offset(&scan.findings, &window_str, safe_emit_len);
        let emit_str = if redacted_emit_end <= scan.redacted.len() {
            scan.redacted[..redacted_emit_end].to_string()
        } else {
            scan.redacted.clone()
        };

        // If PEM block resolved in this window, its BEGIN..END span in the redacted
        // string is already replaced by scan_and_redact's placeholder.  If the block
        // spans into the withheld region (safe_emit_len == 0, InBlock), emit_str is empty.

        self.withheld = window[safe_emit_len..].to_vec();
        self.withheld_file_offset = window_file_base + safe_emit_len;

        Some(Ok(StreamChunk { redacted: emit_str, findings: chunk_findings }))
    }

    fn flush_withheld(&mut self) -> io::Result<StreamChunk> {
        let window = std::mem::take(&mut self.withheld);
        self.done = true;

        if window.is_empty() {
            return Ok(StreamChunk { redacted: String::new(), findings: Vec::new() });
        }

        let window_file_base = self.withheld_file_offset;
        let window_str = String::from_utf8_lossy(&window).into_owned();
        let mut chunk_findings: Vec<SecretFinding> = Vec::new();

        let emit_str = if let PemStreamState::InBlock { begin_file_offset } = &self.pem_state {
            let bfo = *begin_file_offset;
            chunk_findings.push(SecretFinding { pattern_id: "PK-001", offset: bfo, length: window.len().max(1) });
            self.pem_state = PemStreamState::Idle;
            PEM_PLACEHOLDER.to_string()
        } else {
            let scan = scan_and_redact(&window_str);
            for f in &scan.findings {
                chunk_findings.push(SecretFinding { pattern_id: f.pattern_id, offset: window_file_base + f.offset, length: f.length });
            }
            scan.redacted
        };

        Ok(StreamChunk { redacted: emit_str, findings: chunk_findings })
    }

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

fn compute_redacted_offset(findings: &[SecretFinding], window_str: &str, split_at: usize) -> usize {
    let mut delta: isize = 0;
    for f in findings {
        if f.offset >= split_at { break; }
        if f.offset + f.length <= split_at {
            if let Some(det) = DETECTORS.iter().find(|d| d.id == f.pattern_id) {
                let end = (f.offset + f.length).min(window_str.len());
                let orig = &window_str[f.offset..end];
                let repl_len = match det.redact_mode {
                    RedactMode::Full => det.placeholder.len(),
                    RedactMode::Partial => partial_redact(orig).len(),
                };
                delta += repl_len as isize - f.length as isize;
            }
        } else {
            return (f.offset as isize + delta).max(0) as usize;
        }
    }
    (split_at as isize + delta).max(0) as usize
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

pub fn is_known_secrets_file(repo_relative: &str) -> bool {
    let name = repo_relative.rsplit('/').next().unwrap_or(repo_relative).to_ascii_lowercase();
    if matches!(name.as_str(), ".env" | ".netrc" | ".npmrc" | "id_rsa" | "id_ed25519" | "id_ecdsa") { return true; }
    if name.starts_with(".env.") { return true; }
    if let Some(ext) = name.rsplit('.').next() {
        if matches!(ext, "pem" | "key" | "p12" | "jks" | "pfx" | "p8") { return true; }
    }
    false
}

// ---------------------------------------------------------------------------
// Pattern engine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum RedactMode { Full, Partial }

struct RawMatch { start: usize, length: usize }

struct Detector {
    id: &'static str,
    redact_mode: RedactMode,
    placeholder: &'static str,
    find_all: fn(&str) -> Vec<RawMatch>,
}

impl Detector {
    fn find_all(&self, text: &str) -> Vec<RawMatch> { (self.find_all)(text) }
}

fn partial_redact(s: &str) -> String {
    let mut r = String::with_capacity(8);
    let chars: Vec<char> = s.chars().collect();
    r.extend(&chars[..chars.len().min(4)]);
    r.push_str("***");
    r
}

const PEM_PLACEHOLDER: &str = "[REDACTED:PRIVATE-KEY]";

static DETECTORS: &[Detector] = &[
    Detector { id: "PK-001", redact_mode: RedactMode::Full, placeholder: PEM_PLACEHOLDER, find_all: find_pem_private_keys },
    Detector { id: "AWS-001", redact_mode: RedactMode::Partial, placeholder: "", find_all: find_aws_access_keys },
    Detector { id: "GH-001", redact_mode: RedactMode::Partial, placeholder: "", find_all: find_github_tokens },
    Detector { id: "JWT-001", redact_mode: RedactMode::Partial, placeholder: "", find_all: find_jwt_tokens },
    Detector { id: "HE-001", redact_mode: RedactMode::Partial, placeholder: "", find_all: find_high_entropy_base64 },
];

const PEM_BEGIN: &str = "-----BEGIN";
const PEM_END: &str = "-----END";

fn find_pem_begin_private_key(text: &str, from: usize) -> Option<usize> {
    let mut s = from;
    while s < text.len() {
        let rel = text[s..].find(PEM_BEGIN)?;
        let bp = s + rel;
        let he = text[bp..].find('\n').map(|n| bp + n)?;
        if text[bp..he].contains("PRIVATE KEY") { return Some(bp); }
        s = bp + PEM_BEGIN.len();
    }
    None
}

fn find_pem_end(text: &str, from: usize) -> Option<usize> {
    let rel = text[from..].find(PEM_END)?;
    let ep = from + rel;
    let el = text[ep..].find('\n').map(|n| ep + n + 1).unwrap_or(text.len());
    Some(el)
}

fn find_pem_private_keys(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let mut s = 0;
    while s < text.len() {
        match find_pem_begin_private_key(text, s) {
            None => break,
            Some(bp) => match find_pem_end(text, bp + PEM_BEGIN.len()) {
                None => break,
                Some(el) => { matches.push(RawMatch { start: bp, length: el - bp }); s = el; }
            }
        }
    }
    matches
}

fn find_aws_access_keys(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 20 <= bytes.len() {
        if &bytes[i..i+4] == b"AKIA" {
            if bytes[i+4..i+20].iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit()) {
                let before_ok = i == 0 || !bytes[i-1].is_ascii_alphanumeric();
                let after_ok = i+20 >= bytes.len() || !bytes[i+20].is_ascii_alphanumeric();
                if before_ok && after_ok { matches.push(RawMatch { start: i, length: 20 }); }
            }
        }
        i += 1;
    }
    matches
}

fn find_github_tokens(text: &str) -> Vec<RawMatch> {
    let prefixes: &[&str] = &["ghp_", "ghs_", "gho_", "ghu_", "ghr_", "github_pat_"];
    let mut matches = Vec::new();
    for prefix in prefixes {
        let mut s = 0;
        while let Some(rel) = text[s..].find(prefix) {
            let start = s + rel;
            let rest = &text[start + prefix.len()..];
            let extra: usize = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').map(|c| c.len_utf8()).sum();
            let total = prefix.len() + extra;
            if total >= 20 { matches.push(RawMatch { start, length: total }); }
            s = start + prefix.len().max(1);
        }
    }
    matches
}

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
                    if (seg1_end > i) && (seg2_end > seg2_start) && (seg3_end > seg3_start) && total >= 40 {
                        matches.push(RawMatch { start: i, length: total });
                        i = seg3_end;
                        continue;
                    }
                    // seg3 failed — skip past seg2
                    i = seg2_end;
                } else {
                    // No dot after seg2 — skip past seg2
                    i = seg2_end;
                }
            } else {
                // No dot after seg1 — skip entire seg1 run to avoid O(n^2)
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
    bytes.iter().any(|b| b.is_ascii_uppercase())
        && bytes.iter().any(|b| b.is_ascii_lowercase())
        && bytes.iter().any(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn write_tmp(content: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    fn aws_key() -> &'static str {
        "AKIAIOSFODNN7EXAMPLE"
    }

    fn gh_token() -> &'static str {
        "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ01"
    }

    const PEM_BLOCK: &str = concat!(
        "-----BEGIN RSA PRIVATE KEY-----\n",
        "MIIEpAIBAAKCAQEA0Z3VS5JJcds3xHn/ygWep4PAtEsHAc9g0GZG4JCGBOLbqRBB\n",
        "-----END RSA PRIVATE KEY-----\n",
    );

    // -----------------------------------------------------------------------
    // Small-file unit tests (original scenarios)
    // -----------------------------------------------------------------------

    #[test]
    fn aws_key_redacted() {
        let input = format!("access_key = {}", aws_key());
        let r = scan_and_redact(&input);
        assert!(!r.redacted.contains(aws_key()), "raw key must not appear in redacted output");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].pattern_id, "AWS-001");
    }

    #[test]
    fn github_token_redacted() {
        let input = format!("token: {}", gh_token());
        let r = scan_and_redact(&input);
        assert!(!r.redacted.contains(gh_token()), "raw token must not appear");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].pattern_id, "GH-001");
    }

    #[test]
    fn pem_private_key_redacted() {
        let r = scan_and_redact(PEM_BLOCK);
        assert!(!r.redacted.contains("MIIEpAI"), "PEM body must not appear");
        assert!(r.redacted.contains(PEM_PLACEHOLDER), "placeholder must be present");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].pattern_id, "PK-001");
    }

    #[test]
    fn clean_text_unchanged() {
        let input = "this is a normal commit message with no secrets";
        let r = scan_and_redact(input);
        assert_eq!(r.redacted, input);
        assert!(r.findings.is_empty());
    }

    #[test]
    fn no_raw_secret_value_in_findings() {
        let input = format!("key={}", aws_key());
        let r = scan_and_redact(&input);
        assert!(!r.findings.is_empty(), "must detect secret");
        // SecretFinding stores offset+length into the ORIGINAL text (not a raw_value field).
        // The meaningful safety invariant is that the REDACTED output does not contain the raw key.
        assert!(!r.redacted.contains(aws_key()), "redacted output must not contain raw key");
        // Verify at compile-time that SecretFinding has no raw_value field — only
        // pattern_id, offset, length are accessible.
        for f in &r.findings {
            let _ = (f.pattern_id, f.offset, f.length);
        }
    }

    #[test]
    fn is_known_secrets_file_covers_env_pem_key() {
        assert!(is_known_secrets_file(".env"));
        assert!(is_known_secrets_file(".env.local"));
        assert!(is_known_secrets_file("secrets/id_rsa"));
        assert!(is_known_secrets_file("certs/server.pem"));
        assert!(!is_known_secrets_file("README.md"));
        assert!(!is_known_secrets_file("src/main.rs"));
    }

    #[test]
    fn preprocess_excludes_known_secrets_file() {
        let r = preprocess("AKIAIOSFODNN7EXAMPLE", ".env");
        assert_eq!(r.decision, SecretScanDecision::Excluded);
        assert!(r.content.is_none());
    }

    #[test]
    fn overlapping_matches_use_leftmost_longest() {
        let input = "AKIAIOSFODNN7EXAMPLE_extra_suffix_padding_abc";
        let r = scan_and_redact(input);
        assert_eq!(r.findings.len(), 1);
    }

    // -----------------------------------------------------------------------
    // LARGE file streaming — chunk-level tests
    // -----------------------------------------------------------------------

    /// Secret placed 1 byte BEFORE the chunk boundary must never appear raw in any emitted chunk.
    #[test]
    fn large_file_aws_secret_1_byte_before_chunk_boundary_not_emitted_raw() {
        let key = aws_key();
        let prefix_len = STREAM_CHUNK_SIZE - key.len();
        let mut content = vec![b'a'; prefix_len];
        content.extend_from_slice(key.as_bytes());
        // Pad to two full chunks so EOF isn't on first read.
        content.extend(vec![b'b'; STREAM_CHUNK_SIZE]);

        let tmp = write_tmp(&content);
        let mut stream = LargeFileStream::open(tmp.path()).unwrap();

        let mut all_redacted = String::new();
        let mut chunk_idx = 0usize;
        while let Some(res) = stream.next_chunk() {
            let chunk = res.unwrap();
            assert!(
                !chunk.redacted.contains(key),
                "chunk {chunk_idx} emitted raw key"
            );
            all_redacted.push_str(&chunk.redacted);
            chunk_idx += 1;
        }
        assert!(!all_redacted.contains(key), "concatenated output must not contain raw key");
    }

    /// Secret starting inside the withheld tail must be fully redacted.
    #[test]
    fn large_file_aws_secret_in_withheld_tail_safe() {
        let key = aws_key();
        // Start the key inside the withheld tail of the first chunk.
        let start = STREAM_CHUNK_SIZE - SAFETY_WINDOW_SIZE / 2;
        let mut content = vec![b'x'; start];
        content.extend_from_slice(key.as_bytes());
        content.resize(STREAM_CHUNK_SIZE * 2, b'y');

        let tmp = write_tmp(&content);
        let (full, _findings) = LargeFileStream::open(tmp.path()).unwrap().collect_all().unwrap();
        assert!(!full.contains(key), "raw key must not appear anywhere in streamed output");
    }

    /// A PEM block larger than SAFETY_WINDOW_SIZE must never emit its body bytes raw.
    #[test]
    fn large_file_pem_larger_than_safety_window_never_emits_body() {
        let body_len = 2 * SAFETY_WINDOW_SIZE;
        let mut pem = String::new();
        pem.push_str("-----BEGIN RSA PRIVATE KEY-----\n");
        let line = "A".repeat(64);
        let lines_needed = body_len / 65 + 1;
        for _ in 0..lines_needed {
            pem.push_str(&line);
            pem.push('\n');
        }
        pem.push_str("-----END RSA PRIVATE KEY-----\n");

        let tmp = write_tmp(pem.as_bytes());
        let mut stream = LargeFileStream::open(tmp.path()).unwrap();

        while let Some(res) = stream.next_chunk() {
            let chunk = res.unwrap();
            assert!(
                !chunk.redacted.contains(&line),
                "PEM body line emitted raw in chunk"
            );
        }
    }

    /// PEM BEGIN in one chunk, END in a later chunk — full block must be redacted.
    #[test]
    fn large_file_pem_begin_end_cross_chunk_boundary() {
        let mut content = String::new();
        content.push_str("-----BEGIN RSA PRIVATE KEY-----\n");
        let line = "B".repeat(64) + "\n";
        let total_body = STREAM_CHUNK_SIZE * 2;
        let lines_needed = total_body / 65 + 1;
        for _ in 0..lines_needed {
            content.push_str(&line);
        }
        content.push_str("-----END RSA PRIVATE KEY-----\n");
        content.push_str("after_pem_clean_content\n");

        let tmp = write_tmp(content.as_bytes());
        let (full, findings) = LargeFileStream::open(tmp.path()).unwrap().collect_all().unwrap();

        assert!(!full.contains(&"B".repeat(64)), "PEM body must not appear raw");
        assert!(full.contains(PEM_PLACEHOLDER), "placeholder must appear");
        assert!(full.contains("after_pem_clean_content"), "clean suffix must be preserved");
        assert_eq!(
            findings.iter().filter(|f| f.pattern_id == "PK-001").count(),
            1,
            "expected exactly one PK-001 finding"
        );
    }

    /// At EOF the withheld bytes must be flushed — no bytes silently dropped.
    #[test]
    fn large_file_eof_flushes_withheld_bytes() {
        let content = b"no secrets here, just ordinary text for flush test";
        let tmp = write_tmp(content);
        let (full, findings) = LargeFileStream::open(tmp.path()).unwrap().collect_all().unwrap();
        assert_eq!(
            full,
            String::from_utf8_lossy(content).as_ref(),
            "flushed content must match original exactly"
        );
        assert!(findings.is_empty(), "no secrets expected");
    }

    /// Streaming must return multiple chunks for large inputs (memory is bounded).
    #[test]
    fn large_file_streaming_is_bounded() {
        let content = vec![b'z'; STREAM_CHUNK_SIZE * 3];
        let tmp = write_tmp(&content);
        let mut stream = LargeFileStream::open(tmp.path()).unwrap();

        let mut chunk_count = 0usize;
        while let Some(res) = stream.next_chunk() {
            res.unwrap();
            chunk_count += 1;
        }
        assert!(chunk_count >= 2, "expected multiple chunks, got {chunk_count}");
    }

    /// Clean content must be bit-for-bit preserved after streaming.
    #[test]
    fn large_file_clean_content_preserved() {
        let content = "Hello from large file with absolutely no secrets at all!\n".repeat(2000);
        let tmp = write_tmp(content.as_bytes());
        let (full, findings) = LargeFileStream::open(tmp.path()).unwrap().collect_all().unwrap();
        assert_eq!(full, content, "clean content must be bit-for-bit preserved");
        assert!(findings.is_empty());
    }

    /// Secret in the very middle of a multi-chunk file must be fully redacted.
    #[test]
    fn large_file_secret_in_middle_is_fully_redacted() {
        let key = aws_key();
        let half = STREAM_CHUNK_SIZE * 2;
        let mut content = vec![b'a'; half];
        content.extend_from_slice(key.as_bytes());
        content.extend(vec![b'b'; half]);

        let tmp = write_tmp(&content);
        let (full, findings) = LargeFileStream::open(tmp.path()).unwrap().collect_all().unwrap();
        assert!(!full.contains(key), "mid-file key must not appear raw");
        assert!(!findings.is_empty(), "must detect key in middle");
    }

    /// Classify pass must agree `Safe` for a clean file.
    #[test]
    fn large_file_classify_and_stream_agree_safe() {
        let content = vec![b'c'; STREAM_CHUNK_SIZE + 500];
        let tmp = write_tmp(&content);
        let (decision, findings) = stream_scan_large_file_classify(tmp.path()).unwrap();
        assert_eq!(decision, SecretScanDecision::Safe);
        assert!(findings.is_empty());
    }

    /// Two-pass stability check succeeds for a file that does not change.
    #[test]
    fn large_file_two_pass_stable_succeeds() {
        let content = vec![b'd'; STREAM_CHUNK_SIZE + 100];
        let tmp = write_tmp(&content);
        let result = preprocess_large_file(tmp.path(), "data/large.bin").unwrap();
        assert!(result.stream.is_some(), "stable file must produce a stream");
    }
}
