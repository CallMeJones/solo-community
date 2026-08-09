// SPDX-License-Identifier: Apache-2.0

//! v0.10.1 F3 — integration test that pins the rmcp `Peer<RoleServer>`
//! concurrency invariant exercised by [`super::sampling::PeerSamplingClient`].
//!
//! ## What this closes
//!
//! v0.9.0 P2's audit (`docs/dev-log/0101-v0.9.0-p2-impl.md` §F3) flagged
//! that `PeerSamplingClient::call_peer` calls `peer.create_message(params)
//! .await` directly, and that when N parallel `SamplingLlmClient::complete`
//! calls share the same `Arc<PeerSamplingClient>`, they all dispatch into
//! the SAME `Peer<RoleServer>`. The unwritten assumption: rmcp's
//! request-id-keyed response dispatch routes each response back to the
//! caller that initiated it (no cross-wiring across concurrent calls).
//!
//! `FakeMcpClient` (the existing fixture in
//! [`crate::test_support::fake_mcp_client`]) is a sync-with-`Mutex` mock —
//! it doesn't exercise rmcp's actual JSON-RPC framing + response
//! correlation. F3 stays open until we have an integration test that
//! drives a REAL `rmcp::Peer<RoleServer>` over a REAL transport.
//!
//! ## Topology
//!
//! MCP sampling reverses the usual roles. The SERVER (Solo daemon) is the
//! party that asks the CLIENT to "please sample your LLM with this
//! prompt." So our test mirrors production:
//!
//! ```text
//!  ┌────────────────────────────────────────────────────────────────┐
//!  │                       tokio::io::duplex                        │
//!  └────────────────────────────────────────────────────────────────┘
//!         ↑                                                  ↑
//!         │  Peer<RoleServer>      Peer<RoleClient>          │
//!  ┌─────────────┐                                  ┌──────────────────┐
//!  │ Solo server │ ─── peer.create_message(p) ──→  │ TestClientHandler│
//!  │ (this test) │ ←── CreateMessageResult   ───   │ (this test)      │
//!  └─────────────┘                                  └──────────────────┘
//! ```
//!
//! `rmcp::service::serve_directly` skips the MCP initialize handshake
//! (we don't need it — `Peer::create_message` doesn't read peer
//! capabilities unless we pass `tools` / `tool_choice`, which we don't).
//! Both sides run their dispatch loops in tokio tasks; the duplex carries
//! framed JSON-RPC between them.
//!
//! ## Marker-based cross-wiring detection
//!
//! Each parallel call sends a unique prompt of the form `CALL_<n>`. The
//! `TestClientHandler` echoes that marker back inside the
//! `CreateMessageResult`. If rmcp pipelined without correct response-id
//! correlation, some call's response would arrive at the wrong caller —
//! the test fails because call `i` would receive `"CALL_<j>"` where
//! `j != i`.
//!
//! ## Result (verified at commit time)
//!
//! All three tests pass cleanly on rmcp 1.7.0. The F3 concern is closed:
//! `Peer<RoleServer>` correlates concurrent responses by the
//! per-request id assigned in `send_request_with_option` and dispatched
//! through the `local_responder_pool` keyed `HashMap<RequestId, _>`. No
//! cross-wiring at N = 16 parallel, no cross-wiring with a slow call
//! interleaved among fast ones, and serial calls round-trip correctly.

#![cfg(test)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::handler::client::ClientHandler;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CreateMessageRequestParams, CreateMessageResult, ErrorData as McpError, Role as RmcpRole,
    SamplingMessage, SamplingMessageContent,
};
use rmcp::service::{Peer, RequestContext, RoleClient, RoleServer, serve_directly};
use tokio::io::duplex;

/// Minimal stub `ServerHandler`. We never actually drive any
/// CLIENT → SERVER requests in these tests — the test only sends
/// SERVER → CLIENT `sampling/createMessage` requests through the
/// `Peer<RoleServer>`. `ServerHandler` exists only because
/// `serve_directly` needs SOMETHING to bind on the server side so the
/// dispatch loop has a task to run.
struct TestServerHandler;
impl ServerHandler for TestServerHandler {}

