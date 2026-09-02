use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub const SMALL_FILE_THRESHOLD: u64 = 4 * 1024 * 1024;
pub const VERY_LARGE_FILE_THRESHOLD: u64 = 50 * 1024 * 1024;
pub const STREAM_CHUNK_SIZE: usize = 64 * 1024;
pub const SAFETY_WINDOW_SIZE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSizeTier {
    Small,
    Large,
    VeryLarge,
}

pub fn classify_file_size(size_bytes: u64) -> FileSizeTier {
    if size_bytes <= SMALL_FILE_THRESHOLD {
        FileSizeTier::Small
    } else if size_bytes <= VERY_LARGE_FILE_THRESHOLD {
        FileSizeTier::Large
    } else {
        FileSizeTier::VeryLarge
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanResult {
    pub redacted: String,
    pub findings: Vec<SecretFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    pub pattern_id: &'static str,
    pub offset: usize,
    pub length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScanDecision {
    Safe,
    Redacted,
    Excluded,
    PartialScan,
}

#[derive(Debug, Clone)]
pub struct StreamChunk {
    pub redacted: String,
    pub findings: Vec<SecretFinding>,
}

#[derive(Debug)]
pub struct PreprocessResult {
    pub decision: SecretScanDecision,
    pub content: Option<String>,
    pub stream: Option<LargeFileStream>,
    pub findings: Vec<SecretFinding>,
}

/// Source identity: BLAKE3 content hash of the file (64 lowercase hex chars).
///
/// This aligns with the `SourceRevision` / `ManifestEntry` contract in
/// `manifest.rs` — the same BLAKE3 primitive is used throughout the codebase.
/// A size+mtime approach is intentionally avoided because mtime resolution
/// can be as coarse as 1 second on some filesystems, making silent
/// modification invisible to the identity check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    pub(crate) content_hash: String,
}

/// Compute the BLAKE3 content hash of a file.  Streams in `STREAM_CHUNK_SIZE`
/// blocks to bound memory use.  Mirrors `hash_file_content` in `manifest.rs`.
fn file_identity(path: &Path) -> io::Result<FileIdentity> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; STREAM_CHUNK_SIZE];
    loop {
        let n = read_exact_up_to(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(FileIdentity {
        content_hash: hasher.finalize().to_hex().to_string(),
    })
}

#[derive(Debug, Clone)]
enum PemStreamState {
    Idle,
    Draining {
        begin_file_offset: usize,
        tail: Vec<u8>,
    },
}

const PEM_BEGIN: &str = "-----BEGIN";
const PEM_PRIVATE_KEY_MARKER: &str = "PRIVATE KEY-----";
const PEM_END: &str = "-----END";
const PEM_PLACEHOLDER: &str = "[REDACTED:PRIVATE-KEY]";

pub fn scan_and_redact(text: &str) -> ScanResult {
    struct PM<'d> {
        start: usize,
        length: usize,
        detector: &'d Detector,
    }
    let mut pending: Vec<PM<'_>> = Vec::new();
    for d in DETECTORS {
        for m in d.find_all(text) {
            pending.push(PM {
                start: m.start,
                length: m.length,
                detector: d,
            });
        }
    }
    pending.sort_by(|a, b| a.start.cmp(&b.start).then(b.length.cmp(&a.length)));
    let mut non_overlapping: Vec<PM<'_>> = Vec::new();
    let mut next_free = 0usize;
    for pm in pending {
        if pm.start >= next_free {
            next_free = pm.start + pm.length;
            non_overlapping.push(pm);
        }
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
        let old = pm.length;
        let new_ = repl.len();
        redacted.replace_range(as_..ae, &repl);
        delta += new_ as isize - old as isize;
        findings.push(SecretFinding {
            pattern_id: pm.detector.id,
            offset: pm.start,
            length: pm.length,
        });
    }
    ScanResult { redacted, findings }
}

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

pub fn preprocess_large_file(path: &Path, repo_relative: &str) -> io::Result<PreprocessResult> {
    if is_known_secrets_file(repo_relative) {
        return Ok(PreprocessResult {
            decision: SecretScanDecision::Excluded,
            content: None,
            stream: None,
            findings: Vec::new(),
        });
    }
    // The classify pass computes the BLAKE3 hash alongside the secret scan in
    // a single streaming read.  The returned hash becomes the FileIdentity
    // stored in the stream.  The stream's running hasher verifies at EOF that
    // the bytes it reads match this hash, detecting any modification between
    // the classify pass and actual streaming.
    let (decision, all_findings, classify_hash) = stream_scan_large_file_classify(path)?;
    let identity = FileIdentity {
        content_hash: classify_hash,
    };
    let stream = LargeFileStream::open_with_identity(path, identity)?;
    Ok(PreprocessResult {
        decision,
        content: None,
        stream: Some(stream),
        findings: all_findings,
    })
}

/// Scan a large file for secrets and compute its BLAKE3 content hash in a
/// single streaming pass.
///
/// Returns `(decision, findings, content_hash)`.  The `content_hash` is fed
/// into [`FileIdentity`] so that the subsequent [`LargeFileStream`] can verify
/// it is reading the same bytes without a third full file read.
pub fn stream_scan_large_file_classify(
    path: &Path,
) -> io::Result<(SecretScanDecision, Vec<SecretFinding>, String)> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
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
            if let PemStreamState::Draining {
                begin_file_offset, ..
            } = pem_state
            {
                let len = file_offset_of_new_bytes
                    .saturating_sub(begin_file_offset)
                    .max(1);
                if !all_findings
                    .iter()
                    .any(|f| f.pattern_id == "PK-001" && f.offset == begin_file_offset)
                {
                    all_findings.push(SecretFinding {
                        pattern_id: "PK-001",
                        offset: begin_file_offset,
                        length: len,
                    });
                }
            }
            break;
        }
        // Feed raw bytes into the content hasher before any text processing.
        hasher.update(&new_chunk[..n]);

        window.extend_from_slice(&new_chunk[..n]);
        let window_str = String::from_utf8_lossy(&window).into_owned();
        let window_file_base = file_offset_of_new_bytes.saturating_sub(overlap_len);
        pem_classify_window(
            &window_str,
            window_file_base,
            &mut pem_state,
            &mut all_findings,
        );
        let scan = scan_and_redact(&window_str);
        for f in &scan.findings {
            if f.pattern_id == "PK-001" {
                continue;
            }
            let abs = window_file_base + f.offset;
            let in_new = abs >= file_offset_of_new_bytes;
            let spans = f.offset < overlap_len && f.offset + f.length > overlap_len;
            if (in_new || spans)
                && !all_findings
                    .iter()
                    .any(|e| e.pattern_id == f.pattern_id && e.offset == abs)
            {
                all_findings.push(SecretFinding {
                    pattern_id: f.pattern_id,
                    offset: abs,
                    length: f.length,
                });
            }
        }
        file_offset_of_new_bytes += n;
        let new_slice = &new_chunk[..n];
        overlap_buf = if n > SAFETY_WINDOW_SIZE {
            new_slice[n - SAFETY_WINDOW_SIZE..].to_vec()
        } else {
            new_slice.to_vec()
        };
        if n < STREAM_CHUNK_SIZE {
            if let PemStreamState::Draining {
                begin_file_offset, ..
            } = &pem_state
            {
                let ps = *begin_file_offset;
                let pl = file_offset_of_new_bytes.saturating_sub(ps).max(1);
                if !all_findings
                    .iter()
                    .any(|f| f.pattern_id == "PK-001" && f.offset == ps)
                {
                    all_findings.push(SecretFinding {
                        pattern_id: "PK-001",
                        offset: ps,
                        length: pl,
                    });
                }
            }
            break;
        }
    }
    let decision = if all_findings.is_empty() {
        SecretScanDecision::Safe
    } else {
        SecretScanDecision::Redacted
    };
    let content_hash = hasher.finalize().to_hex().to_string();
    Ok((decision, all_findings, content_hash))
}

