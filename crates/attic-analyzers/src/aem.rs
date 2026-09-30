//! Adobe Experience Manager (AEM) platform plugin.
//!
//! AEM projects mix general-purpose file types whose meaning comes from where
//! they live in a FileVault content package, so this plugin classifies files
//! by **path**, not by extension alone:
//!
//! | Kind        | Files                                                        | Extracted structure |
//! |-------------|--------------------------------------------------------------|---------------------|
//! | `aem-jcr`   | `.content.xml`, and `*.xml` under `jcr_root/`                | one node + symbol per JCR node (path-qualified), `jcr:primaryType` / `sling:resourceType` / `sling:resourceSuperType` / `cq:template` metadata; resource types, super types, templates and clientlib `embed`/`dependencies` as imports |
//! | `aem-htl`   | `*.html` under `jcr_root/`, and `*.htl` anywhere             | `data-sly-template` definitions, `data-sly-use` bindings; `data-sly-use`/`include`/`resource` targets as imports |
//! | `aem-osgi`  | `*.cfg.json` anywhere; `*.config` in `config*`/`osgiconfig` folders under `jcr_root/` | the configuration PID (factory name and run modes in metadata) plus one symbol per property |
//! | `aem-clientlib` | `js.txt` / `css.txt` under `jcr_root/`                   | the clientlib folder plus one import per listed file (honouring `#base=`) |
//!
//! Retrieval units always come from [`GenericAnalyzer`], so AEM files keep
//! exactly the lexical coverage they had before; this plugin only adds
//! structure. Parsing is tolerant (malformed markup yields partial structure
//! and a warning, never an error), budgets and cancellation are honoured, and
//! capabilities are declared honestly: symbols and imports only, no
//! cross-file resolution.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use attic_core::{SourceSpan, SymbolKind};

use crate::api::{
    Analyzer, AnalyzerCapabilities, AnalyzerContent, AnalyzerDescriptor, AnalyzerDiagnostic,
    AnalyzerInput, AnalyzerOutput, CapabilityKind, CapabilityLevel, ImportSpec, StructuralNodeSpec,
    SymbolSpec, diagnostic_codes,
};
use crate::generic::GenericAnalyzer;
use crate::plugin::{AnalyzerPlugin, PluginPath};
use crate::registry::AnalyzerRegistry;

/// JCR content XML (`.content.xml`, `*.xml` under `jcr_root/`).
pub const AEM_JCR_TAG: &str = "aem-jcr";
/// HTL / Sightly templates.
pub const AEM_HTL_TAG: &str = "aem-htl";
/// OSGi configurations (`*.cfg.json`, Felix `*.config`).
pub const AEM_OSGI_TAG: &str = "aem-osgi";
/// Client library manifests (`js.txt`, `css.txt`).
pub const AEM_CLIENTLIB_TAG: &str = "aem-clientlib";

/// Upper bound on structural entities per file, before the unit budget.
const ENTITY_CAP: usize = 20_000;
/// Check cancellation and the time budget every this many entities.
const CHECK_EVERY: usize = 256;

/// Which AEM artifact a file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AemKind {
    /// JCR content XML.
    Jcr,
    /// HTL template.
    Htl,
    /// OSGi configuration.
    Osgi,
    /// Clientlib manifest.
    Clientlib,
}

impl AemKind {
    /// Language tag registered for this kind.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Jcr => AEM_JCR_TAG,
            Self::Htl => AEM_HTL_TAG,
            Self::Osgi => AEM_OSGI_TAG,
            Self::Clientlib => AEM_CLIENTLIB_TAG,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Jcr => {
                "AEM JCR content XML: path-qualified JCR nodes, resource types and templates"
            }
            Self::Htl => {
                "AEM HTL templates: data-sly-template definitions and use/include/resource targets"
            }
            Self::Osgi => "AEM OSGi configurations: PID, factory name, run modes and properties",
            Self::Clientlib => "AEM clientlib manifests: listed JS/CSS sources",
        }
    }

    const ALL: [Self; 4] = [Self::Jcr, Self::Htl, Self::Osgi, Self::Clientlib];
}

/// Classify a path as an AEM artifact, or `None` for ordinary files. Plain
/// `.html`/`.xml`/`.json` outside AEM layouts are never claimed.
pub fn classify(path: &PluginPath) -> Option<AemKind> {
    let name = path.file_name();
    let in_jcr_root = path.has_dir_segment("jcr_root");

    if name.ends_with(".cfg.json") {
        return Some(AemKind::Osgi);
    }
    if name == ".content.xml" {
        return Some(AemKind::Jcr);
    }
    if path.extension() == Some("htl") {
        return Some(AemKind::Htl);
    }
    if !in_jcr_root {
        return None;
    }
    if name == "js.txt" || name == "css.txt" {
        return Some(AemKind::Clientlib);
    }
    if path.extension() == Some("config") && path.dir_segments().any(is_osgi_config_dir) {
        return Some(AemKind::Osgi);
    }
    match path.extension() {
        Some("xml") => Some(AemKind::Jcr),
        Some("html") => Some(AemKind::Htl),
        _ => None,
    }
}

fn is_osgi_config_dir(segment: &str) -> bool {
    segment == "osgiconfig" || segment == "config" || segment.starts_with("config.")
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

/// The AEM plugin (`id = "aem"`).
pub struct AemPlugin;

impl AnalyzerPlugin for AemPlugin {
    fn id(&self) -> &'static str {
        "aem"
    }

    fn description(&self) -> &'static str {
        "Adobe Experience Manager: JCR content, HTL templates, OSGi configs and clientlibs"
    }

    fn language_hint(&self, path: &PluginPath) -> Option<&'static str> {
        classify(path).map(AemKind::tag)
    }

    fn register(&self, registry: &mut AnalyzerRegistry) {
        for kind in AemKind::ALL {
            registry.register_for_language(kind.tag(), Arc::new(AemAnalyzer::new(kind)));
        }
    }
}

// ---------------------------------------------------------------------------
// Analyzer
// ---------------------------------------------------------------------------

/// Structural analyzer for one [`AemKind`].
pub struct AemAnalyzer {
    kind: AemKind,
    descriptor: AnalyzerDescriptor,
}

