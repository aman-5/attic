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

/// Default per-file size ceiling for semantic admission (256 KiB).
///
/// Above a few hundred KB a text file is almost always generated data — an
/// export, a dump, a fixture, a bundle — whose thousands of chunks embed
/// slowly on CPU and then mostly duplicate each other in the vector space
/// (measured on a real corpus: five ~4 MiB environment JSON exports produced
/// 98% of the embedding queue). Such files stay fully lexical-searchable;
/// they simply never enter the embedding queue.
pub const DEFAULT_SEMANTIC_MAX_FILE_BYTES: u64 = 256 * 1024;

/// Inspectable knobs (defaults are compile-time constants; tests may vary).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionConfig {
    /// Units below this composite score are not worth an embedding.
    pub min_score: f64,
    /// Hard per-repository cap.
    pub max_units_per_repo: usize,
    /// Hard global cap (queue + storage bound, §20).
    pub max_units_total: usize,
    /// Units whose text exceeds this are NEVER embedded (LARGE safety §19);
    /// enrichment truncates nothing silently. Units between one provider
    /// window and this cap are embedded window-by-window and pooled.
    pub max_input_bytes: usize,
    /// Units from FILES larger than this are never embedded, however good
    /// their score (see [`DEFAULT_SEMANTIC_MAX_FILE_BYTES`]). This is the
    /// gate that keeps multi-megabyte machine-generated dumps out of the
    /// queue; lexical indexing is unaffected.
    pub max_file_bytes: u64,
    /// Additional path globs excluded from embedding (case-insensitive,
    /// matched against the workspace-relative path; `*` stays within a
    /// segment, `**` crosses segments, a trailing `/` matches a directory
    /// anywhere in the path, and a pattern without `/` also matches the bare
    /// file name). Empty by default — [`GENERATED_MARKERS`] already covers
    /// the common generated trees.
    pub exclude_globs: Vec<String>,
}

impl SelectionConfig {
    /// Default byte ceiling for an embeddable unit.
    ///
    /// Derived from the embedding provider's real read capacity rather than
    /// chosen independently: the gate must never admit a unit the model would
    /// silently truncate. See the `const _` chain assertion below.
    pub const MAX_INPUT_BYTES_DEFAULT: usize =
        crate::qwen3_provider::DEFAULT_MAX_TOKENS * crate::qwen3_provider::MIN_BYTES_PER_TOKEN;

    /// Re-anchor the selection gate to the capacity of the provider that will
    /// actually embed these units.
    ///
    /// [`MAX_INPUT_BYTES_DEFAULT`](Self::MAX_INPUT_BYTES_DEFAULT) is a
    /// *compile-time* constant derived from the Candle provider. That made the
    /// documented `selection gate <= provider capacity` chain hold only for
    /// Candle. When the ONNX/DirectML provider ran with a smaller window
    /// (seq_len 512 => 1024 bytes) while the gate still admitted 2048, every
    /// unit in the 1025..=2048 band passed selection, was enqueued, and then
    /// failed *permanently* at the enrichment pre-check with "input too large".
    /// Observed on a real corpus as 18 of 20 chunks dead (1227..=1769 bytes)
    /// while only the two units under 1024 bytes embedded.
    ///
    /// Calling this with the live provider's windowed capacity
    /// ([`crate::windowed::windowed_capacity`]) closes the band: units beyond
    /// what enrichment can window are excluded at selection as
    /// [`EX_TOO_LARGE`] — visible and counted — instead of becoming permanent
    /// queue failures.
    pub fn for_provider_capacity(mut self, provider_max_input_bytes: usize) -> Self {
        if provider_max_input_bytes > 0 {
            self.max_input_bytes = self.max_input_bytes.min(provider_max_input_bytes);
        }
        self
    }

