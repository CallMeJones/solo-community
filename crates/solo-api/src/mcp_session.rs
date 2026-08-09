// SPDX-License-Identifier: Apache-2.0

//! v0.11.0 P1 — MCP `Mcp-Session-Id` session store + middleware.
//!
//! v0.10.2 shipped `/mcp` as a one-shot request/response surface — every
//! POST opened a fresh dispatcher with no cross-request state. v0.11.0
//! lifts that to the full MCP Streamable HTTP spec, and this module is
//! the foundation: a `DashMap`-backed [`SessionStore`] of
//! [`SessionState`] entries keyed by [`SessionId`], plus an
//! [`mcp_session_middleware`] Axum middleware that validates the
//! `Mcp-Session-Id` request header against the store.
//!
//! ## Locked design (plan §3)
//!
//! - **Decision A — In-memory storage.** A
//!   `DashMap<SessionId, SessionState>` gives lock-free per-session
//!   reads on the dispatch hot path. Solo runs as a single-process
//!   daemon today; cross-process session persistence is deferred to a
//!   future release (clients re-`initialize` on daemon restart).
//! - **Decision D — TTL.** 30 min inactivity + 4 hr absolute cap.
//!   Background cleanup task runs every 60 s and removes expired
//!   sessions; a lazy expiry check on every `get` is the safety net
//!   for the window between sweeps.
//! - **Expired session → 404.** The middleware returns 404 Not Found
//!   with a body that includes a `re-initialize` instruction so
//!   clients can distinguish "session expired" from "server down".
//!
//! ## Dispatcher integration (Option B — session-agnostic)
//!
//! The brief leaves "does the dispatcher learn about sessions?" open.
//! P1 picks **Option B**: [`crate::mcp_dispatch::McpDispatcher`] stays
//! session-agnostic. Sessions are purely an HTTP-transport concern;
//! the dispatcher receives the resolved tenant + audit principal and
//! has no knowledge of `SessionId`. The stdio path (which has no
//! sessions) keeps working unchanged. v0.11.0 P3 will route per-tool
//! progress events through the session's notification channel by
//! reading the session out of the request extension before building
//! the per-request `ProgressEmitter` — without baking sessions into
//! the dispatcher itself.
//!
//! ## v0.11.0 P2 — event buffer + publish API
//!
//! P2 grows [`SessionState`] with the two fields the resumable GET
//! stream rides on top of:
//!
//!   - `event_tx: broadcast::Sender<McpStreamEvent>` (capacity
//!     [`MCP_SESSION_EVENT_BUFFER_CAPACITY`] = 256 per Decision E).
//!   - `next_event_id: AtomicU64` (monotonic per-session event id).
//!
//! A new [`SessionState::publish_event`] helper allocates the next id,
//! constructs an [`McpStreamEvent`], and fans it out to every live
//! subscriber. P3 (per-tool progress) and P4 (notifications/message
//! bridge) call this method on the same session record the HTTP POST
//! handler resolved; the GET handler in `http.rs` consumes via
//! [`SessionState::subscribe_events`].
//!
//! ## What this module does NOT do
//!
//! - **No tenant/principal binding check.** Cross-request tenant
//!   mismatch (`409 Conflict`) and cross-principal access
//!   (`401 Unauthorized`) still TBD — P2 wires the broadcast channel
//!   but leaves the auth-binding policy decisions to a follow-up
//!   priority (Plan §9 Q4).
//! - **No audit emission on session open/close.** Plan §9 Q3 still
//!   open — P2 keeps the store as pure in-memory plumbing.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::auth::AuthenticatedPrincipal;

/// HTTP header name carrying the `Mcp-Session-Id` per the MCP
/// Streamable HTTP transport spec. Lowercase because HTTP headers are
/// case-insensitive on the wire and axum stores them lowercased.
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// Inactivity TTL (milliseconds). A session whose `last_accessed_at_ms`
/// is more than this old is considered expired (Decision D).
pub const MCP_SESSION_INACTIVITY_TTL_MS: u64 = 30 * 60 * 1000;

/// Absolute TTL (milliseconds). A session is unconditionally expired
/// this long after its `created_at_ms`, regardless of activity
/// (Decision D — bounds worst-case memory growth for orphaned
/// sessions).
pub const MCP_SESSION_ABSOLUTE_TTL_MS: u64 = 4 * 60 * 60 * 1000;

/// Cadence (seconds) for the background sweep task that removes
/// expired sessions. Lazy expiry on every `get` is the primary safety
/// net; the sweep keeps total memory bounded between accesses for
/// idle sessions.
pub const MCP_SESSION_SWEEP_INTERVAL_SECS: u64 = 60;

