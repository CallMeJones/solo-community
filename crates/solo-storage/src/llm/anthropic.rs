// SPDX-License-Identifier: Apache-2.0

//! Anthropic Claude HTTP backend (`POST /v1/messages`).
//!
//! Implements [`solo_core::LlmClient`]. Drop-in replacement for the
//! stub used in tests — every higher-level Steward operation
//! (`abstract_cluster`, `detect_contradiction`) works against this
//! backend without changes.
//!
//! ## Wire format
//!
//! Anthropic's `messages` endpoint expects:
//!
//! ```json
//! {
//!   "model": "claude-3-5-sonnet-20241022",
//!   "max_tokens": 1024,
//!   "system": "<optional system prompt>",
//!   "messages": [
//!     { "role": "user",      "content": "..." },
//!     { "role": "assistant", "content": "..." },
//!     ...
//!   ]
//! }
//! ```
//!
//! Solo's [`Message`] uses three roles (`System`, `User`, `Assistant`).
//! Anthropic's `messages` array only supports `user` / `assistant`;
//! the system prompt is its own top-level field. We split on send.
//!
//! Response shape we care about:
//!
//! ```json
//! {
//!   "id": "msg_...",
//!   "model": "...",
//!   "role": "assistant",
//!   "content": [{ "type": "text", "text": "..." }],
//!   "stop_reason": "end_turn",
//!   ...
//! }
//! ```
//!
//! We extract the first `text` block. Tool-use blocks (Anthropic's
//! native function-calling) are out of scope for v0.2.0 — the
//! Steward uses prompt-based JSON output, not Anthropic's
//! tool-calling.
//!
//! ## What's not in v0.2.0
//!
//! - **Streaming.** Steward calls are batch jobs; streaming buys
//!   little for the consolidation pipeline.
//! - **Retries with backoff.** A single network blip fails the
//!   pair (logged + skipped at the `handle_consolidate` level).
//!   Future: wrap with `tower-retry` or hand-roll exponential
//!   backoff on 429 / 5xx.
//! - **Token budgeting.** `max_tokens` is fixed at construction
//!   time. The Steward's `abstraction_max_tokens` config exists
//!   but isn't yet plumbed through (the trait `LlmClient::complete`
//!   doesn't expose a per-call cap).
//! - **Cost / usage tracking.** Anthropic returns `usage:
//!   { input_tokens, output_tokens }` in the response. Logging
//!   that for ops is a future polish pass.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use solo_core::{Error, LlmClient, Message, Result, Role};
use zeroize::Zeroizing;

use super::redact_secret;
use super::retry::{
    RetryConfig, exp_backoff_with_jitter, is_retryable_reqwest_err, is_retryable_status,
    parse_retry_after,
};

const ANTHROPIC_API_VERSION: &str = "2023-06-01";
const ANTHROPIC_MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_MODEL: &str = "claude-3-5-sonnet-20241022";
const DEFAULT_MAX_TOKENS: u32 = 1024;
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Anthropic Claude HTTP backend.
///
/// Construct via [`AnthropicClient::new`] (model + key) or
/// [`build_anthropic_client_from_env`] (reads `ANTHROPIC_API_KEY` +
/// `ANTHROPIC_MODEL` env vars).
///
/// Cheap to clone (the inner `reqwest::Client` and `Arc<Zeroizing<String>>`
/// are both Arc-shared).
#[derive(Clone)]
pub struct AnthropicClient {
    http: reqwest::Client,
    api_key: Arc<Zeroizing<String>>,
    model: String,
    max_tokens: u32,
    retry: RetryConfig,
}

impl AnthropicClient {
    /// Build with the given API key + model. The key is wrapped in
    /// `zeroize::Zeroizing` so its in-memory buffer wipes on drop —
    /// defense against process-memory inspection.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::llm(format!("build reqwest client: {e}")))?;
        Ok(Self {
            http,
            api_key: Arc::new(Zeroizing::new(api_key.into())),
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            retry: RetryConfig::default(),
        })
    }

    /// Override the per-call `max_tokens` cap. Defaults to 1024.
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    /// Override the request timeout. Defaults to 60 sec.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| Error::llm(format!("rebuild reqwest client: {e}")))?;
        Ok(self)
    }

    /// Override the retry policy. Defaults to
    /// [`RetryConfig::default`] (3 retries, 500ms base, 10s cap).
    /// Use [`RetryConfig::none`] to disable retries entirely.
    pub fn with_retry_config(mut self, retry: RetryConfig) -> Self {
        self.retry = retry;
        self
    }
}

#[async_trait]
impl LlmClient for AnthropicClient {
    fn name(&self) -> &str {
        &self.model
    }

