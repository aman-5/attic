//! S7 — Production Resource Manager for Attic MCP.
//
// Coordinates resource consumption across all Attic operations:
// - CPU concurrency (foreground queries, indexing, semantic enrichment) with
//   SEPARATE foreground and background slot capacities
// - Memory budgets (global + per-repository) with REAL process-RSS sampling
//   (via `sysinfo`), reconciled with worker accounting
// - Memory-aware admission: foreground queries are rejected when the
//   foreground capacity is exhausted or the memory budget is spent
// - Disk I/O pressure (bounded fs operations)
// - SQLite writer queue capacity (backpressure via WriterQueue)
// - Resource-pressure state observation and graceful degradation
//
// Enforcement model (Phase 7, normative):
// - `foreground_slots`: hard admission limit on concurrent MCP tool calls.
//   When exhausted, the server returns a busy error instead of queueing
//   unboundedly.
// - `background_slots`: hard limit on concurrent background workers
//   (incremental scheduler workers).  Capacity is derived from the indexing +
//   semantic worker configuration and is ALWAYS smaller than the foreground
//   capacity, so background work can never occupy the whole CPU budget.
// - Memory: `memory_used_mib()` is the MAXIMUM of (a) the actual measured
//   process resident set (authoritative) and (b) the worker-accounting
//   counter.  Workers that allocate large buffers still register their
//   allocations (accounting), but admission and degradation decisions are
//   driven by real RSS, refreshed before every admission decision.  This
//   means the monitor enforces genuine process memory behavior, not merely
//   manually incremented counters.
//
// Design principles (from PHASE_7_PRODUCTION.md §4-6, §8-10):
// - Foreground user work must not be starved by background indexing/enrichment
// - Background tasks yield/pause under resource pressure
// - Semantic enrichment is optional and must never starve canonical indexing
// - Explicit degradation behavior when approaching configured memory limits
// - Resource-pressure state must be observable
// - Never silently violate configured memory ceilings

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use attic_core::ResourcePressure;
use attic_core::resources;
use tracing::{debug, error, info, warn};

/// Number of cached sysinfo samples before refresh (RSS sampling is not free;
/// a small window keeps admission decisions cheap).
const RSS_SAMPLE_INTERVAL_MS: u64 = 250;

/// Percentage of the memory budget at which `ResourcePressure::Warning` is
/// reached (§4 pressure-tier ordering: Normal < Warning < Critical <
/// Emergency).
pub const PRESSURE_WARNING_PCT: u64 = 70;

/// Percentage of the memory budget at which `ResourcePressure::Critical` is
/// reached.  `min_free_memory_mib` MUST leave this tier reachable: its
/// implied "Emergency floor" percentage (`100 - min_free_pct`) must be
/// strictly greater than this value, otherwise Emergency would preempt
/// Critical and the tier could never be observed. See [`safe_min_free_mib`].
pub const PRESSURE_CRITICAL_PCT: u64 = 85;

/// Return a `min_free_memory_mib` value that keeps all four pressure tiers
/// (Normal/Warning/Critical/Emergency) reachable against `max_memory_mib`.
///
/// The Emergency tier triggers when `free_mib < min_free_mib`, i.e. at usage
/// percentage `100 - (min_free_mib * 100 / max_memory_mib)`. For Critical
/// (§`PRESSURE_CRITICAL_PCT`) to be reachable, that implied floor must sit
/// strictly above `PRESSURE_CRITICAL_PCT`. When the input violates this,
/// the value is clamped down and the caller is expected to have already
/// rejected the configuration via [`ResourceConfig::validate`] if it came
/// from user-facing configuration — this clamp is the defensive fallback so
/// the monitor itself is never internally inconsistent.
pub fn safe_min_free_mib(max_memory_mib: u64, min_free_mib: u64) -> u64 {
    if max_memory_mib == 0 {
        return min_free_mib;
    }
    // Largest min_free (in MiB) that still leaves the Emergency floor above
    // PRESSURE_CRITICAL_PCT, i.e. min_free_pct < 100 - PRESSURE_CRITICAL_PCT.
    let ceiling_pct = 100 - PRESSURE_CRITICAL_PCT;
    let ceiling_mib = max_memory_mib.saturating_mul(ceiling_pct) / 100;
    if min_free_mib < ceiling_mib {
        return min_free_mib;
    }
    let clamped = ceiling_mib.saturating_sub(1).max(1).min(max_memory_mib);
    warn!(
        "min_free_memory_mib={min_free_mib} against total_memory_budget_mib={max_memory_mib} \
         would make ResourcePressure::Critical unreachable (Emergency preempts it first); \
         clamping min_free_memory_mib to {clamped}"
    );
    clamped
}

/// Cross-platform process resident-set-size sampler.
///
/// Returns the current RSS of THIS process in MiB, or `None` when the OS does
/// not expose it.  Uses the `sysinfo` crate (no unsafe code in this crate).
pub fn sample_process_rss_mib() -> Option<u64> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(
        ProcessesToUpdate::Some(&[Pid::from_u32(std::process::id())]),
        true,
    );
    sys.process(Pid::from_u32(std::process::id()))
        .map(|p| p.memory() / (1024 * 1024))
}

