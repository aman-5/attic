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
    /// Secret-scanned sections retained for lossless workspace compaction.
    pub sections: Vec<ContextSection>,
    pub prefix: String,
    pub suffix: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextSection {
    pub header: String,
    pub body: String,
}

/// Share only byte-identical bodies; retain every provenance header and disclosure.
pub fn compact_documents(documents: &[ContextDocument]) -> String {
    let mut prefixes = Vec::new();
    let mut suffixes = Vec::new();
    let mut sections: Vec<ContextSection> = Vec::new();
    let mut bodies = std::collections::HashMap::new();
    let mut shared_counts = Vec::new();
    for doc in documents {
        if !doc.prefix.is_empty() && !prefixes.contains(&doc.prefix.as_str()) {
            prefixes.push(doc.prefix.as_str());
        }
        for section in &doc.sections {
            if let Some(&index) = bodies.get(section.body.as_str()) {
                let shared: &mut ContextSection = &mut sections[index];
                shared.header.push_str(&section.header);
                shared_counts[index] += 1;
            } else {
                bodies.insert(section.body.as_str(), sections.len());
                sections.push(section.clone());
                shared_counts.push(1usize);
            }
        }
        if !doc.suffix.is_empty() {
            suffixes.push(doc.suffix.as_str());
        }
    }
    let mut text = prefixes.join("\n");
    for (section, count) in sections.into_iter().zip(shared_counts) {
        text.push_str(&section.header);
        if count > 1 {
            text.push_str("\nShared excerpt for all citations above:\n");
        }
        text.push_str(&section.body);
    }
    for suffix in suffixes {
        text.push_str(suffix);
    }
    text
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
    // The contradiction disclosure is mandatory, so its bytes are reserved
    // up front — evidence blocks fill only what is left, and the finished
    // document never exceeds the budget. Lines that cannot fit at all are
    // cut, with a count of what was omitted.
    let repositories = ranked
        .iter()
        .map(|ev| ev.repository_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    let scope = format!("Repositories: {repositories}");
    let mut disclosure = contradiction_disclosure(
        contradictions,
        (budget_bytes / 2).saturating_sub(scope.len() + 2),
    );
    if !disclosure.is_empty() {
        disclosure = format!("\n{scope}\n{disclosure}");
    }
    let block_budget = budget_bytes.saturating_sub(disclosure.len());
    let mut text = String::new();
    let mut refs = Vec::new();
    let mut dropped = Vec::new();
    let mut seen_snippets = std::collections::HashSet::new();
    let mut sections = Vec::new();

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

    let header = format!(
        "# Attic context for query type {}\n{scope}\n\n",
        qt.as_str()
    );
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

        // Order-sensitive equality, scoped to the same source occurrence.
        if !seen_snippets.insert((
            ev.repository_id.clone(),
            ev.path.clone(),
            ev.source_id.clone(),
            ev.source_revision_id.clone(),
            ev.workspace_snapshot_id.clone(),
            ev.excerpt_span
                .or(ev.source_span)
                .map(|span| span.to_string()),
            snippet.clone(),
        )) {
            dropped.push(DroppedEvidence {
                evidence_id: ev.id.clone(),
                source_type: ev.source_type.as_str().to_owned(),
                drop_reason: DropReason::DuplicateContent,
                score: ev.signals.combined_score.unwrap_or(0.0),
            });
            continue;
        }

        let mut block = format!(
            "\n## [{}] {}:{}\n- repository_id: {}\n- evidence_id: {}\n- authority: {}\n",
            ev.source_type.as_str(),
            ev.path,
            ev.excerpt_span
                .or(ev.source_span)
                .map(|s| format!("{}", s))
                .unwrap_or_else(|| "-".into()),
            ev.repository_id,
            ev.id,
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
        let section = ContextSection {
            header: block.clone(),
            body: format!(
                "{}{}{}",
                if ev.snippet_leading_omitted {
                    "[earlier source omitted]\n"
                } else {
                    ""
                },
                format_args!("```\n{}\n```\n", snippet.trim_end()),
                if ev.snippet_trailing_omitted {
                    "[later source omitted]\n"
                } else {
                    ""
                }
            ),
        };
        block.push_str(&section.body);

        // ── SECRET-SAFETY PASS (per item, before inclusion) ────────────────
        // The approved Phase 1B detector runs over the fully-assembled BLOCK
        // (provenance header + snippet). Any finding drops the ENTIRE item:
        // the block never enters `text`, no EvidenceRef is created for it,
        // and the drop is recorded as SECRET_CONTENT_DETECTED so accounting
        // stays truthful. Raw secrets can therefore never ride into context,
        // claims (claims must cite served refs), or MCP responses — even if
        // upstream redaction layers were bypassed.
        let block_scan = attic_discovery::secrets::scan_and_redact(&block);
        if !block_scan.findings.is_empty() {
            dropped.push(DroppedEvidence {
                evidence_id: ev.id.clone(),
                source_type: ev.source_type.as_str().to_owned(),
                drop_reason: DropReason::SecretContentDetected,
                score: ev.signals.combined_score.unwrap_or(0.0),
            });
            continue;
        }

        // Budget guard on the assembled bytes.
        if text.len() + block.len() > block_budget {
            dropped.push(DroppedEvidence {
                evidence_id: ev.id.clone(),
                source_type: ev.source_type.as_str().to_owned(),
                drop_reason: DropReason::ContextTokenLimit,
                score: ev.signals.combined_score.unwrap_or(0.0),
            });
            continue;
        }

        text.push_str(&block);
        sections.push(section);
        refs.push(EvidenceRef {
            evidence_id: ev.id.clone(),
            source_type: ev.source_type.as_str().to_owned(),
            rank,
            score: ev.signals.combined_score.unwrap_or(0.0) as f32,
            token_count: approx_tokens(block.len()) as u32,
        });
        rank += 1;
    }

    // Contradiction disclosure section - never silently resolved.
    text.push_str(&disclosure);

    // ── SECRET-SAFETY PASS (fail-closed, whole document) ───────────────────
    // Defense in depth: every block was already scanned individually; this
    // final pass covers header/disclosure assembly. If ANYTHING is still
    // flagged, the builder refuses to serve assembled content at all: every
    // served ref is demoted to a recorded SECRET_CONTENT_DETECTED drop, the
    // refs list is emptied (so claims cannot cite withheld support), and
    // only the skeleton + disclosure remain. Token accounting stays exact
    // because it is computed AFTER this decision.
    let final_scan = attic_discovery::secrets::scan_and_redact(&text);
    if !final_scan.findings.is_empty() {
        tracing::warn!(
            findings = final_scan.findings.len(),
            "secret-safety pass flagged assembled context; withholding all blocks"
        );
        for r in &refs {
            dropped.push(DroppedEvidence {
                evidence_id: r.evidence_id.clone(),
                source_type: r.source_type.clone(),
                drop_reason: DropReason::SecretContentDetected,
                score: r.score as f64,
            });
        }
        refs.clear();
        sections.clear();
        text.clear();
        text.push_str(&header);
        text.push_str("[content withheld by secret-safety policy]\n");
        text.push_str(&disclosure);
    }

    let tokens = approx_tokens(text.len());
    ContextDocument {
        prefix: if sections.is_empty() {
            text.clone()
        } else {
            header
        },
        suffix: if sections.is_empty() {
            String::new()
        } else {
            disclosure
        },
        text,
        tokens,
        refs,
        dropped,
        sections,
    }
}

/// Contradiction disclosure text (at most 10 entries) within `max_bytes`.
fn contradiction_disclosure(
    contradictions: &[attic_evidence::Contradiction],
    max_bytes: usize,
) -> String {
    if contradictions.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n## Contradictions detected\n\n");
    let shown = contradictions.iter().take(10).collect::<Vec<_>>();
    for (i, c) in shown.iter().enumerate() {
        let line = format!("- [{}] {}\n", c.kind.as_str(), c.description);
        let omitted = format!("- … {} more omitted (context budget)\n", shown.len() - i);
        if out.len() + line.len() + omitted.len() > max_bytes {
            if out.len() + omitted.len() <= max_bytes {
                out.push_str(&omitted);
            }
            break;
        }
        out.push_str(&line);
    }
    out
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
    if ev.live_source_verified && ev.freshness_state != FreshnessState::Current {
        // Lineage-honest caveat: the INDEXED artifact keeps its stale state;
        // the fact was separately confirmed against live source this query.
        return format!(
            "- freshness: {} as indexed - fact VERIFIED against current source this query\n",
            ev.freshness_state.as_str()
        );
    }

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
        (FreshnessState::Invalid, _) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(id: &str, repo: &str, path: &str, snippet: &str) -> Evidence {
        let mut ev = Evidence::new(id, repo);
        ev.path = path.into();
        ev.snippet = Some(snippet.into());
        ev.signals.combined_score = Some(0.9);
        ev.source_span = Some(attic_core::SourceSpan::new(1, 0, 3, 0));
        ev
    }

    #[test]
    fn compaction_shares_bodies_without_losing_citations_or_differences() {
        let body = format!("fn shared() {{\n{}\n}}\n", "    run();\n".repeat(80));
        let other = "fn different() { stop(); }";
        let inputs = [
            evidence("ev-one", "repo-one", "src/shared.rs", &body),
            evidence("ev-two", "repo-two", "lib/copied.rs", &body),
            evidence("ev-three", "repo-three", "lib/different.rs", other),
        ];
        let docs = inputs
            .iter()
            .map(|ev| {
                build(
                    std::slice::from_ref(ev),
                    &[],
                    QueryType::GenericSearch,
                    16_384,
                    None,
                )
            })
            .collect::<Vec<_>>();
        let original_bytes: usize = docs.iter().map(|doc| doc.text.len()).sum();
        let compact = compact_documents(&docs);
        assert_eq!(compact.matches("fn shared()").count(), 1);
        assert_eq!(compact.matches(other).count(), 1);
        for ev in inputs {
            assert!(compact.contains(&ev.id));
            assert!(compact.contains(&ev.repository_id));
            assert!(compact.contains(&ev.path));
        }
        assert!(compact.len() < original_bytes);
        assert_eq!(docs.iter().map(|doc| doc.refs.len()).sum::<usize>(), 3);
    }

    #[test]
    fn reordered_lines_are_not_duplicates() {
        let one = evidence("ev-one", "repo", "lib.rs", "first();\nsecond();");
        let two = evidence("ev-two", "repo", "lib.rs", "second();\nfirst();");
        let doc = build(&[one, two], &[], QueryType::GenericSearch, 16_384, None);
        assert_eq!(doc.refs.len(), 2);
        assert!(doc.dropped.is_empty());
        assert!(compact_documents(std::slice::from_ref(&doc)).contains("second();\nfirst();"));
    }

    #[test]
    fn compaction_preserves_same_path_in_different_repositories_and_spans() {
        let one = evidence("ev-one", "repo-one", "lib.rs", "run();");
        let mut two = evidence("ev-two", "repo-two", "lib.rs", "run();");
        two.source_span = Some(attic_core::SourceSpan::new(10, 0, 12, 0));
        let doc = build(&[one, two], &[], QueryType::GenericSearch, 16_384, None);
        assert_eq!(doc.refs.len(), 2);
        let compact = compact_documents(std::slice::from_ref(&doc));
        for citation in ["repo-one", "repo-two", "1:0-3:0", "10:0-12:0"] {
            assert!(compact.contains(citation), "missing {citation}");
        }
        assert_eq!(compact.matches("run();").count(), 1);
    }

    #[test]
    fn compaction_keeps_disclosures_and_cannot_reintroduce_withheld_secrets() {
        let secret = evidence(
            "ev-secret",
            "repo",
            "config",
            "aws_access_key_id = \"AKIAIOSFODNN7EXAMPLE\"",
        );
        let benign = evidence("ev-benign", "repo", "lib.rs", "fn safe() {}");
        let doc = build(
            &[secret, benign],
            &[],
            QueryType::GenericSearch,
            16_384,
            None,
        );
        let mut doc = doc;
        doc.suffix
            .push_str("\n## Contradictions detected\n- implementations disagree\n");
        let compact = compact_documents(std::slice::from_ref(&doc));
        assert!(!compact.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(compact.contains("implementations disagree"));
        assert_eq!(doc.refs.len(), 1);
    }

    #[test]
    fn identical_content_keeps_distinct_revision_provenance() {
        let mut one = evidence("ev-one", "repo", "lib.rs", "run();");
        let mut two = evidence("ev-two", "repo", "lib.rs", "run();");
        one.source_revision_id = Some("revision-one".into());
        two.source_revision_id = Some("revision-two".into());
        let doc = build(&[one, two], &[], QueryType::GenericSearch, 16_384, None);
        assert_eq!(doc.refs.len(), 2);
        let compact = compact_documents(&[doc]);
        assert!(compact.contains("ev-one"));
        assert!(compact.contains("ev-two"));
    }

    #[test]
    fn shared_excerpt_and_omissions_are_explicit() {
        let mut one = evidence("ev-one", "repo-one", "lib.rs", "payment();");
        one.snippet_leading_omitted = true;
        one.snippet_trailing_omitted = true;
        one.excerpt_span = Some(attic_core::SourceSpan::new(8, 120, 8, 130));
        let mut two = one.clone();
        two.id = "ev-two".into();
        two.repository_id = "repo-two".into();
        let doc = build(&[one, two], &[], QueryType::GenericSearch, 16_384, None);
        let compact = compact_documents(&[doc]);
        assert!(compact.contains("Shared excerpt for all citations above"));
        assert!(compact.contains("[earlier source omitted]"));
        assert!(compact.contains("[later source omitted]"));
        assert!(compact.contains("8:120-8:130"));
        assert_eq!(compact.matches("payment();").count(), 1);
    }

    #[test]
    fn disclosures_and_identical_withheld_notices_keep_repository_scope() {
        let mut docs = Vec::new();
        for repo in ["repo-one", "repo-two"] {
            let ev = evidence("ev", repo, "lib.rs", "run();");
            let contradiction = attic_evidence::Contradiction {
                kind: attic_evidence::ContradictionKind::ConflictingValues,
                evidence_a: "ev".into(),
                evidence_b: "other".into(),
                description: "lib.rs differs".into(),
            };
            let mut doc = build(
                &[ev],
                &[contradiction],
                QueryType::GenericSearch,
                16_384,
                None,
            );
            assert!(doc.suffix.contains(&format!("Repositories: {repo}")));
            assert!(doc.suffix.contains("lib.rs differs"));
            doc.sections.clear();
            doc.prefix
                .push_str("[content withheld by secret-safety policy]\n");
            docs.push(doc);
        }
        let compact = compact_documents(&docs);
        assert_eq!(
            compact
                .matches("[content withheld by secret-safety policy]")
                .count(),
            2
        );
        for repo in ["repo-one", "repo-two"] {
            assert!(compact.contains(&format!(
                "Repositories: {repo}\n\n## Contradictions detected"
            )));
        }
    }
}
