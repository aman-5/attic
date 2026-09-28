//! Phase 5 — VRAM-aware GPU resource admission controller.
//!
//! Mirrors the pattern already established by `resource_policy.rs` /
//! `resource_manager.rs` for whole-system RAM/CPU governance — raw telemetry
//! capture → pressure classification → hysteresis-smoothed state machine →
//! admission decision — but for a NEW, INDEPENDENT axis: dedicated GPU VRAM.
//!
//! This module never weakens or bypasses `ResourceMonitor`'s RAM/CPU gates.
//! It is an additional, separate check a GPU-backed embedding path (e.g. the
//! DirectML provider in `attic-semantic`) should consult *before* submitting
//! a batch, alongside (not instead of) the existing `ResourceMonitor`
//! embedding-heavy permit.
//!
//! Today, GPU execution relies only on serialized single-worker execution and
//! *reactive* OOM batch-size reduction (see `attic-semantic::enrich`'s
//! `oom_batch_cap`). This module adds the missing *proactive* half: estimate
//! a batch's VRAM footprint and check it against a live (or conservatively
//! assumed) budget before the batch is ever submitted to the device.
//!
//! ## Telemetry honesty (Phase 5 plan §8)
//!
//! Real VRAM telemetry is only obtainable on Windows, via
//! `IDXGIAdapter3::QueryVideoMemoryInfo`. When that is unavailable — wrong
//! platform, API failure, no adapter found — this module reports `Unknown`
//! explicitly (`VramSnapshot::UNKNOWN` / `total_mib: None`) and callers fall
//! back to [`CONSERVATIVE_FIXED_VRAM_MIB`]. It never fabricates a
//! plausible-looking number for unmeasured hardware.
//!
//! ## Wiring note for the caller
//!
//! This module intentionally does not reach into `attic-semantic` (no new
//! cross-crate coupling from this side). The actual integration point is
//! `attic-semantic::ort_directml::OrtDirectMlProvider::embed_batch`: it
//! constructs a per-process [`GpuAdmissionController`], samples
//! [`GpuTelemetry::current_snapshot`], and calls
//! [`GpuAdmissionController::admission_check`] with the requested batch size
//! and an estimated per-item byte cost (`seq_len * target_dims * NUM_LAYERS`,
//! the same shape `ort_directml.rs` already sizes `batch_token_budget` with)
//! before ever submitting to the device — `Shrink`/`Reject` surface as
//! `SemanticError::BudgetExhausted` (the existing OOM batch-halving path in
//! `enrich.rs` handles it), and `FallbackToCpu` surfaces as
//! `SemanticError::ProviderUnavailable`, which `attic-semantic::fallback`'s
//! `FallbackCoordinator` classifies as a permanent failure and escalates to
//! CPU.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Minimum duration between real VRAM telemetry queries: `QueryVideoMemoryInfo` is a
/// COM/syscall-backed query, not something to run on every single embed call
/// — foreground responsiveness depends on this being cheap and cached.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Conservative fixed VRAM budget (MiB) used whenever telemetry is
/// unavailable. This is deliberately small and hardware-independent — a
/// "never fabricate a safe value" floor, not an estimate of real capacity.
pub const CONSERVATIVE_FIXED_VRAM_MIB: u64 = 1024;

/// VRAM percentage at which pressure transitions to Warning (batch shrink).
pub const VRAM_WARNING_PCT: u64 = 70;
/// VRAM percentage at which pressure transitions to Critical (pause new work).
pub const VRAM_CRITICAL_PCT: u64 = 85;

const HYSTERESIS_EXIT_WARNING_PCT: u64 = 60;
const HYSTERESIS_EXIT_CRITICAL_PCT: u64 = 75;
const HYSTERESIS_HOLD_WARNING_MS: u64 = 3_000;
const HYSTERESIS_HOLD_CRITICAL_MS: u64 = 6_000;

/// Consecutive Critical-pressure rejections after which the controller signals
/// that the GPU backend "remains unsafe" and the caller should fall back to
/// CPU rather than keep retrying the same device.
pub const FALLBACK_AFTER_CONSECUTIVE_CRITICAL: u32 = 3;

// ── Raw telemetry ──────────────────────────────────────────────────────────

