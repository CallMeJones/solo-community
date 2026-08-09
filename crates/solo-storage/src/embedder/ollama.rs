// SPDX-License-Identifier: Apache-2.0

//! [`OllamaEmbedder`] — embeds text via Ollama's native `/api/embeddings`
//! endpoint.
//!
//! Structural cousin of [`crate::llm::openai::OpenAIClient`] (the
//! inline retry-loop pattern is mirrored from there).
//!
//! ## Why a separate client, not a wrap-around `OpenAIClient`
//!
//! Ollama exposes both an OpenAI-compatible `/v1` shim *and* a native
//! API. The LLM path ([`crate::llm::ollama::OllamaClient`]) uses the
//! `/v1` shim — the wire format matches OpenAI Chat Completions
//! exactly, so a single HTTP client serves both. The embeddings path
//! has no OpenAI-shim parity: the `/v1/embeddings` shim returns
//! `data: [{embedding: [...]}]` (OpenAI's batch shape), while
//! `/api/embeddings` returns `{embedding: [...]}` (Ollama's single-
//! prompt shape) and is the canonical native endpoint that
//! `ollama pull <model>` documents. We POST against `/api/embeddings`
//! directly with our own retry loop rather than going through a
//! shim, matching the surface that downstream users will actually
//! configure.
//!
//! ## Wire format
//!
//! `POST {base_url}/api/embeddings`:
//!
//! ```json
//! { "model": "nomic-embed-text", "prompt": "the text to embed", "keep_alive": "30s" }
//! ```
//!
//! Response:
//!
//! ```json
//! { "embedding": [0.0123, -0.0456, …, 0.0789] }
//! ```
//!
//! The vector length depends on the model — `nomic-embed-text` is
//! 768 dimensions, `mxbai-embed-large` is 1024, `all-minilm` is 384.
//! The caller passes the expected `dim` at construction; the impl
//! validates each response against it and surfaces a clean
//! [`solo_core::Error::Embedder`] on mismatch rather than silently
//! truncating or padding.
//!
//! ## Retry policy
//!
//! Inline loop mirroring [`crate::llm::openai::OpenAIClient::complete`]:
//! retry on 429 + 5xx (honouring `Retry-After`) and on transient
//! network errors (connect / timeout / request). Bounded by
//! [`crate::llm::retry::RetryConfig`]; defaults to 3 retries with
//! 500ms base / 10s cap and full-jitter exponential backoff.
//!
//! ## Batch semantics
//!
//! Ollama's `/api/embeddings` is single-prompt. [`Embedder::embed_batch`]
//! is implemented by iterating over the input and issuing N HTTP
//! requests. For Solo's batch sizes (single-digit clusters at
//! consolidation time) this is fine; if/when Ollama ships
//! `/api/embed` (multi-prompt) we'll add a fast-path. Each request
//! reuses the same `reqwest::Client` so connection pooling kicks
//! in — no per-call TLS handshake.
//!
//! ## Out of scope in 6A
//!
//! - Env-driven construction (`build_embedder_from_env`) — that's 6B.
//! - CLI wiring (`build_embedder` in `solo-cli/src/commands/common.rs`)
//!   — that's 6C.
//! - Dim probing at `solo init` — that's 6D. The caller passes `dim`
//!   explicitly at construction.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solo_core::{Embedder, Embedding, EmbeddingDtype, Error, Result};

use crate::llm::retry::{
    RetryConfig, exp_backoff_with_jitter, is_retryable_reqwest_err, is_retryable_status,
    parse_retry_after,
};

const DEFAULT_BASE_URL: &str = "http://localhost:11434";
const EMBEDDINGS_PATH: &str = "/api/embeddings";
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_KEEP_ALIVE: &str = "30s";
const ENV_OLLAMA_KEEP_ALIVE: &str = "SOLO_OLLAMA_KEEP_ALIVE";
/// `Embedder::version()` sentinel. Bump on any change that affects
/// the byte-shape of a stored embedding for the same input — `solo
/// reembed` keys on `(name, version)` to decide whether to rebuild.
/// The actual model identity is in `name()` (`"ollama:<model>"`);
/// `version` here is the Solo-side wrapper version, independent of
/// the model's own training version.
const EMBEDDER_VERSION: &str = "v1";