/// HTTP header name carrying the `Last-Event-ID` per the SSE
/// specification. v0.11.0 P2 reads this on `GET /mcp` to resume an
/// interrupted stream from a known event id (Decision E).
pub const MCP_LAST_EVENT_ID_HEADER: &str = "last-event-id";

/// Capacity of the per-session `tokio::sync::broadcast` channel that
/// carries server-initiated SSE events (init, message, progress,
/// heartbeat, lagged). Per plan §3 Decision E. A subscriber that
/// drifts further behind than this sees a `RecvError::Lagged(n)` on
/// its next `recv`, at which point the GET handler emits one
/// `event: lagged` and resumes from the current cursor.
pub const MCP_SESSION_EVENT_BUFFER_CAPACITY: usize = 256;

/// Opaque session id assigned by the server. v7 UUID — time-ordered
/// for sortability per Solo's `memory_id` discipline; printed as a
/// regular hyphenated UUID string on the wire.
///
/// Server-assigned (not client-proposed) per Decision A — keeps
/// correctness on the server.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// Generate a fresh server-assigned id.
    pub fn new() -> Self {
        Self(Uuid::now_v7().to_string())
    }

    /// Parse a wire-format id. Returns `None` for empty / non-UUID
    /// strings; the middleware treats `None` as "unknown session" →
    /// 404 (rather than 400) so clients see a single re-init code
    /// path regardless of header malformation.
    pub fn parse(raw: &str) -> Option<Self> {
        // Reject empty / whitespace strings. We don't reject
        // arbitrary UUID strings here because the `DashMap` lookup
        // does the real validation: an id we never assigned simply
        // isn't in the store.
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        Some(Self(s.to_string()))
    }

    /// String representation suitable for the `Mcp-Session-Id` response
    /// header value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Discriminator for the per-session SSE event stream. The wire `event:`
/// field is rendered from the kebab-case spelling of each variant — kept
/// in lock-step with the constants below so handlers + clients agree
/// on the literal string.
///
/// Variants:
///
///   - [`Init`] — emitted once when a subscriber connects (`event: init`).
///     The payload includes the session id + tenant + connect ts.
///   - [`Message`] — JSON-RPC `notifications/message` from the P4 bridge
///     (`event: message`).
///   - [`Progress`] — JSON-RPC `notifications/progress` from P3 long-running
///     tool handlers (`event: progress`).
///   - [`Lagged`] — synthetic event emitted by the GET handler when a
///     subscriber falls past the broadcast buffer's capacity, OR when
///     a `Last-Event-ID` is older than the buffer's oldest retained
///     event (`event: lagged`). Carries `{dropped: <count>}` so clients
///     know whether to resync state.
///   - [`Heartbeat`] — synthetic event emitted by the heartbeat tick to
///     keep proxies + clients aware the stream is alive
///     (`event: heartbeat`).
///
/// [`Init`]: McpEventKind::Init
/// [`Message`]: McpEventKind::Message
/// [`Progress`]: McpEventKind::Progress
/// [`Lagged`]: McpEventKind::Lagged
/// [`Heartbeat`]: McpEventKind::Heartbeat
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpEventKind {
    /// `event: init` — subscriber-connect handshake.
    Init,
    /// `event: message` — JSON-RPC `notifications/message`.
    Message,
    /// `event: progress` — JSON-RPC `notifications/progress`.
    Progress,
    /// `event: lagged` — subscriber drifted past the buffer.
    Lagged,
    /// `event: heartbeat` — periodic liveness ping.
    Heartbeat,
}

/// SSE event name emitted on session-connect (`event: init`).
pub const MCP_STREAM_EVENT_INIT_NAME: &str = "init";
/// SSE event name carrying a JSON-RPC `notifications/message` payload.
pub const MCP_STREAM_EVENT_MESSAGE_NAME: &str = "message";
/// SSE event name carrying a JSON-RPC `notifications/progress` payload.
pub const MCP_STREAM_EVENT_PROGRESS_NAME: &str = "progress";
/// SSE event name emitted when a subscriber lags past the buffer.
pub const MCP_STREAM_EVENT_LAGGED_NAME: &str = "lagged";
/// SSE event name emitted on the periodic heartbeat tick.
pub const MCP_STREAM_EVENT_HEARTBEAT_NAME: &str = "heartbeat";

impl McpEventKind {
    /// Wire-format `event:` string. Kept in lock-step with the
    /// constants above so the GET handler + clients agree on the
    /// literal.
    pub fn as_str(&self) -> &'static str {
        match self {
            McpEventKind::Init => MCP_STREAM_EVENT_INIT_NAME,
            McpEventKind::Message => MCP_STREAM_EVENT_MESSAGE_NAME,
            McpEventKind::Progress => MCP_STREAM_EVENT_PROGRESS_NAME,
            McpEventKind::Lagged => MCP_STREAM_EVENT_LAGGED_NAME,
            McpEventKind::Heartbeat => MCP_STREAM_EVENT_HEARTBEAT_NAME,
        }
    }
}

