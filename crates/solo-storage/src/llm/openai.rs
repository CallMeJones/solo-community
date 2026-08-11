// SPDX-License-Identifier: Apache-2.0

//! OpenAI Chat Completions HTTP backend (`POST /v1/chat/completions`).
//!
//! Sibling to [`super::anthropic::AnthropicClient`]. Implements
//! [`solo_core::LlmClient`] so the Steward (`abstract_cluster`,
//! `detect_contradiction`) works backend-agnostically.
//!
//! ## Wire format
//!
//! OpenAI's chat-completions endpoint expects:
//!
//! ```json
//! {
//!   "model": "gpt-4o-mini",
//!   "max_tokens": 1024,
//!   "messages": [
//!     { "role": "system",    "content": "..." },
//!     { "role": "user",      "content": "..." },
//!     { "role": "assistant", "content": "..." }
//!   ]
//! }
//! ```
//!
//! Solo's three-role `Message` maps 1:1 — unlike Anthropic, OpenAI
//! keeps `system` as a regular array entry, so no split is needed.
//!
//! Response shape we care about:
//!
//! ```json
//! {
//!   "id": "chatcmpl-...",
//!   "model": "...",
//!   "choices": [{
//!     "index": 0,
//!     "message": { "role": "assistant", "content": "..." },
//!     "finish_reason": "stop"
//!   }],
//!   ...
//! }
//! ```
//!
//! We extract `choices[0].message.content`. Tool-call responses
//! (where `content` is `null` and `tool_calls` is populated) are
//! out of scope for v0.3 — the Steward uses prompt-based JSON,
//! not function-calling.
//!
//! ## What's not in v0.3.0
//!
//! Same gaps as [`super::anthropic`]: no streaming, no retries
//! with backoff, no cost / usage tracking, no per-call token
//! cap (max_tokens fixed at construction).
//!
//! Current OpenAI reasoning models use `max_completion_tokens`; legacy and
//! OpenAI-compatible backends often still use `max_tokens`. Solo selects the
//! current parameter for current OpenAI models and retries once with the
//! alternate spelling only when the endpoint explicitly rejects it.
//!
//! ## OpenAI-compatible endpoints (e.g. local LM Studio, Ollama
//! `/v1`-shim, Together, Groq, Mistral)
//!
//! Use [`OpenAIClient::with_base_url`] to point at an alternate
//! host. The wire format is the same; just override the `base_url`
//! and (optionally) `model`. The env builder honours
//! `OPENAI_BASE_URL` for this case.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solo_core::{Error, LlmClient, Message, Result, Role};
use zeroize::Zeroizing;

