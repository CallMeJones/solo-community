// SPDX-License-Identifier: Apache-2.0

//! v0.10.2 — transport-agnostic MCP JSON-RPC dispatcher.
//!
//! Until v0.10.1 the MCP server logic lived behind rmcp's stdio transport
//! ([`crate::mcp::serve_stdio`]); the only way an MCP client could reach
//! Solo's tools was by spawning `solo mcp-stdio` as a subprocess.
//! v0.10.2 adds an HTTP transport on `/mcp` so a single `solo daemon
//! --http-port` process can serve BOTH `/v1/graph/*` (REST, for solo-web)
//! and `/mcp` (JSON-RPC, for MCP clients) without the writer-lock dance.
//!
//! The dispatcher is the request -> response funnel that both transports
//! call into identically. It carries no transport-specific state — it
//! holds an [`Arc<SoloMcpServer>`](crate::mcp::SoloMcpServer) and routes
//! JSON-RPC method names to the existing direct-dispatch entry points
//! ([`SoloMcpServer::dispatch_list_tools`] +
//! [`SoloMcpServer::dispatch_tool`]). Today the stdio loop continues to
//! use rmcp's `ServerHandler` impl (which handles MCP framing for us);
//! the HTTP route uses this dispatcher directly to avoid hand-rolling
//! framing for one-shot request/response.
//!
//! ## Supported methods
//!
//! * `initialize` — returns the same `ServerInfo` shape rmcp emits over
//!   stdio. v0.10.2 returns the static info; the sampling-capability
//!   gating that lives in [`crate::mcp::SoloMcpServer::initialize`] is
//!   stdio-only because there's no `Peer<RoleServer>` over HTTP. HTTP
//!   clients that try to drive `mcp_sampling`-mode tenants will see
//!   sampling errors at tool-call time instead. Documented in the dev
//!   log for v0.10.2.
//! * `tools/list` — returns [`SoloMcpServer::dispatch_list_tools`].
//! * `tools/call` — returns [`SoloMcpServer::dispatch_tool`].
//! * `ping` — returns an empty object. Useful for HTTP-client liveness
//!   probes without paying the cost of `tools/list`.
//! * Anything else returns `MethodNotFound` per JSON-RPC 2.0.
//!
//! ## Notifications
//!
//! JSON-RPC notifications carry no `id` field; per spec the server MUST
//! NOT respond. `dispatch_notification` accepts these (e.g.
//! `notifications/initialized`) and returns `()`.
//!
//! ## Out of scope (deferred to v0.10.3+)
//!
//! - `Mcp-Session-Id` session affinity
//! - Resumable streams with `Last-Event-ID`
//! - Server-initiated requests over the GET SSE stream
//! - Per-tool streaming (progress events during long tool calls)

use std::sync::Arc;

use rmcp::model::{ErrorCode, ErrorData as McpError, Implementation};
use serde::{Deserialize, Serialize};
use solo_storage::{LibraryHandle, MemoryLibrary};

use crate::mcp::SoloMcpServer;
use crate::mcp_progress::{ProgressReporter, ProgressToken};
use crate::mcp_session::SessionState;

/// JSON-RPC 2.0 request envelope used by the HTTP transport.
///
/// `id` is `Option<Value>` per JSON-RPC 2.0: a missing `id` means the
/// message is a notification (no response expected). Requests with an
/// explicit `id: null` deserialise the same as a missing `id` (both
/// land as `None`) — we treat both as notifications per JSON-RPC 2.0
/// §4.1 ("`null` should not be used for the Id member of a Request
/// object"). Real MCP clients always send numeric or string ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 successful response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcSuccess {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub result: serde_json::Value,
}

/// JSON-RPC 2.0 error response envelope. `id` is `Value::Null` when
/// the server could not read the request id (parse error / unreadable
/// envelope); otherwise it echoes the request id back so the client
/// can correlate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub error: JsonRpcErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorBody {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Either a success or error response, serialised as a single JSON-RPC
/// 2.0 message on the wire. `serde(untagged)` so both shapes share the
/// `{jsonrpc, id, ...}` prefix and are distinguished by the presence of
/// `result` vs. `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse {
    Success(JsonRpcSuccess),
    Error(JsonRpcErrorResponse),
}

