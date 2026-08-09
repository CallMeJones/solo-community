// SPDX-License-Identifier: Apache-2.0

//! Test fixtures shared across `solo-steward`'s own tests + downstream
//! crates that exercise the steward in integration tests.
//!
//! Mirrors `solo_storage::test_support`: gated behind
//! `#[cfg(any(test, feature = "test-support"))]` so it never compiles
//! into release builds. Downstream crates that need the stub LlmClient
//! enable the feature in their `[dev-dependencies]`:
//!
//! ```toml
//! [dev-dependencies]
//! solo-steward = { workspace = true, features = ["test-support"] }
//! ```

#![cfg(any(test, feature = "test-support"))]

use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use solo_core::{LlmClient, Message, Result, Role};

/// A deterministic, no-network `LlmClient` for tests of code paths
/// that consume an `Arc<dyn LlmClient>`.
///
/// Default behavior: every `complete()` call returns a fixed,
/// JSON-shaped assistant message (`{"content": "(stub abstraction)",
/// "triples": [], "confidence": 0.5}`) — parseable by
/// `Steward::abstract_cluster` without ad-hoc setup.
///
/// For tests that need to assert on specific outputs, push canned
/// responses with [`Self::push_canned`]; calls to `complete()` then
/// drain the queue in FIFO order. When the queue is empty, the
/// default response is returned again.
///
/// Every prompt the stub sees is recorded — see [`Self::prompts`] —
/// so tests can assert on what the steward asked.
pub struct StubLlmClient {
    name: String,
    /// Whether `LlmClient::is_real_llm()` should report `true`. The
    /// stub is, of course, a stub — this exists to let integration
    /// tests that exercise LLM-gated code paths (e.g. the writer's
    /// contradiction sweep, which early-returns when `!has_llm()`)
    /// flip the trait answer without swapping in a real backend.
    /// Defaults to `false`, matching the honest semantics.
    is_real_llm_override: bool,
    state: Mutex<StubState>,
}

#[derive(Default)]
struct StubState {
    canned: std::collections::VecDeque<String>,
    prompts: Vec<Vec<Message>>,
    call_count: usize,
}

impl StubLlmClient {
    /// Build a stub with the given backend name. The name is what
    /// `LlmClient::name()` returns and what shows up in
    /// `Provenance::by` on consolidation outputs.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_real_llm_override: false,
            state: Mutex::new(StubState::default()),
        }
    }

    /// Toggle whether `LlmClient::is_real_llm()` reports `true` for
    /// this stub. Use sparingly — only for integration tests that
    /// drive code paths gated on `Steward::has_llm()` but can't yet
    /// be reworked to use a real backend. Test-only behaviour;
    /// production code never constructs a `StubLlmClient`.
    pub fn pretend_real_llm(mut self, yes: bool) -> Self {
        self.is_real_llm_override = yes;
        self
    }

    /// Convenience for the common case: a stub named "stub-llm".
    pub fn default_stub() -> Self {
        Self::new("stub-llm")
    }

    /// Convenience: build with a single canned response already
    /// queued. Useful for `Arc::new(StubLlmClient::with_canned(...))`
    /// one-liners.
    pub fn with_canned(name: impl Into<String>, response: impl Into<String>) -> Self {
        let s = Self::new(name);
        s.push_canned(response);
        s
    }

    /// Push a canned response onto the FIFO queue. The next call to
    /// `complete()` returns it (wrapped in a `Message::assistant`),
    /// then it's consumed.
    pub fn push_canned(&self, response: impl Into<String>) {
        self.state.lock().canned.push_back(response.into());
    }

    /// Number of completed `complete()` calls.
    pub fn call_count(&self) -> usize {
        self.state.lock().call_count
    }

    /// Snapshot of every prompt the stub has been asked. Order =
    /// call order. Useful for asserting what the steward sent.
    pub fn prompts(&self) -> Vec<Vec<Message>> {
        self.state.lock().prompts.clone()
    }

    /// The default response when the canned queue is empty —
    /// JSON-shaped so `Steward::abstract_cluster` can parse it
    /// without a custom shim.
    pub fn default_response() -> &'static str {
        r#"{"content":"(stub abstraction)","triples":[],"confidence":0.5}"#
    }
}