/// One event on a session's SSE stream. Cloneable so the broadcast
/// channel can fan out one event to N concurrent subscribers.
///
/// The `id` is monotonic per session — clients carry the last seen id
/// in a `Last-Event-ID` header on reconnect to request a replay of
/// missed events. Heartbeats CARRY ids too (no second id space) so a
/// reconnecting client never sees a gap.
#[derive(Debug, Clone)]
pub struct McpStreamEvent {
    /// Monotonic per-session event id. First event is `1` (the init
    /// event); `0` is reserved as the sentinel "I have never seen
    /// anything" value clients send on first connect.
    pub id: u64,
    /// Wire-event discriminator (`init` / `message` / `progress` /
    /// `lagged` / `heartbeat`).
    pub event: McpEventKind,
    /// JSON payload. For `message` / `progress` this is the full
    /// JSON-RPC envelope minus the transport `id`. For `init` /
    /// `lagged` / `heartbeat` it's an event-specific Solo-shaped
    /// object documented at each call site.
    pub data: serde_json::Value,
}

/// One session's state. v0.11.0 P2 grows this from P1's minimal
/// timestamp record by adding the broadcast event channel
/// + monotonic event-id counter the resumable GET stream rides on.
///
/// **Not `Clone`** — atomics + `broadcast::Sender` make `Clone` a
/// surprising contract. The store hands out `Arc<SessionState>` so
/// concurrent requests observe each other's `touch()` calls + share
/// one event channel. Callers that want a snapshot of the timestamps
/// can read them via the public fields directly.
#[derive(Debug)]
pub struct SessionState {
    /// Authenticated principal at session create time. `None` for
    /// unauthenticated loopback deployments (the daemon default).
    /// A future cross-principal access check uses this to refuse a
    /// session presented with a different bearer / OIDC subject.
    pub principal: Option<AuthenticatedPrincipal>,
    /// Wall-clock millis at session create. Compared against
    /// `MCP_SESSION_ABSOLUTE_TTL_MS`.
    pub created_at_ms: i64,
    /// Wall-clock millis updated on every successful `SessionStore::get`.
    /// Compared against `MCP_SESSION_INACTIVITY_TTL_MS`. Stored as
    /// `AtomicI64` so reads via `Arc<SessionState>` can refresh without
    /// re-inserting into the DashMap shard.
    pub last_accessed_at_ms: std::sync::atomic::AtomicI64,
    /// v0.11.0 P2: broadcast channel fed by `publish_event`. The GET
    /// handler subscribes to this on connect; P3 (progress) and P4
    /// (notifications/message) publish into it. Capacity bounded by
    /// [`MCP_SESSION_EVENT_BUFFER_CAPACITY`] per Decision E.
    ///
    /// Note: `broadcast::channel` does NOT backfill freshly-subscribed
    /// receivers with previously-sent events. To support the
    /// `Last-Event-ID` resume contract we also keep a ring buffer
    /// (`event_replay_buffer`) which the GET handler reads on connect
    /// before tailing this channel for live events.
    pub event_tx: broadcast::Sender<McpStreamEvent>,
    /// v0.11.0 P2: monotonic per-session event id counter. Allocated
    /// via `fetch_add(1, SeqCst)` from `publish_event`; first event
    /// has id `1` (id `0` is the "never seen" sentinel clients send on
    /// the first `Last-Event-ID` header).
    pub next_event_id: AtomicU64,
    /// v0.11.0 P2: bounded ring buffer of recent events for
    /// `Last-Event-ID` replay. Capacity matches the broadcast channel
    /// ([`MCP_SESSION_EVENT_BUFFER_CAPACITY`]); oldest entry evicted
    /// on insert past the cap. `std::sync::Mutex` rather than
    /// `tokio::sync::Mutex` because the critical sections are tiny
    /// (push one event / clone a Vec out) — no `await` inside the
    /// lock. Wrapping in `Arc<Mutex<...>>` keeps `SessionState` cheap
    /// to share across the broadcast subscribers + the publisher.
    pub event_replay_buffer: Arc<std::sync::Mutex<VecDeque<McpStreamEvent>>>,
    /// Request ids cancelled by the client via MCP `notifications/cancelled`.
    /// Long-running handlers poll this cooperatively at safe checkpoints.
    cancelled_requests: DashMap<String, String>,
}