/// `ClientHandler` impl that responds to every
/// `sampling/createMessage` request by echoing the user-text prompt
/// inside an assistant message. The test inserts unique `CALL_<n>`
/// markers in the prompt and asserts each caller sees the SAME marker
/// echoed back — any rmcp cross-wiring of response ids would surface as
/// call `i` receiving call `j`'s marker.
///
/// `slow_after_ms` is the optional pause inserted before the response
/// is built; the slow-call test uses it to verify that an in-flight
/// "stuck" call doesn't accidentally claim a faster call's response.
struct TestClientHandler {
    /// Per-prompt response delay. Looked up by the caller's marker
    /// string; absent ⇒ no delay. Mutex-wrapped so the test can hand
    /// the handler to `serve_directly` without `Sync` gymnastics over a
    /// `Cell`.
    delay_for_marker: Arc<Mutex<std::collections::HashMap<String, Duration>>>,
}

impl TestClientHandler {
    fn new() -> Self {
        Self {
            delay_for_marker: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    fn with_delay(self, marker: impl Into<String>, delay: Duration) -> Self {
        self.delay_for_marker
            .lock()
            .expect("delay map mutex")
            .insert(marker.into(), delay);
        self
    }

    /// Pull the user-text from the request — same shape as
    /// `super::sampling::extract_text`, but for SamplingMessage-on-input.
    fn extract_user_text(params: &CreateMessageRequestParams) -> String {
        let mut out = String::new();
        for msg in &params.messages {
            if msg.role != RmcpRole::User {
                continue;
            }
            for content in msg.content.iter() {
                if let SamplingMessageContent::Text(text) = content {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&text.text);
                }
            }
        }
        out
    }
}

impl ClientHandler for TestClientHandler {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> std::result::Result<CreateMessageResult, McpError> {
        let prompt = Self::extract_user_text(&params);
        let delay = self
            .delay_for_marker
            .lock()
            .expect("delay map mutex")
            .get(&prompt)
            .copied();
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        // Echo the prompt verbatim. The test asserts on this exact
        // string to detect cross-wiring.
        let echoed = format!("ECHO[{prompt}]");
        Ok(CreateMessageResult::new(
            SamplingMessage::assistant_text(echoed),
            "test-client".to_string(),
        ))
    }
}

/// Wire up the duplex transport + spawn both rmcp service loops.
/// Returns the `Peer<RoleServer>` that the unit-under-test would hold,
/// plus the `RunningService` handles so the test can keep them alive
/// for the duration of the assertions.
async fn make_peer(
    handler: TestClientHandler,
) -> (
    Peer<RoleServer>,
    rmcp::service::RunningService<RoleServer, TestServerHandler>,
    rmcp::service::RunningService<RoleClient, TestClientHandler>,
) {
    // 64 KiB buffer is plenty for the few hundred bytes per round-trip.
    let (server_io, client_io) = duplex(64 * 1024);

    // `serve_directly` skips the initialize handshake. We don't need it
    // for the create_message contract under test.
    let server_service = serve_directly(TestServerHandler, server_io, None);
    let client_service = serve_directly(handler, client_io, None);

    let peer = server_service.peer().clone();
    (peer, server_service, client_service)
}

/// Pull the echoed marker out of an rmcp result the same way
/// `super::sampling::extract_text` does.
fn extract_assistant_text(result: &CreateMessageResult) -> String {
    let mut out = String::new();
    for content in result.message.content.iter() {
        if let SamplingMessageContent::Text(text) = content {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    out
}

/// Build a `CreateMessageRequestParams` whose user message carries
/// `marker` as the prompt text. Matches the shape `PeerSamplingClient`
/// sends in production.
fn marker_request(marker: &str) -> CreateMessageRequestParams {
    CreateMessageRequestParams::new(vec![SamplingMessage::user_text(marker)], 32)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Sanity: 5 sequential calls each get back their own marker.
///
/// If this fails, the round-trip is broken end-to-end and the parallel
/// test wouldn't tell us anything new — pin the serial path first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_create_message_serial_calls_get_correct_responses() {
    let (peer, _server_handle, _client_handle) = make_peer(TestClientHandler::new()).await;

    for i in 0..5 {
        let marker = format!("CALL_{i}");
        let result = peer
            .create_message(marker_request(&marker))
            .await
            .expect("create_message should succeed");
        let echoed = extract_assistant_text(&result);
        assert_eq!(
            echoed,
            format!("ECHO[{marker}]"),
            "serial call {i} should receive its OWN response, not some other call's"
        );
    }
}

/// **Load-bearing test for F3**: N parallel `peer.create_message` calls
/// must each receive ITS OWN response. Any cross-wiring of response ids
/// inside rmcp's dispatch would surface as a marker mismatch here.
///
/// N = 16 is enough to make any non-deterministic ordering surface in a
/// few iterations of the dispatch loop while still being fast on slow
/// CI (each call is ~1ms wall, so total is well under a second).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_create_message_handles_concurrent_calls_without_cross_wiring() {
    let (peer, _server_handle, _client_handle) = make_peer(TestClientHandler::new()).await;
    let peer = Arc::new(peer);

    const N: usize = 16;
    let mut tasks = Vec::with_capacity(N);
    for i in 0..N {
        let peer = Arc::clone(&peer);
        tasks.push(tokio::spawn(async move {
            let marker = format!("CALL_{i}");
            let result = peer
                .create_message(marker_request(&marker))
                .await
                .expect("create_message should succeed");
            (i, extract_assistant_text(&result))
        }));
    }

    let mut got = Vec::with_capacity(N);
    for task in tasks {
        got.push(task.await.expect("join should succeed"));
    }

    // Every call must observe ITS OWN echoed marker.
    for (call_id, echoed) in &got {
        let expected = format!("ECHO[CALL_{call_id}]");
        assert_eq!(
            echoed, &expected,
            "call {call_id} received {echoed:?}, expected {expected:?}. \
             rmcp cross-wired the response ids?"
        );
    }
    // Sanity: every call ran exactly once.
    assert_eq!(got.len(), N);
}

/// **Realistic interleaving**: one slow call mixed in with several fast
/// ones. When the slow call finally resolves, it must receive ITS OWN
/// marker — not a fast call's response that happened to land first.
///
/// This catches a subtler regression class than uniform-cost parallel:
/// if rmcp's dispatch were "first response wins" rather than
/// "response-id matches request-id", the slow call would receive a fast
/// call's payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peer_create_message_handles_slow_call_among_fast_ones() {
    // SLOW_MARKER's response is delayed by ~150 ms; the fast calls
    // resolve in <10 ms each.
    let slow_marker = "CALL_SLOW";
    let handler = TestClientHandler::new().with_delay(slow_marker, Duration::from_millis(150));
    let (peer, _server_handle, _client_handle) = make_peer(handler).await;
    let peer = Arc::new(peer);

    // Kick off the slow call first.
    let slow_task = {
        let peer = Arc::clone(&peer);
        tokio::spawn(async move {
            peer.create_message(marker_request(slow_marker))
                .await
                .expect("slow create_message should succeed")
        })
    };
    // Tiny yield so the slow call definitely lands in the dispatch
    // loop before the fast ones queue up. Not load-bearing — the
    // assertion still holds without it — but it makes the
    // interleaving deterministic on small worker pools.
    tokio::task::yield_now().await;

    // Fire 4 fast calls in parallel. They should all resolve well
    // before the slow one.
    let mut fast_tasks = Vec::new();
    for i in 0..4 {
        let peer = Arc::clone(&peer);
        fast_tasks.push(tokio::spawn(async move {
            let marker = format!("CALL_FAST_{i}");
            let result = peer
                .create_message(marker_request(&marker))
                .await
                .expect("fast create_message should succeed");
            (i, extract_assistant_text(&result))
        }));
    }

    for task in fast_tasks {
        let (i, echoed) = task.await.expect("join");
        let expected = format!("ECHO[CALL_FAST_{i}]");
        assert_eq!(
            echoed, expected,
            "fast call {i} got the wrong response while the slow call was \
             still in flight — cross-wiring detected"
        );
    }

    // Now the slow call. Its result must carry SLOW_MARKER, not any of
    // the fast markers that resolved before it.
    let slow_result = slow_task.await.expect("slow join");
    let echoed = extract_assistant_text(&slow_result);
    assert_eq!(
        echoed,
        format!("ECHO[{slow_marker}]"),
        "slow call received a fast call's response — rmcp dispatch is not \
         correlating response ids correctly"
    );
}
