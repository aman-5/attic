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

/// Query adapter 0's local-segment memory budget via
/// `IDXGIAdapter3::QueryVideoMemoryInfo`. Returns `None` on any failure (no
/// compatible adapter, API error, non-Windows target) — callers must treat
/// `None` as "unknown", never as zero VRAM.
#[cfg(windows)]
pub fn query_raw_vram_facts() -> Option<RawVramFacts> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO,
        IDXGIAdapter3, IDXGIFactory4,
    };
    use windows::core::Interface;

    // Safety: standard DXGI COM interop via the `windows` crate's safe
    // wrappers around the underlying FFI. Every fallible step is `?`-ed, so a
    // missing/incompatible adapter or COM failure simply yields `None`
    // rather than propagating a garbage value.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        let adapter1 = factory.EnumAdapters1(0).ok()?;
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
}