    /// The default configuration as a const expression — the single source of
    /// truth for the defaults, so `EnrichmentConfig::standalone` can stay a
    /// `const fn` (its callers use it in const contexts). `Default::default()`
    /// delegates here; the `baseline_const_matches_default` test pins the two
    /// together.
    pub const fn baseline() -> Self {
        Self {
            min_score: 0.30,
            // Scaled by the same 5x as `max_units_total` below, so a
            // multi-repo workspace's per-repo fairness ratio is preserved —
            // raising only the global cap would do nothing for a workspace
            // with few repos, since each would still stop at the old 512
            // long before the (now much higher) global cap is ever reached.
            max_units_per_repo: 2_560,
            max_units_total: DEFAULT_MAX_UNITS_TOTAL,
            // Was a standalone 16_384, which sat far ABOVE what the provider
            // actually reads — so units between the provider ceiling and this
            // gate passed the check documented as "enrichment truncates
            // nothing silently" and were then truncated by the tokenizer.
            // Units above one provider window are embedded in windows and
            // mean-pooled (see `crate::windowed`), so the gate admits up to
            // the windowed capacity instead of a single window.
            max_input_bytes: crate::windowed::windowed_capacity(Self::MAX_INPUT_BYTES_DEFAULT),
            max_file_bytes: DEFAULT_SEMANTIC_MAX_FILE_BYTES,
            exclude_globs: Vec::new(),
        }
    }
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self::baseline()
    }
}

/// Workspace-wide embedding cap (unique, selected units — applied AFTER
/// dedup and exclusions, never to the raw scan). Sized for industry-scale
/// multi-repository workspaces.
pub const DEFAULT_MAX_UNITS_TOTAL: usize = 500_000;
/// Upper bound accepted for either cap in `attic.toml`.
pub const MAX_UNITS_CAP_LIMIT: usize = 5_000_000;

/// Full-coverage `min_score` used when embedding runs on a GPU.
pub const GPU_MIN_SCORE: f64 = 0.0;
/// Full-coverage per-repository cap used when embedding runs on a GPU.
pub const GPU_MAX_UNITS_PER_REPO: usize = DEFAULT_MAX_UNITS_TOTAL;
/// Full-coverage file-size ceiling used when embedding runs on a GPU (8 MiB).
pub const GPU_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

impl SelectionConfig {
    /// Defaults for the backend that will do the embedding.
    ///
    /// On a GPU, full coverage is cheap enough to be the default: measured
    /// on an RTX A500 laptop, a 3,220-chunk AEM repo embeds in ~9 min and a
    /// 10,596-chunk export folder in ~13 min. On CPU (~1 chunk/s) the same
    /// coverage would take roughly an hour per repository, so the
    /// conservative [`Self::baseline`] stays the CPU default. Any value set
    /// explicitly in `attic.toml` overrides both.
    pub fn for_backend(gpu: bool) -> Self {
        let base = Self::baseline();
        if !gpu {
            return base;
        }
        Self {
            min_score: GPU_MIN_SCORE,
            max_units_per_repo: GPU_MAX_UNITS_PER_REPO,
            max_file_bytes: GPU_MAX_FILE_BYTES,
            ..base
        }
    }
}

/// The size limits in the pipeline must form a chain, or content is silently
/// lost between them:
///
/// ```text
/// selection gate  <=  provider read capacity
/// ```
///
/// This link is guaranteed *by construction* above:
/// [`SelectionConfig::MAX_INPUT_BYTES_DEFAULT`] is derived from the provider's
/// token ceiling rather than chosen independently, which is what went wrong
/// before — a standalone 16_384 against a real ~1 KB tokenizer limit, so units
/// passed the gate documented as "enrichment truncates nothing silently" and
/// were then truncated anyway.
///
/// Units produced *larger* than this gate are fine: they are excluded and
/// counted as [`EX_TOO_LARGE`], which is explicit and inspectable. The
/// invariant that matters is only that nothing admitted here is later cut.
const _: () = {
    assert!(
        SelectionConfig::MAX_INPUT_BYTES_DEFAULT
            <= crate::qwen3_provider::DEFAULT_MAX_TOKENS
                * crate::qwen3_provider::MIN_BYTES_PER_TOKEN,
        "selection gate must never admit a unit larger than the provider reads"
    );
    assert!(
        SelectionConfig::MAX_INPUT_BYTES_DEFAULT > 0,
        "selection gate must admit something"
    );
};

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
    /// True when the scan stopped at the safety bound before reading every
    /// selectable unit (the remainder was not considered this pass).
    pub scan_truncated: bool,
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