impl AemAnalyzer {
    /// Analyzer for `kind`.
    pub fn new(kind: AemKind) -> Self {
        Self {
            kind,
            descriptor: AnalyzerDescriptor {
                name: kind.tag().to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                description: kind.description().to_owned(),
                supported_file_types: vec![],
                capabilities: AnalyzerCapabilities {
                    entries: vec![
                        (CapabilityKind::StructuralParse, CapabilityLevel::Partial),
                        (CapabilityKind::SymbolExtraction, CapabilityLevel::Basic),
                        (CapabilityKind::ImportExtraction, CapabilityLevel::Basic),
                        (CapabilityKind::ReferenceExtraction, CapabilityLevel::None),
                        (
                            CapabilityKind::RelationshipResolution,
                            CapabilityLevel::None,
                        ),
                    ],
                },
            },
        }
    }
}

impl Analyzer for AemAnalyzer {
    fn descriptor(&self) -> &AnalyzerDescriptor {
        &self.descriptor
    }

    fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
        let started = Instant::now();
        let mut extraction =
            Extraction::new(ENTITY_CAP.min(
                usize::try_from(input.resource_budget.max_retrieval_units).unwrap_or(ENTITY_CAP),
            ));
        let mut attempted = false;

        match &input.content {
            AnalyzerContent::StreamingHandle(_) => {
                extraction.diagnostics.push(AnalyzerDiagnostic::warning(
                    "STRUCTURAL_SKIPPED_LARGE_FILE",
                    format!(
                        "{}: structural analysis is not attempted for streamed LARGE files; \
                         output is lexical-only via GenericAnalyzer.",
                        self.kind.tag()
                    ),
                ));
                extraction.complete = false;
            }
            AnalyzerContent::FullBytes(bytes) | AnalyzerContent::RedactedBytes(bytes) => {
                if input.cancellation_token.is_cancelled() {
                    extraction.diagnostics.push(AnalyzerDiagnostic::warning(
                        diagnostic_codes::CANCELLED,
                        "Cancelled before AEM structural analysis started.",
                    ));
                    extraction.complete = false;
                } else {
                    match std::str::from_utf8(bytes) {
                        Ok(text) => {
                            attempted = true;
                            let ctx = Ctx {
                                text,
                                lines: LineIndex::new(text),
                                path: &input.path,
                                started,
                                budget_ms: input.resource_budget.max_time_ms,
                                cancel: &input.cancellation_token,
                            };
                            match self.kind {
                                AemKind::Jcr => extract_jcr(&ctx, &mut extraction),
                                AemKind::Htl => extract_htl(&ctx, &mut extraction),
                                AemKind::Osgi => extract_osgi(&ctx, &mut extraction),
                                AemKind::Clientlib => extract_clientlib(&ctx, &mut extraction),
                            }
                        }
                        Err(_) => {
                            extraction.diagnostics.push(AnalyzerDiagnostic::warning(
                                diagnostic_codes::MALFORMED_INPUT,
                                "AEM file is not valid UTF-8; output is lexical-only.",
                            ));
                            extraction.complete = false;
                        }
                    }
                }
            }
        }

        let mut out = self.unit_output(input);
        out.analyzer_id = self.descriptor.name.clone();
        out.analyzer_version = self.descriptor.version.clone();
        out.diagnostics.extend(extraction.diagnostics);
        out.structural_nodes.extend(extraction.nodes);
        out.symbols.extend(extraction.symbols);
        out.imports.extend(extraction.imports);
        out.capability_used = if !attempted {
            CapabilityKind::Lexical
        } else if out.symbols.is_empty() {
            CapabilityKind::StructuralParse
        } else {
            CapabilityKind::SymbolExtraction
        };
        out.structurally_complete = extraction.complete;
        out
    }
}

impl AemAnalyzer {
    /// Retrieval units exactly as the file would get without this plugin:
    /// OSGi `.cfg.json` files keep the JSON analyzer's canonical,
    /// JSON-pointer-addressed subtree units (falling back to generic chunks
    /// when the JSON is malformed, as dispatch does); everything else uses
    /// `GenericAnalyzer`. The plugin only ever adds structure.
    fn unit_output(&self, input: AnalyzerInput) -> AnalyzerOutput {
        let is_cfg_json = input.path.file_name().is_some_and(|n| {
            n.to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".cfg.json")
        });
        if self.kind != AemKind::Osgi || !is_cfg_json {
            return GenericAnalyzer::new().analyze(input);
        }
        let retry_bytes = match &input.content {
            AnalyzerContent::FullBytes(b) => Some(AnalyzerContent::FullBytes(b.clone())),
            AnalyzerContent::RedactedBytes(b) => Some(AnalyzerContent::RedactedBytes(b.clone())),
            AnalyzerContent::StreamingHandle(_) => None,
        };
        let Some(retry_content) = retry_bytes else {
            return GenericAnalyzer::new().analyze(input);
        };
        let retry_input = AnalyzerInput {
            file_occurrence_id: input.file_occurrence_id,
            path: input.path.clone(),
            content: retry_content,
            language_hint: input.language_hint.clone(),
            file_type: input.file_type,
            size_bytes: input.size_bytes,
            is_partial_scan: input.is_partial_scan,
            cancellation_token: input.cancellation_token.clone(),
            resource_budget: input.resource_budget.clone(),
        };
        let json = crate::json::JsonAnalyzer::new().analyze(input);
        if json.has_errors() {
            let mut generic = GenericAnalyzer::new().analyze(retry_input);
            generic.diagnostics.extend(
                json.diagnostics
                    .into_iter()
                    .map(|d| AnalyzerDiagnostic::warning(d.code, d.message)),
            );
            generic
        } else {
            json
        }
    }
}

// ---------------------------------------------------------------------------
// Extraction plumbing
// ---------------------------------------------------------------------------

struct Ctx<'a> {
    text: &'a str,
    lines: LineIndex,
    path: &'a Path,
    started: Instant,
    budget_ms: u64,
    cancel: &'a crate::CancellationToken,
}

impl Ctx<'_> {
    fn span(&self, start: usize, end: usize) -> SourceSpan {
        let (sl, sc) = self.lines.position(start);
        let (el, ec) = self.lines.position(end);
        SourceSpan::new(sl, sc, el, ec)
    }

    fn hash(&self, start: usize, end: usize) -> String {
        let end = end.min(self.text.len());
        let start = start.min(end);
        blake3::hash(&self.text.as_bytes()[start..end])
            .to_hex()
            .to_string()
    }
}