fn pem_classify_window(
    window_str: &str,
    window_file_base: usize,
    pem_state: &mut PemStreamState,
    all_findings: &mut Vec<SecretFinding>,
) {
    let mut from = 0usize;
    loop {
        match pem_state {
            PemStreamState::Idle => match find_pem_begin_private_key(window_str, from) {
                None => break,
                Some(begin_pos) => {
                    *pem_state = PemStreamState::Draining {
                        begin_file_offset: window_file_base + begin_pos,
                        tail: Vec::new(),
                    };
                    from = begin_pos + PEM_BEGIN.len();
                }
            },
            PemStreamState::Draining {
                begin_file_offset, ..
            } => match find_pem_end(window_str, from) {
                None => break,
                Some(end_pos) => {
                    let ps = *begin_file_offset;
                    let pl = (window_file_base + end_pos).saturating_sub(ps).max(1);
                    if !all_findings
                        .iter()
                        .any(|f| f.pattern_id == "PK-001" && f.offset == ps)
                    {
                        all_findings.push(SecretFinding {
                            pattern_id: "PK-001",
                            offset: ps,
                            length: pl,
                        });
                    }
                    *pem_state = PemStreamState::Idle;
                    from = end_pos;
                }
            },
        }
    }
}

pub struct LargeFileStream {
    path: PathBuf,
    file: std::fs::File,
    withheld: Vec<u8>,
    withheld_file_offset: usize,
    file_bytes_consumed: usize,
    pem_state: PemStreamState,
    done: bool,
    identity: FileIdentity,
    /// Running BLAKE3 hasher fed with every raw chunk read from `file`.
    /// Finalised at EOF and compared to `identity.content_hash` to detect
    /// any modification between the classify pass and actual streaming.
    hasher: blake3::Hasher,
    /// Guards against calling `verify_hash_at_eof` more than once.
    hash_verified: bool,
}