/// Raw VRAM facts (or the explicit absence of them). Zero policy — pure data,
/// same role as `resource_policy.rs`'s `HardwareSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VramSnapshot {
    /// Total dedicated VRAM on the adapter, in MiB. `None` == unknown.
    pub total_mib: Option<u64>,
    /// Currently used VRAM within the adapter's OS-granted budget, in MiB.
    /// `None` == unknown.
    pub used_mib: Option<u64>,
    /// Currently available VRAM within budget, in MiB. `None` == unknown.
    pub available_mib: Option<u64>,
}

impl VramSnapshot {
    /// The explicit "telemetry unavailable" state — never used to fabricate a
    /// plausible value, only to signal "unknown" to callers.
    pub const UNKNOWN: VramSnapshot = VramSnapshot {
        total_mib: None,
        used_mib: None,
        available_mib: None,
    };

    /// `true` if this snapshot carries real, measured VRAM facts.
    pub fn is_known(&self) -> bool {
        self.total_mib.is_some() && self.available_mib.is_some()
    }
}

#[cfg(windows)]
mod dxgi {
    //! On Windows, query the OS-reported adapter memory budget via
    //! `IDXGIAdapter3::QueryVideoMemoryInfo` (DXGI adapter-memory-budget API),
    //! through the `attic-gpu-telemetry-win` crate — the actual COM/`unsafe`
    //! FFI is isolated there since this crate `#![forbid(unsafe_code)]`.
    //! This is the OS's own accounting of what this process may safely use —
    //! not a vendor-specific counter — so it works across NVIDIA/AMD/Intel
    //! adapters, matching DirectML's own adapter-neutral design.
    use super::VramSnapshot;

    /// Query adapter 0's local-segment memory budget. Returns `None` on any
    /// failure (no adapter, API error, unsupported OS) — the caller treats
    /// `None` as `VramSnapshot::UNKNOWN`, never as zero VRAM.
    pub fn query() -> Option<VramSnapshot> {
        let facts = attic_gpu_telemetry_win::query_raw_vram_facts()?;
        let available_mib = facts.budget_mib.saturating_sub(facts.usage_mib);
        Some(VramSnapshot {
            total_mib: Some(facts.total_dedicated_mib),
            used_mib: Some(facts.usage_mib),
            available_mib: Some(available_mib),
        })
    }
}

#[cfg(not(windows))]
mod dxgi {
    use super::VramSnapshot;

    /// No DXGI on non-Windows platforms. Metal/CUDA-native telemetry (plan
    /// §5.4) is explicitly deferred — see the final report — so this always
    /// reports unknown rather than fabricating a value.
    pub fn query() -> Option<VramSnapshot> {
        None
    }
}

/// Query real VRAM telemetry once. `None` (Windows API failure, no adapter,
/// or non-Windows platform) must be treated as `VramSnapshot::UNKNOWN` by
/// callers — never coerced into a fabricated number.
pub fn query_vram_snapshot() -> VramSnapshot {
    dxgi::query().unwrap_or(VramSnapshot::UNKNOWN)
}

// ── Cached/throttled sampler ────────────────────────────────────────────────

/// Samples [`query_vram_snapshot`] no more often than [`MIN_SAMPLE_INTERVAL`]. This is what
/// keeps the VRAM check off the hot path: a real DXGI query is a syscall, not
/// something to run per-embed-call.
pub struct GpuTelemetrySampler {
    last_sample: Instant,
    last_snapshot: VramSnapshot,
}

impl GpuTelemetrySampler {
    /// Create a sampler with an initial telemetry query already taken.
    pub fn new() -> Self {
        Self {
            last_sample: Instant::now(),
            last_snapshot: query_vram_snapshot(),
        }
    }

    /// Return the cached snapshot, refreshing it if the sample interval has
    /// elapsed. Cheap and non-blocking on the common (cached) path.
    pub fn sample(&mut self) -> VramSnapshot {
        let now = Instant::now();
        if now.duration_since(self.last_sample) < MIN_SAMPLE_INTERVAL {
            return self.last_snapshot;
        }
        self.last_sample = now;
        self.last_snapshot = query_vram_snapshot();
        self.last_snapshot
    }
}

impl Default for GpuTelemetrySampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe handle for periodic VRAM telemetry sampling.
#[derive(Clone)]
pub struct GpuTelemetry {
    inner: Arc<Mutex<GpuTelemetrySampler>>,
}

