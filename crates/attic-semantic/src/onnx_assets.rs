//! Runtime acquisition of the ONNX export used by the GPU execution backend.
//!
//! # Why this module exists
//!
//! The safetensors (Candle) path has always downloaded its own weights at
//! runtime: a `DeferredProvider` comes up immediately, a background task
//! fetches the model, and the real provider is hot-swapped in without a
//! restart. The ONNX/DirectML path had nothing equivalent — it simply
//! checked whether `model_fp16.onnx` and `tokenizer.json` already happened
//! to be sitting in a directory the user had configured by hand, and
//! silently skipped GPU acceleration when they were not.
//!
//! That asymmetry is the real reason real GPUs sat idle. A user who never
//! learned about an undocumented environment variable, and never manually
//! downloaded a 1.2 GB ONNX export, could not reach the GPU path at all —
//! and got no diagnostic explaining why. We download one model for them
//! automatically; demanding they hand-stage the other is indefensible.
//!
//! # What "acquired" means here
//!
//! ONNX external-data makes this slightly more than a file copy. The
//! upstream repository stores the graph under `onnx/` but the tokenizer at
//! the repository root, and a large export may carry its weights beside the
//! graph in a companion `.onnx_data` file. ONNX Runtime resolves that
//! companion **relative to the graph file**, so the two must end up in the
//! same directory under their exact expected names.
//!
//! We therefore materialise a flat, self-contained directory rather than
//! pointing the provider at the Hugging Face cache layout:
//!
//! ```text
//! <cache_dir>/onnx-fp16/
//!     model_fp16.onnx
//!     model_fp16.onnx_data   (only when the export uses external data)
//!     tokenizer.json
//! ```
//!
//! Materialisation is two-phase, matching the atomic-activation policy the
//! rest of `model_assets` already follows: files are staged into a sibling
//! temporary directory and promoted with a single rename. A crash or a
//! killed download can therefore never leave a half-written directory that
//! looks complete to the readiness check — the provider either sees a
//! finished directory or no directory at all.

use crate::error::SemanticError;
use crate::model_assets::{ModelAssetError, ModelAssetManager, ModelManifest};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Upstream repository holding the ONNX export.
pub const HF_ONNX_OWNER: &str = "onnx-community";
/// Repository name for the Qwen3 embedding ONNX export.
pub const HF_ONNX_REPO: &str = "Qwen3-Embedding-0.6B-ONNX";

/// Provider id used in error messages so failures are attributable to the
/// GPU path rather than to the safetensors path.
const ONNX_PROVIDER_ID: &str = "qwen3-ort";

/// Name of the directory materialised beneath the model cache.
pub const ONNX_DIR_NAME: &str = "onnx-fp16";

/// The graph file the DirectML provider opens.
pub const MODEL_FILE: &str = "model_fp16.onnx";
/// Companion external-data file. Optional: an export small enough to stay
/// inside the protobuf size limit stores its weights inline and publishes no
/// such file, so a miss here is a normal outcome and never an error.
pub const MODEL_DATA_FILE: &str = "model_fp16.onnx_data";
/// Tokenizer, stored at the repository root rather than under `onnx/`.
pub const TOKENIZER_FILE: &str = "tokenizer.json";

/// Path within the repository for the graph and its companion data.
const REPO_MODEL_PATH: &str = "onnx/model_fp16.onnx";
const REPO_MODEL_DATA_PATH: &str = "onnx/model_fp16.onnx_data";

/// Revision pinned for reproducibility.
///
/// Verified against the upstream repository rather than assumed: this is the
/// commit that actually publishes `onnx/model_fp16.onnx` and its companion
/// `onnx/model_fp16.onnx_data`. Pinning matters more here than convenience
/// does — tracking a moving branch would let an upstream re-export silently
/// change the vector space underneath an existing index, and the fingerprint
/// (`model_revision: "onnx-community-fp16"`) would not notice.
pub const DEFAULT_ONNX_REVISION: &str = "c25a394dd583836952667c12f008335071b3f43d";

/// Treat tiny placeholder files as incomplete installs. Real ONNX assets are
/// orders of magnitude larger; a 1-byte sentinel must never pass readiness.
const MIN_READY_BYTES: u64 = 1_024;
/// Graphs that genuinely use external data are tiny relative to their
/// companion weight file, so never scan arbitrarily large inline graphs into
/// RAM just to look for the companion-file name.
const EXTERNAL_DATA_SCAN_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