impl JsonRpcResponse {
    /// Build a success response with an explicit id.
    pub fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self::Success(JsonRpcSuccess {
            jsonrpc: "2.0".to_string(),
            id,
            result,
        })
    }

    /// Build an error response with an explicit id. Pass
    /// `serde_json::Value::Null` for `id` when the server could not read
    /// the request id (parse error / unreadable envelope).
    pub fn error(id: serde_json::Value, code: i32, message: impl Into<String>) -> Self {
        Self::Error(JsonRpcErrorResponse {
            jsonrpc: "2.0".to_string(),
            id,
            error: JsonRpcErrorBody {
                code,
                message: message.into(),
                data: None,
            },
        })
    }

    /// Convenience constructor: map an rmcp [`McpError`] to a JSON-RPC
    /// error response.
    pub fn from_mcp_error(id: serde_json::Value, err: McpError) -> Self {
        Self::error(id, err.code.0, err.message.to_string())
    }
}

/// Transport-agnostic MCP dispatcher used by the v0.10.2 HTTP `/mcp`
/// route. Holds the per-tenant [`SoloMcpServer`] needed to answer
/// `tools/list` and `tools/call`.
///
/// The dispatcher itself is stateless beyond the held server — callers
/// build a fresh dispatcher per request (cheap; the server is
/// `Arc`-cloneable) and discard it after dispatch returns.
#[derive(Clone)]
pub struct McpDispatcher {
    server: SoloMcpServer,
}

impl McpDispatcher {
    /// Build a dispatcher for one tenant. The caller is expected to
    /// resolve the Community Memory Library and pass its handle here.
    ///
    /// `audit_principal` is the subject that authored every tool call
    /// dispatched through this server — typically `"bearer"` for
    /// bearer-authenticated HTTP requests, the `SOLO_MCP_PRINCIPAL_TOKEN`
    /// env-var value for stdio, or `None` for unauthenticated loopback.
    pub fn new(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
        audit_principal: Option<String>,
    ) -> Self {
        Self::new_with_workspace_file_access(
            registry,
            tenant,
            user_aliases,
            audit_principal,
            crate::WorkspaceFileAccessPolicy::unrestricted(),
        )
    }

    pub fn new_with_workspace_file_access(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
        audit_principal: Option<String>,
        workspace_file_access: crate::WorkspaceFileAccessPolicy,
    ) -> Self {
        Self::new_with_workspace_file_access_and_tasks(
            registry,
            tenant,
            user_aliases,
            audit_principal,
            workspace_file_access,
            crate::mcp_task::TaskStore::new(),
        )
    }

    pub fn new_with_workspace_file_access_and_tasks(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
        audit_principal: Option<String>,
        workspace_file_access: crate::WorkspaceFileAccessPolicy,
        task_store: crate::mcp_task::TaskStore,
    ) -> Self {
        let server = SoloMcpServer::new_for_tenant_with_principal_workspace_file_access_and_tasks(
            registry,
            tenant,
            user_aliases,
            audit_principal,
            workspace_file_access,
            task_store,
        );
        Self { server }
    }

    /// Wrap an already-built [`SoloMcpServer`]. Used by tests that want
    /// to pin the underlying server's principal exactly; production
    /// callers should prefer [`Self::new`].
    pub fn from_server(server: SoloMcpServer) -> Self {
        Self { server }
    }

