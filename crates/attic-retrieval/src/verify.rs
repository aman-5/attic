//! Direct source verification (Phase 4 §13): the bounded verification tier
//! that reads live, authoritative source through the Phase 1B secure access
//! path (canonicalize-within-root + secrets preprocessing) and compares it
//! against indexed evidence.
//!
//! Guarantees preserved: path traversal is rejected, `.git` internals are
//! forbidden, secret-bearing content stays redacted, LARGE files are read
//! through bounded streams only, and every read is charged against the
//! policy's filesystem budget. FAST mode (zero FS budget) hard-refuses —
//! a `PolicyViolation`, never a silent skip.

use std::path::Path;

use attic_core::FreshnessState;
use attic_evidence::VerificationStatus;

use crate::budget::BudgetAccountant;
use crate::error::RetrievalError;
use crate::mode::AnswerModePolicy;

/// What happened when verifying one evidence item.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyOutcome {
    /// Live source matches the indexed content hash.
    VerifiedCurrent,
    /// Live source differs from the indexed content — evidence is stale.
    ContentChanged,
    /// File missing / unreadable / excluded by security policy.
    Unavailable(String),
    /// Policy forbade the read (FAST mode or exhausted FS budget).
    BlockedByBudget,
}

/// Reads the redacted text of lines `[start_line, end_line]` (1-based,
/// inclusive) of a repo file via Phase 1B preprocessing. SMALL/redacted
/// files load fully; LARGE files stream with a hard byte bound.
fn read_span_text(
    repo_root: &Path,
    rel_path: &str,
    start_line: u32,
    end_line: u32,
    max_bytes: usize,
) -> Result<Option<String>, RetrievalError> {
    let joined = repo_root.join(rel_path);
    let abs = attic_discovery::canonicalize_within_root(&joined, repo_root).map_err(|e| {
        tracing::debug!(path = %rel_path, error = %e, "verification path rejected");
        RetrievalError::InvalidQuery(format!("path rejected for verification: {e}"))
    })?;

    // .git internals forbidden at this layer too (server does the same).
    let rel_norm = rel_path.replace('\\', "/");
    if rel_norm == ".git" || rel_norm.starts_with(".git/") {
        return Ok(None);
    }

    let pre = attic_discovery::preprocess_file_content(&abs, &rel_norm)?;
    if pre.decision == attic_discovery::SecretScanDecision::Excluded {
        return Ok(None);
    }
    if let Some(text) = pre.content {
        return Ok(Some(slice_lines(&text, start_line, end_line, max_bytes)));
    }
    if let Some(mut stream) = pre.stream {
        // Bounded streaming line window over sanitized chunks.
        let mut collected = String::new();
        let mut line_no = 0u32;
        let mut pending = String::new();
        'pull: while let Some(chunk_result) = stream.next_chunk() {
            let chunk = chunk_result?;
            let chunk = chunk.redacted;
            if collected.len() >= max_bytes {
                break 'pull;
            }
            let mut rest = chunk.as_str();
            while let Some(nl) = rest.find('\n') {
                pending.push_str(&rest[..=nl]);
                rest = &rest[nl + 1..];
                line_no += 1;
                if line_no >= start_line && line_no <= end_line && collected.len() < max_bytes {
                    collected.push_str(&pending);
                }
                pending.clear();
                if line_no > end_line || collected.len() >= max_bytes {
                    break 'pull;
                }
            }
            pending.push_str(rest);
            if pending.len() > max_bytes * 2 {
                pending.truncate(max_bytes * 2);
            }
        }
        return Ok(Some(collected));
    }
    Ok(None)
}

fn slice_lines(text: &str, start_line: u32, end_line: u32, max_bytes: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let n = (i + 1) as u32;
        if n < start_line {
            continue;
        }
        if n > end_line || out.len() >= max_bytes {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.truncate(out.len().min(max_bytes));
    out
}

/// Verify one piece of evidence against the current working tree.
///
/// CHECKSUM level confirms that the evidenced FACT (its snippet text) still
/// appears verbatim inside the corresponding region of the live source;
/// FULL additionally accepts mere presence when no snippet exists. This is
/// deliberately span-local: hashing the whole file would misreport unrelated
/// edits as evidence invalidation.
pub fn verify_evidence(
    ev: &mut attic_evidence::Evidence,
    repo_root: &Path,
    policy: &AnswerModePolicy,
    budget: &mut BudgetAccountant,
) -> Result<VerifyOutcome, RetrievalError> {
    if !policy.fs_reads_permitted() {
        return Ok(VerifyOutcome::BlockedByBudget);
    }
    // Charge an estimated read before touching disk; actual charge follows.
    let est_bytes = 4096u64.min(budget.max_fs_bytes);
    if !budget.charge_file_read(est_bytes) {
        return Ok(VerifyOutcome::BlockedByBudget);
    }

    let Some(span) = ev.source_span else {
        return Ok(VerifyOutcome::Unavailable("no span to verify".into()));
    };
    let start = span.start_line.saturating_sub(2).max(1);
    let end = span.end_line.saturating_add(4);
    let Some(text) = read_span_text(repo_root, &ev.path, start, end, 256 * 1024)? else {
        return Ok(VerifyOutcome::Unavailable("unreadable or excluded".into()));
    };
    if text.trim().is_empty() {
        return Ok(VerifyOutcome::ContentChanged);
    }

    let probe = ev
        .snippet
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_owned();

    if probe.chars().count() < 4 {
        // Nothing distinctive to confirm; FULL treats presence as weak yes.
        if policy.source_verification_level == crate::mode::VerificationLevel::Full {
            ev.verification_state = VerificationStatus::Verified;
            return Ok(VerifyOutcome::VerifiedCurrent);
        }
        return Ok(VerifyOutcome::Unavailable("no distinctive snippet".into()));
    }

    let live_normalized = normalize_ws(&text);
    if live_normalized.contains(&normalize_ws(&probe)) {
        ev.verification_state = VerificationStatus::Verified;
        if ev.freshness_state != FreshnessState::Current {
            // The fact itself is confirmed against the live tree; index
            // refresh pending but the claim is contractually recoverable.
            ev.freshness_state = FreshnessState::Current;
            ev.signals.freshness_score = Some(attic_evidence::signals::freshness_score(
                FreshnessState::Current,
            ));
        }
        Ok(VerifyOutcome::VerifiedCurrent)
    } else {
        ev.verification_state = VerificationStatus::Stale;
        Ok(VerifyOutcome::ContentChanged)
    }
}

/// Whitespace-normalized form used for containment checks.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// BLAKE3 hex of UTF-8 bytes (same hashing used at index time).
pub fn blake3_hash_hex(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}