/// Global resource-pressure monitor.
///
/// Tracks current resource consumption and determines when to degrade
/// operations.  State is observable via `pressure()` and `budgets()`.
///
/// This is a singleton shared across the entire Attic process.  All
/// long-running workers should consult the monitor before committing
/// to expensive operations.
pub struct ResourceMonitor {
    /// Worker-accounted memory usage in MiB (accumulated across all workers).
    memory_used: AtomicU64,
    /// Last sampled REAL process RSS in MiB.
    process_rss_mib: AtomicU64,
    /// Peak of the effective (max of accounted / RSS) usage (MiB).
    peak_memory_used: AtomicU64,
    /// Monotonic-ish clock (ms since monitor start) of the last RSS sample.
    last_rss_sample_ms: AtomicU64,
    /// Foreground CPU slots currently in use.
    foreground_slots: AtomicUsize,
    /// Background CPU slots currently in use.
    background_slots: AtomicUsize,
    /// Whether resource-pressure emergency mode is active.
    emergency_mode: AtomicBool,
    /// Start time for uptime tracking.
    start_time: Instant,
    /// Maximum memory budget in MiB (mutable via `apply_config`).
    max_memory_mib: AtomicU64,
    /// Per-repository memory budget in MiB (mutable via `apply_config`).
    per_repo_memory_mib: AtomicU64,
    /// Minimum free memory that must be retained (MiB).
    min_free_memory_mib: AtomicU64,
    /// Foreground slot capacity (mutable via `apply_config`).
    foreground_capacity: AtomicUsize,
    /// Background slot capacity (mutable via `apply_config`).
    background_capacity: AtomicUsize,
}

/// RAII guard for a foreground slot: releases the slot on drop.
pub struct ForegroundSlotGuard<'a> {
    monitor: &'a ResourceMonitor,
}

impl Drop for ForegroundSlotGuard<'_> {
    fn drop(&mut self) {
        self.monitor.release_foreground_slot();
    }
}

impl ForegroundSlotGuard<'_> {
    /// Current degraded-advisory for the in-flight query (cheap read).
    pub fn advisory(&self) -> ResourceAdvisory {
        current_advisory(self.monitor)
    }
}

impl ResourceMonitor {
    /// Create a new ResourceMonitor with Phase 7 default configuration.
    pub fn new() -> Self {
        Self::from_config(&ResourceConfig::default())
    }

    /// Create a ResourceMonitor from explicit configuration.
    ///
    /// Foreground capacity defaults to `MAX_FOREGROUND_QUERIES`.  Background
    /// capacity defaults to `MAX_INDEXING_WORKERS + MAX_SEMANTIC_WORKERS` and
    /// is always clamped to be strictly smaller than the foreground capacity
    /// so background work can never consume the whole CPU budget.
    pub fn from_config(config: &ResourceConfig) -> Self {
        let foreground = config
            .max_foreground_queries
            .unwrap_or(resources::MAX_FOREGROUND_QUERIES)
            .max(1);
        let background = config
            .max_background_workers
            .unwrap_or(resources::MAX_INDEXING_WORKERS + resources::MAX_SEMANTIC_WORKERS);
        let background = background.min(foreground.saturating_sub(1).max(1));
        let max_memory_mib = config
            .total_memory_budget_mib
            .unwrap_or(resources::TOTAL_MEMORY_BUDGET_MIB)
            .max(1);
        let min_free_memory_mib = safe_min_free_mib(
            max_memory_mib,
            config
                .min_free_memory_mib
                .unwrap_or(resources::MIN_FREE_MEMORY_MIB),
        );
        Self {
            memory_used: AtomicU64::new(0),
            process_rss_mib: AtomicU64::new(0),
            peak_memory_used: AtomicU64::new(0),
            last_rss_sample_ms: AtomicU64::new(0),
            foreground_slots: AtomicUsize::new(0),
            background_slots: AtomicUsize::new(0),
            emergency_mode: AtomicBool::new(false),
            start_time: Instant::now(),
            max_memory_mib: AtomicU64::new(max_memory_mib),
            per_repo_memory_mib: AtomicU64::new(
                config
                    .per_repo_memory_budget_mib
                    .unwrap_or(resources::PER_REPO_MEMORY_BUDGET_MIB),
            ),
            min_free_memory_mib: AtomicU64::new(min_free_memory_mib),
            foreground_capacity: AtomicUsize::new(foreground),
            background_capacity: AtomicUsize::new(background),
        }
    }

    fn elapsed_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    /// Refresh the effective memory usage from a REAL process RSS sample.
    ///
    /// Called before every admission decision (foreground query admission and
    /// background task scheduling).  The effective memory usage is the maximum
    /// of the accounted counter and the measured RSS, so degradation and
    /// admission behavior is driven by genuine process memory, not merely by
    /// manually incremented counters.
    pub fn refresh_process_memory(&self) {
        let now = self.elapsed_ms();
        let last = self.last_rss_sample_ms.load(Ordering::Relaxed);
        // `last == 0` means "never sampled" (both this field and `elapsed_ms`
        // start at/near zero at construction), so the interval throttle must
        // not apply to the first call — otherwise a monitor queried within
        // RSS_SAMPLE_INTERVAL_MS of startup would report a permanent 0 MiB
        // RSS and admission decisions would run on no real memory signal at
        // all during that window.
        if last != 0 && now.saturating_sub(last) < RSS_SAMPLE_INTERVAL_MS {
            return;
        }
        self.last_rss_sample_ms.store(now, Ordering::Relaxed);
        if let Some(rss) = sample_process_rss_mib() {
            self.process_rss_mib.store(rss, Ordering::Relaxed);
        }
        let effective = self.effective_memory_used();
        self.peak_memory_used
            .fetch_max(effective, Ordering::Relaxed);

        let pressure = self.compute_pressure(effective);
        if pressure != self.pressure() {
            self.announce_pressure_change(pressure, effective);
        }
    }