    /// Dispatch one JSON-RPC request and return the wire response.
    ///
    /// Returns `None` when the input is a notification (no `id` field) —
    /// per JSON-RPC 2.0 the server MUST NOT respond to notifications.
    /// The HTTP transport translates `None` into a 204 No Content or
    /// empty 200, depending on the client; the stdio path doesn't use
    /// this method (rmcp handles framing for stdio).
    ///
    /// v0.11.0 P3: `session` is the `Arc<SessionState>` planted on
    /// `/mcp` requests by the session middleware (Some on HTTP transport,
    /// None on stdio). When the request is a `tools/call` carrying
    /// `_meta.progressToken` AND `session` is `Some`, the handler builds
    /// a [`ProgressReporter`] and threads it into [`SoloMcpServer::dispatch_tool`].
    /// Other request types ignore the session argument entirely.
    pub async fn dispatch(
        &self,
        request: JsonRpcRequest,
        session: Option<Arc<SessionState>>,
    ) -> Option<JsonRpcResponse> {
        // Notifications: no `id`, no reply per JSON-RPC 2.0 §4.1.
        let Some(id) = request.id.clone() else {
            if request.method == "notifications/cancelled" {
                self.handle_cancelled_notification(request.params, session);
            }
            // We still want to log unexpected notification methods so
            // operators can diagnose silent client bugs.
            tracing::debug!(
                method = %request.method,
                "mcp-http: notification received (no id; no reply)"
            );
            return None;
        };

        let params = request.params.unwrap_or(serde_json::Value::Null);

        let response = match request.method.as_str() {
            "initialize" => self.handle_initialize(id.clone(), params),
            "tools/list" => self.handle_tools_list(id.clone()),
            "tools/call" => self.handle_tools_call(id.clone(), params, session).await,
            "resources/list" => self.handle_resources_list(id.clone()).await,
            "resources/templates/list" => self.handle_resource_templates_list(id.clone()),
            "resources/read" => self.handle_resources_read(id.clone(), params).await,
            "tasks/list" => self.handle_tasks_list(id.clone()),
            "tasks/get" => self.handle_tasks_get(id.clone(), params),
            "tasks/result" => self.handle_tasks_result(id.clone(), params),
            "tasks/cancel" => self.handle_tasks_cancel(id.clone(), params),
            "ping" => JsonRpcResponse::success(id.clone(), serde_json::json!({})),
            other => JsonRpcResponse::error(
                id.clone(),
                ErrorCode::METHOD_NOT_FOUND.0,
                format!("unknown method `{other}`"),
            ),
        };
        Some(response)
    }

    /// `initialize` — return a minimal `ServerInfo` matching the stdio
    /// transport's shape. v0.10.2 returns the static info; the
    /// sampling-capability gating that lives in the rmcp `ServerHandler`
    /// path is intentionally not replicated here — HTTP has no `Peer`
    /// to call back into. Tenants configured with `[llm] mode =
    /// "mcp_sampling"` will see sampling failures at consolidate-time
    /// instead of at `initialize` (documented in v0.10.2 dev log).
    fn handle_initialize(
        &self,
        id: serde_json::Value,
        _params: serde_json::Value,
    ) -> JsonRpcResponse {
        // `protocolVersion` reports Solo's explicitly tracked latest
        // supported MCP protocol version. Keep this tied to
        // `mcp_protocol` so the initialize result, HTTP header
        // validation, and tests move together.
        //
        // `capabilities.tools = {}` is the bare-minimum capability set
        // (we expose tools, nothing else). `serverInfo` is pinned to
        // `{"name": "solo", "version": <crate version>}` per the
        // `server_info_identity_is_solo_not_rmcp_or_solo_api` invariant.
        let server_info = Implementation::new(
            "solo".to_string(),
            solo_core::build_info::version_with_build_metadata(),
        );
        let result = serde_json::json!({
            "protocolVersion": crate::mcp_protocol::MCP_PROTOCOL_VERSION_LATEST,
            "capabilities": {
                "tools": {},
                "resources": {},
                "tasks": {
                    "list": {},
                    "cancel": {},
                    "requests": {
                        "tools": {
                            "call": {}
                        }
                    }
                },
            },
            "serverInfo": server_info,
            "instructions": crate::mcp::SERVER_INSTRUCTIONS,
        });
        JsonRpcResponse::success(id, result)
    }

