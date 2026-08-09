// SPDX-License-Identifier: Apache-2.0

//! v0.11.0 P4 — bridge per-tenant `InvalidateEvent` broadcasts into
//! MCP `notifications/message` events on each session's SSE stream.
//!
//! ## Scope
//!
//! v0.11.0 ships only the two MCP-spec-defined notification shapes
//! (`notifications/progress` and `notifications/message`) per plan §3
//! Decision B. P3 wired `notifications/progress` (per-tool progress).
//! P4 wires `notifications/message` by subscribing to each tenant's
//! existing `LibraryHandle::invalidate_sender()` broadcast channel (the
//! same channel that powers `/v1/graph/stream` for solo-web) and mapping
//! each `InvalidateEvent` into a JSON-RPC `notifications/message`
//! envelope on the per-session broadcast channel from P1+P2.
//!
//! ## Mapping policy (locked for v0.11.0)
//!
//! `InvalidateEvent` carries a `kind` discriminator (`episode`,
//! `document`, `chunk`, `cluster`, `triple`, `tenant`) and a `reason`
//! string (`memory.remember`, `memory.forget`, …) per
//! `docs/dev-log/0118-graph-stream-impl.md`. P4 maps these conservatively
//! into MCP `data` strings the client can switch on:
//!
//! | `kind`     | `data` string          | Notes |
//! |------------|------------------------|-------|
//! | `episode`  | `memories_updated`     | Coarse signal: any episode-level change |
//! | `document` | `documents_updated`    | Ingest / forget at document granularity |
//! | `chunk`    | `documents_updated`    | Coalesced under documents (chunks are a leaf of one document) |
//! | `cluster`  | `consolidation_updated`| Steward consolidation landed |
//! | `triple`   | `graph_updated`        | Triples-extract committed |
//! | `tenant`   | `library_updated`      | GDPR cascade or library lifecycle |
//! | other      | `memory_updated`       | Fallback so a future writer kind never silently drops |
//!
//! `level = "info"` for every variant; `logger = "solo"`. The MCP spec's
//! `notifications/message` shape allows `level` ∈ {debug, info, warning,
//! error}; v0.11.0 emits at `info` because every wire shape is a
//! committed write (invariant: `InvalidateEvent` is only sent after the
//! writer-actor's commit succeeds — see `solo_core::InvalidateEvent`'s
//! docstring).
//!
//! Each emitted MCP message ALSO carries the original `reason`,
//! `reason`, `kind`, and `ts_ms` in a nested `details` object so clients that
//! want the structured shape get it without losing the coarse
//! `data` switch. The two halves stay in lock-step — `data` is what to
//! refetch; `details.reason` is why.
//!
//! ## Per-session task lifecycle
//!
//! For each session created by `POST /mcp`, the POST handler calls
//! [`spawn_invalidate_bridge`] passing the freshly-resolved
//! `Arc<LibraryHandle>` + `Arc<SessionState>`. The function:
//!
//!   1. Calls `tenant.invalidate_sender().subscribe()` BEFORE downgrading
//!      the session Arc to a `Weak<SessionState>`.
//!   2. Spawns a `tokio::spawn` task that loops on `rx.recv()`:
//!      - `Ok(event)` → map via [`map_invalidate_to_message`], call
//!        `session.publish_event(Message, envelope)` via the `Weak`
//!        upgrade. Returns immediately if upgrade fails (session
//!        dropped — task exits).
//!      - `Err(RecvError::Lagged(n))` → log warn (matches
//!        `/v1/graph/stream` discipline) and continue. The session's
//!        own event-channel lag handling kicks in on the GET side; the
//!        bridge itself is best-effort fan-out.
//!      - `Err(RecvError::Closed)` → tenant handle dropped (rare
//!        outside test shutdown); task exits cleanly.
//!
//! The task holds NO strong reference to `SessionState` — only a
//! `Weak`. The session's `Arc` lives in the [`SessionStore`]; when the
//! store drops the session (TTL expiry or `delete`), the Weak upgrade
//! fails on the next event and the task exits. This is the
//! `session_task_exits_when_session_dropped` test contract.
//!
//! ## Out of scope (v0.12.0+)
//!
//! - **Coalescing rapid bursts.** v0.11.0 emits one MCP message per
//!   `InvalidateEvent`. A pathological pattern (e.g. 1000 rapid
//!   `memory.remember` calls under one session) would emit 1000
//!   messages. The per-session broadcast channel is bounded
//!   ([`crate::mcp_session::MCP_SESSION_EVENT_BUFFER_CAPACITY`] = 256),
//!   so a slow GET subscriber would see `event: lagged` per P2 rather
//!   than memory growth. A future v0.12.0 follow-up could coalesce
//!   bursts at the bridge tier (e.g. "if N events of the same kind in
//!   1s, emit one"); not needed for v0.11.0 launch.
//! - **Solo-custom event types** (`audit_event`, `memory_consolidated`,
//!   `tenant_changed` as Solo-custom shapes). Deferred to v0.12.0+ per
//!   plan §3 Decision B. P4 ships only spec-compliant
//!   `notifications/message`.
//! - **Per-event auth scoping.** The bridge inherits the session's
//!   bound tenant; cross-tenant filtering happens at the broadcast
//!   layer (each tenant has its own `invalidate_sender`) so cross-
//!   tenant leakage is impossible by construction.

