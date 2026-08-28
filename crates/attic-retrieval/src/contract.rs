//! Query Evidence Contracts (`docs/ARCHITECTURE.md`).
//!
//! Every QueryType maps to an explicit contract stating which evidence is
//! REQUIRED before Attic may speak, what is merely preferred, how fresh it
//! must be, and which bounded fallbacks may run when requirements are not
//! met. Generic confidence never substitutes for these requirements.

use attic_evidence::str_enum;
// (str_enum is #[macro_export]; 2018-style path import works)
use serde::{Deserialize, Serialize};

use crate::query::QueryType;

str_enum! {
    /// Freshness floor for evidence counted toward a requirement.
    FreshnessRequirement {
        /// Only CURRENT freshness accepted.
        CurrentOnly => "CURRENT_ONLY",
        /// STALE accepted with an explicit caveat.
        CurrentOrStale => "CURRENT_OR_STALE",
        /// UNKNOWN also accepted (lower-confidence queries).
        Any => "ANY",
    }
}

str_enum! {
    /// Bounded fallback strategies allowed when requirements are unmet.
    FallbackStrategy {
        /// Expand FTS query terms.
        BroaderFts => "BROADER_FTS",
        /// Expand graph traversal by 1–2 hops.
        BoundedGraph => "BOUNDED_GRAPH",
        /// Read directly from authoritative source (Phase 1B access).
        SourceVerification => "SOURCE_VERIFICATION",
        /// Search knowledge docs if not already done.
        KnowledgeLookup => "KNOWLEDGE_LOOKUP",
        /// Semantic retrieval — Phase 5+; NEVER executed in Phase 4.
        SemanticSearch => "SEMANTIC_SEARCH",
    }
}

str_enum! {
    /// Query repository scope.
    RepositoryScope {
        /// Scoped to one repository.
        Single => "SINGLE",
        /// Spans all indexed repositories in the workspace.
        Workspace => "WORKSPACE",
        /// Caller supplied an explicit repository list.
        Specified => "SPECIFIED",
    }
}

/// One named evidence requirement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvidenceRequirement {
    /// Requirement label (e.g. `"definition"`, `"implementation"`).
    pub evidence_type: String,
    /// Acceptable canonical source types.
    pub source_types: Vec<attic_evidence::EvidenceSourceType>,
    /// Minimum number of satisfying evidence items.
    pub min_count: u32,
}

impl EvidenceRequirement {
    fn req(label: &str, types: &[attic_evidence::EvidenceSourceType], min: u32) -> Self {
        Self {
            evidence_type: label.to_owned(),
            source_types: types.to_vec(),
            min_count: min,
        }
    }
}

use attic_evidence::EvidenceSourceType as ST;

const CODE: &[ST] = &[ST::SourceCode];
const CODE_GEN: &[ST] = &[ST::SourceCode, ST::GeneratedSource];
const CONFIG: &[ST] = &[ST::Configuration];
const KNOW: &[ST] = &[ST::Knowledge];
const KNOW_DOC: &[ST] = &[ST::Knowledge, ST::Documentation];
const TEST: &[ST] = &[ST::Test];
const REL: &[ST] = &[ST::Relationship];
const REL_CODE: &[ST] = &[ST::Relationship, ST::SourceCode];
const DEP_DECL: &[ST] = &[ST::Configuration, ST::SourceCode, ST::Relationship];

/// Per-query specification of required/preferred evidence and expansion
/// bounds (`docs/ARCHITECTURE.md`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryEvidenceContract {
    /// The intent this contract belongs to.
    pub query_type: QueryType,
    /// MUST be satisfied before any answer is produced.
    pub required_evidence: Vec<EvidenceRequirement>,
    /// Improves confidence when present; absence lowers it, never blocks.
    pub preferred_evidence: Vec<EvidenceRequirement>,
    /// Freshness floor for required evidence.
    pub freshness_requirement: FreshnessRequirement,
    /// Minimum relationship confidence to count graph evidence toward a
    /// requirement.
    pub relationship_confidence_min: Option<f64>,
    /// Repository scope for candidate generation.
    pub repository_scope: RepositoryScope,
    /// Allowed bounded fallback strategies, in execution order.
    pub allowed_fallbacks: Vec<FallbackStrategy>,
    /// Expansion bounds.
    pub expansion_budget: ExpansionBudget,
}