/// The most recent selection outcome, published for `status`.
///
/// Selection decides how much of the index is eligible for embedding at all,
/// and it can legitimately reject the overwhelming majority of units — a
/// JSON-heavy corpus was observed producing 45,110 units of which only 20
/// were selected. That may be correct (duplicates, low signal, caps) or a
/// misconfiguration, and previously there was no way to tell the two apart:
/// the report was computed on every reconcile, logged only when something
/// was enqueued, and then dropped. An operator saw "45,110 units indexed,
/// 20 embedded" with no reason attached.
///
/// Publishing the last report makes the gap self-explaining, which is the
/// same standard applied to the backend, the stall verdict and the GPU
/// capability report.
static LAST_SELECTION: std::sync::OnceLock<std::sync::Mutex<Option<SelectionReport>>> =
    std::sync::OnceLock::new();

fn last_selection_cell() -> &'static std::sync::Mutex<Option<SelectionReport>> {
    LAST_SELECTION.get_or_init(|| std::sync::Mutex::new(None))
}

/// Record the outcome of a selection pass. Never panics on a poisoned lock:
/// losing an observability snapshot must not take down enrichment.
pub fn publish_selection_report(report: &SelectionReport) {
    if let Ok(mut slot) = last_selection_cell().lock() {
        *slot = Some(report.clone());
    }
}

/// The last published selection outcome, if any pass has run.
pub fn last_selection_report() -> Option<SelectionReport> {
    last_selection_cell().lock().ok().and_then(|s| s.clone())
}

pub const EX_GENERATED_PATH: &str = "generated_path";
pub const EX_GENERATED_TYPE: &str = "generated_file_type";
pub const EX_TOO_LARGE: &str = "exceeds_max_input_bytes";
pub const EX_FILE_TOO_BIG: &str = "file_exceeds_max_file_bytes";
pub const EX_EXCLUDED_GLOB: &str = "excluded_by_glob";
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

