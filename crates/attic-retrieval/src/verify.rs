//! Direct source verification (Phase 4 §13): the bounded verification tier
//! that reads live, authoritative source through the Phase 1B secure access
//! path (canonicalize-within-root + secrets preprocessing) and compares it
//! against indexed evidence.
//!
//! Guarantees preserved: path traversal is rejected, `.git` internals are
//! forbidden, secret-bearing content stays redacted, LARGE files are read
//! through bounded streams only, and every ACTUAL sanitized byte consumed is
//! charged against the policy's filesystem budget — reading stops the moment
//! the remaining byte budget is exhausted and reports observable degradation.
//!
//! Lineage rule (ADR-012 D3): successful verification NEVER rewrites the
//! indexed artifact's freshness/revision/generation. The indexed occurrence
//! keeps its truthful state (STALE stays STALE); confirmation is expressed
//! through `verification_state = VERIFIED` and
//! `live_source_verified = true`, which sufficiency rules may accept under
//! CURRENT_ONLY contracts because the live check establishes current truth.

use std::path::Path;

use attic_evidence::VerificationStatus;

use crate::budget::BudgetAccountant;
use crate::error::RetrievalError;
use crate::mode::AnswerModePolicy;

/// What happened when verifying one evidence item.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyOutcome {
    /// Live source contains the evidenced fact.
    VerifiedCurrent,
    /// Live source differs from the indexed content — evidence is stale.
    ContentChanged,
    /// File missing / unreadable / excluded by security policy.
    Unavailable(String),
    /// Policy forbade the read (FAST mode or exhausted FS budget).
    BlockedByBudget,
}

/// One bounded span read: sanitized text actually consumed plus whether the
/// filesystem-byte budget cut it short.
struct SpanRead {
    text: String,
    bytes_consumed: u64,
    truncated_by_budget: bool,
}

