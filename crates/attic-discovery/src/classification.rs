//! Priority classification for discovered files.
//!
//! Implements the `DiscoveryPriority` assignment algorithm from
//! the discovery contract §Priority Classification.

use crate::policy::{DiscoveryPolicy, DiscoveryPriority, GlobRule};

/// Default exclusion patterns.
///
/// Paths matching these patterns receive `DiscoveryPriority::Ignored`
/// when `DiscoveryPolicy::default_exclusions = true`.
///
/// These are matched against repo-relative forward-slash paths.
const DEFAULT_IGNORED_PATTERNS: &[&str] = &[
    ".git/",
    "node_modules/",
    "coverage/",
    ".nyc_output/",
    ".cache/",
    "__pycache__/",
    ".pytest_cache/",
    ".venv/",
    "venv/",
    "env/",
    ".env/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    ".parcel-cache/",
    ".gradle/",
    "target/",
    "build/",
    "dist/",
    "out/",
    "_site/",
    ".tox/",
];

/// Patterns for directories that are low-priority by default (not excluded).
const DEFAULT_LOW_PRIORITY_PREFIXES: &[&str] = &[
    "vendor/",
    "generated/",
    "fixtures/",
    "snapshots/",
];

/// Patterns for directories that are high-priority by default.
const DEFAULT_HIGH_PRIORITY_PREFIXES: &[&str] = &[
    "src/",
    "lib/",
    "app/",
    "services/",
    "knowledge/",
    "cmd/",
    "pkg/",
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Classify a repo-relative forward-slash path according to the policy.
///
/// Returns `DiscoveryPriority::Ignored` if the file should not be indexed.
/// Returns `DiscoveryPriority::HighPriority`, `Normal`, or `LowPriority`
/// otherwise.
///
/// Rule application order (per discovery contract):
/// 1. Security exclusions — handled before this function is called.
/// 2. Default Attic exclusions (if `policy.default_exclusions`).
/// 3. Attic workspace/repository-level exclude rules.
/// 4. Attic repository-level include rules (can re-include excluded).
/// 5. Priority overrides.
pub fn classify(path: &str, policy: &DiscoveryPolicy) -> DiscoveryPriority {
    // Evaluate include rules once — a matching include rule rescues the path
    // from any exclusion and signals explicit intent, which we honour with at
    // least Normal priority (skipping the default low-priority heuristics).
    let explicitly_included = policy_includes(path, policy);

    // --- Step 2: Default exclusions ---
    if policy.default_exclusions && matches_default_ignored(path) && !explicitly_included {
        return DiscoveryPriority::Ignored;
    }

    // --- Step 3: Attic exclude rules ---
    if matches_glob_rules(path, &policy.attic_exclude_rules) && !explicitly_included {
        return DiscoveryPriority::Ignored;
    }

    // --- Step 5: Custom priority overrides ---
    for rule in &policy.custom_priority_overrides {
        if glob_matches(&rule.pattern, path) {
            return rule.priority;
        }
    }

    // --- Explicitly re-included paths default to Normal ---
    // (They bypassed exclusion by explicit policy intent; do not down-grade to
    // LowPriority based on heuristics like "vendor/" prefix.)
    if explicitly_included {
        // Still honour high-priority defaults.
        for prefix in DEFAULT_HIGH_PRIORITY_PREFIXES {
            if path.starts_with(prefix) || path == prefix.trim_end_matches('/') {
                return DiscoveryPriority::HighPriority;
            }
        }
        return DiscoveryPriority::Normal;
    }

    // --- Default priority by path prefix ---
    default_priority(path)
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Returns `true` if `path` matches any of the default IGNORED patterns.
fn matches_default_ignored(path: &str) -> bool {
    for pattern in DEFAULT_IGNORED_PATTERNS {
        // Pattern ends with '/' → directory prefix match
        if let Some(dir) = pattern.strip_suffix('/') {
            // Matches if path starts with "dir/" or is exactly "dir"
            if path == dir
                || path.starts_with(&format!("{dir}/"))
                || is_path_component(path, dir)
            {
                return true;
            }
        } else if path == *pattern {
            return true;
        }
    }
    false
}

/// Check whether `segment` appears as a path component in `path`.
///
/// e.g. `is_path_component("a/node_modules/b/c", "node_modules")` → `true`
fn is_path_component(path: &str, segment: &str) -> bool {
    for part in path.split('/') {
        if part == segment {
            return true;
        }
    }
    false
}

/// Returns `true` if any rule in `attic_include_rules` matches `path`.
///
/// Include rules represent positive intent ("always index this"), regardless
/// of the `negation` flag (which is a gitignore-style concept not used here).
fn policy_includes(path: &str, policy: &DiscoveryPolicy) -> bool {
    for rule in &policy.attic_include_rules {
        if glob_matches(&rule.pattern, path) {
            return true;
        }
    }
    false
}

/// Returns `true` if any of the non-negation rules in `rules` match `path`.
fn matches_glob_rules(path: &str, rules: &[GlobRule]) -> bool {
    for rule in rules {
        if !rule.negation && glob_matches(&rule.pattern, path) {
            return true;
        }
    }
    false
}

/// Determine default priority based on well-known path prefixes.
fn default_priority(path: &str) -> DiscoveryPriority {
    // High-priority prefixes
    for prefix in DEFAULT_HIGH_PRIORITY_PREFIXES {
        if path.starts_with(prefix) || path == prefix.trim_end_matches('/') {
            return DiscoveryPriority::HighPriority;
        }
    }

    // Low-priority prefixes
    for prefix in DEFAULT_LOW_PRIORITY_PREFIXES {
        if path.starts_with(prefix) || path == prefix.trim_end_matches('/') {
            return DiscoveryPriority::LowPriority;
        }
    }

    // Files in well-known test/docs directories
    for prefix in &["tests/", "test/", "__tests__/", "spec/", "docs/", "config/", "migrations/"] {
        if path.starts_with(prefix) || is_path_component(path, prefix.trim_end_matches('/')) {
            return DiscoveryPriority::Normal;
        }
    }

    DiscoveryPriority::Normal
}

/// Simple glob matching for `**`, `*`, and `?` patterns.
///
/// Supports:
/// - `**` — matches any number of path segments (including zero)
/// - `*` — matches any sequence of characters that does not include `/`
/// - `?` — matches any single character that is not `/`
/// - Literal characters
///
/// Pattern is matched against the full repo-relative path.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    glob_match_recursive(pattern.as_bytes(), path.as_bytes())
}

fn glob_match_recursive(pattern: &[u8], text: &[u8]) -> bool {
    let mut pi = 0usize;
    let mut ti = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;

    loop {
        if pi < pattern.len() && ti < text.len() {
            match pattern[pi] {
                b'?' => {
                    // '?' does not match '/'
                    if text[ti] != b'/' {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
                b'*' => {
                    // Check for '**'
                    if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                        // '**' — skip all '/'-separated segments
                        // Use a recursive approach for the ** case
                        let rest_pattern = &pattern[pi + 2..];
                        // trim leading '/' from rest_pattern if present
                        let rest_pattern = if rest_pattern.first() == Some(&b'/') {
                            &rest_pattern[1..]
                        } else {
                            rest_pattern
                        };
                        // Try matching rest_pattern against text[ti..] at all positions
                        for start in ti..=text.len() {
                            if glob_match_recursive(rest_pattern, &text[start..]) {
                                return true;
                            }
                            // Advance past next '/' boundary or end
                            if start < text.len() && text[start] == b'/' {
                                // try next char
                            }
                            if start < text.len() {
                                // continue iterating
                            } else {
                                break;
                            }
                        }
                        return false;
                    } else {
                        // Single '*' — matches any chars except '/'
                        star_pi = Some(pi);
                        star_ti = ti;
                        // advance pattern; try matching from current text position
                        pi += 1;
                        continue;
                    }
                }
                c => {
                    if text[ti] == c {
                        pi += 1;
                        ti += 1;
                        continue;
                    }
                }
            }
        } else if pi == pattern.len() && ti == text.len() {
            return true;
        } else if pi == pattern.len() {
            // pattern exhausted but text remains
            // Unless star can absorb remaining — check if last pattern char was *
            return false;
        } else if ti == text.len() {
            // text exhausted; pattern may have trailing *
            while pi < pattern.len() && pattern[pi] == b'*' {
                pi += 1;
            }
            return pi == pattern.len();
        }

        // Backtrack to last single '*' position if available
        if let Some(sp) = star_pi {
            // '*' cannot match '/'
            if star_ti < text.len() && text[star_ti] != b'/' {
                star_ti += 1;
                pi = sp + 1;
                ti = star_ti;
                continue;
            }
        }

        return false;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::DiscoveryPolicy;

    fn default_policy() -> DiscoveryPolicy {
        DiscoveryPolicy::default_git()
    }

    #[test]
    fn node_modules_is_ignored() {
        assert_eq!(classify("node_modules/lodash/index.js", &default_policy()), DiscoveryPriority::Ignored);
        assert_eq!(classify("node_modules/react/package.json", &default_policy()), DiscoveryPriority::Ignored);
    }

    #[test]
    fn target_dir_is_ignored() {
        assert_eq!(classify("target/debug/attic", &default_policy()), DiscoveryPriority::Ignored);
        assert_eq!(classify("target/release/attic.exe", &default_policy()), DiscoveryPriority::Ignored);
    }

    #[test]
    fn build_dist_out_ignored() {
        assert_eq!(classify("build/output/bundle.js", &default_policy()), DiscoveryPriority::Ignored);
        assert_eq!(classify("dist/index.js", &default_policy()), DiscoveryPriority::Ignored);
        assert_eq!(classify("out/server.js", &default_policy()), DiscoveryPriority::Ignored);
    }

    #[test]
    fn coverage_ignored() {
        assert_eq!(classify("coverage/lcov.info", &default_policy()), DiscoveryPriority::Ignored);
        assert_eq!(classify(".nyc_output/coverage.json", &default_policy()), DiscoveryPriority::Ignored);
    }

    #[test]
    fn src_is_high_priority() {
        assert_eq!(classify("src/main.rs", &default_policy()), DiscoveryPriority::HighPriority);
        assert_eq!(classify("lib/utils.ts", &default_policy()), DiscoveryPriority::HighPriority);
    }

    #[test]
    fn tests_are_normal() {
        assert_eq!(classify("tests/integration.rs", &default_policy()), DiscoveryPriority::Normal);
        assert_eq!(classify("test/unit/foo.js", &default_policy()), DiscoveryPriority::Normal);
    }

    #[test]
    fn vendor_is_low_priority() {
        assert_eq!(classify("vendor/lib/foo.js", &default_policy()), DiscoveryPriority::LowPriority);
    }

    #[test]
    fn generated_is_low_priority() {
        assert_eq!(classify("generated/proto/foo.rs", &default_policy()), DiscoveryPriority::LowPriority);
    }

    #[test]
    fn fixtures_are_low_priority() {
        assert_eq!(classify("fixtures/git/repo1/file.rs", &default_policy()), DiscoveryPriority::LowPriority);
    }

    #[test]
    fn migrations_are_normal() {
        assert_eq!(classify("migrations/0001_initial.sql", &default_policy()), DiscoveryPriority::Normal);
    }

    #[test]
    fn attic_exclude_rule_ignores_path() {
        let mut policy = default_policy();
        policy.attic_exclude_rules.push(GlobRule::exclude("vendor/**"));
        assert_eq!(classify("vendor/foo/bar.js", &policy), DiscoveryPriority::Ignored);
    }

    #[test]
    fn attic_include_rule_re_includes_excluded() {
        let mut policy = default_policy();
        policy.attic_exclude_rules.push(GlobRule::exclude("vendor/**"));
        policy.attic_include_rules.push(GlobRule::include("vendor/critical-lib/**"));
        assert_eq!(classify("vendor/critical-lib/index.js", &policy), DiscoveryPriority::Normal);
        assert_eq!(classify("vendor/other/index.js", &policy), DiscoveryPriority::Ignored);
    }

    #[test]
    fn glob_star_matches_within_segment() {
        assert!(glob_matches("src/*.rs", "src/main.rs"));
        assert!(!glob_matches("src/*.rs", "src/sub/main.rs"));
    }

    #[test]
    fn glob_double_star_matches_across_segments() {
        assert!(glob_matches("vendor/**", "vendor/foo/bar.js"));
        assert!(glob_matches("vendor/**", "vendor/foo/bar/baz.js"));
        assert!(!glob_matches("vendor/**", "src/main.rs"));
    }

    #[test]
    fn default_exclusions_disabled_includes_target() {
        let mut policy = default_policy();
        policy.default_exclusions = false;
        // target/ no longer ignored
        assert_ne!(classify("target/debug/bin", &policy), DiscoveryPriority::Ignored);
    }

    #[test]
    fn custom_priority_override_applied() {
        use crate::policy::PriorityRule;
        let mut policy = default_policy();
        policy.custom_priority_overrides.push(PriorityRule {
            pattern: "docs/**".to_string(),
            priority: DiscoveryPriority::HighPriority,
        });
        assert_eq!(classify("docs/architecture.md", &policy), DiscoveryPriority::HighPriority);
    }
}