use std::sync::{Arc, Weak};

use solo_core::InvalidateEvent;
use solo_storage::LibraryHandle;
use tokio::sync::broadcast;

use crate::mcp_session::{McpEventKind, SessionState};

/// JSON-RPC method literal for MCP `notifications/message`. Held as
/// `pub const` so audits can grep the wire literal once and trust it
/// never drifts (Lesson #25 / #39 grep-ability).
pub const MCP_NOTIFICATION_MESSAGE_METHOD: &str = "notifications/message";

/// `level` field of every MCP `notifications/message` envelope this
/// bridge emits. The spec permits `debug` / `info` / `warning` /
/// `error`; v0.11.0 emits `info` because the invariant on
/// [`solo_core::InvalidateEvent`] is that it ONLY fires after a
/// successful writer-actor commit — there is no error/warning path.
pub const MCP_NOTIFICATION_MESSAGE_LEVEL: &str = "info";

/// `logger` field of every MCP `notifications/message` envelope this
/// bridge emits. The MCP spec requires `logger` to be a server-chosen
/// identifier — `"solo"` distinguishes our messages from any
/// proxy / middleware that might emit on the same stream.
pub const MCP_NOTIFICATION_MESSAGE_LOGGER: &str = "solo";

/// `data` discriminator for episode-level invalidations. Coarse
/// signal: "any episode-level memory changed; refetch what you care
/// about". Maps `InvalidateEvent.kind = "episode"`.
pub const MCP_NOTIFICATION_DATA_MEMORIES_UPDATED: &str = "memories_updated";

/// `data` discriminator for document-level invalidations. Includes
/// chunks (coalesced — chunks are a leaf of a parent document, so
/// refetching the document covers both).
pub const MCP_NOTIFICATION_DATA_DOCUMENTS_UPDATED: &str = "documents_updated";

/// `data` discriminator for consolidation/cluster invalidations.
pub const MCP_NOTIFICATION_DATA_CONSOLIDATION_UPDATED: &str = "consolidation_updated";

/// `data` discriminator for graph (triples) invalidations.
pub const MCP_NOTIFICATION_DATA_GRAPH_UPDATED: &str = "graph_updated";

/// `data` discriminator for memory-library lifecycle invalidations (today
/// fired by the GDPR `forget_user` cascade per
/// `solo_core::InvalidateEvent`'s kind taxonomy).
pub const MCP_NOTIFICATION_DATA_LIBRARY_UPDATED: &str = "library_updated";

/// Fallback `data` for `InvalidateEvent.kind` values P4 doesn't
/// recognise. Defensive: a future writer command emits a new kind
/// string we haven't mapped yet → the client still sees a refetch
/// signal rather than the bridge dropping the event silently.
pub const MCP_NOTIFICATION_DATA_MEMORY_UPDATED: &str = "memory_updated";