/// Default model when the caller doesn't override. Matches the
/// v0.5.x roadmap's locked default (`nomic-embed-text`, 768-dim,
/// 270 MB, MIT). Used by [`OllamaEmbedder::with_defaults`].
pub const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";

/// Default output dimension matching [`DEFAULT_OLLAMA_MODEL`].
/// Documented for sub-step 6D (init-time dim probing): probing
/// would override this once the sentinel embed returns.
pub const DEFAULT_OLLAMA_DIM: usize = 768;

/// Ollama `/api/embeddings` HTTP backend.
///
/// Cheap to clone (the inner `reqwest::Client` is `Arc`-backed
/// internally; configuration strings are plain `String`/`usize`).
#[derive(Clone)]
pub struct OllamaEmbedder {
    http: reqwest::Client,
    base_url: String,
    model: String,
    dim: usize,
    retry: RetryConfig,
    keep_alive: String,
    /// Precomputed `"ollama:<model>"` so [`Embedder::name`] can
    /// return `&str` without per-call allocation. Mirrors
    /// [`crate::llm::ollama::OllamaClient::display_name`].
    display_name: String,
}

impl OllamaEmbedder {
    /// Build with explicit base URL, model, and output dimension.
    /// The dim is required at construction because the
    /// [`Embedder`] trait promises a stable dim across calls
    /// without round-tripping — probing happens externally (sub-
    /// step 6D at `solo init`).
    ///
    /// Defaults: 60s timeout, [`RetryConfig::default`] retry policy.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>, dim: usize) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .map_err(|e| Error::embedder(format!("build reqwest client: {e}")))?;
        let mut base = base_url.into();
        // Trim trailing slashes so `format!("{base}{path}")` doesn't
        // produce `http://host//api/embeddings`. Same hygiene as
        // `OpenAIClient::with_base_url`.
        while base.ends_with('/') {
            base.pop();
        }
        let model = model.into();
        let display_name = format!("ollama:{model}");
        let keep_alive = ollama_keep_alive_from_env();
        Ok(Self {
            http,
            base_url: base,
            model,
            dim,
            retry: RetryConfig::default(),
            keep_alive,
            display_name,
        })
    }

    /// Build with the documented defaults: localhost Ollama,
    /// `nomic-embed-text`, 768 dim. Useful for tests and for the
    /// future env-builder fast path when none of `SOLO_OLLAMA_BASE_URL`
    /// / `SOLO_OLLAMA_EMBED_MODEL` is set.
    pub fn with_defaults() -> Result<Self> {
        Self::new(DEFAULT_BASE_URL, DEFAULT_OLLAMA_MODEL, DEFAULT_OLLAMA_DIM)
    }

    /// Override the request timeout. Defaults to 60 sec.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| Error::embedder(format!("rebuild reqwest client: {e}")))?;
        Ok(self)
    }

    /// Override the retry policy. Defaults to
    /// [`RetryConfig::default`] (3 retries, 500ms base, 10s cap).
    /// Use [`RetryConfig::none`] for single-shot semantics (tests
    /// that want to observe the first failure without backoff).
    pub fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    /// Override Ollama's model residency after each embedding request.
    /// Uses Ollama's duration syntax, for example `30s`, `5m`, or `0`.
    pub fn with_keep_alive(mut self, keep_alive: impl Into<String>) -> Self {
        let keep_alive = keep_alive.into();
        self.keep_alive = normalize_keep_alive(&keep_alive);
        self
    }

    /// Borrow the configured base URL (without trailing slash).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Borrow the configured model name (without the `ollama:` prefix).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Borrow the configured Ollama keep-alive duration.
    pub fn keep_alive(&self) -> &str {
        &self.keep_alive
    }

    /// Probe the actual embedding dimension by issuing one sentinel
    /// embed against the configured `(base_url, model)` and returning
    /// the length of the returned `Vec<f32>`. Does NOT enforce the
    /// `self.dim` invariant — that's the whole point: at `solo init`
    /// time we don't yet know the model's dim, so we ask the server
    /// and persist whatever it returns.
    ///
    /// Used by [`crate::embedder::probe_embedder_config_from_env`] in
    /// sub-step 6D; callers passing through the regular
    /// [`Embedder::embed_batch`] path keep the strict dim check.
    ///
    /// The `dim` field on `self` does not affect the probe — construct
    /// the embedder with `DEFAULT_OLLAMA_DIM` (or any non-zero
    /// placeholder), call `probe_dim`, then either rebuild a fresh
    /// `OllamaEmbedder` with the probed dim or discard `self` after
    /// persisting the value to config.
    pub async fn probe_dim(&self) -> Result<usize> {
        // Sentinel chosen to be short + unambiguous in case it leaks
        // to a debug log somewhere. Ollama doesn't care about the
        // text content — any non-empty prompt produces a full vector.
        let vec = self.embed_one("solo_init_dim_probe").await?;
        if vec.is_empty() {
            return Err(Error::embedder(
                "ollama /api/embeddings returned an empty vector during dim probe",
            ));
        }
        Ok(vec.len())
    }

    /// Embed a single text via one POST to `/api/embeddings`.
    /// Internal helper called from [`Embedder::embed_batch`] once
    /// per input. Returns the raw `Vec<f32>` — the caller is
    /// responsible for wrapping it into an [`Embedding`] with
    /// dim/dtype validation.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let body = EmbeddingsRequest {
            model: &self.model,
            prompt: text,
            keep_alive: &self.keep_alive,
        };
        let url = format!("{}{}", self.base_url, EMBEDDINGS_PATH);

        // Retry loop — same shape as OpenAIClient::complete. The
        // retry primitives (status classifier, jitter, Retry-After
        // parser) are shared between LLM + embedder paths.
        let mut attempt: u32 = 0;
        loop {
            let send_res = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            match send_res {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let parsed: EmbeddingsResponse = resp.json().await.map_err(|e| {
                            Error::embedder(format!("ollama embeddings parse: {e}"))
                        })?;
                        if parsed.embedding.is_empty() {
                            return Err(Error::embedder(
                                "ollama /api/embeddings returned empty embedding vector",
                            ));
                        }
                        return Ok(parsed.embedding);
                    }

                    let retry_after_hdr = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let body_text = resp.text().await.unwrap_or_default();

                    if attempt < self.retry.max_retries && is_retryable_status(status.as_u16()) {
                        let delay =
                            parse_retry_after(retry_after_hdr.as_deref(), self.retry.max_delay)
                                .unwrap_or_else(|| {
                                    exp_backoff_with_jitter(attempt + 1, &self.retry)
                                });
                        tracing::warn!(
                            attempt = attempt + 1,
                            status = %status,
                            delay_ms = delay.as_millis() as u64,
                            "ollama embeddings retryable HTTP error; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(Error::embedder(format!(
                        "ollama embeddings HTTP {}: {}",
                        status,
                        truncate(&body_text, 500)
                    )));
                }
                Err(e) => {
                    if attempt < self.retry.max_retries && is_retryable_reqwest_err(&e) {
                        let delay = exp_backoff_with_jitter(attempt + 1, &self.retry);
                        tracing::warn!(
                            attempt = attempt + 1,
                            error = %e,
                            delay_ms = delay.as_millis() as u64,
                            "ollama embeddings retryable network error; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(Error::embedder(format!("ollama embeddings request: {e}")));
                }
            }
        }
    }
}

