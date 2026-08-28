//! V1 query taxonomy and the deterministic query router (classifier).
//!
//! Contract: `docs/ARCHITECTURE.md` §Definitions + invariant 6:
//! classification is a pure function of the input text — no network, no LLM,
//! no wall-clock. Uncertain classifications are surfaced with an explicit
//! confidence level and competing signals; they never silently become
//! unjustified certainty.

use attic_evidence::str_enum;
use serde::{Deserialize, Serialize};

/// Hard cap on accepted query length. Longer inputs are rejected as
/// malformed rather than truncated silently.
pub const MAX_QUERY_CHARS: usize = 512;

str_enum! {
    /// The system's classification of a query's intent.
    ///
    /// The ten approved contract types plus two Phase-4 additions
    /// (`EXACT_LOOKUP`, `CROSS_REPO_QUESTION`) documented in ADR-012.
    QueryType {
        /// "Where is config.properties?" / exact path or identifier lookup.
        ExactLookup => "EXACT_LOOKUP",
        /// "Where is X defined?"
        DefinitionLookup => "DEFINITION_LOOKUP",
        /// "Show me callers/callees of X".
        SymbolNavigation => "SYMBOL_NAVIGATION",
        /// "What value is setting Y?"
        ConfigurationLookup => "CONFIGURATION_LOOKUP",
        /// "How does subsystem Z work / behave?"
        ArchitectureExplanation => "ARCHITECTURE_EXPLANATION",
        /// "Why does X fail / behave unexpectedly?"
        DebuggingRootCause => "DEBUGGING_ROOT_CAUSE",
        /// "What depends on library X?"
        DependencyQuestion => "DEPENDENCY_QUESTION",
        /// "What would break if I modify X?"
        ImpactAnalysis => "IMPACT_ANALYSIS",
        /// Workspace-wide question spanning several repositories.
        CrossRepoQuestion => "CROSS_REPO_QUESTION",
        /// "What does test suite X verify?"
        TestBehavior => "TEST_BEHAVIOR",
        /// "What does the project documentation say about X?"
        KnowledgeQuestion => "KNOWLEDGE_QUESTION",
        /// No specific intent recognized; unconstrained search.
        GenericSearch => "GENERIC_SEARCH",
    }
}

str_enum! {
    /// Confidence of the classification itself (not of any answer).
    ClassificationConfidence {
        /// Unambiguous keyword evidence for exactly one type.
        High => "HIGH",
        /// Primary signal present but weaker/overlapping.
        Medium => "MEDIUM",
        /// Only weak or contradictory signals; effectively uncertain.
        Low => "LOW",
    }
}

/// Terms and hints extracted from the raw query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedQuery {
    /// Searchable terms (lowercased, deduplicated, order preserved).
    pub terms: Vec<String>,
    /// Path-like token if one appears (contains `/` or a known extension).
    pub path_hint: Option<String>,
    /// Identifier most likely to be a symbol name (longest identifier token).
    pub symbol_hint: Option<String>,
    /// Config key hint when the query names a setting.
    pub config_key_hint: Option<String>,
}

/// The classifier output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Classification {
    /// Classified intent.
    pub query_type: QueryType,
    /// How certain the classification is.
    pub confidence: ClassificationConfidence,
    /// Signals that fired, in evaluation order (inspectable trace).
    pub matched_signals: Vec<String>,
    /// Competing candidate types that also matched some signal.
    pub competing_types: Vec<QueryType>,
    /// Extracted searchable structure.
    pub extracted: ExtractedQuery,
}