    async fn complete(&self, messages: &[Message]) -> Result<Message> {
        let (system, msgs) = split_system_and_msgs(messages);
        let body = AnthropicRequest {
            model: &self.model,
            max_tokens: self.max_tokens,
            system,
            messages: msgs,
        };

        // Retry loop: at most `max_retries + 1` total attempts.
        // Retryable conditions: 429, 5xx, connection / timeout
        // errors. Each retryable failure sleeps for either the
        // server's `Retry-After` header (capped at max_delay) or
        // exponential backoff with full jitter.
        let mut attempt: u32 = 0;
        loop {
            let send_res = self
                .http
                .post(ANTHROPIC_MESSAGES_URL)
                .header("x-api-key", self.api_key.as_str())
                .header("anthropic-version", ANTHROPIC_API_VERSION)
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
                    let response_bytes = super::bounded_response_bytes(resp, "anthropic").await?;
                    if status.is_success() {
                        let parsed: AnthropicResponse = serde_json::from_slice(&response_bytes)
                            .map_err(|e| Error::llm(format!("anthropic response parse: {e}")))?;
                        let text = parsed
                            .content
                            .into_iter()
                            .find_map(|block| match block {
                                ContentBlock::Text { text } => Some(text),
                                ContentBlock::Other => None,
                            })
                            .ok_or_else(|| {
                                Error::llm(
                                    "anthropic response had no text content block".to_string(),
                                )
                            })?;
                        return Ok(Message {
                            role: Role::Assistant,
                            content: text,
                        });
                    }

                    // Non-2xx. Decide retry.
                    // Consume the response body for diagnostics
                    // (ignored on retry, surfaced on terminal).
                    let body_text = String::from_utf8_lossy(&response_bytes);

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
                            "anthropic retryable HTTP error; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    let safe_body = redact_secret(&body_text, self.api_key.as_str());
                    return Err(Error::llm(format!(
                        "anthropic HTTP {}: {}",
                        status,
                        truncate(&safe_body, 500)
                    )));
                }
                Err(e) => {
                    if attempt < self.retry.max_retries && is_retryable_reqwest_err(&e) {
                        let delay = exp_backoff_with_jitter(attempt + 1, &self.retry);
                        tracing::warn!(
                            attempt = attempt + 1,
                            error = %e,
                            delay_ms = delay.as_millis() as u64,
                            "anthropic retryable network error; backing off"
                        );
                        tokio::time::sleep(delay).await;
                        attempt += 1;
                        continue;
                    }
                    return Err(Error::llm(format!("anthropic request: {e}")));
                }
            }
        }
    }
}

/// Convenience constructor that reads `ANTHROPIC_API_KEY` from the
/// environment and (optionally) `ANTHROPIC_MODEL`. Returns:
///
///   - `Ok(Some(client))` if both succeed.
///   - `Ok(None)` if `ANTHROPIC_API_KEY` is unset or empty (caller
///     proceeds without a Steward).
///   - `Err(_)` if the key is set but the HTTP client can't be
///     built (rare — TLS init failure).
///
/// Stderr warning logged when reading from env (matches the
/// `SOLO_PASSPHRASE` warning shape) because process environments can
/// be exposed to same-user processes or diagnostic tools.
pub fn build_anthropic_client_from_env() -> Result<Option<Arc<dyn LlmClient>>> {
    let key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return Ok(None),
    };
    eprintln!(
        "warning: reading ANTHROPIC_API_KEY from the process environment; it may be visible to \
         same-user processes or diagnostic tools. \
         File-based key support is a planned follow-up."
    );
    let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let client = AnthropicClient::new(key, model)?;
    Ok(Some(Arc::new(client)))
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<AnthropicMessage>,
}

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: &'static str, // "user" or "assistant"
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

/// Content blocks in an Anthropic response. v0.2.0 only handles
/// text; tool-use blocks are skipped (logged via the
/// "no text content block" error if no text block is present).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

