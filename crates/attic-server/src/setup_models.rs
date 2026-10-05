//! `attic-server setup-models`: download the embedding models at install
//! time, so the first MCP session starts on the right device immediately
//! instead of silently running on CPU while a 1.2 GB model downloads.
//!
//! Uses the server's own paths, config and GPU eligibility, and the same
//! verified downloaders (pinned revision, SHA-256 manifest, atomic staging).
//! Already-present assets are skipped without touching the network. Once the
//! downloads finish, duplicate files the models never read (the ONNX download
//! cache, Windows blob copies) are removed (`attic_semantic::model_cache`).
//!
//! Exit codes: 0 ready (or semantic disabled), 1 usage error, 2 a download
//! failed (the server still downloads on first use).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Asset {
    /// ONNX fp16 export for the DirectML GPU backend.
    Onnx,
    /// Qwen3 safetensors for the Candle backend (CPU, or CUDA/Metal).
    Safetensors,
}

#[derive(Debug, Default)]
struct Opts {
    cpu_only: bool,
    gpu_only: bool,
    quiet: bool,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts::default();
    for a in args {
        match a.as_str() {
            "--cpu-only" => o.cpu_only = true,
            "--gpu-only" => o.gpu_only = true,
            "--quiet" | "-q" => o.quiet = true,
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if o.cpu_only && o.gpu_only {
        return Err("--cpu-only and --gpu-only are mutually exclusive".into());
    }
    Ok(o)
}

/// Which assets to fetch, in order: the GPU model first so an eligible
/// machine is GPU-ready as early as possible; safetensors are the CPU
/// fallback target (and the only model elsewhere).
pub(crate) fn plan(gpu_eligible: bool, cpu_only: bool, gpu_only: bool) -> Vec<Asset> {
    match (gpu_eligible, cpu_only, gpu_only) {
        (_, true, _) => vec![Asset::Safetensors],
        (true, _, true) => vec![Asset::Onnx],
        (false, _, true) => vec![],
        (true, _, _) => vec![Asset::Onnx, Asset::Safetensors],
        (false, _, _) => vec![Asset::Safetensors],
    }
}

/// True when this build and machine would run embeddings on DirectML and
/// Attic manages the ONNX export itself (no explicit directory override).
fn gpu_eligible(cfg: &attic_core::AtticConfig) -> bool {
    #[cfg(all(windows, target_env = "msvc"))]
    {
        if cfg.semantic.onnx_model_dir.is_some()
            || std::env::var_os("ATTIC_ONNX_MODEL_DIR").is_some()
        {
            return false;
        }
        let adapter = attic_storage::gpu_telemetry::query_adapter_info();
        crate::gpu_gate(&cfg.semantic, adapter.as_ref()).is_ok()
    }
    #[cfg(not(all(windows, target_env = "msvc")))]
    {
        let _ = cfg;
        false
    }
}

fn safetensors_present(models: &Path) -> bool {
    let mgr = attic_semantic::ModelAssetManager::new(
        models,
        attic_semantic::ModelManifest::qwen3_default(),
    );
    matches!(
        mgr.check_status(),
        attic_semantic::ModelAssetStatus::Active { .. }
    )
}

fn dir_size(p: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// Bytes in the hf-hub download cache under `models` (`models--*` trees).
/// Materialised directories are excluded: they hard-link the cache blobs and
/// would otherwise be counted twice.
fn download_bytes(models: &Path) -> u64 {
    let Ok(rd) = std::fs::read_dir(models) else {
        return 0;
    };
    rd.flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("models--"))
        .map(|e| dir_size(&e.path()))
        .sum()
}

/// Print the bytes downloaded under `models` once a second until `done`.
fn progress(
    models: PathBuf,
    label: &'static str,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    use std::io::Write;
    std::thread::spawn(move || {
        let start = download_bytes(&models);
        let t0 = std::time::Instant::now();
        while !done.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let got = download_bytes(&models).saturating_sub(start) as f64 / 1_048_576.0;
            let rate = got / t0.elapsed().as_secs_f64().max(0.001);
            print!("\r  {label}: {got:>7.1} MB downloaded ({rate:.1} MB/s)   ");
            let _ = std::io::stdout().flush();
        }
        println!();
    })
}

fn download(asset: Asset, models: &Path) -> Result<(), String> {
    match asset {
        Asset::Onnx => attic_semantic::onnx_assets::ensure_onnx_assets(models, None)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        Asset::Safetensors => attic_semantic::Qwen3Embedder::download_assets(models)
            .map_err(|e| e.to_string())
            .and_then(|_| {
                attic_semantic::ModelAssetManager::new(
                    models,
                    attic_semantic::ModelManifest::qwen3_default(),
                )
                .verify_active_snapshot()
                .map(|_| ())
                .map_err(|e| format!("verification failed: {e}"))
            }),
    }
}

fn fetch(asset: Asset, models: &Path, quiet: bool) -> Result<&'static str, String> {
    let label = match asset {
        Asset::Onnx => "GPU model (ONNX fp16, DirectML)",
        Asset::Safetensors => "CPU model (Qwen3 safetensors)",
    };
    let present = match asset {
        Asset::Onnx => attic_semantic::onnx_assets::assets_present(
            &attic_semantic::onnx_assets::onnx_dir(models),
        ),
        Asset::Safetensors => safetensors_present(models),
    };
    if present {
        return Ok("already installed");
    }
    if !quiet {
        println!("Downloading {label} into {} ...", models.display());
    }
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reporter = (!quiet).then(|| progress(models.to_path_buf(), label, done.clone()));
    let result = download(asset, models);
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(r) = reporter {
        let _ = r.join();
    }
    result.map(|()| "installed and verified")
}

