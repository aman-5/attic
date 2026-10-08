//! Pluggable analyzer catalog.
//!
//! An [`AnalyzerPlugin`] bundles everything Attic needs to support one
//! language or platform: which files it claims (a language hint derived from
//! the repository-relative path) and which analyzers it registers. A
//! [`PluginCatalog`] composes plugins into an [`AnalyzerRegistry`] according
//! to an [`AnalyzerSelection`] (normally `attic.toml [indexing] analyzers /
//! disabled_analyzers`).
//!
//! Adding a language or platform (Swift, AEM, …) is therefore one plugin
//! value plus one catalog entry: indexing, storage, retrieval and the server
//! never change. Third-party plugins can be added to a catalog at runtime via
//! [`PluginCatalog::with_plugin`].
//!
//! Hints are selection-independent on purpose: a disabled plugin may still
//! claim a path, but its tag is simply not registered, so
//! [`AnalyzerRegistry::select`] falls through to the `FileType` map and then
//! to the `GenericAnalyzer`. Disabling a plugin never makes a file
//! unsearchable.

use std::collections::BTreeSet;
use std::sync::{Arc, OnceLock};

use crate::api::Analyzer;
use crate::generic::GenericAnalyzer;
use crate::registry::AnalyzerRegistry;
use crate::structural::{
    c, cpp, csharp, dockerfile, go, java, javascript, kotlin, lua, php, python, ruby, rust, scala,
    swift, typescript,
};

// ---------------------------------------------------------------------------
// Path view handed to plugins
// ---------------------------------------------------------------------------

/// Normalized view of a repository-relative path.
///
/// Separators are normalized to `/`, and every accessor is ASCII
/// case-insensitive, so a plugin classifies `Foo.JAVA`, `src\x.ts` and
/// `src/x.ts` identically on Windows, macOS and Linux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginPath {
    lower: String,
}

impl PluginPath {
    /// Build from a repository-relative (or absolute) path string.
    pub fn new(path: &str) -> Self {
        Self {
            lower: path.replace('\\', "/").to_ascii_lowercase(),
        }
    }

    /// The normalized, lowercased path.
    pub fn as_str(&self) -> &str {
        &self.lower
    }

    /// Lowercased final path segment.
    pub fn file_name(&self) -> &str {
        self.lower.rsplit('/').next().unwrap_or("")
    }

    /// Lowercased extension without the dot. A dot-file with no further dot
    /// (`.gitignore`) has no extension; `.content.xml` has `xml`.
    pub fn extension(&self) -> Option<&str> {
        let name = self.file_name();
        let dot = name.rfind('.')?;
        if dot == 0 {
            return None;
        }
        let ext = &name[dot + 1..];
        (!ext.is_empty()).then_some(ext)
    }

    /// Lowercased directory segments (everything except the file name).
    pub fn dir_segments(&self) -> impl Iterator<Item = &str> {
        let mut parts: Vec<&str> = self.lower.split('/').filter(|s| !s.is_empty()).collect();
        parts.pop();
        parts.into_iter()
    }

    /// `true` when any directory segment equals `segment` (case-insensitive).
    pub fn has_dir_segment(&self, segment: &str) -> bool {
        let segment = segment.to_ascii_lowercase();
        self.dir_segments().any(|s| s == segment)
    }
}

// ---------------------------------------------------------------------------
// Plugin contract
// ---------------------------------------------------------------------------

/// One language or platform integration.
///
/// Implementations must be cheap to query: `language_hint` runs once per
/// indexed file.
pub trait AnalyzerPlugin: Send + Sync {
    /// Stable, config-facing identifier (`"java"`, `"swift"`, `"aem"`).
    /// Lowercase ASCII; used in `attic.toml [indexing] analyzers`.
    fn id(&self) -> &'static str;

    /// One-line human description (surfaced in diagnostics).
    fn description(&self) -> &'static str;

    /// Language tag this plugin claims for `path`, or `None`. Returned tags
    /// must be registered by [`register`](Self::register) via
    /// [`AnalyzerRegistry::register_for_language`]. Plugins routed purely by
    /// `FileType` (e.g. Java) return `None`.
    fn language_hint(&self, path: &PluginPath) -> Option<&'static str>;

    /// Register this plugin's analyzers.
    fn register(&self, registry: &mut AnalyzerRegistry);
}

// ---------------------------------------------------------------------------
// Selection + errors
// ---------------------------------------------------------------------------

