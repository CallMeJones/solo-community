// SPDX-License-Identifier: Apache-2.0

//! [`BundledEmbedder`] is Solo Community's local CPU sentence-transformer.
//!
//! Official Windows and Ubuntu packages carry the pinned quantized
//! `all-MiniLM-L6-v2` ONNX and tokenizer assets beside the executable (or in
//! `/usr/share/solo`). Those installs perform semantic memory operations
//! without Ollama, an API key, or a first-use network request. Source builds
//! retain fastembed's cache/download path as a developer fallback when
//! packaged assets are not present.
//!
//! Model assets stay out of Git history. Release and CI workflows fetch the
//! exact revision and verify every SHA-256 from `installer/models/` before
//! packaging. `SOLO_EMBEDDING_MODEL_DIR` is a strict override: if set, a
//! missing or incomplete model directory is an error instead of a download.
//!
//! ## Identity
//!
//! `Embedder::name() == "bundled:all-MiniLM-L6-v2"`,
//! `version() == "v2"`, `dim() == 384`, `dtype() == F32`.
//!
//! The v2 identity distinguishes the pinned quantized package from the
//! historical registry-provided model so `solo migrate-embedder` can
//! deterministically regenerate vectors.
//!
//! ## Thread safety
//!
//! `BundledEmbedder` holds an `Arc<tokio::sync::OnceCell<Arc<Mutex<
//! TextEmbedding>>>>`. The OnceCell guarantees single initialisation
//! across concurrent first-use calls. The inner `tokio::sync::Mutex`
//! serialises `TextEmbedding::embed` (which takes `&mut self`); ort's
//! inference is non-reentrant on a single session, so per-batch
//! serialisation matches the upstream contract.
//!
//! ## Fallback semantics
//!
//! `try_new` is fallible. If construction fails (missing packaged files,
//! ort init panic-mapped-to-Err, incompatible system library, etc.) the
//! caller is expected to fall back to
//! [`crate::embedder::StubEmbedder::default_stub`] and emit a
//! `tracing::warn!` line — `build_embedder_from_env` in `mod.rs`
//! implements this fallback path explicitly.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use fastembed::{
    EmbeddingModel, InitOptions, Pooling, QuantizationMode, TextEmbedding, TokenizerFiles,
    UserDefinedEmbeddingModel,
};
use solo_core::{Embedder, Embedding, EmbeddingDtype, Error, Result};
use tokio::sync::{Mutex, OnceCell};

/// Embedder identity (matches `Embedder::name()`).
pub const BUNDLED_EMBEDDER_NAME: &str = "bundled:all-MiniLM-L6-v2";
/// Solo-side wrapper version. Bump on any change to embedding output
/// shape so `solo reembed` regenerates affected vectors.
pub const BUNDLED_EMBEDDER_VERSION: &str = "v2";
/// 384-dim — fixed for all-MiniLM-L6-v2; baked into the model.
pub const BUNDLED_EMBEDDER_DIM: usize = 384;
/// Optional explicit path to the five packaged embedding-model files.
pub const BUNDLED_MODEL_DIR_ENV: &str = "SOLO_EMBEDDING_MODEL_DIR";
/// Installation-relative directory used by Windows ZIP and setup packages.
pub const BUNDLED_MODEL_RELATIVE_DIR: &str = "models/all-MiniLM-L6-v2";

const BUNDLED_MODEL_FILE: &str = "model.onnx";
const BUNDLED_TOKENIZER_FILE: &str = "tokenizer.json";
const BUNDLED_CONFIG_FILE: &str = "config.json";
const BUNDLED_SPECIAL_TOKENS_FILE: &str = "special_tokens_map.json";
const BUNDLED_TOKENIZER_CONFIG_FILE: &str = "tokenizer_config.json";

/// Bundled CPU sentence-transformer embedder.
///
/// Cheap to clone — the underlying model handle is `Arc`-wrapped.
#[derive(Clone)]
pub struct BundledEmbedder {
    /// Lazy-loaded model handle. First call to [`Self::ensure_model`]
    /// runs the constructor under the OnceCell guard; subsequent calls
    /// share the loaded model.
    ///
    /// `Arc<Mutex<TextEmbedding>>` (not `Arc<TextEmbedding>`) because
    /// fastembed's `TextEmbedding::embed` takes `&mut self`. Locking
    /// per batch matches ort's non-reentrant inference contract.
    model: Arc<OnceCell<Arc<Mutex<TextEmbedding>>>>,
}

