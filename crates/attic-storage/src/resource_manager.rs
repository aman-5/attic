//! S7 — Production Resource Manager for Attic MCP.
//
// Phase 93+ adaptive limits (plan §6-9):
// - RecoveryStage: graduated reopening after pressure clears
// - adaptive_indexing_limit / adaptive_embedding_limit / adaptive_embedding_batch:
//   pure policy functions — single source of truth
// - EmbeddingHeavyPermit: independent RAII permit for model/batch execution
// - update_effective_limits: called from refresh_process_memory and
//   announce_pressure_change to keep all effective counters in sync.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use attic_core::ResourcePressure;
use attic_core::resources;
use tracing::{info, warn};

const RSS_SAMPLE_INTERVAL_MS: u64 = 250;

/// RSS percentage of the memory budget at which pressure transitions to Warning.
pub const PRESSURE_WARNING_PCT: u64 = 70;
/// RSS percentage of the memory budget at which pressure transitions to Critical.
pub const PRESSURE_CRITICAL_PCT: u64 = 85;

const HYSTERESIS_EXIT_WARNING_PCT: u64 = 65;
const HYSTERESIS_EXIT_CRITICAL_PCT: u64 = 78;
const HYSTERESIS_EXIT_EMERGENCY_PCT: u64 = 82;

const HYSTERESIS_HOLD_WARNING_MS: u64 = 5_000;
const HYSTERESIS_HOLD_CRITICAL_MS: u64 = 10_000;
const HYSTERESIS_HOLD_EMERGENCY_MS: u64 = 15_000;

const TIER_NORMAL: u64 = 0;
const TIER_WARNING: u64 = 1;
const TIER_CRITICAL: u64 = 2;
const TIER_EMERGENCY: u64 = 3;

fn tier_from_pressure(p: ResourcePressure) -> u64 {
    match p {
        ResourcePressure::Normal => TIER_NORMAL,
        ResourcePressure::Warning => TIER_WARNING,
        ResourcePressure::Critical => TIER_CRITICAL,
        ResourcePressure::Emergency => TIER_EMERGENCY,
    }
}

fn pressure_from_tier(t: u64) -> ResourcePressure {
    match t {
        TIER_WARNING => ResourcePressure::Warning,
        TIER_CRITICAL => ResourcePressure::Critical,
        TIER_EMERGENCY => ResourcePressure::Emergency,
        _ => ResourcePressure::Normal,
    }
}

/// Clamp `min_free_mib` so it cannot make `ResourcePressure::Critical` unreachable.
///
/// If the requested `min_free_mib` is so large that the free-memory check fires
/// before the percentage check for Critical, it is reduced to just below the
/// Critical threshold and a warning is emitted.
pub fn safe_min_free_mib(max_memory_mib: u64, min_free_mib: u64) -> u64 {
    if max_memory_mib == 0 {
        return min_free_mib;
    }
    let ceiling_pct = 100 - PRESSURE_CRITICAL_PCT;
    let ceiling_mib = max_memory_mib.saturating_mul(ceiling_pct) / 100;
    if min_free_mib < ceiling_mib {
        return min_free_mib;
    }
    let clamped = ceiling_mib.saturating_sub(1).max(1).min(max_memory_mib);
    warn!(
        "min_free_memory_mib={min_free_mib} against total_memory_budget_mib={max_memory_mib} \
         would make ResourcePressure::Critical unreachable; \
         clamping min_free_memory_mib to {clamped}"
    );
    clamped
}

/// Sample the current process RSS (resident set size) in MiB.
///
/// Returns `None` if the process information is unavailable on this platform.
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

// ── Phase 93+: Recovery stage ──────────────────────────────────────────────

/// Graduated capacity-reopening stage used during memory-pressure recovery.
///
/// After severe pressure clears, the system does not immediately jump back to
/// full capacity.  Instead it advances through these stages, waiting for a
/// stability period at each one, to avoid saw-tooth RSS oscillation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecoveryStage {
    /// No new heavy work is permitted; system is still under Emergency pressure.
    Emergency,
    /// First recovery step — a small fraction of max capacity is reopened.
    Step1,
    /// Second recovery step — roughly half of max capacity is available.
    Step2,
    /// Third recovery step — most capacity is available; awaiting final stability.
    Step3,
    /// Full capacity has been restored.
    Full,
}

impl RecoveryStage {
    /// Encode this stage as a `u64` for atomic storage.
    pub fn as_u64(self) -> u64 {
        match self {
            Self::Emergency => 0,
            Self::Step1 => 1,
            Self::Step2 => 2,
            Self::Step3 => 3,
            Self::Full => 4,
        }
    }

    /// Decode a stage from the `u64` produced by [`RecoveryStage::as_u64`].
    pub fn from_u64(v: u64) -> Self {
        match v {
            0 => Self::Emergency,
            1 => Self::Step1,
            2 => Self::Step2,
            3 => Self::Step3,
            _ => Self::Full,
        }
    }

    /// Numerator of the capacity fraction for this stage (denominator is 4).
    ///
    /// `Emergency` → 0/4, `Step1` → 1/4, …, `Full` → 4/4.
    pub fn capacity_fraction(self) -> usize {
        match self {
            Self::Emergency => 0,
            Self::Step1 => 1,
            Self::Step2 => 2,
            Self::Step3 => 3,
            Self::Full => 4,
        }
    }

    /// Milliseconds of stable Normal pressure required before advancing to the next stage.
    pub fn stability_required_ms(self) -> u64 {
        match self {
            Self::Emergency => 5_000,
            Self::Step1 => 8_000,
            Self::Step2 => 10_000,
            Self::Step3 => 12_000,
            Self::Full => u64::MAX,
        }
    }

    /// Return the next stage in the recovery sequence.
    ///
    /// Calling `advance` on [`RecoveryStage::Full`] is a no-op and returns `Full`.
    pub fn advance(self) -> Self {
        match self {
            Self::Emergency => Self::Step1,
            Self::Step1 => Self::Step2,
            Self::Step2 => Self::Step3,
            _ => Self::Full,
        }
    }
}

// ── Phase 93+: Adaptive policy free functions ──────────────────────────────

fn ceil_frac(n: usize, numer: usize, denom: usize) -> usize {
    if n == 0 || numer == 0 {
        return 0;
    }
    n.saturating_mul(numer).div_ceil(denom)
}

/// Compute the effective maximum indexing-heavy permits given `max` and current `pressure`.
///
/// This is the single authoritative policy function for indexing concurrency.
/// Normal → `max`, Warning → 75 %, Critical → 25 % (min 1), Emergency → 0.
pub fn adaptive_indexing_limit(max: usize, pressure: ResourcePressure) -> usize {
    match pressure {
        ResourcePressure::Normal => max,
        ResourcePressure::Warning => ceil_frac(max, 3, 4),
        ResourcePressure::Critical => ceil_frac(max, 1, 4).max(1),
        ResourcePressure::Emergency => 0,
    }
}

