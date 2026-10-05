//! Removes model files Attic never reads again.
//!
//! Two kinds of waste build up under the model cache (`~/.attic/models`):
//!
//! 1. **The ONNX download cache.** `onnx_assets::ensure_onnx_assets` downloads
//!    the GPU export into the Hugging Face cache layout
//!    (`models--onnx-community--Qwen3-Embedding-0.6B-ONNX`) and then builds the
//!    self-contained `onnx-fp16/` directory the GPU provider actually opens.
//!    The download cache is never read again (~1.15 GB).
//! 2. **Duplicate blobs (Windows).** Without symlink privileges the
//!    Hugging Face client stores every file twice: once under `blobs/` and
//!    once as a separate copy under `snapshots/<rev>/`. Attic reads only the
//!    snapshot copy, so the blob is a second full copy (~1.15 GB for the CPU
//!    model).
//!
//! Cleanup is conservative: the download cache is removed only once
//! `onnx-fp16/` is complete, and a blob is replaced by a hard link to its
//! snapshot copy only when both are byte-identical, so the cache layout stays
//! valid and nothing is ever downloaded again. Every step is best effort: a
//! failure (file in use, no hard-link support) leaves the files as they were
//! and is retried on the next run.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::onnx_assets;

/// What one cleanup pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    /// Bytes no longer stored on disk.
    pub bytes_freed: u64,
    /// One line per action taken, for logs and `setup-models` output.
    pub actions: Vec<String>,
}

/// Hugging Face cache folder of the ONNX export.
pub fn onnx_download_cache_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join(format!(
        "models--{}--{}",
        onnx_assets::HF_ONNX_OWNER,
        onnx_assets::HF_ONNX_REPO
    ))
}

/// Run every cleanup step over `cache_dir` (the models directory).
pub fn cleanup_model_cache(cache_dir: &Path) -> CleanupReport {
    let mut report = CleanupReport::default();
    prune_onnx_download_cache(cache_dir, &mut report);
    if let Ok(entries) = fs::read_dir(cache_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("models--") && entry.path().is_dir() {
                dedup_snapshot_blobs(&entry.path(), &mut report);
            }
        }
    }
    report
}

/// Delete the ONNX download cache once `onnx-fp16/` holds a complete copy.
///
/// "Complete" means the graph and tokenizer are present (`assets_present`)
/// and, when the cached export carries an external-data file, `onnx-fp16/`
/// has one of exactly the same size. `onnx-fp16/` files are hard links or
/// full copies, so removing the cache never removes data the provider uses.
pub fn prune_onnx_download_cache(cache_dir: &Path, report: &mut CleanupReport) {
    let repo_dir = onnx_download_cache_dir(cache_dir);
    if !repo_dir.is_dir() {
        return;
    }
    let target = onnx_assets::onnx_dir(cache_dir);
    if !onnx_assets::assets_present(&target) {
        return;
    }
    for cached_data in find_files_named(&repo_dir, onnx_assets::MODEL_DATA_FILE) {
        let cached_len = fs::metadata(&cached_data).map(|m| m.len()).ok();
        let target_len = fs::metadata(target.join(onnx_assets::MODEL_DATA_FILE))
            .map(|m| m.len())
            .ok();
        if cached_len.is_none() || cached_len != target_len {
            tracing::debug!(
                "ONNX download cache kept: onnx-fp16 has no matching external-data file"
            );
            return;
        }
    }
    let freed = unique_bytes(&repo_dir);
    match fs::remove_dir_all(&repo_dir) {
        Ok(()) => {
            let locks = cache_dir.join(".locks").join(
                repo_dir
                    .file_name()
                    .map(|n| n.to_os_string())
                    .unwrap_or_default(),
            );
            let _ = fs::remove_dir_all(locks);
            report.bytes_freed += freed;
            report.actions.push(format!(
                "removed the ONNX download cache {} ({} MB); the GPU model in {} is kept",
                repo_dir.display(),
                freed / 1_048_576,
                target.display()
            ));
        }
        Err(e) => {
            tracing::debug!("ONNX download cache not removed yet ({e}); retried on the next run")
        }
    }
}