/// True when `dir` already contains everything the GPU provider needs.
///
/// The graph and tokenizer must both exist and be non-trivially sized. The
/// companion external-data file is required only when the graph actually
/// references it (managed fp16 export), so a genuine inline export can still
/// count as ready while an interrupted fp16 download cannot.
pub fn assets_present(dir: &Path) -> bool {
    let model = dir.join(MODEL_FILE);
    let tokenizer = dir.join(TOKENIZER_FILE);
    if !sized_file_present(&model) || !sized_file_present(&tokenizer) {
        return false;
    }
    if !graph_uses_external_data(&model) {
        return true;
    }
    sized_file_present(&dir.join(MODEL_DATA_FILE))
}

fn sized_file_present(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() >= MIN_READY_BYTES)
        .unwrap_or(false)
}

fn graph_uses_external_data(model: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(model) else {
        return false;
    };
    if !meta.is_file() || meta.len() > EXTERNAL_DATA_SCAN_LIMIT_BYTES {
        return false;
    }

    let Ok(file) = std::fs::File::open(model) else {
        return false;
    };
    let needle = MODEL_DATA_FILE.as_bytes();
    let overlap = needle.len().saturating_sub(1);
    let mut reader = std::io::BufReader::new(file);
    let mut carry = Vec::with_capacity(overlap);
    let mut buf = [0u8; 8 * 1024];
    loop {
        let Ok(read) = reader.read(&mut buf) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        let mut chunk = Vec::with_capacity(carry.len() + read);
        chunk.extend_from_slice(&carry);
        chunk.extend_from_slice(&buf[..read]);
        if chunk.windows(needle.len()).any(|w| w == needle) {
            return true;
        }
        carry.clear();
        let keep = overlap.min(chunk.len());
        carry.extend_from_slice(&chunk[chunk.len() - keep..]);
    }
}

/// Resolve the directory the ONNX assets are materialised into.
pub fn onnx_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join(ONNX_DIR_NAME)
}

fn provider_unavailable(reason: impl Into<String>) -> SemanticError {
    SemanticError::ProviderUnavailable {
        provider: ONNX_PROVIDER_ID.into(),
        reason: reason.into(),
    }
}

fn pinned_manifest(revision: &str) -> Result<ModelManifest, SemanticError> {
    if revision != DEFAULT_ONNX_REVISION {
        return Err(provider_unavailable(format!(
            "unsupported ONNX revision {revision}; only pinned revision {DEFAULT_ONNX_REVISION} is trusted"
        )));
    }
    Ok(ModelManifest::qwen3_onnx_default())
}

fn quarantine_materialized_dir(target: &Path) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = target.with_file_name(format!("{ONNX_DIR_NAME}.corrupt-{stamp}"));
    std::fs::rename(target, &dest)?;
    Ok(dest)
}

fn remove_cached_download(path: &Path) {
    let resolved = std::fs::canonicalize(path).ok();
    let _ = std::fs::remove_file(path);
    if let Some(resolved) = resolved
        && resolved != path
    {
        let _ = std::fs::remove_file(resolved);
    }
}

fn verify_materialized_dir(target: &Path, manifest: &ModelManifest) -> Result<(), ModelAssetError> {
    ModelAssetManager::validate_directory_files(target, &manifest.files)
}

fn verify_downloaded_assets(
    manifest: &ModelManifest,
    model: &Path,
    model_data: &Path,
    tokenizer: &Path,
) -> Result<(), ModelAssetError> {
    let spec = |name: &str| {
        manifest
            .files
            .iter()
            .find(|file| file.filename == name)
            .ok_or_else(|| {
                ModelAssetError::ValidationFailed(format!("missing ONNX manifest entry for {name}"))
            })
    };

    ModelAssetManager::validate_file(model, spec(MODEL_FILE)?)?;
    ModelAssetManager::validate_file(model_data, spec(MODEL_DATA_FILE)?)?;
    ModelAssetManager::validate_file(tokenizer, spec(TOKENIZER_FILE)?)?;
    Ok(())
}

