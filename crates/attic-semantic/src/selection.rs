//! SemanticUnitSelection (Phase 5 §4, ADR-014).
//!
//! ```text
//! ~500K retrieval units  !=  ~500K embeddings
//! ```
//!
//! An EXPLICIT, versioned, inspectable scoring policy decides which units
//! earn embeddings. Every signal is measurable, every exclusion reason is
//! counted in a report, and the ordering is deterministic (score desc,
//! unit id asc). No opaque importance model.

use std::collections::HashMap;

use attic_storage::SemanticUnitRow;

use crate::store::SemanticStore;

/// Version of THIS policy. Changing it invalidates all semantic artifacts
/// stamped with an older version (identity component, §5).
pub const SEMANTIC_SELECTION_VERSION: &str = "sem-sel-v1";

/// Inspectable knobs (defaults are compile-time constants; tests may vary).
#[derive(Debug, Clone)]
pub struct SelectionConfig {
    /// Units below this composite score are not worth an embedding.
    pub min_score: f64,
    /// Hard per-repository cap.
    pub max_units_per_repo: usize,
    /// Hard global cap (queue + storage bound, §20).
    pub max_units_total: usize,
    /// Units whose text exceeds this are NEVER embedded (LARGE safety §19);
    /// enrichment truncates nothing silently.
    pub max_input_bytes: usize,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            min_score: 0.30,
            max_units_per_repo: 512,
            max_units_total: 20_000,
            max_input_bytes: 16_384,
        }
    }
}

/// One unit's observable signal vector.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SelectionSignals {
    /// Source-class prior (code 1.0 … generated 0.15).
    pub source_class: f64,
    /// Text-length fit around the ~800-char sweet spot.
    pub size_fit: f64,
    /// Structural nodes mapped into the unit (normalized).
    pub structural: f64,
    /// Definition symbols in the backing file (normalized proxy for symbol
    /// importance at unit granularity; documented approximation).
    pub symbol_importance: f64,
    /// Inverse repository-size focus factor.
    pub repo_importance: f64,
    /// Normalized retrieval demand observed since last reconcile.
    pub query_demand: f64,
    /// Recency of `last_indexed_at` within a 30-day window.
    pub recent_activity: f64,
}

/// A unit that earned an embedding slot.
#[derive(Debug, Clone)]
pub struct SelectedUnit {
    pub row: SemanticUnitRow,
    pub score: f64,
    pub signals: SelectionSignals,
}

/// Why a unit did NOT get embedded. Counts are part of the plan-grade
/// observability contract ("inspectable", §4).
#[derive(Debug, Default, Clone)]
pub struct SelectionReport {
    pub scanned: usize,
    pub selected: usize,
    /// exclusion reason → count
    pub excluded: HashMap<&'static str, usize>,
    pub per_repo_selected: HashMap<String, usize>,
}

impl SelectionReport {
    fn exclude(&mut self, reason: &'static str) {
        *self.excluded.entry(reason).or_insert(0) += 1;
    }
}

pub const EX_GENERATED_PATH: &str = "generated_path";
pub const EX_GENERATED_TYPE: &str = "generated_file_type";
pub const EX_TOO_LARGE: &str = "exceeds_max_input_bytes";
pub const EX_DUPLICATE: &str = "duplicate_content";
pub const EX_BELOW_THRESHOLD: &str = "below_score_threshold";
pub const EX_CAP_REPO: &str = "per_repository_cap";
pub const EX_CAP_TOTAL: &str = "global_cap";

/// Paths/names that mark machine-generated or low-value content.
const GENERATED_MARKERS: &[&str] = &[
    "/target/",
    "/node_modules/",
    "/dist/",
    "/build/",
    "/out/",
    "/.git/",
    "package-lock.json",
    "cargo.lock",
    "go.sum",
    "yarn.lock",
    "pnpm-lock.yaml",
    ".min.js",
    ".min.css",
    ".pb.go",
    "_pb2.py",
    ".snap",
];

fn is_generated_path(path_lower: &str) -> bool {
    GENERATED_MARKERS.iter().any(|m| path_lower.contains(m))
}

