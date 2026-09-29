//! Windows-only DXGI adapter-memory-budget FFI shim.
//!
//! This crate exists ONLY to isolate the small amount of `unsafe` COM interop
//! `IDXGIAdapter3::QueryVideoMemoryInfo` requires away from `attic-storage`,
//! which sets `#![forbid(unsafe_code)]` crate-wide (a `forbid` lint cannot be
//! locally re-allowed anywhere in that crate, even under `#[cfg(windows)]`).
//!
//! `attic-storage::gpu_telemetry` is the actual Phase 5 VRAM telemetry /
//! admission-controller module; it depends on this crate only on Windows
//! (via a `target.'cfg(windows)'.dependencies` entry) and treats every
//! `None`/`Err` here as "telemetry unavailable" — never fabricating a value.

/// Raw facts read directly from the OS's DXGI adapter-memory-budget API, in
/// MiB. `total_dedicated_mib` is the adapter's total dedicated VRAM;
/// `budget_mib`/`usage_mib` are the OS-granted local-segment budget and this
/// process's current usage within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawVramFacts {
    pub total_dedicated_mib: u64,
    pub budget_mib: u64,
    pub usage_mib: u64,
}

/// Identity of the adapter DirectML will run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawAdapterInfo {
    /// Driver-reported adapter name.
    pub name: String,
    /// PCI vendor id.
    pub vendor_id: u32,
    /// Dedicated VRAM in MiB.
    pub dedicated_mib: u64,
    /// Shared system memory the adapter may use, in MiB.
    pub shared_mib: u64,
    /// Basic Render Driver / WARP.
    pub software: bool,
}

/// Dedicated VRAM below this is a carve-out, not a discrete card. DXGI has
/// no integrated flag, and every adapter reports shared memory, so the
/// dedicated size is the only portable signal.
pub const INTEGRATED_MAX_DEDICATED_MIB: u64 = 1024;

impl RawAdapterInfo {
    /// Heuristic integrated-GPU check; see [`INTEGRATED_MAX_DEDICATED_MIB`].
    pub fn is_integrated(&self) -> bool {
        !self.software && self.dedicated_mib < INTEGRATED_MAX_DEDICATED_MIB
    }
}

/// The adapter DirectML selects with `PerformancePreference::HighPerformance`:
/// the first non-software adapter in high-performance order. On hybrid
/// laptops adapter 0 is usually the integrated GPU, so enumerating by index
/// would describe the wrong device.
#[cfg(windows)]
unsafe fn preferred_adapter() -> Option<windows::Win32::Graphics::Dxgi::IDXGIAdapter1> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
        IDXGIAdapter1, IDXGIFactory1, IDXGIFactory6,
    };
    use windows::core::Interface;

    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let is_hw = |a: &IDXGIAdapter1| {
            a.GetDesc1()
                .map(|d| d.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) == 0)
                .unwrap_or(false)
        };
        if let Ok(f6) = factory.cast::<IDXGIFactory6>() {
            for i in 0..16 {
                let Ok(a) = f6
                    .EnumAdapterByGpuPreference::<IDXGIAdapter1>(i, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
                else {
                    break;
                };
                if is_hw(&a) {
                    return Some(a);
                }
            }
        }
        factory.EnumAdapters1(0).ok()
    }
}

/// Describe the adapter DirectML will use. `None` when there is no adapter
/// or the query fails (and always on non-Windows targets).
#[cfg(windows)]
pub fn query_adapter_info() -> Option<RawAdapterInfo> {
    use windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_FLAG_SOFTWARE;

    // Safety: DXGI COM calls through the `windows` crate; every fallible step
    // yields `None` instead of a garbage value.
    unsafe {
        let adapter = preferred_adapter()?;
        let desc = adapter.GetDesc1().ok()?;
        let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(desc.Description.len());
        Some(RawAdapterInfo {
            name: String::from_utf16_lossy(&desc.Description[..len]).trim().to_string(),
            vendor_id: desc.VendorId,
            dedicated_mib: (desc.DedicatedVideoMemory as u64) / (1024 * 1024),
            shared_mib: (desc.SharedSystemMemory as u64) / (1024 * 1024),
            software: desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0,
        })
    }
}

#[cfg(not(windows))]
pub fn query_adapter_info() -> Option<RawAdapterInfo> {
    None
}

/// Query the preferred adapter's local-segment memory budget via
/// `IDXGIAdapter3::QueryVideoMemoryInfo`. Returns `None` on any failure (no
/// compatible adapter, API error, non-Windows target) — callers must treat
/// `None` as "unknown", never as zero VRAM.
#[cfg(windows)]
pub fn query_raw_vram_facts() -> Option<RawVramFacts> {
    use windows::Win32::Graphics::Dxgi::{
        DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
    };
    use windows::core::Interface;

    // Safety: standard DXGI COM interop via the `windows` crate's safe
    // wrappers around the underlying FFI. Every fallible step is `?`-ed, so a
    // missing/incompatible adapter or COM failure simply yields `None`
    // rather than propagating a garbage value.
    unsafe {
        let adapter1 = preferred_adapter()?;
        let desc = adapter1.GetDesc1().ok()?;
        let adapter3: IDXGIAdapter3 = adapter1.cast().ok()?;
        let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        adapter3
            .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut info)
            .ok()?;

        let total_dedicated_mib = (desc.DedicatedVideoMemory as u64) / (1024 * 1024);
        if total_dedicated_mib == 0 {
            // A software/basic-render adapter with no dedicated VRAM is not
            // a usable GPU budget.
            return None;
        }
        let budget_mib = info.Budget / (1024 * 1024);
        let usage_mib = info.CurrentUsage / (1024 * 1024);

        Some(RawVramFacts {
            total_dedicated_mib,
            budget_mib,
            usage_mib,
        })
    }
}

#[cfg(not(windows))]
pub fn query_raw_vram_facts() -> Option<RawVramFacts> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Smoke test only: asserts internal consistency IF an adapter was
    /// found. Does not assert `Some(_)` — CI/dev machines may have no GPU,
    /// a software adapter, or a locked-down driver, and `None` is a
    /// perfectly valid, honest result. This does NOT constitute hardware
    /// validation of VRAM pressure behavior — see attic-storage's
    /// gpu_telemetry tests for the (synthetic-telemetry) pressure-response
    /// coverage.
    #[test]
    fn query_raw_vram_facts_is_internally_consistent_if_present() {
        if let Some(facts) = query_raw_vram_facts() {
            assert!(facts.total_dedicated_mib > 0);
        }
    }

    #[test]
    fn adapter_info_matches_vram_facts_if_present() {
        let info = query_adapter_info();
        eprintln!("preferred adapter: {info:?}");
        if let (Some(info), Some(facts)) = (info, query_raw_vram_facts()) {
            assert!(!info.name.is_empty());
            assert_eq!(info.dedicated_mib, facts.total_dedicated_mib);
        }
    }

    #[test]
    fn integrated_heuristic() {
        let a = |dedicated_mib, software| RawAdapterInfo {
            name: "x".into(),
            vendor_id: 0,
            dedicated_mib,
            shared_mib: 8192,
            software,
        };
        assert!(a(128, false).is_integrated());
        assert!(!a(4096, false).is_integrated());
        assert!(!a(0, true).is_integrated());
    }
}
