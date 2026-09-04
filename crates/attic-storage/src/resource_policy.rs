//! Phase 8 — hardware-aware resource tuning.
//!
//! `HardwareSnapshot` (raw machine facts) → `detect_resource_mode`
//! (classification) → `ResourcePolicy` (runtime-tunable baseline) →
//! `EffectiveResourceConfig` (final, clamped values actually handed to the
//! scheduler / SQLite / writer / `ResourceMonitor`).
//!
//! `ResourcePolicy`/`ResourceMode` and `EmbeddingPolicy`
//! (`attic_semantic::embedding_policy`) are two fully independent axes: this
//! module only decides how aggressively Attic *runs* — it never decides
//! *which* embedding model runs.

use attic_core::config::{ResourceModeSetting, ResourceOverrides};

use crate::resource_manager::ResourceConfig;

/// Error capturing the machine facts (or the reason they could not be read).
#[derive(Debug, Clone, thiserror::Error)]
#[error("hardware detection failed: {0}")]
pub struct ResourceDetectionError(pub String);

/// Raw machine facts. Zero policy — `detect_resource_mode` is the only
/// consumer that assigns meaning to these numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareSnapshot {
    /// Total system RAM, in MiB.
    pub total_memory_mib: u64,
    /// Physical CPU core count.
    pub cpu_cores: usize,
}

impl HardwareSnapshot {
    /// Capture real hardware facts via `sysinfo`. Fallible so hardware
    /// detection failure is a real, expressible state — the caller decides
    /// the fallback (see [`resolve_effective_config`]), never this function.
    pub fn capture() -> Result<Self, ResourceDetectionError> {
        use sysinfo::System;
        let mut sys = System::new();
        sys.refresh_memory();
        let total_memory_mib = sys.total_memory() / (1024 * 1024);
        let cpu_cores = sys.physical_core_count().unwrap_or(0);
        if total_memory_mib == 0 || cpu_cores == 0 {
            return Err(ResourceDetectionError(format!(
                "implausible hardware snapshot (total_memory_mib={total_memory_mib}, cpu_cores={cpu_cores})"
            )));
        }
        Ok(Self {
            total_memory_mib,
            cpu_cores,
        })
    }
}

/// Machine classification. Drives ONLY elastic runtime tuning
/// (`ResourcePolicy`); it is never itself persisted (re-detected every
/// launch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceMode {
    /// Conservative baseline for constrained machines.
    Low,
    /// Baseline mirroring today's existing hardcoded defaults.
    Balanced,
    /// Aggressive baseline for high-core/high-RAM machines.
    Performance,
}

impl ResourceMode {
    /// Lowercase tag used in the `status` MCP response.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }
}

/// Classify a [`HardwareSnapshot`] into a [`ResourceMode`].
///
/// Starting thresholds — not benchmark-derived, adjust after real-world
/// testing (see Low-Level Design §2 confidence note).
pub fn detect_resource_mode(s: &HardwareSnapshot) -> ResourceMode {
    if s.total_memory_mib < 8192 || s.cpu_cores <= 2 {
        ResourceMode::Low
    } else if s.total_memory_mib < 16384 || s.cpu_cores <= 4 {
        ResourceMode::Balanced
    } else {
        ResourceMode::Performance
    }
}

/// Where the active [`ResourceMode`] came from (for `status` observability —
/// answers "did my `attic.toml`/env override actually take effect?").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceModeSource {
    /// Detected from `HardwareSnapshot` (the default, `mode = "auto"`).
    Detected,
    /// Forced via `attic.toml`'s `[resources].mode`.
    TomlOverride,
    /// Forced via `ATTIC_RESOURCE_MODE`.
    EnvOverride,
    /// Hardware detection failed; forced to `Low` as a safe fallback.
    DetectionFailed,
}

impl ResourceModeSource {
    /// Lowercase tag used in the `status` MCP response.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detected => "detected",
            Self::TomlOverride => "toml_override",
            Self::EnvOverride => "env_override",
            Self::DetectionFailed => "detection_failed",
        }
    }
}