/// Byte offset → 0-based (line, column) lookup.
struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Self { starts }
    }

    fn position(&self, offset: usize) -> (u32, u32) {
        let line = self
            .starts
            .partition_point(|&s| s <= offset)
            .saturating_sub(1);
        let col = offset - self.starts[line];
        (
            u32::try_from(line).unwrap_or(u32::MAX),
            u32::try_from(col).unwrap_or(u32::MAX),
        )
    }
}

struct Extraction {
    nodes: Vec<StructuralNodeSpec>,
    symbols: Vec<SymbolSpec>,
    imports: Vec<ImportSpec>,
    diagnostics: Vec<AnalyzerDiagnostic>,
    complete: bool,
    cap: usize,
    checks: usize,
    stopped: bool,
}

impl Extraction {
    fn new(cap: usize) -> Self {
        Self {
            nodes: Vec::new(),
            symbols: Vec::new(),
            imports: Vec::new(),
            diagnostics: Vec::new(),
            complete: true,
            cap,
            checks: 0,
            stopped: false,
        }
    }

    /// `false` once the entity cap, time budget or cancellation stops
    /// extraction; records the reason exactly once.
    fn may_continue(&mut self, ctx: &Ctx<'_>) -> bool {
        if self.stopped {
            return false;
        }
        self.checks += 1;
        let entities = self.nodes.len() + self.imports.len();
        let reason = if entities >= self.cap {
            Some((
                diagnostic_codes::RESOURCE_EXHAUSTED,
                format!(
                    "entity cap ({}) reached; structural output is PARTIAL",
                    self.cap
                ),
            ))
        } else if self.checks.is_multiple_of(CHECK_EVERY) {
            if ctx.cancel.is_cancelled() {
                Some((
                    diagnostic_codes::CANCELLED,
                    "AEM structural analysis cancelled mid-extraction; output is PARTIAL".into(),
                ))
            } else if ctx.started.elapsed().as_millis() as u64 >= ctx.budget_ms {
                Some((
                    diagnostic_codes::RESOURCE_EXHAUSTED,
                    "time budget exhausted during AEM structural extraction; output is PARTIAL"
                        .into(),
                ))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((code, msg)) = reason {
            self.diagnostics
                .push(AnalyzerDiagnostic::warning(code, msg));
            self.complete = false;
            self.stopped = true;
            return false;
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn push_symbol(
        &mut self,
        ctx: &Ctx<'_>,
        node_type: &str,
        qualified_name: String,
        short_name: String,
        kind: SymbolKind,
        start: usize,
        end: usize,
        parent_index: Option<usize>,
        metadata: Option<serde_json::Value>,
    ) -> usize {
        let span = ctx.span(start, end);
        let node_index = self.nodes.len();
        self.nodes.push(StructuralNodeSpec {
            node_type: node_type.to_owned(),
            name: short_name.clone(),
            span,
            parent_index,
            structural_identity: blake3::hash(
                format!("aem|{node_type}|{qualified_name}").as_bytes(),
            )
            .to_hex()
            .to_string(),
            content_hash: ctx.hash(start, end),
            metadata_json: metadata.map(|m| m.to_string()),
        });
        self.symbols.push(SymbolSpec {
            qualified_name,
            short_name,
            kind,
            definition_span: span,
            is_public: true,
            disambiguator: None,
            signature: None,
            visibility: None,
            is_definition: true,
            node_index: Some(node_index),
        });
        node_index
    }

    fn push_import(&mut self, ctx: &Ctx<'_>, raw: &str, kind: &str, start: usize, end: usize) {
        let raw = raw.trim();
        if raw.is_empty() {
            return;
        }
        self.imports.push(ImportSpec {
            raw_specifier: raw.to_owned(),
            resolved_path: None,
            span: ctx.span(start, end),
            import_kind: kind.to_owned(),
        });
    }

    /// Give every repeated `(qualified_name, kind)` a deterministic
    /// disambiguator (sibling JCR nodes may legitimately repeat names in
    /// different XML files, but never within one symbol table).
    fn disambiguate(&mut self) {
        let mut seen: std::collections::HashMap<(String, SymbolKind), u32> =
            std::collections::HashMap::new();
        for s in &mut self.symbols {
            let n = seen.entry((s.qualified_name.clone(), s.kind)).or_insert(0);
            if *n > 0 {
                s.disambiguator = Some(format!("dup:{n}"));
            }
            *n += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// JCR path helpers
// ---------------------------------------------------------------------------

/// Decode a FileVault platform name (`_cq_dialog` → `cq:dialog`).
fn decode_platform_name(segment: &str) -> String {
    if let Some(rest) = segment.strip_prefix('_')
        && let Some((ns, local)) = rest.split_once('_')
        && !ns.is_empty()
        && !local.is_empty()
        && ns.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return format!("{ns}:{local}");
    }
    segment.to_owned()
}

/// Directory segments after the last `jcr_root`, decoded, from an absolute
/// or relative path, using the original spelling.
fn jcr_dir_segments(path: &Path) -> Option<Vec<String>> {
    let components: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let root = components
        .iter()
        .rposition(|c| c.eq_ignore_ascii_case("jcr_root"))?;
    let dirs = &components[root + 1..components.len().saturating_sub(1)];
    Some(dirs.iter().map(|s| decode_platform_name(s)).collect())
}

/// JCR path of the node a content-XML file describes: the containing folder
/// for `.content.xml`, otherwise the folder plus the file stem.
fn jcr_node_path(path: &Path) -> String {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut segments = jcr_dir_segments(path).unwrap_or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .map(|n| vec![n.to_string_lossy().into_owned()])
            .unwrap_or_default()
    });
    if !file_name.eq_ignore_ascii_case(".content.xml") {
        let stem = file_name
            .rsplit_once('.')
            .map_or(file_name.as_str(), |(stem, _)| stem);
        segments.push(decode_platform_name(stem));
    }
    format!("/{}", segments.join("/"))
}

// ---------------------------------------------------------------------------
// JCR content XML
// ---------------------------------------------------------------------------

struct XmlTag<'a> {
    name: &'a str,
    attrs: Vec<(&'a str, String)>,
    start: usize,
    end: usize,
    self_closing: bool,
}

enum XmlEvent<'a> {
    Open(XmlTag<'a>),
    Close,
}

/// Tolerant XML tag scanner: skips comments, CDATA, processing instructions
/// and declarations; never panics on malformed input.
struct XmlScanner<'a> {
    text: &'a str,
    pos: usize,
    malformed: bool,
}

impl<'a> XmlScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            pos: 0,
            malformed: false,
        }
    }