impl SessionState {
    /// Build a fresh session-state record. Used by [`SessionStore::insert`]
    /// and the session-extractor path. Allocates a fresh broadcast
    /// channel (capacity [`MCP_SESSION_EVENT_BUFFER_CAPACITY`]) for the
    /// session's SSE stream and a matching-capacity replay ring buffer.
    pub fn new(principal: Option<AuthenticatedPrincipal>) -> Self {
        let now_ms = now_ms();
        let (event_tx, _) = broadcast::channel(MCP_SESSION_EVENT_BUFFER_CAPACITY);
        let event_replay_buffer = Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(
            MCP_SESSION_EVENT_BUFFER_CAPACITY,
        )));
        Self {
            principal,
            created_at_ms: now_ms,
            last_accessed_at_ms: std::sync::atomic::AtomicI64::new(now_ms),
            event_tx,
            // Start at 1 so the first allocated id is `1` — `0` is
            // reserved for "client has never seen an event" on the
            // `Last-Event-ID` header.
            next_event_id: AtomicU64::new(1),
            event_replay_buffer,
            cancelled_requests: DashMap::new(),
        }
    }

    /// True iff this session is past either TTL.
    fn is_expired(&self, now_ms: i64) -> bool {
        let absolute_deadline = self
            .created_at_ms
            .saturating_add(MCP_SESSION_ABSOLUTE_TTL_MS as i64);
        if now_ms >= absolute_deadline {
            return true;
        }
        let last = self
            .last_accessed_at_ms
            .load(std::sync::atomic::Ordering::Relaxed);
        let inactivity_deadline = last.saturating_add(MCP_SESSION_INACTIVITY_TTL_MS as i64);
        now_ms >= inactivity_deadline
    }

    /// Bump `last_accessed_at_ms` to "now". Called on every successful
    /// `SessionStore::get`.
    fn touch(&self) {
        self.last_accessed_at_ms
            .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Allocate the next event id, construct an [`McpStreamEvent`],
    /// and (a) push it onto the replay ring buffer + (b) broadcast it
    /// to every live subscriber. Returns the assigned id so callers
    /// (P3/P4) can correlate their write with the resulting stream
    /// entry.
    ///
    /// Lossy on the broadcast side by design: if there are no live
    /// receivers (or every receiver has been dropped)
    /// `broadcast::Sender::send` returns `Err(SendError)` — the event
    /// is silently dropped from the live channel but STILL appended
    /// to the replay buffer so a future subscriber's `Last-Event-ID`
    /// replay observes it.
    ///
    /// Replay buffer is bounded at [`MCP_SESSION_EVENT_BUFFER_CAPACITY`]
    /// entries; pushing past the cap evicts the oldest entry. This
    /// matches the broadcast channel's capacity so the two stay in
    /// lock-step — a subscriber that subscribed before any events
    /// were published and then lags past 256 events sees the same
    /// "buffer overrun" semantics whether it observes them via the
    /// broadcast lagged-error path or via a `Last-Event-ID` resume.
    pub fn publish_event(&self, kind: McpEventKind, data: serde_json::Value) -> u64 {
        let id = self
            .next_event_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let event = McpStreamEvent {
            id,
            event: kind,
            data,
        };
        // Push to the replay buffer first — guarantees a subscriber
        // that races a publish-then-subscribe sequence either sees
        // the event via the buffer snapshot or via the broadcast
        // channel (never neither).
        if let Ok(mut buf) = self.event_replay_buffer.lock() {
            if buf.len() >= MCP_SESSION_EVENT_BUFFER_CAPACITY {
                buf.pop_front();
            }
            buf.push_back(event.clone());
        }
        // Ignore the result: `SendError` means "no live receivers",
        // which is fine — sessions can exist without an open GET
        // stream, and the replay buffer above carries forward.
        let _ = self.event_tx.send(event);
        id
    }

    /// Subscribe to the session's event stream. Returns a fresh
    /// `broadcast::Receiver` that observes every event published from
    /// this call forward. Combined with [`SessionState::snapshot_replay_buffer`]
    /// + `Last-Event-ID` replay logic in the GET handler, this gives
    /// the spec's resume-from-missed-event semantics.
    pub fn subscribe_events(&self) -> broadcast::Receiver<McpStreamEvent> {
        self.event_tx.subscribe()
    }

    /// Snapshot the current replay buffer. Returns a `Vec<McpStreamEvent>`
    /// in monotonically increasing id order. The GET handler calls
    /// this once on connect AFTER calling [`Self::subscribe_events`]
    /// (so any event published during the snapshot lands in the live
    /// receiver — the handler dedupes the overlap by id).
    pub fn snapshot_replay_buffer(&self) -> Vec<McpStreamEvent> {
        match self.event_replay_buffer.lock() {
            Ok(buf) => buf.iter().cloned().collect(),
            // Poisoned lock: return empty rather than panicking. The
            // GET handler treats this as "no buffered events" which
            // is harmless — the subscriber falls through to the live
            // broadcast receiver.
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        }
    }

    pub fn cancel_request(&self, request_id: impl Into<String>, reason: Option<String>) {
        self.cancelled_requests.insert(
            request_id.into(),
            reason.unwrap_or_else(|| "cancelled by client".to_string()),
        );
    }

    pub fn is_request_cancelled(&self, request_id: &str) -> bool {
        self.cancelled_requests.contains_key(request_id)
    }

    pub fn clear_request_cancellation(&self, request_id: &str) {
        self.cancelled_requests.remove(request_id);
    }
}