/// Source-class prior from the recorded file_type OR the path when the
/// column carries a language string instead of the storage enum (both
/// shapes exist across index generations).
fn source_class_of(file_type: &str, path_lower: &str) -> f64 {
    match file_type {
        "SOURCE" | "CONFIG" | "DOCUMENT" | "INFRA" | "GENERATED" | "BINARY" | "UNKNOWN" => {
            return match file_type {
                "SOURCE" => 1.0,
                "CONFIG" => 0.9,
                "DOCUMENT" => 0.85,
                "INFRA" => 0.8,
                _ => 0.15,
            };
        }
        _ => {}
    }
    let name = path_lower.rsplit('/').next().unwrap_or(path_lower);
    const SRC: &[&str] = &[
        ".java", ".py", ".rs", ".js", ".jsx", ".ts", ".tsx", ".go", ".c", ".h", ".cpp", ".cc",
        ".hpp", ".rb", ".php", ".kt", ".swift", ".cs", ".m", ".scala", ".sh",
    ];
    const CFG: &[&str] = &[
        ".yml",
        ".yaml",
        ".toml",
        ".json",
        ".ini",
        ".properties",
        ".xml",
        ".cfg",
        ".conf",
        ".env",
    ];
    const DOC: &[&str] = &[".md", ".rst", ".txt", ".adoc"];
    const INFRA: &[&str] = &[
        "dockerfile",
        "jenkinsfile",
        "makefile",
        ".mk",
        ".tf",
        ".hcl",
    ];
    if SRC.iter().any(|e| name.ends_with(e)) {
        1.0
    } else if CFG.iter().any(|e| name.ends_with(e)) {
        0.9
    } else if DOC.iter().any(|e| name.ends_with(e)) {
        0.85
    } else if INFRA.iter().any(|e| name.contains(e)) {
        0.8
    } else {
        0.4 // unknown-but-indexable
    }
}

/// Size-fit curve: peak at ~800 chars, gentle slope, floor 0.15.
fn size_fit(len: usize) -> f64 {
    let l = len as f64;
    if l < 32.0 {
        return 0.05; // trivial fragments carry no semantics
    }
    let ideal = 800.0;
    let spread = 3200.0;
    (1.0 - ((l - ideal).abs() / spread)).clamp(0.15, 1.0)
}

/// Signal weights (explicit table — same spirit as Phase 4 ranking).
const W_SOURCE: f64 = 1.2;
const W_SIZE: f64 = 0.6;
const W_STRUCTURAL: f64 = 0.8;
const W_SYMBOL: f64 = 0.5;
const W_REPO: f64 = 0.2;
const W_DEMAND: f64 = 1.2;
const W_ACTIVITY: f64 = 0.4;

/// Select units to embed from the canonical index.
///
/// `demand` comes from the disposable store (`sem_query_demand`); pass an
/// empty map when the store is unavailable — selection still works.
pub fn select_units(
    rows: &[SemanticUnitRow],
    demand: &HashMap<String, u64>,
    cfg: &SelectionConfig,
) -> (Vec<SelectedUnit>, SelectionReport) {
    let mut report = SelectionReport {
        scanned: rows.len(),
        ..Default::default()
    };

    // Repo sizes for the focus factor.
    let mut repo_sizes: HashMap<&str, i64> = HashMap::new();
    for r in rows {
        *repo_sizes.entry(r.repository_id.as_str()).or_insert(0) += 1;
    }
    let max_demand = demand.values().copied().max().unwrap_or(0).max(1) as f64;
    let now_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0);

    // Deterministic duplicate handling: first occurrence by unit id wins.
    let mut seen_content: HashMap<String, ()> = HashMap::new();

    let mut scored: Vec<SelectedUnit> = Vec::new();
    // Pass 1: hard exclusions + scoring (deterministic input order).
    for r in rows {
        let lower_path = r.path.to_lowercase();
        if is_generated_path(&lower_path) {
            report.exclude(EX_GENERATED_PATH);
            continue;
        }
        if r.file_type == "GENERATED" || r.file_type == "BINARY" {
            report.exclude(EX_GENERATED_TYPE);
            continue;
        }
        if r.discovery_class == "IGNORED" {
            report.exclude(EX_GENERATED_TYPE);
            continue;
        }
        if r.retrieval_text.len() > cfg.max_input_bytes {
            report.exclude(EX_TOO_LARGE);
            continue;
        }
        let ch = crate::identity::content_hash(&r.retrieval_text);
        if seen_content.insert(ch, ()).is_some() {
            report.exclude(EX_DUPLICATE);
            continue;
        }

        let source_class = source_class_of(&r.file_type, &lower_path);
        let n = r.unit_node_count.max(0) as f64;
        let structural = (n * 0.25).min(1.0);
        let sdef = r.file_symbol_defs.max(0) as f64;
        let symbol_importance = (sdef * 0.2).min(1.0);
        let size = size_fit(r.retrieval_text.len());
        let repo_n = repo_sizes
            .get(r.repository_id.as_str())
            .copied()
            .unwrap_or(1) as f64;
        let repo_importance = 1.0 / (1.0 + repo_n.log10().max(0.0));
        let query_demand = demand.get(&r.path).copied().unwrap_or(0) as f64 / max_demand;
        let recent_activity = r
            .last_indexed_at_us
            .map(|t| {
                let hours = ((now_us - t) / 3_600_000_000).max(0) as f64;
                (1.0 - hours / 720.0).clamp(0.0, 1.0)
            })
            .unwrap_or(0.0);

        let signals = SelectionSignals {
            source_class,
            size_fit: size,
            structural,
            symbol_importance,
            repo_importance,
            query_demand,
            recent_activity,
        };
        let num = W_SOURCE * source_class
            + W_SIZE * size
            + W_STRUCTURAL * structural
            + W_SYMBOL * symbol_importance
            + W_REPO * repo_importance
            + W_DEMAND * query_demand
            + W_ACTIVITY * recent_activity;
        let den = W_SOURCE + W_SIZE + W_STRUCTURAL + W_SYMBOL + W_REPO + W_DEMAND + W_ACTIVITY;
        let score = num / den;

        if score < cfg.min_score {
            report.exclude(EX_BELOW_THRESHOLD);
            continue;
        }
        scored.push(SelectedUnit {
            row: r.clone(),
            score,
            signals,
        });
    }

    // Deterministic order: score desc, then unit id asc.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.row.unit_id.cmp(&b.row.unit_id))
    });

    // Pass 2: caps (per-repo, then global), preserving order.
    let mut out = Vec::with_capacity(scored.len());
    let mut per_repo: HashMap<String, usize> = HashMap::new();
    for su in scored {
        let rc = per_repo.entry(su.row.repository_id.clone()).or_insert(0);
        if *rc >= cfg.max_units_per_repo {
            report.exclude(EX_CAP_REPO);
            continue;
        }
        if out.len() >= cfg.max_units_total {
            report.exclude(EX_CAP_TOTAL);
            break;
        }
        *rc += 1;
        report
            .per_repo_selected
            .entry(su.row.repository_id.clone())
            .and_modify(|c| *c += 1)
            .or_insert(1);
        out.push(su);
    }
    report.selected = out.len();
    (out, report)
}