impl Default for BundledEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for BundledEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundledEmbedder")
            .field("name", &BUNDLED_EMBEDDER_NAME)
            .field("dim", &BUNDLED_EMBEDDER_DIM)
            .field("loaded", &self.model.get().is_some())
            .finish()
    }
}

impl BundledEmbedder {
    /// Construct a lazy embedder handle. The model is NOT loaded here; the
    /// first `embed_batch` call triggers packaged-model loading and init.
    ///
    /// This keeps daemon startup fast and lets `solo doctor` test config
    /// validity without loading ~22 MB of model data or spinning up ort.
    pub fn new() -> Self {
        Self {
            model: Arc::new(OnceCell::new()),
        }
    }

    /// Eagerly load the model. Useful at `solo daemon` boot time so the
    /// first user query doesn't pay the model-load tail latency.
    ///
    /// Idempotent: a second call after `try_new` (or after a lazy
    /// embed_batch) is a no-op that returns immediately.
    ///
    /// Returns the cached model handle so callers can verify a single
    /// init occurred.
    pub async fn try_new(&self) -> Result<Arc<Mutex<TextEmbedding>>> {
        self.ensure_model().await.cloned()
    }

    /// Internal: lazy model init. Returns a reference into the OnceCell
    /// so we can verify single-init by `Arc::strong_count` in tests.
    async fn ensure_model(&self) -> Result<&Arc<Mutex<TextEmbedding>>> {
        self.model
            .get_or_try_init(|| async {
                tracing::info!(model = BUNDLED_EMBEDDER_NAME, "loading bundled embedder");
                // `spawn_blocking` because fastembed's `TextEmbedding::
                // try_new` is synchronous and may run ort's session-init
                // + an hf-hub fetch (~22 MB on a cold cache) on the
                // calling thread. Off-loading keeps the tokio runtime
                // responsive during the one-time model load.
                let model = tokio::task::spawn_blocking(load_bundled_model)
                    .await
                    .map_err(|e| {
                        Error::embedder(format!(
                            "bundled embedder init task panicked or was cancelled: {e}"
                        ))
                    })?
                    .map_err(|e| {
                        Error::embedder(format!(
                            "bundled embedder init failed (packaged model/fastembed/ort): {e}. \
                         Reinstall Solo to restore the packaged model, or fall back to \
                         SOLO_EMBEDDER=ollama."
                        ))
                    })?;
                Ok(Arc::new(Mutex::new(model)))
            })
            .await
    }
}

fn required_model_files(model_dir: &Path) -> [PathBuf; 5] {
    [
        model_dir.join(BUNDLED_MODEL_FILE),
        model_dir.join(BUNDLED_TOKENIZER_FILE),
        model_dir.join(BUNDLED_CONFIG_FILE),
        model_dir.join(BUNDLED_SPECIAL_TOKENS_FILE),
        model_dir.join(BUNDLED_TOKENIZER_CONFIG_FILE),
    ]
}

fn validate_model_dir(model_dir: &Path) -> std::result::Result<(), String> {
    let missing: Vec<String> = required_model_files(model_dir)
        .into_iter()
        .filter(|path| !path.is_file())
        .map(|path| path.display().to_string())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "embedding model directory {} is incomplete; missing {}",
            model_dir.display(),
            missing.join(", ")
        ))
    }
}

fn packaged_model_dir() -> std::result::Result<Option<PathBuf>, String> {
    if let Some(raw) = std::env::var_os(BUNDLED_MODEL_DIR_ENV) {
        if raw.is_empty() {
            return Err(format!("{BUNDLED_MODEL_DIR_ENV} is set but empty"));
        }
        let model_dir = PathBuf::from(raw);
        validate_model_dir(&model_dir)?;
        return Ok(Some(model_dir));
    }

    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            candidates.push(parent.join(BUNDLED_MODEL_RELATIVE_DIR));
        }
    }
    #[cfg(target_os = "linux")]
    candidates.push(PathBuf::from("/usr/share/solo").join(BUNDLED_MODEL_RELATIVE_DIR));

    for model_dir in candidates {
        if model_dir.exists() {
            validate_model_dir(&model_dir)?;
            return Ok(Some(model_dir));
        }
    }
    Ok(None)
}

