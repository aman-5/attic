//! GPU temperature telemetry and the thermal guard policy.
//!
//! Sustained embedding keeps a laptop GPU at 100% for hours; measured on an
//! RTX A500 the core sits at 84–86 °C. The guard pauses GPU work before the
//! vendor's own throttle point and resumes once the card has cooled, instead
//! of letting the driver throttle (or the OS kill the device) mid-batch.
//!
//! Temperature sources, per platform:
//! - **NVIDIA (Windows + Linux):** `nvidia-smi`, shipped with every driver.
//! - **Linux AMD/Intel:** hwmon sysfs under `/sys/class/drm/card*/device/hwmon`.
//! - **macOS / other adapters:** no public GPU temperature API exists; the
//!   reading is `None` and the guard is inactive (the OS's own thermal
//!   management still applies).
//!
//! Readings are cached for [`SAMPLE_INTERVAL`] so the per-batch check is a
//! mutex read, never a process spawn. A source that fails once is disabled
//! for the process lifetime rather than retried on every batch.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Minimum spacing between real temperature reads.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);

/// Default core temperature (°C) at which new GPU work pauses.
pub const DEFAULT_PAUSE_C: u32 = 90;

/// Default core temperature (°C) below which paused GPU work resumes.
pub const DEFAULT_RESUME_C: u32 = 85;

/// What the embedding loop should do given the current temperature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThermalAction {
    /// Run at the configured batch budget.
    Run,
    /// One degree below the pause point: halve the batch budget to shed heat.
    Throttle,
    /// At or above the pause point (or still cooling): submit nothing.
    Pause,
}

/// Hysteresis thermal guard: pauses at `pause_c`, resumes at `resume_c`.
#[derive(Debug)]
pub struct ThermalGuard {
    pause_c: u32,
    resume_c: u32,
    paused: Mutex<bool>,
}

impl ThermalGuard {
    /// Build a guard. `resume_c` is clamped below `pause_c` so the guard can
    /// never oscillate on a single reading.
    pub fn new(pause_c: u32, resume_c: u32) -> Self {
        let pause_c = pause_c.max(1);
        let resume_c = resume_c.min(pause_c.saturating_sub(1));
        Self {
            pause_c,
            resume_c,
            paused: Mutex::new(false),
        }
    }

    /// Pause threshold in °C.
    pub fn pause_c(&self) -> u32 {
        self.pause_c
    }

    /// Resume threshold in °C.
    pub fn resume_c(&self) -> u32 {
        self.resume_c
    }

    /// Decide from one reading. `None` (no sensor) always runs: an absent
    /// sensor must never stall embedding.
    pub fn decide(&self, temp_c: Option<u32>) -> ThermalAction {
        let Some(t) = temp_c else {
            *self.paused.lock().unwrap_or_else(|e| e.into_inner()) = false;
            return ThermalAction::Run;
        };
        let mut paused = self.paused.lock().unwrap_or_else(|e| e.into_inner());
        if *paused {
            if t <= self.resume_c {
                *paused = false;
            } else {
                return ThermalAction::Pause;
            }
        }
        if t >= self.pause_c {
            *paused = true;
            ThermalAction::Pause
        } else if t + 1 >= self.pause_c {
            ThermalAction::Throttle
        } else {
            ThermalAction::Run
        }
    }
}

struct Cache {
    at: Option<Instant>,
    value: Option<u32>,
    nvidia_smi_ok: bool,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(Cache {
            at: None,
            value: None,
            nvidia_smi_ok: true,
        })
    })
}

/// Hottest GPU core temperature in °C, cached for [`SAMPLE_INTERVAL`].
/// `None` means no supported sensor on this machine.
pub fn gpu_temperature_c() -> Option<u32> {
    let mut c = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(at) = c.at
        && at.elapsed() < SAMPLE_INTERVAL
    {
        return c.value;
    }
    let mut value = None;
    if c.nvidia_smi_ok {
        value = read_nvidia_smi();
        if value.is_none() {
            c.nvidia_smi_ok = false;
            tracing::debug!("nvidia-smi temperature unavailable; disabling that source");
        }
    }
    if value.is_none() {
        value = read_hwmon();
    }
    c.at = Some(Instant::now());
    c.value = value;
    value
}

fn read_nvidia_smi() -> Option<u32> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=temperature.gpu", "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: never flash a console from a background daemon.
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_max_temperature(&String::from_utf8_lossy(&out.stdout))
}

/// Max of one-integer-per-line output (multi-GPU hosts report one line each).
fn parse_max_temperature(text: &str) -> Option<u32> {
    text.lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|t| *t > 0 && *t < 150)
        .max()
}

#[cfg(target_os = "linux")]
fn read_hwmon() -> Option<u32> {
    let mut best: Option<u32> = None;
    for card in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let hwmon = card.path().join("device").join("hwmon");
        let Ok(entries) = std::fs::read_dir(&hwmon) else {
            continue;
        };
        for h in entries.flatten() {
            if let Ok(s) = std::fs::read_to_string(h.path().join("temp1_input"))
                && let Ok(milli) = s.trim().parse::<u32>()
            {
                let c = milli / 1000;
                if c > 0 && c < 150 {
                    best = Some(best.map_or(c, |b| b.max(c)));
                }
            }
        }
    }
    best
}

#[cfg(not(target_os = "linux"))]
fn read_hwmon() -> Option<u32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_pauses_at_threshold_and_resumes_with_hysteresis() {
        let g = ThermalGuard::new(90, 85);
        assert_eq!(g.decide(Some(80)), ThermalAction::Run);
        assert_eq!(g.decide(Some(89)), ThermalAction::Throttle);
        assert_eq!(g.decide(Some(90)), ThermalAction::Pause);
        // Still paused while cooling but above resume.
        assert_eq!(g.decide(Some(88)), ThermalAction::Pause);
        assert_eq!(g.decide(Some(86)), ThermalAction::Pause);
        assert_eq!(g.decide(Some(85)), ThermalAction::Run);
    }

    #[test]
    fn missing_sensor_never_pauses() {
        let g = ThermalGuard::new(90, 85);
        assert_eq!(g.decide(Some(95)), ThermalAction::Pause);
        assert_eq!(g.decide(None), ThermalAction::Run);
    }

    #[test]
    fn resume_is_clamped_below_pause() {
        let g = ThermalGuard::new(80, 95);
        assert_eq!(g.resume_c(), 79);
    }

    #[test]
    fn parses_multi_gpu_output() {
        assert_eq!(parse_max_temperature("71\n84\n"), Some(84));
        assert_eq!(parse_max_temperature("[N/A]\n"), None);
        assert_eq!(parse_max_temperature(""), None);
    }
}