/// Compute the effective maximum embedding-heavy permits given `max` and current `pressure`.
///
/// Normal → `max`, Warning → 62.5 % (min 1), Critical → 1, Emergency → 0.
pub fn adaptive_embedding_limit(max: usize, pressure: ResourcePressure) -> usize {
    match pressure {
        ResourcePressure::Normal => max,
        ResourcePressure::Warning => ceil_frac(max, 5, 8).max(1),
        ResourcePressure::Critical => 1,
        ResourcePressure::Emergency => 0,
    }
}

/// Compute the effective embedding batch size given the configured `max` and current `pressure`.
///
/// Normal → `max`, Warning → `max/2` (min 1), Critical → `max/4` (min 1),
/// Emergency → 0 (no new batches).
pub fn adaptive_embedding_batch(max: usize, pressure: ResourcePressure) -> usize {
    match pressure {
        ResourcePressure::Normal => max,
        ResourcePressure::Warning => (max / 2).max(1),
        ResourcePressure::Critical => (max / 4).max(1),
        ResourcePressure::Emergency => 0,
    }
}

/// Compute the indexing-heavy permit limit imposed by the current [`RecoveryStage`].
///
/// Emergency → 0, Step1 → 1/4, Step2 → 2/4, Step3 → 3/4, Full → `max`.
/// The result is the *stage* cap; the caller should also apply the pressure cap
/// and take the minimum of both.
pub fn stage_indexing_limit(max: usize, stage: RecoveryStage) -> usize {
    let frac = stage.capacity_fraction();
    if frac == 0 {
        return 0;
    }
    if frac >= 4 {
        return max;
    }
    ceil_frac(max, frac, 4).max(1)
}

// ── ResourceMonitor struct ─────────────────────────────────────────────────

/// Central adaptive resource monitor for the Attic MCP server.
///
/// Tracks process RSS, applies hysteresis-smoothed pressure tiers, and exposes
/// RAII permits for foreground queries, background indexing, and embedding
/// model execution.  All fields are atomics so the monitor can be shared via
/// `Arc` without a runtime lock.
pub struct ResourceMonitor {
    memory_used: AtomicU64,
    process_rss_mib: AtomicU64,
    peak_memory_used: AtomicU64,
    last_rss_sample_ms: AtomicU64,
    foreground_slots: AtomicUsize,
    background_slots: AtomicUsize,
    emergency_mode: AtomicBool,
    start_time: Instant,
    max_memory_mib: AtomicU64,
    per_repo_memory_mib: AtomicU64,
    min_free_memory_mib: AtomicU64,
    foreground_capacity: AtomicUsize,
    background_capacity: AtomicUsize,
    hysteresis_tier: AtomicU64,
    hysteresis_exit_eligible_ms: AtomicU64,
    // Phase 93+: adaptive indexing heavy permits
    indexing_heavy_active: AtomicUsize,
    max_indexing_heavy: AtomicUsize,
    effective_indexing_heavy: AtomicUsize,
    // Phase 93+: adaptive embedding heavy permits
    embedding_heavy_active: AtomicUsize,
    max_embedding_heavy: AtomicUsize,
    effective_embedding_heavy: AtomicUsize,
    max_embedding_batch: AtomicUsize,
    effective_embedding_batch: AtomicUsize,
    // Phase 93+: graduated recovery
    recovery_stage: AtomicU64,
    recovery_stage_since_ms: AtomicU64,
    // Phase 93+: observability
    mcp_pressure_rejections: AtomicU64,
    /// Number of times the supervised relay has successfully reconnected to a
    /// replacement daemon.  Incremented by the daemon-recovery path in
    /// `attic-server`.
    pub daemon_reconnect_count: AtomicU64,
    // Phase 93+: Condvar notifications
    indexing_capacity_notify: Arc<(Mutex<()>, Condvar)>,
    embedding_capacity_notify: Arc<(Mutex<()>, Condvar)>,
    // Phase 96: deterministic testing hooks
    forced_pressure_tier: AtomicU64,
}

// ── RAII guards ────────────────────────────────────────────────────────────

/// RAII guard that holds one foreground-query slot.
///
/// The slot is released automatically when this guard is dropped.
pub struct ForegroundSlotGuard<'a> {
    monitor: &'a ResourceMonitor,
}

impl Drop for ForegroundSlotGuard<'_> {
    fn drop(&mut self) {
        self.monitor.release_foreground_slot();
    }
}

impl ForegroundSlotGuard<'_> {
    /// Return the current [`ResourceAdvisory`] for this slot's monitor.
    pub fn advisory(&self) -> ResourceAdvisory {
        current_advisory(self.monitor)
    }
}

/// RAII permit that represents one active indexing-heavy operation.
///
/// Acquiring this permit counts against both the adaptive indexing-heavy
/// limit and the background-slot capacity.  Both are released on drop.
pub struct IndexingHeavyPermit<'a> {
    monitor: &'a ResourceMonitor,
}

impl Drop for IndexingHeavyPermit<'_> {
    fn drop(&mut self) {
        self.monitor
            .indexing_heavy_active
            .fetch_sub(1, Ordering::AcqRel);
        self.monitor.release_background_slot();
        let (lock, cvar) = &*self.monitor.indexing_capacity_notify;
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        cvar.notify_all();
    }
}

/// RAII permit that represents one active embedding model-execution operation.
///
/// Acquiring this permit counts against the adaptive embedding-heavy limit.
/// The limit is released on drop; the permit is independent of indexing-heavy
/// permits so the two can be throttled separately.
pub struct EmbeddingHeavyPermit<'a> {
    monitor: &'a ResourceMonitor,
}

impl Drop for EmbeddingHeavyPermit<'_> {
    fn drop(&mut self) {
        self.monitor
            .embedding_heavy_active
            .fetch_sub(1, Ordering::AcqRel);
        let (lock, cvar) = &*self.monitor.embedding_capacity_notify;
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        cvar.notify_all();
    }
}

// ── ResourceMonitor impl ───────────────────────────────────────────────────

impl ResourceMonitor {
    /// Create a new `ResourceMonitor` with compiled-in default limits.
    pub fn new() -> Self {
        Self::from_config(&ResourceConfig::default())
    }