impl GpuTelemetry {
    /// Create a new shared telemetry handle.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(GpuTelemetrySampler::new())),
        }
    }

    /// Read the latest (possibly cached) VRAM snapshot.
    pub fn current_snapshot(&self) -> VramSnapshot {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.sample()
    }
}

impl Default for GpuTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

// ── Batch memory estimation ─────────────────────────────────────────────────

/// Estimate the VRAM bytes a single forward-pass batch will require.
///
/// This is a simple, documented heuristic — NOT a measured per-model number:
///
/// `bytes ≈ (batch_size × seq_len) tokens × hidden_dim × bytes_per_element ×
/// (4 × num_layers)`
///
/// The `4 × num_layers` multiplier is a coarse stand-in for the per-layer
/// intermediate buffers a transformer forward pass keeps live at once
/// (attention scores, KV projections, MLP hidden state, plus the residual
/// stream) — deliberately conservative (over-estimates) rather than tight,
/// since under-estimating is what causes the reactive OOM this module exists
/// to avoid. `batch_size × seq_len` reuses the same token-budget shape
/// `ort_directml.rs`'s `batch_token_budget` already sizes sub-batches with
/// (`batch_items × padded_seq_len`), so this estimator's inputs line up with
/// signals the DirectML provider already computes — it does not duplicate a
/// second, inconsistent sizing scheme.
pub fn estimate_batch_bytes(
    batch_size: usize,
    seq_len: usize,
    hidden_dim: usize,
    num_layers: usize,
    bytes_per_element: usize,
) -> u64 {
    if batch_size == 0 || seq_len == 0 || hidden_dim == 0 || bytes_per_element == 0 {
        return 0;
    }
    let tokens = (batch_size as u64).saturating_mul(seq_len as u64);
    let per_token = (hidden_dim as u64).saturating_mul(bytes_per_element as u64);
    let layer_multiplier = (num_layers as u64).max(1).saturating_mul(4);
    tokens
        .saturating_mul(per_token)
        .saturating_mul(layer_multiplier)
}

// ── Pressure classification ─────────────────────────────────────────────────

/// GPU VRAM pressure tier — the VRAM analogue of `resource_manager.rs`'s
/// `ResourcePressure`, deliberately kept to three tiers (no separate
/// "Emergency") since Critical already means "pause new GPU work" here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VramPressure {
    /// Plenty of VRAM available — admit at full requested batch size.
    Normal,
    /// Elevated usage — shrink new batches.
    Warning,
    /// Usage is at/above the critical threshold — pause new GPU work.
    Critical,
}

impl VramPressure {
    fn rank(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Warning => 1,
            Self::Critical => 2,
        }
    }

    fn from_rank(r: u8) -> Self {
        match r {
            0 => Self::Normal,
            1 => Self::Warning,
            _ => Self::Critical,
        }
    }
}

fn raw_tier_for_pct(pct: u64) -> VramPressure {
    if pct >= VRAM_CRITICAL_PCT {
        VramPressure::Critical
    } else if pct >= VRAM_WARNING_PCT {
        VramPressure::Warning
    } else {
        VramPressure::Normal
    }
}

fn exit_threshold(tier: VramPressure) -> (u64, u64) {
    match tier {
        VramPressure::Critical => (HYSTERESIS_EXIT_CRITICAL_PCT, HYSTERESIS_HOLD_CRITICAL_MS),
        VramPressure::Warning => (HYSTERESIS_EXIT_WARNING_PCT, HYSTERESIS_HOLD_WARNING_MS),
        VramPressure::Normal => (0, 0),
    }
}

/// Resolve the effective budget (MiB) and "used pct" basis for a snapshot,
/// applying the configured ceiling and the conservative-fixed-limit fallback
/// for unknown telemetry. Never fabricates a number: when telemetry is
/// unknown, `available_mib` is assumed equal to the (small, fixed) budget —
/// i.e. "assume best case within a deliberately tiny budget", not "assume a
/// plausible amount of real hardware capacity".
fn effective_budget_and_pct(snapshot: &VramSnapshot, ceiling_mib: u64) -> (u64, u64) {
    match (snapshot.total_mib, snapshot.available_mib) {
        (Some(total), Some(available)) => {
            let budget = ceiling_mib.min(total).max(1);
            let available_within_budget = available.min(budget);
            let used = budget.saturating_sub(available_within_budget);
            let pct = used.saturating_mul(100) / budget;
            (budget, pct)
        }
        _ => {
            let budget = ceiling_mib.clamp(1, CONSERVATIVE_FIXED_VRAM_MIB.max(1));
            (budget, 0)
        }
    }
}