fn load_packaged_model(model_dir: &Path) -> anyhow::Result<TextEmbedding> {
    validate_model_dir(model_dir).map_err(anyhow::Error::msg)?;
    let read = |name: &str| {
        std::fs::read(model_dir.join(name)).map_err(|error| {
            anyhow::anyhow!(
                "read packaged embedding asset {}: {error}",
                model_dir.join(name).display()
            )
        })
    };
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: read(BUNDLED_TOKENIZER_FILE)?,
        config_file: read(BUNDLED_CONFIG_FILE)?,
        special_tokens_map_file: read(BUNDLED_SPECIAL_TOKENS_FILE)?,
        tokenizer_config_file: read(BUNDLED_TOKENIZER_CONFIG_FILE)?,
    };
    let model = UserDefinedEmbeddingModel::new(read(BUNDLED_MODEL_FILE)?, tokenizer_files)
        .with_pooling(Pooling::Mean)
        .with_quantization(QuantizationMode::Dynamic);
    TextEmbedding::try_new_from_user_defined(model, Default::default())
}

fn load_bundled_model() -> anyhow::Result<TextEmbedding> {
    if let Some(model_dir) = packaged_model_dir().map_err(anyhow::Error::msg)? {
        tracing::info!(path = %model_dir.display(), "loading packaged embedding model");
        return load_packaged_model(&model_dir);
    }

    tracing::warn!(
        "packaged embedding model not found; source build will use the fastembed cache/download path"
    );
    TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::AllMiniLML6V2Q).with_show_download_progress(false),
    )
}

#[async_trait]
impl Embedder for BundledEmbedder {
    fn name(&self) -> &str {
        BUNDLED_EMBEDDER_NAME
    }

    fn version(&self) -> &str {
        BUNDLED_EMBEDDER_VERSION
    }

    fn dim(&self) -> usize {
        BUNDLED_EMBEDDER_DIM
    }