fn ensure_onnx_assets_with_fetch<F>(
    cache_dir: &Path,
    manifest: &ModelManifest,
    fetch: F,
) -> Result<PathBuf, SemanticError>
where
    F: Fn(&'static str) -> Result<PathBuf, SemanticError> + Copy + Sync,
{
    let target = onnx_dir(cache_dir);
    if target.exists() {
        match verify_materialized_dir(&target, manifest) {
            Ok(()) => return Ok(target),
            Err(ModelAssetError::MissingFile(_)) => {
                std::fs::remove_dir_all(&target).map_err(|e| {
                    provider_unavailable(format!(
                        "failed to remove incomplete ONNX asset directory {}: {e}",
                        target.display()
                    ))
                })?;
            }
            Err(e) => {
                let quarantine = quarantine_materialized_dir(&target)
                    .map(|p| format!("; quarantined to {}", p.display()))
                    .unwrap_or_else(|qe| format!("; quarantine failed: {qe}"));
                return Err(provider_unavailable(format!(
                    "existing ONNX asset directory failed verification: {e}{quarantine}"
                )));
            }
        }
    }

    let (model, tokenizer, model_data) = std::thread::scope(|s| {
        let m = s.spawn(|| fetch(REPO_MODEL_PATH));
        let t = s.spawn(|| fetch(TOKENIZER_FILE));
        let d = s.spawn(|| fetch(REPO_MODEL_DATA_PATH));
        let join = |h: std::thread::ScopedJoinHandle<'_, Result<PathBuf, SemanticError>>| {
            h.join()
                .unwrap_or_else(|_| Err(provider_unavailable("ONNX download thread panicked")))
        };
        (join(m), join(t), join(d))
    });
    let (model, tokenizer, model_data) = (model?, tokenizer?, model_data?);

    if let Err(e) = verify_downloaded_assets(manifest, &model, &model_data, &tokenizer) {
        for path in [&model, &model_data, &tokenizer] {
            remove_cached_download(path);
        }
        return Err(provider_unavailable(format!(
            "downloaded ONNX assets failed verification: {e}; removed the fetched cache files so the next attempt redownloads them"
        )));
    }

    materialise(&target, &model, Some(&model_data), &tokenizer)?;
    Ok(target)
}

/// Ensure the ONNX assets exist locally, downloading them if required, and
/// return the flat directory containing them.
///
/// Idempotent and cheap on the common path: an already-materialised
/// directory returns immediately without contacting the network, so this is
/// safe to call on every startup.
///
/// This performs network I/O and can take minutes on a cold cache. It must
/// never be called on the startup path — callers run it on the same
/// background-download task the safetensors path uses, so canonical and
/// lexical indexing are never blocked waiting for a GPU model.
pub fn ensure_onnx_assets(
    cache_dir: &Path,
    revision: Option<&str>,
) -> Result<PathBuf, SemanticError> {
    let revision = revision.unwrap_or(DEFAULT_ONNX_REVISION);
    let manifest = pinned_manifest(revision)?;
    // The three files are independent: fetch them in parallel (one client
    // each, so no shared-state assumptions about the hf-hub client).
    let fetch = |path: &'static str| -> Result<PathBuf, SemanticError> {
        let client = hf_hub::HFClient::builder()
            .cache_dir(cache_dir.to_path_buf())
            .build_sync()
            .map_err(|e| {
                provider_unavailable(format!(
                    "failed to build hf-hub client for ONNX assets: {e}"
                ))
            })?;
        let repo = client.model(HF_ONNX_OWNER.to_string(), HF_ONNX_REPO.to_string());
        fetch_required(&repo, path, revision)
    };
    ensure_onnx_assets_with_fetch(cache_dir, &manifest, fetch)
}

fn fetch_required(
    repo: &hf_hub::HFRepositorySync<hf_hub::RepoTypeModel>,
    filename: &str,
    revision: &str,
) -> Result<PathBuf, SemanticError> {
    repo.download_file()
        .filename(filename)
        .revision(revision)
        .send()
        .map_err(|e| SemanticError::ProviderUnavailable {
            provider: ONNX_PROVIDER_ID.into(),
            reason: format!(
                "failed to fetch {filename} from {HF_ONNX_OWNER}/{HF_ONNX_REPO}@{revision}: {e}"
            ),
        })
}

