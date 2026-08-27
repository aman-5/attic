//! Simple value types used across the domain model.

use serde::{Deserialize, Serialize};

/// A half-open byte/line span within a source file.
///
/// Lines and columns are 0-based; `end_line`/`end_col` are exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceSpan {
    /// Starting line (0-based, inclusive).
    pub start_line: u32,
    /// Starting column (0-based, inclusive).
    pub start_col: u32,
    /// Ending line (0-based, exclusive).
    pub end_line: u32,
    /// Ending column (0-based, exclusive).
    pub end_col: u32,
}

impl SourceSpan {
    /// Construct a new [`SourceSpan`].
    pub fn new(start_line: u32, start_col: u32, end_line: u32, end_col: u32) -> Self {
        Self {
            start_line,
            start_col,
            end_line,
            end_col,
        }
    }

    /// Return `true` if `other` is entirely contained within this span.
    pub fn contains(&self, other: &Self) -> bool {
        (self.start_line < other.start_line
            || (self.start_line == other.start_line && self.start_col <= other.start_col))
            && (self.end_line > other.end_line
                || (self.end_line == other.end_line && self.end_col >= other.end_col))
    }
}

impl std::fmt::Display for SourceSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}-{}:{}",
            self.start_line, self.start_col, self.end_line, self.end_col
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_span_display() {
        let s = SourceSpan::new(1, 0, 3, 10);
        assert_eq!(s.to_string(), "1:0-3:10");
    }

    #[test]
    fn source_span_serde_round_trip() {
        let original = SourceSpan::new(10, 4, 10, 20);
        let json = serde_json::to_string(&original).unwrap();
        let decoded: SourceSpan = serde_json::from_str(&json).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn contains_works() {
        let outer = SourceSpan::new(0, 0, 10, 0);
        let inner = SourceSpan::new(2, 5, 5, 5);
        assert!(outer.contains(&inner));
        assert!(!inner.contains(&outer));
    }
}

// Resource budgets for a single indexing/retrieval operation.
///
/// These are tracked per-task and checked against global limits at
/// runtime.  When budgets are exceeded, the operation is deferred or
/// degraded rather than risking uncontrolled resource consumption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBudgets {
    /// Maximum memory to use for this operation (bytes).  When the global
    /// memory-pressure monitor observes consumption approaching
    /// `TOTAL_MEMORY_BUDGET_MIB`, operations may be paused or scaled back.
    pub memory_budget_bytes: u64,
    /// Maximum CPU time (ms) for this operation before it should yield.
    pub cpu_budget_ms: u64,
    /// Maximum elapsed time (ms) before the operation is cancelled.
    pub timeout_ms: u64,
}