    fn dtype(&self) -> EmbeddingDtype {
        EmbeddingDtype::F32
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Embedding>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let model = self.ensure_model().await?.clone();

        // Copy texts to owned strings so the spawn_blocking closure
        // can move them — &[&str] borrows can't cross the boundary.
        let owned: Vec<String> = texts.iter().map(|t| (*t).to_string()).collect();

        let vectors: Vec<Vec<f32>> = tokio::task::spawn_blocking(move || {
            let mut guard = model.blocking_lock();
            guard.embed(&owned, None)
        })
        .await
        .map_err(|e| {
            Error::embedder(format!(
                "bundled embedder inference task panicked or was cancelled: {e}"
            ))
        })?
        .map_err(|e| Error::embedder(format!("bundled embedder embed_batch failed: {e}")))?;

        // Convert each Vec<f32> into our Embedding wire shape. Validate
        // dim per vector — defends against fastembed returning a
        // truncated batch on partial-failure (we'd rather error than
        // silently miscompare in HNSW recall).
        let mut out = Vec::with_capacity(vectors.len());
        for v in vectors {
            if v.len() != BUNDLED_EMBEDDER_DIM {
                return Err(Error::embedder(format!(
                    "bundled embedder returned dim {} (expected {})",
                    v.len(),
                    BUNDLED_EMBEDDER_DIM
                )));
            }
            let mut data = Vec::with_capacity(v.len() * 4);
            for f in &v {
                data.extend_from_slice(&f.to_le_bytes());
            }
            out.push(Embedding {
                dtype: EmbeddingDtype::F32,
                dim: BUNDLED_EMBEDDER_DIM,
                data,
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    //! Tests for [`BundledEmbedder`]. Gated by `#[cfg(feature =
    //! "bundled-embedder")]` at the module-include site in `mod.rs`,
    //! so this whole file only compiles + runs when the feature is on.
    //!
    //! On the first test run in a fresh build environment fastembed
    //! downloads ~22 MB from HuggingFace; subsequent runs reuse the
    //! cache. Tests share a single `BundledEmbedder` handle via the
    //! `SHARED_EMBEDDER` static — this serialises model init across
    //! the parallel cargo-test runners (without the share, eight
    //! concurrent first-use calls each try to `download_model_to_cache`
    //! and hf-hub's per-file lock surfaces flaky "Failed to retrieve
    //! model.onnx" errors when two writers race on the same partial-
    //! download path). The `is_lazy_at_construction` and
    //! `try_new_loads_eagerly_and_is_idempotent` cases need a fresh
    //! handle — they construct their own.

    use super::*;
    use std::sync::OnceLock;

    /// Shared embedder reused across the data-validation tests so
    /// fastembed only runs `try_new` once per test binary (the
    /// hf-hub cache lock is process-wide; concurrent first-use from
    /// parallel test tasks otherwise hits a flaky "model.onnx not
    /// found" inside `~/.cache/huggingface/hub/`).
    fn shared() -> &'static BundledEmbedder {
        static SHARED: OnceLock<BundledEmbedder> = OnceLock::new();
        SHARED.get_or_init(BundledEmbedder::new)
    }

    /// Cosine similarity helper for the semantic-sanity tests.
    fn cosine(a: &Embedding, b: &Embedding) -> f32 {
        let av = a.as_f32_slice().expect("a is f32");
        let bv = b.as_f32_slice().expect("b is f32");
        assert_eq!(av.len(), bv.len(), "dim mismatch");
        let dot: f32 = av.iter().zip(bv.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = av.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = bv.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb).max(1e-9)
    }

    #[tokio::test]
    async fn bundled_embedder_produces_384_dim_vectors() {
        let v = shared()
            .embed("hello world")
            .await
            .expect("embed should succeed");
        assert_eq!(v.dim, BUNDLED_EMBEDDER_DIM);
        assert_eq!(v.dtype, EmbeddingDtype::F32);
        assert_eq!(v.data.len(), BUNDLED_EMBEDDER_DIM * 4);
        v.validate().expect("embedding length invariant");
    }

    #[tokio::test]
    async fn bundled_embedder_emits_expected_identity() {
        // Identity-only — no need to share, no embed call.
        let e = BundledEmbedder::new();
        assert_eq!(e.name(), "bundled:all-MiniLM-L6-v2");
        assert_eq!(e.version(), "v2");
        assert_eq!(e.dim(), 384);
        assert_eq!(e.dtype(), EmbeddingDtype::F32);
    }

    #[tokio::test]
    async fn bundled_embedder_is_deterministic_across_calls() {
        let a = shared().embed("the quick brown fox").await.unwrap();
        let b = shared().embed("the quick brown fox").await.unwrap();
        assert_eq!(a.data, b.data, "same input must produce identical bytes");
    }

    #[tokio::test]
    async fn bundled_embedder_distinct_inputs_produce_distinct_vectors() {
        let a = shared().embed("alpha").await.unwrap();
        let b = shared().embed("beta").await.unwrap();
        assert_ne!(a.data, b.data);
    }

    #[tokio::test]
    async fn bundled_embedder_does_semantic_work() {
        // The whole point of bundling a real model: cosine(semantically
        // similar) should beat cosine(dissimilar). If this regresses,
        // either the model swapped under us or the embed pipeline is
        // returning nonsense.
        let a = shared().embed("the cat sat on the mat").await.unwrap();
        let b = shared().embed("a feline rested on the rug").await.unwrap();
        let c = shared()
            .embed("Rust's borrow checker enforces aliasing rules")
            .await
            .unwrap();

        let sim_ab = cosine(&a, &b);
        let sim_ac = cosine(&a, &c);
        assert!(
            sim_ab > sim_ac,
            "semantically similar pair (cat/feline) should beat dissimilar \
             (cat/Rust): sim_ab={sim_ab} sim_ac={sim_ac}"
        );
        assert!(sim_ab > 0.0, "semantic similarity should be positive");
    }

    #[tokio::test]
    async fn bundled_embedder_handles_utf8_multi_byte() {
        // Multi-byte: emoji, CJK, RTL Arabic. The tokenizer is
        // sentencepiece/wordpiece — UTF-8-safe.
        let v = shared()
            .embed("こんにちは 🦀 مرحبا")
            .await
            .expect("multi-byte UTF-8 must embed cleanly");
        assert_eq!(v.dim, BUNDLED_EMBEDDER_DIM);
        v.validate().unwrap();
    }

    #[tokio::test]
    async fn bundled_embedder_empty_input_returns_empty_batch() {
        // Documented choice: empty batch → empty output (no error).
        // Avoids forcing every caller to filter out empty input lists.
        // For a single empty STRING via embed("") fastembed produces a
        // valid vector (the model has a [CLS] embedding) — covered by
        // a separate case below.
        let out = shared().embed_batch(&[]).await.unwrap();
        assert_eq!(out.len(), 0);
    }

    #[tokio::test]
    async fn bundled_embedder_empty_string_returns_valid_vector() {
        // Empty string: fastembed/tokenizer produces a [CLS]-only
        // embedding. We return it — caller can filter if they want.
        let v = shared()
            .embed("")
            .await
            .expect("empty string is valid input");
        assert_eq!(v.dim, BUNDLED_EMBEDDER_DIM);
        v.validate().unwrap();
    }

    #[tokio::test]
    async fn bundled_embedder_batch_preserves_input_order() {
        let inputs = ["one", "two", "three", "four"];
        let batch = shared().embed_batch(&inputs).await.unwrap();
        assert_eq!(batch.len(), inputs.len());
        let mut singles = Vec::with_capacity(inputs.len());
        for text in inputs {
            singles.push(shared().embed(text).await.unwrap());
        }
        // Dynamic quantization plus batch padding can cause small numeric
        // differences from single-item inference, so byte equality is not
        // a valid contract. Each batch entry must nevertheless match its
        // corresponding input more closely than any other input.
        for (i, text) in inputs.iter().enumerate() {
            let own_similarity = cosine(&batch[i], &singles[i]);
            let other_similarity = singles
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, single)| cosine(&batch[i], single))
                .fold(f32::NEG_INFINITY, f32::max);
            assert!(
                own_similarity > other_similarity,
                "batch[{i}] did not align with {text}: own={own_similarity}, other={other_similarity}"
            );
        }
    }

    #[tokio::test]
    async fn bundled_embedder_concurrent_calls_do_not_deadlock() {
        // 8 parallel embed tasks against the shared handle. The
        // OnceCell serialises init; the inner Mutex serialises
        // inference. None of them should deadlock; all should
        // eventually return.
        let mut handles = Vec::new();
        for i in 0..8 {
            handles.push(tokio::spawn(async move {
                let text = format!("concurrent call number {i}");
                shared().embed(&text).await
            }));
        }
        for h in handles {
            let v = h.await.expect("join").expect("embed");
            assert_eq!(v.dim, BUNDLED_EMBEDDER_DIM);
        }
    }

    #[tokio::test]
    async fn bundled_embedder_is_lazy_at_construction() {
        // Constructor must NOT load the model — `new()` should return
        // before any hf-hub download or ort session-init runs.
        // Verified by inspecting the OnceCell state.
        //
        // This case needs a FRESH embedder (not `shared()`) so we can
        // assert the OnceCell is empty pre-call. After this test
        // completes the per-test embedder is dropped — no leak into
        // the shared one. The lazy-init assertion is the load-bearing
        // claim of the test; we don't need to actually call embed()
        // here (and avoiding that call also dodges a second hf-hub
        // download race against the shared embedder running in
        // parallel).
        let e = BundledEmbedder::new();
        assert!(
            e.model.get().is_none(),
            "OnceCell must be empty before any embed/try_new call"
        );
    }

    #[tokio::test]
    async fn bundled_embedder_try_new_loads_eagerly_and_is_idempotent() {
        // Use the shared handle so we don't trigger a second hf-hub
        // download race. After the first test in the binary has
        // populated the OnceCell, both try_new calls here are
        // OnceCell-hit fast paths.
        let model1 = shared().try_new().await.expect("eager init");
        let model2 = shared().try_new().await.expect("second eager init");
        // Same Arc — both calls must hit the same OnceCell slot.
        assert!(
            Arc::ptr_eq(&model1, &model2),
            "try_new should be idempotent"
        );
    }

    #[tokio::test]
    async fn bundled_embedder_normalised_or_valid_floats() {
        // Sanity: every component must be finite (no NaN/Inf). Some
        // models normalise to unit length, some don't; all-MiniLM-L6-v2
        // does NOT normalise by default (fastembed leaves that to the
        // caller). We just check finiteness here — recall code handles
        // normalisation downstream.
        let v = shared().embed("finite floats only").await.unwrap();
        let slice = v.as_f32_slice().unwrap();
        for (i, f) in slice.iter().enumerate() {
            assert!(f.is_finite(), "non-finite component at index {i}: {f}");
        }
    }

    #[test]
    fn packaged_model_directory_requires_every_runtime_asset() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(BUNDLED_MODEL_FILE), b"model").unwrap();
        let error = validate_model_dir(temp.path()).unwrap_err();
        assert!(error.contains(BUNDLED_TOKENIZER_FILE));
        assert!(error.contains(BUNDLED_CONFIG_FILE));

        for path in required_model_files(temp.path()) {
            std::fs::write(path, b"fixture").unwrap();
        }
        validate_model_dir(temp.path()).unwrap();
    }
}