/// Deliberately small glob matcher (this crate carries no external glob
/// dependency): `*` matches within a path segment, `**` matches across
/// segments, `?` matches one byte. Byte-oriented by design — it only ever
/// produces a bool, never slices the text.
fn glob_matches(pattern: &str, text: &str) -> bool {
    fn inner(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') => {
                if p.get(1) == Some(&b'*') {
                    // `**` crosses `/`; a following `/` may match zero segments.
                    let rest = if p.get(2) == Some(&b'/') {
                        &p[3..]
                    } else {
                        &p[2..]
                    };
                    (0..=t.len()).any(|i| inner(rest, &t[i..]))
                } else {
                    // `*` matches zero or more non-`/` bytes.
                    let mut i = 0;
                    loop {
                        if inner(&p[1..], &t[i..]) {
                            return true;
                        }
                        if i >= t.len() || t[i] == b'/' {
                            return false;
                        }
                        i += 1;
                    }
                }
            }
            Some(&c) => !t.is_empty() && (c == b'?' || c == t[0]) && inner(&p[1..], &t[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// True when `path_lower` (already lowercased) matches any configured glob.
/// A pattern with no `/` also matches the bare file name, so `*.min.js`
/// catches `web/js/app.min.js`; a trailing `/` matches the directory anywhere
/// in the path.
fn path_is_excluded(path_lower: &str, globs: &[String]) -> bool {
    globs.iter().any(|g| {
        let g = g.trim().to_lowercase();
        if g.is_empty() {
            return false;
        }
        if let Some(dir) = g.strip_suffix('/') {
            let dir = dir.trim_matches('/');
            return !dir.is_empty()
                && (path_lower.starts_with(&format!("{dir}/"))
                    || path_lower.contains(&format!("/{dir}/")));
        }
        glob_matches(&g, path_lower)
            || (!g.contains('/')
                && path_lower
                    .rsplit('/')
                    .next()
                    .is_some_and(|name| glob_matches(&g, name)))
    })
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
) -> (
    Vec<SelectedUnit>,
    Vec<(SemanticUnitRow, String)>,
    SelectionReport,
) {
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
    // Excluded-as-duplicate rows still need an occurrence record (r02/§5):
    // the winning unit above does the one embedding job, but every OTHER
    // unit sharing that canonical body is still a real repo/path/environment
    // occurrence and must stay retrievable, not silently vanish because it
    // lost the "first by unit id" tiebreak.
    let mut duplicates: Vec<(SemanticUnitRow, String)> = Vec::new();

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
        // Admission gates: explicit path rules first, then the per-file size
        // ceiling. Both are about the *file*, not the unit's score — a
        // machine-generated dump produces thousands of individually
        // fine-looking units.
        if path_is_excluded(&lower_path, &cfg.exclude_globs) {
            report.exclude(EX_EXCLUDED_GLOB);
            continue;
        }
        if r.size_bytes >= 0 && r.size_bytes as u64 > cfg.max_file_bytes {
            report.exclude(EX_FILE_TOO_BIG);
            continue;
        }
        if r.canonical_len > cfg.max_input_bytes {
            report.exclude(EX_TOO_LARGE);
            continue;
        }
        // Dedup on the CANONICAL body (r03): occurrence decoration (JSON
        // pointer / environment headers) lives outside the hash, so identical
        // logical content across files and environments collapses to one
        // embedding. Prefer the pipeline-recorded hash; legacy rows (pre-0002)
        // hash the canonical text here — identical result for undecorated
        // units, where canonical_text == retrieval_text.
        let ch = r
            .canonical_hash
            .clone()
            .unwrap_or_else(|| crate::identity::content_hash(&r.canonical_text));
        if seen_content.insert(ch.clone(), ()).is_some() {
            report.exclude(EX_DUPLICATE);
            duplicates.push((r.clone(), ch));
            continue;
        }

        let source_class = source_class_of(&r.file_type, &lower_path);
        let n = r.unit_node_count.max(0) as f64;
        let structural = (n * 0.25).min(1.0);
        let sdef = r.file_symbol_defs.max(0) as f64;
        let symbol_importance = (sdef * 0.2).min(1.0);
        let size = size_fit(r.canonical_len);
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
    (out, duplicates, report)
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
            // Small enough to stay far under `DEFAULT_SEMANTIC_MAX_FILE_BYTES`
            // so the admission size gate never fires in pre-existing tests.
            size_bytes: 100,
            canonical_hash: None,
            canonical_text: text.to_owned(),
            canonical_len: text.len(),
        }
    }

    #[test]
    fn canonical_hash_dedups_across_decorated_occurrences() {
        // r03: two units with DIFFERENT retrieval_text (different JSON
        // pointer/env headers) but the SAME canonical body must dedup.
        let body = "{\"service\":\"payment\"}";
        let hash = crate::identity::content_hash(body);
        let mut a = row(
            "u-dev",
            "DEV-Form.json",
            "CONFIG",
            "// json-pointer: /s (env: DEV)\n",
        );
        let mut b = row(
            "u-prod",
            "PROD-Form.json",
            "CONFIG",
            "// json-pointer: /s (env: PROD)\n",
        );
        a.canonical_hash = Some(hash.clone());
        b.canonical_hash = Some(hash);
        let (sel, dups, rep) = select_units(&[a, b], &HashMap::new(), &SelectionConfig::default());
        assert_eq!(sel.len(), 1, "identical canonical bodies dedup to one");
        assert_eq!(rep.excluded.get(EX_DUPLICATE), Some(&1));
        // The dedup loser must still be reported so its occurrence can be
        // registered (r02): its metadata must never be silently dropped.
        assert_eq!(
            dups.len(),
            1,
            "duplicate must still surface for occurrence linking"
        );
        assert_eq!(dups[0].1, sel[0].row.canonical_hash.clone().unwrap());
        assert_ne!(dups[0].0.unit_id, sel[0].row.unit_id);
    }

    #[test]
    fn baseline_const_matches_default() {
        // `baseline()` exists so `EnrichmentConfig::standalone` can be const;
        // it must never drift from `Default`.
        assert_eq!(SelectionConfig::baseline(), SelectionConfig::default());
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
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
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
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].row.unit_id, "u-b"); // first in deterministic scan order
        assert_eq!(rep.excluded.get(EX_DUPLICATE), Some(&1));
    }

    #[test]
    fn oversized_units_never_embed() {
        // Beyond the windowed capacity (16 windows x 2 KiB): excluded, counted.
        let big = "x".repeat(40_000);
        let rows = vec![row("big", "src/big.rs", "SOURCE", &big)];
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
        assert_eq!(sel.len(), 0);
        assert_eq!(rep.excluded.get(EX_TOO_LARGE), Some(&1));
    }

    #[test]
    fn files_over_the_semantic_size_cap_are_never_selected() {
        // The Dump-corpus regression: five ~4 MiB environment JSON exports
        // produced 98% of the embedding queue. Their units score fine
        // individually; the gate is on the FILE.
        let mut dump = row("u1", "DEV-Code.json", "CONFIG", "ordinary chunk text");
        dump.size_bytes = 5 * 1024 * 1024;
        let rows = vec![
            dump,
            row("u2", "docs/guide.md", "DOCUMENT", "ordinary chunk text 2"),
        ];
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].row.unit_id, "u2");
        assert_eq!(rep.excluded.get(EX_FILE_TOO_BIG), Some(&1));
    }

    #[test]
    fn configured_globs_exclude_paths_from_embedding() {
        let cfg = SelectionConfig {
            exclude_globs: vec!["*-code.json".to_owned(), "fixtures/".to_owned()],
            ..Default::default()
        };
        let rows = vec![
            row("u1", "DEV-Code.json", "CONFIG", "body one"),
            row("u2", "fixtures/seed.rs", "SOURCE", "body two"),
            row("u3", "src/main.rs", "SOURCE", "body three"),
        ];
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &cfg);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].row.unit_id, "u3");
        assert_eq!(rep.excluded.get(EX_EXCLUDED_GLOB), Some(&2));
    }

    #[test]
    fn glob_matcher_segment_semantics() {
        assert!(glob_matches("*.min.js", "app.min.js"));
        assert!(!glob_matches("*.min.js", "app.js"));
        assert!(glob_matches("fixtures/**", "fixtures/a/b.json"));
        assert!(
            !glob_matches("*.json", "a/b.json"),
            "`*` must not cross `/`"
        );
        assert!(glob_matches("**/gen/**", "a/gen/b.rs"));
        assert!(path_is_excluded(
            "web/js/app.min.js",
            &["*.min.js".to_owned()]
        ));
        assert!(path_is_excluded(
            "a/node_modules/b.js",
            &["node_modules/".to_owned()]
        ));
        assert!(!path_is_excluded("src/main.rs", &["*.json".to_owned()]));
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
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &cfg);
        assert_eq!(sel.len(), 3); // repo cap binds before global cap
        assert_eq!(rep.excluded.get(EX_CAP_REPO), Some(&7));
    }

    /// The regression this guards: the selection gate was a compile-time
    /// constant derived from the Candle provider (1024 tokens => 2048 bytes),
    /// while the live ONNX/DirectML provider ran a 512-token window
    /// (=> 1024 bytes). Units in the 1025..=2048 band passed selection, were
    /// enqueued, and then failed *permanently* with "input too large".
    /// A real corpus lost 18 of 20 chunks (1227..=1769 bytes) this way.
    /// A selection pass that rejects almost everything must still be
    /// explainable — publishing is what turns "45,110 units, 20 embedded"
    /// from a mystery into a breakdown.
    #[test]
    fn the_last_selection_outcome_is_observable() {
        let rows = vec![
            row("r-1", "src/a.rs", "SOURCE", "distinct alpha body one"),
            row("r-2", "src/b.rs", "SOURCE", "distinct beta body two"),
        ];
        let (_sel, _dups, rep) = select_units(&rows, &HashMap::new(), &SelectionConfig::default());
        publish_selection_report(&rep);

        let seen = last_selection_report().expect("a published report must be readable");
        assert_eq!(seen.scanned, rep.scanned);
        assert_eq!(seen.selected, rep.selected);
    }

    #[test]
    fn gate_never_admits_more_than_the_live_provider_reads() {
        const NARROW_PROVIDER_BYTES: usize = 512 * 2;

        let cfg = SelectionConfig {
            max_input_bytes: SelectionConfig::MAX_INPUT_BYTES_DEFAULT,
            ..Default::default()
        }
        .for_provider_capacity(NARROW_PROVIDER_BYTES);
        assert!(
            cfg.max_input_bytes <= NARROW_PROVIDER_BYTES,
            "gate {} must not exceed provider capacity {NARROW_PROVIDER_BYTES}",
            cfg.max_input_bytes
        );

        // A unit inside the old dead band must now be excluded and COUNTED,
        // never silently admitted for a permanent downstream failure.
        let big = "x".repeat(1_600);
        let rows = vec![row("r-big", "src/big.rs", "SOURCE", &big)];
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &cfg);
        assert!(sel.is_empty(), "oversized unit must not be selected");
        assert_eq!(rep.excluded.get(EX_TOO_LARGE), Some(&1));
    }

    /// A provider wider than the default must not *raise* the gate — the
    /// default encodes other limits too, so the clamp is one-directional.
    #[test]
    fn units_above_one_window_are_selected_for_windowed_embedding() {
        // The Dump regression: DirectML at seq_len 512 reads 1024 bytes, and
        // every 1025..=2048-byte chunk was excluded. They now fit the
        // windowed capacity and are selected.
        let cfg = SelectionConfig::default()
            .for_provider_capacity(crate::windowed::windowed_capacity(512 * 2));
        let rows = vec![row(
            "r-mid",
            "docs/guide.md",
            "DOCUMENT",
            &"word ".repeat(340),
        )];
        let (sel, _dups, rep) = select_units(&rows, &HashMap::new(), &cfg);
        assert_eq!(sel.len(), 1, "report: {rep:?}");
    }

    #[test]
    fn gpu_defaults_are_full_coverage_cpu_defaults_stay_conservative() {
        let cpu = SelectionConfig::for_backend(false);
        assert_eq!(cpu.min_score, SelectionConfig::baseline().min_score);
        assert_eq!(cpu.max_units_per_repo, 2_560);
        assert_eq!(cpu.max_file_bytes, DEFAULT_SEMANTIC_MAX_FILE_BYTES);

        let gpu = SelectionConfig::for_backend(true);
        assert_eq!(gpu.min_score, 0.0);
        assert_eq!(gpu.max_units_per_repo, 500_000);
        assert_eq!(gpu.max_file_bytes, 8 * 1024 * 1024);
        assert_eq!(gpu.max_units_total, cpu.max_units_total);
        assert_eq!(cpu.max_units_total, 500_000);
        assert_eq!(gpu.max_input_bytes, cpu.max_input_bytes);
    }

    /// A provider wider than the default must not *raise* the gate — the
    /// default encodes other limits too, so the clamp is one-directional.
    #[test]
    fn a_wider_provider_does_not_loosen_the_gate() {
        let base = SelectionConfig::default().max_input_bytes;
        let cfg = SelectionConfig::default().for_provider_capacity(base * 4);
        assert_eq!(cfg.max_input_bytes, base);
    }

    /// A provider reporting zero capacity is nonsense; keep the default
    /// rather than clamping the gate to zero and excluding everything.
    #[test]
    fn a_zero_capacity_provider_is_ignored() {
        let base = SelectionConfig::default().max_input_bytes;
        assert_eq!(
            SelectionConfig::default()
                .for_provider_capacity(0)
                .max_input_bytes,
            base
        );
    }
}