/// Convenience: read demand from the disposable store when available.
pub fn demand_from_store(store: Option<&SemanticStore>) -> HashMap<String, u64> {
    store.and_then(|s| s.demand_map().ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, path: &str, file_type: &str, text: &str) -> SemanticUnitRow {
        SemanticUnitRow {
            unit_id: id.to_owned(),
            repository_id: "repo-a".into(),
            file_occurrence_id: format!("fo-{id}"),
            index_generation_id: "g1".into(),
            retrieval_text: text.to_owned(),
            lexical_state: "CURRENT".into(),
            freshness_state: "CURRENT".into(),
            is_redacted: false,
            path: path.to_owned(),
            source_revision_id: "rev-1".into(),
            content_hash: "h".into(),
            file_type: file_type.to_owned(),
            discovery_class: "NORMAL".into(),
            last_indexed_at_us: None,
            unit_node_count: 2,
            file_symbol_defs: 1,
        }
    }

    #[test]
    fn generated_and_locked_content_is_excluded_with_reasons() {
        let rows = vec![
            row("u1", "src/main.rs", "SOURCE", "fn main() {}"),
            // GENERATED type with a marker-free path (type-flag exclusion).
            row("u2", "src/gen/legacy_output.rs", "GENERATED", "generated!"),
            // Marker-bearing path (lockfile exclusion).
            row("u3", "package-lock.json", "INFRA", "{}"),
            // Nested build output caught by the /build/ path marker.
            row("u4", "debug/build/out_gen.rs", "SOURCE", "artifact bytes"),
        ];
        let (sel, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].row.unit_id, "u1");
        assert_eq!(rep.excluded.get(EX_GENERATED_TYPE), Some(&1));
        assert_eq!(rep.excluded.get(EX_GENERATED_PATH), Some(&2));
    }

    #[test]
    fn duplicates_keep_first_deterministic_winner() {
        let text = "identical body text for duplication check";
        let rows = vec![
            row("u-b", "src/b.rs", "SOURCE", text),
            row("u-a", "src/a_copy.rs", "SOURCE", text),
        ];
        let (sel, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].row.unit_id, "u-b"); // first in deterministic scan order
        assert_eq!(rep.excluded.get(EX_DUPLICATE), Some(&1));
    }

    #[test]
    fn oversized_units_never_embed() {
        let big = "x".repeat(20_000);
        let rows = vec![row("big", "src/big.rs", "SOURCE", &big)];
        let (sel, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
        assert_eq!(sel.len(), 0);
        assert_eq!(rep.excluded.get(EX_TOO_LARGE), Some(&1));
    }

    #[test]
    fn caps_bind_per_repo_then_globally() {
        let mut rows = Vec::new();
        for i in 0..10 {
            rows.push(row(
                &format!("r-{i:02}"),
                &format!("src/f{i}.rs"),
                "SOURCE",
                &format!(
                    "distinct body {i}: unique tokens prevent duplicate exclusion {}",
                    i * 7
                ),
            ));
        }
        let cfg = SelectionConfig {
            max_units_per_repo: 3,
            max_units_total: 5,
            ..Default::default()
        };
        let (sel, rep) = select_units(&rows, &HashMap::new(), &cfg);
        assert_eq!(sel.len(), 3); // repo cap binds before global cap
        assert_eq!(rep.excluded.get(EX_CAP_REPO), Some(&7));
    }
}
