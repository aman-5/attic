//! Bounded debounce + coalescing state machine.
//!
//! Pure and deterministic: time is injected (`now_ms`), so tests drive the
//! exact event sequences from the Phase 2 contract without sleeping.
//!
//! Sequences handled per contract §4:
//! - `create → modify → modify`   → one Upsert hint
//! - `modify → delete`            → one Remove hint
//! - `create → delete` pre-flush  → nothing (path vanishes)
//! - `delete → recreate`          → one Upsert hint
//! - `rename old → new`           → one Rename pair (when both ends observed)
//! - duplicate storms             → collapsed per path
//!
//! Bound: at most [`EventCoalescer::capacity`] distinct paths pending.  A
//! burst beyond that sets `overflowed` instead of growing memory; the caller
//! must respond by marking affected state UNKNOWN and scheduling
//! reconciliation (queue overflow never silently claims CURRENT).

use std::collections::BTreeMap;

use crate::events::{FsEventKind, NormalizedEvent};

/// One coalesced, still-unverified change leaving the debouncer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoalescedChange {
    /// Path likely added or modified.
    Upsert(String),
    /// Path likely removed.
    Remove(String),
    /// Observed rename pair (hint only).
    Rename(String, String),
}

#[derive(Debug, Clone)]
struct PendingPath {
    first_seen_ms: u64,
    last_seen_ms: u64,
    saw_create: bool,
    saw_remove: bool,
    /// True when the LAST observed op for this path was a removal.
    last_op_remove: bool,
    /// Set when this path is the destination of a RenamedTo without a match.
    renamed_to: bool,
}

/// Bounded debouncer/coalescer.
#[derive(Debug)]
pub struct EventCoalescer {
    quiet_period_ms: u64,
    capacity: usize,
    pending: BTreeMap<String, PendingPath>,
    /// Unmatched RenamedFrom sources awaiting their RenamedTo partner.
    renames_from: BTreeMap<String, u64>,
    /// Rename destination → matched origin (consumed at drain).
    renamed_origins: BTreeMap<String, String>,
    overflowed: bool,
    total_events_in: u64,
}

impl EventCoalescer {
    /// Create a coalescer with the given quiet period (debounce window) and
    /// maximum number of simultaneously-pending paths.
    pub fn new(quiet_period_ms: u64, capacity: usize) -> Self {
        Self {
            quiet_period_ms,
            capacity: capacity.max(1),
            pending: BTreeMap::new(),
            renames_from: BTreeMap::new(),
            renamed_origins: BTreeMap::new(),
            overflowed: false,
            total_events_in: 0,
        }
    }

    /// Whether events had to be shed because the pending set was full.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Total accepted raw events (observability).
    pub fn total_events(&self) -> u64 {
        self.total_events_in
    }

    /// Number of paths currently pending.
    pub fn pending_count(&self) -> usize {
        self.pending.len() + self.renames_from.len()
    }

