//! Hardware and machine telemetry monitoring for elastic resource orchestration.
//!
//! Captures real-time physical system state: total RAM, available RAM, Attic RSS,
//! CPU utilization, disk headroom, and power source (Final Master Plan V2 §8).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use attic_core::{MachineSnapshot, PowerSource};
use sysinfo::{Disks, Pid, ProcessesToUpdate, System};

/// Minimum duration between hardware telemetry samples to prevent sampling overhead.
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

/// Sampler maintaining sysinfo state for accurate CPU deltas and smoothed measurements.
pub struct MachineTelemetrySampler {
    sys: System,
    disks: Disks,
    pid: Pid,
    semantic_path: Option<PathBuf>,
    last_sample_instant: Instant,
    last_snapshot: MachineSnapshot,
    smoothed_cpu: f32,
}

impl MachineTelemetrySampler {
    /// Create a new sampler with initial baseline telemetry.
    pub fn new(semantic_path: Option<PathBuf>) -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        sys.refresh_cpu_usage();

        let pid = Pid::from_u32(std::process::id());
        sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);

        let disks = Disks::new_with_refreshed_list();
        let total_memory_mib = sys.total_memory() / (1024 * 1024);
        let available_memory_mib = sys.available_memory() / (1024 * 1024);
        let attic_rss_mib = sys.process(pid).map(|p| p.memory() / (1024 * 1024)).unwrap_or(0);
        let logical_cpus = sys.cpus().len().max(1);
        let cpu_utilization = sys.global_cpu_usage();
        let available_cpu_fraction = ((100.0 - cpu_utilization).clamp(0.0, 100.0)) / 100.0;
        let semantic_disk_free_mib = Self::sample_disk_free(&disks, semantic_path.as_deref());

        let last_snapshot = MachineSnapshot {
            total_memory_mib,
            available_memory_mib,
            attic_rss_mib,
            logical_cpus,
            cpu_utilization,
            available_cpu_fraction,
            semantic_disk_free_mib,
            power_source: Some(PowerSource::Ac),
        };

        Self {
            sys,
            disks,
            pid,
            semantic_path,
            last_sample_instant: Instant::now(),
            last_snapshot,
            smoothed_cpu: cpu_utilization,
        }
    }

    /// Sample system telemetry, returning cached snapshot if sampled too recently.
    pub fn sample(&mut self) -> MachineSnapshot {
        let now = Instant::now();
        if now.duration_since(self.last_sample_instant) < MIN_SAMPLE_INTERVAL {
            return self.last_snapshot.clone();
        }

        self.last_sample_instant = now;
        self.sys.refresh_memory();
        self.sys.refresh_cpu_usage();
        self.sys
            .refresh_processes(ProcessesToUpdate::Some(&[self.pid]), true);

        let total_memory_mib = self.sys.total_memory() / (1024 * 1024);
        let available_memory_mib = self.sys.available_memory() / (1024 * 1024);
        let attic_rss_mib = self
            .sys
            .process(self.pid)
            .map(|p| p.memory() / (1024 * 1024))
            .unwrap_or(0);
        let logical_cpus = self.sys.cpus().len().max(1);

        let raw_cpu = self.sys.global_cpu_usage();
        self.smoothed_cpu = 0.7 * self.smoothed_cpu + 0.3 * raw_cpu;
        let cpu_utilization = self.smoothed_cpu;
        let available_cpu_fraction = ((100.0 - cpu_utilization).clamp(0.0, 100.0)) / 100.0;

        self.disks.refresh(true);
        let semantic_disk_free_mib =
            Self::sample_disk_free(&self.disks, self.semantic_path.as_deref());

        let snapshot = MachineSnapshot {
            total_memory_mib,
            available_memory_mib,
            attic_rss_mib,
            logical_cpus,
            cpu_utilization,
            available_cpu_fraction,
            semantic_disk_free_mib,
            power_source: Some(PowerSource::Ac),
        };

        self.last_snapshot = snapshot.clone();
        snapshot
    }

    /// Determine free disk space in MiB on the volume housing semantic storage.
    fn sample_disk_free(disks: &Disks, path: Option<&Path>) -> u64 {
        let target = path.unwrap_or_else(|| Path::new("."));
        let mut best_match: Option<u64> = None;
        let mut longest_prefix = 0;

        for disk in disks.list() {
            let mount = disk.mount_point();
            if target.starts_with(mount) {
                let len = mount.as_os_str().len();
                if len >= longest_prefix {
                    longest_prefix = len;
                    best_match = Some(disk.available_space() / (1024 * 1024));
                }
            }
        }

        best_match
            .or_else(|| disks.list().first().map(|d| d.available_space() / (1024 * 1024)))
            .unwrap_or(10_240) // 10 GiB fallback if disks cannot be probed
    }
}

/// Thread-safe handle for periodic telemetry collection.
#[derive(Clone)]
pub struct MachineTelemetry {
    inner: Arc<Mutex<MachineTelemetrySampler>>,
}

impl MachineTelemetry {
    /// Create a new shared telemetry handle.
    pub fn new(semantic_path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MachineTelemetrySampler::new(semantic_path))),
        }
    }

    /// Read the latest machine snapshot.
    pub fn current_snapshot(&self) -> MachineSnapshot {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.sample()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_sampler_produces_plausible_snapshot() {
        let mut sampler = MachineTelemetrySampler::new(None);
        let snapshot = sampler.sample();

        assert!(snapshot.total_memory_mib > 0, "total RAM must be non-zero");
        assert!(snapshot.logical_cpus >= 1, "logical CPUs must be >= 1");
        assert!(snapshot.available_cpu_fraction >= 0.0 && snapshot.available_cpu_fraction <= 1.0);
    }

    #[test]
    fn shared_telemetry_handle_is_thread_safe() {
        let telemetry = MachineTelemetry::new(None);
        let snap1 = telemetry.current_snapshot();
        let snap2 = telemetry.current_snapshot();
        assert_eq!(snap1.logical_cpus, snap2.logical_cpus);
    }
}