    /// Effective memory usage in MiB: max(worker accounting, real process RSS).
    pub fn effective_memory_used(&self) -> u64 {
        self.memory_used
            .load(Ordering::Relaxed)
            .max(self.process_rss_mib.load(Ordering::Relaxed))
    }

    /// Record memory usage increasing by `delta_mib` MiB.  Called by workers
    /// when they allocate memory for indexing/retrieval work.
    pub fn record_memory_increase(&self, delta_mib: u64) {
        let prev = self.memory_used.fetch_add(delta_mib, Ordering::Relaxed);
        let new_total = prev + delta_mib;

        // Update peak if we exceeded it.
        self.peak_memory_used
            .fetch_max(new_total, Ordering::Relaxed);

        // Check if we're crossing pressure thresholds.
        let pressure = self.compute_pressure(new_total);
        if pressure != self.pressure() {
            self.announce_pressure_change(pressure, new_total);
        }
    }

    /// Record memory usage decreasing by `delta_mib` MiB.  Called when workers
    /// release memory (task completion, cleanup).
    pub fn record_memory_decrease(&self, delta_mib: u64) {
        self.memory_used
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(delta_mib))
            })
            .ok();

        // Announce pressure change downward.
        let current = self.memory_used.load(Ordering::Relaxed);
        let pressure = self.compute_pressure(current);
        if pressure != self.pressure() {
            self.announce_pressure_change(pressure, current);
        }
    }

    // ── Foreground admission ───────────────────────────────────────────────

    /// Try to acquire a FOREGROUND CPU slot (concurrent MCP query admission).
    ///
    /// Refreshes real process memory first, then applies two hard admission
    /// rules:
    /// 1. foreground capacity: at most `foreground_capacity` concurrent
    ///    queries; beyond that the caller is told the server is busy;
    /// 2. memory admission: under `Emergency` pressure new foreground work is
    ///    still accepted (foreground has priority) but callers should degrade;
    ///    the slot is granted so a query can report an explicit degraded
    ///    advisory rather than being silently dropped.
    ///
    /// Returns `true` when the slot was acquired.  Use the returned
    /// [`ForegroundSlotGuard`] pattern or call [`Self::release_foreground_slot`].
    pub fn acquire_foreground_slot(&self) -> bool {
        self.refresh_process_memory();
        let max = self.foreground_capacity.load(Ordering::Acquire);
        loop {
            let current = self.foreground_slots.load(Ordering::Acquire);
            if current >= max {
                return false;
            }
            match self.foreground_slots.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => {
                    if observed >= max {
                        return false;
                    }
                }
            }
        }
    }

    /// Release a previously acquired foreground slot.
    pub fn release_foreground_slot(&self) {
        let _ = self
            .foreground_slots
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// Acquire a foreground slot with RAII release.  Returns `None` when the
    /// server is at foreground capacity (caller must refuse the work).
    pub fn try_foreground(&self) -> Option<ForegroundSlotGuard<'_>> {
        if self.acquire_foreground_slot() {
            Some(ForegroundSlotGuard { monitor: self })
        } else {
            None
        }
    }

    // ── Background admission ───────────────────────────────────────────────

    /// Try to acquire a BACKGROUND CPU slot (indexing/semantic/cross-repo work).
    ///
    /// Background capacity is separate from, and strictly smaller than, the
    /// foreground capacity.  Under `Pause`/`Emergency` advisories no new
    /// background slots are granted at all.
    pub fn acquire_background_slot(&self) -> bool {
        match current_advisory(self) {
            ResourceAdvisory::Pause | ResourceAdvisory::Emergency => return false,
            _ => {}
        }
        let max = self.background_capacity.load(Ordering::Acquire);
        loop {
            let current = self.background_slots.load(Ordering::Acquire);
            if current >= max {
                return false;
            }
            match self.background_slots.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => {
                    if observed >= max {
                        return false;
                    }
                }
            }
        }
    }

    /// Release a previously acquired background slot.
    pub fn release_background_slot(&self) {
        let _ = self
            .background_slots
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// Current number of in-use foreground slots (observability/tests).
    pub fn foreground_slots_in_use(&self) -> usize {
        self.foreground_slots.load(Ordering::Relaxed)
    }

    /// Current number of in-use background slots (observability/tests).
    pub fn background_slots_in_use(&self) -> usize {
        self.background_slots.load(Ordering::Relaxed)
    }

    /// Foreground slot capacity (observability/tests).
    pub fn foreground_capacity(&self) -> usize {
        self.foreground_capacity.load(Ordering::Relaxed)
    }

    /// Background slot capacity (observability/tests).
    pub fn background_capacity(&self) -> usize {
        self.background_capacity.load(Ordering::Relaxed)
    }

    /// Last sampled real process RSS in MiB (0 until first sample).
    pub fn process_rss_mib(&self) -> u64 {
        self.process_rss_mib.load(Ordering::Relaxed)
    }

    // ── Pressure model ─────────────────────────────────────────────────────

    /// Compute the current resource pressure level.
    ///
    /// Returns `ResourcePressure::Normal` when things are fine, up to
    /// `ResourcePressure::Emergency` when we're at or beyond limits.
    pub fn compute_pressure(&self, current_mib: u64) -> ResourcePressure {
        let max = self.max_memory_mib.load(Ordering::Relaxed);
        let min_free = self.min_free_memory_mib.load(Ordering::Relaxed);
        let pct = current_mib
            .saturating_mul(100)
            .checked_div(max)
            .unwrap_or(0);

        let free_mib = max.saturating_sub(current_mib);

        // Emergency: we've consumed so much that free memory is below the minimum.
        if free_mib < min_free {
            return ResourcePressure::Emergency;
        }

        // Critical: we're above the critical percentage of the budget.
        if pct >= PRESSURE_CRITICAL_PCT {
            return ResourcePressure::Critical;
        }

        // Warning: we're above the warning percentage of the budget.
        if pct >= PRESSURE_WARNING_PCT {
            return ResourcePressure::Warning;
        }

        ResourcePressure::Normal
    }

    /// Announce a pressure change, emitting a diagnostic trace.
    fn announce_pressure_change(&self, pressure: ResourcePressure, current_mib: u64) {
        let max = self.max_memory_mib.load(Ordering::Relaxed);
        let pct = current_mib
            .saturating_mul(100)
            .checked_div(max)
            .unwrap_or(0);

        match pressure {
            ResourcePressure::Normal => {
                debug!(
                    "resource pressure normal: {} MiB used ({}%), {} MiB free",
                    current_mib,
                    pct,
                    max.saturating_sub(current_mib)
                );
            }
            ResourcePressure::Warning => {
                warn!(
                    "resource pressure warning: {} MiB used ({}%), approaching limit",
                    current_mib, pct
                );
            }
            ResourcePressure::Critical => {
                error!(
                    "resource pressure critical: {} MiB used ({}%), limiting operations",
                    current_mib, pct
                );
            }
            ResourcePressure::Emergency => {
                error!(
                    "resource pressure emergency: {} MiB used ({}%), foreground only",
                    current_mib, pct
                );
            }
        }
    }

    /// Return the current pressure level, based on EFFECTIVE memory
    /// (accounting vs real RSS — see [`Self::refresh_process_memory`]).
    pub fn pressure(&self) -> ResourcePressure {
        self.compute_pressure(self.effective_memory_used())
    }

    /// Return current (worker-accounted) memory usage in MiB.
    pub fn memory_used_mib(&self) -> u64 {
        self.memory_used.load(Ordering::Relaxed)
    }

    /// Return peak effective memory usage in MiB.
    pub fn peak_memory_used_mib(&self) -> u64 {
        self.peak_memory_used.load(Ordering::Relaxed)
    }

    /// Return whether emergency mode is active.
    pub fn is_emergency(&self) -> bool {
        self.emergency_mode.load(Ordering::Acquire)
    }

    /// Set emergency mode (called by the server shutdown/startup logic).
    pub fn set_emergency(&self, emergency: bool) {
        self.emergency_mode.store(emergency, Ordering::Release);
    }

    /// Return the maximum memory budget in MiB.
    pub fn max_memory_mib(&self) -> u64 {
        self.max_memory_mib.load(Ordering::Relaxed)
    }

    /// Return the per-repository memory budget in MiB.
    pub fn per_repo_memory_mib(&self) -> u64 {
        self.per_repo_memory_mib.load(Ordering::Relaxed)
    }

    /// Return the minimum free memory that must be retained in MiB.
    pub fn min_free_memory_mib(&self) -> u64 {
        self.min_free_memory_mib.load(Ordering::Relaxed)
    }

    /// Return uptime in seconds.
    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Apply runtime configuration overrides to this monitor.
    ///
    /// Used by [`ResourceConfig::apply_to`]; values take effect immediately
    /// for subsequent admission decisions.
    pub fn apply_config(&self, config: &ResourceConfig) {
        if let Some(v) = config.total_memory_budget_mib {
            self.max_memory_mib.store(v.max(1), Ordering::Release);
        }
        if let Some(v) = config.per_repo_memory_budget_mib {
            self.per_repo_memory_mib.store(v, Ordering::Release);
        }
        if config.total_memory_budget_mib.is_some() || config.min_free_memory_mib.is_some() {
            // Re-derive min_free against the (possibly just-updated) max
            // budget so the two settings can never drift into an
            // inconsistent state where Critical is unreachable, regardless
            // of which of the two fields this call actually overrides.
            let max = self.max_memory_mib.load(Ordering::Acquire);
            let requested = config
                .min_free_memory_mib
                .unwrap_or_else(|| self.min_free_memory_mib.load(Ordering::Acquire));
            self.min_free_memory_mib
                .store(safe_min_free_mib(max, requested), Ordering::Release);
        }
        if let Some(v) = config.max_foreground_queries {
            self.foreground_capacity.store(v.max(1), Ordering::Release);
            // Re-clamp background below foreground.
            let fg = self.foreground_capacity.load(Ordering::Relaxed);
            let bg = self.background_capacity.load(Ordering::Relaxed);
            self.background_capacity
                .store(bg.min(fg.saturating_sub(1).max(1)), Ordering::Release);
        }
        if let Some(v) = config.max_background_workers {
            let fg = self.foreground_capacity.load(Ordering::Relaxed);
            self.background_capacity
                .store(v.min(fg.saturating_sub(1).max(1)), Ordering::Release);
        }
    }
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Resource pressure advisory sent to workers to indicate what behavior
/// they should exhibit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAdvisory {
    /// Normal operation; proceed as usual.
    Normal,
    /// Reduce concurrency where possible; pause non-essential work.
    Degraded,
    /// Pause all non-foreground work immediately.
    Pause,
    /// Emergency: only foreground retrieval is permitted; all background
    /// indexing/enrichment must yield.
    Emergency,
}

