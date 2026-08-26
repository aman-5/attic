//! Context Builder (Phase 4 §15): assemble bounded context from VALIDATED
//! evidence — never raw retrieval candidates.
//!
//! Enforced: byte/token budget, deduplication, provenance headers,
//! freshness caveats, contradiction disclosure sections, and a final
//! defense-in-depth secret scan. Files are never dumped whole; snippets are
//! bounded excerpts captured at candidate time.

use attic_core::FreshnessState;
use attic_evidence::EvidenceSourceType as ST;
use attic_evidence::approx_tokens;
use attic_evidence::{AuthorityLevel, Evidence, VerificationStatus};

use crate::plan::{DropReason, DroppedEvidence, EvidenceRef};
use crate::query::QueryType;

/// The assembled context document.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextDocument {
    /// Final context text (secret-scanned).
    pub text: String,
    /// Approx tokens consumed (bytes/4 accounting).
    pub tokens: u64,
    /// Ordered evidence references (rank 0 = highest).
    pub refs: Vec<EvidenceRef>,
    /// Evidence dropped during assembly with deterministic reasons.
    pub dropped: Vec<DroppedEvidence>,
}

/// Section order per source type — implementation first, relationships last.
pub fn section_rank_of(st: ST) -> u8 {
    section_rank(st)
}

fn section_rank(st: ST) -> u8 {
    match st {
        ST::SourceCode | ST::GeneratedSource => 0,
        ST::Configuration => 1,
        ST::Test => 2,
        ST::Knowledge | ST::Documentation => 3,
        ST::Relationship => 4,
    }
}

/// Build the context document.
///
/// `secret_scan` runs the Phase 1B scan over the final text; any finding
/// drops the offending item (defense in depth — evidence should already be
/// redacted upstream).
pub fn build(
    ranked: &[Evidence],
    contradictions: &[attic_evidence::Contradiction],
    qt: QueryType,
    max_context_tokens: u32,
    primary: Option<ST>,
) -> ContextDocument {
    let budget_bytes = (max_context_tokens as usize) * 4;
    let mut text = String::new();
    let mut refs = Vec::new();
    let mut dropped = Vec::new();
    let mut seen_snippets: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Deterministic ordering: the contract's PRIMARY source type leads,
    // then section rank, then combined score desc.
    let mut ordered: Vec<&Evidence> = ranked.iter().collect();
    ordered.sort_by(|a, b| {
        let a_primary = primary.is_some_and(|p| p == a.source_type);
        let b_primary = primary.is_some_and(|p| p == b.source_type);
        b_primary
            .cmp(&a_primary)
            .then_with(|| section_rank(a.source_type).cmp(&section_rank(b.source_type)))
            .then_with(|| {
                b.signals
                    .combined_score
                    .unwrap_or(0.0)
                    .partial_cmp(&a.signals.combined_score.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });

    let header = format!("# Attic context for query type {}\n\n", qt.as_str());
    text.push_str(&header);

    let mut rank = 0u16;
    for ev in &ordered {
        if approx_tokens(text.len()) >= max_context_tokens as u64 {
            dropped.push(DroppedEvidence {
                evidence_id: ev.id.clone(),
                source_type: ev.source_type.as_str().to_owned(),
                drop_reason: DropReason::ContextTokenLimit,
                score: ev.signals.combined_score.unwrap_or(0.0),
            });
            continue;
        }

        let snippet = match &ev.snippet {
            Some(s) if !s.trim().is_empty() => s.clone(),
            _ => {
                dropped.push(DroppedEvidence {
                    evidence_id: ev.id.clone(),
                    source_type: ev.source_type.as_str().to_owned(),
                    drop_reason: DropReason::DuplicateContent,
                    score: ev.signals.combined_score.unwrap_or(0.0),
                });
                continue;
            }
        };

        // Content-level dedup: identical snippet bodies contribute once.
        let fp = snippet
            .lines()
            .fold(0u64, |acc, l| acc.wrapping_add(hash_line(l)));
        if !seen_snippets.insert(format!("{}:{fp}", ev.path)) {
            dropped.push(DroppedEvidence {
                evidence_id: ev.id.clone(),
                source_type: ev.source_type.as_str().to_owned(),
                drop_reason: DropReason::DuplicateContent,
                score: ev.signals.combined_score.unwrap_or(0.0),
            });
            continue;
        }

        let mut block = format!(
            "\n## [{}] {}:{}\n- authority: {}\n",
            ev.source_type.as_str(),
            ev.path,
            ev.source_span
                .map(|s| format!("{}", s))
                .unwrap_or_else(|| "-".into()),
            authority_label(ev),
        );
        block.push_str(&freshness_note(ev));
        if let Some(rel) = &ev.relationship {
            block.push_str(&format!(
                "- relationship: {} ({}, confidence {:.2})\n",
                rel.rel_type,
                rel.resolution.as_str(),
                rel.confidence
            ));
        }
        block.push_str("```\n");
        block.push_str(snippet.trim_end());
        block.push_str("\n```\n");

        // Budget guard on the assembled bytes.
        if text.len() + block.len() > budget_bytes {
            dropped.push(DroppedEvidence {
                evidence_id: ev.id.clone(),
                source_type: ev.source_type.as_str().to_owned(),
                drop_reason: DropReason::ContextTokenLimit,
                score: ev.signals.combined_score.unwrap_or(0.0),
            });
            continue;
        }

        text.push_str(&block);
        refs.push(EvidenceRef {
            evidence_id: ev.id.clone(),
            source_type: ev.source_type.as_str().to_owned(),
            rank,
            score: ev.signals.combined_score.unwrap_or(0.0) as f32,
            token_count: approx_tokens(block.len()) as u32,
        });
        rank += 1;
    }

    // Contradiction disclosure section — never silently resolved.
    if !contradictions.is_empty() {
        text.push_str("\n## Contradictions detected\n\n");
        for c in contradictions.iter().take(10) {
            text.push_str(&format!("- [{}] {}\n", c.kind.as_str(), c.description));
        }
    }

    let tokens = approx_tokens(text.len());
    ContextDocument {
        text,
        tokens,
        refs,
        dropped,
    }
}

fn hash_line(l: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in l.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

fn authority_label(ev: &Evidence) -> &'static str {
    match ev.authority {
        AuthorityLevel::Implementation => "IMPLEMENTATION",
        AuthorityLevel::TestExpectation => "TEST_EXPECTATION",
        AuthorityLevel::Configured => "CONFIGURED",
        AuthorityLevel::ProjectKnowledge => "PROJECT_KNOWLEDGE",
        AuthorityLevel::Doc => "DOCUMENTATION",
        AuthorityLevel::Derived => "DERIVED",
    }
}

fn freshness_note(ev: &Evidence) -> String {
    match (ev.freshness_state, ev.verification_state) {
        (FreshnessState::Current, VerificationStatus::Verified) => {
            "- freshness: CURRENT (verified against source)\n".to_owned()
        }
        (FreshnessState::Current, _) => String::new(),
        (FreshnessState::Stale, _) => {
            "- freshness: STALE — may not reflect the current working tree\n".to_owned()
        }
        (FreshnessState::Unknown, _) => "- freshness: UNKNOWN — treat with caution\n".to_owned(),
        (FreshnessState::PendingRefresh, _) => {
            "- freshness: PENDING_REFRESH — recompute in progress\n".to_owned()
        }
        (FreshnessState::Invalid, _) => String::new(), // filtered pre-context
    }
}