fn load_config(db_path: &Path) -> Result<attic_core::AtticConfig, String> {
    match std::fs::read_to_string(attic_core::sibling(db_path, "attic.toml")) {
        Ok(s) => {
            attic_core::AtticConfig::parse_str(&s).map_err(|e| format!("invalid attic.toml: {e}"))
        }
        Err(_) => Ok(attic_core::AtticConfig::default()),
    }
}

/// Entry point; `args` excludes the program name and the subcommand.
pub(crate) fn run(args: &[String]) -> i32 {
    let opts = match parse(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!(
                "attic setup-models: {e}\nusage: attic-server setup-models [--cpu-only|--gpu-only] [--quiet]"
            );
            return 1;
        }
    };
    let db_path = match attic_core::AtticPaths::resolve() {
        Ok(p) => p.db_path().clone(),
        Err(e) => {
            eprintln!("attic setup-models: {e}");
            return 1;
        }
    };
    let cfg = match load_config(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("attic setup-models: {e}");
            return 1;
        }
    };
    if !cfg.semantic.enabled {
        println!("Semantic search is disabled in attic.toml; no models needed.");
        return 0;
    }
    let models = std::env::var("ATTIC_MODEL_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| attic_core::sibling(&db_path, "models"));
    if let Err(e) = std::fs::create_dir_all(&models) {
        eprintln!(
            "attic setup-models: cannot create {}: {e}",
            models.display()
        );
        return 2;
    }

    let eligible = gpu_eligible(&cfg);
    if opts.gpu_only && !eligible {
        eprintln!(
            "attic setup-models: --gpu-only, but this machine/build is not eligible for the \
             DirectML GPU backend (see `status` -> semantic_identity.gpu); nothing downloaded"
        );
        return 2;
    }
    let steps = plan(eligible, opts.cpu_only, opts.gpu_only);
    if !opts.quiet {
        println!(
            "Attic model setup ({}): {} step(s)",
            if eligible { "GPU eligible" } else { "CPU" },
            steps.len()
        );
    }
    let mut failed = false;
    for step in steps {
        match fetch(step, &models, opts.quiet) {
            Ok(msg) if !opts.quiet => println!("  {step:?}: {msg}"),
            Ok(_) => {}
            Err(e) => {
                failed = true;
                eprintln!("  {step:?}: download failed: {e}");
            }
        }
    }
    let cleanup = attic_semantic::model_cache::cleanup_model_cache(&models);
    if !opts.quiet {
        for action in &cleanup.actions {
            println!("  cleanup: {action}");
        }
        if cleanup.bytes_freed > 0 {
            println!(
                "  cleanup: {:.1} GB of duplicate model files removed",
                cleanup.bytes_freed as f64 / 1_073_741_824.0
            );
        }
    }
    if failed {
        eprintln!(
            "Some models could not be downloaded. Attic still works (text search) and retries \
             the download automatically when it starts."
        );
        return 2;
    }
    if !opts.quiet {
        println!("Models ready.");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_orders_gpu_first_and_respects_flags() {
        assert_eq!(
            plan(true, false, false),
            vec![Asset::Onnx, Asset::Safetensors]
        );
        assert_eq!(plan(false, false, false), vec![Asset::Safetensors]);
        assert_eq!(plan(true, true, false), vec![Asset::Safetensors]);
        assert_eq!(plan(true, false, true), vec![Asset::Onnx]);
        assert!(plan(false, false, true).is_empty());
    }

    #[test]
    fn options_parse_and_reject_unknown() {
        assert!(parse(&["--cpu-only".into()]).unwrap().cpu_only);
        assert!(parse(&["-q".into()]).unwrap().quiet);
        assert!(parse(&["--bogus".into()]).is_err());
        assert!(parse(&["--cpu-only".into(), "--gpu-only".into()]).is_err());
    }

    #[test]
    fn present_onnx_is_skipped_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = attic_semantic::onnx_assets::onnx_dir(tmp.path());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(attic_semantic::onnx_assets::MODEL_FILE), "g").unwrap();
        std::fs::write(dir.join(attic_semantic::onnx_assets::TOKENIZER_FILE), "t").unwrap();
        assert_eq!(
            fetch(Asset::Onnx, tmp.path(), true),
            Ok("already installed")
        );
    }
}