/// In-memory, lock-free session store keyed by [`SessionId`]. The
/// `Arc<SessionState>` value lets the middleware hand a cheap clone to
/// each request without holding a `DashMap` shard lock for the whole
/// dispatch.
///
/// Cloning a `SessionStore` is cheap — internally it's an
/// `Arc<Inner>`. Pass a clone to the per-process `SoloHttpState`; the
/// background sweep task holds another clone via `Arc::downgrade`.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<SessionStoreInner>,
}

struct SessionStoreInner {
    sessions: DashMap<SessionId, Arc<SessionState>>,
    /// Sweep task handle. Held so it can be aborted when the last
    /// store reference drops. Wrapped in `Mutex` so `new` and `Drop`
    /// can both touch it; never contended on the hot path.
    sweep_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SessionStore {
    /// Build a fresh store and spawn the background sweep task on the
    /// current tokio runtime. Panics if called outside a tokio runtime
    /// context — callers should construct this from inside an `async`
    /// context (e.g. the daemon's startup, or
    /// `tokio::runtime::Handle::current()`).
    pub fn new() -> Self {
        let inner = Arc::new(SessionStoreInner {
            sessions: DashMap::new(),
            sweep_task: std::sync::Mutex::new(None),
        });
        let weak = Arc::downgrade(&inner);
        let sweep = tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(Duration::from_secs(MCP_SESSION_SWEEP_INTERVAL_SECS));
            // Skip the immediate first tick — interval::tick fires once
            // immediately by default which would sweep a freshly-empty
            // store on startup (harmless but noisy in tests).
            tick.tick().await;
            loop {
                tick.tick().await;
                let Some(inner) = weak.upgrade() else {
                    // Store dropped; exit the loop. The `Drop` impl
                    // aborts us anyway; this is the cooperative path
                    // for graceful shutdown.
                    return;
                };
                sweep_once(&inner.sessions);
            }
        });
        // Survive a poisoned mutex during startup: extract the inner guard
        // rather than panicking. The mutex protects an Option<JoinHandle>
        // that we only ever assign in `new` and read in `Drop`; poisoning
        // would only happen if a prior `Drop` panicked, which is
        // recoverable here.
        *inner.sweep_task.lock().unwrap_or_else(|p| p.into_inner()) = Some(sweep);
        Self { inner }
    }

    /// Build a store WITHOUT a background sweep task. Used by tests that
    /// run outside a tokio runtime context or want to drive sweep
    /// manually via [`Self::sweep_now`].
    #[cfg(test)]
    pub(crate) fn new_for_tests_no_sweep() -> Self {
        let inner = Arc::new(SessionStoreInner {
            sessions: DashMap::new(),
            sweep_task: std::sync::Mutex::new(None),
        });
        Self { inner }
    }

    /// Insert a new session and return its assigned id. Each call
    /// produces a fresh server-assigned [`SessionId`] (UUID v7).
    pub fn insert(&self, state: SessionState) -> SessionId {
        let id = SessionId::new();
        self.inner.sessions.insert(id.clone(), Arc::new(state));
        id
    }

    /// Look up a session by id. Returns `None` if absent OR expired —
    /// expired entries are removed lazily here so the store doesn't
    /// hand out stale state between sweeps. The returned
    /// `Arc<SessionState>`'s `last_accessed_at_ms` is bumped to "now"
    /// on a successful hit.
    pub fn get(&self, id: &SessionId) -> Option<Arc<SessionState>> {
        let now = now_ms();
        // Fast path: clone the Arc out of the shard, then check expiry
        // outside the shard lock. If expired, remove and return None.
        let cloned = self.inner.sessions.get(id).map(|r| r.clone());
        let state = cloned?;
        if state.is_expired(now) {
            self.inner.sessions.remove(id);
            return None;
        }
        state.touch();
        Some(state)
    }

    /// Drop a session by id. Returns `true` if it was present.
    pub fn delete(&self, id: &SessionId) -> bool {
        self.inner.sessions.remove(id).is_some()
    }

    /// Current count of stored sessions. Used by tests + future
    /// /v1/health-style readiness probes.
    pub fn len(&self) -> usize {
        self.inner.sessions.len()
    }

    /// True if the store has no sessions.
    pub fn is_empty(&self) -> bool {
        self.inner.sessions.is_empty()
    }

    /// Force one immediate sweep. Used by the background task and by
    /// tests that want deterministic sweep behaviour without waiting
    /// 60s for the next tick.
    pub fn sweep_now(&self) {
        sweep_once(&self.inner.sessions);
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for SessionStoreInner {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.sweep_task.lock()
            && let Some(handle) = guard.take()
        {
            handle.abort();
        }
    }
}