#[async_trait]
impl Embedder for OllamaEmbedder {
    fn name(&self) -> &str {
        &self.display_name
    }

    fn version(&self) -> &str {
        EMBEDDER_VERSION
    }

    fn dim(&self) -> usize {
        self.dim
    }

    fn dtype(&self) -> EmbeddingDtype {
        EmbeddingDtype::F32
    }

    fn runtime_probe_url(&self) -> Option<String> {
        Some(format!("{}{}", self.base_url, "/api/tags"))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Embedding>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out: Vec<Embedding> = Vec::with_capacity(texts.len());
        // `/api/embeddings` is single-prompt; iterate. The
        // `reqwest::Client` re-uses its connection pool across
        // calls, so the per-extra-prompt cost is just the round-
        // trip, not a fresh TLS handshake.
        for text in texts {
            let vec = self.embed_one(text).await?;
            if vec.len() != self.dim {
                return Err(Error::embedder(format!(
                    "ollama {} produced {} dims, expected {}",
                    self.model,
                    vec.len(),
                    self.dim
                )));
            }
            let mut bytes = Vec::with_capacity(self.dim * 4);
            for v in &vec {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            out.push(Embedding {
                dtype: EmbeddingDtype::F32,
                dim: self.dim,
                data: bytes,
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    keep_alive: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    /// The dense embedding vector. Length matches the model's
    /// hidden_size (`nomic-embed-text` = 768, `mxbai-embed-large`
    /// = 1024). Missing/empty is treated as a protocol error in
    /// [`OllamaEmbedder::embed_one`].
    #[serde(default)]
    embedding: Vec<f32>,
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

fn ollama_keep_alive_from_env() -> String {
    std::env::var(ENV_OLLAMA_KEEP_ALIVE)
        .ok()
        .map(|value| normalize_keep_alive(&value))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_KEEP_ALIVE.to_string())
}

fn normalize_keep_alive(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        DEFAULT_KEEP_ALIVE.to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a 768-dim F32 vector deterministically from `seed` so
    /// tests can assert on the byte shape without hard-coding a
    /// 768-entry literal.
    fn fixture_embedding(seed: u32, dim: usize) -> Vec<f32> {
        (0..dim)
            .map(|i| ((seed.wrapping_add(i as u32)) as f32) * 1e-3)
            .collect()
    }

    /// Convenience: build an `OllamaEmbedder` pointed at a wiremock
    /// `MockServer` with retries disabled by default. Tests that
    /// exercise retry override `with_retry_config(...)`.
    fn embedder_for(server: &MockServer, dim: usize) -> OllamaEmbedder {
        OllamaEmbedder::new(server.uri(), "nomic-embed-test", dim)
            .unwrap()
            .with_retry_config(RetryConfig::none())
    }

    #[tokio::test]
    async fn happy_path_returns_embedding_vec() {
        let server = MockServer::start().await;
        let dim = 8;
        let fixture = fixture_embedding(1, dim);

        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture
            })))
            .expect(1)
            .mount(&server)
            .await;

        let e = embedder_for(&server, dim);
        assert_eq!(e.dim(), dim);
        assert_eq!(e.dtype(), EmbeddingDtype::F32);

        let out = e.embed("hello world").await.expect("embed succeeds");
        assert_eq!(out.dim, dim);
        assert_eq!(out.dtype, EmbeddingDtype::F32);
        assert_eq!(out.data.len(), dim * 4);

        // Roundtrip bytes → f32 and confirm match.
        let parsed = out.as_f32_slice().expect("F32 slice");
        for (i, expected) in fixture.iter().enumerate() {
            assert!(
                (parsed[i] - expected).abs() < 1e-6,
                "dim {i}: got {} expected {}",
                parsed[i],
                expected
            );
        }
    }

    #[tokio::test]
    async fn batch_iterates_and_preserves_order() {
        let server = MockServer::start().await;
        let dim = 4;
        // Each call returns a different fixture so we can assert
        // order is preserved across the iterated POSTs.
        let fixture_a = fixture_embedding(10, dim);
        let fixture_b = fixture_embedding(20, dim);
        let fixture_c = fixture_embedding(30, dim);

        // Wiremock doesn't natively cycle responses, so use a
        // body matcher per prompt: the embedder sends `prompt`
        // verbatim in the JSON body.
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"prompt": "alpha"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture_a
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"prompt": "beta"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture_b
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"prompt": "gamma"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture_c
            })))
            .mount(&server)
            .await;

        let e = embedder_for(&server, dim);
        let out = e
            .embed_batch(&["alpha", "beta", "gamma"])
            .await
            .expect("batch succeeds");
        assert_eq!(out.len(), 3);
        let a = out[0].as_f32_slice().unwrap();
        let b = out[1].as_f32_slice().unwrap();
        let c = out[2].as_f32_slice().unwrap();
        assert!((a[0] - fixture_a[0]).abs() < 1e-6, "row 0 first elem");
        assert!((b[0] - fixture_b[0]).abs() < 1e-6, "row 1 first elem");
        assert!((c[0] - fixture_c[0]).abs() < 1e-6, "row 2 first elem");
        // And distinct.
        assert_ne!(a, b);
        assert_ne!(b, c);
    }

    #[tokio::test]
    async fn server_500_retries_then_succeeds() {
        let server = MockServer::start().await;
        let dim = 4;
        let fixture = fixture_embedding(99, dim);

        // First mock matches the first request only (up_to_n_times(1))
        // and returns 503. Subsequent requests fall through to the
        // second mock returning 200.
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Custom retry: short delays so the test stays under a
        // second even if jitter peaks.
        let retry = RetryConfig {
            max_retries: 2,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        };
        let e = OllamaEmbedder::new(server.uri(), "nomic-embed-test", dim)
            .unwrap()
            .with_retry_config(retry);

        let out = e.embed("retry test").await.expect("eventual success");
        assert_eq!(out.dim, dim);
        let parsed = out.as_f32_slice().unwrap();
        assert!((parsed[0] - fixture[0]).abs() < 1e-6);
    }

    #[tokio::test]
    async fn server_500_permanently_fails_after_max_retries() {
        let server = MockServer::start().await;
        let dim = 4;

        // Every request gets 500. With max_retries=2 we expect
        // 1 initial + 2 retries = 3 total hits.
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;

        let retry = RetryConfig {
            max_retries: 2,
            base_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(20),
        };
        let e = OllamaEmbedder::new(server.uri(), "nomic-embed-test", dim)
            .unwrap()
            .with_retry_config(retry);

        let err = e
            .embed("perma fail")
            .await
            .expect_err("expected error after exhausting retries");
        // Surface should be Embedder-flavoured with the HTTP status
        // embedded so operators can grep logs.
        let msg = format!("{err}");
        assert!(
            msg.contains("ollama embeddings HTTP 500"),
            "unexpected error message: {msg}"
        );
    }

    #[tokio::test]
    async fn name_returns_ollama_prefixed_model() {
        // Pure-construction test — no HTTP, doesn't need wiremock.
        let e = OllamaEmbedder::new("http://localhost:11434", "nomic-embed-text", 768).unwrap();
        assert_eq!(e.name(), "ollama:nomic-embed-text");
        assert_eq!(e.version(), "v1");
        assert_eq!(e.dim(), 768);
        assert_eq!(e.dtype(), EmbeddingDtype::F32);
        assert_eq!(e.model(), "nomic-embed-text");
        assert_eq!(e.base_url(), "http://localhost:11434");
        assert_eq!(
            e.runtime_probe_url().as_deref(),
            Some("http://localhost:11434/api/tags")
        );
    }

    #[tokio::test]
    async fn with_defaults_matches_locked_roadmap_values() {
        let e = OllamaEmbedder::with_defaults().unwrap();
        assert_eq!(e.name(), "ollama:nomic-embed-text");
        assert_eq!(e.dim(), 768);
        assert_eq!(e.base_url(), "http://localhost:11434");
    }

    #[tokio::test]
    async fn runtime_probe_url_uses_configured_base_url() {
        let e =
            OllamaEmbedder::new("http://custom-host:31000///", "nomic-embed-test", 768).unwrap();
        assert_eq!(
            e.runtime_probe_url().as_deref(),
            Some("http://custom-host:31000/api/tags")
        );
    }

    #[tokio::test]
    async fn request_includes_keep_alive() {
        let server = MockServer::start().await;
        let dim = 4;
        let fixture = fixture_embedding(1, dim);

        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .and(wiremock::matchers::body_partial_json(
                serde_json::json!({"keep_alive": "1s"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture
            })))
            .expect(1)
            .mount(&server)
            .await;

        let e = embedder_for(&server, dim).with_keep_alive("1s");
        assert_eq!(e.keep_alive(), "1s");
        let out = e.embed("keep alive test").await.expect("embed succeeds");
        assert_eq!(out.dim, dim);
    }

    #[tokio::test]
    async fn base_url_trailing_slashes_are_trimmed() {
        let e = OllamaEmbedder::new("http://localhost:11434///", "m", 1).unwrap();
        assert_eq!(e.base_url(), "http://localhost:11434");
    }

    #[tokio::test]
    async fn malformed_response_errors_cleanly() {
        let server = MockServer::start().await;
        let dim = 4;

        // No `embedding` field — serde's default fills with `[]`,
        // which our impl treats as a protocol violation.
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "not_embedding": [0.1, 0.2, 0.3, 0.4]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let e = embedder_for(&server, dim);
        let err = e
            .embed("malformed")
            .await
            .expect_err("missing embedding field must error, not panic");
        let msg = format!("{err}");
        assert!(
            msg.contains("empty embedding"),
            "expected clean empty-vector error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn dim_mismatch_surfaces_as_error_not_silent_truncation() {
        let server = MockServer::start().await;
        let configured_dim = 8;
        let server_returned_dim = 4;
        let fixture = fixture_embedding(1, server_returned_dim);

        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture
            })))
            .expect(1)
            .mount(&server)
            .await;

        let e = embedder_for(&server, configured_dim);
        let err = e
            .embed("dim mismatch")
            .await
            .expect_err("dim mismatch must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("produced 4 dims, expected 8"),
            "unexpected dim-mismatch error: {msg}"
        );
    }

    #[tokio::test]
    async fn probe_dim_reports_server_returned_length_ignoring_configured_dim() {
        // The probe is meant to be used *before* the operator knows
        // the model's dim, so the embedder is typically constructed
        // with a placeholder (`DEFAULT_OLLAMA_DIM` = 768). Verify
        // that `probe_dim` reports whatever the server returns,
        // regardless of what the embedder was constructed with.
        let server = MockServer::start().await;
        let placeholder_dim = 768;
        let actual_dim = 384;
        let fixture = fixture_embedding(7, actual_dim);

        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture
            })))
            .expect(1)
            .mount(&server)
            .await;

        let e = embedder_for(&server, placeholder_dim);
        let probed = e.probe_dim().await.expect("probe ok");
        assert_eq!(probed, actual_dim);
    }

    #[tokio::test]
    async fn probe_dim_surfaces_empty_response_as_error() {
        let server = MockServer::start().await;
        // Empty `embedding` array — embed_one already catches this
        // and returns Err, so probe_dim should propagate.
        let empty: Vec<f32> = Vec::new();
        Mock::given(method("POST"))
            .and(path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": empty
            })))
            .expect(1)
            .mount(&server)
            .await;

        let e = embedder_for(&server, 768);
        let err = e
            .probe_dim()
            .await
            .expect_err("empty probe response must error");
        let msg = format!("{err}");
        assert!(
            msg.contains("empty"),
            "expected empty-vector error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn empty_batch_yields_empty_output_no_http_calls() {
        let server = MockServer::start().await;
        // Mount no mocks — any unexpected call would 404 + fail the
        // mock server's `.verify()` at drop time.
        let e = embedder_for(&server, 768);
        let out = e.embed_batch(&[]).await.expect("empty batch is ok");
        assert!(out.is_empty());
    }

    /// Real-Ollama smoke. Ignored by default. Run with:
    ///
    /// ```sh
    /// ollama pull nomic-embed-text
    /// cargo test -p solo-storage --ignored ollama_embedder_smoke_real_ollama
    /// ```
    #[tokio::test]
    #[ignore]
    async fn ollama_embedder_smoke_real_ollama() {
        // Reads SOLO_OLLAMA_BASE_URL + SOLO_OLLAMA_EMBED_MODEL so a
        // dev with a non-default Ollama install can still run this
        // by exporting the vars manually. Defaults are the locked
        // roadmap values.
        let base_url =
            std::env::var("SOLO_OLLAMA_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let model = std::env::var("SOLO_OLLAMA_EMBED_MODEL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_string());
        // nomic-embed-text → 768. If the dev picks a different
        // model, they'll need to override here; in 6D solo init
        // will probe automatically.
        let dim = if model == "nomic-embed-text" {
            DEFAULT_OLLAMA_DIM
        } else if model == "mxbai-embed-large" {
            1024
        } else {
            eprintln!(
                "ollama_embedder_smoke_real_ollama: unknown model {model}, \
                 cannot pick dim; skipping. Override `dim` literal in test \
                 source to run."
            );
            return;
        };

        let e = OllamaEmbedder::new(base_url, model, dim).unwrap();
        let out = e
            .embed("the quick brown fox jumps over the lazy dog")
            .await
            .expect("real-Ollama embed");
        assert_eq!(out.dim, dim);
        assert_eq!(out.dtype, EmbeddingDtype::F32);
        let slice = out.as_f32_slice().unwrap();
        // Sanity: not all zeros.
        let mag: f32 = slice.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(mag > 0.0, "embedding should not be all-zero");
    }
}