/// Replace each `blobs/` file that is a separate byte-identical copy of a
/// `snapshots/` file with a hard link to that snapshot file.
///
/// Snapshot entries that are symlinks (the normal Linux/macOS layout) already
/// share storage and are skipped. Pairs are matched by size, then confirmed
/// by SHA-256 of both files; a marker file records completed pairs so later
/// runs do not re-hash gigabytes on every start.
pub fn dedup_snapshot_blobs(repo_dir: &Path, report: &mut CleanupReport) {
    let blobs_dir = repo_dir.join("blobs");
    let snapshots_dir = repo_dir.join("snapshots");
    if !blobs_dir.is_dir() || !snapshots_dir.is_dir() {
        return;
    }
    let marker = repo_dir.join(".attic-deduplicated");
    let done: Vec<String> = fs::read_to_string(&marker)
        .map(|s| s.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let mut newly_done = Vec::new();

    let blobs: Vec<(PathBuf, u64)> = fs::read_dir(&blobs_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let md = fs::symlink_metadata(e.path()).ok()?;
            md.is_file().then(|| (e.path(), md.len()))
        })
        .collect();

    for snap in walk_files(&snapshots_dir) {
        let Ok(md) = fs::symlink_metadata(&snap) else {
            continue;
        };
        if !md.is_file() || md.len() == 0 {
            continue;
        }
        for (blob, len) in &blobs {
            if *len != md.len() {
                continue;
            }
            let key = format!(
                "{}|{}|{len}",
                blob.file_name().unwrap_or_default().to_string_lossy(),
                snap.strip_prefix(repo_dir).unwrap_or(&snap).display()
            );
            if done.contains(&key) {
                continue;
            }
            match (sha256_of(blob), sha256_of(&snap)) {
                (Some(a), Some(b)) if a == b => {}
                _ => continue,
            }
            match relink(blob, &snap) {
                Ok(()) => {
                    report.bytes_freed += len;
                    report.actions.push(format!(
                        "de-duplicated {} ({} MB) in {}",
                        snap.file_name().unwrap_or_default().to_string_lossy(),
                        len / 1_048_576,
                        repo_dir.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    newly_done.push(key);
                }
                Err(e) => tracing::debug!(
                    "could not de-duplicate {} ({e}); left as is",
                    blob.display()
                ),
            }
        }
    }
    if !newly_done.is_empty() {
        let mut all = done;
        all.extend(newly_done);
        let _ = fs::write(&marker, all.join("\n"));
    }
}

/// Replace `blob` by a hard link to `snapshot`, restoring `blob` on failure.
fn relink(blob: &Path, snapshot: &Path) -> std::io::Result<()> {
    let backup = blob.with_extension("attic-relink");
    fs::rename(blob, &backup)?;
    match fs::hard_link(snapshot, blob) {
        Ok(()) => {
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(e) => {
            let _ = fs::rename(&backup, blob);
            Err(e)
        }
    }
}

fn sha256_of(path: &Path) -> Option<String> {
    use sha2::Digest;
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex::encode(hasher.finalize()))
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            match fs::symlink_metadata(&p) {
                Ok(md) if md.is_dir() => stack.push(p),
                Ok(_) => out.push(p),
                Err(_) => {}
            }
        }
    }
    out
}

fn find_files_named(dir: &Path, name: &str) -> Vec<PathBuf> {
    walk_files(dir)
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n == name))
        .collect()
}