    fn skip_past(&mut self, from: usize, terminator: &str) -> bool {
        match self.text[from..].find(terminator) {
            Some(i) => {
                self.pos = from + i + terminator.len();
                true
            }
            None => {
                self.malformed = true;
                self.pos = self.text.len();
                false
            }
        }
    }

    fn next_event(&mut self) -> Option<XmlEvent<'a>> {
        let text = self.text;
        loop {
            let lt = self.pos + text[self.pos..].find('<')?;
            let rest = &text[lt..];
            if rest.starts_with("<!--") {
                if !self.skip_past(lt + 4, "-->") {
                    return None;
                }
            } else if rest.starts_with("<![CDATA[") {
                if !self.skip_past(lt + 9, "]]>") {
                    return None;
                }
            } else if rest.starts_with("<?") {
                if !self.skip_past(lt + 2, "?>") {
                    return None;
                }
            } else if rest.starts_with("<!") {
                if !self.skip_past(lt + 2, ">") {
                    return None;
                }
            } else if rest.starts_with("</") {
                if !self.skip_past(lt + 2, ">") {
                    return None;
                }
                return Some(XmlEvent::Close);
            } else {
                return self.open_tag(lt).map(XmlEvent::Open);
            }
        }
    }

    fn open_tag(&mut self, lt: usize) -> Option<XmlTag<'a>> {
        let text = self.text;
        let bytes = text.as_bytes();
        let mut i = lt + 1;
        let name_start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'/'
            && bytes[i] != b'>'
        {
            i += 1;
        }
        let name = &text[name_start..i];
        let mut attrs = Vec::new();
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= bytes.len() {
                self.malformed = true;
                self.pos = text.len();
                return None;
            }
            match bytes[i] {
                b'>' => {
                    self.pos = i + 1;
                    return Some(XmlTag {
                        name,
                        attrs,
                        start: lt,
                        end: i + 1,
                        self_closing: false,
                    });
                }
                b'/' if bytes.get(i + 1) == Some(&b'>') => {
                    self.pos = i + 2;
                    return Some(XmlTag {
                        name,
                        attrs,
                        start: lt,
                        end: i + 2,
                        self_closing: true,
                    });
                }
                _ => {}
            }
            let attr_start = i;
            while i < bytes.len()
                && bytes[i] != b'='
                && !bytes[i].is_ascii_whitespace()
                && bytes[i] != b'>'
                && bytes[i] != b'/'
            {
                i += 1;
            }
            let attr_name = &text[attr_start..i];
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if bytes.get(i) != Some(&b'=') {
                if attr_name.is_empty() {
                    // Stray character (e.g. a lone '/'): step over it.
                    i += 1;
                }
                continue;
            }
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let Some(&quote) = bytes.get(i).filter(|&&q| q == b'"' || q == b'\'') else {
                self.malformed = true;
                self.pos = text.len();
                return None;
            };
            let value_start = i + 1;
            let Some(len) = text[value_start..].find(quote as char) else {
                self.malformed = true;
                self.pos = text.len();
                return None;
            };
            attrs.push((
                attr_name,
                decode_xml_entities(&text[value_start..value_start + len]),
            ));
            i = value_start + len + 1;
        }
    }
}

fn decode_xml_entities(raw: &str) -> String {
    if !raw.contains('&') {
        return raw.to_owned();
    }
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Strip a JCR type hint (`{Boolean}true` → `true`).
fn strip_jcr_type(value: &str) -> &str {
    if value.starts_with('{')
        && let Some(end) = value.find('}')
    {
        return &value[end + 1..];
    }
    value
}

/// Values of a JCR multi-value property (`[a,b]` → `["a","b"]`).
fn jcr_values(value: &str) -> Vec<String> {
    let value = strip_jcr_type(value).trim();
    match value.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        Some(inner) => inner
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        None if value.is_empty() => Vec::new(),
        None => vec![value.to_owned()],
    }
}

fn kind_for_primary_type(primary_type: Option<&str>) -> SymbolKind {
    match primary_type.unwrap_or("") {
        "cq:Component" | "cq:Template" | "cq:Page" => SymbolKind::Class,
        "cq:ClientLibraryFolder" | "sling:Folder" | "sling:OrderedFolder" | "nt:folder" => {
            SymbolKind::Module
        }
        "sling:OsgiConfig" => SymbolKind::Constant,
        _ => SymbolKind::Variable,
    }
}

const JCR_METADATA_KEYS: &[&str] = &[
    "jcr:primaryType",
    "jcr:mixinTypes",
    "jcr:title",
    "sling:resourceType",
    "sling:resourceSuperType",
    "cq:template",
    "componentGroup",
    "categories",
    "embed",
    "dependencies",
    "allowProxy",
];

