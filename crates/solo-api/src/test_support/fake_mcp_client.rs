// SPDX-License-Identifier: Apache-2.0

//! [`FakeMcpClient`] — an in-process mock of the MCP-client `sampling/
//! createMessage` RPC for Solo's `SamplingLlmClient` tests.
//!
//! ## Why an in-process fake instead of wiremock
//!
//! v0.8.0 P3 introduced the fake-IdP pattern (banked lesson #26):
//! spin up a `wiremock` server, point the JWKS validator at it, and
//! drive the test via HTTP. That works because OIDC validation IS
//! HTTP-shaped — the validator opens a TCP socket regardless of who
//! the server is.
//!
//! MCP sampling is not HTTP-shaped. The rmcp client/server pair
//! exchanges JSON-RPC messages over a transport (stdio in v0.8.x,
//! HTTP/SSE in v0.9.x). The server's `peer.create_message(params)`
//! call routes through rmcp's internal request dispatch — there's no
//! HTTP socket to point at.
//!
//! `rmcp::Peer<RoleServer>` has private fields and is constructed
//! inside rmcp's transport setup. We can't make a fake of it directly.
//! Solution: the production [`super::super::llm::sampling`] module
//! exposes a tiny [`SamplingClient`](super::super::llm::sampling::
//! SamplingClient) trait that abstracts `peer.create_message`; the
//! production impl wraps `Arc<Peer<RoleServer>>`, the test impl is
//! [`FakeMcpClient`].
//!
//! Per the locked plan §3 Decision 5: roll our own fixture (option a)
//! rather than rely on rmcp internals (option b) or forge JSON-RPC
//! framing (option c). Same shape as v0.8.0 P3's fake-IdP — a tiny
//! struct with controllable responses that lives under `test_support/`.
//!
//! ## Configurable behaviors
//!
//! Per the plan's spot-test list, the fake must cover at least:
//!
//! * **Happy path**: returns canned assistant text.
//! * **Client refusal**: simulates "user did not approve the sampling
//!   request" — surfaces as `FakeSamplingError::Refused`.
//! * **Timeout**: sleeps past a caller-configurable timeout to drive
//!   the `SamplingLlmClient`'s timeout path.
//! * **Malformed response**: simulates an assistant message whose
//!   content has zero text blocks (the client must still return a
//!   structured error, not panic).
//! * **Reconfiguration**: tests can swap the response between calls
//!   to verify per-call audit isolation.
//!
//! All behaviors are pinned by tests in this module.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{CreateMessageRequestParams, CreateMessageResult, Role, SamplingMessage};

/// In-test errors emitted by [`FakeMcpClient`]. Maps to the real
/// `rmcp::service::ServiceError` shape from outside: the production
/// [`super::super::llm::sampling::SamplingClient`] trait erases the
/// concrete error type so the fake can use its own.
#[derive(Debug, Clone)]
pub enum FakeSamplingError {
    /// The client refused the sampling request — user did not approve,
    /// or the client doesn't support sampling at all. Maps to the
    /// `Forbidden`-class audit row in `SamplingLlmClient::complete`.
    Refused { reason: String },
    /// Transport / network error.
    Transport { message: String },
    /// The response carried no text content. Drives the
    /// `SamplingLlmClient::complete` malformed-response path.
    MalformedResponse { message: String },
}

impl std::fmt::Display for FakeSamplingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused { reason } => write!(f, "client refused: {reason}"),
            Self::Transport { message } => write!(f, "transport: {message}"),
            Self::MalformedResponse { message } => {
                write!(f, "malformed response: {message}")
            }
        }
    }
}

impl std::error::Error for FakeSamplingError {}

/// Per-call behavior the fake should produce.
#[derive(Debug, Clone)]
pub enum FakeResponse {
    /// Return a `CreateMessageResult` whose assistant message contains
    /// `text` as the (single) text content block.
    Text { text: String, model: String },
    /// Sleep for `duration` before resolving with `Text`. Drives the
    /// caller's timeout path when the caller's deadline is shorter.
    Slow {
        text: String,
        model: String,
        duration: Duration,
    },
    /// Return a `CreateMessageResult` whose assistant message contains
    /// no text content. Drives the malformed-response error path.
    EmptyContent,
    /// Return the named error from `create_message`.
    Error(FakeSamplingError),
}

impl FakeResponse {
    /// Convenience constructor — the most common case (canned assistant
    /// text, mock model name).
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            model: "fake-claude".to_string(),
        }
    }

    /// Convenience: a refusal response.
    pub fn refused(reason: impl Into<String>) -> Self {
        Self::Error(FakeSamplingError::Refused {
            reason: reason.into(),
        })
    }

    /// Convenience: a slow response (for timeout tests).
    pub fn slow(text: impl Into<String>, duration: Duration) -> Self {
        Self::Slow {
            text: text.into(),
            model: "fake-claude".to_string(),
            duration,
        }
    }
}