    /// Create a new `ResourceMonitor` initialised from the given [`ResourceConfig`].
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
        let default_embedding_batch = 64usize;
        let forced_val = match std::env::var("ATTIC_FORCE_RESOURCE_PRESSURE")
            .ok()
            .as_deref()
        {
            Some("normal") => 1,
            Some("warning") => 2,
            Some("critical") => 3,
            Some("emergency") => 4,
            _ => 0,
        };
        let init_tier = match forced_val {
            1 => TIER_NORMAL,
            2 => TIER_WARNING,
            3 => TIER_CRITICAL,
            4 => TIER_EMERGENCY,
            _ => TIER_NORMAL,
        };
        let is_emergency = forced_val == 4;
        let monitor = Self {
            memory_used: AtomicU64::new(0),
            process_rss_mib: AtomicU64::new(0),
            peak_memory_used: AtomicU64::new(0),
            last_rss_sample_ms: AtomicU64::new(0),
            foreground_slots: AtomicUsize::new(0),
            background_slots: AtomicUsize::new(0),
            emergency_mode: AtomicBool::new(is_emergency),
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
            hysteresis_tier: AtomicU64::new(init_tier),
            hysteresis_exit_eligible_ms: AtomicU64::new(0),
            indexing_heavy_active: AtomicUsize::new(0),
            max_indexing_heavy: AtomicUsize::new(background),
            effective_indexing_heavy: AtomicUsize::new(background),
            embedding_heavy_active: AtomicUsize::new(0),
            max_embedding_heavy: AtomicUsize::new(background),
            effective_embedding_heavy: AtomicUsize::new(background),
            max_embedding_batch: AtomicUsize::new(default_embedding_batch),
            effective_embedding_batch: AtomicUsize::new(default_embedding_batch),
            recovery_stage: AtomicU64::new(RecoveryStage::Full.as_u64()),
            recovery_stage_since_ms: AtomicU64::new(0),
            mcp_pressure_rejections: AtomicU64::new(0),
            daemon_reconnect_count: AtomicU64::new(0),
            indexing_capacity_notify: Arc::new((Mutex::new(()), Condvar::new())),
            embedding_capacity_notify: Arc::new((Mutex::new(()), Condvar::new())),
            forced_pressure_tier: AtomicU64::new(forced_val),
        };
        if forced_val != 0 {
            monitor.update_effective_limits();
        }
        monitor
    }

    fn elapsed_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    // ── Phase 93+: Apply resource policy maximums ──────────────────────────

    /// Apply resource-mode maximums (indexing workers, embedding workers, batch size).
    ///
    /// Called once at startup by the server after the `ResourceMode` has been
    /// resolved.  Immediately recomputes all effective limits.
    pub fn apply_resource_policy(
        &self,
        max_indexing_heavy: usize,
        max_embedding_heavy: usize,
        max_embedding_batch: usize,
    ) {
        self.max_indexing_heavy
            .store(max_indexing_heavy, Ordering::Release);
        self.max_embedding_heavy
            .store(max_embedding_heavy, Ordering::Release);
        self.max_embedding_batch
            .store(max_embedding_batch, Ordering::Release);
        self.update_effective_limits();
    }

    // ── Phase 93+: Adaptive limit computation ─────────────────────────────

    fn update_effective_limits(&self) {
        let pressure = self.guidance_pressure();
        let max_idx = self.max_indexing_heavy.load(Ordering::Relaxed);
        let max_emb = self.max_embedding_heavy.load(Ordering::Relaxed);
        let max_batch = self.max_embedding_batch.load(Ordering::Relaxed);

        let stage = RecoveryStage::from_u64(self.recovery_stage.load(Ordering::Relaxed));
        let pressure_idx_limit = adaptive_indexing_limit(max_idx, pressure);
        let stage_idx_limit = stage_indexing_limit(max_idx, stage);
        let eff_idx = pressure_idx_limit.min(stage_idx_limit);
        self.effective_indexing_heavy
            .store(eff_idx, Ordering::Release);

        let eff_emb = adaptive_embedding_limit(max_emb, pressure);
        self.effective_embedding_heavy
            .store(eff_emb, Ordering::Release);

        let eff_batch = adaptive_embedding_batch(max_batch, pressure);
        self.effective_embedding_batch
            .store(eff_batch, Ordering::Release);

        self.maybe_advance_recovery_stage(pressure);

        {
            let (lock, cvar) = &*self.indexing_capacity_notify;
            let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
            cvar.notify_all();
        }
        {
            let (lock, cvar) = &*self.embedding_capacity_notify;
            let _g = lock.lock().unwrap_or_else(|e| e.into_inner());
            cvar.notify_all();
        }
    }

    fn maybe_advance_recovery_stage(&self, pressure: ResourcePressure) {
        let stage = RecoveryStage::from_u64(self.recovery_stage.load(Ordering::Acquire));
        if stage == RecoveryStage::Full {
            return;
        }

        match pressure {
            ResourcePressure::Emergency => {
                self.recovery_stage
                    .store(RecoveryStage::Emergency.as_u64(), Ordering::Release);
                self.recovery_stage_since_ms
                    .store(self.elapsed_ms(), Ordering::Release);
                return;
            }
            ResourcePressure::Critical => {
                if stage > RecoveryStage::Step1 {
                    self.recovery_stage
                        .store(RecoveryStage::Emergency.as_u64(), Ordering::Release);
                    self.recovery_stage_since_ms
                        .store(self.elapsed_ms(), Ordering::Release);
                }
                return;
            }
            ResourcePressure::Warning => {
                if stage > RecoveryStage::Step1 {
                    self.recovery_stage
                        .store(RecoveryStage::Step1.as_u64(), Ordering::Release);
                    self.recovery_stage_since_ms
                        .store(self.elapsed_ms(), Ordering::Release);
                }
                return;
            }
            ResourcePressure::Normal => {}
        }

        let since_ms = self.recovery_stage_since_ms.load(Ordering::Acquire);
        let now = self.elapsed_ms();
        let stable_for = now.saturating_sub(since_ms);

        let required_ms = if let Ok(fast) = std::env::var("ATTIC_FAST_RECOVERY_MS") {
            fast.parse::<u64>()
                .unwrap_or_else(|_| stage.stability_required_ms())
        } else {
            stage.stability_required_ms()
        };

        if stable_for >= required_ms {
            let next = stage.advance();
            self.recovery_stage.store(next.as_u64(), Ordering::Release);
            self.recovery_stage_since_ms.store(now, Ordering::Release);
            info!(
                stage = ?stage,
                next_stage = ?next,
                stable_for_ms = stable_for,
                "recovery stage advanced"
            );
        }
    }

    fn check_file_pressure_override(&self) {
        let mut paths = Vec::new();
        if let Ok(p) = std::env::var("ATTIC_PRESSURE_OVERRIDE_FILE") {
            paths.push(std::path::PathBuf::from(p));
        }
        if let Ok(home) = std::env::var("ATTIC_HOME") {
            paths.push(std::path::PathBuf::from(home).join("pressure_override"));
        }
        if let Ok(db) = std::env::var("ATTIC_DB_PATH")
            && let Some(parent) = std::path::Path::new(&db).parent()
        {
            paths.push(parent.join("pressure_override"));
        }

        for p in paths {
            if let Ok(content) = std::fs::read_to_string(&p) {
                let trimmed = content.trim().to_lowercase();
                let forced = match trimmed.as_str() {
                    "normal" => Some(ResourcePressure::Normal),
                    "warning" => Some(ResourcePressure::Warning),
                    "critical" => Some(ResourcePressure::Critical),
                    "emergency" => Some(ResourcePressure::Emergency),
                    _ => None,
                };
                self.set_forced_pressure_for_testing(forced);
                return;
            }
        }

        if std::env::var("ATTIC_FORCE_RESOURCE_PRESSURE").is_err()
            && self.forced_pressure_tier.load(Ordering::Relaxed) != 0
        {
            self.set_forced_pressure_for_testing(None);
        }
    }

    // ── Memory tracking ────────────────────────────────────────────────────

    /// Sample the process RSS (if the sampling interval has elapsed) and update
    /// the pressure tier and all effective limits.
    ///
    /// Should be called periodically by the background resource-monitor task
    /// and also before blocking permit acquisitions.
    pub fn refresh_process_memory(&self) {
        let now = self.elapsed_ms();
        let last = self.last_rss_sample_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < RSS_SAMPLE_INTERVAL_MS {
            return;
        }
        self.last_rss_sample_ms.store(now, Ordering::Relaxed);

        self.check_file_pressure_override();

        if self.forced_pressure_tier.load(Ordering::Relaxed) != 0 {
            self.update_effective_limits();
            return;
        }

        if let Some(rss) = sample_process_rss_mib() {
            self.process_rss_mib.store(rss, Ordering::Relaxed);
            let accounted = self.memory_used.load(Ordering::Relaxed);
            let effective = rss.max(accounted);
            let prev_peak = self.peak_memory_used.load(Ordering::Relaxed);
            if effective > prev_peak {
                self.peak_memory_used.store(effective, Ordering::Relaxed);
            }
            let old_pressure = self.guidance_pressure();
            self.recompute_pressure_hysteresis(effective);
            let new_pressure = self.guidance_pressure();
            if old_pressure != new_pressure {
                self.announce_pressure_change(old_pressure, new_pressure);
            }
        }
        // Always recompute effective limits after RSS refresh.
        self.update_effective_limits();
    }

    fn recompute_pressure_hysteresis(&self, effective_mib: u64) {
        let max = self.max_memory_mib.load(Ordering::Relaxed);
        let min_free = self.min_free_memory_mib.load(Ordering::Relaxed);
        let now = self.elapsed_ms();

        // Compute instantaneous raw pressure.
        let raw = if let Some(used_pct) = effective_mib.saturating_mul(100).checked_div(max) {
            let free_mib = max.saturating_sub(effective_mib);
            if free_mib <= min_free {
                ResourcePressure::Emergency
            } else if used_pct >= PRESSURE_CRITICAL_PCT {
                ResourcePressure::Critical
            } else if used_pct >= PRESSURE_WARNING_PCT {
                ResourcePressure::Warning
            } else {
                ResourcePressure::Normal
            }
        } else {
            ResourcePressure::Normal
        };

        let current_tier = self.hysteresis_tier.load(Ordering::Relaxed);
        let current = pressure_from_tier(current_tier);
        let raw_tier = tier_from_pressure(raw);

        // Escalation is immediate.
        if raw_tier > current_tier {
            self.hysteresis_tier.store(raw_tier, Ordering::Relaxed);
            self.hysteresis_exit_eligible_ms.store(0, Ordering::Relaxed);
            return;
        }

        // De-escalation requires hysteresis.
        if raw_tier < current_tier {
            let (exit_pct, hold_ms) = match current {
                ResourcePressure::Emergency => {
                    (HYSTERESIS_EXIT_EMERGENCY_PCT, HYSTERESIS_HOLD_EMERGENCY_MS)
                }
                ResourcePressure::Critical => {
                    (HYSTERESIS_EXIT_CRITICAL_PCT, HYSTERESIS_HOLD_CRITICAL_MS)
                }
                ResourcePressure::Warning => {
                    (HYSTERESIS_EXIT_WARNING_PCT, HYSTERESIS_HOLD_WARNING_MS)
                }
                ResourcePressure::Normal => return,
            };

            let max_for_pct = self.max_memory_mib.load(Ordering::Relaxed);
            let pct = effective_mib
                .saturating_mul(100)
                .checked_div(max_for_pct)
                .unwrap_or(0);

            if pct >= exit_pct {
                // Not yet below the exit band.
                self.hysteresis_exit_eligible_ms.store(0, Ordering::Relaxed);
                return;
            }

            let eligible = self.hysteresis_exit_eligible_ms.load(Ordering::Relaxed);
            if eligible == 0 {
                // Start the hold timer.
                self.hysteresis_exit_eligible_ms
                    .store(now, Ordering::Relaxed);
                return;
            }

            if now.saturating_sub(eligible) >= hold_ms {
                // Hold period satisfied — de-escalate one tier.
                let new_tier = current_tier.saturating_sub(1);
                self.hysteresis_tier.store(new_tier, Ordering::Relaxed);
                self.hysteresis_exit_eligible_ms.store(0, Ordering::Relaxed);
            }
        } else {
            // Same tier — reset exit timer.
            self.hysteresis_exit_eligible_ms.store(0, Ordering::Relaxed);
        }
    }

    fn announce_pressure_change(&self, old: ResourcePressure, new: ResourcePressure) {
        info!(
            old_pressure = ?old,
            new_pressure = ?new,
            rss_mib = self.process_rss_mib.load(Ordering::Relaxed),
            memory_budget_mib = self.max_memory_mib.load(Ordering::Relaxed),
            "resource pressure changed"
        );
        if matches!(new, ResourcePressure::Emergency) {
            self.emergency_mode.store(true, Ordering::Release);
        } else if matches!(old, ResourcePressure::Emergency) {
            self.emergency_mode.store(false, Ordering::Release);
        }
        // Recompute effective limits on every tier transition.
        self.update_effective_limits();
    }

    /// Return the effective memory used in MiB — the maximum of the
    /// accountable watermark and the last sampled process RSS.
    pub fn effective_memory_used(&self) -> u64 {
        let accounted = self.memory_used.load(Ordering::Relaxed);
        let rss = self.process_rss_mib.load(Ordering::Relaxed);
        accounted.max(rss)
    }

    /// Record an increase in accountable memory usage by `mib` MiB.
    pub fn record_memory_increase(&self, mib: u64) {
        let prev = self.memory_used.fetch_add(mib, Ordering::AcqRel);
        let now_used = prev + mib;
        let peak = self.peak_memory_used.load(Ordering::Relaxed);
        if now_used > peak {
            self.peak_memory_used.store(now_used, Ordering::Relaxed);
        }
    }

    /// Record a decrease in accountable memory usage by `mib` MiB.
    pub fn record_memory_decrease(&self, mib: u64) {
        self.memory_used.fetch_sub(
            mib.min(self.memory_used.load(Ordering::Relaxed)),
            Ordering::AcqRel,
        );
    }

    // ── Foreground slot management ─────────────────────────────────────────

    /// Attempt to acquire a foreground-query slot without blocking.
    ///
    /// Returns `Some(ForegroundSlotGuard)` on success, or `None` if the
    /// foreground capacity is exhausted.
    pub fn acquire_foreground_slot(&self) -> Option<ForegroundSlotGuard<'_>> {
        let cap = self.foreground_capacity.load(Ordering::Acquire);
        let prev = self.foreground_slots.fetch_add(1, Ordering::AcqRel);
        if prev < cap {
            Some(ForegroundSlotGuard { monitor: self })
        } else {
            self.foreground_slots.fetch_sub(1, Ordering::AcqRel);
            None
        }
    }

    fn release_foreground_slot(&self) {
        self.foreground_slots.fetch_sub(1, Ordering::AcqRel);
    }

    /// Try to acquire a foreground slot, returning `Some(guard)` or `None` if capacity
    /// is exhausted.
    pub fn try_foreground(&self) -> Option<ForegroundSlotGuard<'_>> {
        self.refresh_process_memory();
        self.acquire_foreground_slot()
    }

    // ── Background slot management ─────────────────────────────────────────

    /// Attempt to acquire a background-worker slot without blocking.
    ///
    /// Returns `true` if the slot was granted, `false` if capacity is full.
    pub fn acquire_background_slot(&self) -> bool {
        let cap = self.background_capacity.load(Ordering::Acquire);
        let prev = self.background_slots.fetch_add(1, Ordering::AcqRel);
        if prev < cap {
            true
        } else {
            self.background_slots.fetch_sub(1, Ordering::AcqRel);
            false
        }
    }

    /// Release a previously acquired background-worker slot.
    pub fn release_background_slot(&self) {
        self.background_slots.fetch_sub(1, Ordering::AcqRel);
    }

    // ── Phase 93+: Indexing heavy permits ─────────────────────────────────

    /// Try to acquire an indexing-heavy permit without blocking.
    ///
    /// Returns `Some(IndexingHeavyPermit)` if a permit is available under
    /// current pressure limits, or `None` if the effective limit is reached.
    /// Also acquires one background slot.
    pub fn try_indexing_heavy(&self) -> Option<IndexingHeavyPermit<'_>> {
        let eff = self.effective_indexing_heavy.load(Ordering::Acquire);
        if eff == 0 {
            return None;
        }
        let prev = self.indexing_heavy_active.fetch_add(1, Ordering::AcqRel);
        if prev < eff {
            if self.acquire_background_slot() {
                return Some(IndexingHeavyPermit { monitor: self });
            }
            self.indexing_heavy_active.fetch_sub(1, Ordering::AcqRel);
        } else {
            self.indexing_heavy_active.fetch_sub(1, Ordering::AcqRel);
        }
        None
    }

    /// Block until an indexing-heavy permit becomes available, then return it.
    ///
    /// The `cancel` closure is polled on each wake; if it returns `true` the
    /// function returns `None` immediately (cancellation).  Otherwise blocks
    /// until a permit is available, refreshing RSS on each iteration.
    pub fn acquire_indexing_heavy_blocking(
        &self,
        cancel: impl Fn() -> bool,
    ) -> Option<IndexingHeavyPermit<'_>> {
        loop {
            if cancel() {
                return None;
            }
            self.refresh_process_memory();
            if let Some(permit) = self.try_indexing_heavy() {
                return Some(permit);
            }
            let (lock, cvar) = &*self.indexing_capacity_notify;
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = cvar.wait_timeout(guard, std::time::Duration::from_millis(500));
        }
    }

    // ── Phase 93+: Embedding heavy permits ────────────────────────────────

    /// Try to acquire an embedding-heavy permit without blocking.
    ///
    /// Returns `Some(EmbeddingHeavyPermit)` if a permit is available under
    /// the current adaptive embedding limit, or `None` otherwise.
    pub fn try_embedding_heavy(&self) -> Option<EmbeddingHeavyPermit<'_>> {
        let eff = self.effective_embedding_heavy.load(Ordering::Acquire);
        if eff == 0 {
            return None;
        }
        let prev = self.embedding_heavy_active.fetch_add(1, Ordering::AcqRel);
        if prev < eff {
            Some(EmbeddingHeavyPermit { monitor: self })
        } else {
            self.embedding_heavy_active.fetch_sub(1, Ordering::AcqRel);
            None
        }
    }

    /// Block until an embedding-heavy permit becomes available, then return it.
    ///
    /// The `cancel` closure is polled on each wake; if it returns `true` the
    /// function returns `None` immediately (cancellation).  Otherwise blocks
    /// until a permit is available, refreshing RSS on each iteration.
    pub fn acquire_embedding_heavy_blocking(
        &self,
        cancel: impl Fn() -> bool,
    ) -> Option<EmbeddingHeavyPermit<'_>> {
        loop {
            if cancel() {
                return None;
            }
            self.refresh_process_memory();
            if let Some(permit) = self.try_embedding_heavy() {
                return Some(permit);
            }
            let (lock, cvar) = &*self.embedding_capacity_notify;
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let _ = cvar.wait_timeout(guard, std::time::Duration::from_millis(500));
        }
    }

    /// Return the current effective embedding batch size.
    ///
    /// Callers must read this immediately before constructing each new batch;
    /// do not cache the value across batch boundaries.
    pub fn current_embedding_batch(&self) -> usize {
        self.effective_embedding_batch.load(Ordering::Acquire)
    }

    // ── Phase 93+: Observability accessors ────────────────────────────────

    /// Return the current effective indexing-heavy permit limit.
    pub fn effective_indexing_heavy_limit(&self) -> usize {
        self.effective_indexing_heavy.load(Ordering::Relaxed)
    }

    /// Return the current effective embedding-heavy permit limit.
    pub fn effective_embedding_limit(&self) -> usize {
        self.effective_embedding_heavy.load(Ordering::Relaxed)
    }

    /// Return the configured maximum indexing-heavy permit count.
    pub fn max_indexing_heavy(&self) -> usize {
        self.max_indexing_heavy.load(Ordering::Relaxed)
    }

    /// Return the number of indexing-heavy permits currently held.
    pub fn indexing_heavy_active(&self) -> usize {
        self.indexing_heavy_active.load(Ordering::Relaxed)
    }

    /// Return the number of embedding-heavy permits currently held.
    pub fn embedding_heavy_active(&self) -> usize {
        self.embedding_heavy_active.load(Ordering::Relaxed)
    }

    /// Return the current [`RecoveryStage`].
    pub fn recovery_stage(&self) -> RecoveryStage {
        RecoveryStage::from_u64(self.recovery_stage.load(Ordering::Relaxed))
    }

    /// Return the total number of MCP requests rejected due to memory pressure.
    pub fn mcp_pressure_rejections(&self) -> u64 {
        self.mcp_pressure_rejections.load(Ordering::Relaxed)
    }

    /// Increment the MCP pressure-rejection counter by one.
    pub fn record_mcp_pressure_rejection(&self) {
        self.mcp_pressure_rejections.fetch_add(1, Ordering::Relaxed);
    }

    // ── Slot/memory observability ──────────────────────────────────────────

    /// Return the number of foreground slots currently in use.
    pub fn foreground_slots_in_use(&self) -> usize {
        self.foreground_slots.load(Ordering::Relaxed)
    }

    /// Return the number of background slots currently in use.
    pub fn background_slots_in_use(&self) -> usize {
        self.background_slots.load(Ordering::Relaxed)
    }

    /// Return the configured foreground-query capacity.
    pub fn foreground_capacity(&self) -> usize {
        self.foreground_capacity.load(Ordering::Relaxed)
    }

    /// Return the configured background-worker capacity.
    pub fn background_capacity(&self) -> usize {
        self.background_capacity.load(Ordering::Relaxed)
    }

    /// Return the last sampled process RSS in MiB.
    pub fn process_rss_mib(&self) -> u64 {
        self.process_rss_mib.load(Ordering::Relaxed)
    }

    /// Return the current accountable memory watermark in MiB.
    pub fn memory_used_mib(&self) -> u64 {
        self.memory_used.load(Ordering::Relaxed)
    }

    /// Return the peak accountable memory watermark in MiB.
    pub fn peak_memory_used_mib(&self) -> u64 {
        self.peak_memory_used.load(Ordering::Relaxed)
    }

    /// Return `true` if the monitor is currently in Emergency mode.
    pub fn is_emergency(&self) -> bool {
        self.emergency_mode.load(Ordering::Acquire)
    }

    /// Manually set or clear the emergency-mode flag.
    ///
    /// Normally set automatically by pressure changes; exposed for testing.
    pub fn set_emergency(&self, value: bool) {
        self.emergency_mode.store(value, Ordering::Release);
    }

    /// Return the configured total memory budget in MiB.
    pub fn max_memory_mib(&self) -> u64 {
        self.max_memory_mib.load(Ordering::Relaxed)
    }

    /// Return the configured per-repository memory budget in MiB.
    pub fn per_repo_memory_mib(&self) -> u64 {
        self.per_repo_memory_mib.load(Ordering::Relaxed)
    }

    /// Return the configured minimum free memory in MiB.
    pub fn min_free_memory_mib(&self) -> u64 {
        self.min_free_memory_mib.load(Ordering::Relaxed)
    }

    /// Return the number of seconds since this monitor was created.
    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Apply a new [`ResourceConfig`], updating all limits and recomputing
    /// effective values.
    pub fn apply_config(&self, config: &ResourceConfig) {
        let max_memory_mib = config
            .total_memory_budget_mib
            .unwrap_or(self.max_memory_mib.load(Ordering::Relaxed))
            .max(1);
        self.max_memory_mib.store(max_memory_mib, Ordering::Release);
        if let Some(v) = config.per_repo_memory_budget_mib {
            self.per_repo_memory_mib.store(v, Ordering::Release);
        }
        let min_free = safe_min_free_mib(
            max_memory_mib,
            config
                .min_free_memory_mib
                .unwrap_or(self.min_free_memory_mib.load(Ordering::Relaxed)),
        );
        self.min_free_memory_mib.store(min_free, Ordering::Release);
        if let Some(v) = config.max_foreground_queries {
            self.foreground_capacity.store(v.max(1), Ordering::Release);
        }
        if let Some(v) = config.max_background_workers {
            self.background_capacity.store(v, Ordering::Release);
        }
        self.update_effective_limits();
    }

    /// Manually force a resource pressure tier for deterministic testing.
    ///
    /// Pass `Some(tier)` to override dynamic RSS/hysteresis tracking, or `None`
    /// to resume normal dynamic tracking.
    pub fn set_forced_pressure_for_testing(&self, pressure: Option<ResourcePressure>) {
        let old = self.guidance_pressure();
        let val = match pressure {
            None => 0,
            Some(ResourcePressure::Normal) => 1,
            Some(ResourcePressure::Warning) => 2,
            Some(ResourcePressure::Critical) => 3,
            Some(ResourcePressure::Emergency) => 4,
        };
        self.forced_pressure_tier.store(val, Ordering::Release);
        if let Some(p) = pressure {
            self.hysteresis_tier
                .store(tier_from_pressure(p), Ordering::Release);
            if matches!(p, ResourcePressure::Emergency) {
                self.emergency_mode.store(true, Ordering::Release);
            } else {
                self.emergency_mode.store(false, Ordering::Release);
            }
            if old != p {
                self.announce_pressure_change(old, p);
            }
        } else {
            self.emergency_mode.store(false, Ordering::Release);
            self.hysteresis_tier.store(TIER_NORMAL, Ordering::Release);
            if old != ResourcePressure::Normal {
                self.announce_pressure_change(old, ResourcePressure::Normal);
            }
        }
        self.update_effective_limits();
    }

    /// Manually set the recovery stage for deterministic testing.
    pub fn set_recovery_stage_for_testing(&self, stage: RecoveryStage) {
        self.recovery_stage.store(stage.as_u64(), Ordering::Release);
        self.recovery_stage_since_ms
            .store(self.elapsed_ms(), Ordering::Release);
        self.update_effective_limits();
    }

    // ── Pressure accessors ─────────────────────────────────────────────────

    /// Return the raw hysteresis-smoothed pressure tier (internal helper).
    pub fn guidance_pressure(&self) -> ResourcePressure {
        match self.forced_pressure_tier.load(Ordering::Relaxed) {
            1 => ResourcePressure::Normal,
            2 => ResourcePressure::Warning,
            3 => ResourcePressure::Critical,
            4 => ResourcePressure::Emergency,
            _ => pressure_from_tier(self.hysteresis_tier.load(Ordering::Relaxed)),
        }
    }

    /// Return the stable hysteresis-smoothed [`ResourcePressure`] tier.
    ///
    /// This is the same as [`pressure`] and is provided as a named alias for
    /// call sites that prefer the `stable_tier_pressure` name.
    pub fn stable_tier_pressure(&self) -> ResourcePressure {
        self.guidance_pressure()
    }

    /// Return the current hysteresis-smoothed [`ResourcePressure`].
    pub fn pressure(&self) -> ResourcePressure {
        self.guidance_pressure()
    }
}

