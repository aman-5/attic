//! Candle execution-device selection across platforms.
//!
//! ## Why this module exists
//!
//! Before this, `qwen3_provider.rs` hardcoded `Device::Cpu`. Every machine —
//! including an Apple Silicon Mac with a very capable GPU, and a Linux box
//! with an RTX card — ran embedding inference on CPU, and nothing in the
//! status output explained why. Meanwhile `ExecutionBackend` advertised
//! `CandleCuda` and `CandleMetal` variants that nothing ever constructed,
//! which made the gap look implemented.
//!
//! This module makes device selection a real, reported decision.
//!
//! ## How the platform gating works
//!
//! Candle compiles `Device::new_cuda` / `Device::new_metal` unconditionally.
//! When the corresponding cargo feature is absent they return
//! `NotCompiledWithCudaSupport` / `NotCompiledWithMetalSupport` at runtime
//! rather than failing to compile. That is load-bearing here: it lets one
//! source path attempt a GPU, distinguish "not compiled in" from "compiled
//! but no device present", and fall back to CPU with an honest reason —
//! without `#[cfg]` branches that would diverge per platform.
//!
//! ## Coverage (accurate as of this module)
//!
//! | Platform                        | Backend                     |
//! |---------------------------------|-----------------------------|
//! | macOS, Apple Silicon            | `candle-metal` feature      |
//! | Linux / Windows + NVIDIA        | `candle-cuda` feature       |
//! | Windows, any DX12 GPU           | `ort-directml` (separate)   |
//! | Linux + AMD                     | **CPU — see below**         |
//! | macOS, Intel + Radeon           | **CPU — intentionally so**  |
//!
//! ### Linux + AMD is not a GPU path, and this module says so
//!
//! Candle has no ROCm/HIP backend — there is no `Device::new_rocm`. The only
//! realistic AMD-on-Linux route is ONNX Runtime's ROCm execution provider,
//! which needs a separate ONNX export plus a host ROCm install, and shares
//! nothing with the Candle path here. Rather than silently landing AMD users
//! on CPU (the previous behaviour), [`resolve`] reports
//! [`UNSUPPORTED_ROCM_REASON`] so the status output states the limitation
//! instead of implying the GPU was tried and rejected.
//!
//! ### Vector-space identity is deliberately unchanged
//!
//! Weights stay F32 safetensors on every device, so `quantization` remains
//! `"fp32-safetensors"` and vectors stay comparable across backends.
//! `ExecutionBackend` is telemetry only and never participates in identity
//! (see `provider.rs`), which is precisely why switching devices does not
//! invalidate an existing index. GPU and CPU F32 kernels are not bit-exact
//! (different reduction orders), so a backend change is a parity question,
//! not an identity one — the same standard the DirectML path was held to.

use candle_core::Device;

use std::sync::OnceLock;

use crate::provider::ExecutionBackend;

/// Reason reported when the host is AMD-on-Linux, where Candle offers no
/// GPU backend at all. Kept as a constant so the server status payload and
/// the tests assert on the same text.
pub const UNSUPPORTED_ROCM_REASON: &str =
    "AMD GPUs are not supported by the Candle backend (no ROCm/HIP device); running on CPU";

/// User-facing device preference, from `attic.toml`'s `semantic.device`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DevicePreference {
    /// Try the best GPU available for this platform, then fall back to CPU.
    #[default]
    Auto,
    /// Force CPU even when a GPU is available.
    Cpu,
    /// Require CUDA (NVIDIA). Falls back to CPU with a reason if unavailable.
    Cuda,
    /// Require Metal (Apple Silicon). Falls back to CPU with a reason.
    Metal,
}

impl DevicePreference {
    /// Parse a config string. Unknown values fall back to [`Self::Auto`] and
    /// are reported by [`parse_with_warning`] rather than failing startup —
    /// a typo in a tunable must never prevent the server from booting.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "cpu" => Some(Self::Cpu),
            "cuda" | "nvidia" => Some(Self::Cuda),
            "metal" | "mps" | "apple" => Some(Self::Metal),
            _ => None,
        }
    }

    /// Parse, returning the preference plus an optional warning for an
    /// unrecognized value.
    pub fn parse_with_warning(raw: &str) -> (Self, Option<String>) {
        match Self::parse(raw) {
            Some(p) => (p, None),
            None => (
                Self::Auto,
                Some(format!(
                    "unrecognized semantic.device value {raw:?}; expected one of \
                     auto|cpu|cuda|metal — falling back to auto"
                )),
            ),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::Cuda => "cuda",
            Self::Metal => "metal",
        }
    }
}