/// Expansion budget (`docs/ARCHITECTURE.md` ExpansionBudget).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpansionBudget {
    /// Maximum targeted-expansion rounds.
    pub max_expansion_rounds: u32,
    /// Maximum extra candidates across all rounds.
    pub max_extra_candidates: u32,
    /// Maximum extra files read during source verification.
    pub max_extra_files: u32,
    /// Maximum extra bytes read during source verification.
    pub max_extra_bytes: u64,
}

impl Default for ExpansionBudget {
    fn default() -> Self {
        Self {
            max_expansion_rounds: 2,
            max_extra_candidates: 50,
            max_extra_files: 5,
            max_extra_bytes: 512 * 1024,
        }
    }
}

/// Return the approved V1 contract for a query type.
///
/// Unrecognized/`GENERIC_SEARCH` accepts any evidence (no requirement).
pub fn contract_for(query_type: QueryType) -> QueryEvidenceContract {
    use self::FallbackStrategy as F;
    match query_type {
        QueryType::DefinitionLookup => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("definition", CODE_GEN, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("symbol_occurrence", CODE, 1),
                EvidenceRequirement::req("implementation_span", CODE, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOnly,
            relationship_confidence_min: None,
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![F::BroaderFts, F::SourceVerification],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 2,
                max_extra_candidates: 20,
                ..ExpansionBudget::default()
            },
        },
        QueryType::ExactLookup => QueryEvidenceContract {
            query_type,
            // An exact path/identifier request is satisfied by ANY current
            // artifact AT that location — code, config, doc or test.
            required_evidence: vec![EvidenceRequirement::req(
                "file_content",
                &[
                    ST::SourceCode,
                    ST::Configuration,
                    ST::Documentation,
                    ST::Knowledge,
                    ST::Test,
                ],
                1,
            )],
            preferred_evidence: vec![EvidenceRequirement::req("structural_outline", CODE, 1)],
            freshness_requirement: FreshnessRequirement::CurrentOnly,
            relationship_confidence_min: None,
            repository_scope: RepositoryScope::Specified,
            allowed_fallbacks: vec![F::BroaderFts, F::SourceVerification],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 2,
                max_extra_candidates: 10,
                ..ExpansionBudget::default()
            },
        },
        QueryType::SymbolNavigation => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("symbol_occurrence", CODE, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("call_relationship", REL, 1),
                EvidenceRequirement::req("import", CODE, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOrStale,
            relationship_confidence_min: Some(0.6),
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![F::BoundedGraph, F::BroaderFts],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 2,
                max_extra_candidates: 30,
                ..ExpansionBudget::default()
            },
        },
        QueryType::ConfigurationLookup => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("config_value", CONFIG, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("config_schema", &[ST::SourceCode, ST::Documentation], 1),
                EvidenceRequirement::req("knowledge_note", KNOW, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOnly,
            relationship_confidence_min: None,
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![F::BroaderFts, F::KnowledgeLookup, F::SourceVerification],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 2,
                max_extra_candidates: 20,
                ..ExpansionBudget::default()
            },
        },
        QueryType::ArchitectureExplanation => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("implementation", CODE, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("knowledge_architecture", KNOW, 1),
                EvidenceRequirement::req("test_expectation", TEST, 1),
                EvidenceRequirement::req("configuration", CONFIG, 1),
                EvidenceRequirement::req("caller_chain", REL, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOrStale,
            relationship_confidence_min: Some(0.5),
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![
                F::BoundedGraph,
                F::KnowledgeLookup,
                F::BroaderFts,
                F::SourceVerification,
            ],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 3,
                max_extra_candidates: 50,
                max_extra_files: 5,
                max_extra_bytes: 512 * 1024,
            },
        },
        QueryType::DebuggingRootCause => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("implementation", CODE, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("test_expectation", TEST, 1),
                EvidenceRequirement::req("error_handling_span", CODE, 1),
                EvidenceRequirement::req("configuration", CONFIG, 1),
                EvidenceRequirement::req("knowledge_note", KNOW, 1),
                EvidenceRequirement::req("dependency_chain", REL, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOnly,
            relationship_confidence_min: Some(0.5),
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![
                F::SourceVerification,
                F::BoundedGraph,
                F::KnowledgeLookup,
                F::BroaderFts,
            ],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 3,
                max_extra_candidates: 50,
                max_extra_files: 10,
                max_extra_bytes: 1024 * 1024,
            },
        },
        QueryType::ImpactAnalysis => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("definition", CODE_GEN, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("callers", REL_CODE, 1),
                EvidenceRequirement::req("dependents", REL, 1),
                EvidenceRequirement::req("test_coverage", TEST, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOrStale,
            relationship_confidence_min: Some(0.5),
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![F::BoundedGraph, F::BroaderFts],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 2,
                max_extra_candidates: 40,
                ..ExpansionBudget::default()
            },
        },
        QueryType::DependencyQuestion | QueryType::CrossRepoQuestion => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req(
                "dependency_declaration",
                DEP_DECL,
                1,
            )],
            preferred_evidence: vec![
                EvidenceRequirement::req("transitive_dependency", REL, 1),
                EvidenceRequirement::req("build_config", CONFIG, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOrStale,
            relationship_confidence_min: Some(0.6),
            repository_scope: if query_type == QueryType::CrossRepoQuestion {
                RepositoryScope::Workspace
            } else {
                RepositoryScope::Specified
            },
            allowed_fallbacks: vec![F::BoundedGraph, F::BroaderFts],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 2,
                max_extra_candidates: 30,
                ..ExpansionBudget::default()
            },
        },
        QueryType::KnowledgeQuestion => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("knowledge_content", KNOW_DOC, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("related_source", CODE, 1),
                EvidenceRequirement::req("related_config", CONFIG, 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOrStale,
            relationship_confidence_min: None,
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![F::KnowledgeLookup, F::BroaderFts],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 1,
                max_extra_candidates: 20,
                ..ExpansionBudget::default()
            },
        },
        QueryType::TestBehavior => QueryEvidenceContract {
            query_type,
            required_evidence: vec![EvidenceRequirement::req("test_content", TEST, 1)],
            preferred_evidence: vec![
                EvidenceRequirement::req("subject_implementation", CODE, 1),
                EvidenceRequirement::req("test_fixture", &[ST::Test, ST::Configuration], 1),
            ],
            freshness_requirement: FreshnessRequirement::CurrentOrStale,
            relationship_confidence_min: None,
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![F::BroaderFts, F::SourceVerification],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 2,
                max_extra_candidates: 30,
                ..ExpansionBudget::default()
            },
        },
        QueryType::GenericSearch => QueryEvidenceContract {
            query_type,
            required_evidence: vec![],
            preferred_evidence: vec![EvidenceRequirement::req(
                "any",
                &[ST::SourceCode, ST::Test, ST::Configuration, ST::Knowledge],
                1,
            )],
            freshness_requirement: FreshnessRequirement::Any,
            relationship_confidence_min: None,
            repository_scope: RepositoryScope::Workspace,
            allowed_fallbacks: vec![F::BroaderFts, F::KnowledgeLookup],
            expansion_budget: ExpansionBudget {
                max_expansion_rounds: 1,
                max_extra_candidates: 50,
                ..ExpansionBudget::default()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_type_has_a_contract() {
        for qt in QueryType::all() {
            let c = contract_for(*qt);
            assert_eq!(c.query_type, *qt);
        }
    }

    #[test]
    fn definition_lookup_contract_matches_doc() {
        let c = contract_for(QueryType::DefinitionLookup);
        assert_eq!(c.required_evidence.len(), 1);
        assert_eq!(c.required_evidence[0].min_count, 1);
        assert_eq!(c.freshness_requirement, FreshnessRequirement::CurrentOnly);
        assert!(
            c.allowed_fallbacks
                .contains(&FallbackStrategy::SourceVerification)
        );
    }

    #[test]
    fn generic_search_has_no_required_evidence() {
        let c = contract_for(QueryType::GenericSearch);
        assert!(c.required_evidence.is_empty());
        assert_eq!(c.freshness_requirement, FreshnessRequirement::Any);
    }

    #[test]
    fn debugging_allows_larger_verification_budget() {
        let c = contract_for(QueryType::DebuggingRootCause);
        assert_eq!(c.expansion_budget.max_extra_files, 10);
        assert_eq!(c.expansion_budget.max_extra_bytes, 1024 * 1024);
    }
}