/// Classify a raw user query. Deterministic: same input → same output.
///
/// Malformed input (empty, oversized, control characters) is rejected —
/// untrusted repository-adjacent text must never reach SQL/FTS unfiltered.
pub fn classify(raw: &str) -> Result<Classification, crate::error::RetrievalError> {
    if raw.trim().is_empty() {
        return Err(crate::error::RetrievalError::InvalidQuery(
            "query is empty".into(),
        ));
    }
    if raw.chars().count() > MAX_QUERY_CHARS {
        return Err(crate::error::RetrievalError::InvalidQuery(format!(
            "query exceeds {MAX_QUERY_CHARS} characters"
        )));
    }
    // Control characters are REJECTED outright (untrusted input): silent
    // stripping could turn an attack string into a different query.
    if raw
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r')
    {
        return Err(crate::error::RetrievalError::InvalidQuery(
            "query contains control characters".into(),
        ));
    }
    let sanitized: String = raw.chars().collect();

    let lower = sanitized.to_lowercase();
    let mut signals = Vec::new();
    let mut competing: Vec<QueryType> = Vec::new();

    let extracted = extract(&sanitized, &lower);

    // ── Ordered heuristic rules ────────────────────────────────────────────
    // Each rule records its signal; first strong match wins. Overlaps are
    // recorded as competing types so uncertainty stays visible.
    let mut detected: Option<(QueryType, ClassificationConfidence)> = None;
    let mut consider =
        |ty: QueryType,
         conf: ClassificationConfidence,
         signal: &str,
         detected: &mut Option<(QueryType, ClassificationConfidence)>| {
            signals.push(signal.to_owned());
            match detected {
                None => *detected = Some((ty, conf)),
                Some((existing, existing_conf)) => {
                    if *existing != ty {
                        competing.push(ty);
                        // A HIGH-confidence rule beats an earlier MEDIUM hit.
                        if conf == ClassificationConfidence::High
                            && *existing_conf != ClassificationConfidence::High
                        {
                            *detected = Some((ty, conf));
                        }
                    }
                }
            }
        };

    // Exact/path lookup: explicit path token without definition wording.
    if extracted.path_hint.is_some() && !lower.contains("defin") && !lower.contains("call") {
        consider(
            QueryType::ExactLookup,
            ClassificationConfidence::High,
            "path_token_without_intent_words",
            &mut detected,
        );
    }
    // Definition lookup.
    if contains_any(
        &lower,
        &[
            "where is",
            "where's ",
            "definition of",
            "defined in",
            "declared in",
            "locate the definition",
        ],
    ) || (lower.starts_with("find the definition")
        || lower.starts_with("show me the definition"))
    {
        consider(
            QueryType::DefinitionLookup,
            ClassificationConfidence::High,
            "definition_keywords",
            &mut detected,
        );
    }
    // Symbol navigation.
    if contains_any(
        &lower,
        &[
            "caller",
            "callee",
            "who calls",
            "what calls",
            "usages of",
            "usage of",
            "references to",
            "all references",
        ],
    ) {
        consider(
            QueryType::SymbolNavigation,
            ClassificationConfidence::High,
            "navigation_keywords",
            &mut detected,
        );
    }
    // Configuration lookup.
    if contains_any(
        &lower,
        &[
            "configured",
            "configuration",
            "config value",
            "setting",
            "port is",
            "database url",
            "environment variable",
            "env var",
            ".yml",
            ".yaml",
            ".toml",
            ".properties",
            "timeout value",
        ],
    ) {
        consider(
            QueryType::ConfigurationLookup,
            ClassificationConfidence::High,
            "configuration_keywords",
            &mut detected,
        );
    }
    // Debugging / root cause (before architecture so "why does X fail" wins).
    if contains_any(
        &lower,
        &[
            "why does",
            "why is",
            "fails",
            "failing",
            "failure",
            "root cause",
            "unexpected",
            "broken",
            "debug",
            "error when",
            "exception when",
            "not working",
        ],
    ) {
        consider(
            QueryType::DebuggingRootCause,
            ClassificationConfidence::High,
            "debugging_keywords",
            &mut detected,
        );
    }
    // Impact analysis.
    if contains_any(
        &lower,
        &[
            "impact",
            "what would break",
            "break if",
            "affected by changing",
            "ripple",
            "blast radius",
            "who uses this",
            "downstream effects",
        ],
    ) {
        consider(
            QueryType::ImpactAnalysis,
            ClassificationConfidence::High,
            "impact_keywords",
            &mut detected,
        );
    }
    // Dependency question.
    if contains_any(
        &lower,
        &[
            "depends on",
            "depend on",
            "depending on",
            "dependency",
            "which services use",
            "which repos use",
            "package depend",
            "library depend",
            "transitive",
        ],
    ) {
        consider(
            QueryType::DependencyQuestion,
            ClassificationConfidence::High,
            "dependency_keywords",
            &mut detected,
        );
    }
    // Explicit cross-repository scope wording.
    if contains_any(
        &lower,
        &[
            "cross-repo",
            "cross repo",
            "across repositories",
            "across repos",
            "other repositories",
        ],
    ) {
        consider(
            QueryType::CrossRepoQuestion,
            ClassificationConfidence::High,
            "cross_repo_keywords",
            &mut detected,
        );
    }
    // Test behavior.
    if contains_any(
        &lower,
        &[
            "test suite",
            "tests cover",
            "test cover",
            "what scenarios",
            "unit test",
            "integration test",
            "verify in tests",
            "tested",
        ],
    ) {
        consider(
            QueryType::TestBehavior,
            ClassificationConfidence::High,
            "test_keywords",
            &mut detected,
        );
    }
    // Knowledge question.
    if contains_any(
        &lower,
        &[
            "runbook",
            "documented",
            "documentation say",
            "docs say",
            "knowledge",
            "project notes",
            "architecture doc",
            "adr",
            "design doc",
        ],
    ) {
        consider(
            QueryType::KnowledgeQuestion,
            ClassificationConfidence::High,
            "knowledge_keywords",
            &mut detected,
        );
    }
    // Architecture / behavior explanation.
    if contains_any(
        &lower,
        &[
            "how does",
            "how is",
            "explain",
            "walk me through",
            "architecture",
            "overview of",
            "behavior of",
            "behaves",
            "flow of",
            "works",
        ],
    ) {
        consider(
            QueryType::ArchitectureExplanation,
            ClassificationConfidence::Medium,
            "explanation_keywords",
            &mut detected,
        );
    }
    // Bare identifier → definition-ish lookup (medium confidence).
    if detected.is_none() && extracted.symbol_hint.is_some() {
        consider(
            QueryType::DefinitionLookup,
            ClassificationConfidence::Medium,
            "bare_identifier_defaults_to_definition",
            &mut detected,
        );
    }

    let (query_type, confidence) =
        detected.unwrap_or((QueryType::GenericSearch, ClassificationConfidence::Low));
    if query_type == QueryType::GenericSearch {
        signals.push("no_matching_signal_defaulted_to_generic".to_owned());
    }
    if !competing.is_empty() && confidence == ClassificationConfidence::High {
        // Overlap keeps certainty honest: downgrade to Medium when other
        // types also matched.
        return Ok(finish(
            query_type,
            ClassificationConfidence::Medium,
            signals,
            competing,
            extracted,
        ));
    }

    Ok(finish(
        query_type, confidence, signals, competing, extracted,
    ))
}