/// Minimal in-process mock of the MCP client side of `sampling/
/// createMessage`.
///
/// Construct with [`FakeMcpClient::new`] (defaults to a single canned
/// response) and reconfigure mid-test via
/// [`FakeMcpClient::respond_with`], [`FakeMcpClient::respond_each`], or
/// [`FakeMcpClient::reject_with`].
///
/// Records every request via [`FakeMcpClient::record_requests`] so tests
/// can assert on the wire shape Steward asked for.
///
/// Cheap to clone — every field is `Arc<Mutex<_>>`. The same handle can
/// be wired into `SamplingLlmClient` and into the test's assertions.
#[derive(Clone, Default)]
pub struct FakeMcpClient {
    /// The queue of responses to emit. `respond_with(R)` sets a
    /// single-element vec; `respond_each(Vec<R>)` sets a multi-call
    /// sequence. Calls past the last queued response cycle the last
    /// element (so tests don't have to count exactly).
    responses: Arc<Mutex<Vec<FakeResponse>>>,
    /// Index of the next response to emit. Wraps to the last element
    /// of `responses` once it runs out.
    next_idx: Arc<Mutex<usize>>,
    /// Records every request received. Tests can read this with
    /// [`Self::record_requests`].
    requests: Arc<Mutex<Vec<CreateMessageRequestParams>>>,
}