/// Which plugins to enable. Normalized (trimmed, lowercased, sorted,
/// de-duplicated) so equal selections compare and hash equal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AnalyzerSelection {
    enabled: Vec<String>,
    disabled: Vec<String>,
}

impl AnalyzerSelection {
    /// Every plugin in the catalog.
    pub fn all() -> Self {
        Self::default()
    }

    /// `enabled` empty = every plugin; `disabled` is applied afterwards.
    pub fn new<E, D, S1, S2>(enabled: E, disabled: D) -> Self
    where
        E: IntoIterator<Item = S1>,
        D: IntoIterator<Item = S2>,
        S1: AsRef<str>,
        S2: AsRef<str>,
    {
        fn norm<S: AsRef<str>>(ids: impl IntoIterator<Item = S>) -> Vec<String> {
            let set: BTreeSet<String> = ids
                .into_iter()
                .map(|s| s.as_ref().trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            set.into_iter().collect()
        }
        Self {
            enabled: norm(enabled),
            disabled: norm(disabled),
        }
    }

    /// Explicitly enabled ids (empty = all).
    pub fn enabled(&self) -> &[String] {
        &self.enabled
    }

    /// Explicitly disabled ids.
    pub fn disabled(&self) -> &[String] {
        &self.disabled
    }

    /// `true` for the all-plugins default.
    pub fn is_all(&self) -> bool {
        self.enabled.is_empty() && self.disabled.is_empty()
    }

    fn includes(&self, id: &str) -> bool {
        (self.enabled.is_empty() || self.enabled.iter().any(|e| e == id))
            && !self.disabled.iter().any(|d| d == id)
    }
}

/// Invalid analyzer configuration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnalyzerConfigError {
    /// A selection named a plugin the catalog does not contain.
    #[error("unknown analyzer plugin '{id}' (known plugins: {known})")]
    UnknownPlugin {
        /// The unrecognized id.
        id: String,
        /// Comma-separated list of valid ids.
        known: String,
    },
    /// Two plugins in one catalog share an id.
    #[error("analyzer plugin id '{0}' is registered more than once")]
    DuplicatePlugin(String),
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// An ordered set of plugins. Order matters only for hints: the first plugin
/// that claims a path wins, so path-specific platform plugins (AEM) precede
/// extension-based language plugins.
#[derive(Clone)]
pub struct PluginCatalog {
    plugins: Vec<Arc<dyn AnalyzerPlugin>>,
}

impl std::fmt::Debug for PluginCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginCatalog")
            .field("plugins", &self.plugin_ids())
            .finish()
    }
}

impl PluginCatalog {
    /// A catalog with no plugins (the registry will hold only the generic
    /// fallback).
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    /// Every built-in plugin, in hint-precedence order.
    pub fn builtin() -> Self {
        static BUILTIN: OnceLock<Vec<Arc<dyn AnalyzerPlugin>>> = OnceLock::new();
        Self {
            plugins: BUILTIN.get_or_init(builtin_plugins).clone(),
        }
    }

    /// Append a plugin (e.g. a third-party language). Fails on a duplicate
    /// id so two plugins can never silently shadow each other.
    pub fn with_plugin(
        mut self,
        plugin: Arc<dyn AnalyzerPlugin>,
    ) -> Result<Self, AnalyzerConfigError> {
        if self.plugins.iter().any(|p| p.id() == plugin.id()) {
            return Err(AnalyzerConfigError::DuplicatePlugin(plugin.id().to_owned()));
        }
        self.plugins.push(plugin);
        Ok(self)
    }

