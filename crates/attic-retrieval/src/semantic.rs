//! Semantic candidate generation (Phase 5 §12–§15, ADR-014).
//!
//! The semantic layer enters the Phase 4 pipeline ONLY as another bounded
//! candidate producer feeding the existing fusion → ranking → validation
//! chain. Similarity is a ranking signal, never evidence authority: a high
//! cosine score cannot satisfy a contract requirement by itself, and every
//! semantic candidate passes the same validation gate as any other.
//!
//! Fallback is explicit and observable (§15): missing embeddings, an
//! unavailable provider, or a spent time budget degrade to the canonical
//! non-semantic path with a recorded reason — never a stall, never a guess.

use std::sync::Arc;

use attic_evidence::Evidence;
use rusqlite::Connection;

use crate::candidates::{
    Candidate, GeneratorEnv, authority_for, bound_snippet, source_type_for_path, unit_span,
};
use crate::error::RetrievalError;
use crate::mode::AnswerModePolicy;

/// Truncate `s` to at most `max_bytes` bytes, cutting on a char boundary.
pub(crate) fn truncate_to_byte_limit(s: &str, max_bytes: usize) -> String {
    let mut s = s.to_owned();
    if s.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    s
}

/// Everything semantic queries need. Cheap to clone/share.
#[derive(Clone)]
pub struct SemanticStack {
    pub store: Arc<attic_semantic::SemanticStore>,
    pub provider: Arc<dyn attic_semantic::SemanticProvider>,
}

impl SemanticStack {
    /// Open (or create) the disposable store beside a canonical index.
    pub fn open(
        semantic_db_path: &std::path::Path,
        provider: Arc<dyn attic_semantic::SemanticProvider>,
    ) -> Result<Self, String> {
        let store =
            attic_semantic::SemanticStore::open(semantic_db_path).map_err(|e| e.to_string())?;
        Ok(Self {
            store: Arc::new(store),
            provider,
        })
    }

    /// In-memory stack for tests.
    pub fn in_memory(provider: Arc<dyn attic_semantic::SemanticProvider>) -> Result<Self, String> {
        let store = attic_semantic::SemanticStore::open_in_memory().map_err(|e| e.to_string())?;
        Ok(Self {
            store: Arc::new(store),
            provider,
        })
    }

    pub fn provider_id(&self) -> &'static str {
        self.provider.id()
    }

    pub fn model_id(&self) -> &str {
        self.provider.model_id()
    }
}

/// Why the semantic generator contributed nothing. Recorded verbatim in the
/// plan trace (`semantic_fallback_reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticFallback {
    /// Policy forbids semantics (FAST, or caller-disabled).
    Disabled,
    /// Active model has zero embeddings for the scope.
    NoEmbeddings,
    /// Provider reported itself unavailable.
    ProviderUnavailable,
    /// Time/candidate budget exhausted before any result.
    TimeBudget,
    /// Contributed candidates (no fallback).
    Contributed,
    /// The disposable store itself failed (poisoned/IO) — canonical path
    /// continues untouched (disposable-layer invariant).
    StoreUnavailable,
}

impl SemanticFallback {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "SEMANTIC_DISABLED",
            Self::NoEmbeddings => "NO_EMBEDDINGS_FOR_MODEL",
            Self::ProviderUnavailable => "PROVIDER_UNAVAILABLE",
            Self::TimeBudget => "SEMANTIC_TIME_BUDGET",
            Self::StoreUnavailable => "SEMANTIC_STORE_UNAVAILABLE",
            Self::Contributed => "",
        }
    }
}

/// Outcome of one semantic generator invocation (§21 observability).
#[derive(Debug, Clone, Copy)]
pub struct SemanticOutcome {
    pub candidates: usize,
    pub fallback: SemanticFallback,
}

/// Minimum cosine for a kNN hit to become a candidate. Below this the
/// embedding space is not confident enough to justify competing with exact
/// signals — an explicit, inspectable noise floor (ADR-014).
pub const SEMANTIC_MIN_SIMILARITY: f32 = 0.34;

/// Bounded kNN candidate generator over the disposable layer.
pub struct SemanticCandidateGenerator;