impl Default for ResourceMonitor {
    fn default() -> Self {
        Self::new()
    }
}

// ── ResourceAdvisory ──────────────────────────────────────────────────────

/// A coarse advisory summarising the current resource health of the monitor.
///
/// Callers can use this to make admission decisions without reading individual
/// pressure values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAdvisory {
    /// Resource usage is within normal bounds; all operations are permitted.
    Ok,
    /// Resource usage is elevated; background work should be throttled.
    Degraded,
    /// Resource usage is critical or emergency; new expensive work should be
    /// refused.
    Restricted,
}

/// Compute the [`ResourceAdvisory`] for the given monitor.
pub fn current_advisory(monitor: &ResourceMonitor) -> ResourceAdvisory {
    match monitor.pressure() {
        ResourcePressure::Normal => ResourceAdvisory::Ok,
        ResourcePressure::Warning => ResourceAdvisory::Degraded,
        ResourcePressure::Critical | ResourcePressure::Emergency => ResourceAdvisory::Restricted,
    }
}

// ── ResourceConfig ────────────────────────────────────────────────────────

/// Runtime resource-limit configuration for the Attic server.
///
/// All fields are optional; `None` means "use the compiled-in default".
#[derive(Debug, Clone, Default)]
pub struct ResourceConfig {
    /// Total memory budget for the server process, in MiB.
    pub total_memory_budget_mib: Option<u64>,
    /// Per-repository memory budget used for admission decisions, in MiB.
    pub per_repo_memory_budget_mib: Option<u64>,
    /// Minimum free memory required before new heavy work is allowed, in MiB.
    pub min_free_memory_mib: Option<u64>,
    /// Maximum number of concurrent foreground (user-facing) queries.
    pub max_foreground_queries: Option<usize>,
    /// Maximum number of concurrent background indexing workers.
    pub max_background_workers: Option<usize>,
}