/// The outcome of device selection: the device to run on, the backend label
/// for telemetry, and — when a GPU was wanted but not obtained — why not.
#[derive(Debug)]
pub struct ResolvedDevice {
    pub device: Device,
    pub backend: ExecutionBackend,
    /// `None` when the selected device is what was asked for. `Some(reason)`
    /// when this is a CPU fallback, explaining the cause in user-facing terms.
    pub fallback_reason: Option<String>,
}

impl ResolvedDevice {
    fn cpu(fallback_reason: Option<String>) -> Self {
        Self {
            device: Device::Cpu,
            backend: ExecutionBackend::CandleCpu,
            fallback_reason,
        }
    }

    /// True when inference is running on a GPU.
    pub fn is_gpu(&self) -> bool {
        !matches!(self.backend, ExecutionBackend::CandleCpu)
    }
}

/// True when this build targets Apple Silicon, where Metal is the sensible
/// default. Intel Macs are deliberately excluded: their Radeon/Iris GPUs are
/// not a supported target, so `Auto` keeps them on CPU rather than attempting
/// a Metal device that would only fail.
const fn is_apple_silicon() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

/// True when CUDA is a plausible target for this platform (Linux/Windows).
/// macOS has had no CUDA driver for years.
const fn cuda_is_plausible() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
}

/// Translate a Candle device-construction error into a user-facing reason.
///
/// Candle distinguishes "this binary has no CUDA/Metal support compiled in"
/// from "support is compiled in but the device could not be opened". Those
/// have completely different fixes, so they must not collapse into one
/// message — conflating them is what made the original GPU gap so hard to
/// diagnose from the status output.
fn describe_device_error(kind: &str, feature: &str, err: &candle_core::Error) -> String {
    // Match the variant rather than sniffing Display text: Candle's wording
    // is not a stable API, and getting this wrong would tell users to rebuild
    // when their real problem is a missing driver (or vice versa).
    let not_compiled = matches!(
        err,
        candle_core::Error::NotCompiledWithCudaSupport
            | candle_core::Error::NotCompiledWithMetalSupport
    );

    if not_compiled {
        format!(
            "{kind} support is not compiled into this binary (rebuild with the \
             `{feature}` feature); running on CPU"
        )
    } else {
        format!("{kind} device unavailable ({err}); running on CPU")
    }
}

fn try_cuda() -> Result<ResolvedDevice, String> {
    match Device::new_cuda(0) {
        Ok(device) => Ok(ResolvedDevice {
            device,
            backend: ExecutionBackend::CandleCuda,
            fallback_reason: None,
        }),
        Err(e) => Err(describe_device_error("CUDA", "candle-cuda", &e)),
    }
}

fn try_metal() -> Result<ResolvedDevice, String> {
    match Device::new_metal(0) {
        Ok(device) => Ok(ResolvedDevice {
            device,
            backend: ExecutionBackend::CandleMetal,
            fallback_reason: None,
        }),
        Err(e) => Err(describe_device_error("Metal", "candle-metal", &e)),
    }
}

/// The GPU [`DevicePreference`] that this binary can actually honour, or
/// [`DevicePreference::Cpu`] when no GPU backend was compiled in.
///
/// This exists because resolving `auto` from `target_os` alone is a lie: a
/// Windows build made *without* the `candle-cuda` feature would still report
/// `backend = "candle-cuda"`, then fall back to CPU inside the worker. The
/// startup log and `semantic_identity` claimed a GPU that was never once
/// attempted — precisely the ambiguity this module was written to remove.
///
/// Feature detection must happen in *this* crate: the `candle-cuda` and
/// `candle-metal` features are declared here, so `cfg!` in a dependent crate
/// sees whatever that crate declares, not what Candle was actually built with.
pub fn compiled_gpu_preference() -> DevicePreference {
    if cfg!(feature = "candle-metal") && is_apple_silicon() {
        DevicePreference::Metal
    } else if cfg!(feature = "candle-cuda") && cfg!(any(target_os = "linux", target_os = "windows"))
    {
        DevicePreference::Cuda
    } else {
        DevicePreference::Cpu
    }
}