/// The complete baseline for every resource-controlled value — not just the
/// ones that happen to get a hardware-dependent clamp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourcePolicy {
    /// Incremental scheduler worker thread count.
    pub scheduler_workers: usize,
    /// SQLite `PRAGMA cache_size` (negative = KiB).
    pub sqlite_cache_pages: i64,
    /// SQLite `PRAGMA mmap_size`, in bytes.
    pub sqlite_mmap_bytes: u64,
    /// `ResourceMonitor` total memory budget, in MiB.
    pub memory_budget_mib: u64,
    /// `ResourceMonitor` minimum free memory, in MiB.
    pub min_free_memory_mib: u64,
    /// `ResourceMonitor` foreground admission capacity.
    pub max_foreground_queries: usize,
    /// Embedding batch size (consumed by a future `SemanticProvider`
    /// implementation; not yet consumed by `HashingEmbedder`).
    pub embedding_batch_size: usize,
    /// `WriterQueue` batch size.
    pub writer_batch_size: usize,
    /// `WriterQueue` flush interval, in ms.
    pub writer_flush_interval_ms: u64,
    /// `WriterQueue` bounded channel capacity.
    pub writer_queue_capacity: usize,
    /// [FIX] Now really enforced: throttles the `WriterQueue`'s own `COMMIT`
    /// rate (one `flush_batch` call = one real disk I/O commit/fsync via
    /// SQLite WAL) — see `writer.rs`'s `worker_loop`. Scoped honestly to
    /// what that component can actually see: this is NOT a throttle on all
    /// disk I/O (reads, WAL checkpoints, backups are outside its
    /// visibility), only the writer's commit cadence.
    pub max_io_ops_per_sec: u32,
}

impl ResourcePolicy {
    /// The baseline for a given mode — NOT yet clamped to real hardware.
    /// `Balanced` deliberately mirrors today's exact hardcoded defaults
    /// (`attic_core::resources`, `writer.rs` constants) so typical-hardware
    /// users see zero behavior change during rollout. `Low`/`Performance`
    /// are proposed starting points, not yet benchmark-tuned.
    pub fn baseline_for_mode(mode: ResourceMode) -> Self {
        match mode {
            ResourceMode::Low => Self {
                scheduler_workers: 2,
                sqlite_cache_pages: -16384,
                sqlite_mmap_bytes: 128 * 1024 * 1024,
                memory_budget_mib: 2048,
                // [FIX] 400 previously violated the newly-enforced
                // min_free_memory_mib < memory_budget_mib * 15% constraint
                // (2048 * 15% = 307) — a real, previously-masked
                // inconsistency only caught once validate() actually checked
                // it; silently auto-corrected at runtime by
                // safe_min_free_mib() before this fix, never surfaced. 256
                // sits safely under the 307 ceiling with headroom.
                min_free_memory_mib: 256,
                max_foreground_queries: 32,
                embedding_batch_size: 8,
                writer_batch_size: 128,
                writer_flush_interval_ms: 100,
                writer_queue_capacity: 256,
                max_io_ops_per_sec: 100,
            },
            ResourceMode::Balanced => Self {
                scheduler_workers: 2,
                sqlite_cache_pages: -32768,
                sqlite_mmap_bytes: 512 * 1024 * 1024,
                memory_budget_mib: 4096,
                min_free_memory_mib: 400,
                max_foreground_queries: 64,
                embedding_batch_size: 32,
                writer_batch_size: 256,
                writer_flush_interval_ms: 50,
                writer_queue_capacity: 512,
                max_io_ops_per_sec: 200,
            },
            ResourceMode::Performance => Self {
                scheduler_workers: 8,
                sqlite_cache_pages: -65536,
                sqlite_mmap_bytes: 1024 * 1024 * 1024,
                memory_budget_mib: 8192,
                min_free_memory_mib: 400,
                max_foreground_queries: 128,
                embedding_batch_size: 64,
                writer_batch_size: 512,
                writer_flush_interval_ms: 25,
                writer_queue_capacity: 1024,
                max_io_ops_per_sec: 400,
            },
        }
    }

    /// Layer `attic.toml`/env `[resources]` overrides on top of this
    /// baseline. `Some(x)` replaces the current value; `None` keeps it.
    pub fn apply_overrides(self, overrides: &ResourceOverrides) -> Self {
        Self {
            memory_budget_mib: overrides
                .total_memory_budget_mib
                .unwrap_or(self.memory_budget_mib),
            min_free_memory_mib: overrides
                .min_free_memory_mib
                .unwrap_or(self.min_free_memory_mib),
            max_foreground_queries: overrides
                .max_foreground_queries
                .unwrap_or(self.max_foreground_queries),
            writer_batch_size: overrides
                .writer_batch_size
                .unwrap_or(self.writer_batch_size),
            writer_flush_interval_ms: overrides
                .writer_flush_interval_ms
                .unwrap_or(self.writer_flush_interval_ms),
            writer_queue_capacity: overrides
                .writer_queue_capacity
                .unwrap_or(self.writer_queue_capacity),
            max_io_ops_per_sec: overrides
                .max_io_ops_per_sec
                .unwrap_or(self.max_io_ops_per_sec),
            ..self
        }
    }

