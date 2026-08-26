//! Per-query budget accounting (`docs/contracts/answer_modes.md` rules
//! AM-E1..E8). Every limit that is reached is recorded as an observable
//! event; exhaustion never silently looks like a complete answer.

use std::time::{Duration, Instant};

use crate::mode::AnswerModePolicy;

/// Tracks consumption of every mode budget during one query.
#[derive(Debug)]
pub struct BudgetAccountant {
    started: Instant,
    max_time: Duration,
    pub max_candidates: u32,
    pub max_fs_files: u32,
    pub max_fs_bytes: u64,
    pub max_graph_nodes: u32,

    pub candidates_used: u32,
    pub fs_files_used: u32,
    pub fs_bytes_used: u64,
    pub graph_nodes_used: u32,
    /// Field names of limits actually hit, deterministic insertion order.
    limits_hit: Vec<String>,
}

impl BudgetAccountant {
    pub fn new(policy: &AnswerModePolicy) -> Self {
        Self {
            started: Instant::now(),
            max_time: Duration::from_millis(policy.max_time_ms),
            max_candidates: policy.max_candidates,
            max_fs_files: policy.max_fs_files,
            max_fs_bytes: policy.max_fs_bytes,
            max_graph_nodes: policy.max_graph_nodes,
            candidates_used: 0,
            fs_files_used: 0,
            fs_bytes_used: 0,
            graph_nodes_used: 0,
            limits_hit: Vec::new(),
        }
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn remaining_ms(&self) -> u64 {
        self.max_time
            .saturating_sub(self.started.elapsed())
            .as_millis() as u64
    }

    pub fn time_exceeded(&self) -> bool {
        self.started.elapsed() >= self.max_time
    }

    fn note_limit(&mut self, field: &'static str) {
        if !self.limits_hit.iter().any(|f| f == field) {
            self.limits_hit.push(field.to_owned());
        }
    }

    /// Try to admit one more candidate into the ranking pool (AM-E2).
    /// Returns false when the candidate budget is exhausted.
    pub fn admit_candidate(&mut self) -> bool {
        if self.candidates_used >= self.max_candidates {
            self.note_limit("max_candidates");
            return false;
        }
        self.candidates_used += 1;
        true
    }

    /// Whether any candidate slots remain.
    pub fn candidates_available(&self) -> bool {
        self.candidates_used < self.max_candidates
    }

    /// Charge a filesystem read attempt (AM-E4). Returns false when the FS
    /// budget forbids or exhausted the read.
    pub fn charge_file_read(&mut self, bytes: u64) -> bool {
        if self.max_fs_files == 0 || self.max_fs_bytes == 0 {
            self.note_limit("max_fs_files");
            return false;
        }
        if self.fs_files_used >= self.max_fs_files {
            self.note_limit("max_fs_files");
            return false;
        }
        let new_bytes = self.fs_bytes_used.saturating_add(bytes);
        if new_bytes > self.max_fs_bytes {
            self.note_limit("max_fs_bytes");
            return false;
        }
        self.fs_files_used += 1;
        self.fs_bytes_used = new_bytes;
        true
    }

    /// Charge one visited node in a graph walk (AM-E3).
    pub fn charge_graph_node(&mut self) -> bool {
        if self.graph_nodes_used >= self.max_graph_nodes {
            self.note_limit("max_graph_nodes");
            return false;
        }
        self.graph_nodes_used += 1;
        true
    }

    /// Limits reached so far.
    pub fn limits_hit(&self) -> &[String] {
        &self.limits_hit
    }

    /// Final PolicyResult derived from observed consumption.
    pub fn derive_final_result(
        &self,
        insufficient: bool,
        hard_cancelled: bool,
    ) -> crate::mode::PolicyResult {
        use crate::mode::PolicyResult as PR;
        if hard_cancelled {
            return PR::HardCancelled;
        }
        if insufficient && !self.limits_hit.is_empty() {
            // Budget exhaustion contributed to insufficiency; surface which.
            return PR::DegradedByTime;
        }
        if insufficient {
            return PR::InsufficientEvidence;
        }
        match self.limits_hit.first().map(String::as_str) {
            None => PR::CompletedWithinBudget,
            Some("max_candidates") => PR::DegradedByCandidates,
            Some("max_fs_files") | Some("max_fs_bytes") => PR::DegradedByFsBudget,
            Some(_) => PR::DegradedByTime,
        }
    }
}
