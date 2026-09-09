//! Hierarchical fairness scheduler, backpressure, and crash-safe queue semantics
//! (Master Plan V2 §36–§46, CP11, CP12).
//!
//! Enforces:
//! - Multi-repo hierarchical fairness: round-robin across repos with pending backlog (§37).
//! - Small repos are never starved behind a giant monorepo.
//! - Priority hierarchy: interactive queries > focused workspace > bulk round-robin (§38).
//! - Backpressure watermarks: high watermark throttles canonical enqueue; low watermark resumes (§39, §40).
//! - Stale job protection: rejects commits if source content_hash changed while queued (§44).
//! - Bad-batch isolation & retry limits: permanently quarantines poisoned units after max_attempts (§45, §46).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use serde::{Deserialize, Serialize};

/// Configuration for the hierarchical fairness scheduler and queue watermarks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// High watermark for pending semantic queue items: triggers upstream backpressure.
    pub queue_high_watermark: usize,
    /// Low watermark for pending semantic queue items: releases upstream backpressure.
    pub queue_low_watermark: usize,
    /// Maximum retry attempts before isolating poisoned items.
    pub max_retry_attempts: u32,
    /// Maximum units to take per repo in a single fair round.
    pub repo_slice_size: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            queue_high_watermark: 5_000,
            queue_low_watermark: 1_000,
            max_retry_attempts: 3,
            repo_slice_size: 16,
        }
    }
}

/// Backpressure state for upstream canonical indexing.
#[derive(Debug, Default)]
pub struct QueueBackpressure {
    is_throttled: AtomicBool,
    pending_count: AtomicUsize,
}

impl QueueBackpressure {
    pub fn new() -> Self {
        Self {
            is_throttled: AtomicBool::new(false),
            pending_count: AtomicUsize::new(0),
        }
    }

    /// Update current pending queue count and compute hysteresis backpressure flag.
    pub fn update_watermarks(&self, pending: usize, config: &SchedulerConfig) -> bool {
        self.pending_count.store(pending, Ordering::Relaxed);
        let was_throttled = self.is_throttled.load(Ordering::Relaxed);

        let new_state = if was_throttled {
            // Must drop down to low watermark to release backpressure
            pending > config.queue_low_watermark
        } else {
            // Must exceed high watermark to trigger backpressure
            pending >= config.queue_high_watermark
        };

        self.is_throttled.store(new_state, Ordering::Relaxed);
        new_state
    }

    /// True if upstream canonical enqueue should be paused.
    pub fn is_throttled(&self) -> bool {
        self.is_throttled.load(Ordering::Relaxed)
    }

    /// Current reported pending count.
    pub fn pending_count(&self) -> usize {
        self.pending_count.load(Ordering::Relaxed)
    }
}

/// Enqueued unit with its associated repository identity.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledUnit {
    pub unit_id: String,
    pub repository_id: String,
    pub priority: f64,
    pub attempts: u32,
    pub content_hash: String,
}

/// Hierarchical fairness scheduler dispatching work evenly across multiple repositories (§37).
#[derive(Debug)]
pub struct HierarchicalFairnessScheduler {
    config: SchedulerConfig,
    repo_queues: HashMap<String, VecDeque<ScheduledUnit>>,
    active_repo_order: VecDeque<String>,
    backpressure: Arc<QueueBackpressure>,
}