/// Place `src` at `dst` without a second full write when possible: a hard
/// link to the resolved cache blob (following any cache symlink first so the
/// link never points at a relative symlink), falling back to a copy when
/// linking is impossible (different volume, filesystem without links).
fn link_or_copy(src: &Path, dst: &Path) -> std::io::Result<()> {
    let resolved = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    match std::fs::hard_link(&resolved, dst) {
        Ok(()) => Ok(()),
        Err(_) => std::fs::copy(&resolved, dst).map(|_| ()),
    }
}

/// Copy the resolved files into a flat directory using stage-then-rename so
/// a partially written directory is never observable.
fn materialise(
    target: &Path,
    model: &Path,
    model_data: Option<&Path>,
    tokenizer: &Path,
) -> Result<(), SemanticError> {
    let io = |ctx: &str, e: std::io::Error| SemanticError::ProviderUnavailable {
        provider: ONNX_PROVIDER_ID.into(),
        reason: format!("{ctx}: {e}"),
    };

    let parent = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| io("failed to create the model cache directory", e))?;

    // A staging sibling rather than a system temp dir: the promote below is
    // a rename, and a rename across filesystems is not atomic (and on
    // Windows fails outright). Keeping staging next to the target keeps
    // both on the same volume by construction.
    let staging = parent.join(format!(".{ONNX_DIR_NAME}.staging"));
    if staging.exists() {
        // Left behind by an interrupted attempt. It is not a valid target
        // and never promoted, so discarding it is always safe.
        std::fs::remove_dir_all(&staging)
            .map_err(|e| io("failed to clear a stale ONNX staging directory", e))?;
    }
    std::fs::create_dir_all(&staging)
        .map_err(|e| io("failed to create the ONNX staging directory", e))?;

    link_or_copy(model, &staging.join(MODEL_FILE))
        .map_err(|e| io("failed to stage the ONNX graph", e))?;
    link_or_copy(tokenizer, &staging.join(TOKENIZER_FILE))
        .map_err(|e| io("failed to stage the ONNX tokenizer", e))?;
    if let Some(data) = model_data {
        link_or_copy(data, &staging.join(MODEL_DATA_FILE))
            .map_err(|e| io("failed to stage the ONNX external-data file", e))?;
    }

    // Losing a race here is success, not failure: another process having
    // already produced a complete directory is exactly the desired end
    // state, so we drop our staging copy and use theirs.
    if target.exists() {
        let _ = std::fs::remove_dir_all(&staging);
        return Ok(());
    }
    match std::fs::rename(&staging, target) {
        Ok(()) => Ok(()),
        Err(_) if assets_present(target) => {
            let _ = std::fs::remove_dir_all(&staging);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(io("failed to promote the staged ONNX directory", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_or_copy_places_identical_content() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("blob");
        std::fs::write(&src, "weights").unwrap();
        let dst = tmp.path().join("placed");
        link_or_copy(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "weights");
        // Removing the placed file must never remove the cache blob.
        std::fs::remove_file(&dst).unwrap();
        assert!(src.exists());
    }

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn payload(ch: char) -> String {
        ch.to_string().repeat(MIN_READY_BYTES as usize)
    }

    #[test]
    fn readiness_requires_nontrivial_files_and_the_expected_external_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!assets_present(&dir), "an empty directory is not ready");

        write(&dir.join(MODEL_FILE), &payload('g'));
        assert!(!assets_present(&dir), "graph alone is not ready");

        write(&dir.join(TOKENIZER_FILE), &payload('t'));
        assert!(
            assets_present(&dir),
            "an inline export with no companion data file must count as ready"
        );

        let external = tmp.path().join("external");
        write(
            &external.join(MODEL_FILE),
            &format!("{}{}", payload('g'), MODEL_DATA_FILE),
        );
        write(&external.join(TOKENIZER_FILE), &payload('t'));
        assert!(
            !assets_present(&external),
            "a graph that references external data must not pass without it"
        );
        write(&external.join(MODEL_DATA_FILE), &payload('d'));
        assert!(assets_present(&external));
    }

    #[test]
    fn tiny_placeholder_files_never_count_as_ready() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tiny");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir.join(MODEL_FILE), "graph");
        write(&dir.join(TOKENIZER_FILE), "token");
        assert!(
            !assets_present(&dir),
            "5-byte placeholders must not masquerade as a complete install"
        );
    }

    #[test]
    fn small_graphs_detect_external_data_references() {
        let tmp = tempfile::tempdir().unwrap();
        let model = tmp.path().join(MODEL_FILE);
        write(&model, &format!("{}{}", payload('g'), MODEL_DATA_FILE));
        assert!(graph_uses_external_data(&model));
    }

    #[test]
    fn large_sparse_graphs_do_not_scan_for_external_data() {
        let tmp = tempfile::tempdir().unwrap();
        let model = tmp.path().join(MODEL_FILE);
        let mut file = std::fs::File::create(&model).unwrap();
        std::io::Write::write_all(&mut file, MODEL_DATA_FILE.as_bytes()).unwrap();
        file.set_len(EXTERNAL_DATA_SCAN_LIMIT_BYTES + 1).unwrap();
        assert!(
            !graph_uses_external_data(&model),
            "large inline graphs must short-circuit on size instead of being read into memory"
        );
    }

    #[test]
    fn materialise_produces_a_flat_directory_from_split_repo_layout() {
        // The upstream layout puts the graph under onnx/ and the tokenizer
        // at the root. ORT resolves external data relative to the graph, so
        // the whole point of materialising is that they end up adjacent.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let graph = format!("{}{}", payload('g'), MODEL_DATA_FILE);
        let weights = payload('w');
        let tokenizer = payload('t');
        write(&src.join("onnx").join(MODEL_FILE), &graph);
        write(&src.join("onnx").join(MODEL_DATA_FILE), &weights);
        write(&src.join(TOKENIZER_FILE), &tokenizer);

        let target = onnx_dir(&tmp.path().join("cache"));
        materialise(
            &target,
            &src.join("onnx").join(MODEL_FILE),
            Some(&src.join("onnx").join(MODEL_DATA_FILE)),
            &src.join(TOKENIZER_FILE),
        )
        .unwrap();

        assert!(assets_present(&target));
        assert!(target.join(MODEL_DATA_FILE).is_file());
        assert_eq!(
            std::fs::read_to_string(target.join(MODEL_DATA_FILE)).unwrap(),
            weights
        );
    }

    #[test]
    fn an_inline_export_materialises_without_a_data_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let graph = payload('g');
        let tokenizer = payload('t');
        write(&src.join(MODEL_FILE), &graph);
        write(&src.join(TOKENIZER_FILE), &tokenizer);

        let target = onnx_dir(&tmp.path().join("cache"));
        materialise(
            &target,
            &src.join(MODEL_FILE),
            None,
            &src.join(TOKENIZER_FILE),
        )
        .unwrap();

        assert!(assets_present(&target));
        assert!(
            !target.join(MODEL_DATA_FILE).exists(),
            "must not fabricate an external-data file the export does not have"
        );
    }

    #[test]
    fn a_stale_staging_directory_does_not_block_a_retry() {
        // Simulates an interrupted download: staging exists, is incomplete,
        // and was never promoted. The next attempt must recover on its own
        // rather than requiring a human to clear the cache.
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let staging = cache.join(format!(".{ONNX_DIR_NAME}.staging"));
        write(&staging.join("partial.bin"), "junk");

        let src = tmp.path().join("src");
        let graph = payload('g');
        let tokenizer = payload('t');
        write(&src.join(MODEL_FILE), &graph);
        write(&src.join(TOKENIZER_FILE), &tokenizer);

        let target = onnx_dir(&cache);
        materialise(
            &target,
            &src.join(MODEL_FILE),
            None,
            &src.join(TOKENIZER_FILE),
        )
        .unwrap();

        assert!(assets_present(&target));
        assert!(
            !target.join("partial.bin").exists(),
            "stale junk must not survive"
        );
        assert!(!staging.exists(), "staging must not be left behind");
    }

    fn sha_hex(body: &[u8]) -> String {
        use sha2::Digest;
        hex::encode(sha2::Sha256::digest(body))
    }

    fn manifest_for(model: &[u8], tokenizer: &[u8], model_data: Option<&[u8]>) -> ModelManifest {
        let mut files = vec![
            crate::model_assets::ModelFileSpec {
                filename: MODEL_FILE.to_string(),
                expected_sha256: Some(sha_hex(model)),
                expected_size_bytes: Some(model.len() as u64),
            },
            crate::model_assets::ModelFileSpec {
                filename: TOKENIZER_FILE.to_string(),
                expected_sha256: Some(sha_hex(tokenizer)),
                expected_size_bytes: Some(tokenizer.len() as u64),
            },
        ];
        if let Some(data) = model_data {
            files.push(crate::model_assets::ModelFileSpec {
                filename: MODEL_DATA_FILE.to_string(),
                expected_sha256: Some(sha_hex(data)),
                expected_size_bytes: Some(data.len() as u64),
            });
        }
        ModelManifest {
            model_id: "test-onnx".into(),
            repo_owner: "test".into(),
            repo_name: "repo".into(),
            pinned_revision: "test-rev".into(),
            files,
            license: "Apache-2.0".into(),
            provenance: "test".into(),
        }
    }

    #[test]
    fn warm_cache_is_verified_without_fetching() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().to_path_buf();
        let target = onnx_dir(&cache);
        let graph = format!("{}{}", payload('g'), MODEL_DATA_FILE);
        let weights = payload('w');
        let tokenizer = payload('t');
        write(&target.join(MODEL_FILE), &graph);
        write(&target.join(MODEL_DATA_FILE), &weights);
        write(&target.join(TOKENIZER_FILE), &tokenizer);
        let manifest = manifest_for(
            graph.as_bytes(),
            tokenizer.as_bytes(),
            Some(weights.as_bytes()),
        );

        let got = ensure_onnx_assets_with_fetch(&cache, &manifest, |_| {
            panic!("warm cache verification must not fetch")
        })
        .expect("warm cache must not need the network");
        assert_eq!(got, target);
    }

    #[test]
    fn corrupt_materialized_dir_is_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().to_path_buf();
        let target = onnx_dir(&cache);
        let graph = format!("{}{}", payload('g'), MODEL_DATA_FILE);
        let weights = payload('w');
        let tokenizer = payload('t');
        write(&target.join(MODEL_FILE), &graph);
        write(&target.join(MODEL_DATA_FILE), &weights);
        write(&target.join(TOKENIZER_FILE), "tampered");
        let manifest = manifest_for(
            graph.as_bytes(),
            tokenizer.as_bytes(),
            Some(weights.as_bytes()),
        );

        let err = ensure_onnx_assets_with_fetch(&cache, &manifest, |_| {
            panic!("corrupt warm cache must fail closed before fetching")
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("failed verification"), "{err}");
        assert!(!target.exists(), "corrupt target must be moved aside");
        let quarantines: Vec<_> = std::fs::read_dir(&cache)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("onnx-fp16.corrupt-"))
            .collect();
        assert_eq!(quarantines.len(), 1, "{quarantines:?}");
    }

    #[test]
    fn downloaded_files_are_verified_before_materialise() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let downloads = tmp.path().join("downloads");
        let graph = format!("{}{}", payload('g'), MODEL_DATA_FILE);
        let weights = payload('w');
        let tokenizer = payload('t');
        write(&downloads.join("onnx").join(MODEL_FILE), &graph);
        write(&downloads.join("onnx").join(MODEL_DATA_FILE), "tampered");
        write(&downloads.join(TOKENIZER_FILE), &tokenizer);
        let manifest = manifest_for(
            graph.as_bytes(),
            tokenizer.as_bytes(),
            Some(weights.as_bytes()),
        );

        let err = ensure_onnx_assets_with_fetch(&cache, &manifest, |name| match name {
            REPO_MODEL_PATH => Ok(downloads.join("onnx").join(MODEL_FILE)),
            REPO_MODEL_DATA_PATH => Ok(downloads.join("onnx").join(MODEL_DATA_FILE)),
            TOKENIZER_FILE => Ok(downloads.join(TOKENIZER_FILE)),
            other => panic!("unexpected fetch path {other}"),
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("failed verification"), "{err}");
        assert!(!onnx_dir(&cache).exists(), "target must never be promoted");
        assert!(
            !downloads.join("onnx").join(MODEL_DATA_FILE).exists(),
            "tampered download must be removed so the next attempt redownloads it"
        );
    }
}