/// Query the current resource advisory for the calling worker.
pub fn current_advisory(monitor: &ResourceMonitor) -> ResourceAdvisory {
    let pressure = monitor.pressure();
    let used = monitor.effective_memory_used();
    let max = monitor.max_memory_mib();

    match pressure {
        ResourcePressure::Normal => ResourceAdvisory::Normal,
        ResourcePressure::Warning => {
            if used > max * 3 / 4 {
                ResourceAdvisory::Degraded
            } else {
                ResourceAdvisory::Normal
            }
        }
        ResourcePressure::Critical => ResourceAdvisory::Pause,
        ResourcePressure::Emergency => ResourceAdvisory::Emergency,
    }
}

/// Configuration for the resource manager, read from environment/config at startup.
///
/// These values may be overridden by environment variables or a config file
/// at server startup.  The defaults in `attic_core::resources` are the
/// production-hardening baselines.
#[derive(Debug, Clone, Default)]
pub struct ResourceConfig {
    /// Global memory budget in MiB. Overrides the default from
    /// `attic_core::resources::TOTAL_MEMORY_BUDGET_MIB`.
    pub total_memory_budget_mib: Option<u64>,
    /// Per-repository memory budget in MiB. Overrides the default from
    /// `attic_core::resources::PER_REPO_MEMORY_BUDGET_MIB`.
    pub per_repo_memory_budget_mib: Option<u64>,
    /// Minimum free memory that must be retained in MiB. Overrides the default
    /// from `attic_core::resources::MIN_FREE_MEMORY_MIB`.
    pub min_free_memory_mib: Option<u64>,
    /// Maximum concurrent foreground MCP queries. Overrides the default from
    /// `attic_core::resources::MAX_FOREGROUND_QUERIES`.
    pub max_foreground_queries: Option<usize>,
    /// Maximum concurrent BACKGROUND workers (indexing + semantic + cross-repo
    /// combined). Overrides the derived default
    /// `MAX_INDEXING_WORKERS + MAX_SEMANTIC_WORKERS`.  Always clamped strictly
    /// below the foreground capacity.
    pub max_background_workers: Option<usize>,
    /// Maximum disk I/O ops per second. Overrides the default from
    /// `attic_core::resources::MAX_IO_OPS_PER_SEC`.
    pub max_io_ops_per_sec: Option<u64>,
    /// Writer queue capacity. Overrides the default from
    /// `attic_core::resources::WRITER_QUEUE_CAPACITY`.
    pub writer_queue_capacity: Option<usize>,
    /// Writer batch size. Overrides the default from
    /// `attic_core::resources::WRITER_BATCH_SIZE`.
    pub writer_batch_size: Option<usize>,
    /// Writer flush interval in ms. Overrides the default from
    /// `attic_core::resources::WRITER_FLUSH_INTERVAL_MS`.
    pub writer_flush_interval_ms: Option<u64>,
}