    /// Plugin ids in catalog order.
    pub fn plugin_ids(&self) -> Vec<&'static str> {
        self.plugins.iter().map(|p| p.id()).collect()
    }

    /// Language tag for a repository-relative path: the first plugin that
    /// claims it, independent of which plugins are enabled.
    pub fn language_hint(&self, path: &str) -> Option<&'static str> {
        let path = PluginPath::new(path);
        self.plugins.iter().find_map(|p| p.language_hint(&path))
    }

    /// Reject selections that name unknown plugins.
    pub fn validate(&self, selection: &AnalyzerSelection) -> Result<(), AnalyzerConfigError> {
        let ids = self.plugin_ids();
        for id in selection.enabled.iter().chain(&selection.disabled) {
            if !ids.contains(&id.as_str()) {
                return Err(AnalyzerConfigError::UnknownPlugin {
                    id: id.clone(),
                    known: ids.join(", "),
                });
            }
        }
        Ok(())
    }

    /// Ids that `selection` enables, in catalog order.
    pub fn enabled_ids(
        &self,
        selection: &AnalyzerSelection,
    ) -> Result<Vec<&'static str>, AnalyzerConfigError> {
        self.validate(selection)?;
        Ok(self
            .plugins
            .iter()
            .map(|p| p.id())
            .filter(|id| selection.includes(id))
            .collect())
    }

    /// Stable fingerprint of the analyzers a selection produces. Changes
    /// whenever the effective plugin set changes, so caches keyed on it
    /// never replay output computed under a different analyzer set.
    pub fn fingerprint(
        &self,
        selection: &AnalyzerSelection,
    ) -> Result<String, AnalyzerConfigError> {
        Ok(self.enabled_ids(selection)?.join(","))
    }

    /// Build a registry with the generic fallback plus every enabled plugin.
    pub fn build_registry(
        &self,
        selection: &AnalyzerSelection,
    ) -> Result<AnalyzerRegistry, AnalyzerConfigError> {
        self.validate(selection)?;
        let mut registry =
            AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
        for plugin in self.plugins.iter().filter(|p| selection.includes(p.id())) {
            plugin.register(&mut registry);
        }
        Ok(registry)
    }

    /// Registry with every plugin in this catalog enabled (infallible: the
    /// all-plugins selection names no ids).
    pub fn build_all(&self) -> AnalyzerRegistry {
        let mut registry =
            AnalyzerRegistry::new(Arc::new(GenericAnalyzer::new()) as Arc<dyn Analyzer>);
        for plugin in &self.plugins {
            plugin.register(&mut registry);
        }
        registry
    }
}

/// Language tag for `path` from the built-in catalog.
pub fn language_hint(path: &str) -> Option<&'static str> {
    PluginCatalog::builtin().language_hint(path)
}

// ---------------------------------------------------------------------------
// Built-in plugins
// ---------------------------------------------------------------------------

/// How a built-in plugin registers its analyzers.
enum Registration {
    /// Keyed by `FileType` (tier-1 hand-written analyzers, JSON).
    Specialized(fn() -> Arc<dyn Analyzer>),
    /// Arbitrary registration (TypeScript registers `.ts` and `.tsx`).
    Custom(fn(&mut AnalyzerRegistry)),
}

/// Data-driven built-in plugin: extension / file-name hint rules plus a
/// registration strategy.
struct BuiltinPlugin {
    id: &'static str,
    description: &'static str,
    /// `(lowercase file name, tag)`, checked before extensions.
    file_names: &'static [(&'static str, &'static str)],
    /// `(lowercase extension, tag)`.
    extensions: &'static [(&'static str, &'static str)],
    registration: Registration,
}

impl BuiltinPlugin {
    fn hint_tags(&self) -> BTreeSet<&'static str> {
        self.file_names
            .iter()
            .map(|(_, tag)| *tag)
            .chain(self.extensions.iter().map(|(_, tag)| *tag))
            .collect()
    }
}

impl AnalyzerPlugin for BuiltinPlugin {
    fn id(&self) -> &'static str {
        self.id
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn language_hint(&self, path: &PluginPath) -> Option<&'static str> {
        let name = path.file_name();
        if let Some((_, tag)) = self.file_names.iter().find(|(n, _)| *n == name) {
            return Some(tag);
        }
        let ext = path.extension()?;
        self.extensions
            .iter()
            .find(|(e, _)| *e == ext)
            .map(|(_, tag)| *tag)
    }

    fn register(&self, registry: &mut AnalyzerRegistry) {
        match &self.registration {
            Registration::Specialized(make) => {
                let analyzer = make();
                registry.register_specialized(Arc::clone(&analyzer));
                for tag in self.hint_tags() {
                    registry.register_for_language(tag, Arc::clone(&analyzer));
                }
            }
            Registration::Custom(register) => register(registry),
        }
    }
}

fn register_typescript(registry: &mut AnalyzerRegistry) {
    registry.register_specialized(typescript::analyzer());
    // `.tsx` uses the JSX-aware grammar under an explicit language tag: both
    // specs declare identical capability levels, so registering both under
    // `FileType::TypeScript` would hit `best_entry`'s alphabetical tie-break
    // and select the TSX grammar for every `.ts` file. `.ts` carries the
    // `"typescript"` hint, which is not registered by tag and therefore falls
    // through to the `FileType::TypeScript` entry.
    registry.register_for_language("tsx", typescript::tsx_analyzer());
}

fn register_kotlin(registry: &mut AnalyzerRegistry) {
    registry.register_for_language("kotlin", kotlin::analyzer());
}