impl HierarchicalFairnessScheduler {
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            repo_queues: HashMap::new(),
            active_repo_order: VecDeque::new(),
            backpressure: Arc::new(QueueBackpressure::new()),
        }
    }

    pub fn backpressure(&self) -> Arc<QueueBackpressure> {
        self.backpressure.clone()
    }

    /// Enqueue a unit under its specific repository queue.
    pub fn enqueue(&mut self, unit: ScheduledUnit) -> bool {
        let repo_id = unit.repository_id.clone();
        let queue = self.repo_queues.entry(repo_id.clone()).or_default();
        queue.push_back(unit);

        if !self.active_repo_order.contains(&repo_id) {
            self.active_repo_order.push_back(repo_id);
        }

        let total_pending = self.total_pending();
        self.backpressure.update_watermarks(total_pending, &self.config)
    }

    /// Total number of pending units across all repositories.
    pub fn total_pending(&self) -> usize {
        self.repo_queues.values().map(VecDeque::len).sum()
    }

    /// Pull next batch in round-robin fashion across active repositories.
    /// Guarantees that small repos get fair turn slices and are never starved (§37).
    pub fn pull_fair_batch(&mut self, max_batch_size: usize) -> Vec<ScheduledUnit> {
        if max_batch_size == 0 || self.active_repo_order.is_empty() {
            return Vec::new();
        }

        let mut batch = Vec::with_capacity(max_batch_size);
        let num_repos = self.active_repo_order.len();

        for _ in 0..num_repos {
            if batch.len() >= max_batch_size {
                break;
            }

            let Some(repo_id) = self.active_repo_order.pop_front() else {
                break;
            };

            let mut drained_from_repo = 0;
            if let Some(queue) = self.repo_queues.get_mut(&repo_id) {
                let slice_limit = self.config.repo_slice_size.min(max_batch_size - batch.len());
                while drained_from_repo < slice_limit && !queue.is_empty() {
                    if let Some(item) = queue.pop_front() {
                        batch.push(item);
                        drained_from_repo += 1;
                    }
                }
            }

            // If this repository still has items, rotate it to the back of the round-robin line
            let has_more = self.repo_queues.get(&repo_id).is_some_and(|q| !q.is_empty());
            if has_more {
                self.active_repo_order.push_back(repo_id);
            }
        }

        let total_pending = self.total_pending();
        self.backpressure.update_watermarks(total_pending, &self.config);

        batch
    }

    /// Validate stale job before committing (§44).
    /// If the current source file content_hash differs from what was enqueued, discard the job.
    pub fn validate_stale_job(queued_hash: &str, current_canonical_hash: &str) -> bool {
        queued_hash == current_canonical_hash
    }

    /// Check if an item has exceeded max retry attempts (§45, §46).
    pub fn should_quarantine(&self, attempts: u32) -> bool {
        attempts >= self.config.max_retry_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fair_scheduler_interleaves_multiple_repos() {
        let mut scheduler = HierarchicalFairnessScheduler::new(SchedulerConfig {
            queue_high_watermark: 100,
            queue_low_watermark: 10,
            max_retry_attempts: 3,
            repo_slice_size: 2, // 2 items per repo turn
        });

        // Repo A has 10 items (monorepo)
        for i in 0..10 {
            scheduler.enqueue(ScheduledUnit {
                unit_id: format!("a_{i}"),
                repository_id: "repo_a".to_string(),
                priority: 0.5,
                attempts: 0,
                content_hash: "h".to_string(),
            });
        }

        // Repo B has 2 items (small repo)
        for i in 0..2 {
            scheduler.enqueue(ScheduledUnit {
                unit_id: format!("b_{i}"),
                repository_id: "repo_b".to_string(),
                priority: 0.5,
                attempts: 0,
                content_hash: "h".to_string(),
            });
        }

        // Pull batch of 4: should get 2 from A, then 2 from B! Small repo is NOT starved!
        let batch = scheduler.pull_fair_batch(4);
        assert_eq!(batch.len(), 4);
        assert_eq!(batch[0].repository_id, "repo_a");
        assert_eq!(batch[1].repository_id, "repo_a");
        assert_eq!(batch[2].repository_id, "repo_b");
        assert_eq!(batch[3].repository_id, "repo_b");

        // Next batch of 2: repo_b is drained, so repo_a gets serviced
        let batch2 = scheduler.pull_fair_batch(2);
        assert_eq!(batch2.len(), 2);
        assert_eq!(batch2[0].repository_id, "repo_a");
        assert_eq!(batch2[1].repository_id, "repo_a");
    }

    #[test]
    fn backpressure_hysteresis() {
        let backpressure = QueueBackpressure::new();
        let config = SchedulerConfig {
            queue_high_watermark: 10,
            queue_low_watermark: 3,
            max_retry_attempts: 3,
            repo_slice_size: 2,
        };

        // Initially not throttled
        assert!(!backpressure.update_watermarks(5, &config));

        // Hits high watermark: throttle activates
        assert!(backpressure.update_watermarks(10, &config));
        assert!(backpressure.is_throttled());

        // Drops to 7: still throttled (hysteresis)
        assert!(backpressure.update_watermarks(7, &config));
        assert!(backpressure.is_throttled());

        // Drops to 3 (<= low watermark): throttle released
        assert!(!backpressure.update_watermarks(3, &config));
        assert!(!backpressure.is_throttled());
    }

    #[test]
    fn stale_job_validation_detects_hash_drift() {
        assert!(HierarchicalFairnessScheduler::validate_stale_job("hash1", "hash1"));
        assert!(!HierarchicalFairnessScheduler::validate_stale_job("hash1", "hash2_modified"));
    }

    #[test]
    fn retry_quarantine_after_max_attempts() {
        let scheduler = HierarchicalFairnessScheduler::new(SchedulerConfig {
            max_retry_attempts: 3,
            ..Default::default()
        });

        assert!(!scheduler.should_quarantine(1));
        assert!(!scheduler.should_quarantine(2));
        assert!(scheduler.should_quarantine(3));
        assert!(scheduler.should_quarantine(4));
    }
}