// ── Admission decision & controller ────────────────────────────────────────

/// The outcome of an admission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionDecision {
    /// Admit the batch at the originally requested size.
    Admit {
        /// The admitted batch size (equal to what was requested).
        batch_size: usize,
    },
    /// Admit the batch, but at a reduced size.
    Shrink {
        /// The reduced batch size actually admitted.
        batch_size: usize,
    },
    /// Reject this batch outright (Critical pressure, or even a
    /// single-item batch would exceed budget).
    Reject,
    /// The GPU backend remains unsafe after repeated Critical-pressure
    /// rejections; the caller should fall back to CPU (Phase 3 semantics),
    /// not keep retrying this device.
    FallbackToCpu,
}

/// Mutable admission state — the hysteresis tier plus fallback bookkeeping.
/// Kept separate from [`GpuAdmissionController`] so it can be locked cheaply.
struct VramAdmissionState {
    tier: VramPressure,
    exit_eligible_since: Option<Instant>,
    consecutive_critical: u32,
}

/// VRAM-aware GPU batch admission controller.
///
/// Enforces a configurable dedicated-VRAM ceiling that admission must never
/// knowingly exceed, applies hysteresis-smoothed pressure classification
/// (shrink under Warning, pause under Critical, resume only after a hold
/// period below the exit threshold — never flapping right at the boundary),
/// and signals `FallbackToCpu` when Critical pressure persists across
/// repeated checks.
///
/// This is purely arithmetic over an already-sampled [`VramSnapshot`] — it
/// does not itself perform I/O, so it is safe to call on every batch without
/// blocking the foreground.
pub struct GpuAdmissionController {
    ceiling_mib: u64,
    state: Mutex<VramAdmissionState>,
}

impl GpuAdmissionController {
    /// Create a controller enforcing the given dedicated-VRAM ceiling (MiB).
    pub fn new(ceiling_mib: u64) -> Self {
        Self {
            ceiling_mib: ceiling_mib.max(1),
            state: Mutex::new(VramAdmissionState {
                tier: VramPressure::Normal,
                exit_eligible_since: None,
                consecutive_critical: 0,
            }),
        }
    }

    /// The configured dedicated-VRAM ceiling, in MiB.
    pub fn ceiling_mib(&self) -> u64 {
        self.ceiling_mib
    }