fn extract_jcr(ctx: &Ctx<'_>, ex: &mut Extraction) {
    let base_path = jcr_node_path(ctx.path);
    let mut scanner = XmlScanner::new(ctx.text);
    // (node index, JCR path) of each open element.
    let mut stack: Vec<(usize, String)> = Vec::new();

    while let Some(event) = scanner.next_event() {
        let tag = match event {
            XmlEvent::Close => {
                stack.pop();
                continue;
            }
            XmlEvent::Open(tag) => tag,
        };
        if tag.name.is_empty() {
            continue;
        }
        if !ex.may_continue(ctx) {
            break;
        }

        let (path, short_name) = match stack.last() {
            None => (
                base_path.clone(),
                base_path.rsplit('/').next().unwrap_or("").to_owned(),
            ),
            Some((_, parent)) => {
                let name = decode_platform_name(tag.name);
                (format!("{}/{}", parent.trim_end_matches('/'), name), name)
            }
        };
        let attr = |key: &str| {
            tag.attrs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_str())
        };
        let primary_type = attr("jcr:primaryType").map(strip_jcr_type);

        let mut metadata = serde_json::Map::new();
        metadata.insert("jcrPath".into(), serde_json::Value::String(path.clone()));
        for key in JCR_METADATA_KEYS {
            if let Some(v) = attr(key) {
                let stripped = strip_jcr_type(v).trim();
                let json = if stripped.starts_with('[') && stripped.ends_with(']') {
                    serde_json::Value::from(jcr_values(v))
                } else {
                    serde_json::Value::String(stripped.to_owned())
                };
                metadata.insert((*key).into(), json);
            }
        }

        let node_index = ex.push_symbol(
            ctx,
            "JCR_NODE",
            path.clone(),
            short_name,
            kind_for_primary_type(primary_type),
            tag.start,
            tag.end,
            stack.last().map(|(i, _)| *i),
            Some(serde_json::Value::Object(metadata)),
        );

        for (key, kind) in [
            ("sling:resourceSuperType", "SLING_RESOURCE_SUPER_TYPE"),
            ("sling:resourceType", "SLING_RESOURCE_TYPE"),
            ("cq:template", "CQ_TEMPLATE"),
        ] {
            if let Some(v) = attr(key) {
                ex.push_import(ctx, strip_jcr_type(v), kind, tag.start, tag.end);
            }
        }
        if primary_type == Some("cq:ClientLibraryFolder") {
            for (key, kind) in [
                ("embed", "CLIENTLIB_EMBED"),
                ("dependencies", "CLIENTLIB_DEPENDENCY"),
            ] {
                if let Some(v) = attr(key) {
                    for category in jcr_values(v) {
                        ex.push_import(ctx, &category, kind, tag.start, tag.end);
                    }
                }
            }
        }

        if !tag.self_closing {
            stack.push((node_index, path));
        }
    }

    if scanner.malformed {
        ex.diagnostics.push(AnalyzerDiagnostic::warning(
            diagnostic_codes::MALFORMED_INPUT,
            "JCR content XML is malformed; structural output is PARTIAL",
        ));
        ex.complete = false;
    }
    ex.disambiguate();
}

// ---------------------------------------------------------------------------
// HTL
// ---------------------------------------------------------------------------

/// Literal path inside an HTL expression (`${'x.html' @ ...}` → `x.html`);
/// plain attribute values are returned unchanged.
fn htl_literal(value: &str) -> Option<String> {
    let v = value.trim();
    let Some(expr) = v.strip_prefix("${").and_then(|e| e.strip_suffix('}')) else {
        return (!v.is_empty()).then(|| v.to_owned());
    };
    let head = expr.split('@').next().unwrap_or("").trim();
    let quoted = head
        .strip_prefix('\'')
        .and_then(|h| h.strip_suffix('\''))
        .or_else(|| head.strip_prefix('"').and_then(|h| h.strip_suffix('"')));
    quoted.map(str::to_owned)
}

/// `resourceType='x/y'` option inside an HTL expression.
fn htl_option(value: &str, option: &str) -> Option<String> {
    let idx = value.find(option)?;
    let rest = value[idx + option.len()..]
        .trim_start()
        .strip_prefix('=')?
        .trim_start();
    let quote = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let body = &rest[1..];
    body.find(quote).map(|end| body[..end].to_owned())
}

fn extract_htl(ctx: &Ctx<'_>, ex: &mut Extraction) {
    let text = ctx.text;
    let bytes = text.as_bytes();
    let template_path = ctx
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut from = 0usize;

    while let Some(rel) = text[from..].find("data-sly-") {
        let attr_start = from + rel;
        let mut i = attr_start + "data-sly-".len();
        while i < bytes.len()
            && (bytes[i].is_ascii_alphanumeric()
                || bytes[i] == b'.'
                || bytes[i] == b'-'
                || bytes[i] == b'_')
        {
            i += 1;
        }
        let attr_name = &text[attr_start + "data-sly-".len()..i];
        let (block, identifier) = attr_name
            .split_once('.')
            .map_or((attr_name, None), |(b, id)| (b, Some(id)));

        let mut j = i;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let mut value: Option<&str> = None;
        let mut attr_end = i;
        if bytes.get(j) == Some(&b'=') {
            j += 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if let Some(&q) = bytes.get(j).filter(|&&q| q == b'"' || q == b'\'')
                && let Some(len) = text[j + 1..].find(q as char)
            {
                value = Some(&text[j + 1..j + 1 + len]);
                attr_end = j + 1 + len + 1;
            }
        }
        from = attr_end.max(i).max(attr_start + 1);

        if !ex.may_continue(ctx) {
            break;
        }
        match (block, identifier, value) {
            ("template", Some(name), _) if !name.is_empty() => {
                ex.push_symbol(
                    ctx,
                    "HTL_TEMPLATE",
                    format!("{template_path}#{name}"),
                    name.to_owned(),
                    SymbolKind::Function,
                    attr_start,
                    attr_end,
                    None,
                    None,
                );
            }
            ("use", identifier, Some(v)) => {
                let target = htl_literal(v).unwrap_or_else(|| v.trim().to_owned());
                if let Some(name) = identifier.filter(|n| !n.is_empty()) {
                    ex.push_symbol(
                        ctx,
                        "HTL_USE",
                        format!("{template_path}#{name}"),
                        name.to_owned(),
                        SymbolKind::Variable,
                        attr_start,
                        attr_end,
                        None,
                        Some(serde_json::json!({ "use": target })),
                    );
                }
                ex.push_import(ctx, &target, "HTL_USE", attr_start, attr_end);
            }
            ("include", _, Some(v)) => {
                if let Some(target) = htl_literal(v) {
                    ex.push_import(ctx, &target, "HTL_INCLUDE", attr_start, attr_end);
                }
            }
            ("resource", _, Some(v)) => {
                if let Some(rt) = htl_option(v, "resourceType") {
                    ex.push_import(ctx, &rt, "SLING_RESOURCE_TYPE", attr_start, attr_end);
                } else if let Some(target) = htl_literal(v) {
                    ex.push_import(ctx, &target, "HTL_RESOURCE", attr_start, attr_end);
                }
            }
            _ => {}
        }
    }
    ex.disambiguate();
}