    /// `tools/list` — wraps [`SoloMcpServer::dispatch_list_tools`].
    fn handle_tools_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        let tools = self.server.dispatch_list_tools();
        let result = serde_json::json!({ "tools": tools });
        JsonRpcResponse::success(id, result)
    }

    async fn handle_resources_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        response_from_result(id, self.server.dispatch_list_resources().await)
    }

    fn handle_resource_templates_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        response_from_result(id, Ok(self.server.dispatch_list_resource_templates()))
    }

    async fn handle_resources_read(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
        let uri = match params.get("uri").and_then(|v| v.as_str()) {
            Some(uri) => uri,
            None => {
                return JsonRpcResponse::error(
                    id,
                    ErrorCode::INVALID_PARAMS.0,
                    "resources/read: missing `uri` field",
                );
            }
        };
        response_from_result(id, self.server.dispatch_read_resource(uri).await)
    }

    fn handle_tasks_list(&self, id: serde_json::Value) -> JsonRpcResponse {
        response_from_result(id, Ok(self.server.task_store().list()))
    }

    fn handle_tasks_get(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
        match serde_json::from_value::<rmcp::model::GetTaskInfoParams>(params) {
            Ok(params) => response_from_result(id, self.server.task_store().get(&params.task_id)),
            Err(e) => JsonRpcResponse::error(
                id,
                ErrorCode::INVALID_PARAMS.0,
                format!("tasks/get: invalid params: {e}"),
            ),
        }
    }

    fn handle_tasks_result(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
        match serde_json::from_value::<rmcp::model::GetTaskResultParams>(params) {
            Ok(params) => {
                response_from_result(id, self.server.task_store().result(&params.task_id))
            }
            Err(e) => JsonRpcResponse::error(
                id,
                ErrorCode::INVALID_PARAMS.0,
                format!("tasks/result: invalid params: {e}"),
            ),
        }
    }

    fn handle_tasks_cancel(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> JsonRpcResponse {
        match serde_json::from_value::<rmcp::model::CancelTaskParams>(params) {
            Ok(params) => {
                response_from_result(id, self.server.task_store().cancel(&params.task_id))
            }
            Err(e) => JsonRpcResponse::error(
                id,
                ErrorCode::INVALID_PARAMS.0,
                format!("tasks/cancel: invalid params: {e}"),
            ),
        }
    }

    fn handle_cancelled_notification(
        &self,
        params: Option<serde_json::Value>,
        session: Option<Arc<SessionState>>,
    ) {
        let Some(session) = session else {
            return;
        };
        let Some(params) = params else {
            return;
        };
        let Some(request_id) = params.get("requestId").or_else(|| params.get("request_id")) else {
            tracing::debug!("mcp-http: notifications/cancelled missing requestId");
            return;
        };
        let reason = params
            .get("reason")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
        session.cancel_request(json_rpc_id_key(request_id), reason);
    }

    /// `tools/call` — wraps [`SoloMcpServer::dispatch_tool`]. JSON-RPC
    /// `params` carries `{"name": "...", "arguments": {...}, "_meta":
    /// {"progressToken": ...}?}`.
    ///
    /// v0.11.0 P3: if `_meta.progressToken` is present in `params` AND
    /// the request carried an `Arc<SessionState>` (HTTP transport), the
    /// handler builds a [`ProgressReporter`] correlated to the client's
    /// token and threads it into [`SoloMcpServer::dispatch_tool`].
    /// Long-running tool handlers (`memory_ingest_document`,
    /// `memory_ingest_staged_document`, `memory_import_documents`,
    /// `memory_search_docs`, `memory_remember_batch`) call
    /// `reporter.report(...)` at sensible checkpoints. Other handlers
    /// ignore the reporter — backward compat preserved.
    async fn handle_tools_call(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        session: Option<Arc<SessionState>>,
    ) -> JsonRpcResponse {
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.to_string(),
            None => {
                return JsonRpcResponse::error(
                    id,
                    ErrorCode::INVALID_PARAMS.0,
                    "tools/call: missing `name` field",
                );
            }
        };
        // `arguments` is optional; treat absent / null as empty object so
        // tools with all-optional args (e.g. `memory_themes`) can be
        // called with `{}` from the wire.
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
        if params.get("task").is_some() {
            return match self.server.enqueue_tool_task(&name, arguments) {
                Ok(created) => response_from_result(id, Ok(created)),
                Err(e) => JsonRpcResponse::from_mcp_error(id, e),
            };
        }
        // v0.11.0 P3: parse the progress token from the call params'
        // `_meta` block. Only Some when the client opted in.
        let reporter = match (session.clone(), ProgressToken::from_meta(&params)) {
            (Some(s), Some(token)) => Some(ProgressReporter::new(s, token)),
            // Either no session (stdio) or no progressToken (client did
            // not opt in) → no reporter. The handlers see `None` and
            // skip emission silently.
            _ => None,
        };
        let request_id = json_rpc_id_key(&id);
        let cancellation = session
            .as_ref()
            .map(|session| {
                crate::mcp_task::CancellationToken::from_request(
                    session.clone(),
                    request_id.clone(),
                )
            })
            .unwrap_or_else(crate::mcp_task::CancellationToken::none);
        let result = self
            .server
            .dispatch_tool_with_cancellation(&name, arguments, reporter, cancellation)
            .await;
        if let Some(session) = session.as_ref() {
            session.clear_request_cancellation(&request_id);
        }
        match result {
            Ok(call_result) => {
                // Serialise the rmcp `CallToolResult` directly — it
                // already round-trips through serde and matches the
                // MCP wire shape the stdio transport emits.
                let result = match serde_json::to_value(&call_result) {
                    Ok(v) => v,
                    Err(e) => {
                        return JsonRpcResponse::error(
                            id,
                            ErrorCode::INTERNAL_ERROR.0,
                            format!("serialize tool result: {e}"),
                        );
                    }
                };
                JsonRpcResponse::success(id, result)
            }
            Err(e) => JsonRpcResponse::from_mcp_error(id, e),
        }
    }
}

