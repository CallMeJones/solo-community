// SPDX-License-Identifier: Apache-2.0

//! v0.11.0 P3 — per-tool progress events for long-running MCP tool calls.
//!
//! When an MCP `tools/call` request carries `_meta.progressToken` in its
//! params, the dispatcher builds a [`ProgressReporter`] wired to the
//! caller's [`crate::mcp_session::SessionState`]. The reporter is passed
//! into the tool handler, which calls [`ProgressReporter::report`] at
//! sensible checkpoints. Each call publishes a JSON-RPC
//! `notifications/progress` envelope onto the session's broadcast
//! channel, where the GET SSE stream subscriber forwards it to the
//! client.
//!
//! ## Wire shape (Decision C — MCP spec verbatim)
//!
//! ```json
//! {
//!   "jsonrpc": "2.0",
//!   "method": "notifications/progress",
//!   "params": {
//!     "progressToken": "<echo of the client's token>",
//!     "progress": 5,
//!     "total": 12,
//!     "message": "Processing cluster 5/12"
//!   }
//! }
//! ```
//!
//! The `progressToken` is whatever shape the client sent — string OR
//! number per the spec. [`ProgressToken`] is `serde_json::Value`-backed
//! so we preserve the wire shape losslessly on echo.
//!
//! ## Stdio backward compat
//!
//! [`ProgressReporter`] is constructed only for HTTP requests that have
//! both an attached `SessionState` (planted by the session middleware)
//! and a `_meta.progressToken` in their `tools/call` params. The stdio
//! path (rmcp `call_tool`) and the unit tests that drive `dispatch_tool`
//! directly pass `None` — handlers see `Option<ProgressReporter>` and
//! skip reporting silently when it's `None`.
//!
//! ## Backpressure
//!
//! [`crate::mcp_session::SessionState::publish_event`] is lossy on the
//! broadcast channel (slow subscribers see a `lagged` event on
//! reconnect) but lossless to the replay buffer for `Last-Event-ID`
//! resume. Progress events are inherently best-effort — a client that
//! falls behind 256 events still sees the final tool result on the
//! POST side.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::mcp_session::{McpEventKind, SessionState};

/// JSON-RPC method name for `notifications/progress` per the MCP
/// Streamable HTTP transport spec. Held as a const so any code that
/// emits the envelope agrees on the exact wire spelling — the GET
/// stream client matches on this string to route into its progress
/// handler.
pub const MCP_NOTIFICATION_PROGRESS_METHOD: &str = "notifications/progress";

/// Threshold above which `memory_search_docs` calls emit progress events.
/// Below 100 results the search completes fast enough that progress
/// notifications add wire-overhead with no UX benefit (Decision C).
pub const MCP_SEARCH_DOCS_PROGRESS_TOP_K_THRESHOLD: u32 = 100;

/// Threshold above which `memory_remember_batch` calls emit per-item
/// embedding progress. Below 50 items the batch completes inside one
/// or two embedder round-trips and progress notifications add
/// wire-overhead with no UX benefit (Decision C).
pub const MCP_REMEMBER_BATCH_PROGRESS_ITEM_THRESHOLD: usize = 50;

/// How often (in items processed) `memory_remember_batch` emits a
/// progress event during the embed loop. Plan §6 P3 spec: "every 25
/// rows". Set to 25 so a 51-item batch emits at items 25 + 50 + the
/// final terminal event.
pub const MCP_REMEMBER_BATCH_PROGRESS_EMIT_EVERY: usize = 25;

/// The progress token an MCP client passes in `tools/call` params
/// `_meta.progressToken`. Spec allows EITHER a string OR a number;
/// the server echoes back whatever shape the client sent. Held as
/// `serde_json::Value` so we don't lose the original wire shape on
/// the round-trip.
///
/// Created from the request `params._meta.progressToken` JSON value
/// via [`ProgressToken::from_meta`]; the dispatcher's `handle_tools_call`
/// is the sole caller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressToken(pub serde_json::Value);