    /// Range + cross-field relational validation on every field. Errors on
    /// violation rather than silently clamping — clamping is reserved for
    /// the final, hardware-dependent step (see [`Self::clamp_to_hardware`]).
    pub fn validate(self) -> Result<Self, attic_core::config::ConfigError> {
        use attic_core::config::ConfigError;
        if self.scheduler_workers == 0 {
            return Err(ConfigError::Invalid(
                "scheduler_workers must be >= 1".into(),
            ));
        }
        if self.max_foreground_queries == 0 {
            return Err(ConfigError::Invalid(
                "max_foreground_queries must be >= 1".into(),
            ));
        }
        if self.embedding_batch_size == 0 {
            return Err(ConfigError::Invalid(
                "embedding_batch_size must be >= 1".into(),
            ));
        }
        // [FIX] Now that max_io_ops_per_sec is actually enforced (writer.rs's
        // commit-rate throttle), a 0 value must be rejected explicitly rather
        // than silently reaching a throttle calculation — 0 previously meant
        // nothing (the field was unenforced) so this was never checked.
        if self.max_io_ops_per_sec == 0 {
            return Err(ConfigError::Invalid(
                "max_io_ops_per_sec must be >= 1".into(),
            ));
        }
        if self.writer_batch_size == 0 {
            return Err(ConfigError::Invalid(
                "writer_batch_size must be >= 1".into(),
            ));
        }
        if self.writer_queue_capacity == 0 {
            return Err(ConfigError::Invalid(
                "writer_queue_capacity must be >= 1".into(),
            ));
        }
        if self.writer_batch_size > self.writer_queue_capacity {
            return Err(ConfigError::Invalid(format!(
                "writer_batch_size ({}) must be <= writer_queue_capacity ({})",
                self.writer_batch_size, self.writer_queue_capacity
            )));
        }
        if self.writer_flush_interval_ms == 0 {
            return Err(ConfigError::Invalid(
                "writer_flush_interval_ms must be >= 1".into(),
            ));
        }
        if self.memory_budget_mib == 0 {
            return Err(ConfigError::Invalid(
                "memory_budget_mib must be >= 1".into(),
            ));
        }
        // [FIX — round 19, reverted] An earlier round added a hard-reject
        // here for `min_free_memory_mib` vs `memory_budget_mib`, using the
        // same formula `safe_min_free_mib` already clamps at runtime. That
        // was too strict: it broke a previously-fine single-field override
        // (e.g. lowering only `total_memory_budget_mib`, leaving
        // `min_free_memory_mib` at its mode's normal baseline) by turning it
        // into a hard startup failure. The actual runtime consumer
        // (`safe_min_free_mib`) already auto-adjusts this safely — `validate()`
        // doesn't need to duplicate that as a reject; `clamp_to_hardware()`/
        // `apply_fallback_safety_limits()` now call the SAME real function
        // (not a 4th reimplementation of its formula) as their final step.
        Ok(self)
    }

    /// FINAL step, success path: hardware-dependent safety clamp, applied to
    /// the fully-resolved value so an override can never bypass it.
    pub fn clamp_to_hardware(self, snapshot: &HardwareSnapshot) -> EffectiveResourceConfig {
        let memory_budget_mib = self
            .memory_budget_mib
            .min(snapshot.total_memory_mib * 60 / 100);
        let min_free_memory_mib =
            crate::resource_manager::safe_min_free_mib(memory_budget_mib, self.min_free_memory_mib);
        EffectiveResourceConfig {
            memory_budget_mib,
            min_free_memory_mib,
            scheduler_workers: self.scheduler_workers.min(snapshot.cpu_cores).max(1),
            ..self.into()
        }
    }