/// Map one [`solo_core::InvalidateEvent`] into the MCP spec's
/// `notifications/message` JSON envelope.
///
/// Output shape:
///
/// ```json
/// {
///   "jsonrpc": "2.0",
///   "method": "notifications/message",
///   "params": {
///     "level": "info",
///     "logger": "solo",
///     "data": "memories_updated",
///     "details": {
///       "reason": "memory.remember",
///       "ts_ms": 1715625600000,
///       "kind": "episode"
///     }
///   }
/// }
/// ```
///
/// The `details` object preserves the full original event for clients
/// that want the structured shape; the top-level `data` carries the
/// coarse switch that drives a "refetch what you care about" UX. Per
/// plan §3 Decision B + §6 P4, this is the canonical mapping the
/// publish path uses.
pub fn map_invalidate_to_message(event: &InvalidateEvent) -> serde_json::Value {
    let data_kind = match event.kind.as_str() {
        "episode" => MCP_NOTIFICATION_DATA_MEMORIES_UPDATED,
        "document" | "chunk" => MCP_NOTIFICATION_DATA_DOCUMENTS_UPDATED,
        "cluster" => MCP_NOTIFICATION_DATA_CONSOLIDATION_UPDATED,
        "triple" => MCP_NOTIFICATION_DATA_GRAPH_UPDATED,
        "tenant" => MCP_NOTIFICATION_DATA_LIBRARY_UPDATED,
        _ => MCP_NOTIFICATION_DATA_MEMORY_UPDATED,
    };
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": MCP_NOTIFICATION_MESSAGE_METHOD,
        "params": {
            "level": MCP_NOTIFICATION_MESSAGE_LEVEL,
            "logger": MCP_NOTIFICATION_MESSAGE_LOGGER,
            "data": data_kind,
            "details": {
                "reason": event.reason,
                "ts_ms": event.ts_ms,
                "kind": event.kind,
            }
        }
    })
}

/// Spawn the per-session invalidate-bridge task.
///
/// Subscribes to the tenant's existing `invalidate_sender` BEFORE
/// downgrading the `SessionState` Arc to a `Weak` so a publish that
/// races the bridge spawn isn't dropped. The spawned task forwards
/// each [`solo_core::InvalidateEvent`] into the session's broadcast
/// channel as an [`McpEventKind::Message`] event whose JSON payload is
/// the [`map_invalidate_to_message`] result.
///
/// Task lifecycle (the canonical "when does this exit" answer):
///
///   - **Session dropped from store** (TTL expiry or `SessionStore::delete`):
///     the `Weak::upgrade` returns `None` on the next received event
///     → task exits cleanly. Pinned by
///     `session_task_exits_when_session_dropped`.
///   - **Tenant handle dropped** (rare outside test shutdown — production
///     keeps tenants alive for the daemon lifetime): `rx.recv()`
///     returns `RecvError::Closed` → task exits cleanly.
///   - **Subscriber lagged** (broadcast capacity overrun): log a `warn`
///     and continue receiving. The next-event semantics matches
///     `/v1/graph/stream` and ADR-0003's "invalidations are idempotent
///     refetch signals" invariant.
///
/// Returns the spawned `JoinHandle` so callers that want explicit
/// teardown (tests, future graceful-shutdown work) can await it; the
/// production POST handler discards the handle and lets the task run
/// detached until one of the exit conditions trips.
pub fn spawn_invalidate_bridge(
    tenant: Arc<LibraryHandle>,
    session: Arc<SessionState>,
) -> tokio::task::JoinHandle<()> {
    let rx = tenant.invalidate_sender().subscribe();
    let weak = Arc::downgrade(&session);
    // Drop the strong session ref BEFORE the spawn so the task only
    // holds the Weak. Without this drop, the task captures a strong
    // ref through the closure and never exits when the store evicts.
    drop(session);
    let session_id_for_log = match weak.upgrade() {
        Some(s) => format!("{:p}", Arc::as_ptr(&s)),
        None => "<dropped>".to_string(),
    };
    let task_session_id = session_id_for_log.clone();
    tokio::spawn(async move {
        run_invalidate_bridge(rx, weak, task_session_id).await;
    })
}