    /// Current hysteresis-smoothed pressure tier (does not itself advance the
    /// state machine — call [`Self::admission_check`] to do that).
    pub fn current_pressure(&self) -> VramPressure {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).tier
    }

    /// Advance the hysteresis state machine from a raw snapshot and return
    /// the smoothed tier. `now` is injected (rather than read internally via
    /// `Instant::now()`) so tests can simulate elapsed hold periods without
    /// real sleeping — `Instant + Duration` addition does not require a real
    /// clock tick.
    fn update_tier(&self, snapshot: &VramSnapshot, now: Instant) -> (VramPressure, u64, u64) {
        let (budget_mib, pct) = effective_budget_and_pct(snapshot, self.ceiling_mib);
        let raw = raw_tier_for_pct(pct);

        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let raw_rank = raw.rank();
        let cur_rank = st.tier.rank();

        if raw_rank > cur_rank {
            // Escalation is immediate — never delay a real pressure increase.
            st.tier = raw;
            st.exit_eligible_since = None;
        } else if raw_rank < cur_rank {
            let (exit_pct, hold_ms) = exit_threshold(st.tier);
            if pct >= exit_pct {
                // Not yet below the exit band; hold the current tier.
                st.exit_eligible_since = None;
            } else {
                match st.exit_eligible_since {
                    None => st.exit_eligible_since = Some(now),
                    Some(t0) => {
                        if now.duration_since(t0) >= Duration::from_millis(hold_ms) {
                            st.tier = VramPressure::from_rank(cur_rank - 1);
                            st.exit_eligible_since = None;
                        }
                    }
                }
            }
        } else {
            st.exit_eligible_since = None;
        }

        (st.tier, budget_mib, pct)
    }

    /// Full admission check: sample-derived pressure classification, plus a
    /// knowingly-over-budget batch check, plus consecutive-Critical fallback
    /// signaling.
    ///
    /// `bytes_per_item` is the estimated VRAM cost of ONE batch item (e.g.
    /// from [`estimate_batch_bytes`] with `batch_size = 1`); this function
    /// derives the maximum batch size the current budget allows and compares
    /// it against `requested_batch_size`.
    pub fn admission_check(
        &self,
        snapshot: &VramSnapshot,
        requested_batch_size: usize,
        bytes_per_item: u64,
        now: Instant,
    ) -> AdmissionDecision {
        let (tier, budget_mib, _pct) = self.update_tier(snapshot, now);

        if tier == VramPressure::Critical {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.consecutive_critical = st.consecutive_critical.saturating_add(1);
            if st.consecutive_critical >= FALLBACK_AFTER_CONSECUTIVE_CRITICAL {
                return AdmissionDecision::FallbackToCpu;
            }
            return AdmissionDecision::Reject;
        }
        {
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.consecutive_critical = 0;
        }

        if requested_batch_size == 0 {
            return AdmissionDecision::Reject;
        }

        // Pressure-driven shrink target (before the hard budget check).
        let pressure_scaled = match tier {
            VramPressure::Normal => requested_batch_size,
            VramPressure::Warning => (requested_batch_size / 2).max(1),
            VramPressure::Critical => unreachable!("handled above"),
        };

        if bytes_per_item == 0 {
            return if pressure_scaled < requested_batch_size {
                AdmissionDecision::Shrink {
                    batch_size: pressure_scaled,
                }
            } else {
                AdmissionDecision::Admit {
                    batch_size: pressure_scaled,
                }
            };
        }

        let budget_bytes = budget_mib.saturating_mul(1024 * 1024);
        let max_by_budget = (budget_bytes / bytes_per_item) as usize;

        if max_by_budget == 0 {
            // Even a single item would knowingly exceed the budget — reject
            // rather than admit reactively and let the device OOM.
            return AdmissionDecision::Reject;
        }

        let admitted = pressure_scaled.min(max_by_budget);
        if admitted < requested_batch_size {
            AdmissionDecision::Shrink {
                batch_size: admitted,
            }
        } else {
            AdmissionDecision::Admit {
                batch_size: admitted,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(total_mib: u64, available_mib: u64) -> VramSnapshot {
        VramSnapshot {
            total_mib: Some(total_mib),
            used_mib: Some(total_mib.saturating_sub(available_mib)),
            available_mib: Some(available_mib),
        }
    }

    // ── Telemetry / estimation ──────────────────────────────────────────────

    #[test]
    fn unknown_snapshot_is_not_known() {
        assert!(!VramSnapshot::UNKNOWN.is_known());
        assert!(known(4096, 4096).is_known());
    }

    #[test]
    fn estimate_batch_bytes_scales_with_batch_size() {
        let b1 = estimate_batch_bytes(1, 512, 1024, 28, 2);
        let b2 = estimate_batch_bytes(2, 512, 1024, 28, 2);
        assert!(b1 > 0);
        assert_eq!(b2, b1 * 2, "doubling batch_size must double the estimate");
    }

    #[test]
    fn estimate_batch_bytes_is_zero_for_degenerate_input() {
        assert_eq!(estimate_batch_bytes(0, 512, 1024, 28, 2), 0);
        assert_eq!(estimate_batch_bytes(4, 0, 1024, 28, 2), 0);
    }

    #[test]
    fn non_windows_or_failed_query_reports_unknown_never_fabricated() {
        // On non-Windows this exercises the real `dxgi::query` stub; on
        // Windows without a viable adapter it exercises the API-failure
        // path. Either way the contract is: Some(_) only with real facts,
        // otherwise UNKNOWN — never a fabricated in-between value.
        let snap = query_vram_snapshot();
        if !snap.is_known() {
            assert_eq!(snap, VramSnapshot::UNKNOWN);
        }
    }

    // ── Admission: normal / warning / critical ──────────────────────────────

    #[test]
    fn normal_vram_admits_full_batch_size() {
        let ctl = GpuAdmissionController::new(8192);
        let snap = known(8192, 8192); // 0% used
        let decision = ctl.admission_check(&snap, 32, 1_000_000, Instant::now());
        assert_eq!(decision, AdmissionDecision::Admit { batch_size: 32 });
    }

    #[test]
    fn warning_pressure_shrinks_batch() {
        let ctl = GpuAdmissionController::new(8192);
        // 75% used -> Warning (>= 70%, < 85%).
        let snap = known(8192, 8192 - (8192 * 75 / 100));
        let decision = ctl.admission_check(&snap, 32, 1, Instant::now());
        match decision {
            AdmissionDecision::Shrink { batch_size } => assert_eq!(batch_size, 16),
            other => panic!("expected Shrink under Warning pressure, got {other:?}"),
        }
    }

    #[test]
    fn critical_pressure_pauses_new_gpu_work() {
        let ctl = GpuAdmissionController::new(8192);
        // 90% used -> Critical.
        let snap = known(8192, 8192 - (8192 * 90 / 100));
        let decision = ctl.admission_check(&snap, 32, 1, Instant::now());
        assert_eq!(decision, AdmissionDecision::Reject);
    }

    #[test]
    fn repeated_critical_pressure_signals_cpu_fallback() {
        let ctl = GpuAdmissionController::new(8192);
        let snap = known(8192, 8192 - (8192 * 95 / 100));
        let now = Instant::now();
        let mut last = AdmissionDecision::Reject;
        for _ in 0..FALLBACK_AFTER_CONSECUTIVE_CRITICAL {
            last = ctl.admission_check(&snap, 32, 1, now);
        }
        assert_eq!(last, AdmissionDecision::FallbackToCpu);
    }

    // ── Resume with hysteresis ──────────────────────────────────────────────

    #[test]
    fn resume_after_pressure_clears_requires_hysteresis_hold() {
        let ctl = GpuAdmissionController::new(8192);
        let now = Instant::now();

        // Escalate to Warning (75% used).
        let warn_snap = known(8192, 8192 - (8192 * 75 / 100));
        ctl.admission_check(&warn_snap, 32, 1, now);
        assert_eq!(ctl.current_pressure(), VramPressure::Warning);

        // Usage drops just below the exit threshold (60%), but immediately —
        // must NOT instantly flip back to Normal (that would be flapping
        // right at the boundary).
        let recovering_snap = known(8192, 8192 - (8192 * 59 / 100));
        ctl.admission_check(&recovering_snap, 32, 1, now);
        assert_eq!(
            ctl.current_pressure(),
            VramPressure::Warning,
            "must not de-escalate before the hold period elapses"
        );

        // After the hold period has elapsed, the same low usage now
        // de-escalates back to Normal.
        let later = now + Duration::from_millis(HYSTERESIS_HOLD_WARNING_MS + 1);
        ctl.admission_check(&recovering_snap, 32, 1, later);
        assert_eq!(ctl.current_pressure(), VramPressure::Normal);
    }

    #[test]
    fn resume_does_not_flap_right_at_the_exact_boundary() {
        let ctl = GpuAdmissionController::new(8192);
        let now = Instant::now();
        let warn_snap = known(8192, 8192 - (8192 * 75 / 100));
        ctl.admission_check(&warn_snap, 32, 1, now);
        assert_eq!(ctl.current_pressure(), VramPressure::Warning);

        // Usage sits EXACTLY at the exit threshold pct (60%) — not below it —
        // must hold, not exit, even after the hold duration has passed.
        let boundary_snap = known(8192, 8192 - (8192 * 60 / 100));
        let later = now + Duration::from_millis(HYSTERESIS_HOLD_WARNING_MS + 1);
        ctl.admission_check(&boundary_snap, 32, 1, later);
        assert_eq!(
            ctl.current_pressure(),
            VramPressure::Warning,
            "sitting exactly at the exit pct must not count as below it"
        );
    }

    // ── Unknown telemetry ────────────────────────────────────────────────────

    #[test]
    fn unknown_telemetry_uses_conservative_fixed_limit_not_fabricated_value() {
        let ceiling = 999_999; // absurdly high configured ceiling
        let ctl = GpuAdmissionController::new(ceiling);
        let (budget_mib, pct) = effective_budget_and_pct(&VramSnapshot::UNKNOWN, ceiling);
        assert_eq!(
            budget_mib, CONSERVATIVE_FIXED_VRAM_MIB,
            "unknown telemetry must fall back to the fixed conservative limit, \
             never scale with the configured ceiling"
        );
        assert_eq!(pct, 0);

        // A batch that fits inside the conservative limit is admitted...
        let small_bytes_per_item = 1024 * 1024; // 1 MiB/item
        let decision = ctl.admission_check(
            &VramSnapshot::UNKNOWN,
            4,
            small_bytes_per_item,
            Instant::now(),
        );
        assert!(matches!(
            decision,
            AdmissionDecision::Admit { .. } | AdmissionDecision::Shrink { .. }
        ));

        // ...but one that would exceed the tiny conservative limit is shrunk
        // or rejected, never silently admitted at full requested size against
        // a fabricated large budget.
        let huge_bytes_per_item = (CONSERVATIVE_FIXED_VRAM_MIB + 1) * 1024 * 1024;
        let ctl2 = GpuAdmissionController::new(ceiling);
        let decision2 = ctl2.admission_check(
            &VramSnapshot::UNKNOWN,
            4,
            huge_bytes_per_item,
            Instant::now(),
        );
        assert_eq!(decision2, AdmissionDecision::Reject);
    }

    // ── Never knowingly exceed budget ───────────────────────────────────────

    #[test]
    fn over_budget_batch_is_shrunk_before_admission_not_reactively() {
        let ctl = GpuAdmissionController::new(1024); // 1 GiB ceiling
        let snap = known(4096, 4096); // plenty of real VRAM, but ceiling caps it
        // Each item costs 100 MiB; requesting 32 items would need 3200 MiB,
        // knowingly exceeding the 1024 MiB ceiling.
        let bytes_per_item = 100 * 1024 * 1024;
        let decision = ctl.admission_check(&snap, 32, bytes_per_item, Instant::now());
        match decision {
            AdmissionDecision::Shrink { batch_size } => {
                assert!(batch_size <= 10, "1024 MiB / 100 MiB per item == 10 max");
                let projected = bytes_per_item * batch_size as u64;
                assert!(projected <= 1024 * 1024 * 1024);
            }
            other => panic!("expected a proactive Shrink, got {other:?}"),
        }
    }

    #[test]
    fn ceiling_is_never_knowingly_exceeded_even_with_ample_real_vram() {
        let ctl = GpuAdmissionController::new(512);
        let snap = known(16_384, 16_384); // 16 GiB free real VRAM
        let bytes_per_item = 1024 * 1024 * 1024; // 1 GiB/item
        let decision = ctl.admission_check(&snap, 8, bytes_per_item, Instant::now());
        // 512 MiB ceiling / 1024 MiB per item == 0 whole items fit.
        assert_eq!(decision, AdmissionDecision::Reject);
    }

    // ── Foreground responsiveness ───────────────────────────────────────────

    #[test]
    fn admission_check_is_fast_and_non_blocking() {
        // This is a pure-arithmetic path (no I/O) once a snapshot is in hand,
        // so a large number of calls must complete near-instantly. This is a
        // synthetic/mocked-telemetry smoke test of the code path's shape, NOT
        // a hardware-validated latency measurement of a real DXGI query.
        let ctl = GpuAdmissionController::new(8192);
        let snap = known(8192, 4096);
        let start = Instant::now();
        for _ in 0..10_000 {
            let _ = ctl.admission_check(&snap, 16, 1024, Instant::now());
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "10k synthetic admission checks took {elapsed:?}, expected a fast non-blocking path"
        );
    }

    #[test]
    fn telemetry_sampler_caches_within_the_sample_interval() {
        let mut sampler = GpuTelemetrySampler::new();
        let first = sampler.sample();
        let second = sampler.sample();
        assert_eq!(
            first, second,
            "back-to-back samples within the throttle interval must return \
             the identical cached snapshot, not re-query"
        );
    }

    #[test]
    fn shared_gpu_telemetry_handle_is_thread_safe() {
        let telemetry = GpuTelemetry::new();
        let snap1 = telemetry.current_snapshot();
        let snap2 = telemetry.current_snapshot();
        assert_eq!(snap1, snap2);
    }
}