    /// FINAL step, hardware-detection-FAILURE path: hardcoded conservative
    /// caps only, since there is no snapshot to scale against.
    ///
    /// [FIX] Previously only clamped `scheduler_workers`; `memory_budget_mib`
    /// stayed at whatever the (possibly explicitly forced, e.g.
    /// `ATTIC_RESOURCE_MODE=performance`) mode's baseline was — up to 8192
    /// MiB — even though no real RAM was ever measured on this path. Capped
    /// at `Low`'s baseline budget, the same conservative default already
    /// used to pick the mode itself on detection failure.
    pub fn apply_fallback_safety_limits(self) -> EffectiveResourceConfig {
        let memory_budget_mib = self
            .memory_budget_mib
            .min(Self::baseline_for_mode(ResourceMode::Low).memory_budget_mib);
        let min_free_memory_mib =
            crate::resource_manager::safe_min_free_mib(memory_budget_mib, self.min_free_memory_mib);
        EffectiveResourceConfig {
            scheduler_workers: self.scheduler_workers.clamp(1, 2),
            memory_budget_mib,
            min_free_memory_mib,
            ..self.into()
        }
    }
}

/// The final, resolved values actually handed to the scheduler / SQLite /
/// writer / `ResourceMonitor`. Same fields as [`ResourcePolicy`]; the
/// distinct type marks "this has been through the hardware clamp."
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectiveResourceConfig {
    /// See [`ResourcePolicy::scheduler_workers`].
    pub scheduler_workers: usize,
    /// See [`ResourcePolicy::sqlite_cache_pages`].
    pub sqlite_cache_pages: i64,
    /// See [`ResourcePolicy::sqlite_mmap_bytes`].
    pub sqlite_mmap_bytes: u64,
    /// See [`ResourcePolicy::memory_budget_mib`].
    pub memory_budget_mib: u64,
    /// See [`ResourcePolicy::min_free_memory_mib`].
    pub min_free_memory_mib: u64,
    /// See [`ResourcePolicy::max_foreground_queries`].
    pub max_foreground_queries: usize,
    /// See [`ResourcePolicy::embedding_batch_size`].
    pub embedding_batch_size: usize,
    /// See [`ResourcePolicy::writer_batch_size`].
    pub writer_batch_size: usize,
    /// See [`ResourcePolicy::writer_flush_interval_ms`].
    pub writer_flush_interval_ms: u64,
    /// See [`ResourcePolicy::writer_queue_capacity`].
    pub writer_queue_capacity: usize,
    /// See [`ResourcePolicy::max_io_ops_per_sec`].
    pub max_io_ops_per_sec: u32,
}

impl From<ResourcePolicy> for EffectiveResourceConfig {
    fn from(p: ResourcePolicy) -> Self {
        Self {
            scheduler_workers: p.scheduler_workers,
            sqlite_cache_pages: p.sqlite_cache_pages,
            sqlite_mmap_bytes: p.sqlite_mmap_bytes,
            memory_budget_mib: p.memory_budget_mib,
            min_free_memory_mib: p.min_free_memory_mib,
            max_foreground_queries: p.max_foreground_queries,
            embedding_batch_size: p.embedding_batch_size,
            writer_batch_size: p.writer_batch_size,
            writer_flush_interval_ms: p.writer_flush_interval_ms,
            writer_queue_capacity: p.writer_queue_capacity,
            max_io_ops_per_sec: p.max_io_ops_per_sec,
        }
    }
}

impl EffectiveResourceConfig {
    /// Project onto the existing [`ResourceConfig`] shape so
    /// `ResourceMonitor::from_config` can consume it without a parallel
    /// admission-control code path. `per_repo_memory_budget_mib` and
    /// `max_background_workers` are outside `ResourcePolicy`'s 11 fields
    /// (per Low-Level Design §1) and keep their existing
    /// `attic_core::resources` defaults here.
    pub fn as_resource_config(&self) -> ResourceConfig {
        ResourceConfig {
            total_memory_budget_mib: Some(self.memory_budget_mib),
            min_free_memory_mib: Some(self.min_free_memory_mib),
            max_foreground_queries: Some(self.max_foreground_queries),
            max_io_ops_per_sec: Some(self.max_io_ops_per_sec as u64),
            writer_queue_capacity: Some(self.writer_queue_capacity),
            writer_batch_size: Some(self.writer_batch_size),
            writer_flush_interval_ms: Some(self.writer_flush_interval_ms),
            ..Default::default()
        }
    }
}

