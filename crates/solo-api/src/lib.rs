// SPDX-License-Identifier: Apache-2.0

#![recursion_limit = "512"]

//! Solo transports: MCP server (rmcp) and HTTP/JSON (axum).
//!
//! - MCP stdio: [`mcp::SoloMcpServer`] + [`mcp::serve_stdio`].
//! - HTTP/JSON: [`http::SoloHttpState`] + [`http::serve_http`].
//! - Auth (v0.8.0 P3): [`auth::AuthConfig`] + [`auth::AuthenticatedPrincipal`].
//! - Direct LLM backends are constructed by `solo-storage`; the legacy
//!   sampling adapter remains available only for compatibility tests.

pub mod asset_download;
pub mod auth;
mod desktop_assets;
pub mod document_upload;
mod graph_paths;
mod graph_relationships;
pub mod http;
pub mod llm;
mod log_tail;
pub mod mcp;
// v0.10.2 P1: transport-agnostic JSON-RPC dispatcher shared by the new
// HTTP `/mcp` route and (eventually) the stdio loop. See
// `docs/dev-log/0129-v0.10.2-mcp-over-http-impl.md` for the refactor.
pub mod mcp_dispatch;
pub mod mcp_protocol;
// v0.11.0 P1: `Mcp-Session-Id` session store + Axum middleware.
// In-memory DashMap-backed, TTL-bounded (30 min inactivity / 4 hr
// absolute). The middleware validates the header on every `/mcp`
// request; the POST handler creates a new session on the first
// request without the header and echoes the id back. See
// `docs/dev-log/0133-v0.11.0-p1-impl.md`.
pub mod mcp_session;
// v0.11.0 P3: per-tool progress events. Wraps the session's broadcast
// channel in a [`mcp_progress::ProgressReporter`] handle long-running
// tool handlers call from sensible checkpoints (parse / embed / insert
// for `memory_ingest_document`, etc). Spec-shape `notifications/progress`
// envelope correlated by the client's `_meta.progressToken`. See
// `docs/dev-log/0135-v0.11.0-p3-impl.md`.
pub mod mcp_progress;
// v0.11.0 P4: bridges memory-library `InvalidateEvent` broadcasts into
// MCP `notifications/message` events on each session's SSE stream.
// Subscribes to the existing `LibraryHandle::invalidate_sender()` (the
// same channel powering `/v1/graph/stream`) and maps `InvalidateEvent`
// kinds into spec-compliant MCP message envelopes. See
// `docs/dev-log/0136-v0.11.0-p4-impl.md`.
pub mod mcp_notify;
pub mod mcp_task;
pub mod workspace_file_access;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use auth::{AuthConfig, AuthError, AuthenticatedPrincipal};
pub use http::{
    RuntimeControl, RuntimeControlCommand, RuntimeRestartResponse, SoloHttpState,
    StewardRuntimeStatus, StewardTriplesBatchStatus, openapi_spec,
    router_with_authenticated_routes, router_with_host_routes, serve_http, unix_ms_after,
};
pub use llm::{SamplingClient, SamplingError, SamplingLlmClient};
pub use mcp::{
    ENV_MCP_PRINCIPAL_TOKEN, SoloMcpServer, resolve_mcp_principal, serve_stdio, tool_names,
};
pub use mcp_dispatch::{
    JsonRpcErrorBody, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse, JsonRpcSuccess,
    McpDispatcher,
};
pub use mcp_notify::{
    MCP_NOTIFICATION_DATA_CONSOLIDATION_UPDATED, MCP_NOTIFICATION_DATA_DOCUMENTS_UPDATED,
    MCP_NOTIFICATION_DATA_GRAPH_UPDATED, MCP_NOTIFICATION_DATA_LIBRARY_UPDATED,
    MCP_NOTIFICATION_DATA_MEMORIES_UPDATED, MCP_NOTIFICATION_DATA_MEMORY_UPDATED,
    MCP_NOTIFICATION_MESSAGE_LEVEL, MCP_NOTIFICATION_MESSAGE_LOGGER,
    MCP_NOTIFICATION_MESSAGE_METHOD, map_invalidate_to_message, spawn_invalidate_bridge,
};
pub use mcp_progress::{
    MCP_NOTIFICATION_PROGRESS_METHOD, MCP_REMEMBER_BATCH_PROGRESS_EMIT_EVERY,
    MCP_REMEMBER_BATCH_PROGRESS_ITEM_THRESHOLD, MCP_SEARCH_DOCS_PROGRESS_TOP_K_THRESHOLD,
    ProgressReporter, ProgressToken, report_if_some,
};
pub use mcp_session::{
    MCP_LAST_EVENT_ID_HEADER, MCP_SESSION_ABSOLUTE_TTL_MS, MCP_SESSION_EVENT_BUFFER_CAPACITY,
    MCP_SESSION_EXPIRED_ERROR, MCP_SESSION_ID_HEADER, MCP_SESSION_INACTIVITY_TTL_MS,
    MCP_SESSION_SWEEP_INTERVAL_SECS, MCP_STREAM_EVENT_HEARTBEAT_NAME, MCP_STREAM_EVENT_INIT_NAME,
    MCP_STREAM_EVENT_LAGGED_NAME, MCP_STREAM_EVENT_MESSAGE_NAME, MCP_STREAM_EVENT_PROGRESS_NAME,
    McpEventKind, McpStreamEvent, SessionId, SessionState, SessionStore, mcp_session_middleware,
    set_session_id_header,
};
pub use workspace_file_access::{ENV_WORKSPACE_FILE_ROOTS, WorkspaceFileAccessPolicy};