/// Inner loop for the bridge task. Split out so unit tests can drive
/// it without paying for `tokio::spawn`.
async fn run_invalidate_bridge(
    mut rx: broadcast::Receiver<InvalidateEvent>,
    weak: Weak<SessionState>,
    session_id_for_log: String,
) {
    loop {
        match rx.recv().await {
            Ok(event) => {
                let Some(state) = weak.upgrade() else {
                    tracing::debug!(
                        session = %session_id_for_log,
                        "mcp invalidate bridge: session dropped; exiting"
                    );
                    return;
                };
                let payload = map_invalidate_to_message(&event);
                state.publish_event(McpEventKind::Message, payload);
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    lagged = n,
                    session = %session_id_for_log,
                    "mcp invalidate bridge subscriber lagged; client will \
                     resync on the next real invalidate"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::debug!(
                    session = %session_id_for_log,
                    "mcp invalidate bridge: tenant invalidate channel closed; exiting"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_event(kind: &str) -> InvalidateEvent {
        InvalidateEvent {
            reason: "memory.remember".to_string(),
            tenant_id: "default".to_string(),
            ts_ms: 1_715_625_600_000,
            kind: kind.to_string(),
        }
    }

    /// Mapping table pin — each known `kind` produces the right `data`
    /// discriminator. Drives every row in the mapping table from the
    /// module docs.
    #[test]
    fn map_invalidate_to_message_routes_each_kind_to_data_discriminator() {
        let cases = [
            ("episode", MCP_NOTIFICATION_DATA_MEMORIES_UPDATED),
            ("document", MCP_NOTIFICATION_DATA_DOCUMENTS_UPDATED),
            ("chunk", MCP_NOTIFICATION_DATA_DOCUMENTS_UPDATED),
            ("cluster", MCP_NOTIFICATION_DATA_CONSOLIDATION_UPDATED),
            ("triple", MCP_NOTIFICATION_DATA_GRAPH_UPDATED),
            ("tenant", MCP_NOTIFICATION_DATA_LIBRARY_UPDATED),
            // Unknown kinds fall back to the generic discriminator so a
            // future writer kind isn't dropped silently.
            (
                "hypothetical_new_kind",
                MCP_NOTIFICATION_DATA_MEMORY_UPDATED,
            ),
        ];
        for (kind, expected_data) in cases {
            let msg = map_invalidate_to_message(&fake_event(kind));
            assert_eq!(
                msg["params"]["data"].as_str(),
                Some(expected_data),
                "kind={kind} must map to data={expected_data}",
            );
        }
    }

    /// Pin the spec envelope shape: `jsonrpc=2.0` + `method =
    /// notifications/message` + `params.{level,logger,data,details}` all
    /// present. Grep-able by the constants so any drift surfaces here.
    #[test]
    fn map_invalidate_to_message_uses_jsonrpc_notifications_message_method() {
        let msg = map_invalidate_to_message(&fake_event("episode"));
        assert_eq!(msg["jsonrpc"].as_str(), Some("2.0"));
        assert_eq!(
            msg["method"].as_str(),
            Some(MCP_NOTIFICATION_MESSAGE_METHOD),
        );
        // params.{level,logger,data} per the spec; details is Solo-
        // additive (allowed by the MCP spec — params is open-shape).
        let params = &msg["params"];
        assert_eq!(
            params["level"].as_str(),
            Some(MCP_NOTIFICATION_MESSAGE_LEVEL)
        );
        assert_eq!(
            params["logger"].as_str(),
            Some(MCP_NOTIFICATION_MESSAGE_LOGGER),
        );
        assert!(params["data"].is_string(), "data must be present + string");
    }

    /// Pin that `level` is `info` for every emitted message (the
    /// invariant from the module-level docstring: invalidate events
    /// only fire post-commit, so there's no error/warning path on this
    /// bridge).
    #[test]
    fn mcp_message_payload_includes_level_and_data() {
        let msg = map_invalidate_to_message(&fake_event("episode"));
        let params = &msg["params"];
        assert_eq!(
            params["level"].as_str(),
            Some("info"),
            "level must be 'info' for invalidate-bridged messages",
        );
        assert_eq!(
            params["data"].as_str(),
            Some(MCP_NOTIFICATION_DATA_MEMORIES_UPDATED),
        );
        // Details object carries the structured original event for
        // clients that want the granular shape — pinned here so a
        // future refactor that drops `details` from the wire surface
        // breaks this test loudly.
        assert_eq!(
            params["details"]["reason"].as_str(),
            Some("memory.remember"),
        );
        assert_eq!(params["details"]["kind"].as_str(), Some("episode"),);
        assert!(params["details"]["ts_ms"].is_number());
        assert!(params["details"].get("tenant_id").is_none());
    }

    /// End-to-end: subscribe to a session's events, drive an invalidate
    /// through the bridge directly (without a real LibraryHandle), and
    /// confirm the session receives the mapped MCP message.
    #[tokio::test]
    async fn run_invalidate_bridge_publishes_mcp_message_to_session() {
        // Build a fake broadcast and an empty session.
        let (tx, _) = broadcast::channel::<InvalidateEvent>(16);
        let state = Arc::new(SessionState::new(None));
        let rx = tx.subscribe();
        let weak = Arc::downgrade(&state);
        // Pre-subscribe to the SESSION channel so we can read what the
        // bridge publishes.
        let mut session_rx = state.subscribe_events();
        // Spawn the bridge.
        let bridge_handle = tokio::spawn(async move {
            run_invalidate_bridge(rx, weak, "test-session".to_string()).await;
        });
        // Fire one invalidate.
        tx.send(fake_event("document"))
            .expect("at least one subscriber");
        // Bridge should publish exactly one MCP Message event onto the
        // session channel.
        let received = tokio::time::timeout(std::time::Duration::from_secs(2), session_rx.recv())
            .await
            .expect("event must arrive within 2s")
            .expect("session receiver must receive published event");
        assert_eq!(received.event, McpEventKind::Message);
        assert_eq!(
            received.data["method"].as_str(),
            Some(MCP_NOTIFICATION_MESSAGE_METHOD),
        );
        assert_eq!(
            received.data["params"]["data"].as_str(),
            Some(MCP_NOTIFICATION_DATA_DOCUMENTS_UPDATED),
        );
        // Drop the session strong ref → next invalidate should exit
        // the bridge.
        drop(state);
        tx.send(fake_event("episode")).ok();
        // Bridge task should observe the dropped session and exit
        // within ~1s.
        tokio::time::timeout(std::time::Duration::from_secs(2), bridge_handle)
            .await
            .expect("bridge must exit when session drops")
            .expect("bridge task panic");
    }

    /// Bridge exits when the session is dropped from the store.
    /// Specifically pins the `Weak::upgrade == None` exit path.
    #[tokio::test]
    async fn session_task_exits_when_session_dropped() {
        let (tx, _) = broadcast::channel::<InvalidateEvent>(16);
        let state = Arc::new(SessionState::new(None));
        let rx = tx.subscribe();
        let weak = Arc::downgrade(&state);
        let bridge = tokio::spawn(async move {
            run_invalidate_bridge(rx, weak, "test-session-drop".to_string()).await;
        });
        // Drop the session BEFORE the bridge sees any events.
        drop(state);
        // Fire one event; the bridge upgrades the Weak, sees None,
        // and exits.
        tx.send(fake_event("episode")).ok();
        tokio::time::timeout(std::time::Duration::from_secs(2), bridge)
            .await
            .expect("bridge must exit within 2s of session drop")
            .expect("bridge task panic");
    }

    /// Bridge exits when the broadcast channel closes (LibraryHandle
    /// dropped). Pins the `RecvError::Closed` exit path.
    #[tokio::test]
    async fn bridge_exits_when_tenant_invalidate_channel_closes() {
        let (tx, _) = broadcast::channel::<InvalidateEvent>(16);
        let state = Arc::new(SessionState::new(None));
        let rx = tx.subscribe();
        let weak = Arc::downgrade(&state);
        let bridge = tokio::spawn(async move {
            run_invalidate_bridge(rx, weak, "test-tenant-drop".to_string()).await;
        });
        // Drop the broadcast Sender → channel closes.
        drop(tx);
        tokio::time::timeout(std::time::Duration::from_secs(2), bridge)
            .await
            .expect("bridge must exit within 2s of channel close")
            .expect("bridge task panic");
        // Keep session alive — it should be retrievable after the
        // bridge exits (proves the bridge didn't accidentally take
        // ownership).
        let _id = state.publish_event(McpEventKind::Message, serde_json::json!({"alive": true}));
    }

    /// Bridge ignores `RecvError::Lagged` and continues forwarding.
    /// Drives the warn-and-continue path explicitly.
    #[tokio::test]
    async fn bridge_logs_and_continues_on_lagged_subscriber() {
        // Channel cap 2 — easy to overrun.
        let (tx, _) = broadcast::channel::<InvalidateEvent>(2);
        let state = Arc::new(SessionState::new(None));
        let rx = tx.subscribe();
        let weak = Arc::downgrade(&state);
        let mut session_rx = state.subscribe_events();
        let bridge = tokio::spawn(async move {
            run_invalidate_bridge(rx, weak, "test-lag".to_string()).await;
        });
        // Publish 5 events before the bridge has a chance to drain. The
        // bridge's broadcast receiver will lag past capacity 2.
        for _ in 0..5 {
            tx.send(fake_event("episode")).ok();
        }
        // The bridge MUST keep running — even if it missed some events
        // due to lag, it should forward the next one. Send another
        // event after a brief settle.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        tx.send(fake_event("triple")).expect("send must succeed");
        // The bridge should publish at least one mapped event to the
        // session. Don't assert exact count (lag is by design lossy);
        // assert that the bridge keeps forwarding after a lag.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session_rx.recv())
            .await
            .expect("bridge must still be alive after lag")
            .expect("session must receive at least one event");
        // Tear down.
        drop(state);
        drop(tx);
        // Bridge should now exit either via closed channel OR weak
        // upgrade failure on the next message — either is fine.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), bridge).await;
    }
}
