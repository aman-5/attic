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

/// True when `dir` already contains everything the GPU provider needs.
///
/// Deliberately does not check the optional external-data file: whether it
/// should be present is a property of the export, not of this directory, so
/// requiring it would permanently report "incomplete" for an inline export.
pub fn assets_present(dir: &Path) -> bool {
    dir.join(MODEL_FILE).is_file() && dir.join(TOKENIZER_FILE).is_file()
}

/// Resolve the directory the ONNX assets are materialised into.
pub fn onnx_dir(cache_dir: &Path) -> PathBuf {
    cache_dir.join(ONNX_DIR_NAME)
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
    let target = onnx_dir(cache_dir);
    if assets_present(&target) {
        return Ok(target);
    }

    let revision = revision.unwrap_or(DEFAULT_ONNX_REVISION);
    let client = hf_hub::HFClient::builder()
        .cache_dir(cache_dir.to_path_buf())
        .build_sync()
        .map_err(|e| SemanticError::ProviderUnavailable {
            provider: ONNX_PROVIDER_ID.into(),
            reason: format!("failed to build hf-hub client for ONNX assets: {e}"),
        })?;
    let repo = client.model(HF_ONNX_OWNER.to_string(), HF_ONNX_REPO.to_string());

    let model = fetch_required(&repo, REPO_MODEL_PATH, revision)?;
    let tokenizer = fetch_required(&repo, TOKENIZER_FILE, revision)?;
    // Required, not optional. The pinned revision demonstrably publishes
    // this companion, and ORT resolves it relative to the graph at load
    // time. Treating a failed fetch as "this export must be inline" would
    // promote a directory that passes the readiness check and only fails
    // much later, inside the worker, as an opaque missing-external-data
    // error -- precisely the half-complete state the staging dance exists
    // to prevent.
    let model_data = fetch_required(&repo, REPO_MODEL_DATA_PATH, revision)?;

    materialise(&target, &model, Some(&model_data), &tokenizer)?;
    Ok(target)
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

    std::fs::copy(model, staging.join(MODEL_FILE))
        .map_err(|e| io("failed to stage the ONNX graph", e))?;
    std::fs::copy(tokenizer, staging.join(TOKENIZER_FILE))
        .map_err(|e| io("failed to stage the ONNX tokenizer", e))?;
    if let Some(data) = model_data {
        std::fs::copy(data, staging.join(MODEL_DATA_FILE))
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

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn readiness_requires_graph_and_tokenizer_but_not_external_data() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("d");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!assets_present(&dir), "an empty directory is not ready");

        write(&dir.join(MODEL_FILE), "graph");
        assert!(!assets_present(&dir), "graph alone is not ready");

        write(&dir.join(TOKENIZER_FILE), "tok");
        assert!(
            assets_present(&dir),
            "an inline export with no companion data file must count as ready"
        );
    }

    #[test]
    fn materialise_produces_a_flat_directory_from_split_repo_layout() {
        // The upstream layout puts the graph under onnx/ and the tokenizer
        // at the root. ORT resolves external data relative to the graph, so
        // the whole point of materialising is that they end up adjacent.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        write(&src.join("onnx").join(MODEL_FILE), "graph");
        write(&src.join("onnx").join(MODEL_DATA_FILE), "weights");
        write(&src.join(TOKENIZER_FILE), "tok");

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
            "weights"
        );
    }

    #[test]
    fn an_inline_export_materialises_without_a_data_file() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        write(&src.join(MODEL_FILE), "graph");
        write(&src.join(TOKENIZER_FILE), "tok");

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
        write(&src.join(MODEL_FILE), "graph");
        write(&src.join(TOKENIZER_FILE), "tok");

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

    #[test]
    fn ensure_is_a_no_op_when_assets_are_already_materialised() {
        // The offline/no-network guarantee: a warm cache must never reach
        // the network, so this call must succeed with no client at all.
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().to_path_buf();
        let target = onnx_dir(&cache);
        write(&target.join(MODEL_FILE), "graph");
        write(&target.join(TOKENIZER_FILE), "tok");

        let got = ensure_onnx_assets(&cache, None).expect("warm cache must not need the network");
        assert_eq!(got, target);
    }
}