    /// Feed one normalized event at virtual time `now_ms`.
    ///
    /// The caller MUST have applied security/eligibility filtering already
    /// ([`crate::events::EventFilter`]); this state machine is policy-agnostic.
    ///
    /// Returns `false` when the event had to be shed (overflow) — the caller
    /// MUST treat this as possible event loss.
    pub fn push(&mut self, ev: &NormalizedEvent, now_ms: u64) -> bool {
        self.total_events_in += 1;

        match ev.kind {
            FsEventKind::RenamedFrom => {
                if self.renames_from.len() >= self.capacity {
                    self.overflowed = true;
                    return false;
                }
                // Pairing window = quiet period; stale unpaired From entries
                // degrade to Remove during drain_due.
                self.renames_from.insert(ev.rel_path.clone(), now_ms);
                return true;
            }
            FsEventKind::RenamedTo => {
                // Find the oldest unpaired From within the pairing window.
                let mut best: Option<(&String, &u64)> = None;
                for (from, t) in self.renames_from.iter() {
                    if now_ms.saturating_sub(*t) <= self.quiet_period_ms * 4 {
                        match best {
                            Some((_, bt)) if *bt <= *t => {}
                            _ => best = Some((from, t)),
                        }
                    }
                }
                if let Some((from, _)) = best {
                    let from = (*from).clone();
                    self.renames_from.remove(&from);
                    self.pending.remove(&from); // prior hints for old path fold in
                    // Destination tracked as an upsert carrying rename origin.
                    let entry = self
                        .pending
                        .entry(ev.rel_path.clone())
                        .or_insert(PendingPath {
                            first_seen_ms: now_ms,
                            last_seen_ms: now_ms,
                            saw_create: false,
                            saw_remove: false,
                            last_op_remove: false,
                            renamed_to: true,
                        });
                    entry.last_seen_ms = now_ms;
                    entry.renamed_to = true;
                    // Remember origin on the destination via a side map keyed
                    // "dest" → "src": encoded through renames_from inverse.
                    self.renamed_origins.insert(ev.rel_path.clone(), from);
                    return true;
                }
                // Unpaired To → plain upsert hint.
                self.touch_pending(&ev.rel_path, now_ms);
                return true;
            }
            _ => {}
        }

        let entry_len = self.pending.len();
        let is_new = !self.pending.contains_key(&ev.rel_path);
        if is_new && entry_len >= self.capacity {
            self.overflowed = true;
            return false;
        }

        match ev.kind {
            FsEventKind::Created => {
                let e = self.touch_pending(&ev.rel_path, now_ms);
                e.saw_create = true;
                e.last_op_remove = false; // remove→create ⇒ recreate (upsert)
                e.last_seen_ms = now_ms;
            }
            FsEventKind::Removed => {
                let e = self.touch_pending(&ev.rel_path, now_ms);
                let was_create_only = e.saw_create && !e.saw_remove;
                e.saw_remove = true;
                e.last_op_remove = true;
                e.last_seen_ms = now_ms;
                if was_create_only && e.last_seen_ms - e.first_seen_ms <= self.quiet_period_ms {
                    // create → remove before debounce: path vanishes entirely.
                    self.pending.remove(&ev.rel_path);
                }
            }
            FsEventKind::Modified | FsEventKind::Other => {
                let e = self.touch_pending(&ev.rel_path, now_ms);
                e.last_seen_ms = now_ms;
            }
            FsEventKind::RenamedFrom | FsEventKind::RenamedTo => unreachable!("handled above"),
        }
        true
    }

    fn touch_pending(&mut self, path: &str, now_ms: u64) -> &mut PendingPath {
        self.pending.entry(path.to_owned()).or_insert(PendingPath {
            first_seen_ms: now_ms,
            last_seen_ms: now_ms,
            saw_create: false,
            saw_remove: false,
            last_op_remove: false,
            renamed_to: false,
        })
    }

    /// Drain every path whose quiet period has elapsed at `now_ms`.
    ///
    /// Deterministic order (BTreeMap iteration): repo-relative lexicographic.
    pub fn drain_due(&mut self, now_ms: u64) -> Vec<CoalescedChange> {
        let mut out = Vec::new();

        // Expire unpaired rename origins.
        let expired_from: Vec<String> = self
            .renames_from
            .iter()
            .filter(|(_, t)| now_ms.saturating_sub(**t) > self.quiet_period_ms * 4)
            .map(|(p, _)| p.clone())
            .collect();
        for from in expired_from {
            self.renames_from.remove(&from);
            out.push(CoalescedChange::Remove(from));
        }

        let due_paths: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, st)| now_ms.saturating_sub(st.last_seen_ms) >= self.quiet_period_ms)
            .map(|(p, _)| p.clone())
            .collect();

        for path in due_paths {
            let Some(st) = self.pending.remove(&path) else {
                continue;
            };
            if st.saw_create && st.saw_remove {
                if st.last_op_remove {
                    // create→delete inside the window: path vanished, no-op.
                    continue;
                }
                // remove→create: recreate — fall through to Upsert.
                out.push(CoalescedChange::Upsert(path));
                continue;
            }
            if st.saw_remove && !st.saw_create {
                out.push(CoalescedChange::Remove(path));
                continue;
            }
            if st.renamed_to
                && let Some(origin) = self.renamed_origins.remove(&path)
            {
                out.push(CoalescedChange::Rename(origin, path));
                continue;
            }
            out.push(CoalescedChange::Upsert(path));
        }
        out.sort_by(|a, b| a.key().cmp(b.key()));
        out
    }

    /// Force-drain everything regardless of quiet period (shutdown/flush).
    pub fn flush_all(&mut self) -> Vec<CoalescedChange> {
        let far_future = u64::MAX / 2;
        self.drain_due(far_future)
    }
}

impl CoalescedChange {
    fn key(&self) -> &str {
        match self {
            CoalescedChange::Upsert(p) | CoalescedChange::Remove(p) => p,
            CoalescedChange::Rename(f, _) => f,
        }
    }
}
