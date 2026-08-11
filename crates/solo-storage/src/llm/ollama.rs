// SPDX-License-Identifier: Apache-2.0

//! [`OllamaClient`] — a thin wrapper around [`super::openai::OpenAIClient`]
//! that surfaces `"ollama:<model>"` as the backend identity instead of
//! the bare model string.
//!
//! Current runtime path: configured Ollama Steward work uses
//! [`OllamaChatClient`] against Ollama's native `/api/chat` endpoint so
//! request-level `keep_alive` is honored. The wrapper notes below
//! describe the legacy OpenAI-shim compatibility path that remains for
//! tests and older call sites.
//!
//! ## Why the wrapper exists
//!
//! Ollama exposes the OpenAI Chat Completions wire format on
//! `localhost:11434/v1`, so Solo's `OpenAIClient` works against it
//! unchanged — set `OPENAI_BASE_URL=http://localhost:11434/v1` and an
//! `OPENAI_MODEL=qwen2.5-coder:7b`, and `complete()` round-trips
//! through Ollama. Operationally fine, but the [`solo_core::LlmClient`]
//! `name()` method (consumed by tracing logs in `daemon.rs` /
//! `common.rs` AND by [`solo_core::Provenance::by`] when persisting
//! abstractions) returns just the model string. An operator reading
//! the log later sees `model="qwen2.5-coder:7b"` with no indication
//! the call hit Ollama instead of hosted OpenAI.
//!
//! [`OllamaClient`] wraps the underlying [`super::openai::OpenAIClient`]
//! and overrides `name()` to return `"ollama:qwen2.5-coder:7b"`. Every
//! other method delegates to the inner client — same retry policy,
//! same wire format, same timeout. The Steward sees no behaviour
//! change; only the identity surface differs.
//!
//! ## How wrapping happens
//!
//! [`super::openai::build_openai_client_from_env`] inspects
//! `OPENAI_BASE_URL` after constructing the `OpenAIClient`. If the
//! URL passes [`is_ollama_base_url`] (today: contains `:11434`, the
//! default Ollama port), the env-builder wraps the client in
//! [`OllamaClient`] before boxing as `Arc<dyn LlmClient>`. Operators
//! who run Ollama on a non-default port lose the prefix but keep
//! identical behaviour — the heuristic is intentionally narrow to
//! avoid mis-tagging non-Ollama backends (LM Studio on `:1234`,
//! anything else on `:11434` would be unusual).
//!
//! ## Provenance impact
//!
//! `Provenance.by` is set from `LlmClient::name()` at write time
//! (`solo_steward::abstraction::abstract_cluster`). Wrapping changes
//! the stored value from `"qwen2.5-coder:7b"` to
//! `"ollama:qwen2.5-coder:7b"` for new abstractions / triples. This
//! is a CHANGE to the data shape but is forward-looking — operators
//! can later filter abstractions by backend ("show me everything
//! Ollama produced") which is more informative than the bare model
//! name. Historical rows produced before this commit landed retain
//! the un-prefixed name.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solo_core::{Error, LlmClient, Message, Result, Role};
use zeroize::Zeroizing;

use super::{bounded_response_bytes, openai::OpenAIClient, redact_secret};

const DEFAULT_BASE_URL: &str = "http://localhost:11434";
const DEFAULT_KEEP_ALIVE: &str = "30s";
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const MAX_ERROR_PREVIEW_CHARS: usize = 2048;
pub const ENV_OLLAMA_LLM_KEEP_ALIVE: &str = "SOLO_OLLAMA_LLM_KEEP_ALIVE";
const ENV_OLLAMA_KEEP_ALIVE: &str = "SOLO_OLLAMA_KEEP_ALIVE";

/// Native Ollama chat client. Unlike Ollama's OpenAI-compatible `/v1`
/// shim, `/api/chat` honors `keep_alive`, so Solo can avoid leaving the
/// large Steward model resident for Ollama's default multi-minute window.
pub struct OllamaChatClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    keep_alive: String,
    display_name: String,
    bearer_token: Option<Zeroizing<String>>,
    structured_outputs: bool,
    allow_format_fallback: bool,
}