impl SemanticCandidateGenerator {
    /// Run the generator. Never blocks on enrichment; never touches the
    /// live filesystem; all writes go to the DISPOSABLE store only.
    pub fn run(
        env: &mut GeneratorEnv<'_>,
        stack: &SemanticStack,
        policy: &AnswerModePolicy,
        query_text: &str,
    ) -> Result<(Vec<Candidate>, SemanticOutcome), RetrievalError> {
        let t0 = std::time::Instant::now();
        // ── Policy gate ────────────────────────────────────────────────────
        if !policy.semantic_allowed || policy.max_semantic_candidates == 0 {
            return Ok((
                Vec::new(),
                SemanticOutcome {
                    candidates: 0,
                    fallback: SemanticFallback::Disabled,
                },
            ));
        }
        // ── Availability probe (cheap, deterministic) ──────────────────────
        if !stack.provider.available() {
            return Ok((
                Vec::new(),
                SemanticOutcome {
                    candidates: 0,
                    fallback: SemanticFallback::ProviderUnavailable,
                },
            ));
        }
        // ── Coverage probe: embeddings for THIS scope under ACTIVE model ───
        let coverage = match stack.store.count(
            stack.provider.id(),
            stack.provider.model_id(),
            env.repository_id.as_deref(),
        ) {
            Ok(n) => n,
            // Store-level failure degrades to canonical retrieval — never a
            // hard error (disposable-layer invariant).
            Err(e) => {
                tracing::warn!("semantic store unavailable (coverage probe): {e}");
                return Ok((
                    Vec::new(),
                    SemanticOutcome {
                        candidates: 0,
                        fallback: SemanticFallback::StoreUnavailable,
                    },
                ));
            }
        };
        if coverage == 0 {
            return Ok((
                Vec::new(),
                SemanticOutcome {
                    candidates: 0,
                    fallback: SemanticFallback::NoEmbeddings,
                },
            ));
        }

        // ── Embed the query (single item; input bounded like enrichment) ───
        let q = truncate_to_byte_limit(query_text, stack.provider.max_input_bytes());
        // ── Query embedding under the mode's time budget (§14/§20) ─────────
        let deadline = t0 + std::time::Duration::from_millis(policy.semantic_time_budget_ms);
        if std::time::Instant::now() >= deadline || !env.budget.candidates_available() {
            return Ok((
                Vec::new(),
                SemanticOutcome {
                    candidates: 0,
                    fallback: SemanticFallback::TimeBudget,
                },
            ));
        }
        let mut usage = attic_semantic::ResourceUsage::default();
        let cancel = attic_semantic::CancelFlag::new();
        let outs = stack
            .provider
            .embed_batch(
                &[attic_semantic::EmbeddingInput {
                    unit_key: "__query__".into(),
                    text: q,
                }],
                &cancel,
                &mut usage,
                Some(deadline),
            )
            .map_err(|e| {
                RetrievalError::Storage(attic_storage::StorageError::Worker(e.to_string()))
            })?;
        let Some(qv) = outs.into_iter().next().map(|o| o.vector) else {
            return Ok((
                Vec::new(),
                SemanticOutcome {
                    candidates: 0,
                    fallback: SemanticFallback::ProviderUnavailable,
                },
            ));
        };

        // ── Bounded kNN: deadline/cancel enforced DURING the scan (§20) ────
        let k = (policy.max_semantic_candidates as usize).min(env.limit);
        let scan_budget = attic_semantic::ScanBudget {
            cancel: &cancel,
            deadline: Some(deadline),
            max_rows: (k.max(1) as u64) * 8, // bounded work even on huge models
        };
        let kn = match stack.store.knn(
            &qv,
            k,
            stack.provider.id(),
            stack.provider.model_id(),
            env.repository_id.as_deref(),
            &scan_budget,
        ) {
            Ok(kn) => kn,
            // Store-level failure degrades to canonical retrieval — it must
            // NEVER fail the answer (disposable-layer invariant).
            Err(e) => {
                tracing::warn!("semantic store unavailable during query: {e}");
                return Ok((
                    Vec::new(),
                    SemanticOutcome {
                        candidates: 0,
                        fallback: SemanticFallback::StoreUnavailable,
                    },
                ));
            }
        };
        let hits = kn.hits;
        if hits.is_empty() {
            return Ok((
                Vec::new(),
                SemanticOutcome {
                    candidates: 0,
                    fallback: if kn.truncated_by_budget {
                        SemanticFallback::TimeBudget
                    } else {
                        SemanticFallback::NoEmbeddings
                    },
                },
            ));
        }

        // Batch-fetch unit text once for snippets (bounded read).
        let ids: Vec<String> = hits.iter().map(|h| h.retrieval_unit_id.clone()).collect();
        let rows = attic_storage::semantic_units_by_ids(env.conn, &ids)?;
        let text_of: std::collections::HashMap<&str, &str> = rows
            .iter()
            .map(|r| (r.unit_id.as_str(), r.retrieval_text.as_str()))
            .collect();

        let mut out = Vec::with_capacity(hits.len());
        let mut demanded: Vec<String> = Vec::new();
        for h in hits {
            if h.similarity < SEMANTIC_MIN_SIMILARITY {
                continue; // below the noise floor: never compete with exact signals
            }
            if !env.budget.admit_candidate() || std::time::Instant::now() >= deadline {
                break;
            }
            let Some(anchor) =
                attic_storage::retrieval_unit_anchor(env.conn, &h.retrieval_unit_id)?
            else {
                continue; // invalidated between kNN and anchor read
            };
            let sim = (h.similarity as f64).clamp(0.0, 1.0);
            // Similarity alone must never look authoritative: confidence is
            // capped BELOW exact-match levels and validation still runs.
            let confidence = (sim * 0.9).clamp(0.05, 0.95);
            let st = source_type_for_path(&anchor.path);
            let mut ev = Evidence::new(
                uuid::Uuid::new_v4().to_string(),
                anchor.repository_id.clone(),
            );
            ev.source_type = st;
            ev.source_id = anchor.file_occurrence_id.clone();
            ev.path = anchor.path.clone();
            ev.freshness_state = attic_core::FreshnessState::from_db_str(&anchor.freshness_state)
                .unwrap_or(attic_core::FreshnessState::Unknown);
            ev.authority = authority_for(st);
            ev.confidence = confidence;
            ev.snippet = text_of
                .get(h.retrieval_unit_id.as_str())
                .map(|t| bound_snippet(t, 1200));
            ev.signals.semantic_score = Some(sim);
            ev.source_span = unit_span(
                anchor.start_line,
                anchor.end_line.map(|e| e.saturating_add(1)),
            );
            crate::candidates::fill_file_provenance(env.conn, &mut ev);
            out.push(Candidate::new(
                crate::candidates::RetrieverKind::Semantic,
                ev,
            ));
            demanded.push(anchor.path);
        }

        // Query-demand signal for the NEXT selection cycle (disposable DB).
        if !demanded.is_empty() {
            let _ = stack.store.bump_demand(&demanded);
        }

        let n = out.len();
        let fallback = if out.is_empty() {
            SemanticFallback::TimeBudget
        } else {
            SemanticFallback::Contributed
        };
        Ok((
            out,
            SemanticOutcome {
                candidates: n,
                fallback,
            },
        ))
    }
}

/// Convenience used by tests and the server bootstrap: reconcile + fully
/// drain the enrichment queue synchronously (bounded by config budget).
pub fn enrich_to_completion(
    conn: &Connection,
    stack: &SemanticStack,
    cfg: &attic_semantic::EnrichmentConfig,
) -> Result<attic_semantic::EnrichStats, String> {
    let sel_cfg = attic_semantic::SelectionConfig::default();
    let _report = attic_semantic::reconcile(conn, &stack.store, stack.provider.as_ref(), &sel_cfg)
        .map_err(|e| e.to_string())?;
    let cancel = attic_semantic::CancelFlag::new();
    // Test/bootstrap convenience — no explicit override provenance is
    // relevant here, so this always claims (if applicable) as a
    // Recommendation, matching the provider's own default identity.
    attic_semantic::drive(
        conn,
        &stack.store,
        stack.provider.as_ref(),
        cfg,
        &cancel,
        attic_semantic::EmbeddingIntentSource::Recommendation,
    )
    .map_err(|e| e.to_string())
}