fn response_from_result<T: Serialize>(
    id: serde_json::Value,
    result: std::result::Result<T, McpError>,
) -> JsonRpcResponse {
    match result {
        Ok(value) => match serde_json::to_value(value) {
            Ok(result) => JsonRpcResponse::success(id, result),
            Err(e) => JsonRpcResponse::error(
                id,
                ErrorCode::INTERNAL_ERROR.0,
                format!("serialize response: {e}"),
            ),
        },
        Err(e) => JsonRpcResponse::from_mcp_error(id, e),
    }
}

fn json_rpc_id_key(id: &serde_json::Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonrpc_success_serialises_with_jsonrpc_field() {
        let resp = JsonRpcResponse::success(serde_json::json!(1), serde_json::json!({"ok": true}));
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""id":1"#));
        assert!(s.contains(r#""result":{"ok":true}"#));
        assert!(!s.contains(r#""error":"#));
    }

    #[test]
    fn jsonrpc_error_serialises_with_error_field() {
        let resp = JsonRpcResponse::error(
            serde_json::json!(7),
            ErrorCode::METHOD_NOT_FOUND.0,
            "unknown method `foo`",
        );
        let s = serde_json::to_string(&resp).unwrap();
        assert!(s.contains(r#""jsonrpc":"2.0""#));
        assert!(s.contains(r#""id":7"#));
        assert!(s.contains(r#""error":{"#));
        assert!(s.contains(r#""code":-32601"#));
        assert!(!s.contains(r#""result":"#));
    }

    #[test]
    fn jsonrpc_notification_has_no_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
        let req: JsonRpcRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.method, "notifications/initialized");
        assert!(req.id.is_none());
    }

    #[test]
    fn jsonrpc_request_with_null_id_parses_as_notification() {
        // Per JSON-RPC 2.0 §4.1 `null` is discouraged for request ids;
        // serde's `#[serde(default)]` deserialises an explicit null the
        // same as a missing field, so both land as a notification
        // (no reply). Real MCP clients always send numeric/string ids.
        let raw = r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#;
        let req: JsonRpcRequest = serde_json::from_str(raw).unwrap();
        assert!(req.id.is_none());
    }
}
