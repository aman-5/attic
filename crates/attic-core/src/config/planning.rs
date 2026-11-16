//! Configuration only: parsing planning settings never creates workflow state.

use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

use super::ConfigError;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
/// Who advances the workflow between steps; approval always requires the user.
pub enum PlanningMode {
    #[default]
    /// Ask the user before advancing each step.
    Manual,
    /// Advance agent-owned steps automatically, stopping for human approval.
    Autopilot,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
/// Git comparison baseline used for commit budgets.
pub enum FastPathScope {
    #[default]
    /// Compare the index with HEAD.
    Staged,
    /// Compare the index with the configured branch merge base.
    Branch,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Spec-first workflow settings, independent of workspace membership.
pub struct PlanningConfig {
    /// Master switch; false must not create workflow state.
    pub enabled: bool,
    /// Host workflow guidance mode.
    pub mode: PlanningMode,
    /// Absolute enabled roots; empty selects all active roots.
    pub roots: Vec<String>,
    /// Repository-relative, slash-separated design document directory.
    pub designs_dir: String,
    /// Small-change budgets and commit enforcement.
    pub fast_path: FastPathConfig,
    /// Implementation lease duration and renewal limits.
    pub lease: LeaseConfig,
    /// Review context delivery limits.
    pub packs: PackConfig,
    /// Citation verification policy.
    pub validation: ValidationConfig,
    /// Installed hook error policy.
    pub hooks: HooksConfig,
}

impl Default for PlanningConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: PlanningMode::Manual,
            roots: Vec::new(),
            designs_dir: "docs/designs".into(),
            fast_path: FastPathConfig::default(),
            lease: LeaseConfig::default(),
            packs: PackConfig::default(),
            validation: ValidationConfig::default(),
            hooks: HooksConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Budgets for changes not covered by an approved implementation.
pub struct FastPathConfig {
    /// False reports violations without blocking.
    pub enforce: bool,
    /// Maximum uncovered file operations.
    pub max_files: usize,
    /// Maximum uncovered added plus removed lines at commit.
    pub max_lines: usize,
    /// Permit uncovered binary changes within the file budget.
    pub allow_binary: bool,
    /// Staged or branch-wide comparison.
    pub scope: FastPathScope,
    /// Merge-base reference required by branch-wide checking.
    pub base_branch: Option<String>,
    /// Commit coverage grace period after successful drift, in hours.
    pub commit_grace_hours: u32,
}

impl Default for FastPathConfig {
    fn default() -> Self {
        Self {
            enforce: false,
            max_files: 2,
            max_lines: 100,
            allow_binary: false,
            scope: FastPathScope::Staged,
            base_branch: None,
            commit_grace_hours: 24,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Bounded implementation lease settings.
pub struct LeaseConfig {
    /// Duration of one lease or renewal in seconds, 60 through 86400.
    pub lease_seconds: u64,
    /// Renewals permitted before a new fenced resume.
    pub max_renewals: u32,
}

impl Default for LeaseConfig {
    fn default() -> Self {
        Self {
            lease_seconds: 900,
            max_renewals: 8,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Review pack admission and output limits.
pub struct PackConfig {
    /// Simultaneous builds daemon-wide, one through four.
    pub max_concurrent: usize,
    /// Maximum UTF-8 output size, including headers and truncation markers.
    pub byte_budget: usize,
    /// Advisory maximum reviewer rounds surfaced to the host.
    pub max_review_rounds: u32,
}

impl Default for PackConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 2,
            byte_budget: 65_536,
            max_review_rounds: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Citation verification and run-retention policy.
pub struct ValidationConfig {
    /// Failed attempts permitted for unchanged document bytes.
    pub max_verify_attempts: u32,
    /// Unsupported symbol analysis is a warning instead of an error.
    pub allow_file_only_fallback: bool,
    /// Missing declared cross-repository edges are errors instead of warnings.
    pub require_cross_repo_check: bool,
    /// Bounded disk/index freshness wait, in milliseconds.
    pub freshness_wait_ms: u64,
    /// Number of recent unreferenced runs retained, with approvals preserved.
    pub max_runs_retained: u32,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            max_verify_attempts: 3,
            allow_file_only_fallback: true,
            require_cross_repo_check: true,
            freshness_wait_ms: 5_000,
            max_runs_retained: 20,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
/// Hook failure policy, not an operating-system enforcement boundary.
pub struct HooksConfig {
    /// Block rather than allow if the checker is unavailable or fails.
    pub fail_closed: bool,
}

impl PlanningConfig {
    /// Reject unsafe paths, unbounded waits, and contradictory budgets.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |message: &str| ConfigError::Invalid(format!("[planning] {message}"));
        if !(1..=4).contains(&self.packs.max_concurrent) {
            return Err(invalid("packs.max_concurrent must be in 1..=4"));
        }
        if !(60..=86_400).contains(&self.lease.lease_seconds) {
            return Err(invalid("lease.lease_seconds must be in 60..=86400"));
        }
        if self.fast_path.max_files == 0 || self.fast_path.max_lines == 0 {
            return Err(invalid(
                "fast_path.max_files and max_lines must be positive",
            ));
        }
        if self.fast_path.scope == FastPathScope::Branch
            && self
                .fast_path
                .base_branch
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
        {
            return Err(invalid("branch scope requires fast_path.base_branch"));
        }
        let dir = &self.designs_dir;
        if dir.is_empty()
            || dir.contains(['\\', ':', '\0'])
            || dir
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || Path::new(dir)
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(invalid(
                "designs_dir must be a nonempty relative /-separated path without . or ..",
            ));
        }
        if self.roots.iter().any(|root| !Path::new(root).is_absolute()) {
            return Err(invalid("roots must contain absolute paths"));
        }
        if self.validation.max_verify_attempts == 0 || self.validation.max_runs_retained == 0 {
            return Err(invalid(
                "validation attempt and run limits must be positive",
            ));
        }
        if !(1..=1_048_576).contains(&self.packs.byte_budget) || self.packs.max_review_rounds == 0 {
            return Err(invalid(
                "packs.byte_budget must be in 1..=1048576 and max_review_rounds positive",
            ));
        }
        if self.validation.freshness_wait_ms > 60_000 {
            return Err(invalid(
                "validation.freshness_wait_ms must not exceed 60000",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ATTIC_TOML_TEMPLATE, AtticConfig};

    #[test]
    fn defaults_and_commented_template_enable_planning() {
        for text in ["", "[planning]", ATTIC_TOML_TEMPLATE] {
            let config = AtticConfig::parse_str(text).unwrap();
            assert_eq!(config.planning, PlanningConfig::default());
            assert!(config.planning.enabled);
            assert_eq!(config.planning.mode, PlanningMode::Manual);
            assert!(!config.planning.fast_path.enforce);
            assert!(config.planning.roots.is_empty());
        }
        let block = ATTIC_TOML_TEMPLATE.split("# [planning]").nth(1).unwrap();
        let active = format!("[planning]{}", block.replace("\n# ", "\n"));
        assert_eq!(
            AtticConfig::parse_str(&active).unwrap().planning,
            PlanningConfig::default()
        );
        let disabled = AtticConfig::parse_str("[planning]\nenabled = false").unwrap();
        assert!(!disabled.planning.enabled);
    }

    #[test]
    fn unknown_fields_are_rejected_at_every_level() {
        for table in [
            "planning",
            "planning.fast_path",
            "planning.lease",
            "planning.packs",
            "planning.validation",
            "planning.hooks",
        ] {
            assert!(
                AtticConfig::parse_str(&format!("[{table}]\nmisspelled = true")).is_err(),
                "{table}"
            );
        }
    }

    #[test]
    fn unsafe_and_unbounded_values_are_rejected() {
        for text in [
            "[planning.packs]\nmax_concurrent = 0",
            "[planning.packs]\nmax_concurrent = 5",
            "[planning.packs]\nbyte_budget = 0",
            "[planning.packs]\nbyte_budget = 1048577",
            "[planning.packs]\nmax_review_rounds = 0",
            "[planning.lease]\nlease_seconds = 59",
            "[planning.lease]\nlease_seconds = 86401",
            "[planning.fast_path]\nmax_files = 0",
            "[planning.fast_path]\nmax_lines = 0",
            "[planning.fast_path]\nscope = 'branch'",
            "[planning.fast_path]\nscope = 'branch'\nbase_branch = ' '",
            "[planning]\ndesigns_dir = '../designs'",
            "[planning]\ndesigns_dir = '/designs'",
            "[planning]\ndesigns_dir = 'docs/./designs'",
            "[planning]\ndesigns_dir = 'C:/designs'",
            "[planning]\ndesigns_dir = ''",
            "[planning]\nroots = ['relative']",
            "[planning.validation]\nmax_verify_attempts = 0",
            "[planning.validation]\nmax_runs_retained = 0",
            "[planning.validation]\nfreshness_wait_ms = 60001",
        ] {
            assert!(AtticConfig::parse_str(text).is_err(), "{text}");
        }
    }

    #[test]
    fn valid_opt_in_and_limit_endpoints() {
        let config = AtticConfig::parse_str("[planning]\nenabled = true\nmode = 'autopilot'\n[planning.fast_path]\nscope = 'branch'\nbase_branch = 'main'\n[planning.lease]\nlease_seconds = 60\n[planning.packs]\nmax_concurrent = 4").unwrap();
        assert!(config.planning.enabled);
        assert_eq!(config.planning.mode, PlanningMode::Autopilot);
        assert!(AtticConfig::parse_str("[planning.lease]\nlease_seconds = 86400").is_ok());
    }
}
