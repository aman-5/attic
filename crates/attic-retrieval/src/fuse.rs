//! Candidate fusion (Phase 4 §8): deterministic merge of candidates
//! discovered through multiple retrievers. The same source discovered via
//! lexical + symbol + structural routes fuses into ONE evidence item with
//! all contributing signals preserved — duplicate discovery never multiplies
//! into fake independent support.

use std::collections::HashMap;

use crate::candidates::{Candidate, RetrieverKind};

/// Fuse candidates by their evidence `fusion_key`.
///
/// Deterministic: stable input order → stable output order; merged items
/// keep the highest-confidence skeleton and the union of retrieval sources,
/// and every signal dimension keeps its best observed value.
pub fn fuse(candidates: Vec<Candidate>) -> Vec<crate::candidates::Candidate> {
    let mut by_key: HashMap<(String, String, u32), usize> = HashMap::new();
    let mut fused: Vec<Vec<Candidate>> = Vec::new();

    for c in candidates {
        let key = c.evidence.fusion_key();
        match by_key.get(&key) {
            Some(&idx) => fused[idx].push(c),
            None => {
                fused.push(vec![c]);
                by_key.insert(key, fused.len() - 1);
            }
        }
    }

    let mut out = Vec::with_capacity(fused.len());
    for mut group in fused {
        // Deterministic representative: highest confidence, then lowest id.
        group.sort_by(|a, b| {
            b.evidence
                .confidence
                .partial_cmp(&a.evidence.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.evidence.id.cmp(&b.evidence.id))
        });
        let mut rep = group.swap_remove(0).evidence;
        for other in &group {
            rep.signals.merge_max(&other.evidence.signals);
            for src in &other.evidence.retrieval_sources {
                if !rep
                    .retrieval_sources
                    .iter()
                    .any(|s| s.retriever_type == src.retriever_type)
                {
                    rep.retrieval_sources.push(src.clone());
                }
            }
        }
        // Retrieval confidence rises only mildly with independent origins —
        // never beyond 0.99 and never as if they were separate facts.
        let origins = rep.retrieval_sources.len().min(3) as f64;
        rep.confidence = (rep.confidence * 0.85 + origins * 0.05).clamp(0.0, 0.99);
        out.push(Candidate {
            kind: RetrieverKind::Fts,
            evidence: rep,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use attic_core::SourceSpan;
    use attic_evidence::{Evidence, EvidenceSourceType};

    fn cand(id: &str, st: EvidenceSourceType, src_id: &str, line: u32, conf: f64) -> Candidate {
        let mut ev = Evidence::new(id, "repo");
        ev.source_type = st;
        ev.source_id = src_id.to_owned();
        ev.source_span = Some(SourceSpan::new(line, 0, line + 5, 0));
        ev.confidence = conf;
        Candidate::new(RetrieverKind::Fts, ev)
    }

    #[test]
    fn same_source_from_two_retrievers_fuses_into_one() {
        let a = cand("e1", EvidenceSourceType::SourceCode, "fo-1", 10, 0.6);
        let mut b = cand("e2", EvidenceSourceType::SourceCode, "fo-1", 10, 0.9);
        b.kind = RetrieverKind::Symbol;
        b.evidence.retrieval_sources[0].retriever_type = "SYMBOL".to_owned();
        let fused = fuse(vec![a, b]);
        assert_eq!(fused.len(), 1, "duplicate discovery must not multiply");
        let ev = &fused[0].evidence;
        assert_eq!(ev.retrieval_sources.len(), 2, "both origins preserved");
        assert!((ev.confidence - 0.9 * 0.85 + 0.10).abs() < 1e-6 || ev.confidence >= 0.85);
    }

    #[test]
    fn different_spans_of_same_file_stay_separate() {
        let a = cand("e1", EvidenceSourceType::SourceCode, "fo-1", 10, 0.6);
        let b = cand("e2", EvidenceSourceType::SourceCode, "fo-1", 40, 0.7);
        assert_eq!(fuse(vec![a, b]).len(), 2);
    }

    #[test]
    fn fusion_is_deterministic() {
        let make = || {
            vec![
                cand("e1", EvidenceSourceType::Test, "t1", 1, 0.4),
                cand("e2", EvidenceSourceType::SourceCode, "s1", 2, 0.8),
                cand("e3", EvidenceSourceType::Configuration, "c1", 3, 0.5),
            ]
        };
        let r1 = fuse(make());
        let r2 = fuse(make());
        assert_eq!(
            r1.iter().map(|c| &c.evidence.id).collect::<Vec<_>>(),
            r2.iter().map(|c| &c.evidence.id).collect::<Vec<_>>()
        );
    }
}