impl ProgressToken {
    /// Read `_meta.progressToken` from the `tools/call` params JSON
    /// blob. Returns `None` if either the `_meta` key is absent, the
    /// `progressToken` key is absent, or the value is `null`. The spec
    /// only permits string and number — any other JSON type returns
    /// `None` so unexpected shapes are treated as "client did not
    /// request progress".
    pub fn from_meta(params: &serde_json::Value) -> Option<Self> {
        let meta = params.get("_meta")?;
        let token = meta.get("progressToken")?;
        match token {
            serde_json::Value::Null => None,
            serde_json::Value::String(_) | serde_json::Value::Number(_) => {
                Some(Self(token.clone()))
            }
            // Reject objects/arrays/bools — the spec only permits
            // string OR number.
            _ => None,
        }
    }

    /// The wire-shape value to echo back in `params.progressToken` of
    /// every emitted `notifications/progress` envelope.
    pub fn as_value(&self) -> &serde_json::Value {
        &self.0
    }
}

/// A handle that long-running tool handlers call to emit progress
/// notifications during their work. Wraps the
/// [`crate::mcp_session::SessionState`] that owns the broadcast channel
/// + replay buffer; calls into `SessionState::publish_event` for the
/// actual fan-out.
///
/// Cloneable so handlers can move it across `tokio::spawn` boundaries
/// (the underlying `Arc<SessionState>` + token clone are cheap).
///
/// Constructed only when the dispatch path has BOTH an attached
/// `Arc<SessionState>` AND a parsed `_meta.progressToken`. Callers
/// (`dispatch_tool` + downstream handlers) accept `Option<ProgressReporter>`
/// and skip reporting silently when it's `None`.
#[derive(Debug, Clone)]
pub struct ProgressReporter {
    session: Arc<SessionState>,
    token: ProgressToken,
}

impl ProgressReporter {
    /// Build a reporter wired to a session + the client's token. Sole
    /// caller today is `McpDispatcher::handle_tools_call` (and tests).
    pub fn new(session: Arc<SessionState>, token: ProgressToken) -> Self {
        Self { session, token }
    }

    /// Echo the original wire-shape token back to the client. Used by
    /// the GET stream sink to correlate progress events with in-flight
    /// `tools/call` requests on the JSON-RPC reply side.
    pub fn token(&self) -> &ProgressToken {
        &self.token
    }

    /// Publish one `notifications/progress` envelope to the session's
    /// event stream. The envelope:
    ///
    /// ```json
    /// {
    ///   "jsonrpc": "2.0",
    ///   "method": "notifications/progress",
    ///   "params": {
    ///     "progressToken": <client's original token>,
    ///     "progress": <current>,
    ///     "total": <expected total or null>,
    ///     "message": <optional human-readable string or null>
    ///   }
    /// }
    /// ```
    ///
    /// Returns the per-session event id assigned by
    /// `SessionState::publish_event` so callers can correlate the
    /// emit with the resulting stream entry. Lossy on the broadcast
    /// side (no live subscriber = silent drop) but lossless to the
    /// replay buffer.
    pub fn report(&self, progress: u64, total: Option<u64>, message: Option<&str>) -> u64 {
        let mut params = serde_json::Map::with_capacity(4);
        params.insert("progressToken".to_string(), self.token.0.clone());
        params.insert(
            "progress".to_string(),
            serde_json::Value::Number(progress.into()),
        );
        if let Some(t) = total {
            params.insert("total".to_string(), serde_json::Value::Number(t.into()));
        }
        if let Some(m) = message {
            params.insert(
                "message".to_string(),
                serde_json::Value::String(m.to_string()),
            );
        }
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "method": MCP_NOTIFICATION_PROGRESS_METHOD,
            "params": serde_json::Value::Object(params),
        });
        self.session.publish_event(McpEventKind::Progress, envelope)
    }
}