fn finish(
    query_type: QueryType,
    confidence: ClassificationConfidence,
    matched_signals: Vec<String>,
    competing_types: Vec<QueryType>,
    extracted: ExtractedQuery,
) -> Classification {
    Classification {
        query_type,
        confidence,
        matched_signals,
        competing_types,
        extracted,
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Extract searchable terms and structural hints. Purely lexical.
fn extract(sanitized: &str, lower: &str) -> ExtractedQuery {
    // Stop words removed deterministically.
    const STOP: &[&str] = &[
        "the", "a", "an", "is", "are", "was", "were", "of", "in", "on", "for", "to", "and", "or",
        "how", "does", "do", "what", "where", "which", "who", "show", "me", "find", "tell",
        "about", "with", "at", "from", "by", "it", "this", "that", "there", "their",
    ];
    let mut terms: Vec<String> = Vec::new();
    for word in lower.split(|c: char| {
        c.is_whitespace() || c == ',' || c == '?' || c == '.' || c == '!' || c == ';' || c == ':'
    }) {
        let w =
            word.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == '(' || c == ')');
        if w.len() < 2 || STOP.contains(&w) {
            continue;
        }
        if !terms.iter().any(|t| t == w) {
            terms.push(w.to_owned());
        }
    }

    // Path hint: first token containing '/' or ending in a known extension.
    let path_hint = sanitized
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| c == '"' || c == '\'' || c == '`' || c == ',' || c == '?')
        })
        .find(|t| t.contains('/') || looks_like_path(t))
        .map(str::to_owned);

    // Symbol hint: longest CamelCase / snake_case / dotted identifier token.
    let symbol_hint = sanitized
        .split_whitespace()
        .map(|t| {
            t.trim_matches(|c: char| {
                c == '"'
                    || c == '\''
                    || c == '`'
                    || c == ','
                    || c == '?'
                    || c == '('
                    || c == ')'
                    || c == '.'
            })
        })
        .filter(|t| looks_like_identifier(t))
        .max_by_key(|t| t.len())
        .map(str::to_owned);

    // Config key hint: the original-case token right after "setting".
    let lower_tokens: Vec<&str> = lower.split_whitespace().collect();
    let orig_tokens: Vec<&str> = sanitized.split_whitespace().collect();
    let config_key_hint = match lower_tokens
        .iter()
        .position(|w| *w == "setting" || *w == "variable")
    {
        Some(i) => {
            let idx = if lower_tokens.get(i + 1).is_some_and(|w| *w == "of") {
                i + 2
            } else {
                i + 1
            };
            orig_tokens.get(idx).map(|t| t.trim_matches('?').to_owned())
        }
        None => None,
    };

    ExtractedQuery {
        terms,
        path_hint,
        symbol_hint,
        config_key_hint,
    }
}

fn looks_like_path(t: &str) -> bool {
    const EXTS: &[&str] = &[
        ".rs",
        ".java",
        ".py",
        ".go",
        ".ts",
        ".tsx",
        ".js",
        ".jsx",
        ".c",
        ".cpp",
        ".h",
        ".md",
        ".yml",
        ".yaml",
        ".toml",
        ".json",
        ".properties",
        ".xml",
        ".ini",
        ".sql",
        ".sh",
    ];
    EXTS.iter().any(|e| t.to_lowercase().ends_with(e))
}

fn looks_like_identifier(t: &str) -> bool {
    if t.is_empty() || t.len() > 128 {
        return false;
    }
    let has_case_mix =
        t.chars().any(|c| c.is_ascii_uppercase()) && t.chars().any(|c| c.is_ascii_lowercase());
    let has_structure = t.contains('_') || t.contains('.') || t.chars().any(|c| c.is_ascii_digit());
    let ident_start = t
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_');
    let all_ok = t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
    ident_start && all_ok && (has_case_mix || has_structure)
}