impl std::fmt::Debug for LargeFileStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LargeFileStream")
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
            path: path.to_path_buf(),
            file,
            withheld: Vec::new(),
            withheld_file_offset: 0,
            file_bytes_consumed: 0,
            pem_state: PemStreamState::Idle,
            done: false,
            identity,
            hasher: blake3::Hasher::new(),
            hash_verified: false,
        })
    }

    /// Finalise the running hasher and compare to the stored identity hash.
    /// An error is returned if the hashes differ, indicating that the file was
    /// modified between the classify pass and the streaming pass.
    fn verify_hash_at_eof(&mut self) -> io::Result<()> {
        if self.hash_verified {
            return Ok(());
        }
        self.hash_verified = true;
        let actual = self.hasher.finalize().to_hex().to_string();
        if actual != self.identity.content_hash {
            return Err(io::Error::other(format!(
                "source identity changed during streaming (unstable capture): {}",
                self.path.display()
            )));
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn withheld_len(&self) -> usize {
        self.withheld.len()
    }

    pub fn next_chunk(&mut self) -> Option<io::Result<StreamChunk>> {
        if self.done && self.withheld.is_empty() {
            if matches!(&self.pem_state, PemStreamState::Draining { .. }) {
                return Some(self.flush_pem_eof());
            }
            // Fully drained: perform final hash integrity check.
            if !self.hash_verified
                && let Err(e) = self.verify_hash_at_eof()
            {
                return Some(Err(e));
            }
            return None;
        }
        if self.done {
            return Some(self.flush_withheld());
        }

        let mut new_buf = vec![0u8; STREAM_CHUNK_SIZE];
        let n = match read_exact_up_to(&mut self.file, &mut new_buf) {
            Ok(n) => n,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        if n == 0 {
            self.done = true;
            if let Err(e) = self.verify_hash_at_eof() {
                return Some(Err(e));
            }
            if matches!(&self.pem_state, PemStreamState::Draining { .. }) {
                return Some(self.flush_pem_eof());
            }
            return if self.withheld.is_empty() {
                None
            } else {
                Some(self.flush_withheld())
            };
        }
        // Feed raw bytes into the running content hasher.
        self.hasher.update(&new_buf[..n]);

        let is_eof = n < STREAM_CHUNK_SIZE;
        if is_eof {
            self.done = true;
            if let Err(e) = self.verify_hash_at_eof() {
                return Some(Err(e));
            }
        }
        self.file_bytes_consumed += n;
        let new_bytes = new_buf[..n].to_vec();

        match self.pem_state.clone() {
            PemStreamState::Draining {
                begin_file_offset,
                tail,
            } => self.process_draining_chunk(&new_bytes, is_eof, begin_file_offset, tail),
            PemStreamState::Idle => self.process_idle_chunk(&new_bytes, is_eof),
        }
    }

    fn process_idle_chunk(
        &mut self,
        new_bytes: &[u8],
        is_eof: bool,
    ) -> Option<io::Result<StreamChunk>> {
        let window_file_base = self.withheld_file_offset;
        let mut window = self.withheld.clone();
        window.extend_from_slice(new_bytes);
        let window_str = String::from_utf8_lossy(&window).into_owned();

        if let Some(bp) = find_pem_begin_private_key(&window_str, 0) {
            return Some(self.handle_pem_begin_in_window(window, window_file_base, bp, is_eof));
        }

        let safe_emit_len = if is_eof {
            window.len()
        } else if window.len() > SAFETY_WINDOW_SIZE {
            window.len() - SAFETY_WINDOW_SIZE
        } else {
            0
        };

        let scan = scan_and_redact(&window_str);
        let mut chunk_findings: Vec<SecretFinding> = Vec::new();
        for f in &scan.findings {
            if f.pattern_id == "PK-001" {
                continue;
            }
            if f.offset < safe_emit_len {
                chunk_findings.push(SecretFinding {
                    pattern_id: f.pattern_id,
                    offset: window_file_base + f.offset,
                    length: f.length,
                });
            }
        }
        let redacted_emit_end = compute_redacted_offset(&scan.findings, &window_str, safe_emit_len);
        let emitted_redacted = scan.redacted[..redacted_emit_end].to_string();
        self.withheld = window[safe_emit_len..].to_vec();
        self.withheld_file_offset = window_file_base + safe_emit_len;
        if emitted_redacted.is_empty() && chunk_findings.is_empty() {
            return None;
        }
        Some(Ok(StreamChunk {
            redacted: emitted_redacted,
            findings: chunk_findings,
        }))
    }

    fn handle_pem_begin_in_window(
        &mut self,
        window: Vec<u8>,
        window_file_base: usize,
        bp: usize,
        is_eof: bool,
    ) -> io::Result<StreamChunk> {
        let pre_pem_str = String::from_utf8_lossy(&window[..bp]).into_owned();
        let pre_scan = scan_and_redact(&pre_pem_str);
        let mut chunk_findings: Vec<SecretFinding> = Vec::new();
        for f in &pre_scan.findings {
            if f.pattern_id != "PK-001" {
                chunk_findings.push(SecretFinding {
                    pattern_id: f.pattern_id,
                    offset: window_file_base + f.offset,
                    length: f.length,
                });
            }
        }
        let mut redacted_output = pre_scan.redacted;
        redacted_output.push_str(PEM_PLACEHOLDER);
        let begin_file_offset = window_file_base + bp;
        let body_start = find_line_end(&window, bp + PEM_BEGIN.len());
        self.pem_state = PemStreamState::Draining {
            begin_file_offset,
            tail: Vec::new(),
        };
        self.withheld.clear();
        self.withheld_file_offset = 0;
        let body_bytes = window[body_start..].to_vec();
        if body_bytes.is_empty() {
            return Ok(StreamChunk {
                redacted: redacted_output,
                findings: chunk_findings,
            });
        }
        match self.process_draining_chunk(&body_bytes, is_eof, begin_file_offset, Vec::new()) {
            Some(Ok(drain_chunk)) => {
                chunk_findings.extend(drain_chunk.findings);
                Ok(StreamChunk {
                    redacted: redacted_output,
                    findings: chunk_findings,
                })
            }
            Some(Err(e)) => Err(e),
            None => Ok(StreamChunk {
                redacted: redacted_output,
                findings: chunk_findings,
            }),
        }
    }

    fn process_draining_chunk(
        &mut self,
        new_bytes: &[u8],
        is_eof: bool,
        begin_file_offset: usize,
        tail: Vec<u8>,
    ) -> Option<io::Result<StreamChunk>> {
        let mut search_window = tail;
        let tail_len = search_window.len();
        search_window.extend_from_slice(new_bytes);
        let search_str = String::from_utf8_lossy(&search_window).into_owned();

        if let Some(end_pos) = find_pem_end(&search_str, 0) {
            let end_line_finish = find_str_line_end_pos(&search_str, end_pos);
            let remainder_start_in_new = end_line_finish.saturating_sub(tail_len);
            let after_end_bytes = new_bytes[remainder_start_in_new.min(new_bytes.len())..].to_vec();
            let bytes_before_end_in_new = end_pos.saturating_sub(tail_len);
            let finding_end_file_offset = self
                .file_bytes_consumed
                .saturating_sub(new_bytes.len())
                .saturating_add(bytes_before_end_in_new);
            let finding_length = finding_end_file_offset
                .saturating_sub(begin_file_offset)
                .max(1);
            let finding = SecretFinding {
                pattern_id: "PK-001",
                offset: begin_file_offset,
                length: finding_length,
            };
            self.pem_state = PemStreamState::Idle;
            self.withheld.clear();
            self.withheld_file_offset = finding_end_file_offset;
            if after_end_bytes.is_empty() {
                return Some(Ok(StreamChunk {
                    redacted: String::new(),
                    findings: vec![finding],
                }));
            }
            match self.process_idle_chunk(&after_end_bytes, is_eof) {
                Some(Ok(mut idle_chunk)) => {
                    idle_chunk.findings.insert(0, finding);
                    Some(Ok(idle_chunk))
                }
                Some(Err(e)) => Some(Err(e)),
                None => Some(Ok(StreamChunk {
                    redacted: String::new(),
                    findings: vec![finding],
                })),
            }
        } else if is_eof {
            let finding_length = self
                .file_bytes_consumed
                .saturating_sub(begin_file_offset)
                .max(1);
            let finding = SecretFinding {
                pattern_id: "PK-001",
                offset: begin_file_offset,
                length: finding_length,
            };
            self.pem_state = PemStreamState::Idle;
            Some(Ok(StreamChunk {
                redacted: String::new(),
                findings: vec![finding],
            }))
        } else {
            let new_tail = if search_window.len() > SAFETY_WINDOW_SIZE {
                search_window[search_window.len() - SAFETY_WINDOW_SIZE..].to_vec()
            } else {
                search_window
            };
            self.pem_state = PemStreamState::Draining {
                begin_file_offset,
                tail: new_tail,
            };
            Some(Ok(StreamChunk {
                redacted: String::new(),
                findings: Vec::new(),
            }))
        }
    }

    fn flush_pem_eof(&mut self) -> io::Result<StreamChunk> {
        if let PemStreamState::Draining {
            begin_file_offset, ..
        } = &self.pem_state
        {
            let bfo = *begin_file_offset;
            let len = self.file_bytes_consumed.saturating_sub(bfo).max(1);
            self.pem_state = PemStreamState::Idle;
            return Ok(StreamChunk {
                redacted: String::new(),
                findings: vec![SecretFinding {
                    pattern_id: "PK-001",
                    offset: bfo,
                    length: len,
                }],
            });
        }
        Ok(StreamChunk {
            redacted: String::new(),
            findings: Vec::new(),
        })
    }

    fn flush_withheld(&mut self) -> io::Result<StreamChunk> {
        let window = std::mem::take(&mut self.withheld);
        if window.is_empty() {
            return Ok(StreamChunk {
                redacted: String::new(),
                findings: Vec::new(),
            });
        }
        let window_file_base = self.withheld_file_offset;
        let window_str = String::from_utf8_lossy(&window).into_owned();
        let scan = scan_and_redact(&window_str);
        let mut findings: Vec<SecretFinding> = Vec::new();
        for f in &scan.findings {
            if f.pattern_id != "PK-001" {
                findings.push(SecretFinding {
                    pattern_id: f.pattern_id,
                    offset: window_file_base + f.offset,
                    length: f.length,
                });
            }
        }
        Ok(StreamChunk {
            redacted: scan.redacted,
            findings,
        })
    }
}

pub fn collect_all(stream: &mut LargeFileStream) -> io::Result<ScanResult> {
    let mut redacted = String::new();
    let mut findings = Vec::new();
    while let Some(result) = stream.next_chunk() {
        let chunk = result?;
        redacted.push_str(&chunk.redacted);
        findings.extend(chunk.findings);
    }
    Ok(ScanResult { redacted, findings })
}

fn read_exact_up_to(file: &mut std::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut total = 0;
    loop {
        match file.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if total == buf.len() {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(total)
}

fn find_pem_begin_private_key(text: &str, from: usize) -> Option<usize> {
    let s = &text[from..];
    let mut search_from = 0;
    while let Some(rel) = s[search_from..].find(PEM_BEGIN) {
        let abs = search_from + rel;
        let after = abs + PEM_BEGIN.len();
        let rest = &s[after..];
        let line_end = rest.find('\n').unwrap_or(rest.len());
        if rest[..line_end].contains(PEM_PRIVATE_KEY_MARKER) {
            return Some(from + abs);
        }
        search_from = abs + 1;
    }
    None
}

fn find_pem_end(text: &str, from: usize) -> Option<usize> {
    text[from..].find(PEM_END).map(|rel| from + rel)
}

fn find_str_line_end_pos(text: &str, pos: usize) -> usize {
    match text[pos..].find('\n') {
        Some(rel) => pos + rel + 1,
        None => text.len(),
    }
}

fn find_line_end(buf: &[u8], from: usize) -> usize {
    match buf[from..].iter().position(|&b| b == b'\n') {
        Some(rel) => from + rel + 1,
        None => buf.len(),
    }
}

fn compute_redacted_offset(
    findings: &[SecretFinding],
    original: &str,
    original_offset: usize,
) -> usize {
    let mut delta: isize = 0;
    for f in findings {
        if f.offset >= original_offset {
            break;
        }
        let end = f.offset + f.length;
        let detector = DETECTORS.iter().find(|d| d.id == f.pattern_id);
        let repl_len = if let Some(d) = detector {
            match d.redact_mode {
                RedactMode::Full => d.placeholder.len(),
                RedactMode::Partial => {
                    let end_clamped = end.min(original.len());
                    partial_redact(&original[f.offset..end_clamped]).len()
                }
            }
        } else {
            f.length
        };
        if end <= original_offset {
            delta += repl_len as isize - f.length as isize;
        } else {
            return (original_offset as isize + delta) as usize;
        }
    }
    (original_offset as isize + delta) as usize
}

fn partial_redact(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let n = chars.len();
    if n <= 8 {
        return "*".repeat(n);
    }
    let keep = (n / 4).min(4);
    let prefix: String = chars[..keep].iter().collect();
    let suffix: String = chars[n - keep..].iter().collect();
    format!("{}{}{}[redacted]", prefix, "*".repeat(n - 2 * keep), suffix)
}

pub(crate) fn is_known_secrets_file(repo_relative: &str) -> bool {
    let lower = repo_relative.to_lowercase();
    let known = [
        ".env",
        ".pem",
        ".key",
        ".p12",
        ".pfx",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
        "id_ed25519",
    ];
    for k in &known {
        if lower.ends_with(k) || lower.contains(&format!("/{}", k)) || lower == *k {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Pattern engine
// ---------------------------------------------------------------------------

struct Match {
    start: usize,
    length: usize,
}

#[derive(Clone, Copy)]
enum RedactMode {
    Full,
    Partial,
}

struct Detector {
    id: &'static str,
    placeholder: &'static str,
    redact_mode: RedactMode,
    find_all: fn(&str) -> Vec<Match>,
}

impl Detector {
    fn find_all(&self, text: &str) -> Vec<Match> {
        (self.find_all)(text)
    }
}

static DETECTORS: &[Detector] = &[
    Detector {
        id: "PK-001",
        placeholder: PEM_PLACEHOLDER,
        redact_mode: RedactMode::Full,
        find_all: find_pem_private_key_matches,
    },
    Detector {
        id: "AWS-001",
        placeholder: "[REDACTED:AWS-KEY]",
        redact_mode: RedactMode::Partial,
        find_all: find_aws_key_matches,
    },
    Detector {
        id: "GH-001",
        placeholder: "[REDACTED:GH-TOKEN]",
        redact_mode: RedactMode::Partial,
        find_all: find_gh_token_matches,
    },
    Detector {
        id: "JWT-001",
        placeholder: "[REDACTED:JWT]",
        redact_mode: RedactMode::Partial,
        find_all: find_jwt_matches,
    },
    Detector {
        id: "HE-001",
        placeholder: "[REDACTED:HE]",
        redact_mode: RedactMode::Partial,
        find_all: find_high_entropy_matches,
    },
];

fn find_pem_private_key_matches(text: &str) -> Vec<Match> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(begin_pos) = find_pem_begin_private_key(text, from) {
        match find_pem_end(text, begin_pos + PEM_BEGIN.len()) {
            None => {
                out.push(Match {
                    start: begin_pos,
                    length: text.len() - begin_pos,
                });
                break;
            }
            Some(end_pos) => {
                let end_of_line = find_str_line_end_pos(text, end_pos);
                out.push(Match {
                    start: begin_pos,
                    length: end_of_line - begin_pos,
                });
                from = end_of_line;
            }
        }
    }
    out
}

fn find_aws_key_matches(text: &str) -> Vec<Match> {
    let mut out = Vec::new();
    let prefixes = ["AKIA", "ASIA", "AROA", "ABIA", "ACCA"];
    let mut from = 0;
    while from < text.len() {
        let mut best: Option<usize> = None;
        for prefix in &prefixes {
            if let Some(pos) = text[from..].find(prefix) {
                let abs = from + pos;
                best = Some(match best {
                    Some(b) => b.min(abs),
                    None => abs,
                });
            }
        }
        let pos = match best {
            Some(p) => p,
            None => break,
        };
        let candidate = &text[pos..];
        let key_len = 20;
        if let Some(key) = candidate.get(..key_len)
            && key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
                let not_continued = candidate.len() == key_len
                    || !candidate
                        .chars()
                        .nth(key_len)
                        .map(|c| c.is_ascii_alphanumeric())
                        .unwrap_or(false);
                if not_continued {
                    out.push(Match {
                        start: pos,
                        length: key_len,
                    });
                    from = pos + key_len;
                    continue;
                }
            }
        from = pos + 1;
    }
    out
}

fn find_gh_token_matches(text: &str) -> Vec<Match> {
    let mut out = Vec::new();
    let prefixes = ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"];
    let mut from = 0;
    while from < text.len() {
        let mut best: Option<usize> = None;
        for prefix in &prefixes {
            if let Some(pos) = text[from..].find(prefix) {
                let abs = from + pos;
                best = Some(match best {
                    Some(b) => b.min(abs),
                    None => abs,
                });
            }
        }
        let pos = match best {
            Some(p) => p,
            None => break,
        };
        let candidate = &text[pos..];
        let suffix_len = 36;
        let total_len = 4 + suffix_len;
        if candidate.len() >= total_len {
            let suffix = &candidate[4..total_len];
            if suffix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                out.push(Match {
                    start: pos,
                    length: total_len,
                });
                from = pos + total_len;
                continue;
            }
        }
        from = pos + 1;
    }
    out
}

fn find_jwt_matches(text: &str) -> Vec<Match> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find("eyJ") {
        let start = from + rel;
        let rest = &text[start..];
        let is_b64url = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=';
        let end = rest
            .find(|c: char| !is_b64url(c) && c != '.')
            .unwrap_or(rest.len());
        let candidate = &rest[..end];
        let parts: Vec<&str> = candidate.splitn(4, '.').collect();
        if parts.len() >= 3 && parts[0].len() >= 4 && parts[1].len() >= 4 && parts[2].len() >= 4 {
            out.push(Match { start, length: end });
            from = start + end;
        } else {
            from = start + 3;
        }
    }
    out
}

fn find_high_entropy_matches(text: &str) -> Vec<Match> {
    let mut out = Vec::new();
    let is_b64 = |c: char| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=';
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if is_b64(bytes[i] as char) {
            let start = i;
            while i < bytes.len() && is_b64(bytes[i] as char) {
                i += 1;
            }
            let token = &text[start..i];
            if token.len() >= 20 && shannon_entropy(token) > 4.5 {
                out.push(Match {
                    start,
                    length: token.len(),
                });
            }
        } else {
            i += 1;
        }
    }
    out
}

fn shannon_entropy(s: &str) -> f64 {
    let mut freq = [0u32; 256];
    for b in s.bytes() {
        freq[b as usize] += 1;
    }
    let len = s.len() as f64;
    freq.iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // -----------------------------------------------------------------------
    // scan_and_redact — unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn scan_empty_string() {
        let r = scan_and_redact("");
        assert!(r.redacted.is_empty());
        assert!(r.findings.is_empty());
    }

    #[test]
    fn scan_no_secrets() {
        let text = "Hello, world! No secrets here.";
        let r = scan_and_redact(text);
        assert_eq!(r.redacted, text);
        assert!(r.findings.is_empty());
    }

    #[test]
    fn scan_pem_private_key_redacted() {
        let pem =
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n";
        let r = scan_and_redact(pem);
        assert!(r.redacted.contains(PEM_PLACEHOLDER));
        assert!(!r.redacted.contains("MIIEowIBAAKCAQEA"));
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].pattern_id, "PK-001");
    }

    #[test]
    fn scan_pem_public_key_not_redacted() {
        let pem = "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n-----END PUBLIC KEY-----\n";
        let r = scan_and_redact(pem);
        assert!(!r.redacted.contains(PEM_PLACEHOLDER));
        assert!(r.findings.is_empty());
    }

    #[test]
    fn scan_aws_key_detected() {
        let text = "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let r = scan_and_redact(text);
        assert!(!r.findings.is_empty());
        assert!(r.findings.iter().any(|f| f.pattern_id == "AWS-001"));
    }

    #[test]
    fn scan_gh_token_detected() {
        let text = "token: ghp_abcdefghijklmnopqrstuvwxyz1234567890ab";
        let r = scan_and_redact(text);
        assert!(r.findings.iter().any(|f| f.pattern_id == "GH-001"));
    }

    #[test]
    fn scan_jwt_detected() {
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let r = scan_and_redact(jwt);
        assert!(r.findings.iter().any(|f| f.pattern_id == "JWT-001"));
    }

    #[test]
    fn scan_redacted_text_does_not_contain_raw_token() {
        let text = "token: ghp_abcdefghijklmnopqrstuvwxyz1234567890ab end";
        let r = scan_and_redact(text);
        assert!(
            !r.redacted
                .contains("ghp_abcdefghijklmnopqrstuvwxyz1234567890ab")
        );
    }

    #[test]
    fn scan_multiple_findings_non_overlapping() {
        let text = "AKIAIOSFODNN7EXAMPLE and ghp_abcdefghijklmnopqrstuvwxyz1234567890ab";
        let r = scan_and_redact(text);
        assert!(r.findings.len() >= 2);
        let mut sorted = r.findings.clone();
        sorted.sort_by_key(|f| f.offset);
        for i in 1..sorted.len() {
            assert!(
                sorted[i].offset >= sorted[i - 1].offset + sorted[i - 1].length,
                "findings overlap at indices {}/{}",
                i - 1,
                i
            );
        }
    }

    #[test]
    fn no_raw_secret_value_in_findings() {
        let text = "ghp_abcdefghijklmnopqrstuvwxyz1234567890ab";
        let r = scan_and_redact(text);
        for f in &r.findings {
            let raw_slice = &text[f.offset..f.offset + f.length];
            assert!(
                !r.redacted.contains(raw_slice),
                "raw secret value '{}' found verbatim in redacted output",
                raw_slice
            );
        }
    }

    // -----------------------------------------------------------------------
    // preprocess — unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn preprocess_safe_file() {
        let r = preprocess("no secrets", "src/main.rs");
        assert_eq!(r.decision, SecretScanDecision::Safe);
        assert!(r.findings.is_empty());
    }

    #[test]
    fn preprocess_excluded_file() {
        let r = preprocess("anything", ".env");
        assert_eq!(r.decision, SecretScanDecision::Excluded);
        assert!(r.content.is_none());
    }

    #[test]
    fn preprocess_redacted_file() {
        let text = "key: AKIAIOSFODNN7EXAMPLE";
        let r = preprocess(text, "config.yaml");
        assert_eq!(r.decision, SecretScanDecision::Redacted);
        assert!(!r.findings.is_empty());
    }

    #[test]
    fn preprocess_pem_file_extension_excluded() {
        let r = preprocess(
            "-----BEGIN RSA PRIVATE KEY-----\n-----END RSA PRIVATE KEY-----\n",
            "cert.pem",
        );
        assert_eq!(r.decision, SecretScanDecision::Excluded);
    }

    // -----------------------------------------------------------------------
    // is_known_secrets_file
    // -----------------------------------------------------------------------

    #[test]
    fn known_secrets_file_detection() {
        assert!(is_known_secrets_file(".env"));
        assert!(is_known_secrets_file("secrets/.env"));
        assert!(is_known_secrets_file("id_rsa"));
        assert!(is_known_secrets_file("keys/id_rsa"));
        assert!(is_known_secrets_file("cert.pem"));
        assert!(is_known_secrets_file("keystore.p12"));
        assert!(!is_known_secrets_file("src/main.rs"));
        assert!(!is_known_secrets_file("README.md"));
    }

    // -----------------------------------------------------------------------
    // FileSizeTier classification
    // -----------------------------------------------------------------------

    #[test]
    fn file_size_tier_small() {
        assert_eq!(classify_file_size(0), FileSizeTier::Small);
        assert_eq!(
            classify_file_size(SMALL_FILE_THRESHOLD),
            FileSizeTier::Small
        );
    }

    #[test]
    fn file_size_tier_large() {
        assert_eq!(
            classify_file_size(SMALL_FILE_THRESHOLD + 1),
            FileSizeTier::Large
        );
        assert_eq!(
            classify_file_size(VERY_LARGE_FILE_THRESHOLD),
            FileSizeTier::Large
        );
    }

    #[test]
    fn file_size_tier_very_large() {
        assert_eq!(
            classify_file_size(VERY_LARGE_FILE_THRESHOLD + 1),
            FileSizeTier::VeryLarge
        );
    }

    // -----------------------------------------------------------------------
    // LargeFileStream — basic streaming
    // -----------------------------------------------------------------------

    fn tmp_file(content: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn large_file_stream_no_secrets() {
        let data = b"hello world no secrets here";
        let f = tmp_file(data);
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        let result = collect_all(&mut stream).unwrap();
        assert!(result.findings.is_empty());
        assert_eq!(
            result.redacted.trim_end_matches('\0'),
            "hello world no secrets here"
        );
    }

    #[test]
    fn large_file_stream_pem_single_chunk() {
        let pem =
            b"-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n";
        let f = tmp_file(pem);
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        let result = collect_all(&mut stream).unwrap();
        assert!(result.findings.iter().any(|f| f.pattern_id == "PK-001"));
        assert!(!result.redacted.contains("MIIEowIBAAKCAQEA"));
        assert!(result.redacted.contains(PEM_PLACEHOLDER));
    }

    #[test]
    fn large_file_stream_aws_key() {
        let data = b"export AWS_KEY=AKIAIOSFODNN7EXAMPLE done";
        let f = tmp_file(data);
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        let result = collect_all(&mut stream).unwrap();
        assert!(result.findings.iter().any(|f| f.pattern_id == "AWS-001"));
    }

    #[test]
    fn large_file_stream_empty_file() {
        let f = tmp_file(b"");
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        let result = collect_all(&mut stream).unwrap();
        assert!(result.findings.is_empty());
        assert!(result.redacted.is_empty());
    }

    #[test]
    fn large_file_stream_pem_spans_two_chunks() {
        let header = b"-----BEGIN RSA PRIVATE KEY-----\n";
        let body = vec![b'A'; STREAM_CHUNK_SIZE];
        let footer = b"\n-----END RSA PRIVATE KEY-----\n";
        let mut data = Vec::new();
        data.extend_from_slice(header);
        data.extend_from_slice(&body);
        data.extend_from_slice(footer);
        let f = tmp_file(&data);
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        let result = collect_all(&mut stream).unwrap();
        assert!(
            result.findings.iter().any(|f| f.pattern_id == "PK-001"),
            "PEM spanning chunks must be detected"
        );
        assert!(
            !result.redacted.contains("AAAA"),
            "PEM body must not appear in redacted output"
        );
    }

    #[test]
    fn large_file_stream_pem_many_chunks_buffer_is_bounded() {
        let header = b"-----BEGIN RSA PRIVATE KEY-----\n";
        let body_len = STREAM_CHUNK_SIZE * 10;
        let body = vec![b'B'; body_len];
        let footer = b"\n-----END RSA PRIVATE KEY-----\n";
        let mut data = Vec::new();
        data.extend_from_slice(header);
        data.extend_from_slice(&body);
        data.extend_from_slice(footer);
        let f = tmp_file(&data);
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        loop {
            assert!(
                stream.withheld_len() <= SAFETY_WINDOW_SIZE,
                "withheld buffer exceeded SAFETY_WINDOW_SIZE: {} > {}",
                stream.withheld_len(),
                SAFETY_WINDOW_SIZE
            );
            match stream.next_chunk() {
                None => break,
                Some(Ok(_)) => {}
                Some(Err(e)) => panic!("unexpected error: {}", e),
            }
        }
        assert!(stream.withheld_len() <= SAFETY_WINDOW_SIZE);
    }

    // -----------------------------------------------------------------------
    // Source identity enforcement — BLAKE3 hash-based, no escape hatches
    // -----------------------------------------------------------------------

    /// Supplying a stale (pre-modification) FileIdentity to open_with_identity
    /// and then streaming MUST return an error.  The BLAKE3 hash of the
    /// original content will not match the hash of the modified content that
    /// the stream actually reads, so detection is guaranteed regardless of
    /// filesystem mtime resolution.
    #[test]
    fn large_file_modification_between_classify_and_open_detected() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"original content for blake3 identity test")
            .unwrap();
        f.flush().unwrap();

        // Capture identity of the original file.
        let id_before = file_identity(f.path()).unwrap();

        // Overwrite with different content — BLAKE3 hash will differ.
        {
            let mut fh = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .unwrap();
            fh.write_all(b"completely different modified content xyz")
                .unwrap();
            fh.flush().unwrap();
        }

        // Open with the stale identity and drain the stream.
        let mut stream = LargeFileStream::open_with_identity(f.path(), id_before).unwrap();
        let mut got_error = false;
        loop {
            match stream.next_chunk() {
                Some(Err(e)) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("source identity changed") || msg.contains("unstable capture"),
                        "unexpected error message: {}",
                        msg
                    );
                    got_error = true;
                    break;
                }
                Some(Ok(_)) => continue,
                None => break,
            }
        }
        assert!(
            got_error,
            "streaming a file with a stale BLAKE3 identity MUST return an error; \
             no fallback is acceptable — the content hashes are different"
        );
    }

    /// Modifying a file between the first and second next_chunk calls MUST
    /// result in an error by EOF.  With BLAKE3 identity the check fires at
    /// EOF when the accumulated hash diverges from the classify-pass hash.
    #[test]
    fn large_file_modification_during_streaming_returns_error() {
        // File must be larger than one chunk to force multiple next_chunk calls.
        let initial_data = vec![b'X'; STREAM_CHUNK_SIZE + 1024];
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&initial_data).unwrap();
        f.flush().unwrap();

        let mut stream = LargeFileStream::open(f.path()).unwrap();

        // First chunk — must succeed (file unmodified at this point).
        match stream.next_chunk() {
            Some(Ok(_)) => {}
            Some(Err(e)) => panic!("first chunk should succeed: {}", e),
            None => panic!("expected at least one chunk"),
        }

        // Overwrite the file with different content while the stream is open.
        // The running BLAKE3 hasher will accumulate bytes from the new content,
        // producing a final hash that differs from the classify-pass hash.
        {
            let mut fh = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(f.path())
                .unwrap();
            fh.write_all(b"completely different shorter content")
                .unwrap();
            fh.flush().unwrap();
        }

        // Drain remaining chunks — the error MUST appear by EOF.
        let mut got_error = false;
        for _ in 0..10 {
            match stream.next_chunk() {
                Some(Err(e)) => {
                    let msg = e.to_string();
                    assert!(
                        msg.contains("source identity changed") || msg.contains("unstable capture"),
                        "unexpected error: {}",
                        msg
                    );
                    got_error = true;
                    break;
                }
                Some(Ok(_)) => continue,
                None => break,
            }
        }
        assert!(
            got_error,
            "modifying a file during streaming MUST produce an error by EOF; \
             BLAKE3 identity is not subject to mtime-resolution races"
        );
    }

    // -----------------------------------------------------------------------
    // FileIdentity — BLAKE3 property tests
    // -----------------------------------------------------------------------

    #[test]
    fn file_identity_same_content_same_hash() {
        let f1 = tmp_file(b"identical content");
        let f2 = tmp_file(b"identical content");
        let id1 = file_identity(f1.path()).unwrap();
        let id2 = file_identity(f2.path()).unwrap();
        assert_eq!(id1, id2, "same content must produce identical FileIdentity");
    }

    #[test]
    fn file_identity_different_content_different_hash() {
        let f1 = tmp_file(b"content A");
        let f2 = tmp_file(b"content B");
        let id1 = file_identity(f1.path()).unwrap();
        let id2 = file_identity(f2.path()).unwrap();
        assert_ne!(
            id1, id2,
            "different content must produce different FileIdentity"
        );
    }

    #[test]
    fn file_identity_hash_is_64_hex_chars() {
        let f = tmp_file(b"some file content");
        let id = file_identity(f.path()).unwrap();
        assert_eq!(
            id.content_hash.len(),
            64,
            "BLAKE3 hex hash must be 64 characters"
        );
        assert!(
            id.content_hash.chars().all(|c| c.is_ascii_hexdigit()),
            "BLAKE3 hash must contain only hex digits"
        );
    }

    #[test]
    fn file_identity_empty_file_has_known_length_hash() {
        let f = tmp_file(b"");
        let id = file_identity(f.path()).unwrap();
        assert_eq!(id.content_hash.len(), 64);
    }

    // -----------------------------------------------------------------------
    // shannon_entropy
    // -----------------------------------------------------------------------

    #[test]
    fn entropy_uniform_string_high() {
        let s = "aB3dE6gH9jKlMnOpQrStUvWxYz012345";
        assert!(
            shannon_entropy(s) > 4.0,
            "entropy should be > 4.0 for varied string"
        );
    }

    #[test]
    fn entropy_repeated_char_low() {
        let s = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert!(
            shannon_entropy(s) < 0.01,
            "entropy of repeated char should be near 0"
        );
    }

    // -----------------------------------------------------------------------
    // partial_redact
    // -----------------------------------------------------------------------

    #[test]
    fn partial_redact_short_token() {
        let r = partial_redact("abcd");
        assert_eq!(r, "****");
    }

    #[test]
    fn partial_redact_long_token() {
        let r = partial_redact("AKIAIOSFODNN7EXAMPLE");
        assert!(r.contains("[redacted]"));
        assert!(r.starts_with("AKIA"));
    }

    // -----------------------------------------------------------------------
    // stream_scan_large_file_classify
    // -----------------------------------------------------------------------

    #[test]
    fn classify_safe_file() {
        let f = tmp_file(b"no secrets here at all");
        let (decision, findings, hash) = stream_scan_large_file_classify(f.path()).unwrap();
        assert_eq!(decision, SecretScanDecision::Safe);
        assert!(findings.is_empty());
        assert_eq!(hash.len(), 64, "classify must return a 64-char BLAKE3 hash");
    }

    #[test]
    fn classify_file_with_pem() {
        let data =
            b"-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA\n-----END RSA PRIVATE KEY-----\n";
        let f = tmp_file(data);
        let (decision, findings, hash) = stream_scan_large_file_classify(f.path()).unwrap();
        assert_eq!(decision, SecretScanDecision::Redacted);
        assert!(findings.iter().any(|f| f.pattern_id == "PK-001"));
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn classify_file_with_aws_key() {
        let data = b"key: AKIAIOSFODNN7EXAMPLE end";
        let f = tmp_file(data);
        let (decision, findings, _hash) = stream_scan_large_file_classify(f.path()).unwrap();
        assert_eq!(decision, SecretScanDecision::Redacted);
        assert!(findings.iter().any(|f| f.pattern_id == "AWS-001"));
    }

    #[test]
    fn classify_hash_matches_file_identity() {
        // The hash returned by classify must equal the hash from file_identity
        // (both use the same BLAKE3 algorithm over the same raw bytes).
        let data = b"some content without secrets";
        let f = tmp_file(data);
        let (_, _, classify_hash) = stream_scan_large_file_classify(f.path()).unwrap();
        let id = file_identity(f.path()).unwrap();
        assert_eq!(
            classify_hash, id.content_hash,
            "classify hash must equal file_identity hash for the same file"
        );
    }

    // -----------------------------------------------------------------------
    // pem_classify_window
    // -----------------------------------------------------------------------

    #[test]
    fn pem_classify_window_finds_begin_end() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nbody\n-----END RSA PRIVATE KEY-----\n";
        let mut state = PemStreamState::Idle;
        let mut findings = Vec::new();
        pem_classify_window(text, 0, &mut state, &mut findings);
        assert!(findings.iter().any(|f| f.pattern_id == "PK-001"));
        assert!(matches!(state, PemStreamState::Idle));
    }

    #[test]
    fn pem_classify_window_unclosed_stays_draining() {
        let text = "-----BEGIN RSA PRIVATE KEY-----\nbody without end";
        let mut state = PemStreamState::Idle;
        let mut findings = Vec::new();
        pem_classify_window(text, 0, &mut state, &mut findings);
        assert!(matches!(state, PemStreamState::Draining { .. }));
    }

    // -----------------------------------------------------------------------
    // collect_all round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn collect_all_produces_same_as_scan_and_redact_for_small_content() {
        let text = "no secrets in this file content at all, just normal text.";
        let f = tmp_file(text.as_bytes());
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        let streamed = collect_all(&mut stream).unwrap();
        let direct = scan_and_redact(text);
        assert_eq!(
            streamed.redacted.trim_end_matches('\0'),
            direct.redacted.trim_end_matches('\0')
        );
    }

    #[test]
    fn collect_all_with_gh_token() {
        let text = b"auth: ghp_abcdefghijklmnopqrstuvwxyz1234567890ab ok";
        let f = tmp_file(text);
        let mut stream = LargeFileStream::open(f.path()).unwrap();
        let result = collect_all(&mut stream).unwrap();
        assert!(result.findings.iter().any(|f| f.pattern_id == "GH-001"));
        assert!(
            !result
                .redacted
                .contains("ghp_abcdefghijklmnopqrstuvwxyz1234567890ab")
        );
    }

    // -----------------------------------------------------------------------
    // preprocess_large_file
    // -----------------------------------------------------------------------

    #[test]
    fn preprocess_large_file_excluded() {
        let f = tmp_file(b"secret content");
        let result = preprocess_large_file(f.path(), "private.pem").unwrap();
        assert_eq!(result.decision, SecretScanDecision::Excluded);
        assert!(result.stream.is_none());
    }

    #[test]
    fn preprocess_large_file_safe_produces_stream() {
        let f = tmp_file(b"safe content with no secrets at all");
        let result = preprocess_large_file(f.path(), "data.bin").unwrap();
        assert_eq!(result.decision, SecretScanDecision::Safe);
        assert!(result.stream.is_some());
    }

    #[test]
    fn preprocess_large_file_redacted_produces_stream() {
        let data = b"key: AKIAIOSFODNN7EXAMPLE end";
        let f = tmp_file(data);
        let result = preprocess_large_file(f.path(), "config.yaml").unwrap();
        assert_eq!(result.decision, SecretScanDecision::Redacted);
        assert!(result.stream.is_some());
    }

    // -----------------------------------------------------------------------
    // AWS key boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn aws_key_prefix_variants_detected() {
        for prefix in &["AKIA", "ASIA", "AROA", "ABIA", "ACCA"] {
            let key = format!("{}IOSFODNN7EXAMPLE", prefix);
            let text = format!("key={}", key);
            let r = scan_and_redact(&text);
            assert!(
                r.findings.iter().any(|f| f.pattern_id == "AWS-001"),
                "prefix {} not detected",
                prefix
            );
        }
    }

    #[test]
    fn aws_key_too_short_not_detected() {
        let text = "AKIA123456789";
        let r = scan_and_redact(text);
        assert!(!r.findings.iter().any(|f| f.pattern_id == "AWS-001"));
    }

    #[test]
    fn aws_key_continued_alphanumeric_not_detected() {
        let text = "AKIAIOSFODNN7EXAMPLE1";
        let r = scan_and_redact(text);
        assert!(!r.findings.iter().any(|f| f.pattern_id == "AWS-001"));
    }

    // -----------------------------------------------------------------------
    // GitHub token tests
    // -----------------------------------------------------------------------

    #[test]
    fn gh_token_prefix_variants_detected() {
        for prefix in &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let token = format!("{}abcdefghijklmnopqrstuvwxyz1234567890ab", prefix);
            let r = scan_and_redact(&token);
            assert!(
                r.findings.iter().any(|f| f.pattern_id == "GH-001"),
                "prefix {} not detected",
                prefix
            );
        }
    }

    // -----------------------------------------------------------------------
    // JWT tests
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_three_part_detected() {
        let jwt =
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let r = scan_and_redact(jwt);
        assert!(r.findings.iter().any(|f| f.pattern_id == "JWT-001"));
    }

    #[test]
    fn jwt_two_parts_not_detected() {
        let text = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1c2VyIn0";
        let r = scan_and_redact(text);
        assert!(
            !r.findings.iter().any(|f| f.pattern_id == "JWT-001"),
            "two-part eyJ string should not be detected as JWT"
        );
    }
}
