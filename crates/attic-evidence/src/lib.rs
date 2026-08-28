//! attic-evidence — canonical evidence objects for Attic retrieval (Phase 4).
//!
//! This crate is dependency-light: it defines the **canonical shapes** shared
//! by every retriever, the Evidence Manager, the Context Builder and the
//! Answer Verifier. It performs no I/O. Serialization is deterministic:
//! enums render as their contract strings (`SCREAMING_SNAKE_CASE`), floats as
//! f64, ids as lowercase hyphenated UUID strings.
//!
//! Contracts implemented here:
//! - `docs/ARCHITECTURE.md` (Evidence, SourceType, AuthorityLevel,
//!   FreshnessState, VerificationState, RetrievalSource, RankingSignals)
//! - claim/answer-verification shapes used by the Phase 4 Answer Verifier.

pub mod claim;
pub mod ranking;
pub mod signals;
pub mod types;

pub use claim::{Claim, ClaimType, ClaimVerdict, VerifiedClaim};
pub use ranking::RankingSignals;
pub use types::{
    AuthorityLevel, Contradiction, ContradictionKind, Evidence, EvidenceSourceType,
    RelationshipProvenance, ResolutionLevel, RetrievalSource, VerificationStatus,
};

/// Generates a string-backed enum with contract-string serialization.
///
/// Exported so downstream crates (`attic-retrieval`) can define their own
/// contract-string enums with identical semantics.
#[macro_export]
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $s:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $( $(#[$vmeta])* $variant, )+
        }

        impl $name {
            /// Canonical string form stored/logged everywhere.
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( Self::$variant => $s, )+
                }
            }

            /// Parse from the canonical string form.
            pub fn from_db_str(s: &str) -> Option<Self> {
                match s {
                    $( $s => Some(Self::$variant), )+
                    _ => None,
                }
            }

            /// Every variant, in declaration order (deterministic iteration).
            pub fn all() -> &'static [Self] {
                &[ $( Self::$variant, )+ ]
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = String::deserialize(d)?;
                Self::from_db_str(&raw).ok_or_else(|| {
                    serde::de::Error::custom(format!("unknown {} variant: {raw}", stringify!($name)))
                })
            }
        }
    };
}

/// Re-exported freshness vocabulary so callers need only this crate plus
/// `attic-core` types they already hold.
pub use attic_core::FreshnessState as Freshness;

/// Approximate token count for budget accounting: bytes / 4, rounded up.
///
/// Deliberately simple and deterministic; the exact tokenizer is irrelevant
/// to V1 budgets as long as the approximation is stable.
pub fn approx_tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attic_core::FreshnessState;

    #[test]
    fn approx_tokens_rounds_up() {
        assert_eq!(approx_tokens(0), 0);
        assert_eq!(approx_tokens(1), 1);
        assert_eq!(approx_tokens(8), 2);
        assert_eq!(approx_tokens(9), 3);
    }

    #[test]
    fn freshness_state_values_match_contract() {
        assert_eq!(FreshnessState::Current.as_str(), "CURRENT");
        assert_eq!(FreshnessState::Stale.as_str(), "STALE");
        assert_eq!(FreshnessState::Unknown.as_str(), "UNKNOWN");
        assert_eq!(FreshnessState::Invalid.as_str(), "INVALID");
        assert_eq!(FreshnessState::PendingRefresh.as_str(), "PENDING_REFRESH");
    }
}