use super::retry::{
    RetryConfig, exp_backoff_with_jitter, is_retryable_reqwest_err, is_retryable_status,
    parse_retry_after,
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const DEFAULT_MODEL: &str = "gpt-4o-mini";
const DEFAULT_MAX_TOKENS: u32 = 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 60;
const ENV_LLM_ENV_OVERRIDE: &str = "SOLO_LLM_ENV_OVERRIDE";

/// OpenAI Chat Completions HTTP backend.
///
/// Construct via [`OpenAIClient::new`] (model + key) or
/// [`build_openai_client_from_env`] (reads `OPENAI_API_KEY` +
/// `OPENAI_MODEL` + optional `OPENAI_BASE_URL`).
///
/// Cheap to clone (the inner `reqwest::Client` and
/// `Arc<Zeroizing<String>>` are both Arc-shared).
#[derive(Clone)]
pub struct OpenAIClient {
    http: reqwest::Client,
    api_key: Arc<Zeroizing<String>>,
    model: String,
    max_tokens: u32,
    base_url: String,
    retry: RetryConfig,
    json_response_format: bool,
    temperature: Option<f32>,
}

impl OpenAIClient {
    /// Build with the given API key + model. Defaults to OpenAI's
    /// hosted endpoint; use [`OpenAIClient::with_base_url`] for an
    /// OpenAI-compatible service.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .map_err(|e| Error::llm(format!("build reqwest client: {e}")))?;
        Ok(Self {
            http,
            api_key: Arc::new(Zeroizing::new(api_key.into())),
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            base_url: DEFAULT_BASE_URL.to_string(),
            retry: RetryConfig::default(),
            json_response_format: false,
            temperature: None,
        })
    }

    /// Override the per-call `max_tokens` cap. Defaults to 1024.
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Request a JSON object response from OpenAI-compatible backends
    /// that support Chat Completions `response_format`.
    pub fn with_json_response_format(mut self) -> Self {
        self.json_response_format = true;
        self
    }

    /// Override temperature for deterministic extraction tasks.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Borrow the configured model identifier. Used by
    /// [`super::ollama::OllamaClient::wrap`] to construct its
    /// display name (`"ollama:<model>"`) from an existing
    /// `OpenAIClient` without re-parsing the env.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Borrow the configured base URL. Used by the env-builder to
    /// decide whether to wrap this client in `OllamaClient` based on
    /// the URL's port, and useful for any future "show me which
    /// endpoint Solo is calling" surface.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Override the request timeout. Defaults to 60 sec.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| Error::llm(format!("rebuild reqwest client: {e}")))?;
        Ok(self)
    }

    /// Override the base URL. The string should be the API root
    /// without a trailing slash (e.g. `https://api.openai.com/v1`,
    /// `http://localhost:1234/v1`). The chat-completions path is
    /// appended internally.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        let mut s = base_url.into();
        while s.ends_with('/') {
            s.pop();
        }
        self.base_url = s;
        self
    }

    /// Override the retry policy. Defaults to
    /// [`RetryConfig::default`] (3 retries, 500ms base, 10s cap).
    /// Use [`RetryConfig::none`] to disable retries entirely.
    pub fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }

    fn request_body<'a>(
        &'a self,
        messages: &[Message],
        json_response_format: bool,
        temperature_enabled: bool,
        use_max_completion_tokens: bool,
    ) -> OpenAIRequest<'a> {
        OpenAIRequest {
            model: &self.model,
            max_tokens: (!use_max_completion_tokens).then_some(self.max_tokens),
            max_completion_tokens: use_max_completion_tokens.then_some(self.max_tokens),
            messages: messages.iter().map(to_openai_message).collect(),
            temperature: temperature_enabled.then_some(()).and(self.temperature),
            response_format: json_response_format.then_some(OpenAIResponseFormat {
                response_type: "json_object",
            }),
        }
    }

    fn prefers_max_completion_tokens(&self) -> bool {
        self.base_url == DEFAULT_BASE_URL
            && (self.model.starts_with("gpt-5")
                || self.model.starts_with("o1")
                || self.model.starts_with("o3")
                || self.model.starts_with("o4"))
    }
}

#[async_trait]
impl LlmClient for OpenAIClient {
    fn name(&self) -> &str {
        &self.model
    }

    async fn complete(&self, messages: &[Message]) -> Result<Message> {
        let url = format!("{}{}", self.base_url, CHAT_COMPLETIONS_PATH);
        let mut use_json_response_format = self.json_response_format;
        let mut retried_without_json_response_format = false;
        let mut use_temperature = self.temperature.is_some();
        let mut retried_without_temperature = false;
        let mut use_max_completion_tokens = self.prefers_max_completion_tokens();
        let mut retried_with_alternate_token_limit = false;

        // Retry loop — same shape as the Anthropic client. See
        // `super::retry` for the policy definition.
        let mut attempt: u32 = 0;
        loop {
            let body = self.request_body(
                messages,
                use_json_response_format,
                use_temperature,
                use_max_completion_tokens,
            );
            let send_res = self
                .http
                .post(&url)
                .bearer_auth(self.api_key.as_str())
                .header("content-type", "application/json")
                .json(&body)
                .send()
                .await;

            match send_res {
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after_hdr = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let response_bytes = super::bounded_response_bytes(resp, "openai").await?;
                    if status.is_success() {
                        let parsed: OpenAIResponse = serde_json::from_slice(&response_bytes)
                            .map_err(|e| Error::llm(format!("openai response parse: {e}")))?;
                        let text = parsed
                            .choices
                            .into_iter()
                            .next()
                            .and_then(|c| c.message.content)
                            .ok_or_else(|| {
                                Error::llm(
                                    "openai response had no choices[0].message.content".to_string(),
                                )
                            })?;
                        return Ok(Message {
                            role: Role::Assistant,
                            content: text,
                        });
                    }

                    let body_text = String::from_utf8_lossy(&response_bytes);

                    if use_json_response_format
                        && !retried_without_json_response_format
                        && is_response_format_unsupported(status.as_u16(), &body_text)
                    {
                        tracing::warn!(
                            status = %status,
                            "openai-compatible backend rejected response_format; retrying without JSON response mode"
                        );
                        use_json_response_format = false;
                        retried_without_json_response_format = true;
                        continue;
                    }
                    if use_temperature
                        && !retried_without_temperature
                        && is_temperature_unsupported(status.as_u16(), &body_text)
                    {
                        tracing::warn!(
                            status = %status,
                            "openai-compatible backend rejected temperature; retrying without temperature override"
                        );
                        use_temperature = false;
                        retried_without_temperature = true;
                        continue;
                    }
                    if !retried_with_alternate_token_limit
                        && is_token_limit_parameter_unsupported(
                            status.as_u16(),
                            &body_text,
                            use_max_completion_tokens,
                        )
                    {
                        use_max_completion_tokens = !use_max_completion_tokens;
                        retried_with_alternate_token_limit = true;
                        tracing::warn!(
                            status = %status,
                            parameter = if use_max_completion_tokens { "max_completion_tokens" } else { "max_tokens" },
                            "openai-compatible backend rejected the token-limit parameter; retrying with the alternate spelling"
                        );
                        continue;
                    }

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
                            "openai retryable HTTP error; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(Error::llm(format!(
                        "openai HTTP {}: {}",
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
                            "openai retryable network error; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(Error::llm(format!("openai request: {e}")));
                }
            }
        }
    }
}