impl OllamaChatClient {
    pub fn new(base_url: impl AsRef<str>, model: impl Into<String>) -> Result<Self> {
        let model = model.into();
        let base_url = native_ollama_base_url(base_url.as_ref());
        let parsed = reqwest::Url::parse(&base_url)
            .map_err(|_| Error::invalid_input("Ollama base URL is invalid"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(Error::invalid_input(
                "Ollama base URL must use http or https and include a host",
            ));
        }
        if !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::invalid_input(
                "Ollama base URL must not contain credentials, a query, or a fragment",
            ));
        }
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            // Provider API endpoints are final destinations. Refusing redirects
            // prevents a custom/cloud endpoint from forwarding Solo's bearer
            // token or memory payload to a different location.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::llm(format!("build Ollama reqwest client: {e}")))?;
        Ok(Self {
            http,
            base_url: parsed.as_str().trim_end_matches('/').to_string(),
            keep_alive: keep_alive_from_env(),
            display_name: format!("ollama:{model}"),
            model,
            bearer_token: None,
            structured_outputs: true,
            allow_format_fallback: true,
        })
    }

    pub fn with_keep_alive(mut self, keep_alive: impl AsRef<str>) -> Self {
        self.keep_alive = normalize_keep_alive(keep_alive.as_ref());
        self
    }

    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(Zeroizing::new(token.into()));
        self
    }

    pub fn with_structured_outputs(mut self, enabled: bool) -> Self {
        self.structured_outputs = enabled;
        self
    }

    pub fn with_format_fallback(mut self, enabled: bool) -> Self {
        self.allow_format_fallback = enabled;
        self
    }

    pub fn with_display_prefix(mut self, prefix: &str) -> Self {
        self.display_name = format!("{prefix}:{}", self.model);
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn keep_alive(&self) -> &str {
        &self.keep_alive
    }
}

#[async_trait]
impl LlmClient for OllamaChatClient {
    fn name(&self) -> &str {
        &self.display_name
    }

    async fn complete(&self, messages: &[Message]) -> Result<Message> {
        let response = self.send_with_format_fallback(messages).await?;
        let content = self.parse_chat_response(response).await?;
        if serde_json::from_str::<serde_json::Value>(&content).is_ok() {
            return Ok(Message {
                role: Role::Assistant,
                content,
            });
        }

        // Ollama Cloud currently does not support the native `format`
        // parameter. Keep the prompt-based JSON contract dependable by
        // giving an otherwise-successful non-JSON response one bounded
        // repair attempt. This is intentionally local to the Steward's
        // JSON-only client rather than a general conversational retry.
        let mut repair_messages = messages.to_vec();
        repair_messages.push(Message {
            role: Role::User,
            content: "The previous response was not valid JSON. Return only one valid JSON value that satisfies the original request, with no markdown fences or commentary."
                .to_string(),
        });
        let repaired = self
            .parse_chat_response(self.send_with_format_fallback(&repair_messages).await?)
            .await?;
        serde_json::from_str::<serde_json::Value>(&repaired).map_err(|error| {
            Error::llm(format!(
                "ollama chat returned invalid JSON after one repair attempt: {error}"
            ))
        })?;
        Ok(Message {
            role: Role::Assistant,
            content: repaired,
        })
    }
}

impl OllamaChatClient {
    async fn send_with_format_fallback(&self, messages: &[Message]) -> Result<reqwest::Response> {
        let response = self.send_chat(messages, self.structured_outputs).await?;
        let status = response.status();
        if !status.is_success()
            && self.structured_outputs
            && self.allow_format_fallback
            && matches!(status.as_u16(), 400 | 404 | 422)
        {
            self.send_chat(messages, false).await
        } else {
            Ok(response)
        }
    }