// ---------------------------------------------------------------------------
// OSGi configuration
// ---------------------------------------------------------------------------

fn extract_osgi(ctx: &Ctx<'_>, ex: &mut Extraction) {
    let file_name = ctx
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let lower = file_name.to_ascii_lowercase();
    let is_json = lower.ends_with(".cfg.json");
    let stem = if is_json {
        &file_name[..file_name.len() - ".cfg.json".len()]
    } else {
        file_name
            .rsplit_once('.')
            .map_or(file_name.as_str(), |(stem, _)| stem)
    };
    let (pid, factory) = match stem.split_once('~') {
        Some((pid, name)) => (pid, Some(name)),
        None => (stem, None),
    };
    let run_modes: Vec<String> = ctx
        .path
        .parent()
        .and_then(|p| p.file_name())
        .map(|d| d.to_string_lossy().into_owned())
        .and_then(|d| {
            d.strip_prefix("config.")
                .map(|modes| modes.split('.').map(str::to_owned).collect())
        })
        .unwrap_or_default();

    let root = ex.push_symbol(
        ctx,
        "OSGI_CONFIG",
        pid.to_owned(),
        pid.rsplit('.').next().unwrap_or(pid).to_owned(),
        SymbolKind::Module,
        0,
        ctx.text.len(),
        None,
        Some(serde_json::json!({
            "pid": pid,
            "factoryName": factory,
            "runModes": run_modes,
            "format": if is_json { "cfg.json" } else { "config" },
        })),
    );

    let keys: Vec<(String, usize, usize)> = if is_json {
        match serde_json::from_str::<serde_json::Value>(ctx.text) {
            Ok(serde_json::Value::Object(map)) => map
                .keys()
                .map(|key| {
                    // Key order in the parsed map is not file order, so each
                    // key is located independently. JSON forbids duplicate
                    // keys, so the first quoted occurrence is the key itself
                    // unless an earlier value happens to contain it verbatim —
                    // an acceptable approximation for an anchor span.
                    let needle = format!("\"{key}\"");
                    match ctx.text.find(&needle) {
                        Some(start) => (key.clone(), start, start + needle.len()),
                        None => (key.clone(), 0, 0),
                    }
                })
                .collect(),
            Ok(_) | Err(_) => {
                ex.diagnostics.push(AnalyzerDiagnostic::warning(
                    diagnostic_codes::MALFORMED_INPUT,
                    "OSGi .cfg.json is not a JSON object; properties were not extracted",
                ));
                ex.complete = false;
                Vec::new()
            }
        }
    } else {
        let mut offset = 0usize;
        let mut keys = Vec::new();
        for line in ctx.text.split_inclusive('\n') {
            let trimmed = line.trim_start();
            let lead = line.len() - trimmed.len();
            if !trimmed.starts_with('#')
                && let Some((key, _)) = trimmed.split_once('=')
            {
                let key = key.trim();
                if !key.is_empty() {
                    keys.push((key.to_owned(), offset + lead, offset + lead + key.len()));
                }
            }
            offset += line.len();
        }
        keys
    };

    for (key, start, end) in keys {
        if !ex.may_continue(ctx) {
            break;
        }
        ex.push_symbol(
            ctx,
            "OSGI_PROPERTY",
            format!("{pid}#{key}"),
            key,
            SymbolKind::Variable,
            start,
            end,
            Some(root),
            None,
        );
    }
}

// ---------------------------------------------------------------------------
// Clientlib manifests
// ---------------------------------------------------------------------------