/// Convenience constructor that reads `OPENAI_API_KEY` from the
/// environment, plus optional `OPENAI_MODEL` and `OPENAI_BASE_URL`.
/// Returns:
///
///   - `Ok(Some(client))` if `OPENAI_API_KEY` is set + non-empty.
///   - `Ok(None)` if the key is unset/empty (caller proceeds
///     without a Steward, or falls back to another backend).
///   - `Err(_)` if the key is set but the HTTP client can't be
///     built (rare — TLS init failure).
///
/// Stderr warning logged when reading from env (matches the
/// `SOLO_PASSPHRASE` / `ANTHROPIC_API_KEY` warning shape) because process
/// environments can be exposed to same-user processes or diagnostic tools.
pub fn build_openai_client_from_env() -> Result<Option<Arc<dyn LlmClient>>> {
    let key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return Ok(None),
    };
    eprintln!(
        "warning: reading OPENAI_API_KEY from the process environment; it may be visible to \
         same-user processes or diagnostic tools. \
         File-based key support is a planned follow-up."
    );
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let api_key_is_ollama_placeholder = is_ollama_placeholder_key(&key);
    let mut client = OpenAIClient::new(key, model)?;
    if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
        if !base.is_empty() {
            client = client.with_base_url(base);
        }
    }

    // If the configured base URL points at Ollama, use the native
    // `/api/chat` client instead of Ollama's OpenAI-compatible shim.
    // The native API honors `keep_alive`, which lets Solo release the
    // large Steward model quickly after background extraction.
    let explicit_ollama_override = std::env::var_os(ENV_LLM_ENV_OVERRIDE).is_some();
    let arc: Arc<dyn LlmClient> = if should_use_native_ollama(
        client.base_url(),
        api_key_is_ollama_placeholder,
        explicit_ollama_override,
    ) {
        Arc::new(super::ollama::OllamaChatClient::new(
            client.base_url(),
            client.model().to_string(),
        )?)
    } else {
        Arc::new(client.with_json_response_format().with_temperature(0.0))
    };
    Ok(Some(arc))
}

fn should_use_native_ollama(
    base_url: &str,
    api_key_is_ollama_placeholder: bool,
    explicit_ollama_override: bool,
) -> bool {
    super::ollama::is_ollama_base_url(base_url)
        || (api_key_is_ollama_placeholder && explicit_ollama_override)
}

fn is_ollama_placeholder_key(api_key: &str) -> bool {
    api_key.trim() == "ollama"
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OpenAIRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenAIResponseFormat>,
}

#[derive(Debug, Serialize)]
struct OpenAIResponseFormat {
    #[serde(rename = "type")]
    response_type: &'static str,
}

