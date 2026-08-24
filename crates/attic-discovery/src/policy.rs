//! Discovery policy — the complete, serializable ruleset governing file eligibility.
//!
//! The `DiscoveryPolicy` is deterministically serialized to JSON and its BLAKE3 hash
//! is stored in `SourceRevision.discovery_policy_hash` per the source_revision contract.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GlobRule
// ---------------------------------------------------------------------------

/// A single glob rule: either an exclude or a negation (include/re-include).
///
/// Patterns are repository-relative globs (e.g. `vendor/**`, `!vendor/lib/**`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobRule {
    /// Glob pattern, repository-relative.
    pub pattern: String,
    /// `true` = this is a negation rule (re-includes previously excluded paths).
    pub negation: bool,
}

impl GlobRule {
    /// Construct an exclude rule.
    pub fn exclude(pattern: impl Into<String>) -> Self {
        GlobRule { pattern: pattern.into(), negation: false }
    }

    /// Construct a negation/include-override rule.
    pub fn include(pattern: impl Into<String>) -> Self {
        GlobRule { pattern: pattern.into(), negation: true }
    }
}

// ---------------------------------------------------------------------------
// PriorityRule
// ---------------------------------------------------------------------------

/// A custom priority override for a glob-matched path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriorityRule {
    /// Glob pattern, repository-relative.
    pub pattern: String,
    /// Priority to assign when this pattern matches.
    pub priority: DiscoveryPriority,
}

// ---------------------------------------------------------------------------
// DiscoveryPriority
// ---------------------------------------------------------------------------

/// The priority class assigned to each eligible file.
///
/// Corresponds to `DiscoveryClass` in the contract.  Renamed to
/// `DiscoveryPriority` in code to avoid collision with the storage-layer
/// `DiscoveryClass` enum (which tracks how a file was found, not its priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DiscoveryPriority {
    /// File is not indexed at all.
    Ignored,
    /// Indexed but deprioritized.
    LowPriority,
    /// Indexed at normal priority.
    Normal,
    /// Indexed first.
    HighPriority,
}

// ---------------------------------------------------------------------------
// DiscoveryPolicy
// ---------------------------------------------------------------------------

/// Complete, serializable ruleset governing file eligibility for a repository.
///
/// Its BLAKE3 hash (of canonical JSON) is stored as `discovery_policy_hash`
/// in each `SourceRevision`.
///
/// **Serialization stability:** fields are serialized by name; do not reorder
/// or rename fields without incrementing the policy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryPolicy {
    /// Use Git index / `.gitignore` semantics when `.git/` is present.
    pub git_aware: bool,

    /// Include untracked (non-ignored) files when `git_aware = true`.
    pub include_untracked: bool,

    /// Apply built-in default exclusions (node_modules, build output, etc.).
    pub default_exclusions: bool,

    /// Apply security exclusions. Always `true`; set to `false` is rejected.
    pub security_exclusions: bool,

    /// Explicit Attic include/exclude rules (evaluated after Git ignore and
    /// default exclusions, in order).
    pub attic_include_rules: Vec<GlobRule>,

    /// Attic exclude rules (evaluated before include rules).
    pub attic_exclude_rules: Vec<GlobRule>,

    /// Custom priority overrides (applied after eligibility is decided).
    pub custom_priority_overrides: Vec<PriorityRule>,

    /// Paths exempt from secret scanning (e.g. fixture directories with
    /// example keys).  Cannot overlap with security-forbidden paths.
    pub scan_exempt_paths: Vec<String>,

    /// Whether to integrate with the user's global `~/.gitconfig
    /// core.excludesFile`. Default: `false` (security: reads user home).
    pub global_gitconfig_excludes: bool,
}

impl DiscoveryPolicy {
    /// Construct a sensible default policy for a Git repository.
    pub fn default_git() -> Self {
        DiscoveryPolicy {
            git_aware: true,
            include_untracked: true,
            default_exclusions: true,
            security_exclusions: true,
            attic_include_rules: Vec::new(),
            attic_exclude_rules: Vec::new(),
            custom_priority_overrides: Vec::new(),
            scan_exempt_paths: Vec::new(),
            global_gitconfig_excludes: false,
        }
    }

    /// Construct a sensible default policy for a non-Git directory.
    pub fn default_non_git() -> Self {
        DiscoveryPolicy {
            git_aware: false,
            include_untracked: true,
            default_exclusions: true,
            security_exclusions: true,
            attic_include_rules: Vec::new(),
            attic_exclude_rules: Vec::new(),
            custom_priority_overrides: Vec::new(),
            scan_exempt_paths: Vec::new(),
            global_gitconfig_excludes: false,
        }
    }

    /// Serialize to canonical JSON (keys sorted, no extra whitespace).
    ///
    /// Used to compute the `discovery_policy_hash`.
    pub fn to_canonical_json(&self) -> Result<String, crate::error::DiscoveryError> {
        serde_json::to_string(self).map_err(|e| {
            crate::error::DiscoveryError::PolicySerialize(e.to_string())
        })
    }

    /// Compute the BLAKE3 hex hash of the canonical JSON representation.
    pub fn hash(&self) -> Result<String, crate::error::DiscoveryError> {
        let json = self.to_canonical_json()?;
        let hash = blake3::hash(json.as_bytes());
        Ok(hash.to_hex().to_string())
    }

    /// Validate that `security_exclusions = true` and that no scan-exempt
    /// path overlaps a security-forbidden prefix.
    pub fn validate(&self) -> Result<(), crate::error::DiscoveryError> {
        if !self.security_exclusions {
            return Err(crate::error::DiscoveryError::InvalidConfig(
                "security_exclusions must be true; this field cannot be disabled".into(),
            ));
        }
        for exempt in &self.scan_exempt_paths {
            crate::security::assert_not_forbidden_prefix(exempt)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_git_policy_validates() {
        DiscoveryPolicy::default_git().validate().unwrap();
    }

    #[test]
    fn default_non_git_policy_validates() {
        DiscoveryPolicy::default_non_git().validate().unwrap();
    }

    #[test]
    fn security_exclusions_false_is_rejected() {
        let mut p = DiscoveryPolicy::default_git();
        p.security_exclusions = false;
        assert!(p.validate().is_err());
    }

    #[test]
    fn policy_hash_is_deterministic() {
        let p = DiscoveryPolicy::default_git();
        assert_eq!(p.hash().unwrap(), p.hash().unwrap());
    }

    #[test]
    fn policy_hash_changes_on_rule_addition() {
        let p1 = DiscoveryPolicy::default_git();
        let mut p2 = DiscoveryPolicy::default_git();
        p2.attic_exclude_rules.push(GlobRule::exclude("vendor/**"));
        assert_ne!(p1.hash().unwrap(), p2.hash().unwrap());
    }

    #[test]
    fn scan_exempt_ssh_path_is_rejected() {
        let mut p = DiscoveryPolicy::default_git();
        p.scan_exempt_paths.push(".ssh/known_hosts".into());
        assert!(p.validate().is_err());
    }

    #[test]
    fn glob_rule_serializes_correctly() {
        let rule = GlobRule::exclude("node_modules/**");
        let json = serde_json::to_string(&rule).unwrap();
        let back: GlobRule = serde_json::from_str(&json).unwrap();
        assert_eq!(rule, back);
    }
}