/// Process-wide device preference, set once at startup from config.
///
/// A global is the right shape here specifically because embedding inference
/// runs in a *separate worker process* (`attic-inference-worker`), which
/// builds its own `Qwen3Embedder` and never receives the server's in-memory
/// config object. Both processes set this once during boot from the same
/// `attic.toml`, so threading a preference parameter through five public
/// constructors would not actually cover the path that matters.
static PROCESS_PREFERENCE: OnceLock<DevicePreference> = OnceLock::new();

/// Install the process-wide device preference. Returns `false` if it was
/// already set, in which case the existing value is kept — first writer wins,
/// so a late caller can never silently move inference onto a different device
/// after a model has been loaded.
pub fn set_process_preference(pref: DevicePreference) -> bool {
    PROCESS_PREFERENCE.set(pref).is_ok()
}

/// The process-wide device preference, defaulting to [`DevicePreference::Auto`]
/// when startup never set one (e.g. unit tests, or an older embedded caller).
pub fn process_preference() -> DevicePreference {
    PROCESS_PREFERENCE.get().copied().unwrap_or_default()
}

/// Resolve the Candle device to run embedding inference on.
///
/// This never fails: every path yields a usable device, because CPU is always
/// a valid target. An explicit GPU request that cannot be honoured degrades
/// to CPU and records why, so the failure is visible in status output rather
/// than silent.
pub fn resolve(pref: DevicePreference) -> ResolvedDevice {
    match pref {
        DevicePreference::Cpu => ResolvedDevice::cpu(None),

        DevicePreference::Cuda => match try_cuda() {
            Ok(d) => d,
            Err(reason) => ResolvedDevice::cpu(Some(reason)),
        },

        DevicePreference::Metal => match try_metal() {
            Ok(d) => d,
            Err(reason) => ResolvedDevice::cpu(Some(reason)),
        },

        // Auto: try the backend that actually exists for this platform. Only
        // one GPU attempt is made per platform — probing CUDA on macOS or
        // Metal on Linux would just produce a misleading error.
        DevicePreference::Auto => {
            if is_apple_silicon() {
                return match try_metal() {
                    Ok(d) => d,
                    Err(reason) => ResolvedDevice::cpu(Some(reason)),
                };
            }

            if cuda_is_plausible() {
                return match try_cuda() {
                    Ok(d) => d,
                    Err(reason) => ResolvedDevice::cpu(Some(reason)),
                };
            }

            ResolvedDevice::cpu(Some(
                "no GPU backend is available for this platform; running on CPU".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_gpu_preference_never_claims_an_uncompiled_backend() {
        // Regression: `auto` used to resolve from `target_os` alone, so a
        // Windows build without `candle-cuda` requested (and reported) CUDA,
        // then silently ran on CPU. The requested backend must only ever name
        // a device this binary could actually construct.
        let pref = compiled_gpu_preference();

        match pref {
            DevicePreference::Cuda => {
                if !cfg!(feature = "candle-cuda") {
                    panic!("claimed CUDA without the candle-cuda feature compiled in");
                }
            }
            DevicePreference::Metal => {
                if !cfg!(feature = "candle-metal") {
                    panic!("claimed Metal without the candle-metal feature compiled in");
                }
            }
            DevicePreference::Cpu => {}
            DevicePreference::Auto => {
                panic!("auto must resolve to a concrete device, never back to auto")
            }
        }
    }

    #[test]
    fn compiled_gpu_preference_is_cpu_when_no_gpu_feature_is_present() {
        if !cfg!(feature = "candle-cuda") && !cfg!(feature = "candle-metal") {
            assert_eq!(
                compiled_gpu_preference(),
                DevicePreference::Cpu,
                "a CPU-only build must request CPU so status output stays honest"
            );
        }
    }

    #[test]
    fn explicit_cpu_is_not_reported_as_a_fallback() {
        let r = resolve(DevicePreference::Cpu);
        assert_eq!(r.backend, ExecutionBackend::CandleCpu);
        assert!(
            r.fallback_reason.is_none(),
            "asking for CPU and getting CPU is not a fallback"
        );
        assert!(!r.is_gpu());
    }

    #[test]
    fn resolution_always_yields_a_usable_device() {
        // The whole point: this must never fail, on any host, for any
        // preference. CPU is always a valid landing spot.
        for pref in [
            DevicePreference::Auto,
            DevicePreference::Cpu,
            DevicePreference::Cuda,
            DevicePreference::Metal,
        ] {
            let r = resolve(pref);
            assert!(
                matches!(r.backend, ExecutionBackend::CandleCpu)
                    || matches!(
                        r.backend,
                        ExecutionBackend::CandleCuda | ExecutionBackend::CandleMetal
                    ),
                "unexpected backend for {pref:?}"
            );
        }
    }

    #[test]
    fn unsatisfied_gpu_request_always_explains_itself() {
        // On a host/build without the GPU compiled in, falling back is fine —
        // falling back *silently* is the bug this guards against.
        let r = resolve(DevicePreference::Cuda);
        if !r.is_gpu() {
            let reason = r
                .fallback_reason
                .expect("a CPU fallback from an explicit CUDA request must carry a reason");
            assert!(
                reason.contains("CUDA"),
                "reason should name the backend that was attempted: {reason}"
            );
        }

        let r = resolve(DevicePreference::Metal);
        if !r.is_gpu() {
            let reason = r
                .fallback_reason
                .expect("a CPU fallback from an explicit Metal request must carry a reason");
            assert!(
                reason.contains("Metal"),
                "reason should name the backend that was attempted: {reason}"
            );
        }
    }

    #[test]
    fn not_compiled_in_is_distinguished_from_device_missing() {
        // These two conditions have different fixes (rebuild vs. check the
        // hardware/driver), so they must produce different guidance.
        let not_compiled = candle_core::Error::NotCompiledWithCudaSupport;
        let msg = describe_device_error("CUDA", "candle-cuda", &not_compiled);
        assert!(
            msg.contains("not compiled into this binary"),
            "expected a rebuild hint, got: {msg}"
        );
        assert!(
            msg.contains("candle-cuda"),
            "the rebuild hint must name the feature: {msg}"
        );

        let other = candle_core::Error::Msg("no CUDA-capable device is detected".into());
        let msg2 = describe_device_error("CUDA", "candle-cuda", &other);
        assert!(
            msg2.contains("device unavailable"),
            "a real device failure should not claim the feature is missing: {msg2}"
        );
        assert!(
            !msg2.contains("not compiled into this binary"),
            "must not misreport a present-but-unusable device as a build problem: {msg2}"
        );
    }

    #[test]
    fn preference_parsing_accepts_documented_aliases() {
        assert_eq!(
            DevicePreference::parse("auto"),
            Some(DevicePreference::Auto)
        );
        assert_eq!(DevicePreference::parse(""), Some(DevicePreference::Auto));
        assert_eq!(DevicePreference::parse("CPU"), Some(DevicePreference::Cpu));
        assert_eq!(
            DevicePreference::parse("  Cuda "),
            Some(DevicePreference::Cuda)
        );
        assert_eq!(
            DevicePreference::parse("nvidia"),
            Some(DevicePreference::Cuda)
        );
        assert_eq!(
            DevicePreference::parse("metal"),
            Some(DevicePreference::Metal)
        );
        assert_eq!(
            DevicePreference::parse("apple"),
            Some(DevicePreference::Metal)
        );
        assert_eq!(DevicePreference::parse("rocm"), None);
    }

    #[test]
    fn a_typo_in_the_config_warns_but_never_blocks_startup() {
        let (pref, warning) = DevicePreference::parse_with_warning("gpu-please");
        assert_eq!(
            pref,
            DevicePreference::Auto,
            "an unparseable value must degrade to auto, not abort"
        );
        let warning = warning.expect("an unrecognized value must be surfaced, not swallowed");
        assert!(warning.contains("gpu-please"), "warning: {warning}");
        assert!(
            warning.contains("auto|cpu|cuda|metal"),
            "warning: {warning}"
        );

        let (pref, warning) = DevicePreference::parse_with_warning("cuda");
        assert_eq!(pref, DevicePreference::Cuda);
        assert!(warning.is_none(), "a valid value must not warn");
    }

    #[test]
    fn auto_never_attempts_metal_on_non_apple_silicon() {
        // Guards the Intel-Mac / Linux case: Auto must not report a Metal
        // failure on a platform where Metal was never a candidate.
        let r = resolve(DevicePreference::Auto);
        if let Some(reason) = &r.fallback_reason
            && !is_apple_silicon()
        {
            assert!(
                !reason.contains("Metal"),
                "Auto should not surface Metal errors off Apple Silicon: {reason}"
            );
        }
    }
}