#[derive(Debug, Serialize)]
struct OpenAIMessage {
    role: &'static str, // "system" | "user" | "assistant"
    content: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIResponse {
    #[serde(default)]
    choices: Vec<OpenAIChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIChoiceMessage,
}

/// `content` is `Option` because tool-call responses set it to
/// `null`. We treat null + missing both as "no text" → error in
/// `complete()`.
#[derive(Debug, Deserialize)]
struct OpenAIChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

fn to_openai_message(m: &Message) -> OpenAIMessage {
    OpenAIMessage {
        role: match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        content: m.content.clone(),
    }
}

fn is_response_format_unsupported(status: u16, body_text: &str) -> bool {
    if !matches!(status, 400 | 404 | 422) {
        return false;
    }
    let lower = body_text.to_ascii_lowercase();
    let mentions_response_format =
        lower.contains("response_format") || lower.contains("json_object");
    let looks_unsupported = lower.contains("unsupported")
        || lower.contains("not support")
        || lower.contains("not_supported")
        || lower.contains("unrecognized")
        || lower.contains("unknown")
        || lower.contains("invalid")
        || lower.contains("extra_forbidden");
    mentions_response_format && looks_unsupported
}

fn is_temperature_unsupported(status: u16, body_text: &str) -> bool {
    if !matches!(status, 400 | 404 | 422) {
        return false;
    }
    let lower = body_text.to_ascii_lowercase();
    let mentions_temperature = lower.contains("temperature");
    let looks_unsupported = lower.contains("unsupported")
        || lower.contains("not support")
        || lower.contains("not_supported")
        || lower.contains("unrecognized")
        || lower.contains("unknown")
        || lower.contains("invalid")
        || lower.contains("extra_forbidden");
    mentions_temperature && looks_unsupported
}

fn is_token_limit_parameter_unsupported(
    status: u16,
    body_text: &str,
    used_max_completion_tokens: bool,
) -> bool {
    if !matches!(status, 400 | 404 | 422) {
        return false;
    }
    let lower = body_text.to_ascii_lowercase();
    let parameter = if used_max_completion_tokens {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    lower.contains(parameter)
        && (lower.contains("unsupported")
            || lower.contains("not support")
            || lower.contains("not_supported")
            || lower.contains("unrecognized")
            || lower.contains("unknown")
            || lower.contains("extra_forbidden"))
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_mapping_preserves_three_roles() {
        let msgs = vec![
            Message::system("you are helpful"),
            Message::user("hi"),
            Message::assistant("hello"),
        ];
        let mapped: Vec<OpenAIMessage> = msgs.iter().map(to_openai_message).collect();
        assert_eq!(mapped.len(), 3);
        assert_eq!(mapped[0].role, "system");
        assert_eq!(mapped[0].content, "you are helpful");
        assert_eq!(mapped[1].role, "user");
        assert_eq!(mapped[1].content, "hi");
        assert_eq!(mapped[2].role, "assistant");
        assert_eq!(mapped[2].content, "hello");
    }

    #[test]
    fn response_parses_choices_zero_content() {
        let raw = r#"{
            "id": "chatcmpl-1",
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello world" },
                "finish_reason": "stop"
            }]
        }"#;
        let parsed: OpenAIResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.choices.len(), 1);
        assert_eq!(
            parsed.choices[0].message.content.as_deref(),
            Some("hello world")
        );
    }

    #[test]
    fn response_with_tool_call_has_null_content() {
        // OpenAI returns `content: null` when the assistant chose
        // a tool call instead of text. We treat this as "no text"
        // — the Steward uses prompt-based JSON, not tool-calling.
        let raw = r#"{
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{"id":"x","type":"function","function":{"name":"f","arguments":"{}"}}]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let parsed: OpenAIResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.choices[0].message.content.is_none());
    }

    #[test]
    fn response_with_no_choices_yields_error_in_complete_path() {
        let raw = r#"{ "choices": [] }"#;
        let parsed: OpenAIResponse = serde_json::from_str(raw).unwrap();
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content);
        assert!(text.is_none(), "no choices → None for text");
    }

    #[test]
    fn name_returns_configured_model() {
        let c = OpenAIClient::new("dummy", "gpt-test-model").unwrap();
        assert_eq!(c.name(), "gpt-test-model");
    }

    #[test]
    fn with_max_tokens_overrides_default() {
        let c = OpenAIClient::new("dummy", "m")
            .unwrap()
            .with_max_tokens(2048);
        assert_eq!(c.max_tokens, 2048);
    }

    #[test]
    fn json_response_format_serializes_when_enabled() {
        let c = OpenAIClient::new("dummy", "m")
            .unwrap()
            .with_json_response_format()
            .with_temperature(0.0);
        let body = OpenAIRequest {
            model: c.model(),
            max_tokens: Some(c.max_tokens),
            max_completion_tokens: None,
            messages: vec![OpenAIMessage {
                role: "user",
                content: "json please".to_string(),
            }],
            temperature: c.temperature,
            response_format: c.json_response_format.then_some(OpenAIResponseFormat {
                response_type: "json_object",
            }),
        };
        let value = serde_json::to_value(body).unwrap();
        assert_eq!(value["temperature"], 0.0);
        assert_eq!(value["response_format"]["type"], "json_object");
    }

    #[test]
    fn current_openai_models_use_max_completion_tokens() {
        let current = OpenAIClient::new("dummy", "gpt-5.6-terra").unwrap();
        assert!(current.prefers_max_completion_tokens());
        let body = current.request_body(&[Message::user("hi")], true, true, true);
        let value = serde_json::to_value(body).unwrap();
        assert_eq!(value["max_completion_tokens"], DEFAULT_MAX_TOKENS);
        assert!(value.get("max_tokens").is_none());

        let compatible = OpenAIClient::new("dummy", "gpt-5.6-terra")
            .unwrap()
            .with_base_url("https://compatible.example.test/v1");
        assert!(!compatible.prefers_max_completion_tokens());
    }

    #[test]
    fn token_limit_fallback_detection_is_specific() {
        assert!(is_token_limit_parameter_unsupported(
            400,
            r#"{"error":{"message":"Unsupported parameter: max_completion_tokens"}}"#,
            true,
        ));
        assert!(is_token_limit_parameter_unsupported(
            422,
            r#"{"detail":"extra_forbidden: max_tokens"}"#,
            false,
        ));
        assert!(!is_token_limit_parameter_unsupported(
            400,
            r#"{"error":"max_tokens must be below 4096"}"#,
            false,
        ));
        assert!(!is_token_limit_parameter_unsupported(
            500,
            r#"{"error":"max_completion_tokens unavailable"}"#,
            true,
        ));
    }

    #[test]
    fn unsupported_response_format_detection_is_specific() {
        assert!(is_response_format_unsupported(
            400,
            r#"{"error":{"message":"Unsupported parameter: response_format"}}"#
        ));
        assert!(is_response_format_unsupported(
            422,
            r#"{"detail":"extra_forbidden: response_format"}"#
        ));
        assert!(!is_response_format_unsupported(
            500,
            r#"{"error":"response_format temporarily failed"}"#
        ));
        assert!(!is_response_format_unsupported(
            400,
            r#"{"error":"invalid model"}"#
        ));
    }

    #[test]
    fn unsupported_temperature_detection_is_specific() {
        assert!(is_temperature_unsupported(
            400,
            r#"{"error":{"message":"Unsupported parameter: temperature"}}"#
        ));
        assert!(is_temperature_unsupported(
            422,
            r#"{"detail":"extra_forbidden: temperature"}"#
        ));
        assert!(!is_temperature_unsupported(
            500,
            r#"{"error":"temperature temporarily failed"}"#
        ));
        assert!(!is_temperature_unsupported(
            400,
            r#"{"error":"invalid model"}"#
        ));
    }

    #[test]
    fn native_ollama_selection_supports_explicit_custom_port_override() {
        assert!(should_use_native_ollama(
            "http://localhost:11434/v1",
            false,
            false
        ));
        assert!(should_use_native_ollama(
            "http://localhost:31000/v1",
            true,
            true
        ));
        assert!(!should_use_native_ollama(
            "http://localhost:1234/v1",
            true,
            false
        ));
        assert!(!should_use_native_ollama(
            "http://localhost:31000/v1",
            false,
            true
        ));
    }

    #[test]
    fn with_base_url_strips_trailing_slashes() {
        // Trailing slashes are easy to leave in — strip so the
        // joined URL doesn't end up with `//chat/completions`.
        let c = OpenAIClient::new("dummy", "m")
            .unwrap()
            .with_base_url("http://localhost:1234/v1//");
        assert_eq!(c.base_url, "http://localhost:1234/v1");
    }

    #[test]
    fn with_base_url_keeps_clean_url_unchanged() {
        let c = OpenAIClient::new("dummy", "m")
            .unwrap()
            .with_base_url("https://api.openai.com/v1");
        assert_eq!(c.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn build_from_env_returns_none_when_key_missing() {
        // SAFETY: same caveat as the Anthropic builder tests —
        // single-process, env writes aren't synchronised across
        // tests; run in isolation if it flakes.
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
        let r = build_openai_client_from_env().unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn build_from_env_returns_none_when_key_empty() {
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "");
        }
        let r = build_openai_client_from_env().unwrap();
        assert!(r.is_none());
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    /// Real-API smoke. Ignored by default; runs when
    /// `OPENAI_API_KEY` is present and the test is selected via
    /// `cargo test --ignored openai_smoke`.
    #[tokio::test]
    #[ignore]
    async fn openai_smoke_real_api() {
        let Ok(key) = std::env::var("OPENAI_API_KEY") else {
            eprintln!("OPENAI_API_KEY not set; skipping");
            return;
        };
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let mut client = OpenAIClient::new(key, model).unwrap();
        if let Ok(base) = std::env::var("OPENAI_BASE_URL") {
            if !base.is_empty() {
                client = client.with_base_url(base);
            }
        }
        let resp = client
            .complete(&[Message::user("Reply with the single word: ok")])
            .await
            .expect("openai round-trip");
        assert_eq!(resp.role, Role::Assistant);
        assert!(!resp.content.is_empty());
    }
}