impl FakeMcpClient {
    /// Build a fake that returns `response` for every call until
    /// reconfigured.
    pub fn new(response: FakeResponse) -> Self {
        Self {
            responses: Arc::new(Mutex::new(vec![response])),
            next_idx: Arc::new(Mutex::new(0)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// One-shot canned response. Subsequent calls (after the first)
    /// repeat `response` since the queue has only one element.
    pub fn respond_with(&self, response: FakeResponse) {
        *self.responses.lock().expect("FakeMcpClient mutex poisoned") = vec![response];
        *self.next_idx.lock().expect("FakeMcpClient mutex poisoned") = 0;
    }

    /// Multi-call sequence. Calls past the end of `responses` repeat
    /// the last entry (so a 2-element sequence handles 2-or-more calls
    /// without panicking the test).
    pub fn respond_each(&self, responses: Vec<FakeResponse>) {
        assert!(
            !responses.is_empty(),
            "FakeMcpClient::respond_each: pass at least one response"
        );
        *self.responses.lock().expect("FakeMcpClient mutex poisoned") = responses;
        *self.next_idx.lock().expect("FakeMcpClient mutex poisoned") = 0;
    }

    /// Configure the fake to reject every call until reconfigured.
    pub fn reject_with(&self, reason: impl Into<String>) {
        self.respond_with(FakeResponse::refused(reason));
    }

    /// Snapshot of every `create_message` request received so far.
    pub fn record_requests(&self) -> Vec<CreateMessageRequestParams> {
        self.requests
            .lock()
            .expect("FakeMcpClient mutex poisoned")
            .clone()
    }

    /// Returns the next response, advancing the cursor (with wrap-to-
    /// last behaviour).
    fn next_response(&self) -> FakeResponse {
        let responses = self.responses.lock().expect("FakeMcpClient mutex poisoned");
        if responses.is_empty() {
            // Fallback: empty queue (shouldn't happen since `new`
            // seeds one element); produce an error so the test fails
            // loudly.
            return FakeResponse::Error(FakeSamplingError::Transport {
                message: "FakeMcpClient: no response configured".to_string(),
            });
        }
        let mut idx = self.next_idx.lock().expect("FakeMcpClient mutex poisoned");
        let r = responses[(*idx).min(responses.len() - 1)].clone();
        if *idx < responses.len() - 1 {
            *idx += 1;
        }
        r
    }
}

/// Bridge between the `FakeMcpClient` and the production
/// `SamplingClient` trait. The trait's full definition lives next to
/// `SamplingLlmClient` in `crates/solo-api/src/llm/sampling.rs`; we
/// implement it here so the fake's only dep is on the trait's shape.
#[async_trait]
impl crate::llm::sampling::SamplingClient for FakeMcpClient {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
    ) -> Result<CreateMessageResult, crate::llm::sampling::SamplingError> {
        self.requests
            .lock()
            .expect("FakeMcpClient mutex poisoned")
            .push(params.clone());
        match self.next_response() {
            FakeResponse::Text { text, model } => Ok(CreateMessageResult::new(
                SamplingMessage::assistant_text(text),
                model,
            )
            .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN)),
            FakeResponse::Slow {
                text,
                model,
                duration,
            } => {
                tokio::time::sleep(duration).await;
                Ok(
                    CreateMessageResult::new(SamplingMessage::assistant_text(text), model)
                        .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN),
                )
            }
            FakeResponse::EmptyContent => {
                // Build a result with an assistant message whose
                // content vec is empty. `SamplingMessage::new_multiple`
                // with an empty vec is the canonical "no text blocks"
                // shape; `SamplingLlmClient::extract_text` MUST handle
                // it as `MalformedResponse`.
                Ok(CreateMessageResult::new(
                    SamplingMessage::new_multiple(Role::Assistant, Vec::new()),
                    "fake-claude".to_string(),
                )
                .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN))
            }
            FakeResponse::Error(err) => Err(crate::llm::sampling::SamplingError::Fake(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::sampling::SamplingClient;

    fn req() -> CreateMessageRequestParams {
        CreateMessageRequestParams::new(vec![SamplingMessage::user_text("hi")], 512)
    }

    /// `new(FakeResponse::text("ok"))` returns the canned text from
    /// `create_message`.
    #[tokio::test]
    async fn happy_path_returns_canned_text() {
        let fake = FakeMcpClient::new(FakeResponse::text("hello world"));
        let result = fake.create_message(req()).await.expect("ok");
        let content = result.message.content.into_vec();
        let text = content[0].as_text().expect("text content").text.clone();
        assert_eq!(text, "hello world");
        assert_eq!(result.model, "fake-claude");
    }

    /// `respond_with` replaces the queued response.
    #[tokio::test]
    async fn respond_with_replaces_response() {
        let fake = FakeMcpClient::new(FakeResponse::text("first"));
        fake.respond_with(FakeResponse::text("second"));
        let result = fake.create_message(req()).await.expect("ok");
        let content = result.message.content.into_vec();
        assert_eq!(content[0].as_text().unwrap().text, "second");
    }

    /// `respond_each` walks through responses; calls past the queue end
    /// repeat the last entry (no panic).
    #[tokio::test]
    async fn respond_each_sequences_and_wraps_to_last() {
        let fake = FakeMcpClient::default();
        fake.respond_each(vec![FakeResponse::text("a"), FakeResponse::text("b")]);
        let r1 = fake.create_message(req()).await.expect("ok");
        let r2 = fake.create_message(req()).await.expect("ok");
        let r3 = fake.create_message(req()).await.expect("ok"); // wraps
        assert_eq!(
            r1.message.content.into_vec()[0].as_text().unwrap().text,
            "a"
        );
        assert_eq!(
            r2.message.content.into_vec()[0].as_text().unwrap().text,
            "b"
        );
        assert_eq!(
            r3.message.content.into_vec()[0].as_text().unwrap().text,
            "b"
        );
    }

    /// `reject_with` simulates user-refusal — the call returns
    /// `SamplingError::Fake(Refused)` and the audit caller maps to
    /// `result = "forbidden"`.
    #[tokio::test]
    async fn reject_with_returns_refused_error() {
        let fake = FakeMcpClient::new(FakeResponse::text("won't see this"));
        fake.reject_with("user dismissed");
        let err = fake.create_message(req()).await.unwrap_err();
        match err {
            crate::llm::sampling::SamplingError::Fake(FakeSamplingError::Refused { reason }) => {
                assert_eq!(reason, "user dismissed");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    /// `EmptyContent` produces a result with zero content blocks.
    /// `SamplingLlmClient::extract_text` must surface this as a
    /// malformed-response error.
    #[tokio::test]
    async fn empty_content_returns_zero_content_blocks() {
        let fake = FakeMcpClient::new(FakeResponse::EmptyContent);
        let result = fake.create_message(req()).await.expect("ok");
        let content = result.message.content.into_vec();
        assert!(content.is_empty(), "EmptyContent must produce zero blocks");
    }

    /// `Slow` actually sleeps. The duration is observable to the caller
    /// — drives the timeout test path in `SamplingLlmClient`.
    #[tokio::test]
    async fn slow_response_actually_sleeps() {
        let fake = FakeMcpClient::new(FakeResponse::slow("late", Duration::from_millis(40)));
        let start = std::time::Instant::now();
        let _ = fake.create_message(req()).await.expect("ok");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(35),
            "slow response should sleep at least ~40ms; observed {elapsed:?}"
        );
    }

    /// `record_requests` collects every `create_message` arg.
    #[tokio::test]
    async fn record_requests_captures_each_call() {
        let fake = FakeMcpClient::new(FakeResponse::text("ok"));
        let _ = fake.create_message(req()).await;
        let mut p2 = req();
        p2.max_tokens = 1024;
        let _ = fake.create_message(p2.clone()).await;
        let recorded = fake.record_requests();
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].max_tokens, 512);
        assert_eq!(recorded[1].max_tokens, 1024);
    }

    /// Default `FakeMcpClient` (no canned response yet) errors loudly
    /// rather than panicking — so a test author who forgot to call
    /// `respond_with` sees a clear failure mode.
    #[tokio::test]
    async fn default_with_no_response_returns_transport_error() {
        let fake = FakeMcpClient::default();
        // Override to truly-empty queue (default seeds `responses=
        // vec![]` via Default of `Vec`); confirms the empty-queue
        // fallback path.
        *fake.responses.lock().expect("FakeMcpClient mutex poisoned") = Vec::new();
        let err = fake.create_message(req()).await.unwrap_err();
        match err {
            crate::llm::sampling::SamplingError::Fake(FakeSamplingError::Transport { .. }) => {}
            other => panic!("expected Transport error, got {other:?}"),
        }
    }
}