/// Read `ATTIC_RESOURCE_MODE` / the `ATTIC_*` resource env vars as a
/// [`ResourceOverrides`] layer — the sole env-var reader for these names now
/// that the pre-Phase-8 `ResourceConfig::load()` (a second, independent
/// parser for the same names) has been removed as dead code.
pub fn env_resource_overrides() -> ResourceOverrides {
    let mode = match std::env::var("ATTIC_RESOURCE_MODE").ok().as_deref() {
        Some("low") => ResourceModeSetting::Low,
        Some("balanced") => ResourceModeSetting::Balanced,
        Some("performance") => ResourceModeSetting::Performance,
        _ => ResourceModeSetting::Auto,
    };
    ResourceOverrides {
        mode,
        total_memory_budget_mib: std::env::var("ATTIC_TOTAL_MEMORY_BUDGET_MIB")
            .ok()
            .and_then(|v| v.parse().ok()),
        min_free_memory_mib: std::env::var("ATTIC_MIN_FREE_MEMORY_MIB")
            .ok()
            .and_then(|v| v.parse().ok()),
        max_foreground_queries: std::env::var("ATTIC_MAX_FOREGROUND_QUERIES")
            .ok()
            .and_then(|v| v.parse().ok()),
        writer_batch_size: std::env::var("ATTIC_WRITER_BATCH_SIZE")
            .ok()
            .and_then(|v| v.parse().ok()),
        writer_flush_interval_ms: std::env::var("ATTIC_WRITER_FLUSH_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse().ok()),
        writer_queue_capacity: std::env::var("ATTIC_WRITER_QUEUE_CAPACITY")
            .ok()
            .and_then(|v| v.parse().ok()),
        max_io_ops_per_sec: std::env::var("ATTIC_MAX_IO_OPS_PER_SEC")
            .ok()
            .and_then(|v| v.parse().ok()),
    }
}

/// Full resolution result: the clamped effective config, plus which
/// [`ResourceMode`] was selected and where it came from (for `status`).
#[derive(Debug, Clone, Copy)]
pub struct ResourceResolution {
    /// The final, hardware-clamped values.
    pub effective: EffectiveResourceConfig,
    /// The selected mode.
    pub mode: ResourceMode,
    /// Where `mode` came from.
    pub mode_source: ResourceModeSource,
}

/// Resolve the fully-effective resource configuration.
///
/// Precedence (never bypassable — clamping is always the LAST step):
/// `ATTIC_RESOURCE_MODE` (env) > `attic.toml [resources].mode` > detected
/// mode (default `auto`) → baseline → apply `attic.toml` overrides → apply
/// env overrides → `validate()` → `clamp_to_hardware()` /
/// `apply_fallback_safety_limits()`.
pub fn resolve_effective_config(
    toml_overrides: &ResourceOverrides,
    env_overrides: &ResourceOverrides,
    snapshot: &Result<HardwareSnapshot, ResourceDetectionError>,
) -> Result<ResourceResolution, attic_core::config::ConfigError> {
    let (mode, mode_source) = match (env_overrides.mode, toml_overrides.mode, snapshot) {
        (m, _, _) if !matches!(m, ResourceModeSetting::Auto) => {
            (setting_to_mode(m), ResourceModeSource::EnvOverride)
        }
        (_, m, _) if !matches!(m, ResourceModeSetting::Auto) => {
            (setting_to_mode(m), ResourceModeSource::TomlOverride)
        }
        (_, _, Ok(snap)) => (detect_resource_mode(snap), ResourceModeSource::Detected),
        (_, _, Err(_)) => (ResourceMode::Low, ResourceModeSource::DetectionFailed),
    };

    let merged = toml_overrides.clone().layer(env_overrides);
    let policy = ResourcePolicy::baseline_for_mode(mode)
        .apply_overrides(&merged)
        .validate()?;

    let effective = match snapshot {
        Ok(snap) => policy.clamp_to_hardware(snap),
        Err(_) => policy.apply_fallback_safety_limits(),
    };

    Ok(ResourceResolution {
        effective,
        mode,
        mode_source,
    })
}

fn setting_to_mode(setting: ResourceModeSetting) -> ResourceMode {
    match setting {
        ResourceModeSetting::Low => ResourceMode::Low,
        ResourceModeSetting::Balanced => ResourceMode::Balanced,
        ResourceModeSetting::Performance => ResourceMode::Performance,
        ResourceModeSetting::Auto => ResourceMode::Balanced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(mem_mib: u64, cores: usize) -> HardwareSnapshot {
        HardwareSnapshot {
            total_memory_mib: mem_mib,
            cpu_cores: cores,
        }
    }

    #[test]
    fn detects_low_on_constrained_hardware() {
        assert_eq!(detect_resource_mode(&snap(4096, 2)), ResourceMode::Low);
    }

    #[test]
    fn detects_balanced_on_mid_hardware() {
        assert_eq!(
            detect_resource_mode(&snap(12000, 4)),
            ResourceMode::Balanced
        );
    }

    #[test]
    fn detects_performance_on_big_hardware() {
        assert_eq!(
            detect_resource_mode(&snap(32768, 16)),
            ResourceMode::Performance
        );
    }

    #[test]
    fn baseline_balanced_mirrors_todays_hardcoded_defaults() {
        let p = ResourcePolicy::baseline_for_mode(ResourceMode::Balanced);
        assert_eq!(p.scheduler_workers, 2);
        assert_eq!(p.sqlite_cache_pages, -32768);
        assert_eq!(p.sqlite_mmap_bytes, 512 * 1024 * 1024);
        assert_eq!(p.memory_budget_mib, 4096);
        assert_eq!(p.writer_batch_size, 256);
        assert_eq!(p.writer_flush_interval_ms, 50);
        assert_eq!(p.writer_queue_capacity, 512);
    }

    #[test]
    fn apply_overrides_replaces_only_set_fields() {
        let p = ResourcePolicy::baseline_for_mode(ResourceMode::Balanced);
        let overrides = ResourceOverrides {
            total_memory_budget_mib: Some(1234),
            ..Default::default()
        };
        let applied = p.apply_overrides(&overrides);
        assert_eq!(applied.memory_budget_mib, 1234);
        assert_eq!(applied.writer_batch_size, p.writer_batch_size);
    }

    #[test]
    fn validate_rejects_batch_size_over_queue_capacity() {
        let p = ResourcePolicy {
            writer_batch_size: 1000,
            writer_queue_capacity: 500,
            ..ResourcePolicy::baseline_for_mode(ResourceMode::Balanced)
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_max_io_ops_per_sec() {
        let p = ResourcePolicy {
            max_io_ops_per_sec: 0,
            ..ResourcePolicy::baseline_for_mode(ResourceMode::Balanced)
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn validate_never_rejects_min_free_vs_memory_budget() {
        // [FIX — round 19] validate() must NOT hard-reject this relationship
        // — a user who only overrides total_memory_budget_mib downward,
        // leaving min_free_memory_mib at its mode's normal default, must
        // still be able to start. 400 against 2048 (2048*15%=307) is exactly
        // that real-world case: validate() accepts it; the clamp functions
        // (see below) are what actually keep it safe.
        let p = ResourcePolicy {
            memory_budget_mib: 2048,
            min_free_memory_mib: 400,
            ..ResourcePolicy::baseline_for_mode(ResourceMode::Balanced)
        };
        assert!(p.validate().is_ok());
    }

    #[test]
    fn clamp_to_hardware_fixes_up_an_unsafe_min_free() {
        let p = ResourcePolicy {
            memory_budget_mib: 2048,
            min_free_memory_mib: 400, // unsafe against 2048 (ceiling is 307)
            ..ResourcePolicy::baseline_for_mode(ResourceMode::Balanced)
        };
        let effective = p.clamp_to_hardware(&snap(32768, 16)); // ample real RAM, budget stays 2048
        assert!(
            effective.min_free_memory_mib < 307,
            "clamp_to_hardware must apply the same safe_min_free_mib formula the runtime relies on"
        );
    }

    #[test]
    fn apply_fallback_safety_limits_fixes_up_an_unsafe_min_free() {
        let p = ResourcePolicy {
            memory_budget_mib: 2048,
            min_free_memory_mib: 400,
            ..ResourcePolicy::baseline_for_mode(ResourceMode::Balanced)
        };
        let effective = p.apply_fallback_safety_limits();
        assert!(effective.min_free_memory_mib < 307);
    }

    #[test]
    fn apply_fallback_safety_limits_caps_memory_budget_even_when_mode_is_forced() {
        // [FIX] Regression: previously only scheduler_workers was clamped on
        // the detection-failure path — an ATTIC_RESOURCE_MODE=performance
        // override left an 8192 MiB budget in effect on a machine whose real
        // RAM was never measured.
        let p = ResourcePolicy::baseline_for_mode(ResourceMode::Performance); // wants 8192 MiB
        let effective = p.apply_fallback_safety_limits();
        assert!(
            effective.memory_budget_mib
                <= ResourcePolicy::baseline_for_mode(ResourceMode::Low).memory_budget_mib,
            "fallback path must cap memory_budget_mib conservatively, got {}",
            effective.memory_budget_mib
        );
    }

    #[test]
    fn every_baseline_mode_passes_its_own_validation() {
        for mode in [
            ResourceMode::Low,
            ResourceMode::Balanced,
            ResourceMode::Performance,
        ] {
            let p = ResourcePolicy::baseline_for_mode(mode);
            assert!(
                p.validate().is_ok(),
                "{mode:?} baseline must be internally consistent: {p:?}"
            );
        }
    }

    #[test]
    fn clamp_never_exceeds_60_percent_of_real_ram() {
        let p = ResourcePolicy::baseline_for_mode(ResourceMode::Performance); // wants 8192 MiB
        let small_machine = snap(4096, 16); // 60% of 4096 = 2457
        let effective = p.clamp_to_hardware(&small_machine);
        assert!(effective.memory_budget_mib <= 4096 * 60 / 100);
    }

    #[test]
    fn clamp_never_exceeds_real_core_count() {
        let p = ResourcePolicy::baseline_for_mode(ResourceMode::Performance); // wants 8 workers
        let effective = p.clamp_to_hardware(&snap(32768, 4));
        assert_eq!(effective.scheduler_workers, 4);
    }

    #[test]
    fn fallback_safety_limits_cap_workers_at_two() {
        let p = ResourcePolicy::baseline_for_mode(ResourceMode::Performance);
        let effective = p.apply_fallback_safety_limits();
        assert_eq!(effective.scheduler_workers, 2);
    }

    #[test]
    fn resolve_prefers_env_mode_over_toml_and_detected() {
        let toml = ResourceOverrides {
            mode: ResourceModeSetting::Low,
            ..Default::default()
        };
        let env = ResourceOverrides {
            mode: ResourceModeSetting::Performance,
            ..Default::default()
        };
        let snapshot = Ok(snap(4096, 2)); // would detect Low
        let resolution = resolve_effective_config(&toml, &env, &snapshot).unwrap();
        assert_eq!(resolution.mode, ResourceMode::Performance);
        assert_eq!(resolution.mode_source, ResourceModeSource::EnvOverride);
    }

    #[test]
    fn resolve_falls_back_to_low_on_detection_failure() {
        let snapshot = Err(ResourceDetectionError("boom".into()));
        let resolution = resolve_effective_config(
            &ResourceOverrides::default(),
            &ResourceOverrides::default(),
            &snapshot,
        )
        .unwrap();
        assert_eq!(resolution.mode, ResourceMode::Low);
        assert_eq!(resolution.mode_source, ResourceModeSource::DetectionFailed);
        assert_eq!(resolution.effective.scheduler_workers, 2);
    }

    #[test]
    fn resolve_uses_detected_mode_when_no_overrides() {
        let snapshot = Ok(snap(32768, 16));
        let resolution = resolve_effective_config(
            &ResourceOverrides::default(),
            &ResourceOverrides::default(),
            &snapshot,
        )
        .unwrap();
        assert_eq!(resolution.mode, ResourceMode::Performance);
        assert_eq!(resolution.mode_source, ResourceModeSource::Detected);
    }

    #[test]
    fn resolve_clamp_is_never_bypassed_by_override() {
        // A user override asking for way more memory than the machine has
        // must still be clamped at the end.
        let toml = ResourceOverrides {
            total_memory_budget_mib: Some(999_999),
            ..Default::default()
        };
        let snapshot = Ok(snap(4096, 4));
        let resolution =
            resolve_effective_config(&toml, &ResourceOverrides::default(), &snapshot).unwrap();
        assert!(resolution.effective.memory_budget_mib <= 4096 * 60 / 100);
    }

    #[test]
    fn hardware_snapshot_capture_returns_plausible_values() {
        let snap = HardwareSnapshot::capture().expect("capture should succeed on a real machine");
        assert!(snap.total_memory_mib > 0);
        assert!(snap.cpu_cores > 0);
    }
}