/// Helper for handlers: emit a progress event if-and-only-if a
/// reporter is present. Reads cleaner at every checkpoint than the
/// `if let Some(r) = …` pattern repeated 4x per handler.
///
/// The `_unused` reply is intentional: callers don't generally care
/// about the assigned event id; the `_unused = …` pattern in the
/// handlers documents the intent without polluting the call site.
pub fn report_if_some(
    reporter: Option<&ProgressReporter>,
    progress: u64,
    total: Option<u64>,
    message: Option<&str>,
) {
    if let Some(r) = reporter {
        let _id = r.report(progress, total, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fresh_session() -> Arc<SessionState> {
        Arc::new(SessionState::new(None))
    }

    #[test]
    fn progress_token_from_meta_reads_string_token() {
        let params = json!({
            "name": "memory_ingest_document",
            "_meta": { "progressToken": "abc-123" },
        });
        let token = ProgressToken::from_meta(&params).expect("string token");
        assert_eq!(token.as_value(), &json!("abc-123"));
    }

    #[test]
    fn progress_token_from_meta_reads_number_token() {
        let params = json!({
            "name": "memory_search_docs",
            "_meta": { "progressToken": 42 },
        });
        let token = ProgressToken::from_meta(&params).expect("number token");
        assert_eq!(token.as_value(), &json!(42));
    }

    #[test]
    fn progress_token_from_meta_returns_none_without_meta() {
        let params = json!({"name": "memory_inspect"});
        assert!(ProgressToken::from_meta(&params).is_none());
    }

    #[test]
    fn progress_token_from_meta_returns_none_for_null_token() {
        let params = json!({"_meta": {"progressToken": null}});
        assert!(ProgressToken::from_meta(&params).is_none());
    }

    #[test]
    fn progress_token_from_meta_rejects_object_token() {
        let params = json!({"_meta": {"progressToken": {"nope": 1}}});
        assert!(ProgressToken::from_meta(&params).is_none());
    }

    #[test]
    fn progress_reporter_publishes_spec_shape() {
        let session = fresh_session();
        let mut rx = session.subscribe_events();
        let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!("tok-1")));
        let _id = reporter.report(3, Some(12), Some("crunching"));
        let event = rx.try_recv().expect("event published");
        assert_eq!(event.event, McpEventKind::Progress);
        // Spec shape: jsonrpc + method + params{progressToken, progress, total, message}.
        assert_eq!(event.data["jsonrpc"], json!("2.0"));
        assert_eq!(
            event.data["method"],
            json!(MCP_NOTIFICATION_PROGRESS_METHOD)
        );
        assert_eq!(event.data["params"]["progressToken"], json!("tok-1"));
        assert_eq!(event.data["params"]["progress"], json!(3));
        assert_eq!(event.data["params"]["total"], json!(12));
        assert_eq!(event.data["params"]["message"], json!("crunching"));
    }

    #[test]
    fn progress_reporter_omits_total_and_message_when_absent() {
        let session = fresh_session();
        let mut rx = session.subscribe_events();
        let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!(7)));
        let _id = reporter.report(1, None, None);
        let event = rx.try_recv().expect("event published");
        assert_eq!(event.data["params"]["progressToken"], json!(7));
        assert_eq!(event.data["params"]["progress"], json!(1));
        // Per the spec, total and message are optional — absent keys
        // (not null) when the handler doesn't have a value to emit.
        assert!(
            event.data["params"].get("total").is_none(),
            "total key must be omitted when None"
        );
        assert!(
            event.data["params"].get("message").is_none(),
            "message key must be omitted when None"
        );
    }

    #[test]
    fn progress_reporter_emits_monotonic_event_ids() {
        let session = fresh_session();
        let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!("t")));
        let id_a = reporter.report(1, None, None);
        let id_b = reporter.report(2, None, None);
        let id_c = reporter.report(3, None, None);
        assert!(id_a < id_b);
        assert!(id_b < id_c);
    }

    #[test]
    fn report_if_some_noop_when_none() {
        // No session, no reporter — must not panic.
        report_if_some(None, 1, Some(2), Some("noop"));
    }

    #[test]
    fn report_if_some_publishes_when_present() {
        let session = fresh_session();
        let mut rx = session.subscribe_events();
        let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!("t")));
        report_if_some(Some(&reporter), 1, Some(1), Some("hi"));
        let event = rx.try_recv().expect("event published");
        assert_eq!(event.data["params"]["progress"], json!(1));
    }
}
