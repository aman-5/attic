//! Contradiction detection (Phase 4 §14): find meaningful contradictory
//! evidence and PRESERVE the conflict as metadata. Contradictions are
//! surfaced to context and answer verification — never silently resolved by
//! picking whichever candidate ranked first.

use std::collections::HashMap;

use attic_core::FreshnessState;
use attic_evidence::{Contradiction, ContradictionKind, Evidence, EvidenceSourceType as ST};

/// Detect contradictions among validated evidence. Deterministic: pairs are
/// examined in evidence-id order.
pub fn detect(validated: &[Evidence]) -> Vec<Contradiction> {
    let mut out = Vec::new();

    // ── Ambiguous definitions: same qualified name defined in >1 file ──────
    // Snippets of definition-bearing source evidence carry the symbol name;
    // group by path-independent key extracted from the retrieval origin.
    {
        let mut by_name: HashMap<String, Vec<&Evidence>> = HashMap::new();
        for ev in validated.iter().filter(|e| e.source_type == ST::SourceCode) {
            if let Some(name) = definition_name(ev) {
                by_name.entry(name).or_default().push(ev);
            }
        }
        let mut keys: Vec<_> = by_name.keys().cloned().collect();
        keys.sort();
        for name in keys {
            let mut defs = by_name.remove(&name).unwrap_or_default();
            defs.sort_by(|a, b| a.id.cmp(&b.id));
            for w in defs.windows(2) {
                if w[0].path != w[1].path
                    && w[0].freshness_state == FreshnessState::Current
                    && w[1].freshness_state == FreshnessState::Current
                {
                    out.push(Contradiction {
                        kind: ContradictionKind::AmbiguousDefinition,
                        evidence_a: w[0].id.clone(),
                        evidence_b: w[1].id.clone(),
                        description: format!(
                            "multiple incompatible definitions of `{name}` remain: {} vs {}",
                            w[0].path, w[1].path
                        ),
                    });
                }
            }
        }
    }

    // ── Configuration value conflicts between CURRENT config items ─────────
    {
        let mut by_key: HashMap<String, Vec<(&Evidence, String)>> = HashMap::new();
        for ev in validated
            .iter()
            .filter(|e| e.source_type == ST::Configuration)
        {
            if ev.freshness_state != FreshnessState::Current {
                continue;
            }
            if let Some((key, value)) = config_key_value(ev) {
                by_key.entry(key).or_default().push((ev, value));
            }
        }
        let mut keys: Vec<_> = by_key.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let entries = by_key.remove(&key).unwrap_or_default();
            for w in entries.windows(2) {
                if w[0].1 != w[1].1 && w[0].0.path != w[1].0.path {
                    out.push(Contradiction {
                        kind: ContradictionKind::ConflictingValues,
                        evidence_a: w[0].0.id.clone(),
                        evidence_b: w[1].0.id.clone(),
                        description: format!(
                            "configuration key `{key}` has conflicting values across {} and {}",
                            w[0].0.path, w[1].0.path
                        ),
                    });
                }
            }
        }
    }

    // ── Knowledge vs implementation/config mismatch on shared keys ─────────
    {
        let knowledge_values: Vec<&Evidence> = validated
            .iter()
            .filter(|e| matches!(e.source_type, ST::Knowledge | ST::Documentation))
            .collect();
        let impl_config: Vec<&Evidence> = validated
            .iter()
            .filter(|e| matches!(e.source_type, ST::SourceCode | ST::Configuration))
            .collect();
        for k in knowledge_values {
            let Some(kval) = notable_value(k) else {
                continue;
            };
            for ic in &impl_config {
                if ic.freshness_state != FreshnessState::Current {
                    continue;
                }
                if let Some(ival) = notable_value(ic)
                    && kval.key == ival.key
                    && ival.value != kval.value
                    && !k
                        .path
                        .starts_with(ic.path.trim_end_matches('/').trim_end_matches(".md"))
                {
                    out.push(Contradiction {
                        kind: ContradictionKind::KnowledgeVsImplementation,
                        evidence_a: k.id.clone(),
                        evidence_b: ic.id.clone(),
                        description: format!(
                            "knowledge item `{}` states `{}`={} but {} has a different value",
                            k.path,
                            kval.key,
                            first_line(&kval.value),
                            ic.path
                        ),
                    });
                }
            }
        }
    }

    // ── Stale duplicates of items that also have CURRENT evidence ──────────
    {
        let mut current_keys: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for ev in validated
            .iter()
            .filter(|e| e.freshness_state == FreshnessState::Current)
        {
            current_keys.insert((ev.source_type.as_str().to_owned(), ev.path.clone()));
        }
        for ev in validated
            .iter()
            .filter(|e| e.freshness_state == FreshnessState::Stale)
        {
            let key = (ev.source_type.as_str().to_owned(), ev.path.clone());
            if current_keys.contains(&key) {
                out.push(Contradiction {
                    kind: ContradictionKind::SupersededStale,
                    evidence_a: ev.id.clone(),
                    evidence_b: ev.id.clone(),
                    description: format!(
                        "stale duplicate coexists with current evidence for {}",
                        ev.path
                    ),
                });
            }
        }
    }

    out
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(80).collect()
}

/// Extract the definition symbol name from an evidence snippet produced by
/// the symbol generator (`name(path:line)` convention is not used; instead
/// the snippet's first token before any whitespace).
fn definition_name(ev: &Evidence) -> Option<String> {
    let snip = ev.snippet.as_ref()?;
    let first = snip.split_whitespace().next()?;
    let cleaned = first.trim_matches(|c: char| c.is_ascii_punctuation());
    if cleaned.len() >= 2 && cleaned.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Some(cleaned.to_lowercase())
    } else {
        None
    }
}

struct KeyValue {
    key: String,
    value: String,
}

/// Extract `key = value` / `key: value` from a snippet (first match).
fn config_key_value(ev: &Evidence) -> Option<(String, String)> {
    let kv = notable_value(ev)?;
    Some((kv.key.to_lowercase(), kv.value))
}

fn notable_value(ev: &Evidence) -> Option<KeyValue> {
    let snip = ev.snippet.as_ref()?;
    for line in snip.lines() {
        let line = line.trim();
        let sep = line.find(['=', ':'])?;
        let (before, after) = line.split_at(sep);
        let key = before.trim();
        let value = after[1..].trim().trim_matches(['"', '\'', ',']).to_string();
        let too_many_words = key.contains(' ') && key.split_whitespace().count() > 3;
        if key.is_empty() || value.is_empty() || too_many_words {
            continue;
        }
        return Some(KeyValue {
            key: key.to_string(),
            value,
        });
    }
    None
}