#[async_trait]
impl LlmClient for StubLlmClient {
    fn name(&self) -> &str {
        &self.name
    }

    async fn complete(&self, messages: &[Message]) -> Result<Message> {
        let response = {
            let mut state = self.state.lock();
            state.call_count += 1;
            state.prompts.push(messages.to_vec());
            state
                .canned
                .pop_front()
                .unwrap_or_else(|| Self::default_response().to_string())
        };
        Ok(Message {
            role: Role::Assistant,
            content: response,
        })
    }

    /// The stub returns canned data; not a real LLM. Callers
    /// (notably `Steward::has_llm()`) read this to gate work that
    /// can't be faithfully arbitrated by hard-coded responses.
    /// Tests that need to drive LLM-gated code paths through the
    /// stub set the [`Self::pretend_real_llm`] flag to flip this.
    fn is_real_llm(&self) -> bool {
        self.is_real_llm_override
    }
}

/// Convenience wrapper: `Arc<dyn LlmClient>` from a fresh stub.
pub fn arc_stub() -> Arc<dyn LlmClient> {
    Arc::new(StubLlmClient::default_stub())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn default_response_is_parseable_json() {
        let s = StubLlmClient::default_stub();
        let resp = rt()
            .block_on(s.complete(&[Message::user("hello")]))
            .unwrap();
        assert_eq!(resp.role, Role::Assistant);
        let v: serde_json::Value = serde_json::from_str(&resp.content).expect("parses as JSON");
        assert_eq!(v["content"], "(stub abstraction)");
        assert_eq!(v["confidence"], 0.5);
    }

    #[test]
    fn canned_responses_drain_in_fifo_order() {
        let s = StubLlmClient::default_stub();
        s.push_canned("first");
        s.push_canned("second");
        let r1 = rt().block_on(s.complete(&[])).unwrap();
        let r2 = rt().block_on(s.complete(&[])).unwrap();
        let r3 = rt().block_on(s.complete(&[])).unwrap();
        assert_eq!(r1.content, "first");
        assert_eq!(r2.content, "second");
        // Queue empty → fall back to default.
        assert_eq!(r3.content, StubLlmClient::default_response());
        assert_eq!(s.call_count(), 3);
    }

    #[test]
    fn prompts_records_every_call() {
        let s = StubLlmClient::default_stub();
        let _ = rt()
            .block_on(s.complete(&[Message::user("alpha")]))
            .unwrap();
        let _ = rt()
            .block_on(s.complete(&[Message::system("S"), Message::user("beta")]))
            .unwrap();
        let prompts = s.prompts();
        assert_eq!(prompts.len(), 2);
        assert_eq!(prompts[0].len(), 1);
        assert_eq!(prompts[0][0].content, "alpha");
        assert_eq!(prompts[1].len(), 2);
        assert_eq!(prompts[1][1].content, "beta");
    }

    #[test]
    fn name_is_returned_unchanged() {
        let s = StubLlmClient::new("my-test-backend");
        assert_eq!(s.name(), "my-test-backend");
        let s2 = StubLlmClient::default_stub();
        assert_eq!(s2.name(), "stub-llm");
    }

    #[test]
    fn with_canned_constructor_queues_first_response() {
        let s = StubLlmClient::with_canned("named", r#"{"x":1}"#);
        let resp = rt().block_on(s.complete(&[])).unwrap();
        assert_eq!(resp.content, r#"{"x":1}"#);
    }

    #[test]
    fn is_real_llm_defaults_to_false_for_stub() {
        let s = StubLlmClient::default_stub();
        assert!(
            !s.is_real_llm(),
            "stub must report `is_real_llm` = false by default"
        );
    }

    #[test]
    fn pretend_real_llm_flips_is_real_llm_to_true() {
        let s = StubLlmClient::default_stub().pretend_real_llm(true);
        assert!(s.is_real_llm());
        let s2 = StubLlmClient::default_stub().pretend_real_llm(false);
        assert!(!s2.is_real_llm());
    }
}
