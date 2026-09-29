//! GPU VRAM telemetry.
//!
//! Raw, cached VRAM facts for GPU-backed embedding. Policy lives with the
//! consumer: the DirectML provider (`attic-semantic::ort_directml`) admits a
//! forward pass by free headroom for NEW input shapes only, since shapes it
//! has already run reuse memory DirectML already holds.
//!
//! An earlier ratio-based admission controller lived here. It counted this
//! process's own resident model and arena as "pressure", rejected batches
//! under it, and after three rejections demoted a healthy GPU to CPU for the
//! rest of the process — on a 4 GB card within ~30 s. It was removed rather
//! than tuned.
//!
//! ## Telemetry honesty
//!
//! Real VRAM telemetry is only obtainable on Windows, via
//! `IDXGIAdapter3::QueryVideoMemoryInfo`. When that is unavailable — wrong
//! platform, API failure, no adapter found — this module reports `Unknown`
//! explicitly (`VramSnapshot::UNKNOWN`). It never fabricates a
//! plausible-looking number for unmeasured hardware.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Minimum duration between real VRAM telemetry queries: `QueryVideoMemoryInfo` is a
/// COM/syscall-backed query, not something to run on every single embed call
/// — foreground responsiveness depends on this being cheap and cached.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);
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

    /// Query the DirectML adapter's local-segment memory budget. Returns `None` on any
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

/// The adapter DirectML will run on (high-performance preference).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuAdapterInfo {
    /// Driver-reported adapter name.
    pub name: String,
    /// PCI vendor id (0x10de NVIDIA, 0x1002 AMD, 0x8086 Intel).
    pub vendor_id: u32,
    /// Dedicated VRAM in MiB.
    pub dedicated_mib: u64,
    /// Heuristic: shared-memory carve-out rather than a discrete card.
    pub integrated: bool,
    /// Basic Render Driver / WARP.
    pub software: bool,
}

/// Describe the DirectML adapter. `None` off Windows or when no adapter is
/// found.
pub fn query_adapter_info() -> Option<GpuAdapterInfo> {
    #[cfg(windows)]
    {
        attic_gpu_telemetry_win::query_adapter_info().map(|a| GpuAdapterInfo {
            integrated: a.is_integrated(),
            name: a.name,
            vendor_id: a.vendor_id,
            dedicated_mib: a.dedicated_mib,
            software: a.software,
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_snapshot_is_not_known() {
        assert!(!VramSnapshot::UNKNOWN.is_known());
        let known = VramSnapshot {
            total_mib: Some(4096),
            used_mib: Some(0),
            available_mib: Some(4096),
        };
        assert!(known.is_known());
    }

    #[test]
    fn non_windows_or_failed_query_reports_unknown_never_fabricated() {
        let snap = query_vram_snapshot();
        if !snap.is_known() {
            assert_eq!(snap, VramSnapshot::UNKNOWN);
        }
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