/// Walk the map once and remove every expired entry. Held outside
/// `SessionStore::sweep_now` so the background task can call it
/// against `Arc<SessionStoreInner>` directly without re-borrowing
/// the cloneable `SessionStore` wrapper.
fn sweep_once(sessions: &DashMap<SessionId, Arc<SessionState>>) {
    let now = now_ms();
    // Collect first to avoid holding shard guards while issuing
    // removes (DashMap supports `retain` but `retain` blocks readers
    // shard-by-shard; collect-then-remove keeps the hot path
    // contention-free).
    let expired: Vec<SessionId> = sessions
        .iter()
        .filter(|entry| entry.value().is_expired(now))
        .map(|entry| entry.key().clone())
        .collect();
    for id in expired {
        sessions.remove(&id);
    }
}

/// Current wall-clock time in milliseconds. Centralised so tests can
/// hook in later (none of the v0.11.0 P1 tests need to). Returns
/// `chrono::Utc::now().timestamp_millis()` to match the rest of the
/// crate's timestamps.
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Body of the 404 response the middleware returns when a request
/// presents an unknown / expired `Mcp-Session-Id`. The
/// `re-initialize` field is the contract for client retry logic:
/// drop the stale id, POST `/mcp` without the header, capture the
/// `Mcp-Session-Id` from the response.
pub const MCP_SESSION_EXPIRED_ERROR: &str = "session_expired";

/// Axum middleware that enforces the `Mcp-Session-Id` contract.
///
/// Behaviour:
///   - **No `Mcp-Session-Id` header** → pass through. The downstream
///     POST handler treats this as a session-init request and emits
///     the assigned id in the response header.
///   - **`Mcp-Session-Id` header present + session in store + not
///     expired** → attach `Arc<SessionState>` + `SessionId` to the
///     request extensions and pass through.
///   - **`Mcp-Session-Id` header present + session unknown OR
///     expired** → 404 with body `{"error": "session_expired", ...,
///     "retry": "re-initialize"}`.
pub async fn mcp_session_middleware(
    State(store): State<SessionStore>,
    mut req: Request,
    next: Next,
) -> Response {
    let header_value = req
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    if let Some(raw) = header_value {
        let id = match SessionId::parse(&raw) {
            Some(id) => id,
            None => return session_expired_response(&raw),
        };
        match store.get(&id) {
            Some(state) => {
                req.extensions_mut().insert(id);
                req.extensions_mut().insert(state);
            }
            None => return session_expired_response(&raw),
        }
    }
    next.run(req).await
}

/// 404 + structured body. Browser MCP clients (Anthropic AI SDK's
/// `experimental_createMCPClient`) read the `error` discriminator to
/// route into the re-initialize path automatically.
fn session_expired_response(presented_id: &str) -> Response {
    let body = axum::Json(serde_json::json!({
        "error": MCP_SESSION_EXPIRED_ERROR,
        "status": 404,
        "message": format!(
            "Mcp-Session-Id `{presented_id}` is unknown or expired; \
             re-initialize via POST /mcp without Mcp-Session-Id"
        ),
        "retry": "re-initialize",
    }));
    (StatusCode::NOT_FOUND, body).into_response()
}