/// Bytes the cache stores that `onnx-fp16/` does not share: its `blobs/`.
/// (Snapshot copies are either links to blobs or the files `onnx-fp16/` is
/// hard-linked to, so they are not counted.)
fn unique_bytes(dir: &Path) -> u64 {
    walk_files(&dir.join("blobs"))
        .iter()
        .filter_map(|p| fs::symlink_metadata(p).ok())
        .filter(|md| md.is_file())
        .map(|md| md.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, body: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// Layout as the Windows Hugging Face client leaves it: blob + separate
    /// snapshot copy, and onnx-fp16 hard-linked to the snapshot copy.
    fn windows_layout(models: &Path) {
        let repo = onnx_download_cache_dir(models);
        write(&repo.join("blobs/aaa"), b"weights-weights");
        write(&repo.join("blobs/bbb"), b"tok");
        write(
            &repo.join("snapshots/rev/onnx/model_fp16.onnx_data"),
            b"weights-weights",
        );
        write(&repo.join("snapshots/rev/onnx/model_fp16.onnx"), b"graph");
        write(&repo.join("snapshots/rev/tokenizer.json"), b"tok");
        let target = onnx_assets::onnx_dir(models);
        fs::create_dir_all(&target).unwrap();
        for (src, dst) in [
            (
                "snapshots/rev/onnx/model_fp16.onnx_data",
                onnx_assets::MODEL_DATA_FILE,
            ),
            (
                "snapshots/rev/onnx/model_fp16.onnx",
                onnx_assets::MODEL_FILE,
            ),
            ("snapshots/rev/tokenizer.json", onnx_assets::TOKENIZER_FILE),
        ] {
            fs::hard_link(repo.join(src), target.join(dst)).unwrap();
        }
        write(
            &models
                .join(".locks")
                .join(repo.file_name().unwrap())
                .join("x.lock"),
            b"",
        );
    }

    #[test]
    fn onnx_download_cache_is_removed_and_gpu_model_stays_intact() {
        let tmp = tempfile::tempdir().unwrap();
        windows_layout(tmp.path());
        let mut r = CleanupReport::default();
        prune_onnx_download_cache(tmp.path(), &mut r);
        assert!(!onnx_download_cache_dir(tmp.path()).exists());
        assert!(
            !tmp.path()
                .join(".locks/models--onnx-community--Qwen3-Embedding-0.6B-ONNX")
                .exists()
        );
        let target = onnx_assets::onnx_dir(tmp.path());
        assert!(onnx_assets::assets_present(&target));
        assert_eq!(
            fs::read(target.join(onnx_assets::MODEL_DATA_FILE)).unwrap(),
            b"weights-weights"
        );
        assert!(r.bytes_freed > 0, "{r:?}");
        // Idempotent.
        let mut again = CleanupReport::default();
        prune_onnx_download_cache(tmp.path(), &mut again);
        assert_eq!(again, CleanupReport::default());
    }

    #[test]
    fn onnx_download_cache_is_kept_while_the_gpu_model_is_incomplete() {
        let tmp = tempfile::tempdir().unwrap();
        windows_layout(tmp.path());
        let target = onnx_assets::onnx_dir(tmp.path());
        // Missing external data: the cache is still the only full copy.
        fs::remove_file(target.join(onnx_assets::MODEL_DATA_FILE)).unwrap();
        prune_onnx_download_cache(tmp.path(), &mut CleanupReport::default());
        assert!(onnx_download_cache_dir(tmp.path()).exists());
        // Missing graph: not ready at all.
        fs::remove_file(target.join(onnx_assets::MODEL_FILE)).unwrap();
        prune_onnx_download_cache(tmp.path(), &mut CleanupReport::default());
        assert!(onnx_download_cache_dir(tmp.path()).exists());
    }

    #[test]
    fn identical_blob_becomes_a_hard_link_and_mismatches_are_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("models--Qwen--Qwen3-Embedding-0.6B");
        write(&repo.join("blobs/w"), b"safetensors-bytes");
        write(
            &repo.join("snapshots/rev/model.safetensors"),
            b"safetensors-bytes",
        );
        // Same size, different content: must not be linked.
        write(&repo.join("blobs/x"), b"AAAAAAAA");
        write(&repo.join("snapshots/rev/config.json"), b"BBBBBBBB");

        let mut r = CleanupReport::default();
        dedup_snapshot_blobs(&repo, &mut r);
        assert_eq!(r.bytes_freed, 17, "{r:?}");
        assert_eq!(
            fs::read(repo.join("blobs/w")).unwrap(),
            b"safetensors-bytes"
        );
        assert_eq!(fs::read(repo.join("blobs/x")).unwrap(), b"AAAAAAAA");
        // Hard-linked: removing the snapshot name keeps the blob's bytes,
        // and writing through one name is visible through the other.
        fs::write(
            repo.join("snapshots/rev/model.safetensors"),
            b"rewritten-bytes!!",
        )
        .unwrap();
        assert_eq!(
            fs::read(repo.join("blobs/w")).unwrap(),
            b"rewritten-bytes!!"
        );

        // Second run does no work (marker), and never touches the mismatch.
        let mut again = CleanupReport::default();
        dedup_snapshot_blobs(&repo, &mut again);
        assert_eq!(again.bytes_freed, 0, "{again:?}");
    }

    #[test]
    fn cleanup_runs_both_steps_over_the_models_folder() {
        let tmp = tempfile::tempdir().unwrap();
        windows_layout(tmp.path());
        let cpu = tmp.path().join("models--Qwen--Qwen3-Embedding-0.6B");
        write(&cpu.join("blobs/w"), b"cpu-model");
        write(&cpu.join("snapshots/rev/model.safetensors"), b"cpu-model");
        let r = cleanup_model_cache(tmp.path());
        assert!(!onnx_download_cache_dir(tmp.path()).exists());
        assert_eq!(r.actions.len(), 2, "{r:?}");
        assert_eq!(
            fs::read(cpu.join("snapshots/rev/model.safetensors")).unwrap(),
            b"cpu-model"
        );
    }
}