fn extract_clientlib(ctx: &Ctx<'_>, ex: &mut Extraction) {
    let folder = jcr_dir_segments(ctx.path)
        .map(|s| format!("/{}", s.join("/")))
        .unwrap_or_default();
    let manifest = ctx
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ex.push_symbol(
        ctx,
        "CLIENTLIB_MANIFEST",
        format!("{folder}/{manifest}"),
        manifest,
        SymbolKind::Module,
        0,
        ctx.text.len(),
        None,
        Some(serde_json::json!({ "clientlib": folder })),
    );

    let mut base = String::new();
    let mut offset = 0usize;
    for line in ctx.text.split_inclusive('\n') {
        let entry = line.trim();
        let start = offset + (line.len() - line.trim_start().len());
        let end = start + entry.len();
        offset += line.len();
        if entry.is_empty() {
            continue;
        }
        if let Some(b) = entry.strip_prefix("#base=") {
            base = b.trim().trim_end_matches('/').to_owned();
            continue;
        }
        if entry.starts_with('#') || entry.starts_with("//") {
            continue;
        }
        if !ex.may_continue(ctx) {
            break;
        }
        let target = if base.is_empty() || entry.starts_with('/') {
            entry.to_owned()
        } else {
            format!("{base}/{entry}")
        };
        ex.push_import(ctx, &target, "CLIENTLIB_FILE", start, end);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ResourceBudget;
    use attic_core::{FileOccurrenceId, FileType};
    use std::path::PathBuf;

    fn run(kind: AemKind, path: &str, text: &str) -> AnalyzerOutput {
        AemAnalyzer::new(kind).analyze(AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from(path),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            language_hint: Some(kind.tag().to_owned()),
            file_type: FileType::Other,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: crate::CancellationToken::default(),
            resource_budget: ResourceBudget::default(),
        })
    }

    fn qualified(out: &AnalyzerOutput) -> Vec<&str> {
        out.symbols
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect()
    }

    fn imports(out: &AnalyzerOutput) -> Vec<(&str, &str)> {
        out.imports
            .iter()
            .map(|i| (i.import_kind.as_str(), i.raw_specifier.as_str()))
            .collect()
    }

    #[test]
    fn classification_is_path_based_and_cross_platform() {
        let c = |p: &str| classify(&PluginPath::new(p));
        let base = "ui.apps/src/main/content/jcr_root/apps/site";
        assert_eq!(
            c(&format!("{base}/components/button/.content.xml")),
            Some(AemKind::Jcr)
        );
        assert_eq!(
            c(&format!("{base}/components/button/_cq_dialog/.content.xml")),
            Some(AemKind::Jcr)
        );
        assert_eq!(
            c(&format!("{base}/components/button/button.html")),
            Some(AemKind::Htl)
        );
        assert_eq!(
            c(&format!("{base}/clientlibs/base/js.txt")),
            Some(AemKind::Clientlib)
        );
        assert_eq!(
            c(&format!("{base}/config.author/com.acme.Svc.config")),
            Some(AemKind::Osgi)
        );
        assert_eq!(
            c("any/where/com.acme.Svc~prod.cfg.json"),
            Some(AemKind::Osgi)
        );
        assert_eq!(
            c(r"UI.APPS\SRC\MAIN\CONTENT\JCR_ROOT\APPS\SITE\X.HTML"),
            Some(AemKind::Htl)
        );
        assert_eq!(
            c("web/index.html"),
            None,
            "plain HTML outside jcr_root is not AEM"
        );
        assert_eq!(c("pom.xml"), None);
        assert_eq!(c("package.json"), None);
        assert_eq!(c(&format!("{base}/components/button/button.js")), None);
    }

    #[test]
    fn jcr_component_nodes_resource_types_and_dialog_children() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- component definition -->
<jcr:root xmlns:jcr="http://www.jcp.org/jcr/1.0" xmlns:cq="http://www.day.com/jcr/cq/1.0"
    jcr:primaryType="cq:Component"
    jcr:title="Button"
    sling:resourceSuperType="core/wcm/components/button/v2/button"
    componentGroup="Site - Content">
    <cq:dialog jcr:primaryType="nt:unstructured" sling:resourceType="cq/gui/components/authoring/dialog">
        <content jcr:primaryType="nt:unstructured"/>
    </cq:dialog>
</jcr:root>
"#;
        let out = run(
            AemKind::Jcr,
            "/r/ui.apps/src/main/content/jcr_root/apps/site/components/button/.content.xml",
            xml,
        );
        assert_eq!(
            qualified(&out),
            [
                "/apps/site/components/button",
                "/apps/site/components/button/cq:dialog",
                "/apps/site/components/button/cq:dialog/content",
            ]
        );
        assert_eq!(out.symbols[0].kind, SymbolKind::Class);
        assert_eq!(out.structural_nodes[1].parent_index, Some(0));
        assert_eq!(out.structural_nodes[2].parent_index, Some(1));
        let meta: serde_json::Value =
            serde_json::from_str(out.structural_nodes[0].metadata_json.as_deref().unwrap())
                .unwrap();
        assert_eq!(meta["jcr:primaryType"], "cq:Component");
        assert_eq!(meta["jcr:title"], "Button");
        assert_eq!(
            imports(&out),
            [
                (
                    "SLING_RESOURCE_SUPER_TYPE",
                    "core/wcm/components/button/v2/button"
                ),
                ("SLING_RESOURCE_TYPE", "cq/gui/components/authoring/dialog"),
            ]
        );
        assert!(out.structurally_complete);
        assert_eq!(out.analyzer_id, "aem-jcr");
        assert!(
            !out.retrieval_units.is_empty(),
            "lexical coverage must be preserved"
        );
    }

    #[test]
    fn jcr_platform_folder_names_are_decoded() {
        let out = run(
            AemKind::Jcr,
            "jcr_root/apps/site/components/button/_cq_dialog/.content.xml",
            r#"<jcr:root jcr:primaryType="nt:unstructured"/>"#,
        );
        assert_eq!(qualified(&out), ["/apps/site/components/button/cq:dialog"]);
    }

    #[test]
    fn jcr_clientlib_categories_embed_and_dependencies() {
        let out = run(
            AemKind::Jcr,
            "jcr_root/apps/site/clientlibs/site/.content.xml",
            r#"<jcr:root jcr:primaryType="cq:ClientLibraryFolder" categories="[site.base]"
                 embed="[core.wcm.components.button.v2]" dependencies="[granite.utils,jquery]" allowProxy="{Boolean}true"/>"#,
        );
        assert_eq!(out.symbols[0].kind, SymbolKind::Module);
        assert_eq!(
            imports(&out),
            [
                ("CLIENTLIB_EMBED", "core.wcm.components.button.v2"),
                ("CLIENTLIB_DEPENDENCY", "granite.utils"),
                ("CLIENTLIB_DEPENDENCY", "jquery"),
            ]
        );
        let meta: serde_json::Value =
            serde_json::from_str(out.structural_nodes[0].metadata_json.as_deref().unwrap())
                .unwrap();
        assert_eq!(meta["categories"], serde_json::json!(["site.base"]));
        assert_eq!(meta["allowProxy"], "true");
    }

    #[test]
    fn malformed_jcr_xml_is_partial_never_an_error() {
        let out = run(
            AemKind::Jcr,
            "jcr_root/apps/x/.content.xml",
            r#"<jcr:root jcr:primaryType="cq:Component" jcr:title="unterminated"#,
        );
        assert!(!out.structurally_complete);
        assert!(
            !out.has_errors(),
            "tolerant parsing must never fail dispatch"
        );
        assert!(!out.retrieval_units.is_empty());
    }

    #[test]
    fn htl_templates_use_include_and_resource_targets() {
        let html = r#"<div data-sly-use.model="com.acme.core.models.Button"
     data-sly-use.tpl="core/wcm/components/commons/v1/templates.html">
  <template data-sly-template.item="${@ label}"><span>${label}</span></template>
  <sly data-sly-include="${'partials/header.html'}"/>
  <sly data-sly-resource="${'child' @ resourceType='site/components/teaser'}"/>
  <sly data-sly-call="${tpl.placeholder @ isEmpty=true}"/>
</div>"#;
        let out = run(
            AemKind::Htl,
            "jcr_root/apps/site/components/button/button.html",
            html,
        );
        assert_eq!(
            qualified(&out),
            ["button.html#model", "button.html#tpl", "button.html#item"]
        );
        assert_eq!(out.symbols[2].kind, SymbolKind::Function);
        assert_eq!(
            imports(&out),
            [
                ("HTL_USE", "com.acme.core.models.Button"),
                ("HTL_USE", "core/wcm/components/commons/v1/templates.html"),
                ("HTL_INCLUDE", "partials/header.html"),
                ("SLING_RESOURCE_TYPE", "site/components/teaser"),
            ]
        );
    }

    #[test]
    fn osgi_cfg_json_pid_factory_run_modes_and_properties() {
        let out = run(
            AemKind::Osgi,
            "jcr_root/apps/site/osgiconfig/config.author.prod/com.acme.core.Mailer~marketing.cfg.json",
            "{\n  \"host\": \"smtp.example.com\",\n  \"port\": 587\n}\n",
        );
        assert_eq!(
            qualified(&out),
            [
                "com.acme.core.Mailer",
                "com.acme.core.Mailer#host",
                "com.acme.core.Mailer#port"
            ]
        );
        let meta: serde_json::Value =
            serde_json::from_str(out.structural_nodes[0].metadata_json.as_deref().unwrap())
                .unwrap();
        assert_eq!(meta["factoryName"], "marketing");
        assert_eq!(meta["runModes"], serde_json::json!(["author", "prod"]));
        assert_eq!(out.structural_nodes[1].parent_index, Some(0));
        assert_eq!(out.symbols[1].definition_span.start_line, 1);
    }

    #[test]
    fn osgi_felix_config_properties() {
        let out = run(
            AemKind::Osgi,
            "jcr_root/apps/site/config/org.apache.sling.Foo.config",
            "# comment\nenabled=B\"true\"\npaths=[\"/content\"]\n",
        );
        assert_eq!(
            qualified(&out),
            [
                "org.apache.sling.Foo",
                "org.apache.sling.Foo#enabled",
                "org.apache.sling.Foo#paths"
            ]
        );
    }

    #[test]
    fn invalid_cfg_json_is_partial_not_error() {
        let out = run(AemKind::Osgi, "x/com.acme.Svc.cfg.json", "[1, 2]");
        assert!(!out.structurally_complete);
        assert!(!out.has_errors());
        assert_eq!(qualified(&out), ["com.acme.Svc"]);
    }

    /// `.cfg.json` keeps exactly the JSON analyzer's retrieval units (canonical
    /// subtree chunks with JSON-pointer addressing); AEM only adds structure.
    #[test]
    fn cfg_json_units_match_the_json_analyzer() {
        let path = "jcr_root/apps/site/osgiconfig/config/com.acme.Svc.cfg.json";
        let text = "{\n  \"a\": {\"x\": 1, \"y\": [1, 2, 3]},\n  \"b\": \"value\"\n}\n";
        let aem = run(AemKind::Osgi, path, text);
        let json = crate::json::JsonAnalyzer::new().analyze(AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from(path),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            language_hint: None,
            file_type: FileType::Json,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: crate::CancellationToken::default(),
            resource_budget: ResourceBudget::default(),
        });
        let texts = |o: &AnalyzerOutput| -> Vec<String> {
            o.retrieval_units
                .iter()
                .map(|u| u.retrieval_text.clone())
                .collect()
        };
        assert!(!json.retrieval_units.is_empty());
        assert_eq!(texts(&aem), texts(&json));
        assert_eq!(
            qualified(&aem),
            ["com.acme.Svc", "com.acme.Svc#a", "com.acme.Svc#b"]
        );
    }

    /// Malformed `.cfg.json` still yields lexical units via the generic
    /// fallback, never an error that would fail dispatch.
    #[test]
    fn malformed_cfg_json_falls_back_to_generic_units() {
        let out = run(AemKind::Osgi, "x/com.acme.Broken.cfg.json", "{ not json");
        assert!(!out.has_errors());
        assert!(!out.retrieval_units.is_empty());
    }

    #[test]
    fn clientlib_manifest_honours_base() {
        let out = run(
            AemKind::Clientlib,
            "jcr_root/apps/site/clientlibs/base/js.txt",
            "#base=js\nutil.js\n\n// legacy\nmain.js\n#base=../vendor\nlib.js\n",
        );
        assert_eq!(qualified(&out), ["/apps/site/clientlibs/base/js.txt"]);
        assert_eq!(
            imports(&out),
            [
                ("CLIENTLIB_FILE", "js/util.js"),
                ("CLIENTLIB_FILE", "js/main.js"),
                ("CLIENTLIB_FILE", "../vendor/lib.js"),
            ]
        );
    }

    #[test]
    fn cancelled_input_is_lexical_only() {
        let token = crate::CancellationToken::default();
        token.cancel();
        let text = r#"<jcr:root jcr:primaryType="cq:Component"/>"#;
        let out = AemAnalyzer::new(AemKind::Jcr).analyze(AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("jcr_root/apps/x/.content.xml"),
            content: AnalyzerContent::FullBytes(text.as_bytes().to_vec()),
            language_hint: Some(AEM_JCR_TAG.to_owned()),
            file_type: FileType::Other,
            size_bytes: text.len() as u64,
            is_partial_scan: false,
            cancellation_token: token,
            resource_budget: ResourceBudget::default(),
        });
        assert!(out.symbols.is_empty());
        assert!(!out.structurally_complete);
        assert_eq!(out.capability_used, CapabilityKind::Lexical);
    }

    #[test]
    fn entity_cap_marks_output_partial() {
        let mut xml = String::from("<jcr:root jcr:primaryType=\"nt:unstructured\">");
        for i in 0..50 {
            xml.push_str(&format!("<n{i} jcr:primaryType=\"nt:unstructured\"/>"));
        }
        xml.push_str("</jcr:root>");
        let out = AemAnalyzer::new(AemKind::Jcr).analyze(AnalyzerInput {
            file_occurrence_id: FileOccurrenceId::new_v4(),
            path: PathBuf::from("jcr_root/apps/x/.content.xml"),
            content: AnalyzerContent::FullBytes(xml.clone().into_bytes()),
            language_hint: Some(AEM_JCR_TAG.to_owned()),
            file_type: FileType::Other,
            size_bytes: xml.len() as u64,
            is_partial_scan: false,
            cancellation_token: crate::CancellationToken::default(),
            resource_budget: ResourceBudget {
                max_retrieval_units: 10,
                ..ResourceBudget::default()
            },
        });
        assert_eq!(out.symbols.len(), 10);
        assert!(!out.structurally_complete);
        assert!(
            out.diagnostics
                .iter()
                .any(|d| d.code == diagnostic_codes::RESOURCE_EXHAUSTED)
        );
    }
}