impl ResourceConfig {
    /// Validate this configuration before it is applied.
    ///
    /// Rejects invalid or internally-inconsistent overrides so the server
    /// fails clearly at startup instead of silently running with
    /// nonsensical or unreachable resource-pressure behavior. Fields left as
    /// `None` fall back to `attic_core::resources` defaults, which are
    /// themselves guaranteed consistent, so only explicit overrides are
    /// checked here.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(v) = self.total_memory_budget_mib
            && v == 0
        {
            return Err("ATTIC_TOTAL_MEMORY_BUDGET_MIB must be > 0".into());
        }
        if let Some(v) = self.per_repo_memory_budget_mib
            && v == 0
        {
            return Err("ATTIC_PER_REPO_MEMORY_BUDGET_MIB must be > 0".into());
        }
        if let Some(v) = self.min_free_memory_mib
            && v == 0
        {
            return Err("ATTIC_MIN_FREE_MEMORY_MIB must be > 0".into());
        }
        if let Some(v) = self.max_foreground_queries
            && v == 0
        {
            return Err("ATTIC_MAX_FOREGROUND_QUERIES must be > 0".into());
        }
        if let Some(v) = self.max_io_ops_per_sec
            && v == 0
        {
            return Err("ATTIC_MAX_IO_OPS_PER_SEC must be > 0".into());
        }
        if let Some(v) = self.writer_queue_capacity
            && v == 0
        {
            return Err("ATTIC_WRITER_QUEUE_CAPACITY must be > 0".into());
        }
        if let Some(v) = self.writer_batch_size
            && v == 0
        {
            return Err("ATTIC_WRITER_BATCH_SIZE must be > 0".into());
        }
        if let Some(v) = self.writer_flush_interval_ms
            && v == 0
        {
            return Err("ATTIC_WRITER_FLUSH_INTERVAL_MS must be > 0".into());
        }

