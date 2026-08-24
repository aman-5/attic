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
//! | HE-001   | High-entropy base64 string ≥ 20  | partial|
//!
//! "Full" redaction replaces the entire matched region with a redaction
//! placeholder.  "Partial" redaction replaces all but the first 4 characters
//! with `***`.
//!
//! **Nothing that matches a secret pattern may be persisted to storage,
//! passed to FTS, or written to any log at level < ERROR.**

// ---------------------------------------------------------------------------
// Design note: we use simple byte-level pattern matching to avoid pulling in
// a regex crate at this stage.  The patterns are conservative (may produce
//  false-positives) which is intentional — better to over-redact than leak.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

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
    /// Pattern identifier (e.g. `"PK-001"`).
    pub pattern_id: &'static str,
    /// Zero-based byte offset of the match start in the **original** text.
    pub offset: usize,
    /// Length (in bytes) of the matched region in the original text.
    pub length: usize,
}

/// Whether the file should be excluded from all downstream processing because
/// it is likely a secrets file (e.g. `.env`, `*.pem`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretScanDecision {
    /// Content is safe to index (after redaction if needed).
    Safe,
    /// Content contains secrets; redacted version is available.
    Redacted,
    /// File must not be indexed at all; it is a known secrets carrier.
    Excluded,
}