    async fn parse_chat_response(&self, response: reqwest::Response) -> Result<String> {
        let status = response.status();
        let body = bounded_response_bytes(response, "ollama chat").await?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&body);
            let body = self
                .bearer_token
                .as_deref()
                .map_or_else(|| body.to_string(), |token| redact_secret(&body, token));
            return Err(Error::llm(format!(
                "ollama chat HTTP {}: {}",
                status,
                truncate(&body, MAX_ERROR_PREVIEW_CHARS)
            )));
        }

        let parsed: OllamaChatResponse = serde_json::from_slice(&body)
            .map_err(|e| Error::llm(format!("ollama chat response parse: {e}")))?;
        Ok(parsed.message.content)
    }

    async fn send_chat(
        &self,
        messages: &[Message],
        structured_outputs: bool,
    ) -> Result<reqwest::Response> {
        let body = OllamaChatRequest {
            model: &self.model,
            messages: messages.iter().map(to_ollama_message).collect(),
            stream: false,
            response_format: structured_outputs.then_some("json"),
            keep_alive: &self.keep_alive,
            options: OllamaChatOptions { temperature: 0.0 },
        };
        let url = format!("{}/api/chat", self.base_url);
        let mut request = self
            .http
            .post(&url)
            .header("content-type", "application/json")
            .json(&body);
        if let Some(token) = self.bearer_token.as_deref() {
            request = request.bearer_auth(token);
        }
        request
            .send()
            .await
            .map_err(|e| Error::llm(format!("ollama chat request: {e}")))
    }
}

/// Wrap an [`OpenAIClient`] so its `LlmClient::name()` returns
/// `"ollama:<model>"` instead of just `"<model>"`. Every other
/// method delegates to the inner client unchanged.
///
/// Prefer [`OllamaClient::from_arc`] when you already have an
/// `Arc<OpenAIClient>` to share.
pub struct OllamaClient {
    inner: Arc<OpenAIClient>,
    /// Precomputed `"ollama:<model>"` so `name()` can return `&str`
    /// without allocating per call.
    display_name: String,
}

impl OllamaClient {
    /// Wrap a freshly-constructed [`OpenAIClient`].
    pub fn wrap(inner: OpenAIClient) -> Self {
        Self::from_arc(Arc::new(inner))
    }

    /// Wrap an `Arc<OpenAIClient>`. Useful when the same underlying
    /// client is shared by multiple call sites (rare today; the
    /// env-builder always constructs a fresh client).
    pub fn from_arc(inner: Arc<OpenAIClient>) -> Self {
        let display_name = format!("ollama:{}", inner.model());
        Self {
            inner,
            display_name,
        }
    }

    /// Borrow the wrapped client. Useful for tests that want to
    /// assert on the inner state (e.g. base_url) without going
    /// through `LlmClient` indirection.
    pub fn inner(&self) -> &OpenAIClient {
        &self.inner
    }
}

#[async_trait]
impl LlmClient for OllamaClient {
    fn name(&self) -> &str {
        &self.display_name
    }

    async fn complete(&self, messages: &[Message]) -> Result<Message> {
        self.inner.complete(messages).await
    }
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaChatMessage>,
    stream: bool,
    #[serde(rename = "format", skip_serializing_if = "Option::is_none")]
    response_format: Option<&'static str>,
    keep_alive: &'a str,
    options: OllamaChatOptions,
}

#[derive(Debug, Serialize)]
struct OllamaChatOptions {
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct OllamaChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: OllamaChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponseMessage {
    content: String,
}

fn to_ollama_message(message: &Message) -> OllamaChatMessage {
    OllamaChatMessage {
        role: match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        content: message.content.clone(),
    }
}

pub fn native_ollama_base_url(base_url: &str) -> String {
    let mut base = base_url.trim().trim_end_matches('/').to_string();
    if base.is_empty() {
        base = DEFAULT_BASE_URL.to_string();
    }
    if let Some(stripped) = base.strip_suffix("/v1") {
        stripped.to_string()
    } else {
        base
    }
}

fn keep_alive_from_env() -> String {
    std::env::var(ENV_OLLAMA_LLM_KEEP_ALIVE)
        .ok()
        .or_else(|| std::env::var(ENV_OLLAMA_KEEP_ALIVE).ok())
        .map(|value| normalize_keep_alive(&value))
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
        out.push_str("...");
        out
    }
}