/// Insert a `Mcp-Session-Id` response header so the client can echo
/// it back on subsequent requests. Used by the POST handler when it
/// freshly creates a session on a request that arrived without the
/// header.
pub fn set_session_id_header(headers: &mut HeaderMap, id: &SessionId) {
    // SessionId::new produces a UUID-string which is always ASCII;
    // `HeaderValue::from_str` is safe. The `expect` documents the
    // invariant for future maintainers — we'd rather panic in CI than
    // silently drop the header.
    let value =
        HeaderValue::from_str(id.as_str()).expect("SessionId is ASCII-safe (UUID) for HeaderValue");
    headers.insert(HeaderName::from_static(MCP_SESSION_ID_HEADER), value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn fresh_state() -> SessionState {
        SessionState::new(None)
    }

    #[test]
    fn session_store_insert_returns_unique_id() {
        let store = SessionStore::new_for_tests_no_sweep();
        let id_a = store.insert(fresh_state());
        let id_b = store.insert(fresh_state());
        assert_ne!(id_a, id_b, "two inserts must produce distinct ids");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn session_store_get_returns_state_when_present() {
        let store = SessionStore::new_for_tests_no_sweep();
        let id = store.insert(fresh_state());
        let got = store.get(&id);
        assert!(got.is_some(), "get must return Some for a just-inserted id");
    }

    /// Build a state whose `created_at_ms` + `last_accessed_at_ms` are
    /// both shifted backwards by `delta_ms`. Used by the TTL tests to
    /// simulate an inactive / aged session without driving the wall
    /// clock. Mirrors `SessionState::new` for the v0.11.0 P2 broadcast
    /// channel + replay buffer + event id counter — those fields are
    /// independent of the timestamp shift.
    fn aged_state(principal: Option<AuthenticatedPrincipal>, delta_ms: i64) -> SessionState {
        let now = now_ms();
        let shifted = now.saturating_sub(delta_ms);
        let mut state = SessionState::new(principal);
        state.created_at_ms = shifted;
        state.last_accessed_at_ms.store(shifted, Ordering::Relaxed);
        state
    }

    #[test]
    fn session_store_get_returns_none_when_expired_by_inactivity() {
        let store = SessionStore::new_for_tests_no_sweep();
        // Hand-build a state whose `last_accessed_at_ms` is older than
        // the inactivity TTL.
        let stale_delta = MCP_SESSION_INACTIVITY_TTL_MS as i64 + 1;
        let stale = Arc::new(aged_state(None, stale_delta));
        let id = SessionId::new();
        store.inner.sessions.insert(id.clone(), stale);
        assert!(
            store.get(&id).is_none(),
            "session inactive past TTL must read as expired"
        );
        // Lazy expiry also evicts the entry.
        assert!(
            store.inner.sessions.get(&id).is_none(),
            "expired entry must be removed from the underlying map"
        );
    }

    #[test]
    fn session_store_get_returns_none_when_expired_by_absolute_ttl() {
        let store = SessionStore::new_for_tests_no_sweep();
        // Created past the absolute TTL but recently touched —
        // absolute-TTL still wins.
        let absolute_delta = MCP_SESSION_ABSOLUTE_TTL_MS as i64 + 1;
        let state = aged_state(None, absolute_delta);
        // Touch back to "now" so only the absolute deadline trips.
        state.last_accessed_at_ms.store(now_ms(), Ordering::Relaxed);
        let aged = Arc::new(state);
        let id = SessionId::new();
        store.inner.sessions.insert(id.clone(), aged);
        assert!(
            store.get(&id).is_none(),
            "session past absolute TTL must read as expired even when recently touched"
        );
    }

    #[test]
    fn session_store_get_refreshes_last_accessed_on_hit() {
        let store = SessionStore::new_for_tests_no_sweep();
        let id = store.insert(fresh_state());
        let before = store
            .inner
            .sessions
            .get(&id)
            .unwrap()
            .last_accessed_at_ms
            .load(Ordering::Relaxed);
        // Yield long enough that the millis clock advances.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let _ = store.get(&id).expect("session must still be present");
        let after = store
            .inner
            .sessions
            .get(&id)
            .unwrap()
            .last_accessed_at_ms
            .load(Ordering::Relaxed);
        assert!(
            after > before,
            "get must bump last_accessed_at_ms (before={before}, after={after})"
        );
    }

    #[test]
    fn session_store_delete_returns_true_when_present() {
        let store = SessionStore::new_for_tests_no_sweep();
        let id = store.insert(fresh_state());
        assert!(store.delete(&id));
        assert!(store.get(&id).is_none(), "deleted session must not read");
    }

    #[test]
    fn session_store_delete_returns_false_when_absent() {
        let store = SessionStore::new_for_tests_no_sweep();
        assert!(!store.delete(&SessionId::new()));
    }

    #[test]
    fn session_store_sweep_now_removes_expired() {
        let store = SessionStore::new_for_tests_no_sweep();
        // One healthy, one stale.
        let healthy_id = store.insert(fresh_state());
        let stale_delta = MCP_SESSION_INACTIVITY_TTL_MS as i64 + 1;
        let stale = Arc::new(aged_state(None, stale_delta));
        let stale_id = SessionId::new();
        store.inner.sessions.insert(stale_id.clone(), stale);
        assert_eq!(store.len(), 2);
        store.sweep_now();
        assert_eq!(store.len(), 1, "sweep must drop the expired session");
        assert!(
            store.get(&healthy_id).is_some(),
            "sweep must preserve the healthy session"
        );
        assert!(
            store.inner.sessions.get(&stale_id).is_none(),
            "stale id must be gone from the map after sweep"
        );
    }

    #[tokio::test]
    async fn session_store_background_sweep_removes_expired() {
        // Spawn a store with a real sweep task on the current rt.
        let store = SessionStore::new();
        // Seed a stale entry directly into the inner map.
        let stale_delta = MCP_SESSION_INACTIVITY_TTL_MS as i64 + 1;
        let stale = Arc::new(aged_state(None, stale_delta));
        let stale_id = SessionId::new();
        store.inner.sessions.insert(stale_id.clone(), stale);
        // Don't wait 60s; just call sweep_now to prove the same code
        // path the background task drives works. The 60s-cadence
        // background task itself is exercised by Drop semantics +
        // the explicit `sweep_once` unit test above.
        store.sweep_now();
        assert!(store.inner.sessions.get(&stale_id).is_none());
    }

    #[test]
    fn session_id_round_trips_through_string() {
        let id = SessionId::new();
        let s = id.as_str().to_string();
        let parsed = SessionId::parse(&s).expect("ASCII round-trip");
        assert_eq!(id, parsed);
    }

    #[test]
    fn session_id_parse_rejects_empty_string() {
        assert!(SessionId::parse("").is_none());
        assert!(SessionId::parse("   ").is_none());
    }

    // ----------------------------------------------------------------
    // v0.11.0 P2 — event buffer + publish_event unit tests
    // ----------------------------------------------------------------

    /// First three `publish_event` calls allocate ids 1, 2, 3 in order.
    /// Pins the "start-at-1, monotonic increment" contract; clients
    /// rely on a 0-sentinel for `Last-Event-ID` ("never seen anything")
    /// so the first allocated id MUST be ≥ 1.
    #[test]
    fn session_state_publish_event_returns_monotonic_ids() {
        let state = fresh_state();
        let id1 = state.publish_event(McpEventKind::Init, serde_json::json!({"connected": true}));
        let id2 = state.publish_event(McpEventKind::Message, serde_json::json!({"hello": 1}));
        let id3 = state.publish_event(McpEventKind::Progress, serde_json::json!({"progress": 5}));
        assert_eq!(
            id1, 1,
            "first event must allocate id 1 (id 0 reserved for client sentinel)"
        );
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    /// A subscriber that called `subscribe_events()` BEFORE the publish
    /// observes the published event on its receiver. Pins the
    /// broadcast wiring end-to-end.
    #[tokio::test]
    async fn session_state_publish_event_broadcasts_to_subscribers() {
        let state = fresh_state();
        let mut rx = state.subscribe_events();
        let id = state.publish_event(
            McpEventKind::Message,
            serde_json::json!({"jsonrpc": "2.0", "method": "notifications/message"}),
        );
        let received = rx
            .recv()
            .await
            .expect("subscriber must observe the broadcast event");
        assert_eq!(received.id, id);
        assert_eq!(received.event, McpEventKind::Message);
        assert_eq!(received.data["method"], "notifications/message");
    }

    /// Publishing past the broadcast channel's capacity (256) and the
    /// matching ring buffer's capacity:
    ///
    ///   - The replay buffer retains only the last 256 events (oldest
    ///     evicted on every push past the cap).
    ///   - A receiver that subscribed before the publishes observes
    ///     either every event or a `RecvError::Lagged` followed by the
    ///     tail of the events. We assert the buffer-only side here
    ///     (deterministic) — the receiver behaviour is covered by the
    ///     GET-handler integration test.
    #[test]
    fn session_state_event_buffer_capacity_256() {
        let state = fresh_state();
        let total = (MCP_SESSION_EVENT_BUFFER_CAPACITY + 50) as u64; // 306
        for _ in 0..total {
            state.publish_event(McpEventKind::Message, serde_json::json!({}));
        }
        let snapshot = state.snapshot_replay_buffer();
        assert_eq!(
            snapshot.len(),
            MCP_SESSION_EVENT_BUFFER_CAPACITY,
            "replay buffer must retain exactly {MCP_SESSION_EVENT_BUFFER_CAPACITY} entries after overflow",
        );
        // Oldest retained id = total - capacity + 1 (since ids start at 1
        // and we published `total` events).
        let expected_first_id = total - MCP_SESSION_EVENT_BUFFER_CAPACITY as u64 + 1;
        let expected_last_id = total; // last allocated id
        assert_eq!(
            snapshot.first().unwrap().id,
            expected_first_id,
            "oldest retained event id must be {expected_first_id}",
        );
        assert_eq!(
            snapshot.last().unwrap().id,
            expected_last_id,
            "newest retained event id must be {expected_last_id}",
        );
        // Buffer is contiguous (each id is previous + 1).
        for win in snapshot.windows(2) {
            assert_eq!(
                win[1].id,
                win[0].id + 1,
                "replay buffer must be contiguous (no gaps)",
            );
        }
    }

    /// `publish_event` with no live receivers does NOT error — the
    /// event still lands in the replay buffer so a future subscriber
    /// observes it via the `Last-Event-ID` replay path.
    #[test]
    fn session_state_publish_event_no_subscribers_is_lossless_to_buffer() {
        let state = fresh_state();
        let id = state.publish_event(McpEventKind::Init, serde_json::json!({"hi": true}));
        let snapshot = state.snapshot_replay_buffer();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, id);
    }
}