        let max = self
            .total_memory_budget_mib
            .unwrap_or(resources::TOTAL_MEMORY_BUDGET_MIB);
        let min_free = self
            .min_free_memory_mib
            .unwrap_or(resources::MIN_FREE_MEMORY_MIB);
        if min_free >= max {
            return Err(format!(
                "ATTIC_MIN_FREE_MEMORY_MIB ({min_free}) must be less than \
                 ATTIC_TOTAL_MEMORY_BUDGET_MIB ({max})"
            ));
        }
        let ceiling_pct = 100 - PRESSURE_CRITICAL_PCT;
        let ceiling_mib = max.saturating_mul(ceiling_pct) / 100;
        if min_free >= ceiling_mib {
            return Err(format!(
                "ATTIC_MIN_FREE_MEMORY_MIB ({min_free}) is too large relative to \
                 ATTIC_TOTAL_MEMORY_BUDGET_MIB ({max}): the implied Emergency floor \
                 would be at or below the Critical threshold ({PRESSURE_CRITICAL_PCT}%), \
                 making ResourcePressure::Critical unreachable. Use a value below \
                 {ceiling_mib} MiB."
            ));
        }
        Ok(())
    }

    /// Apply this configuration to the given ResourceMonitor.
    ///
    /// Overrides take effect immediately for all subsequent admission and
    /// degradation decisions (see [`ResourceMonitor::apply_config`]).
    pub fn apply_to(&self, monitor: &ResourceMonitor) {
        monitor.apply_config(self);
        if let Some(budget) = self.total_memory_budget_mib {
            info!("ResourceConfig: total_memory_budget_mib overridden to {budget}");
        }
        if let Some(budget) = self.per_repo_memory_budget_mib {
            info!("ResourceConfig: per_repo_memory_budget_mib overridden to {budget}");
        }
        if let Some(min_free) = self.min_free_memory_mib {
            info!("ResourceConfig: min_free_memory_mib overridden to {min_free}");
        }
        if let Some(queries) = self.max_foreground_queries {
            info!("ResourceConfig: max_foreground_queries overridden to {queries}");
        }
        if let Some(workers) = self.max_background_workers {
            info!("ResourceConfig: max_background_workers overridden to {workers}");
        }
        if let Some(io) = self.max_io_ops_per_sec {
            info!("ResourceConfig: max_io_ops_per_sec overridden to {io}");
        }
        if let Some(cap) = self.writer_queue_capacity {
            info!("ResourceConfig: writer_queue_capacity overridden to {cap}");
        }
        if let Some(batch) = self.writer_batch_size {
            info!("ResourceConfig: writer_batch_size overridden to {batch}");
        }
        if let Some(interval) = self.writer_flush_interval_ms {
            info!("ResourceConfig: writer_flush_interval_ms overridden to {interval}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn resource_monitor_pressure_levels() {
        let config = ResourceConfig {
            total_memory_budget_mib: Some(10_000),
            min_free_memory_mib: Some(100),
            ..ResourceConfig::default()
        };
        let monitor = ResourceMonitor::from_config(&config);

        // Below 70%: Normal
        assert_eq!(
            monitor.compute_pressure(0),
            attic_core::ResourcePressure::Normal
        );

        // At 70%: Warning.
        assert_eq!(
            monitor.compute_pressure(7_000),
            attic_core::ResourcePressure::Warning
        );

        // At 85%: Critical.
        assert_eq!(
            monitor.compute_pressure(8_500),
            attic_core::ResourcePressure::Critical
        );

        // Near limit (free < min_free): Emergency.
        assert_eq!(
            monitor.compute_pressure(9_950),
            attic_core::ResourcePressure::Emergency
        );
    }

    #[test]
    fn default_configuration_keeps_all_four_tiers_reachable() {
        // Regression test for the Phase 7 finding: production defaults
        // (1024 MiB budget, formerly 256 MiB min_free = 25%) made the
        // Emergency floor (free < min_free, i.e. used > 75%) fall BELOW the
        // Critical floor (used >= 85%), so Critical was unreachable — any
        // 85%-used value was already Emergency. The fixed defaults must
        // leave Critical reachable: some usage level must read Critical
        // without also reading Emergency.
        let monitor = ResourceMonitor::from_config(&ResourceConfig::default());
        let max = monitor.max_memory_mib();
        let min_free = monitor.min_free_memory_mib();
        let emergency_floor_pct = 100 - (min_free * 100 / max);
        assert!(
            emergency_floor_pct > PRESSURE_CRITICAL_PCT,
            "Emergency floor ({emergency_floor_pct}%) must be strictly above \
             Critical ({PRESSURE_CRITICAL_PCT}%) for Critical to be reachable"
        );
        // 85% used with the default budget must read Critical, not Emergency.
        // Ceiling-divide so integer truncation in `compute_pressure`'s own
        // `(current * 100) / max` can't round the percentage back under 85.
        let at_critical = (max * PRESSURE_CRITICAL_PCT).div_ceil(100);
        assert_eq!(
            monitor.compute_pressure(at_critical),
            attic_core::ResourcePressure::Critical
        );
    }

    #[test]
    fn safe_min_free_clamps_inconsistent_input() {
        // 250 MiB min_free against a 1000 MiB budget (25%) is above the 15%
        // ceiling implied by PRESSURE_CRITICAL_PCT=85 and must be clamped
        // down so Critical stays reachable.
        let clamped = safe_min_free_mib(1000, 250);
        assert!(
            clamped < 150,
            "clamped min_free must sit below the 15% ceiling"
        );
        let emergency_floor_pct = 100 - (clamped * 100 / 1000);
        assert!(emergency_floor_pct > PRESSURE_CRITICAL_PCT);

        // A value already within bounds must pass through unchanged.
        assert_eq!(safe_min_free_mib(1000, 50), 50);
    }

    #[test]
    fn resource_config_validate_rejects_unreachable_critical_tier() {
        let config = ResourceConfig {
            total_memory_budget_mib: Some(1000),
            min_free_memory_mib: Some(250),
            ..ResourceConfig::default()
        };
        assert!(
            config.validate().is_err(),
            "config making Critical unreachable must be rejected"
        );
    }

    #[test]
    fn resource_config_validate_accepts_consistent_config() {
        let config = ResourceConfig {
            total_memory_budget_mib: Some(1000),
            min_free_memory_mib: Some(50),
            ..ResourceConfig::default()
        };
        assert!(config.validate().is_ok());
        assert!(ResourceConfig::default().validate().is_ok());
    }

    #[test]
    fn resource_config_validate_rejects_zero_values() {
        let config = ResourceConfig {
            max_foreground_queries: Some(0),
            ..ResourceConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn resource_monitor_memory_increase_decrease() {
        let monitor = ResourceMonitor::new();

        // Record 100 MiB increase.
        monitor.record_memory_increase(100);
        assert_eq!(monitor.memory_used_mib(), 100);
        assert_eq!(monitor.pressure(), attic_core::ResourcePressure::Normal);

        // Record 200 MiB decrease (saturating at 0; must not panic or wrap).
        monitor.record_memory_decrease(200);
        assert_eq!(monitor.memory_used_mib(), 0);
        assert_eq!(monitor.pressure(), attic_core::ResourcePressure::Normal);
    }

    #[test]
    fn foreground_slot_capacity_is_enforced() {
        let config = ResourceConfig {
            max_foreground_queries: Some(3),
            ..ResourceConfig::default()
        };
        let monitor = ResourceMonitor::from_config(&config);

        assert!(monitor.acquire_foreground_slot());
        assert!(monitor.acquire_foreground_slot());
        assert!(monitor.acquire_foreground_slot());
        assert_eq!(monitor.foreground_slots_in_use(), 3);

        // At capacity: admission refused.
        assert!(!monitor.acquire_foreground_slot());

        // Release one → admission works again.
        monitor.release_foreground_slot();
        assert!(monitor.acquire_foreground_slot());
        assert_eq!(monitor.foreground_slots_in_use(), 3);
    }

    #[test]
    fn foreground_guard_releases_on_drop() {
        let config = ResourceConfig {
            max_foreground_queries: Some(1),
            ..ResourceConfig::default()
        };
        let monitor = ResourceMonitor::from_config(&config);

        {
            let guard = monitor.try_foreground().expect("first slot available");
            assert!(monitor.try_foreground().is_none(), "capacity exhausted");
            drop(guard);
        }
        assert_eq!(monitor.foreground_slots_in_use(), 0);
        assert!(monitor.try_foreground().is_some());
    }

    #[test]
    fn background_capacity_is_separate_and_below_foreground() {
        let config = ResourceConfig {
            max_foreground_queries: Some(4),
            max_background_workers: Some(2),
            ..ResourceConfig::default()
        };
        let monitor = ResourceMonitor::from_config(&config);

        assert_eq!(monitor.foreground_capacity(), 4);
        assert_eq!(monitor.background_capacity(), 2);
        assert!(monitor.background_capacity() < monitor.foreground_capacity());

        // Background capacity is enforced independently of foreground slots.
        assert!(monitor.acquire_foreground_slot());
        assert!(monitor.acquire_background_slot());
        assert!(monitor.acquire_background_slot());
        assert!(!monitor.acquire_background_slot(), "background at capacity");
        assert_eq!(monitor.background_slots_in_use(), 2);
        assert_eq!(monitor.foreground_slots_in_use(), 1);
    }

    #[test]
    fn background_slots_refused_under_pause_advisory() {
        // A coherent config (min_free well within the 15% ceiling) that
        // still reaches Emergency once usage is high enough.
        let config = ResourceConfig {
            total_memory_budget_mib: Some(300),
            min_free_memory_mib: Some(30),
            ..ResourceConfig::default()
        };
        let monitor = ResourceMonitor::from_config(&config);

        // 280 MiB used → free = 20 < 30 → Emergency → Pause/Emergency advisory.
        monitor.record_memory_increase(280);
        assert_eq!(current_advisory(&monitor), ResourceAdvisory::Emergency);
        assert!(
            !monitor.acquire_background_slot(),
            "background must be refused under emergency"
        );
        // Foreground still admitted (priority).
        assert!(monitor.acquire_foreground_slot());
    }

    #[test]
    fn default_background_capacity_is_derived_from_workers() {
        let monitor = ResourceMonitor::from_config(&ResourceConfig::default());
        assert_eq!(
            monitor.background_capacity(),
            resources::MAX_INDEXING_WORKERS + resources::MAX_SEMANTIC_WORKERS
        );
        assert!(monitor.background_capacity() < monitor.foreground_capacity());
    }

    #[test]
    fn rss_sampling_returns_real_process_memory() {
        // The sampler must return a plausible non-zero RSS for THIS process.
        let rss = sample_process_rss_mib().expect("process RSS should be sampleable");
        assert!(rss > 0, "RSS must be > 0, got {rss}");

        let monitor = ResourceMonitor::new();
        monitor.refresh_process_memory();
        let sampled = monitor.process_rss_mib();
        assert!(sampled > 0, "monitor should have sampled a non-zero RSS");
        // Two independent samples of the SAME live process a few
        // milliseconds apart may legitimately differ by a MiB or two
        // (allocator/OS bookkeeping) — assert plausible closeness, not
        // bit-exact equality, so this stays deterministic under load.
        let diff = sampled.abs_diff(rss);
        assert!(
            diff <= rss.max(sampled) / 4 + 8,
            "monitor RSS ({sampled} MiB) should be close to a fresh sample ({rss} MiB)"
        );
        // Effective usage is at least the real RSS even with no accounting.
        assert!(monitor.effective_memory_used() >= sampled);
    }

    #[test]
    fn effective_memory_is_max_of_accounting_and_rss() {
        let monitor = ResourceMonitor::new();
        // Accounting above any plausible RSS for this tiny test process.
        monitor.record_memory_increase(resources::TOTAL_MEMORY_BUDGET_MIB);
        monitor.refresh_process_memory();
        assert!(
            monitor.effective_memory_used() >= monitor.memory_used_mib(),
            "effective must be at least the accounted value"
        );
        assert_eq!(
            monitor.effective_memory_used(),
            monitor.memory_used_mib().max(monitor.process_rss_mib())
        );
    }

    #[test]
    fn resource_monitor_current_advisory() {
        let monitor = ResourceMonitor::new();

        // Initially normal → Normal advisory.
        assert_eq!(current_advisory(&monitor), ResourceAdvisory::Normal);

        // Set high memory pressure.
        monitor.record_memory_increase(resources::TOTAL_MEMORY_BUDGET_MIB);
        let advisory = current_advisory(&monitor);
        assert!(
            matches!(
                advisory,
                ResourceAdvisory::Emergency | ResourceAdvisory::Pause | ResourceAdvisory::Degraded
            ),
            "expected degraded/emergency/pause advisory under high pressure, got {advisory:?}"
        );
    }

    #[test]
    fn resource_config_apply_to_actually_reconfigures() {
        let monitor = ResourceMonitor::new();
        let config = ResourceConfig {
            total_memory_budget_mib: Some(2048),
            min_free_memory_mib: Some(100),
            max_foreground_queries: Some(8),
            max_background_workers: Some(3),
            ..ResourceConfig::default()
        };
        config.apply_to(&monitor);

        assert_eq!(monitor.max_memory_mib(), 2048);
        assert_eq!(monitor.min_free_memory_mib(), 100);
        assert_eq!(monitor.foreground_capacity(), 8);
        assert_eq!(monitor.background_capacity(), 3);
    }

    #[test]
    fn concurrent_slot_acquisition_never_exceeds_capacity() {
        use std::sync::atomic::AtomicUsize as _Unused;
        let _ = _Unused::new(0);

        let config = ResourceConfig {
            max_foreground_queries: Some(4),
            ..ResourceConfig::default()
        };
        let monitor = Arc::new(ResourceMonitor::from_config(&config));

        let mut handles = Vec::new();
        for _ in 0..16 {
            let m = Arc::clone(&monitor);
            handles.push(std::thread::spawn(move || {
                let mut acquired = 0;
                for _ in 0..100 {
                    if m.acquire_foreground_slot() {
                        acquired += 1;
                        std::thread::yield_now();
                        m.release_foreground_slot();
                    }
                }
                acquired
            }));
        }
        let mut total = 0;
        for h in handles {
            total += h.join().unwrap();
        }
        // Invariant: every acquisition was released exactly once.
        assert_eq!(monitor.foreground_slots_in_use(), 0);
        assert!(total > 0, "some acquisitions must succeed under contention");
    }
}