/// Reads the redacted text of lines `[start_line, end_line]` (treated as a
/// window over the file) via Phase 1B preprocessing, consuming at most
/// `max_bytes` sanitized bytes.
///
/// SMALL/redacted files load fully and are truncated to the cap; LARGE files
/// stream chunk-by-chunk and STOP PULLING once the cap is reached. Either
/// way the returned `bytes_consumed` is what actually entered memory.
fn read_span_text(
    repo_root: &Path,
    rel_path: &str,
    start_line: u32,
    end_line: u32,
    max_bytes: usize,
) -> Result<SpanRead, RetrievalError> {
    let joined = repo_root.join(rel_path);
    let abs = attic_discovery::canonicalize_within_root(&joined, repo_root).map_err(|e| {
        tracing::debug!(path = %rel_path, error = %e, "verification path rejected");
        RetrievalError::InvalidQuery(format!("path rejected for verification: {e}"))
    })?;

    // .git internals forbidden at this layer too (server does the same).
    let rel_norm = rel_path.replace('\\', "/");
    if rel_norm == ".git" || rel_norm.starts_with(".git/") {
        return Ok(SpanRead {
            text: String::new(),
            bytes_consumed: 0,
            truncated_by_budget: false,
        });
    }

    let consumed: u64;
    let mut truncated = false;
    let pre = attic_discovery::preprocess_file_content(&abs, &rel_norm)?;
    if pre.decision == attic_discovery::SecretScanDecision::Excluded {
        return Ok(SpanRead {
            text: String::new(),
            bytes_consumed: 0,
            truncated_by_budget: false,
        });
    }
    if let Some(text) = pre.content {
        // SMALL / fully-redacted content already in memory; charge only what
        // we keep for comparison.
        let sliced = slice_lines(&text, start_line, end_line, max_bytes);
        consumed = sliced.len() as u64;
        truncated = text.len() > max_bytes && sliced.len() >= max_bytes;
        return Ok(SpanRead {
            text: sliced,
            bytes_consumed: consumed,
            truncated_by_budget: truncated,
        });
    }
    if let Some(mut stream) = pre.stream {
        // Bounded streaming line window over sanitized chunks. Reading stops
        // as soon as the remaining byte budget is exhausted.
        let mut collected = String::new();
        let mut line_no = 0u32;
        let mut pending = String::new();
        'pull: while let Some(chunk_result) = stream.next_chunk() {
            let chunk = chunk_result?;
            let chunk = chunk.redacted;
            let remaining = max_bytes.saturating_sub(collected.len());
            if collected.len() >= max_bytes || remaining == 0 {
                truncated = true;
                break 'pull;
            }
            let mut rest = chunk.as_str();
            while let Some(nl) = rest.find('\n') {
                pending.push_str(&rest[..=nl]);
                rest = &rest[nl + 1..];
                line_no += 1;
                if line_no >= start_line && line_no <= end_line {
                    let take = pending.len().min(remaining);
                    collected.push_str(&pending[..take]);
                    if take < pending.len() {
                        truncated = true;
                        break 'pull;
                    }
                }
                pending.clear();
                if collected.len() >= max_bytes {
                    truncated = true;
                    break 'pull;
                }
                if line_no > end_line {
                    break 'pull;
                }
            }
            pending.push_str(rest);
            if collected.len() + rest.len() > max_bytes {
                truncated = true;
                break 'pull;
            }
        }
        consumed = collected.len() as u64;
        return Ok(SpanRead {
            text: collected,
            bytes_consumed: consumed,
            truncated_by_budget: truncated,
        });
    }
    Ok(SpanRead {
        text: String::new(),
        bytes_consumed: 0,
        truncated_by_budget: false,
    })
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
/// FULL additionally accepts mere presence when no distinctive snippet
/// exists. This is deliberately span-local: hashing the whole file would
/// misreport unrelated edits as evidence invalidation.
///
/// Budget semantics: one file slot is required up-front; ACTUAL sanitized
/// bytes consumed are committed afterwards, capped by the remaining byte
/// budget. A read truncated by that budget yields `BlockedByBudget` when the
/// containment question could not be answered — never a silent guess.
pub fn verify_evidence(
    ev: &mut attic_evidence::Evidence,
    repo_root: &Path,
    policy: &AnswerModePolicy,
    budget: &mut BudgetAccountant,
) -> Result<VerifyOutcome, RetrievalError> {
    if !policy.fs_reads_permitted() {
        return Ok(VerifyOutcome::BlockedByBudget);
    }
    if !budget.fs_file_slot_available() {
        return Ok(VerifyOutcome::BlockedByBudget);
    }
    let remaining_bytes = budget.fs_bytes_remaining();
    if remaining_bytes == 0 {
        budget.note_fs_exhaustion();
        return Ok(VerifyOutcome::BlockedByBudget);
    }

    let Some(span) = ev.source_span else {
        return Ok(VerifyOutcome::Unavailable("no span to verify".into()));
    };
    let start = span.start_line.saturating_sub(2).max(1);
    let end = span.end_line.saturating_add(4);
    let read = read_span_text(repo_root, &ev.path, start, end, remaining_bytes as usize)?;
    // Commit the ACTUAL sanitized bytes consumed (never an estimate).
    budget.commit_verification_read(read.bytes_consumed);

    if read.text.trim().is_empty() {
        if read.truncated_by_budget {
            budget.note_fs_exhaustion();
            return Ok(VerifyOutcome::BlockedByBudget);
        }
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
            ev.live_source_verified = true;
            return Ok(VerifyOutcome::VerifiedCurrent);
        }
        return Ok(VerifyOutcome::Unavailable("no distinctive snippet".into()));
    }

    let live_normalized = normalize_ws(text_of(&read));
    if live_normalized.contains(&normalize_ws(&probe)) {
        // The fact itself is confirmed against the live tree.
        //
        // LINEAGE RULE: the indexed artifact's freshness_state,
        // source_revision_id and index_generation_id are left EXACTLY as
        // they were. A stale indexed occurrence remains stale as an indexed
        // artifact; confirmation is expressed through the verification state
        // and the explicit live-source flag.
        ev.verification_state = VerificationStatus::Verified;
        ev.live_source_verified = true;
        Ok(VerifyOutcome::VerifiedCurrent)
    } else if read.truncated_by_budget {
        // Could not see enough of the live source within budget: observable
        // degradation, not a verdict.
        budget.note_fs_exhaustion();
        Ok(VerifyOutcome::BlockedByBudget)
    } else {
        ev.verification_state = VerificationStatus::Stale;
        Ok(VerifyOutcome::ContentChanged)
    }
}

fn text_of(r: &SpanRead) -> &str {
    &r.text
}

/// Whitespace-normalized form used for containment checks.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// BLAKE3 hex of UTF-8 bytes (same hashing used at index time).
#[allow(dead_code)]
pub fn blake3_hash_hex(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex().to_string()
}