impl ResourceConfig {
    /// Validate the configuration, returning an error string if any field is
    /// out of range.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(0) = self.total_memory_budget_mib {
            return Err("total_memory_budget_mib must be > 0".to_string());
        }
        if let Some(0) = self.max_foreground_queries {
            return Err("max_foreground_queries must be > 0".to_string());
        }
        Ok(())
    }

    /// Apply this configuration to `monitor`.
    pub fn apply_to(&self, monitor: &ResourceMonitor) {
        monitor.apply_config(self);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor_with_budget(mib: u64) -> ResourceMonitor {
        let config = ResourceConfig {
            total_memory_budget_mib: Some(mib),
            max_foreground_queries: Some(8),
            max_background_workers: Some(8),
            ..Default::default()
        };
        ResourceMonitor::from_config(&config)
    }

    #[test]
    fn test_adaptive_indexing_limit() {
        assert_eq!(adaptive_indexing_limit(8, ResourcePressure::Normal), 8);
        assert_eq!(adaptive_indexing_limit(8, ResourcePressure::Warning), 6);
        assert_eq!(adaptive_indexing_limit(8, ResourcePressure::Critical), 2);
        assert_eq!(adaptive_indexing_limit(8, ResourcePressure::Emergency), 0);
        assert_eq!(adaptive_indexing_limit(1, ResourcePressure::Critical), 1);
    }

    #[test]
    fn test_adaptive_embedding_limit() {
        assert_eq!(adaptive_embedding_limit(8, ResourcePressure::Normal), 8);
        assert_eq!(adaptive_embedding_limit(8, ResourcePressure::Warning), 5);
        assert_eq!(adaptive_embedding_limit(8, ResourcePressure::Critical), 1);
        assert_eq!(adaptive_embedding_limit(8, ResourcePressure::Emergency), 0);
        assert_eq!(adaptive_embedding_limit(1, ResourcePressure::Warning), 1);
    }

    #[test]
    fn test_adaptive_embedding_batch() {
        assert_eq!(adaptive_embedding_batch(64, ResourcePressure::Normal), 64);
        assert_eq!(adaptive_embedding_batch(64, ResourcePressure::Warning), 32);
        assert_eq!(adaptive_embedding_batch(64, ResourcePressure::Critical), 16);
        assert_eq!(adaptive_embedding_batch(64, ResourcePressure::Emergency), 0);
        assert_eq!(adaptive_embedding_batch(1, ResourcePressure::Warning), 1);
        assert_eq!(adaptive_embedding_batch(1, ResourcePressure::Critical), 1);
    }

    #[test]
    fn test_stage_indexing_limit() {
        assert_eq!(stage_indexing_limit(8, RecoveryStage::Emergency), 0);
        assert_eq!(stage_indexing_limit(8, RecoveryStage::Step1), 2);
        assert_eq!(stage_indexing_limit(8, RecoveryStage::Step2), 4);
        assert_eq!(stage_indexing_limit(8, RecoveryStage::Step3), 6);
        assert_eq!(stage_indexing_limit(8, RecoveryStage::Full), 8);
    }

    #[test]
    fn test_recovery_stage_advance() {
        assert_eq!(RecoveryStage::Emergency.advance(), RecoveryStage::Step1);
        assert_eq!(RecoveryStage::Step1.advance(), RecoveryStage::Step2);
        assert_eq!(RecoveryStage::Step2.advance(), RecoveryStage::Step3);
        assert_eq!(RecoveryStage::Step3.advance(), RecoveryStage::Full);
        assert_eq!(RecoveryStage::Full.advance(), RecoveryStage::Full);
    }

    #[test]
    fn test_recovery_stage_roundtrip() {
        for stage in [
            RecoveryStage::Emergency,
            RecoveryStage::Step1,
            RecoveryStage::Step2,
            RecoveryStage::Step3,
            RecoveryStage::Full,
        ] {
            assert_eq!(RecoveryStage::from_u64(stage.as_u64()), stage);
        }
    }

    #[test]
    fn test_foreground_slot_acquisition() {
        let m = monitor_with_budget(8192);
        let s1 = m.acquire_foreground_slot();
        assert!(s1.is_some());
        assert_eq!(m.foreground_slots_in_use(), 1);
        drop(s1);
        assert_eq!(m.foreground_slots_in_use(), 0);
    }

    #[test]
    fn test_foreground_slot_exhaustion() {
        let config = ResourceConfig {
            total_memory_budget_mib: Some(8192),
            max_foreground_queries: Some(2),
            ..Default::default()
        };
        let m = ResourceMonitor::from_config(&config);
        let s1 = m.acquire_foreground_slot();
        let s2 = m.acquire_foreground_slot();
        let s3 = m.acquire_foreground_slot();
        assert!(s1.is_some());
        assert!(s2.is_some());
        assert!(s3.is_none());
        drop(s1);
        let s4 = m.acquire_foreground_slot();
        assert!(s4.is_some());
    }

    #[test]
    fn test_indexing_heavy_permit_normal() {
        let m = monitor_with_budget(8192);
        m.apply_resource_policy(4, 4, 64);
        let p = m.try_indexing_heavy();
        assert!(p.is_some());
        assert_eq!(m.indexing_heavy_active(), 1);
        drop(p);
        assert_eq!(m.indexing_heavy_active(), 0);
    }

    #[test]
    fn test_embedding_heavy_permit_drops() {
        let m = monitor_with_budget(8192);
        m.apply_resource_policy(4, 4, 64);
        let p = m.try_embedding_heavy();
        assert!(p.is_some());
        assert_eq!(m.embedding_heavy_active(), 1);
        drop(p);
        assert_eq!(m.embedding_heavy_active(), 0);
    }

    #[test]
    fn test_current_embedding_batch_normal() {
        let m = monitor_with_budget(8192);
        m.apply_resource_policy(4, 4, 64);
        assert_eq!(m.current_embedding_batch(), 64);
    }

    #[test]
    fn test_memory_increase_decrease() {
        let m = monitor_with_budget(8192);
        m.record_memory_increase(100);
        assert_eq!(m.memory_used_mib(), 100);
        m.record_memory_decrease(40);
        assert_eq!(m.memory_used_mib(), 60);
        m.record_memory_decrease(9999);
        assert_eq!(m.memory_used_mib(), 0);
    }

    #[test]
    fn test_peak_memory() {
        let m = monitor_with_budget(8192);
        m.record_memory_increase(200);
        m.record_memory_decrease(100);
        assert!(m.peak_memory_used_mib() >= 200);
    }

    #[test]
    fn test_advisory_normal() {
        let m = monitor_with_budget(8192);
        assert_eq!(current_advisory(&m), ResourceAdvisory::Ok);
    }

    #[test]
    fn test_mcp_rejection_counter() {
        let m = monitor_with_budget(8192);
        assert_eq!(m.mcp_pressure_rejections(), 0);
        m.record_mcp_pressure_rejection();
        m.record_mcp_pressure_rejection();
        assert_eq!(m.mcp_pressure_rejections(), 2);
    }

    #[test]
    fn test_safe_min_free_mib_clamp() {
        let clamped = safe_min_free_mib(1000, 200);
        assert!(clamped < 200);
        let ok = safe_min_free_mib(1000, 50);
        assert_eq!(ok, 50);
    }

    #[test]
    fn test_resource_config_validate() {
        let mut c = ResourceConfig::default();
        assert!(c.validate().is_ok());
        c.total_memory_budget_mib = Some(0);
        assert!(c.validate().is_err());
        c.total_memory_budget_mib = Some(4096);
        c.max_foreground_queries = Some(0);
        assert!(c.validate().is_err());
    }

    #[test]
    fn test_apply_resource_policy_limits() {
        let m = monitor_with_budget(8192);
        m.apply_resource_policy(8, 6, 64);
        assert_eq!(m.max_indexing_heavy(), 8);
        assert_eq!(m.effective_indexing_heavy_limit(), 8);
        assert_eq!(m.current_embedding_batch(), 64);
    }

    #[test]
    fn test_set_emergency() {
        let m = monitor_with_budget(8192);
        assert!(!m.is_emergency());
        m.set_emergency(true);
        assert!(m.is_emergency());
        m.set_emergency(false);
        assert!(!m.is_emergency());
    }

    #[test]
    fn test_uptime_secs() {
        let m = monitor_with_budget(8192);
        assert!(m.uptime_secs() < 5);
    }

    #[test]
    fn test_daemon_reconnect_count() {
        let m = monitor_with_budget(8192);
        assert_eq!(m.daemon_reconnect_count.load(Ordering::Relaxed), 0);
        m.daemon_reconnect_count.fetch_add(1, Ordering::Relaxed);
        assert_eq!(m.daemon_reconnect_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_set_forced_pressure_for_testing() {
        let m = monitor_with_budget(8192);
        m.apply_resource_policy(8, 6, 64);
        assert_eq!(m.pressure(), ResourcePressure::Normal);
        assert_eq!(m.effective_indexing_heavy_limit(), 8);

        m.set_forced_pressure_for_testing(Some(ResourcePressure::Warning));
        assert_eq!(m.pressure(), ResourcePressure::Warning);
        assert_eq!(m.effective_indexing_heavy_limit(), 6);
        assert_eq!(m.current_embedding_batch(), 32);

        m.set_forced_pressure_for_testing(Some(ResourcePressure::Critical));
        assert_eq!(m.pressure(), ResourcePressure::Critical);
        assert_eq!(m.effective_indexing_heavy_limit(), 2);
        assert_eq!(m.current_embedding_batch(), 16);

        m.set_forced_pressure_for_testing(Some(ResourcePressure::Emergency));
        assert_eq!(m.pressure(), ResourcePressure::Emergency);
        assert!(m.is_emergency());
        assert_eq!(m.effective_indexing_heavy_limit(), 0);
        assert_eq!(m.current_embedding_batch(), 0);

        m.set_forced_pressure_for_testing(None);
        assert_eq!(m.pressure(), ResourcePressure::Normal);
        assert!(!m.is_emergency());
        assert_eq!(m.effective_indexing_heavy_limit(), 8);
    }

    #[test]
    fn test_set_recovery_stage_for_testing() {
        let m = monitor_with_budget(8192);
        m.apply_resource_policy(8, 6, 64);

        m.set_recovery_stage_for_testing(RecoveryStage::Step1);
        assert_eq!(m.recovery_stage(), RecoveryStage::Step1);
        assert_eq!(m.effective_indexing_heavy_limit(), 2);

        m.set_recovery_stage_for_testing(RecoveryStage::Step2);
        assert_eq!(m.recovery_stage(), RecoveryStage::Step2);
        assert_eq!(m.effective_indexing_heavy_limit(), 4);

        m.set_recovery_stage_for_testing(RecoveryStage::Step3);
        assert_eq!(m.recovery_stage(), RecoveryStage::Step3);
        assert_eq!(m.effective_indexing_heavy_limit(), 6);

        m.set_recovery_stage_for_testing(RecoveryStage::Full);
        assert_eq!(m.recovery_stage(), RecoveryStage::Full);
        assert_eq!(m.effective_indexing_heavy_limit(), 8);
    }
}