/// Heuristic: a base URL pointing at port `11434` (Ollama's default)
/// is treated as Ollama. Used by the env-builder to decide whether
/// to wrap the constructed [`OpenAIClient`] in [`OllamaClient`].
///
/// False negatives: an operator running Ollama on a custom port
/// (e.g. behind a reverse proxy on `:8080/ollama/v1`) would not get
/// the `"ollama:"` prefix. They keep the bare model name in
/// `name()` and provenance — same behaviour as v0.3.7. Acceptable
/// for a cosmetic surface; the cost of false positives (mis-tagging
/// LM Studio etc.) is higher than the cost of missing custom-port
/// Ollama.
///
/// False positives: anything else listening on `:11434` would get
/// mis-tagged as Ollama. Unlikely in practice — the port is
/// well-known and reserved.
pub fn is_ollama_base_url(url: &str) -> bool {
    // Match `:11434` as a substring. Covers
    // `http://localhost:11434/v1`, `http://127.0.0.1:11434`,
    // `http://my-ollama-host:11434/v1`, etc. Doesn't match URLs that
    // include `11434` elsewhere (e.g. as a path component) because
    // the `:` prefix is meaningful.
    url.contains(":11434")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn detects_default_ollama_url() {
        assert!(is_ollama_base_url("http://localhost:11434/v1"));
        assert!(is_ollama_base_url("http://127.0.0.1:11434/v1"));
        assert!(is_ollama_base_url("http://localhost:11434"));
        assert!(is_ollama_base_url(
            "http://my-remote-ollama.example.com:11434/v1"
        ));
    }

    #[test]
    fn rejects_other_endpoints() {
        // Hosted OpenAI
        assert!(!is_ollama_base_url("https://api.openai.com/v1"));
        // LM Studio (different default port)
        assert!(!is_ollama_base_url("http://localhost:1234/v1"));
        // Together / Groq / Mistral hosted
        assert!(!is_ollama_base_url("https://api.together.xyz/v1"));
        assert!(!is_ollama_base_url("https://api.groq.com/openai/v1"));
        // 11434 as path component (unlikely but pathological)
        assert!(!is_ollama_base_url("https://api.example.com/v1/11434"));
    }

    #[test]
    fn native_base_url_strips_openai_v1_suffix() {
        assert_eq!(
            native_ollama_base_url("http://localhost:11434/v1/"),
            "http://localhost:11434"
        );
        assert_eq!(
            native_ollama_base_url("http://localhost:11434"),
            "http://localhost:11434"
        );
    }

    #[test]
    fn native_request_serializes_json_mode_and_keep_alive() {
        let body = OllamaChatRequest {
            model: "qwen2.5-coder:7b",
            messages: vec![OllamaChatMessage {
                role: "user",
                content: "hi".to_string(),
            }],
            stream: false,
            response_format: Some("json"),
            keep_alive: "30s",
            options: OllamaChatOptions { temperature: 0.0 },
        };
        let value = serde_json::to_value(body).unwrap();
        assert_eq!(value["format"], "json");
        assert_eq!(value["keep_alive"], "30s");
        assert_eq!(value["options"]["temperature"], 0.0);
    }

    #[test]
    fn wrap_produces_ollama_prefixed_display_name() {
        let inner = OpenAIClient::new("dummy-key", "qwen2.5-coder:7b").unwrap();
        let wrapped = OllamaClient::wrap(inner);
        assert_eq!(wrapped.name(), "ollama:qwen2.5-coder:7b");
    }

    #[test]
    fn wrap_preserves_inner_for_introspection() {
        let inner = OpenAIClient::new("dummy-key", "phi4:14b")
            .unwrap()
            .with_base_url("http://localhost:11434/v1");
        let wrapped = OllamaClient::wrap(inner);
        assert_eq!(wrapped.inner().model(), "phi4:14b");
        assert_eq!(wrapped.inner().base_url(), "http://localhost:11434/v1");
        assert_eq!(wrapped.name(), "ollama:phi4:14b");
    }

    #[test]
    fn from_arc_constructor_works_for_shared_inner() {
        let inner = Arc::new(OpenAIClient::new("dummy-key", "llama3.3:8b").unwrap());
        let wrapped = OllamaClient::from_arc(inner.clone());
        assert_eq!(wrapped.name(), "ollama:llama3.3:8b");
        // Inner is shared — model() reflects the same source.
        assert_eq!(inner.model(), "llama3.3:8b");
    }

    #[test]
    fn chat_client_rejects_credentials_in_base_url() {
        assert!(OllamaChatClient::new("https://token@example.test", "qwen3:8b").is_err());
        assert!(OllamaChatClient::new("https://example.test?token=secret", "qwen3:8b").is_err());
    }

    #[tokio::test]
    async fn cloud_request_uses_bearer_auth_and_omits_unsupported_format() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(header("authorization", "Bearer cloud-secret"))
            .and(body_json(serde_json::json!({
                "model": "gpt-oss:120b-cloud",
                "messages": [{"role": "user", "content": "extract"}],
                "stream": false,
                "keep_alive": "30s",
                "options": {"temperature": 0.0}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "{\"facts\":[]}"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = OllamaChatClient::new(server.uri(), "gpt-oss:120b-cloud")
            .unwrap()
            .with_bearer_token("cloud-secret")
            .with_structured_outputs(false)
            .with_format_fallback(false)
            .with_display_prefix("ollama-cloud");
        let response = client
            .complete(&[Message {
                role: Role::User,
                content: "extract".to_string(),
            }])
            .await
            .unwrap();

        assert_eq!(response.content, "{\"facts\":[]}");
        assert_eq!(client.name(), "ollama-cloud:gpt-oss:120b-cloud");
    }

    #[tokio::test]
    async fn non_json_cloud_response_gets_one_repair_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(body_json(serde_json::json!({
                "model": "cloud-model",
                "messages": [{"role": "user", "content": "extract"}],
                "stream": false,
                "keep_alive": "30s",
                "options": {"temperature": 0.0}
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "not json"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "messages": [
                    {"role": "user", "content": "extract"},
                    {"role": "user", "content": "The previous response was not valid JSON. Return only one valid JSON value that satisfies the original request, with no markdown fences or commentary."}
                ]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "{\"repaired\":true}"}
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = OllamaChatClient::new(server.uri(), "cloud-model")
            .unwrap()
            .with_structured_outputs(false)
            .with_format_fallback(false);
        let response = client
            .complete(&[Message {
                role: Role::User,
                content: "extract".to_string(),
            }])
            .await
            .unwrap();

        assert_eq!(response.content, "{\"repaired\":true}");
    }

    #[tokio::test]
    async fn oversized_chat_response_is_rejected_before_json_parsing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
                b'x';
                super::super::MAX_LLM_RESPONSE_BYTES
                    + 1
            ]))
            .expect(1)
            .mount(&server)
            .await;

        let client = OllamaChatClient::new(server.uri(), "cloud-model")
            .unwrap()
            .with_structured_outputs(false)
            .with_format_fallback(false);
        let error = client
            .complete(&[Message {
                role: Role::User,
                content: "extract".to_string(),
            }])
            .await
            .unwrap_err();

        assert!(error.to_string().contains("response exceeded"));
    }

    #[tokio::test]
    async fn chat_client_does_not_follow_redirects() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/redirect-target", server.uri())),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/redirect-target"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "{\"unexpected\":true}"}
            })))
            .expect(0)
            .mount(&server)
            .await;

        let client = OllamaChatClient::new(server.uri(), "cloud-model")
            .unwrap()
            .with_bearer_token("must-not-be-forwarded")
            .with_structured_outputs(false)
            .with_format_fallback(false);
        let error = client
            .complete(&[Message::user("extract")])
            .await
            .unwrap_err();

        assert!(error.to_string().contains("HTTP 307"));
    }

    #[tokio::test]
    async fn cloud_error_does_not_echo_the_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(401).set_body_string("rejected cloud-secret"))
            .expect(1)
            .mount(&server)
            .await;

        let client = OllamaChatClient::new(server.uri(), "cloud-model")
            .unwrap()
            .with_bearer_token("cloud-secret")
            .with_structured_outputs(false)
            .with_format_fallback(false);
        let error = client
            .complete(&[Message::user("extract")])
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("[REDACTED]"));
        assert!(!error.contains("cloud-secret"));
    }
}