/// Full output of preprocessing one file's content.
#[derive(Debug, Clone)]
pub struct PreprocessResult {
    pub decision: SecretScanDecision,
    /// Redacted content (present for `Safe` and `Redacted` decisions).
    pub content: Option<String>,
    pub findings: Vec<SecretFinding>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Scan and redact secrets in `text`.
///
/// Returns a [`ScanResult`] containing the redacted text and a list of
/// findings.  The redacted text is safe to pass to FTS / embeddings.
pub fn scan_and_redact(text: &str) -> ScanResult {
    // Collect all matches from all detectors against the *original* text.
    // Each match carries a reference to its detector so we can build the
    // replacement in the application phase.
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
    let mut next_free = 0usize; // first byte not covered by a previously accepted match
    for pm in pending {
        if pm.start >= next_free {
            next_free = pm.start + pm.length;
            non_overlapping.push(pm);
        }
        // else: this match overlaps a higher-priority (earlier/longer) one; skip.
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

/// Preprocess file content: decide whether to index, exclude, or redact.
///
/// `repo_relative` is used solely to make path-based decisions (e.g. `.env`
/// file extension); it is never stored or logged with secret content.
pub fn preprocess(content: &str, repo_relative: &str) -> PreprocessResult {
    if is_known_secrets_file(repo_relative) {
        return PreprocessResult {
            decision: SecretScanDecision::Excluded,
            content: None,
            findings: Vec::new(),
        };
    }

    let result = scan_and_redact(content);
    if result.findings.is_empty() {
        PreprocessResult {
            decision: SecretScanDecision::Safe,
            content: Some(result.redacted),
            findings: Vec::new(),
        }
    } else {
        PreprocessResult {
            decision: SecretScanDecision::Redacted,
            content: Some(result.redacted),
            findings: result.findings,
        }
    }
}

/// Returns `true` for files that should never be indexed regardless of their
/// content (mirrors the security-exclusion list in `security.rs`).
pub fn is_known_secrets_file(repo_relative: &str) -> bool {
    let name = repo_relative
        .rsplit('/')
        .next()
        .unwrap_or(repo_relative)
        .to_ascii_lowercase();

    // Exact names
    if matches!(name.as_str(), ".env" | ".netrc" | ".npmrc" | "id_rsa" | "id_ed25519" | "id_ecdsa") {
        return true;
    }

    // Prefix patterns
    if name.starts_with(".env.") {
        return true;
    }

    // Extension patterns
    if let Some(ext) = name.rsplit('.').next()
        && matches!(ext, "pem" | "key" | "p12" | "jks" | "pfx" | "p8")
    {
        return true;
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

// ── PK-001: PEM private key blocks ─────────────────────────────────────────

const PEM_BEGIN: &str = "-----BEGIN";
const PEM_END: &str = "-----END";

fn find_pem_private_keys(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let mut search_from = 0;

    while search_from < text.len() {
        // Find "-----BEGIN"
        match text[search_from..].find(PEM_BEGIN) {
            None => break,
            Some(rel) => {
                let begin_pos = search_from + rel;
                // Check the header contains "PRIVATE KEY" or "RSA PRIVATE KEY"
                let header_end = match text[begin_pos..].find('\n') {
                    Some(n) => begin_pos + n,
                    None => break,
                };
                let header = &text[begin_pos..header_end];
                if !header.contains("PRIVATE KEY") {
                    search_from = begin_pos + PEM_BEGIN.len();
                    continue;
                }
                // Find matching -----END
                match text[header_end..].find(PEM_END) {
                    None => break,
                    Some(rel_end) => {
                        let end_block_start = header_end + rel_end;
                        // Find newline after -----END ... -----
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

// ── AWS-001: AWS access key IDs (AKIA… 20 chars) ───────────────────────────

fn find_aws_access_keys(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let bytes = text.as_bytes();
    let prefix = b"AKIA";

    let mut i = 0;
    while i + 20 <= bytes.len() {
        if bytes[i..i + 4] == *prefix {
            // Following 16 chars must be uppercase alphanumeric.
            if bytes[i + 4..i + 20]
                .iter()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                // Preceded and followed by word boundary (not alphanumeric).
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

// ── GH-001: GitHub personal access tokens (ghp_… / ghs_… / github_pat_…) ──

fn find_github_tokens(text: &str) -> Vec<RawMatch> {
    let prefixes: &[&str] = &["ghp_", "ghs_", "gho_", "ghu_", "ghr_", "github_pat_"];
    let mut matches = Vec::new();

    for prefix in prefixes {
        let mut search_from = 0;
        while let Some(rel) = text[search_from..].find(prefix) {
            let start = search_from + rel;
            let rest = &text[start + prefix.len()..];
            // Consume alphanumeric / underscore / hyphen characters.
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

// ── JWT-001: JSON Web Tokens (three base64url segments separated by `.`) ───

fn find_jwt_tokens(text: &str) -> Vec<RawMatch> {
    let mut matches = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // A JWT starts with a base64url segment.
        if is_base64url_char(bytes[i]) {
            let seg1_end = advance_base64url(bytes, i);
            if seg1_end < len && bytes[seg1_end] == b'.' {
                let seg2_start = seg1_end + 1;
                let seg2_end = advance_base64url(bytes, seg2_start);
                if seg2_end < len && bytes[seg2_end] == b'.' {
                    let seg3_start = seg2_end + 1;
                    let seg3_end = advance_base64url(bytes, seg3_start);
                    // All three segments must be non-empty and total length ≥ 40.
                    let total = seg3_end - i;
                    if (seg1_end > i) && (seg2_end > seg2_start) && (seg3_end > seg3_start) && total >= 40 {
                        matches.push(RawMatch { start: i, length: total });
                        i = seg3_end;
                        continue;
                    }
                }
            }
        }
        i += 1;
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

// ── HE-001: High-entropy base64 strings (≥ 20 chars) ──────────────────────
//
// We approximate entropy by requiring the string to contain uppercase,
// lowercase, and digit characters (characteristic of random base64), and
// be at least 20 characters long.

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
    // Exclude '=' — it is valid base64 padding only at the end of a string,
    // but it also appears widely as an assignment/query operator in source code.
    // Including it causes `key=<SECRET>` to merge into a single candidate,
    // which prevents more-specific detectors (e.g. AWS-001) from matching.
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

    // ── is_known_secrets_file ──────────────────────────────────────────────

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

    // ── PEM private key detection ──────────────────────────────────────────

    #[test]
    fn detects_pem_private_key() {
        let text = "config=yes\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAK...\n-----END RSA PRIVATE KEY-----\ndone";
        let result = scan_and_redact(text);
        assert!(
            !result.findings.is_empty(),
            "should detect PEM private key"
        );
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
        // Public keys must NOT be matched by PK-001.
        assert!(
            !result.findings.iter().any(|f| f.pattern_id == "PK-001"),
            "public keys must not be redacted"
        );
    }

    // ── AWS access key ─────────────────────────────────────────────────────

    #[test]
    fn detects_aws_access_key() {
        let text = "key=AKIAIOSFODNN7EXAMPLE end";
        let result = scan_and_redact(text);
        let aws: Vec<_> = result.findings.iter().filter(|f| f.pattern_id == "AWS-001").collect();
        assert!(!aws.is_empty(), "should detect AWS access key");
        // Partial redact: first 4 chars kept.
        assert!(result.redacted.contains("AKIA"), "first 4 chars must survive partial redaction");
        assert!(!result.redacted.contains("AKIAIOSFODNN7EXAMPLE"), "full key must be gone");
    }

    // ── GitHub token ───────────────────────────────────────────────────────

    #[test]
    fn detects_github_token() {
        let text = "token: ghp_16C7e42F292c6912E7710c838347Ae178B4a";
        let result = scan_and_redact(text);
        let gh: Vec<_> = result.findings.iter().filter(|f| f.pattern_id == "GH-001").collect();
        assert!(!gh.is_empty(), "should detect GitHub token");
    }

    // ── JWT ────────────────────────────────────────────────────────────────

    #[test]
    fn detects_jwt() {
        // Typical JWT header.payload.signature
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let text = format!("Authorization: Bearer {jwt}");
        let result = scan_and_redact(&text);
        let jwt_findings: Vec<_> = result.findings.iter().filter(|f| f.pattern_id == "JWT-001").collect();
        assert!(!jwt_findings.is_empty(), "should detect JWT");
    }

    // ── scan_and_redact on clean content ───────────────────────────────────

    #[test]
    fn clean_content_unchanged() {
        let text = "fn main() { println!(\"hello\"); }";
        let result = scan_and_redact(text);
        // High-entropy detector might match some base64-like sequences in code;
        // but for this short clean snippet it should not.
        assert_eq!(result.redacted, text);
        assert!(result.findings.is_empty());
    }

    // ── preprocess ─────────────────────────────────────────────────────────

    #[test]
    fn preprocess_excludes_env_file() {
        let result = preprocess("SECRET=abc123", ".env");
        assert_eq!(result.decision, SecretScanDecision::Excluded);
        assert!(result.content.is_none());
    }

    #[test]
    fn preprocess_safe_for_clean_code() {
        let result = preprocess("fn main() {}", "src/main.rs");
        assert_eq!(result.decision, SecretScanDecision::Safe);
        assert!(result.content.is_some());
    }

    #[test]
    fn preprocess_redacted_for_secret_content() {
        let text = "key: AKIAIOSFODNN7EXAMPLE";
        let result = preprocess(text, "src/config.rs");
        assert_eq!(result.decision, SecretScanDecision::Redacted);
        assert!(result.content.is_some());
        assert!(!result.findings.is_empty());
    }

    // ── partial_redact ─────────────────────────────────────────────────────

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
}