/// Convert Solo's three-role `Message[]` to Anthropic's two-role
/// shape:
///
///   - All `Role::System` messages are concatenated (newline-joined)
///     into a single `system` string.
///   - `User` / `Assistant` messages keep their role + go into
///     `messages` array, in order.
///
/// Multiple system messages are uncommon in our prompts (the
/// Steward sends exactly one) but we handle the general case
/// because nothing in the trait forbids it.
fn split_system_and_msgs(messages: &[Message]) -> (Option<String>, Vec<AnthropicMessage>) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut msgs: Vec<AnthropicMessage> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => system_parts.push(&m.content),
            Role::User => msgs.push(AnthropicMessage {
                role: "user",
                content: m.content.clone(),
            }),
            Role::Assistant => msgs.push(AnthropicMessage {
                role: "assistant",
                content: m.content.clone(),
            }),
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, msgs)
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
    fn split_extracts_single_system_prompt() {
        let msgs = vec![
            Message::system("you are a helpful assistant"),
            Message::user("hello"),
        ];
        let (system, anth) = split_system_and_msgs(&msgs);
        assert_eq!(system.as_deref(), Some("you are a helpful assistant"));
        assert_eq!(anth.len(), 1);
        assert_eq!(anth[0].role, "user");
        assert_eq!(anth[0].content, "hello");
    }

    #[test]
    fn split_concatenates_multiple_system_messages() {
        let msgs = vec![
            Message::system("rule 1"),
            Message::user("hi"),
            Message::system("rule 2"),
            Message::assistant("response"),
        ];
        let (system, anth) = split_system_and_msgs(&msgs);
        // System messages concatenated with blank lines.
        let s = system.expect("system");
        assert!(s.contains("rule 1"));
        assert!(s.contains("rule 2"));
        // user + assistant preserved in order.
        assert_eq!(anth.len(), 2);
        assert_eq!(anth[0].role, "user");
        assert_eq!(anth[0].content, "hi");
        assert_eq!(anth[1].role, "assistant");
        assert_eq!(anth[1].content, "response");
    }

    #[test]
    fn split_handles_no_system_message() {
        let msgs = vec![Message::user("hi"), Message::assistant("hello")];
        let (system, anth) = split_system_and_msgs(&msgs);
        assert!(system.is_none());
        assert_eq!(anth.len(), 2);
    }

    #[test]
    fn response_parses_text_block() {
        let raw = r#"{
            "id": "msg_01abc",
            "model": "claude-3-5-sonnet-20241022",
            "role": "assistant",
            "content": [{ "type": "text", "text": "hello world" }],
            "stop_reason": "end_turn"
        }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.content.len(), 1);
        match &parsed.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello world"),
            _ => panic!("expected Text block"),
        }
    }

    #[test]
    fn response_skips_unknown_block_types() {
        // Anthropic could return tool_use, image, etc. blocks.
        // Our parser keeps them as Other and lets `complete()`'s
        // find_map skip until it hits the first text.
        let raw = r#"{
            "content": [
                { "type": "tool_use", "id": "toolu_xyz", "name": "calc", "input": {} },
                { "type": "text", "text": "the answer" }
            ]
        }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.content.len(), 2);
        let text_only: Vec<&str> = parsed
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text_only, vec!["the answer"]);
    }

    #[test]
    fn response_with_no_text_block_yields_error_in_complete_path() {
        let raw = r#"{ "content": [{ "type": "tool_use", "id": "x", "name": "y", "input": {} }] }"#;
        let parsed: AnthropicResponse = serde_json::from_str(raw).unwrap();
        // Mimic the find_map in complete().
        let text = parsed.content.into_iter().find_map(|block| match block {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        });
        assert!(text.is_none(), "no text block expected → None for find_map");
    }

    #[test]
    fn name_returns_configured_model() {
        let c = AnthropicClient::new("dummy", "claude-test-model").unwrap();
        assert_eq!(c.name(), "claude-test-model");
    }

    #[test]
    fn with_max_tokens_overrides_default() {
        let c = AnthropicClient::new("dummy", "m")
            .unwrap()
            .with_max_tokens(2048);
        assert_eq!(c.max_tokens, 2048);
    }

    #[test]
    fn build_from_env_returns_none_when_key_missing() {
        // v0.9.1 P1 Fix 2: serialize on the workspace-shared
        // `LLM_ENV_LOCK` so we don't race with
        // `solo_storage::init::tests` (same test binary) when both
        // touch `ANTHROPIC_API_KEY`. Poison-recovery via
        // `unwrap_or_else(into_inner)` keeps a panicking sibling test
        // from sinking this suite.
        let _lock = crate::test_support::LLM_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: lock held; no other thread is racing on these vars.
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
        let r = build_anthropic_client_from_env().unwrap();
        assert!(r.is_none());
    }

    #[test]
    fn build_from_env_returns_none_when_key_empty() {
        // v0.9.1 P1 Fix 2: shared lock — see sibling test above.
        let _lock = crate::test_support::LLM_ENV_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: lock held; no other thread is racing on these vars.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", "");
        }
        let r = build_anthropic_client_from_env().unwrap();
        assert!(r.is_none());
        unsafe {
            std::env::remove_var("ANTHROPIC_API_KEY");
        }
    }

    /// Deliberately not gated on a network feature — runs locally
    /// only when `ANTHROPIC_API_KEY` is in env. Marked `#[ignore]`
    /// so the standard `cargo test` loop skips it; run via
    /// `cargo test --ignored anthropic_smoke` to actually hit the
    /// API.
    #[tokio::test]
    #[ignore]
    async fn anthropic_smoke_real_api() {
        let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
            eprintln!("ANTHROPIC_API_KEY not set; skipping");
            return;
        };
        let model = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let client = AnthropicClient::new(key, model).unwrap();
        let resp = client
            .complete(&[Message::user("Reply with the single word: ok")])
            .await
            .expect("anthropic round-trip");
        assert_eq!(resp.role, Role::Assistant);
        assert!(!resp.content.is_empty());
    }
}