fn register_ruby(registry: &mut AnalyzerRegistry) {
    registry.register_for_language("ruby", ruby::analyzer());
}

fn register_lua(registry: &mut AnalyzerRegistry) {
    registry.register_for_language("lua", lua::analyzer());
}

fn register_php(registry: &mut AnalyzerRegistry) {
    registry.register_for_language("php", php::analyzer());
}

fn register_swift(registry: &mut AnalyzerRegistry) {
    registry.register_for_language("swift", swift::analyzer());
}

fn register_scala(registry: &mut AnalyzerRegistry) {
    registry.register_for_language("scala", scala::analyzer());
}

fn make_json() -> Arc<dyn Analyzer> {
    Arc::new(crate::json::JsonAnalyzer::new())
}

#[allow(dead_code)]
fn builtin_plugins() -> Vec<Arc<dyn AnalyzerPlugin>> {
    vec![
        // Path-specific platform plugins first: AEM claims `.html`, `.xml`
        // and `.cfg.json` only inside AEM layouts, ahead of generic
        // extension rules.
        Arc::new(crate::aem::AemPlugin),
        Arc::new(BuiltinPlugin {
            id: "java",
            description: "Java: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[],
            registration: Registration::Specialized(java::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "python",
            description: "Python: tree-sitter symbols, imports and calls",
            file_names: &[],
            extensions: &[],
            registration: Registration::Specialized(python::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "go",
            description: "Go: tree-sitter symbols, module imports and calls",
            file_names: &[],
            extensions: &[],
            registration: Registration::Specialized(go::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "javascript",
            description: "JavaScript: tree-sitter symbols, imports and calls",
            file_names: &[],
            extensions: &[],
            registration: Registration::Specialized(javascript::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "typescript",
            description: "TypeScript and TSX: tree-sitter symbols, imports and calls",
            file_names: &[],
            extensions: &[("tsx", "tsx"), ("ts", "typescript")],
            registration: Registration::Custom(register_typescript),
        }),
        Arc::new(BuiltinPlugin {
            id: "json",
            description: "JSON: canonical subtree chunks with JSON-pointer addressing",
            file_names: &[],
            extensions: &[],
            registration: Registration::Specialized(make_json),
        }),
        Arc::new(BuiltinPlugin {
            id: "c",
            description: "C: tree-sitter symbols, includes, macros and calls",
            file_names: &[],
            extensions: &[("c", "c"), ("h", "c")],
            registration: Registration::Specialized(c::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "cpp",
            description: "C++: tree-sitter symbols, includes, heritage and calls",
            file_names: &[],
            extensions: &[
                ("cpp", "cpp"),
                ("cc", "cpp"),
                ("cxx", "cpp"),
                ("hpp", "cpp"),
                ("hh", "cpp"),
                ("h++", "cpp"),
                ("hxx", "cpp"),
            ],
            registration: Registration::Specialized(cpp::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "ruby",
            description: "Ruby: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[("rb", "ruby")],
            registration: Registration::Custom(register_ruby),
        }),
        Arc::new(BuiltinPlugin {
            id: "csharp",
            description: "C#: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[("cs", "csharp")],
            registration: Registration::Specialized(csharp::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "scala",
            description: "Scala: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[("scala", "scala"), ("sc", "scala")],
            registration: Registration::Custom(register_scala),
        }),
        Arc::new(BuiltinPlugin {
            id: "php",
            description: "PHP: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[("php", "php")],
            registration: Registration::Custom(register_php),
        }),
        Arc::new(BuiltinPlugin {
            id: "swift",
            description: "Swift: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[("swift", "swift")],
            registration: Registration::Custom(register_swift),
        }),
        Arc::new(BuiltinPlugin {
            id: "lua",
            description: "Lua: tree-sitter symbols, require() imports and calls",
            file_names: &[],
            extensions: &[("lua", "lua")],
            registration: Registration::Custom(register_lua),
        }),
        Arc::new(BuiltinPlugin {
            id: "rust",
            description: "Rust: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[("rs", "rust")],
            registration: Registration::Specialized(rust::analyzer),
        }),
        Arc::new(BuiltinPlugin {
            id: "kotlin",
            description: "Kotlin: tree-sitter symbols, imports, heritage and calls",
            file_names: &[],
            extensions: &[("kt", "kotlin"), ("kts", "kotlin")],
            registration: Registration::Custom(register_kotlin),
        }),
        Arc::new(BuiltinPlugin {
            id: "dockerfile",
            description: "Dockerfile: tree-sitter stages, inputs and stage references",
            file_names: &[("dockerfile", "dockerfile")],
            extensions: &[("dockerfile", "dockerfile")],
            registration: Registration::Specialized(dockerfile::analyzer),
        }),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        AnalyzerCapabilities, AnalyzerDescriptor, AnalyzerInput, AnalyzerOutput, CapabilityKind,
        CapabilityLevel,
    };
    use attic_core::FileType;

    #[test]
    fn plugin_path_normalizes_separators_and_case() {
        let p = PluginPath::new(r"Src\Main\JCR_ROOT\Apps\Foo.HTML");
        assert_eq!(p.as_str(), "src/main/jcr_root/apps/foo.html");
        assert_eq!(p.file_name(), "foo.html");
        assert_eq!(p.extension(), Some("html"));
        assert!(p.has_dir_segment("jcr_root"));
        assert!(
            !p.has_dir_segment("foo.html"),
            "file name is not a directory segment"
        );
        assert_eq!(PluginPath::new(".gitignore").extension(), None);
        assert_eq!(PluginPath::new("a/.content.xml").extension(), Some("xml"));
        assert_eq!(PluginPath::new("Dockerfile").extension(), None);
    }

    #[test]
    fn builtin_ids_are_unique_lowercase_and_stable() {
        let ids = PluginCatalog::builtin().plugin_ids();
        let unique: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "duplicate plugin id in {ids:?}");
        for id in &ids {
            assert_eq!(*id, id.to_ascii_lowercase(), "plugin ids must be lowercase");
        }
        for expected in ["aem", "java", "typescript", "swift", "rust", "json"] {
            assert!(
                ids.contains(&expected),
                "missing built-in plugin {expected}"
            );
        }
    }

    /// The `attic.toml` written on first run documents every built-in
    /// plugin id, so users never have to read the source to find them.
    #[test]
    fn attic_toml_template_lists_every_builtin_plugin() {
        let template = attic_core::ATTIC_TOML_TEMPLATE;
        let marker = "# Built-in ids:";
        let start = template.find(marker).expect("template lists built-in ids") + marker.len();
        let rest = &template[start..];
        let list = &rest[..rest.find('.').expect("id list ends with a period")];
        let listed: BTreeSet<&str> = list
            .split(|c: char| c == ',' || c == '#' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .collect();
        let builtin: BTreeSet<&str> = PluginCatalog::builtin().plugin_ids().into_iter().collect();
        assert_eq!(
            listed, builtin,
            "attic.toml template id list is out of date"
        );
    }

    /// Every tag a plugin can hint must be registered by that same plugin,
    /// except `"typescript"`, which is deliberately FileType-routed.
    #[test]
    fn every_hint_tag_is_registered_by_its_plugin() {
        let samples = [
            "Dockerfile",
            "x.dockerfile",
            "x.tsx",
            "x.rs",
            "x.c",
            "x.h",
            "x.cpp",
            "x.hxx",
            "x.rb",
            "x.cs",
            "x.scala",
            "x.php",
            "x.swift",
            "x.lua",
            "ui.apps/src/main/content/jcr_root/apps/site/components/button/.content.xml",
            "ui.apps/src/main/content/jcr_root/apps/site/components/button/button.html",
            "ui.config/src/main/content/jcr_root/apps/site/osgiconfig/config/com.acme.Svc.cfg.json",
            "ui.apps/src/main/content/jcr_root/apps/site/clientlibs/base/js.txt",
        ];
        let catalog = PluginCatalog::builtin();
        for sample in samples {
            let tag = catalog
                .language_hint(sample)
                .unwrap_or_else(|| panic!("{sample} must be claimed by a built-in plugin"));
            let registry = catalog.build_all();
            assert!(
                registry.known_language_tags().contains(tag),
                "{sample} hints {tag:?}, but no analyzer is registered under that tag"
            );
        }
        assert_eq!(catalog.language_hint("x.ts"), Some("typescript"));
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        let catalog = PluginCatalog::builtin();
        assert_eq!(catalog.language_hint("App/View.SWIFT"), Some("swift"));
        assert_eq!(catalog.language_hint(r"src\lib.RS"), Some("rust"));
    }

    #[test]
    fn unknown_ids_are_rejected_with_the_known_list() {
        let catalog = PluginCatalog::builtin();
        let err = catalog
            .validate(&AnalyzerSelection::new(["swfit"], Vec::<&str>::new()))
            .unwrap_err();
        assert!(
            catalog
                .build_registry(&AnalyzerSelection::new(["swfit"], Vec::<&str>::new()))
                .is_err(),
            "build_registry must refuse unknown ids too"
        );
        match err {
            AnalyzerConfigError::UnknownPlugin { id, known } => {
                assert_eq!(id, "swfit");
                assert!(known.contains("swift"));
            }
            other => panic!("unexpected error {other:?}"),
        }
    }

    #[test]
    fn selection_enables_only_named_plugins_and_disabled_wins() {
        let catalog = PluginCatalog::builtin();
        let only_swift = AnalyzerSelection::new(["Swift ", "aem"], ["aem"]);
        assert_eq!(catalog.enabled_ids(&only_swift).unwrap(), ["swift"]);
        let reg = catalog.build_registry(&only_swift).unwrap();
        assert!(reg.known_language_tags().contains("swift"));
        assert!(!reg.known_language_tags().contains("rust"));
        let (_, is_generic) = reg.select(FileType::Java, None);
        assert!(
            is_generic,
            "java is not enabled, so .java must use the generic analyzer"
        );
    }

    #[test]
    fn disabling_a_plugin_falls_back_without_losing_the_file() {
        let catalog = PluginCatalog::builtin();
        let reg = catalog
            .build_registry(&AnalyzerSelection::new(Vec::<&str>::new(), ["swift"]))
            .unwrap();
        let (analyzer, is_generic) = reg.select(FileType::Other, Some("swift"));
        assert!(is_generic);
        assert_eq!(analyzer.descriptor().name, "generic");
    }

    #[test]
    fn selections_normalize_for_equality() {
        assert_eq!(
            AnalyzerSelection::new(["Rust", "swift", "rust"], Vec::<&str>::new()),
            AnalyzerSelection::new(["swift", "rust"], Vec::<&str>::new())
        );
        assert!(AnalyzerSelection::all().is_all());
    }

    #[test]
    fn fingerprint_tracks_the_effective_plugin_set() {
        let catalog = PluginCatalog::builtin();
        let all = catalog.fingerprint(&AnalyzerSelection::all()).unwrap();
        let no_php = catalog
            .fingerprint(&AnalyzerSelection::new(Vec::<&str>::new(), ["php"]))
            .unwrap();
        assert_ne!(all, no_php);
        assert!(all.contains("aem") && all.contains("swift"));
    }

    struct YamlPlugin;
    struct YamlAnalyzer(AnalyzerDescriptor);

    impl Analyzer for YamlAnalyzer {
        fn descriptor(&self) -> &AnalyzerDescriptor {
            &self.0
        }
        fn analyze(&self, input: AnalyzerInput) -> AnalyzerOutput {
            GenericAnalyzer::new().analyze(input)
        }
    }

    impl AnalyzerPlugin for YamlPlugin {
        fn id(&self) -> &'static str {
            "yaml-custom"
        }
        fn description(&self) -> &'static str {
            "test plugin"
        }
        fn language_hint(&self, path: &PluginPath) -> Option<&'static str> {
            matches!(path.extension(), Some("yml") | Some("yaml")).then_some("yaml-custom")
        }
        fn register(&self, registry: &mut AnalyzerRegistry) {
            registry.register_for_language(
                "yaml-custom",
                Arc::new(YamlAnalyzer(AnalyzerDescriptor {
                    name: "yaml-custom".into(),
                    version: "1".into(),
                    description: "test".into(),
                    supported_file_types: vec![],
                    capabilities: AnalyzerCapabilities::single(
                        CapabilityKind::Lexical,
                        CapabilityLevel::Basic,
                    ),
                })),
            );
        }
    }

    #[test]
    fn third_party_plugin_plugs_in_without_central_changes() {
        let catalog = PluginCatalog::builtin()
            .with_plugin(Arc::new(YamlPlugin))
            .unwrap();
        assert_eq!(catalog.language_hint("ci/deploy.YAML"), Some("yaml-custom"));
        let reg = catalog
            .build_registry(&AnalyzerSelection::new(["yaml-custom"], Vec::<&str>::new()))
            .unwrap();
        let (analyzer, is_generic) = reg.select(FileType::Yaml, Some("yaml-custom"));
        assert!(!is_generic);
        assert_eq!(analyzer.descriptor().name, "yaml-custom");
        assert!(matches!(
            catalog.with_plugin(Arc::new(YamlPlugin)),
            Err(AnalyzerConfigError::DuplicatePlugin(_))
        ));
    }
}
