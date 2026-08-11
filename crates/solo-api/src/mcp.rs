// SPDX-License-Identifier: Apache-2.0

//! MCP (Model Context Protocol) server for Solo.
//!
//! Exposes Solo memory tools to MCP clients (Claude Desktop, Cursor, etc.):
//!
//! Episode tools (v0.1+, with v0.9.2 additions):
//!   - `memory_remember(content, source_type?, source_id?, salience?)` —
//!     store an episode. Returns the new MemoryId. `salience` (v0.9.2+)
//!     is optional in [0.0, 1.0] and defaults to 0.5.
//!   - `memory_remember_batch(items)` (v0.9.2+) — atomically store N
//!     episodes in one writer-actor transaction. Each item has the
//!     same fields as `memory_remember`. Returns an ordered array of
//!     MemoryIds; either all items persist or none do.
//!   - `memory_recall(query, limit?)` — vector search. Returns the top-K
//!     matches with content + tier + status.
//!   - `memory_context(query, subject?, window_days?, limit?)` — combined
//!     recall + themes + facts + contradictions bundle for agent context.
//!   - `memory_update(memory_id, content)` — correct/supersede an active
//!     episode's content and refresh its embedding/index row.
//!   - `memory_inbox(limit?)` — list recent active memories with review
//!     state for a profile.
//!   - `memory_review(memory_id, state?, note?)` — approve, dismiss, or
//!     reset one Inbox review decision without changing memory content.
//!   - `memory_forget(memory_id, reason?)` — soft-delete an episode.
//!   - `memory_inspect(memory_id)` — return the full episode record.
//!   - `memory_attach(memory_id, doc_id? | asset_id?, ...)` — link a
//!     memory to an ingested document or persisted original-file asset.
//!   - `memory_link_document_asset(doc_id, asset_id, ...)` — link an
//!     ingested document to a retained original-file asset.
//!
//! Derived-layer tools (v0.4.0+):
//!   - `memory_themes(window_days?, limit?)` — list cluster themes.
//!   - `memory_facts_about(subject, ...)` — query the structured-fact
//!     knowledge graph (subject-predicate-object triples).
//!   - `memory_entities(query, limit?)` — discover entity ids from the
//!     structured-fact graph.
//!   - `memory_graph_paths(from, to, ...)` — find evidence-backed
//!     relationship paths between two entity nodes.
//!   - `memory_explain_provenance(edge_id)` — inspect the edge and
//!     evidence behind one relationship path step.
//!   - `memory_contradictions(limit?)` — disagreements flagged during
//!     consolidation.
//!   - `memory_contradiction_resolve(...)` — mark a contradiction resolved,
//!     unresolved, or reopened.
//!
//! Derived-layer tools (v0.5.0+):
//!   - `memory_inspect_cluster(cluster_id, full_content?)` — drill
//!     into one cluster's abstraction + source episodes (truncated).
//!
//! Document tools (v0.7.0+):
//!   - `memory_ingest_document(path)` — read a file from disk, split it
//!     into chunks, embed each, and store under documents/document_chunks.
//!   - `memory_search_docs(query, limit?)` — hybrid vector + lexical search
//!     restricted to document chunks; returns chunk content + parent-doc
//!     context.
//!   - `memory_inspect_document(doc_id)` — show one document's metadata
//!     plus a previewed list of its chunks.
//!   - `memory_list_documents(limit?, offset?, include_forgotten?)` —
//!     paginate over ingested documents, newest first.
//!   - `memory_list_assets(...)`, `memory_inspect_asset(...)`,
//!     `memory_list_document_assets(...)`, and
//!     `memory_list_memory_attachments(...)` expose persisted
//!     original-file asset metadata and links.
//!   - `memory_prepare_asset_download(...)` and
//!     `memory_prepare_document_source_download(...)` return authorized
//!     raw-byte download contracts for retained original files.
//!   - `memory_forget_asset(asset_id)` — mark a retained original-file
//!     asset deleted and remove its raw blob bytes while preserving
//!     document/memory provenance links.
//!   - `memory_forget_document(doc_id)` — soft-delete a document; chunks
//!     stop appearing in `memory_search_docs` and tombstone in HNSW.
//!
//! ## Transport
//!
//! `serve_stdio` wires the server to stdin/stdout for use as a subprocess
//! ("`claude_desktop_config.json` or `~/.cursor/mcp.json` invokes
//! `solo mcp-stdio`"). The function awaits a graceful shutdown when stdin
//! closes (parent disconnects) — same lifecycle as `solo daemon`'s
//! Ctrl+C path.
//!
//! ## What's deferred
//!
//! - SSE/HTTP transports — `rmcp` ships them, but v0.1 ships stdio only.
//! - `prompts/` and `resources/` capabilities — not needed for the
//!   four-tool surface; ServerHandler defaults return empty lists.
//! - Tool argument validation beyond JSON Schema typing — we trust rmcp
//!   to deserialize per the schema, then serde-deserialize into our
//!   typed param structs. Bad inputs surface as clear errors.

use std::sync::Arc;

use base64::Engine;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    AnnotateAble, CallToolRequestParams as CallToolRequestParam, CallToolResult, CancelTaskParams,
    CancelTaskResult, Content, CreateTaskResult, GetTaskInfoParams, GetTaskPayloadResult,
    GetTaskResult, GetTaskResultParams, Implementation, InitializeRequestParams, InitializeResult,
    ListResourceTemplatesResult, ListResourcesResult, ListTasksResult, ListToolsResult,
    PaginatedRequestParams as PaginatedRequestParam, ProtocolVersion, RawResource,
    RawResourceTemplate, ReadResourceRequestParams, ReadResourceResult, ResourceContents,
    ServerCapabilities, ServerInfo, TaskSupport, TasksCapability, Tool, ToolAnnotations,
    ToolExecution,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServiceExt};
use serde::{Deserialize, Serialize};
use solo_core::{AssetId, Confidence, DocumentId, EncodingContext, Episode, MemoryId, Tier};
use solo_storage::{EntitySplitRequest, LibraryHandle, MemoryLibrary, MemoryReviewState};
use std::str::FromStr;

/// The MCP server. Cheap to clone — every field is `Arc`-cloneable.
///
/// v0.8.0 P2: an MCP session resolves to **one tenant**. The session's
/// `tenant_handle` is resolved at `initialize` time (today: from the
/// CLI invocation via `solo mcp-stdio`; future versions
/// may resolve per-bearer-token via OIDC). Subsequent `tools/call`
/// invocations route through the cached handle without re-resolving.
/// Operators that need multi-tenant MCP spawn one `solo mcp-stdio`
/// subprocess per tenant.
#[derive(Clone)]
pub struct SoloMcpServer {
    inner: Arc<Inner>,
}

struct Inner {
    /// Multi-tenant registry shared across all sessions. Held so that a
    /// future MCP capability that lists/inspects other tenants has a
    /// path to them (out of scope for v0.8.0 P2). P3 (auth) will use
    /// this to re-resolve the tenant from a bearer-token claim.
    #[allow(dead_code)]
    registry: Arc<MemoryLibrary>,
    /// The tenant this MCP session speaks for. Resolved at session
    /// construction time.
    tenant: Arc<LibraryHandle>,
    /// Read-path aliases for the canonical `"user"` subject. Sourced
    /// from `solo.config.toml` `[identity] user_aliases`; threaded
    /// through to `solo_query::facts_about` so a query for `"alex"`
    /// also surfaces rows historically extracted as `"user"`. Empty
    /// vec = behave as today (no expansion).
    user_aliases: Vec<String>,
    workspace_file_access: crate::WorkspaceFileAccessPolicy,
    task_store: crate::mcp_task::TaskStore,
    /// v0.8.0 P4 audit-log principal for this MCP session. MCP is
    /// bearer-only (no OIDC story in the spec), so the principal is
    /// effectively `"bearer"` when the daemon was started with
    /// `--bearer-token-file` and `None` otherwise. Persisted here so
    /// every tool dispatch threads it into the audit emit without
    /// reconstructing it per call.
    audit_principal: Option<String>,
}

/// v0.9.0 P2: outcome of inspecting the tenant's `[llm]` config + the
/// peer's `sampling` capability at MCP `initialize` time.
///
/// Separating the decision from the actual slot write makes the
/// gating logic unit-testable without needing a real
/// `rmcp::Peer<RoleServer>` (whose constructors are private).
/// `SoloMcpServer::initialize` performs the match and routes to the
/// side-effect path; tests pin the table directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitializeDecision {
    /// Tenant's LLM backend doesn't require an MCP peer; the slot was
    /// populated eagerly at registry-open time (or stays `None` for
    /// `LlmConfig::None`). MCP initialize succeeds without writing the
    /// slot.
    Allow,
    /// Legacy `mcp_sampling` configuration is rejected with migration
    /// guidance regardless of the peer's advertised capabilities.
    RejectDeprecatedSampling,
}

/// v0.9.0 P2: decide the initialize outcome given the tenant's
/// `[llm]` config and whether the peer advertised the `sampling`
/// capability.
///
/// Pure function — no side effects, no rmcp peer required. Pinned by
/// `initialize_decision_*` tests.
pub fn initialize_decision(
    llm_settings: &Option<solo_storage::LlmSettings>,
    _peer_sampling_supported: bool,
) -> InitializeDecision {
    match llm_settings {
        Some(settings) if settings.requires_mcp_peer() => {
            InitializeDecision::RejectDeprecatedSampling
        }
        _ => InitializeDecision::Allow,
    }
}

/// v0.9.0 P2: locked error message body for both the daemon-startup
/// rejection guard and the MCP `initialize` capability gate (plan §3
/// Decision 4 / BLOCKER 2 resolution). Returned verbatim to the
/// operator so the commented-out TOML snippets are copy-pasteable.
///
/// Lives at module scope so the daemon startup path (in `solo-cli`)
/// and the `SoloMcpServer::initialize` hook share one source of truth
/// — a future audit-revision can grep the locked phrasing without
/// chasing two divergent copies.
pub fn sampling_capability_missing_error_message() -> String {
    [
        "LLM backend `mcp_sampling` has been retired because MCP sampling",
        "was deprecated by SEP-2577. Solo no longer calls back into an MCP",
        "client for model inference. Choose a direct backend instead.",
        "",
        "Pick one of:",
        "",
        "  # Anthropic (hosted):",
        "  [llm]",
        "  mode = \"anthropic\"",
        "  api_key_env = \"ANTHROPIC_API_KEY\"",
        "  model = \"claude-sonnet-4-6\"",
        "",
        "  # OpenAI (hosted):",
        "  [llm]",
        "  mode = \"openai\"",
        "  api_key_env = \"OPENAI_API_KEY\"",
        "  model = \"gpt-5.6-terra\"",
        "",
        "  # Ollama (local daemon):",
        "  [llm]",
        "  mode = \"ollama\"",
        "  base_url = \"http://localhost:11434\"",
        "  model = \"qwen3-coder:30b\"",
        "",
        "  # None (cluster-only; abstractions skipped):",
        "  [llm]",
        "  mode = \"none\"",
        "",
        "Review the [llm] settings in solo.config.toml and restart Solo.",
    ]
    .join("\n")
}

/// v0.8.1 P2: env var name MCP clients set when launching the server
/// process to attribute audit rows on the stdio transport. Closes the
/// v0.8.0 known-issue gap where MCP audit rows always carried
/// `principal_subject = NULL` on the daemon path.
///
/// Precedence (when the future HTTP-MCP transport lands):
///   1. `Authorization: Bearer <token>` header on the HTTP-MCP request
///      (resolved through `AuthConfig::Bearer` validator).
///   2. `SOLO_MCP_PRINCIPAL_TOKEN` env var on the spawned process.
///
/// For the v0.8.x stdio-only world only the env-var path applies; the
/// header path is a no-op (no HTTP transport wired). The constant lives
/// at module scope so external callers (CLI subcommand, tests) reference
/// it by name rather than re-typing the string literal.
pub const ENV_MCP_PRINCIPAL_TOKEN: &str = "SOLO_MCP_PRINCIPAL_TOKEN";

/// Server-wide guidance returned in MCP `initialize` responses.
///
/// Codex reads the first part of this field while deciding whether to use
/// Solo's tools, so keep the opening sentences self-contained.
pub const SERVER_INSTRUCTIONS: &str = "Solo is the user's persistent memory and document library. When prior \
     context, preferences, project decisions, known people, known repos, \
     or previous conversations could materially improve an answer, call \
     memory_context with a concise query before answering. Use Solo when \
     the user references something from earlier, asks what they said before, \
     asks about a person/project/place you may know, asks about ingested \
     notes or files, or asks to remember, recall, update, review, or forget \
     something. Do not store secrets, raw credentials, API keys, tokens, or \
     private content the user did not intend to persist.\n\n\
     Best first call for agent work: memory_context, which returns one \
     bounded bundle containing recall, themes, optional facts, and \
     contradictions. Use narrower tools when you need more detail or a \
     specific operation.\n\n\
     Tools to write or look up specific moments: memory_remember saves \
     something worth keeping, memory_update corrects one active saved item, \
     memory_recall searches past conversations by topic, memory_inspect \
     shows one saved item by id, and memory_forget deletes one saved item.\n\n\
     Tools for the bigger picture: memory_themes lists recent topics, \
     memory_facts_about answers what Solo knows about a person, project, or \
     place, memory_entities discovers graph entity ids by name, \
     memory_graph_paths finds evidence-backed paths between entity nodes, \
     memory_explain_provenance inspects the evidence behind a relationship, \
     memory_contradictions finds places where the user has said two things \
     that disagree, memory_contradiction_resolve marks a contradiction \
     resolved or reopened, and memory_inspect_cluster shows the raw \
     conversations behind one summary.\n\n\
     Tools for the user's documents: memory_ingest_document reads a file \
     from disk and adds it to Solo's library, memory_search_docs searches \
     ingested documents by topic, memory_inspect_document shows one \
     document's metadata plus chunk previews, memory_list_documents browses \
     documents by recency, and memory_forget_document drops a document from \
     the library. For files that live on the agent/client machine, use \
     document_upload_prepare first, then send raw bytes over the returned \
     HTTP upload contract exactly as provided, then call \
     document_upload_commit and memory_ingest_staged_document. Do not guess \
     upload routes, methods, or auth headers.";

/// v0.8.1 P2: resolve the MCP-session principal at `initialize`-time.
///
/// Reads `SOLO_MCP_PRINCIPAL_TOKEN` env var (stdio path); future HTTP-MCP
/// callers will pass the bearer header value in via the explicit
/// `header_value` arg. The header beats the env when both are present.
///
/// Returns `Some(subject)` on resolution success; `None` when neither
/// source carries a non-empty value. Empty / whitespace-only values are
/// treated as absent so an accidentally-set `SOLO_MCP_PRINCIPAL_TOKEN=""`
/// in a launcher script doesn't pin every audit row to a blank principal.
///
/// The current implementation treats the env var value as the principal
/// subject directly. A future hardening pass can validate against the
/// daemon's `[auth] bearer.token` config to refuse mismatched tokens —
/// today the env var is operator-trusted (same trust model as
/// `SOLO_PASSPHRASE`).
pub fn resolve_mcp_principal(header_value: Option<&str>) -> Option<String> {
    // HTTP-MCP path wins when configured.
    if let Some(h) = header_value {
        if let Some(token) = h.strip_prefix("Bearer ") {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                // Header carries the raw bearer token. Same shape as the
                // stdio env-var path: the *value* is the principal
                // subject in v0.8.1; v0.8.2+ may validate against a
                // configured token set and surface the JWT `sub` claim
                // instead.
                return Some(trimmed.to_string());
            }
        }
    }
    // Stdio env-var fallback.
    match std::env::var(ENV_MCP_PRINCIPAL_TOKEN) {
        Ok(v) => {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Err(_) => None,
    }
}

impl SoloMcpServer {
    /// Build a server speaking for `tenant` (v0.8.0 P2 — one MCP session
    /// ↔ one tenant). The registry is held so future capabilities can
    /// reach across tenants if needed; today every handler routes
    /// through `self.inner.tenant`.
    ///
    /// v0.8.1 P2: auto-resolves the audit principal from the
    /// `SOLO_MCP_PRINCIPAL_TOKEN` env var (see [`resolve_mcp_principal`]).
    /// When neither the env var nor a header is set, the principal stays
    /// `None` — preserving v0.8.0 behavior for single-user setups.
    pub fn new_for_tenant(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
    ) -> Self {
        let principal = resolve_mcp_principal(None);
        Self::new_for_tenant_with_principal(registry, tenant, user_aliases, principal)
    }

    pub fn new_for_tenant_with_workspace_file_access(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
        workspace_file_access: crate::WorkspaceFileAccessPolicy,
    ) -> Self {
        let principal = resolve_mcp_principal(None);
        Self::new_for_tenant_with_principal_and_workspace_file_access(
            registry,
            tenant,
            user_aliases,
            principal,
            workspace_file_access,
        )
    }

    /// v0.8.0 P4: like [`Self::new_for_tenant`], but records an explicit
    /// audit principal subject for every tool dispatch. MCP is
    /// bearer-only at v0.8.0 — the orchestration layer (today: the
    /// daemon's `--bearer-token-file` path) decides whether a session
    /// counts as "bearer-authenticated" and passes `Some("bearer")`;
    /// CLI / unauth paths pass `None`.
    ///
    /// v0.8.1 P2: when the caller passes `audit_principal = None`, the
    /// env-var auto-resolution still runs (in `new_for_tenant`). Callers
    /// who want to *explicitly* suppress env-var resolution can call
    /// this method with `None` after `std::env::remove_var(...)`, or use
    /// the dedicated test constructor that bypasses env reads.
    pub fn new_for_tenant_with_principal(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
        audit_principal: Option<String>,
    ) -> Self {
        Self::new_for_tenant_with_principal_and_workspace_file_access(
            registry,
            tenant,
            user_aliases,
            audit_principal,
            crate::WorkspaceFileAccessPolicy::unrestricted(),
        )
    }

    pub fn new_for_tenant_with_principal_and_workspace_file_access(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
        audit_principal: Option<String>,
        workspace_file_access: crate::WorkspaceFileAccessPolicy,
    ) -> Self {
        Self::new_for_tenant_with_principal_workspace_file_access_and_tasks(
            registry,
            tenant,
            user_aliases,
            audit_principal,
            workspace_file_access,
            crate::mcp_task::TaskStore::new(),
        )
    }

    pub fn new_for_tenant_with_principal_workspace_file_access_and_tasks(
        registry: Arc<MemoryLibrary>,
        tenant: Arc<LibraryHandle>,
        user_aliases: Vec<String>,
        audit_principal: Option<String>,
        workspace_file_access: crate::WorkspaceFileAccessPolicy,
        task_store: crate::mcp_task::TaskStore,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                registry,
                tenant,
                user_aliases,
                workspace_file_access,
                task_store,
                audit_principal,
            }),
        }
    }
}

/// Convenience: run the server over stdio and await its termination.
/// Returns when stdin closes (parent disconnect) or the runtime exits.
pub async fn serve_stdio(server: SoloMcpServer) -> anyhow::Result<()> {
    use rmcp::transport::io::stdio;
    let (stdin, stdout) = stdio();
    let running = server.serve((stdin, stdout)).await?;
    running.waiting().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tool argument schemas
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberArgs {
    pub content: String,
    #[serde(default)]
    pub source_type: Option<String>,
    #[serde(default)]
    pub source_id: Option<String>,
    /// v0.9.2 — optional salience in [0.0, 1.0]. `None` → 0.5 (preserves
    /// pre-v0.9.2 behaviour). Out-of-range values are rejected by
    /// [`Self::validate_salience`] before reaching the writer.
    #[serde(default)]
    pub salience: Option<f32>,
}

/// v0.9.2 — one item in a `memory_remember_batch` request.
///
/// Mirrors [`RememberArgs`] field-for-field minus the wrapper-tool
/// invariant: callers pass an array of these inside [`RememberBatchArgs`].
/// All items in a batch are persisted in a single `BEGIN IMMEDIATE`
/// transaction (per dev-log 0120 §3 Decision A) so partial-failure
/// scenarios are impossible from the client's perspective — either
/// every item lands or none do.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberItem {
    pub content: String,
    #[serde(default)]
    pub source_type: Option<String>,
    #[serde(default)]
    pub source_id: Option<String>,
    /// Optional salience in [0.0, 1.0]; `None` → 0.5. See
    /// [`RememberArgs::salience`].
    #[serde(default)]
    pub salience: Option<f32>,
}

/// v0.9.2 — args for the new `memory_remember_batch` MCP tool.
///
/// Wraps `Vec<RememberItem>`. The handler validates `items.is_empty()`
/// and `items.len() > MAX_REMEMBER_BATCH_SIZE` before any embedding
/// work; per-item content/salience is validated immediately afterwards.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberBatchArgs {
    pub items: Vec<RememberItem>,
}

/// Validate that an optional salience value is well-formed (NaN-free
/// and inside `[0.0, 1.0]`). Centralised so both `memory_remember` and
/// `memory_remember_batch` share the same rejection shape.
fn validate_salience(salience: Option<f32>) -> std::result::Result<(), McpError> {
    if let Some(s) = salience {
        if !s.is_finite() || !(0.0..=1.0).contains(&s) {
            return Err(McpError::invalid_params(
                format!("salience must be in [0.0, 1.0]; got {s}"),
                None,
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallArgs {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryContextArgs {
    pub query: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub window_days: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetArgs {
    pub memory_id: String,
    #[serde(default = "default_forget_reason")]
    pub reason: String,
}

fn default_forget_reason() -> String {
    "user-initiated via MCP".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectArgs {
    pub memory_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateArgs {
    pub memory_id: String,
    pub content: String,
}

// Memory Inbox review tools, followed by Path 1 derived-layer tools
// (v0.4.0+) that query the Steward's outputs. These handlers translate
// JSON args to function args and serialise query results for the MCP wire.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxArgs {
    #[serde(default = "default_inbox_limit")]
    pub limit: usize,
}

fn default_inbox_limit() -> usize {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewArgs {
    pub memory_id: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn parse_review_state(
    raw: Option<&str>,
) -> std::result::Result<Option<MemoryReviewState>, McpError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let state = raw.trim();
    match state {
        "" | "needs_review" | "reset" | "clear" => Ok(None),
        "approved" => Ok(Some(MemoryReviewState::Approved)),
        "dismissed" => Ok(Some(MemoryReviewState::Dismissed)),
        other => Err(McpError::invalid_params(
            format!(
                "memory_review: state must be approved, dismissed, needs_review, reset, or null; got {other:?}"
            ),
            None,
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemesArgs {
    /// Optional time window in days; `None` = unfiltered, return up
    /// to `limit` most-recent themes across all time. `Some(7)` =
    /// "themes from the last week".
    #[serde(default)]
    pub window_days: Option<i64>,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactsAboutArgs {
    /// Subject id to query — required (predicate-only scans
    /// intentionally not supported).
    pub subject: String,
    #[serde(default)]
    pub predicate: Option<String>,
    #[serde(default)]
    pub since_ms: Option<i64>,
    #[serde(default)]
    pub until_ms: Option<i64>,
    /// v0.5.1 Priority 8 — widen the query to also match rows where
    /// `subject` appears as the object (e.g. surface "Sam pushes back
    /// on PRs about Maya" under `facts_about(subject="maya")`).
    /// Default `false` preserves v0.5.0 behaviour.
    #[serde(default)]
    pub include_as_object: bool,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitiesArgs {
    pub query: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitySplitReviewArgs {
    pub entity_id: String,
    #[serde(default)]
    pub affected_aliases: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphPathsArgs {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub max_hops: Option<u8>,
    #[serde(default)]
    pub as_of_ms: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplainProvenanceArgs {
    pub edge_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionsArgs {
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_contradiction_status() -> String {
    "resolved".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContradictionResolveArgs {
    pub a_id: String,
    pub b_id: String,
    pub kind: String,
    #[serde(default = "default_contradiction_status")]
    pub status: String,
    #[serde(default)]
    pub resolution_note: Option<String>,
    #[serde(default)]
    pub winning_triple_id: Option<String>,
}

/// Args for `memory_inspect_cluster` (v0.5.0 Priority 3). `cluster_id`
/// is required; `full_content` is opt-in for the rare power-user case
/// where 200-char-per-episode truncation is too aggressive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectClusterArgs {
    pub cluster_id: String,
    /// If `true`, episode `content` fields are returned verbatim. If
    /// `false` or omitted (the default), each episode's content is
    /// truncated to `solo_query::EPISODE_TRUNCATE_CHARS` chars with a
    /// trailing `…`.
    #[serde(default)]
    pub full_content: bool,
}

// Document tools (v0.7.0+). Five args structs paired with five handlers.
// Wire shapes per `docs/dev-log/0083-v0.7.0-implementation-plan.md` §2 P5.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestDocumentArgs {
    /// Server-side filesystem path to the file to ingest. Must be
    /// readable by the Solo process. The writer parses the file by
    /// extension, splits it into ~500-token chunks, embeds each, and
    /// stores them under `documents` + `document_chunks`.
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentUploadPrepareArgs {
    pub filename: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    pub size_bytes: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentUploadStatusArgs {
    pub upload_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentUploadChunkBase64Args {
    pub upload_id: String,
    pub offset: u64,
    pub chunk_base64: String,
    #[serde(default)]
    pub upload_length: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentUploadCommitArgs {
    pub upload_id: String,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentUploadAbortArgs {
    pub upload_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestStagedDocumentArgs {
    pub staged_uri: String,
    #[serde(default)]
    pub retain_source_file: bool,
    #[serde(default)]
    pub store_original_file: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachMemoryArgs {
    pub memory_id: String,
    #[serde(default)]
    pub doc_id: Option<String>,
    #[serde(default)]
    pub asset_id: Option<String>,
    #[serde(default)]
    pub relation_type: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkDocumentAssetArgs {
    pub doc_id: String,
    pub asset_id: String,
    #[serde(default)]
    pub relation_type: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareAssetDownloadArgs {
    pub asset_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrepareDocumentSourceDownloadArgs {
    pub doc_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportDocumentsArgs {
    pub path: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_true")]
    pub recursive: bool,
    #[serde(default = "default_import_max_files")]
    pub max_files: usize,
    #[serde(default)]
    pub store_original_file: bool,
}

fn default_true() -> bool {
    true
}

fn default_import_max_files() -> usize {
    DEFAULT_IMPORT_MAX_FILES
}

const DEFAULT_IMPORT_MAX_FILES: usize = 500;
const MAX_IMPORT_MAX_FILES: usize = 5_000;
const MAX_IMPORT_VISITED_ENTRIES: usize = 20_000;

#[derive(Debug, Clone, Serialize)]
struct ImportFile {
    path: String,
    bytes: u64,
    #[serde(skip_serializing)]
    path_buf: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize)]
struct ImportResult {
    path: String,
    bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    doc_id: Option<String>,
    chunks_persisted: u32,
    bytes_ingested: u64,
    deduped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset: Option<solo_storage::StoredAssetReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    document_asset_link: Option<solo_storage::DocumentAssetLinkReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    asset_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ImportResponse {
    path: String,
    source: String,
    source_label: String,
    dry_run: bool,
    recursive: bool,
    truncated: bool,
    total_files: usize,
    total_bytes: u64,
    store_original_file: bool,
    imported: u32,
    deduped: u32,
    failed: u32,
    chunks_persisted: u32,
    assets_retained: u32,
    assets_deduped: u32,
    asset_links: u32,
    asset_failed: u32,
    workspace_roots: WorkspaceRootsResponse,
    files: Vec<ImportFile>,
    results: Vec<ImportResult>,
}

#[derive(Debug, Clone, Serialize)]
struct WorkspaceRootsResponse {
    restricted: bool,
    allowed_roots: Vec<String>,
}

#[derive(Debug)]
struct ImportIngestSummary {
    imported: u32,
    deduped: u32,
    failed: u32,
    chunks_persisted: u32,
    assets_retained: u32,
    assets_deduped: u32,
    asset_links: u32,
    asset_failed: u32,
    results: Vec<ImportResult>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportSource {
    All,
    Markdown,
    Text,
    MarkdownText,
    Json,
}

impl ImportSource {
    fn response_source(self) -> &'static str {
        match self {
            Self::All => "native",
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::MarkdownText => "markdown_text",
            Self::Json => "json",
        }
    }

    fn response_label(self) -> &'static str {
        match self {
            Self::All => "Documents",
            Self::Markdown => "Markdown",
            Self::Text => "Text",
            Self::MarkdownText => "Markdown/Text",
            Self::Json => "JSON",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchDocsArgs {
    pub query: String,
    #[serde(default = "default_search_docs_limit")]
    pub limit: usize,
}

fn default_search_docs_limit() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectDocumentArgs {
    pub doc_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDocumentsArgs {
    #[serde(default = "default_list_documents_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    /// If `true`, also include documents the user has forgotten. Default
    /// `false` matches the agent-UX expectation that recall + listing
    /// ignore soft-deleted rows.
    #[serde(default)]
    pub include_forgotten: bool,
}

fn default_list_documents_limit() -> usize {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListAssetsArgs {
    #[serde(default = "default_list_documents_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub include_deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectAssetArgs {
    pub asset_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetAssetArgs {
    pub asset_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListDocumentAssetsArgs {
    pub doc_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListMemoryAttachmentsArgs {
    pub memory_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForgetDocumentArgs {
    pub doc_id: String,
}

// ---------------------------------------------------------------------------
// ServerHandler implementation
// ---------------------------------------------------------------------------

impl ServerHandler for SoloMcpServer {
    fn get_info(&self) -> ServerInfo {
        // rmcp 1.x: ServerInfo is non-exhaustive AND lives in another crate,
        // so neither struct-literal nor functional-update syntax (..) is
        // allowed from outside. Build via mut on a Default::default().
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_resources()
            .enable_tasks_with(TasksCapability::server_default())
            .build();
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = capabilities;
        // v0.9.1 P1 Fix 1 — `Implementation::from_build_env()` reads
        // `CARGO_PKG_NAME` + `CARGO_PKG_VERSION` from rmcp's OWN build
        // environment (the helper lives in rmcp, so the proc-macro
        // expansion captures rmcp's manifest, not ours). On v0.9.0 every
        // Solo MCP daemon self-identified as `{name: "rmcp", version: "1.7.0"}`.
        // Pinned by `tests::server_info_identity_is_solo_not_rmcp_or_solo_api`.
        // The literal `"solo"` (not `env!("CARGO_PKG_NAME")`) is deliberate:
        // this crate is `solo-api`, but the operator-facing identity is
        // the binary name `solo`.
        info.server_info = Implementation::new(
            "solo".to_string(),
            solo_core::build_info::version_with_build_metadata(),
        );
        info.instructions = Some(SERVER_INSTRUCTIONS.to_string());
        info
    }

    /// Cache peer information and reject retired `mcp_sampling`
    /// configuration with direct-backend migration guidance.
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<InitializeResult, McpError> {
        // Defer to rmcp's default for peer-info caching (matches the
        // `if peer_info().is_none()` shape).
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request.clone());
        }

        let llm_settings = self.inner.tenant.config().llm.as_ref().cloned();
        match initialize_decision(&llm_settings, request.capabilities.sampling.is_some()) {
            InitializeDecision::Allow => {}
            InitializeDecision::RejectDeprecatedSampling => {
                return Err(McpError::invalid_request(
                    sampling_capability_missing_error_message(),
                    None,
                ));
            }
        }

        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: build_tools(),
            next_cursor: None,
            ..Default::default()
        })
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourcesResult, McpError> {
        self.dispatch_list_resources().await
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListResourceTemplatesResult, McpError> {
        Ok(self.dispatch_list_resource_templates())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ReadResourceResult, McpError> {
        self.dispatch_read_resource(&request.uri).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        build_tools()
            .into_iter()
            .find(|tool| tool.name.as_ref() == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, McpError> {
        let CallToolRequestParam {
            name, arguments, ..
        } = request;
        let args_value = serde_json::Value::Object(arguments.unwrap_or_default());
        // v0.11.0 P3: stdio transport has no per-session broadcast
        // channel to publish progress events through (one process =
        // one tenant = one implicit "session" for the subprocess's
        // lifetime). Pass `None` — handlers see it and skip the
        // emission code paths silently.
        self.dispatch_tool(&name, args_value, None).await
    }

    async fn enqueue_task(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CreateTaskResult, McpError> {
        let CallToolRequestParam {
            name, arguments, ..
        } = request;
        let args_value = serde_json::Value::Object(arguments.unwrap_or_default());
        self.enqueue_tool_task(name.as_ref(), args_value)
    }

    async fn list_tasks(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListTasksResult, McpError> {
        Ok(self.inner.task_store.list())
    }

    async fn get_task_info(
        &self,
        request: GetTaskInfoParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<GetTaskResult, McpError> {
        self.inner.task_store.get(&request.task_id)
    }

    async fn get_task_result(
        &self,
        request: GetTaskResultParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<GetTaskPayloadResult, McpError> {
        self.inner.task_store.result(&request.task_id)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> std::result::Result<CancelTaskResult, McpError> {
        self.inner.task_store.cancel(&request.task_id)
    }
}

impl SoloMcpServer {
    /// Direct tool-dispatch path used by both `call_tool` (the
    /// ServerHandler trait method, behind the rmcp protocol layer) and
    /// in-process tests that don't want to spin up a full transport pair.
    /// Bypasses `RequestContext` (which requires a `Peer` not constructible
    /// outside rmcp internals).
    ///
    /// v0.11.0 P3: `progress` is `Some` only when the HTTP transport
    /// dispatched the request AND the client opted in via
    /// `_meta.progressToken`. The three long-running handlers
    /// (`memory_ingest_document`, `memory_search_docs`,
    /// `memory_ingest_staged_document`, `memory_import_documents`,
    /// `memory_search_docs`, and `memory_remember_batch`) consult the
    /// reporter; other handlers ignore it (backward compat with stdio
    /// and with HTTP clients that did not opt in).
    pub async fn dispatch_tool(
        &self,
        name: &str,
        args_value: serde_json::Value,
        progress: Option<crate::mcp_progress::ProgressReporter>,
    ) -> std::result::Result<CallToolResult, McpError> {
        self.dispatch_tool_with_cancellation(
            name,
            args_value,
            progress,
            crate::mcp_task::CancellationToken::none(),
        )
        .await
    }

    pub async fn dispatch_tool_with_cancellation(
        &self,
        name: &str,
        args_value: serde_json::Value,
        progress: Option<crate::mcp_progress::ProgressReporter>,
        cancellation: crate::mcp_task::CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        cancellation.check()?;
        match name {
            "memory_remember" => {
                let args: RememberArgs = parse_args(&args_value)?;
                self.handle_remember(args).await
            }
            "memory_remember_batch" => {
                let args: RememberBatchArgs = parse_args(&args_value)?;
                self.handle_remember_batch(args, progress, cancellation)
                    .await
            }
            "memory_recall" => {
                let args: RecallArgs = parse_args(&args_value)?;
                self.handle_recall(args).await
            }
            "memory_context" => {
                let args: MemoryContextArgs = parse_args(&args_value)?;
                self.handle_memory_context(args).await
            }
            "memory_forget" => {
                let args: ForgetArgs = parse_args(&args_value)?;
                self.handle_forget(args).await
            }
            "memory_inspect" => {
                let args: InspectArgs = parse_args(&args_value)?;
                self.handle_inspect(args).await
            }
            "memory_update" => {
                let args: UpdateArgs = parse_args(&args_value)?;
                self.handle_update(args).await
            }
            "memory_inbox" => {
                let args: InboxArgs = parse_args(&args_value)?;
                self.handle_inbox(args).await
            }
            "memory_review" => {
                let args: ReviewArgs = parse_args(&args_value)?;
                self.handle_review(args).await
            }
            "memory_attach" => {
                let args: AttachMemoryArgs = parse_args(&args_value)?;
                self.handle_attach_memory(args).await
            }
            "memory_link_document_asset" => {
                let args: LinkDocumentAssetArgs = parse_args(&args_value)?;
                self.handle_link_document_asset(args).await
            }
            "memory_themes" => {
                let args: ThemesArgs = parse_args(&args_value)?;
                self.handle_themes(args).await
            }
            "memory_facts_about" => {
                let args: FactsAboutArgs = parse_args(&args_value)?;
                self.handle_facts_about(args).await
            }
            "memory_entities" => {
                let args: EntitiesArgs = parse_args(&args_value)?;
                self.handle_entities(args).await
            }
            "memory_request_entity_split" => {
                let args: EntitySplitReviewArgs = parse_args(&args_value)?;
                self.handle_request_entity_split(args).await
            }
            "memory_graph_paths" => {
                let args: GraphPathsArgs = parse_args(&args_value)?;
                self.handle_graph_paths(args).await
            }
            "memory_explain_provenance" => {
                let args: ExplainProvenanceArgs = parse_args(&args_value)?;
                self.handle_explain_provenance(args).await
            }
            "memory_contradictions" => {
                let args: ContradictionsArgs = parse_args(&args_value)?;
                self.handle_contradictions(args).await
            }
            "memory_contradiction_resolve" => {
                let args: ContradictionResolveArgs = parse_args(&args_value)?;
                self.handle_contradiction_resolve(args).await
            }
            "memory_inspect_cluster" => {
                let args: InspectClusterArgs = parse_args(&args_value)?;
                self.handle_inspect_cluster(args).await
            }
            "memory_ingest_document" => {
                let args: IngestDocumentArgs = parse_args(&args_value)?;
                self.handle_ingest_document(args, progress, cancellation)
                    .await
            }
            "document_upload_prepare" => {
                let args: DocumentUploadPrepareArgs = parse_args(&args_value)?;
                self.handle_document_upload_prepare(args).await
            }
            "document_upload_status" => {
                let args: DocumentUploadStatusArgs = parse_args(&args_value)?;
                self.handle_document_upload_status(args).await
            }
            "document_upload_chunk_base64" => {
                let args: DocumentUploadChunkBase64Args = parse_args(&args_value)?;
                self.handle_document_upload_chunk_base64(args).await
            }
            "document_upload_commit" => {
                let args: DocumentUploadCommitArgs = parse_args(&args_value)?;
                self.handle_document_upload_commit(args).await
            }
            "document_upload_abort" => {
                let args: DocumentUploadAbortArgs = parse_args(&args_value)?;
                self.handle_document_upload_abort(args).await
            }
            "memory_ingest_staged_document" => {
                let args: IngestStagedDocumentArgs = parse_args(&args_value)?;
                self.handle_ingest_staged_document(args, progress, cancellation)
                    .await
            }
            "memory_import_documents" => {
                let args: ImportDocumentsArgs = parse_args(&args_value)?;
                self.handle_import_documents(args, progress, cancellation)
                    .await
            }
            "memory_search_docs" => {
                let args: SearchDocsArgs = parse_args(&args_value)?;
                self.handle_search_docs(args, progress, cancellation).await
            }
            "memory_inspect_document" => {
                let args: InspectDocumentArgs = parse_args(&args_value)?;
                self.handle_inspect_document(args).await
            }
            "memory_list_documents" => {
                let args: ListDocumentsArgs = parse_args(&args_value)?;
                self.handle_list_documents(args).await
            }
            "memory_list_assets" => {
                let args: ListAssetsArgs = parse_args(&args_value)?;
                self.handle_list_assets(args).await
            }
            "memory_inspect_asset" => {
                let args: InspectAssetArgs = parse_args(&args_value)?;
                self.handle_inspect_asset(args).await
            }
            "memory_prepare_asset_download" => {
                let args: PrepareAssetDownloadArgs = parse_args(&args_value)?;
                self.handle_prepare_asset_download(args).await
            }
            "memory_prepare_document_source_download" => {
                let args: PrepareDocumentSourceDownloadArgs = parse_args(&args_value)?;
                self.handle_prepare_document_source_download(args).await
            }
            "memory_list_document_assets" => {
                let args: ListDocumentAssetsArgs = parse_args(&args_value)?;
                self.handle_list_document_assets(args).await
            }
            "memory_list_memory_attachments" => {
                let args: ListMemoryAttachmentsArgs = parse_args(&args_value)?;
                self.handle_list_memory_attachments(args).await
            }
            "memory_forget_asset" => {
                let args: ForgetAssetArgs = parse_args(&args_value)?;
                self.handle_forget_asset(args).await
            }
            "memory_forget_document" => {
                let args: ForgetDocumentArgs = parse_args(&args_value)?;
                self.handle_forget_document(args).await
            }
            other => Err(McpError::invalid_params(
                format!("unknown tool `{other}`"),
                None,
            )),
        }
    }

    /// List the tools this server exposes. Mirrors `ServerHandler::list_tools`
    /// without requiring a RequestContext.
    pub fn dispatch_list_tools(&self) -> Vec<Tool> {
        build_tools()
    }

    pub fn task_store(&self) -> crate::mcp_task::TaskStore {
        self.inner.task_store.clone()
    }

    pub async fn dispatch_list_resources(
        &self,
    ) -> std::result::Result<ListResourcesResult, McpError> {
        let memories = solo_query::memory_inbox(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            50,
        )
        .await
        .map_err(solo_to_mcp)?;
        let documents = solo_query::list_documents(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            100,
            0,
            false,
        )
        .await
        .map_err(solo_to_mcp)?;
        let assets = solo_query::list_assets(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            100,
            0,
            false,
        )
        .await
        .map_err(solo_to_mcp)?;

        let mut resources = Vec::with_capacity(memories.len() + documents.len() + assets.len());
        resources.extend(memories.into_iter().map(|item| {
            RawResource::new(
                memory_resource_uri(&item.memory_id),
                format!("memory {}", item.memory_id),
            )
            .with_title(item.label)
            .with_description(item.preview)
            .with_mime_type("text/plain")
            .no_annotation()
        }));
        resources.extend(documents.into_iter().map(|doc| {
            RawResource::new(
                document_resource_uri(&doc.doc_id),
                doc.title
                    .clone()
                    .unwrap_or_else(|| format!("document {}", doc.doc_id)),
            )
            .with_title(
                doc.title
                    .clone()
                    .unwrap_or_else(|| format!("document {}", doc.doc_id)),
            )
            .with_description(
                doc.source
                    .clone()
                    .unwrap_or_else(|| "ingested Solo document".to_string()),
            )
            .with_mime_type(
                doc.mime_type
                    .unwrap_or_else(|| "application/json".to_string()),
            )
            .no_annotation()
        }));
        resources.extend(assets.into_iter().map(|asset| {
            RawResource::new(
                asset_resource_uri(&asset.asset_id),
                asset
                    .filename
                    .clone()
                    .unwrap_or_else(|| format!("asset {}", asset.asset_id)),
            )
            .with_title(
                asset
                    .filename
                    .clone()
                    .unwrap_or_else(|| format!("asset {}", asset.asset_id)),
            )
            .with_description(format!(
                "Solo persisted asset metadata: {} bytes, {}",
                asset.size_bytes, asset.mime_type
            ))
            .with_mime_type("application/json")
            .no_annotation()
        }));

        Ok(ListResourcesResult::with_all_items(resources))
    }

    pub fn dispatch_list_resource_templates(&self) -> ListResourceTemplatesResult {
        ListResourceTemplatesResult::with_all_items(vec![
            RawResourceTemplate::new("solo://memory/{memory_id}", "Solo memory")
                .with_title("Solo memory")
                .with_description("A saved Solo episodic memory.")
                .with_mime_type("text/plain")
                .no_annotation(),
            RawResourceTemplate::new("solo://document/{doc_id}", "Solo document")
                .with_title("Solo document")
                .with_description("An ingested Solo document with metadata and chunk previews.")
                .with_mime_type("application/json")
                .no_annotation(),
            RawResourceTemplate::new(
                "solo://document/{doc_id}/chunk/{chunk_id}",
                "Solo document chunk",
            )
            .with_title("Solo document chunk")
            .with_description("A full text chunk from an ingested Solo document.")
            .with_mime_type("text/plain")
            .no_annotation(),
            RawResourceTemplate::new("solo://asset/{asset_id}", "Solo asset")
                .with_title("Solo asset")
                .with_description("Persisted original-file asset metadata and links.")
                .with_mime_type("application/json")
                .no_annotation(),
        ])
    }

    pub async fn dispatch_read_resource(
        &self,
        uri: &str,
    ) -> std::result::Result<ReadResourceResult, McpError> {
        if let Some(memory_id) = uri.strip_prefix("solo://memory/") {
            let mid = MemoryId::from_str(memory_id)
                .map_err(|e| McpError::invalid_params(format!("invalid memory_id: {e}"), None))?;
            let row = solo_query::inspect_one(
                self.inner.tenant.read(),
                self.inner.tenant.audit(),
                self.inner.audit_principal.clone(),
                mid,
            )
            .await
            .map_err(solo_to_mcp)?;
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(row.content, uri).with_mime_type("text/plain"),
            ]));
        }

        if let Some(rest) = uri.strip_prefix("solo://document/") {
            if let Some((doc_id_raw, chunk_id)) = rest.split_once("/chunk/") {
                let doc_id = DocumentId::from_str(doc_id_raw)
                    .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?;
                let content = self.read_document_chunk_content(&doc_id, chunk_id).await?;
                return Ok(ReadResourceResult::new(vec![
                    ResourceContents::text(content, uri).with_mime_type("text/plain"),
                ]));
            }

            let doc_id = DocumentId::from_str(rest)
                .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?;
            let result_opt = solo_query::inspect_document(
                self.inner.tenant.read(),
                self.inner.tenant.audit(),
                self.inner.audit_principal.clone(),
                &doc_id,
            )
            .await
            .map_err(solo_to_mcp)?;
            let Some(record) = result_opt else {
                return Err(McpError::invalid_params(
                    format!("document {doc_id} not found"),
                    None,
                ));
            };
            let text = serde_json::to_string_pretty(&record).map_err(|e| {
                McpError::internal_error(format!("serialize document resource: {e}"), None)
            })?;
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("application/json"),
            ]));
        }

        if let Some(asset_id_raw) = uri.strip_prefix("solo://asset/") {
            let asset_id = AssetId::from_str(asset_id_raw)
                .map_err(|e| McpError::invalid_params(format!("invalid asset_id: {e}"), None))?;
            let result_opt = solo_query::inspect_asset(
                self.inner.tenant.read(),
                self.inner.tenant.audit(),
                self.inner.audit_principal.clone(),
                &asset_id,
            )
            .await
            .map_err(solo_to_mcp)?;
            let Some(record) = result_opt else {
                return Err(McpError::invalid_params(
                    format!("asset {asset_id} not found"),
                    None,
                ));
            };
            let text = serde_json::to_string_pretty(&record).map_err(|e| {
                McpError::internal_error(format!("serialize asset resource: {e}"), None)
            })?;
            return Ok(ReadResourceResult::new(vec![
                ResourceContents::text(text, uri).with_mime_type("application/json"),
            ]));
        }

        Err(McpError::invalid_params(
            format!("unsupported Solo resource URI `{uri}`"),
            None,
        ))
    }

    pub fn enqueue_tool_task(
        &self,
        name: &str,
        args_value: serde_json::Value,
    ) -> std::result::Result<CreateTaskResult, McpError> {
        let Some(tool) = self.get_tool(name) else {
            return Err(McpError::invalid_params(
                format!("unknown tool `{name}`"),
                None,
            ));
        };
        if tool.task_support() == TaskSupport::Forbidden {
            return Err(McpError::invalid_params(
                format!("tool `{name}` does not support task-based invocation"),
                None,
            ));
        }

        let (created, handle) = self
            .inner
            .task_store
            .start(format!("running tool `{name}`"));
        let server = self.clone();
        let task_store = self.inner.task_store.clone();
        let tool_name = name.to_string();
        tokio::spawn(async move {
            let result = server
                .dispatch_tool_with_cancellation(
                    &tool_name,
                    args_value,
                    None,
                    handle.cancellation_token(),
                )
                .await;
            match result {
                Ok(call_result) => match serde_json::to_value(&call_result) {
                    Ok(value) => task_store.complete(&handle, value),
                    Err(e) => task_store.fail(&handle, format!("serialize task result: {e}")),
                },
                Err(err) => task_store.fail(&handle, err.message.to_string()),
            }
        });
        Ok(created)
    }

    async fn read_document_chunk_content(
        &self,
        doc_id: &DocumentId,
        chunk_id: &str,
    ) -> std::result::Result<String, McpError> {
        let doc_id_str = doc_id.to_string();
        let chunk_id = chunk_id.to_string();
        let chunk_id_for_query = chunk_id.clone();
        let content: Option<String> = self
            .inner
            .tenant
            .read()
            .interact(move |conn| {
                conn.query_row(
                    "SELECT content FROM document_chunks WHERE doc_id = ?1 AND chunk_id = ?2",
                    rusqlite::params![doc_id_str, chunk_id_for_query],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
            })
            .await
            .map_err(solo_to_mcp)?;
        content.ok_or_else(|| {
            McpError::invalid_params(format!("document chunk {chunk_id} not found"), None)
        })
    }
}

fn memory_resource_uri(memory_id: &str) -> String {
    format!("solo://memory/{memory_id}")
}

fn document_resource_uri(doc_id: &str) -> String {
    format!("solo://document/{doc_id}")
}

fn document_chunk_resource_uri(doc_id: &str, chunk_id: &str) -> String {
    format!("solo://document/{doc_id}/chunk/{chunk_id}")
}

fn asset_resource_uri(asset_id: &str) -> String {
    format!("solo://asset/{asset_id}")
}

fn parse_args<T: serde::de::DeserializeOwned>(
    v: &serde_json::Value,
) -> std::result::Result<T, McpError> {
    serde_json::from_value(v.clone())
        .map_err(|e| McpError::invalid_params(format!("invalid tool arguments: {e}"), None))
}

fn solo_to_mcp(e: solo_core::Error) -> McpError {
    use solo_core::Error;
    match e {
        Error::NotFound(msg) => McpError::invalid_params(msg, None),
        Error::InvalidInput(msg) => McpError::invalid_params(msg, None),
        Error::Conflict(msg) => McpError::invalid_params(msg, None),
        Error::Forbidden(msg) => McpError::invalid_params(msg, None),
        other => McpError::internal_error(other.to_string(), None),
    }
}

fn workspace_roots_response(policy: &crate::WorkspaceFileAccessPolicy) -> WorkspaceRootsResponse {
    WorkspaceRootsResponse {
        restricted: policy.is_restricted(),
        allowed_roots: policy
            .allowed_roots()
            .iter()
            .map(|root| root.display().to_string())
            .collect(),
    }
}

fn parse_import_source(raw: Option<&str>) -> std::result::Result<ImportSource, McpError> {
    let Some(raw) = raw else {
        return Ok(ImportSource::All);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | "native" | "documents" => Ok(ImportSource::All),
        "markdown" | "md" => Ok(ImportSource::Markdown),
        "text" | "txt" => Ok(ImportSource::Text),
        "markdown_text" | "markdown-text" | "markdown/text" | "markdown+text" => {
            Ok(ImportSource::MarkdownText)
        }
        "json" => Ok(ImportSource::Json),
        other => Err(McpError::invalid_params(
            format!(
                "memory_import_documents: source must be native, markdown, text, markdown_text, or json; got {other}"
            ),
            None,
        )),
    }
}

fn native_import_allowed_extensions(configured: &[String], source: ImportSource) -> Vec<String> {
    let wanted: Option<&[&str]> = match source {
        ImportSource::All => None,
        ImportSource::Markdown => Some(&["md", "markdown"]),
        ImportSource::Text => Some(&["txt"]),
        ImportSource::MarkdownText => Some(&["md", "markdown", "txt"]),
        ImportSource::Json => Some(&["json", "jsonl", "ndjson"]),
    };

    let mut out = Vec::new();
    for ext in configured {
        let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
        if normalized.is_empty() {
            continue;
        }
        if wanted.is_none_or(|wanted| wanted.contains(&normalized.as_str()))
            && !out.contains(&normalized)
        {
            out.push(normalized);
        }
    }
    out.sort();
    out
}

fn native_import_home_dir() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                let drive = std::env::var_os("HOMEDRIVE")?;
                let path = std::env::var_os("HOMEPATH")?;
                let mut home = std::path::PathBuf::from(drive);
                home.push(path);
                Some(home)
            })
    } else {
        std::env::var_os("HOME").map(std::path::PathBuf::from)
    }
}

fn expand_home_prefix(raw_path: &str, home_dir: Option<&std::path::Path>) -> std::path::PathBuf {
    let Some(home) = home_dir else {
        return std::path::PathBuf::from(raw_path);
    };
    if raw_path == "~" {
        return home.to_path_buf();
    }
    if let Some(rest) = raw_path
        .strip_prefix("~/")
        .or_else(|| raw_path.strip_prefix("~\\"))
    {
        return home.join(rest);
    }
    std::path::PathBuf::from(raw_path)
}

fn chunk_config_from_document_config_mcp(
    config: &solo_storage::DocumentConfig,
) -> std::result::Result<solo_storage::ChunkConfig, McpError> {
    if config.chunk_token_target == 0 {
        return Err(McpError::invalid_params(
            "documents.chunk_token_target must be > 0".to_string(),
            None,
        ));
    }
    if config.chunk_overlap_tokens >= config.chunk_token_target {
        return Err(McpError::invalid_params(
            format!(
                "documents.chunk_overlap_tokens ({}) must be strictly less than documents.chunk_token_target ({})",
                config.chunk_overlap_tokens, config.chunk_token_target
            ),
            None,
        ));
    }
    Ok(solo_storage::ChunkConfig {
        target_tokens: config.chunk_token_target,
        overlap_tokens: config.chunk_overlap_tokens,
    })
}

fn collect_import_files(
    path: &std::path::Path,
    recursive: bool,
    allowed_extensions: &[String],
    max_files: usize,
    cancellation: &crate::mcp_task::CancellationToken,
) -> std::result::Result<(Vec<ImportFile>, bool), McpError> {
    cancellation.check()?;
    let metadata = std::fs::metadata(path).map_err(|e| {
        McpError::invalid_params(
            format!("path {} is not readable: {e}", path.display()),
            None,
        )
    })?;
    if metadata.is_file() {
        if !has_allowed_extension(path, allowed_extensions) {
            return Ok((Vec::new(), false));
        }
        return Ok((
            vec![ImportFile {
                path: path.display().to_string(),
                bytes: metadata.len(),
                path_buf: path.to_path_buf(),
            }],
            false,
        ));
    }
    if !metadata.is_dir() {
        return Err(McpError::invalid_params(
            format!("path is not a file or directory: {}", path.display()),
            None,
        ));
    }

    let mut files = Vec::new();
    let mut visited_entries = 0usize;
    let truncated = collect_import_dir(
        path,
        recursive,
        allowed_extensions,
        max_files,
        &mut visited_entries,
        &mut files,
        cancellation,
    )?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((files, truncated))
}

fn collect_import_dir(
    dir: &std::path::Path,
    recursive: bool,
    allowed_extensions: &[String],
    max_files: usize,
    visited_entries: &mut usize,
    out: &mut Vec<ImportFile>,
    cancellation: &crate::mcp_task::CancellationToken,
) -> std::result::Result<bool, McpError> {
    cancellation.check()?;
    let read = std::fs::read_dir(dir)
        .map_err(|e| McpError::invalid_params(format!("read_dir {}: {e}", dir.display()), None))?;

    let mut entries = Vec::new();
    let mut entry_cap_reached = false;
    for entry in read {
        cancellation.check()?;
        if *visited_entries >= MAX_IMPORT_VISITED_ENTRIES {
            entry_cap_reached = true;
            break;
        }
        *visited_entries += 1;
        let entry = entry.map_err(|e| {
            McpError::invalid_params(format!("read_dir {}: {e}", dir.display()), None)
        })?;
        entries.push((entry.path(), entry));
    }
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));

    let mut truncated = entry_cap_reached;
    for (path, entry) in entries {
        cancellation.check()?;
        if out.len() >= max_files {
            return Ok(true);
        }
        if is_hidden_entry(&path)? {
            continue;
        }
        let file_type = entry.file_type().map_err(|e| {
            McpError::invalid_params(format!("file_type {}: {e}", path.display()), None)
        })?;
        if file_type.is_dir() {
            if recursive
                && collect_import_dir(
                    &path,
                    recursive,
                    allowed_extensions,
                    max_files,
                    visited_entries,
                    out,
                    cancellation,
                )?
            {
                if out.len() >= max_files {
                    return Ok(true);
                }
                truncated = true;
            }
        } else if file_type.is_file() && has_allowed_extension(&path, allowed_extensions) {
            let bytes = entry
                .metadata()
                .map_err(|e| {
                    McpError::invalid_params(format!("metadata {}: {e}", path.display()), None)
                })?
                .len();
            out.push(ImportFile {
                path: path.display().to_string(),
                bytes,
                path_buf: path,
            });
            if out.len() >= max_files {
                return Ok(true);
            }
        }
    }
    Ok(truncated)
}

fn is_hidden_entry(path: &std::path::Path) -> std::result::Result<bool, McpError> {
    if is_dot_hidden_path(path) {
        return Ok(true);
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|e| McpError::invalid_params(format!("metadata {}: {e}", path.display()), None))?;
    Ok(has_platform_hidden_attribute(&metadata))
}

fn is_dot_hidden_path(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'))
}

#[cfg(windows)]
fn has_platform_hidden_attribute(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

#[cfg(target_os = "macos")]
fn has_platform_hidden_attribute(metadata: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    const UF_HIDDEN: u32 = 0x0000_8000;
    metadata.st_flags() & UF_HIDDEN != 0
}

#[cfg(not(any(windows, target_os = "macos")))]
fn has_platform_hidden_attribute(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn has_allowed_extension(path: &std::path::Path, allowed_extensions: &[String]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let ext = ext.trim_start_matches('.');
            allowed_extensions
                .iter()
                .any(|allowed| allowed.trim_start_matches('.').eq_ignore_ascii_case(ext))
        })
}

// ---------------------------------------------------------------------------
// Tool definitions (JSON Schema)
// ---------------------------------------------------------------------------

fn build_tools() -> Vec<Tool> {
    vec![
        Tool::new(
            "memory_remember",
            "Save something the user has told you — a fact, a \
             preference, a name, a date, a context — so you can pick \
             it up next conversation. Use whenever the user mentions \
             something they'd reasonably expect you to recall later \
             (\"I just started at Quotient\", \"my partner is Maya\"). \
             Returns the saved item's id.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The text to remember.",
                    },
                    "source_type": {
                        "type": "string",
                        "description": "Optional source-type tag (default: \"user_message\"). See docs/mcp/source-types.md for convention values.",
                    },
                    "source_id": {
                        "type": "string",
                        "description": "Optional upstream id for traceability.",
                    },
                    "salience": {
                        "type": "number",
                        "description": "Optional salience in [0.0, 1.0]; defaults to 0.5. Higher values bias toward recall ranking + retention. v0.9.2+.",
                        "minimum": 0.0,
                        "maximum": 1.0,
                    },
                },
                "required": ["content"],
            })),
        ),
        // v0.9.2 — atomic batched-remember for agentic clients. Wraps
        // every item in one BEGIN IMMEDIATE tx so a single
        // `memory_remember_batch` call either persists all N items or
        // none. Designed for batched agent turn flushes (per
        // dev-log 0120 §1).
        Tool::new(
            "memory_remember_batch",
            "Save several items atomically in one transaction — either \
             every item lands or none does. Use this when you have a \
             collection of related episodes from one logical step (a \
             conversation turn, a tool-output bundle, an ingest batch) \
             and partial success would leave the user's memory in a \
             confusing half-state. Each item carries the same fields as \
             memory_remember (content + optional source_type, source_id, \
             salience). Returns an ordered array of memory_ids matching \
             the input items. v0.9.2+.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": format!(
                            "Items to remember atomically. Max {} per call.",
                            solo_storage::MAX_REMEMBER_BATCH_SIZE,
                        ),
                        "minItems": 1,
                        // SOURCE OF TRUTH: solo_storage::MAX_REMEMBER_BATCH_SIZE.
                        // Both the numeric `maxItems` and the human-readable
                        // `description` above interpolate from this constant
                        // so they can never drift. Pinned by
                        // `remember_batch_maxitems_matches_max_batch_size`
                        // in the test module.
                        "maxItems": solo_storage::MAX_REMEMBER_BATCH_SIZE,
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "The text to remember.",
                                },
                                "source_type": {
                                    "type": "string",
                                    "description": "Optional source-type tag (default: \"user_message\"). See docs/mcp/source-types.md.",
                                },
                                "source_id": {
                                    "type": "string",
                                    "description": "Optional upstream id for traceability.",
                                },
                                "salience": {
                                    "type": "number",
                                    "description": "Optional salience in [0.0, 1.0]; defaults to 0.5.",
                                    "minimum": 0.0,
                                    "maximum": 1.0,
                                },
                            },
                            "required": ["content"],
                        },
                    },
                },
                "required": ["items"],
            })),
        ),
        Tool::new(
            "memory_recall",
            "Search past conversations with this user by topic or \
             phrase. Returns up to `limit` of the closest matches, \
             best match first. Use when the user references \
             something they said before (\"that book I told you \
             about\", \"the bug we were debugging last week\"). \
             Skips items the user has deleted.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The query text.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default 5).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": ["query"],
            })),
        ),
        Tool::new(
            "memory_context",
            "Build a compact working-memory bundle for an agent turn. \
             Use this near the start of a substantial answer or task \
             when remembered context may matter. It combines raw \
             episodic recall, recent themes, optional structured facts \
             about `subject`, and known contradictions so clients can \
             ground answers without making four separate calls.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural-language query for episodic recall.",
                    },
                    "subject": {
                        "type": "string",
                        "description": "Optional subject for structured facts. When present, facts also match object-position references.",
                    },
                    "window_days": {
                        "type": "integer",
                        "description": "Optional recency window in days for themes. Omit for unfiltered.",
                        "minimum": 1,
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Per-section maximum results (default 5).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": ["query"],
            })),
        ),
        Tool::new(
            "memory_forget",
            "Delete one saved item by id. Use when the user asks you \
             to forget something specific (\"forget that I said \
             X\"). The item stops appearing in future recalls. \
             Reversible only via backups.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "MemoryId to forget (UUID v7).",
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional free-form reason (logged, not yet persisted).",
                    },
                },
                "required": ["memory_id"],
            })),
        ),
        Tool::new(
            "memory_inspect",
            "Show the full record for one saved item — when it was \
             saved, where it came from, and the full text. Use after \
             memory_recall when you want the complete content of a \
             specific hit (recall results may be truncated).",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "MemoryId to inspect (UUID v7).",
                    },
                },
                "required": ["memory_id"],
            })),
        ),
        Tool::new(
            "memory_update",
            "Correct one active saved memory and refresh its embedding \
             and search index entry. Use when the user says a remembered \
             episode is wrong or outdated and provides the corrected \
             wording. Returns the updated memory id, rowid, content, and \
             timestamp.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "MemoryId to update (UUID v7).",
                    },
                    "content": {
                        "type": "string",
                        "description": "Replacement content for the active memory.",
                        "minLength": 1,
                    },
                },
                "required": ["memory_id", "content"],
            })),
        ),
        // Memory Inbox review tools.
        Tool::new(
            "memory_inbox",
            "List recent active memories with their Inbox review state. \
             Use when you need to help the user review, approve, dismiss, \
             or understand what is waiting in memory. Missing review_state \
             means the item still needs review.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum items to return (default 100, max 200).",
                        "minimum": 1,
                        "maximum": solo_query::INBOX_MAX_LIMIT,
                    },
                },
            })),
        ),
        Tool::new(
            "memory_review",
            "Set or clear one Memory Inbox review decision. This does not \
             edit or delete the memory content; it only marks the review \
             queue for this Solo profile.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "MemoryId to review (UUID v7).",
                    },
                    "state": {
                        "type": "string",
                        "description": "approved, dismissed, needs_review, reset, or omit/null to clear the review decision.",
                        "enum": ["approved", "dismissed", "needs_review", "reset"],
                    },
                    "note": {
                        "type": "string",
                        "description": "Optional review note.",
                    },
                },
                "required": ["memory_id"],
            })),
        ),
        Tool::new(
            "memory_attach",
            "Attach one active memory to either an ingested document or a \
             persisted original-file asset. Use when a remembered fact, \
             decision, or event should keep a durable reference to the \
             file that supports it. Exactly one of doc_id or asset_id is \
             required.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "MemoryId to attach from (UUID v7).",
                    },
                    "doc_id": {
                        "type": "string",
                        "description": "Optional document id target. Mutually exclusive with asset_id.",
                    },
                    "asset_id": {
                        "type": "string",
                        "description": "Optional persisted asset id target. Mutually exclusive with doc_id.",
                    },
                    "relation_type": {
                        "type": "string",
                        "description": "Short relation label such as evidence, source_file, reference, or related. Default related.",
                        "default": "related",
                    },
                    "note": {
                        "type": "string",
                        "description": "Optional human note about why this file is attached.",
                    },
                },
                "oneOf": [
                    {
                        "required": ["doc_id"],
                        "not": { "required": ["asset_id"] }
                    },
                    {
                        "required": ["asset_id"],
                        "not": { "required": ["doc_id"] }
                    }
                ],
                "required": ["memory_id"],
            })),
        ),
        Tool::new(
            "memory_link_document_asset",
            "Link one ingested document to a retained original-file asset. \
             Use when a source file was stored separately from document \
             ingestion, or when you need to repair provenance between a \
             document record and its exact original file. Repeated calls \
             with the same doc_id, asset_id, and relation_type return the \
             existing link.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "doc_id": {
                        "type": "string",
                        "description": "Document id to link from (UUID v7).",
                    },
                    "asset_id": {
                        "type": "string",
                        "description": "Persisted asset id to link to (UUID v7).",
                    },
                    "relation_type": {
                        "type": "string",
                        "description": "Short relation label such as source_upload, original_file, reference, or related. Default source_upload.",
                        "default": "source_upload",
                    },
                    "note": {
                        "type": "string",
                        "description": "Optional human note about why this file is linked to the document.",
                    },
                },
                "required": ["doc_id", "asset_id"],
            })),
        ),
        // Path 1 derived-layer tools (v0.4.0+) — query the Steward's
        // outputs. These are populated by `solo consolidate` and were
        // previously unreadable except via direct SQL.
        Tool::new(
            "memory_themes",
            "Recent topics the user has been thinking about. Use to \
             orient yourself at the start of a conversation, or when \
             the user asks \"what have I been up to\" / \"what was I \
             working on last week\". Pass `window_days` to scope \
             (e.g. 7 for last week); omit for all-time.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "window_days": {
                        "type": "integer",
                        "description": "Optional time window in days. Omit for unfiltered.",
                        "minimum": 1,
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default 5).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": [],
            })),
        ),
        Tool::new(
            "memory_facts_about",
            "Look up what you remember about a person, project, or \
             topic — names, dates, preferences, relationships. Use \
             when the user asks \"what do you know about Alex?\", \
             \"when did I start at Quotient?\", \"who is Maya?\", or \
             whenever you need grounded facts about someone or \
             something before answering. Subject is required (the \
             person/place/thing you're asking about); narrow further \
             with `predicate` (\"works_at\", \"lives_in\") or a date \
             range. Set `include_as_object=true` to also surface \
             facts where the subject appears on the receiving side of \
             a relationship (e.g. \"Sam pushes back on PRs about \
             Maya\" surfaces under facts_about(subject=\"Maya\", \
             include_as_object=true)). (Backed by \
             subject-predicate-object triples distilled from past \
             conversations.) Clients should set a 30s timeout on this \
             call; if exceeded, retry once or fall back to \
             `memory_recall`.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "subject": {
                        "type": "string",
                        "description": "Subject id to query (e.g. 'Sam').",
                    },
                    "predicate": {
                        "type": "string",
                        "description": "Optional predicate filter (e.g. 'works_at').",
                    },
                    "since_ms": {
                        "type": "integer",
                        "description": "Optional valid_from_ms lower bound (epoch ms).",
                    },
                    "until_ms": {
                        "type": "integer",
                        "description": "Optional valid_to_ms upper bound (epoch ms). NULL upper bounds (still-valid facts) pass through.",
                    },
                    "include_as_object": {
                        "type": "boolean",
                        "description": "If true, also match facts where `subject` appears as the object (e.g. 'Sam pushes back on PRs about Maya' surfaces under subject='Maya'). Default false.",
                        "default": false,
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default 5).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": ["subject"],
            })),
        ),
        Tool::new(
            "memory_entities",
            "Discover entity ids from the structured-fact graph. Use \
             before memory_facts_about when you are not sure how a \
             person, project, or topic is keyed in memory, or when the \
             user gives a partial name. Returns entity ids with fact \
             counts and common predicates.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Partial or exact entity id to search for.",
                        "minLength": 1,
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default 5).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": ["query"],
            })),
        ),
        Tool::new(
            "memory_request_entity_split",
            "Request a review operation to split aliases or labels out \
             of one canonical entity. This is an additive governance \
             write: Solo records an entity_review_ops row and a memory \
             revision for later review, but does not rewrite graph facts.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "entity_id": {
                        "type": "string",
                        "description": "Canonical entity id that currently owns the aliases.",
                        "minLength": 1,
                    },
                    "affected_aliases": {
                        "type": "array",
                        "description": "Aliases, labels, or surface forms that should be split out for review.",
                        "items": { "type": "string", "minLength": 1 },
                        "minItems": 1,
                    },
                    "reason": {
                        "type": "string",
                        "description": "Optional reviewer-facing reason for the split request.",
                    },
                },
                "required": ["entity_id", "affected_aliases"],
            })),
        ),
        Tool::new(
            "memory_graph_paths",
            "Find directed, evidence-backed relationship paths between \
             two entity nodes in the temporal memory graph. Pass entity \
             node ids such as `ent:Alex` and `ent:Solo`; returns \
             active one-hop and two-hop paths with edge ids that can be \
             inspected through the graph relationship API.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "from": {
                        "type": "string",
                        "description": "Source entity node id, prefixed as ent:<value>.",
                        "pattern": "^ent:.+"
                    },
                    "to": {
                        "type": "string",
                        "description": "Target entity node id, prefixed as ent:<value>.",
                        "pattern": "^ent:.+"
                    },
                    "max_hops": {
                        "type": "integer",
                        "description": "Maximum hops to search (default 2).",
                        "minimum": 1,
                        "maximum": crate::graph_paths::GRAPH_PATHS_MAX_HOPS,
                        "default": crate::graph_paths::GRAPH_PATHS_DEFAULT_MAX_HOPS
                    },
                    "as_of_ms": {
                        "type": "integer",
                        "description": "Optional epoch-ms instant; when present, every edge in the path must be valid at this time.",
                        "minimum": 0
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum paths to return after global score sorting.",
                        "minimum": 1,
                        "maximum": crate::graph_paths::GRAPH_PATHS_MAX_LIMIT,
                        "default": crate::graph_paths::GRAPH_PATHS_DEFAULT_LIMIT
                    }
                },
                "required": ["from", "to"],
            })),
        ),
        Tool::new(
            "memory_explain_provenance",
            "Inspect the evidence behind one relationship edge returned \
             by memory_graph_paths. Use the edge_id from a path step to \
             get the relationship metadata, supporting memory/document \
             references, confidence scores, timestamps, and a short \
             active-memory preview when available.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "edge_id": {
                        "type": "string",
                        "description": "Relationship edge id to inspect.",
                        "minLength": 1
                    }
                },
                "required": ["edge_id"],
            })),
        ),
        Tool::new(
            "memory_contradictions",
            "Find places where the user's stated beliefs or facts \
             disagree across conversations — flag disagreements \
             before answering. Use whenever you're about to rely on \
             a remembered fact that could have changed (jobs, \
             relationships, preferences, opinions); a disagreement \
             here means the user has told you both X and not-X over \
             time and you should ask which is current instead of \
             guessing. Each result shows both conflicting statements \
             with the topic.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default 5).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": [],
            })),
        ),
        Tool::new(
            "memory_contradiction_resolve",
            "Mark one flagged contradiction as resolved, unresolved, \
             or reopened. Use after the user clarifies which side is \
             current. Pass the a_id, b_id, and kind from \
             memory_contradictions; status defaults to resolved.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "a_id": {
                        "type": "string",
                        "description": "First contradiction id from memory_contradictions.",
                    },
                    "b_id": {
                        "type": "string",
                        "description": "Second contradiction id from memory_contradictions.",
                    },
                    "kind": {
                        "type": "string",
                        "description": "Contradiction kind from memory_contradictions.",
                    },
                    "status": {
                        "type": "string",
                        "enum": ["unresolved", "resolved", "reopened"],
                        "default": "resolved",
                        "description": "New lifecycle status.",
                    },
                    "resolution_note": {
                        "type": "string",
                        "description": "Optional human-readable clarification.",
                    },
                    "winning_triple_id": {
                        "type": "string",
                        "description": "Optional triple id to treat as the current/correct side.",
                    },
                },
                "required": ["a_id", "b_id", "kind"],
            })),
        ),
        Tool::new(
            "memory_inspect_cluster",
            "Show the raw conversations behind one summary. Returns \
             the one-line topic (the LLM-generated summary) and the \
             source conversations the topic was built from. Use \
             after memory_themes when the user asks \"show me the \
             raw context behind this\" or \"why does Solo think \
             that about cluster Y\". Source items are truncated to \
             200 chars unless `full_content` is set.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "cluster_id": {
                        "type": "string",
                        "description": "Cluster id to inspect (from memory_themes hits).",
                    },
                    "full_content": {
                        "type": "boolean",
                        "description": "If true, episode content is returned verbatim. Default false (truncate to 200 chars + ellipsis).",
                    },
                },
                "required": ["cluster_id"],
            })),
        ),
        // Document tools (v0.7.0+). RAG over user-supplied files —
        // markdown notes, PDFs, runbooks, code, etc. Same vector space
        // as episodes; same embedder; same HNSW index.
        Tool::new(
            "memory_ingest_document",
            "Read a file from disk and add it to the user's document \
             library so it becomes searchable alongside past \
             conversations. Use when the user asks you to remember a \
             whole file (\"add my notes/runbook.md\", \"ingest this \
             PDF\"). The file is split into ~500-token chunks and \
             each chunk is embedded; chunks then surface through \
             memory_search_docs. Returns the new document id, chunk \
             count, and a `deduped` flag (true if the same content \
             was already ingested under another id).",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Server-side absolute path to the file to ingest. The file must be readable by the Solo process.",
                    },
                },
                "required": ["path"],
            })),
        ),
        Tool::new(
            "document_upload_prepare",
            "Create a local Solo staging upload for a document that does \
             not already exist on the Solo machine. This is the MCP \
             control-plane step; the returned structured result is the \
             upload contract. Send raw bytes with exactly the returned \
             upload_method, upload_url, and required_headers. Do not infer \
             PUT vs PATCH, do not invent routes, and do not put file bytes \
             in JSON-RPC tool arguments. Supports files up to 100 MiB.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Leaf filename, including extension, for the staged document.",
                    },
                    "mime_type": {
                        "type": "string",
                        "description": "Optional declared MIME type. Solo still validates by extension/parser.",
                    },
                    "size_bytes": {
                        "type": "integer",
                        "description": "Exact file size in bytes. Maximum 104857600.",
                        "minimum": 1,
                        "maximum": 104857600,
                    },
                    "sha256": {
                        "type": "string",
                        "description": "Optional expected SHA-256 hex digest for final commit verification.",
                    },
                },
                "required": ["filename", "size_bytes"],
            })),
        ),
        Tool::new(
            "document_upload_status",
            "Return the current resumable-upload status for a staged \
             document upload. Use this after an interrupted HTTP upload \
             to learn the next byte offset before resuming.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "upload_id": {
                        "type": "string",
                        "description": "Upload id returned by document_upload_prepare.",
                    },
                },
                "required": ["upload_id"],
            })),
        ),
        Tool::new(
            "document_upload_chunk_base64",
            "Fallback upload path for small files when a client cannot send \
             raw HTTP PATCH bytes to upload_url. Prefer raw HTTP whenever \
             available. The decoded chunk must be <= the mcp_fallback limit \
             returned by document_upload_prepare.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "upload_id": {
                        "type": "string",
                        "description": "Upload id returned by document_upload_prepare.",
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Expected byte offset for this decoded chunk.",
                        "minimum": 0,
                    },
                    "chunk_base64": {
                        "type": "string",
                        "description": "Base64-encoded raw bytes for this small fallback chunk.",
                    },
                    "upload_length": {
                        "type": "integer",
                        "description": "Optional original file length; must match prepared size_bytes when supplied.",
                        "minimum": 1,
                    },
                },
                "required": ["upload_id", "offset", "chunk_base64"],
            })),
        ),
        Tool::new(
            "document_upload_commit",
            "Commit a completed staged upload after all bytes have been \
             sent over HTTP. Solo verifies the byte count and optional \
             SHA-256 digest, then returns a solo-staged:// URI that can \
             be ingested with memory_ingest_staged_document.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "upload_id": {
                        "type": "string",
                        "description": "Upload id returned by document_upload_prepare.",
                    },
                    "sha256": {
                        "type": "string",
                        "description": "Optional SHA-256 hex digest to verify at commit time.",
                    },
                },
                "required": ["upload_id"],
            })),
        ),
        Tool::new(
            "document_upload_abort",
            "Abort a staged document upload and delete any temporary \
             upload bytes. Use when the user cancels or the client cannot \
             finish the transfer.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "upload_id": {
                        "type": "string",
                        "description": "Upload id returned by document_upload_prepare.",
                    },
                },
                "required": ["upload_id"],
            })),
        ),
        Tool::new(
            "memory_ingest_staged_document",
            "Ingest a committed solo-staged:// document upload into the \
             user's document library. By default Solo deletes the staged \
             source file after successful ingestion; set \
             retain_source_file=true only when the user explicitly wants \
             the staged raw file kept for retry/debug.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "staged_uri": {
                        "type": "string",
                        "description": "URI returned by document_upload_commit, e.g. solo-staged://upload/<id>.",
                    },
                    "retain_source_file": {
                        "type": "boolean",
                        "description": "Default false. If false, delete the staged upload after successful ingest.",
                    },
                    "store_original_file": {
                        "type": "boolean",
                        "description": "Omit to use Solo's [documents].store_original_files_by_default setting (true by default). Set false explicitly for privacy-sensitive callers that do not want the original upload retained as an asset.",
                    },
                },
                "required": ["staged_uri"],
            })),
        ),
        Tool::new(
            "memory_import_documents",
            "Scan a server-side file or directory that is inside Solo's \
             configured workspace roots, then optionally ingest matching \
             files into the document library. Use dry_run=true before a \
             large directory import so the user can review the file count \
             and root boundary. Supports native document files; source can \
             narrow to markdown, text, markdown_text, or json.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Server-side file or directory path. It must be readable by Solo and inside workspace_file_access.allowed_roots when roots are configured.",
                    },
                    "source": {
                        "type": "string",
                        "description": "Optional native filter: native, markdown, text, markdown_text, or json.",
                        "enum": ["native", "markdown", "text", "markdown_text", "json"],
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, only scan and report matching files without ingesting them.",
                        "default": false,
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "Recursively scan directories. Default true.",
                        "default": true,
                    },
                    "max_files": {
                        "type": "integer",
                        "description": "Maximum matching files to scan/import.",
                        "minimum": 1,
                        "maximum": 5000,
                        "default": 500,
                    },
                    "store_original_file": {
                        "type": "boolean",
                        "description": "Default false. If true, copy each successfully ingested source file into Solo's persistent asset store and link it to the document.",
                        "default": false,
                    },
                },
                "required": ["path"],
            })),
        ),
        Tool::new(
            "memory_search_docs",
            "Search across the user's ingested documents by topic or \
             phrase. Returns up to `limit` matching chunks, best \
             match first, each with the parent document's title + \
             source path so you can cite where the answer came from. \
             Use when the user asks a question that hinges on \
             material they've added as a file (\"what does my \
             runbook say about backups?\", \"find the section in the \
             notes about the new policy\"). Forgotten documents are \
             skipped.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The query text.",
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results (default 5).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                },
                "required": ["query"],
            })),
        ),
        Tool::new(
            "memory_inspect_document",
            "Show one document's metadata plus a preview of every \
             chunk it was split into. Use after memory_search_docs \
             when the user wants the bigger picture for one hit \
             (\"show me the whole document this came from\"), or \
             after memory_list_documents to drill into one entry. \
             Each chunk preview is truncated to 200 chars.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "doc_id": {
                        "type": "string",
                        "description": "Document id to inspect (UUID v7).",
                    },
                },
                "required": ["doc_id"],
            })),
        ),
        Tool::new(
            "memory_list_documents",
            "List the user's ingested documents, newest first. Use \
             when the user asks \"what documents have I added?\" or \
             \"show me my files\". Returns a paginated index — pass \
             `offset` to page further back. Forgotten documents are \
             hidden by default; set `include_forgotten=true` to see \
             them too.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results per page (default 20).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Number of rows to skip (for paging). Default 0.",
                        "minimum": 0,
                    },
                    "include_forgotten": {
                        "type": "boolean",
                        "description": "If true, also include documents the user has forgotten. Default false.",
                    },
                },
            })),
        ),
        Tool::new(
            "memory_list_assets",
            "List persisted original-file assets, newest first. Use \
             when the user asks what source files Solo has retained \
             or when you need an asset_id to inspect or attach later. \
             Deleted assets are hidden by default.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "Maximum results per page (default 20).",
                        "minimum": 1,
                        "maximum": 100,
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Number of rows to skip (for paging). Default 0.",
                        "minimum": 0,
                    },
                    "include_deleted": {
                        "type": "boolean",
                        "description": "If true, include deleted asset metadata. Default false.",
                    },
                },
            })),
        ),
        Tool::new(
            "memory_inspect_asset",
            "Inspect one persisted original-file asset by id. Returns \
             metadata plus any document links and direct memory \
             attachments. This does not return raw file bytes.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "asset_id": {
                        "type": "string",
                        "description": "Asset id to inspect (UUID v7).",
                    },
                },
                "required": ["asset_id"],
            })),
        ),
        Tool::new(
            "memory_prepare_asset_download",
            "Prepare an authorized raw-byte download contract for one \
             retained original-file asset. Returns the HTTP method, URL, \
             auth mode, filename, MIME type, size, SHA-256, ETag, and \
             optional expiry; it does not inline file bytes in MCP.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "asset_id": {
                        "type": "string",
                        "description": "Asset id to download (UUID v7).",
                    },
                },
                "required": ["asset_id"],
            })),
        ),
        Tool::new(
            "memory_prepare_document_source_download",
            "Resolve a document's linked source_upload asset and prepare \
             its raw-byte download contract. Use this when the user asks \
             for the original source file for an ingested document.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "doc_id": {
                        "type": "string",
                        "description": "Document id whose source_upload asset should be downloaded (UUID v7).",
                    },
                },
                "required": ["doc_id"],
            })),
        ),
        Tool::new(
            "memory_list_document_assets",
            "List persisted original-file assets linked to one ingested \
             document. Use after memory_inspect_document or \
             memory_search_docs when the user needs the retained \
             source file metadata for that document.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "doc_id": {
                        "type": "string",
                        "description": "Document id whose linked assets should be listed (UUID v7).",
                    },
                },
                "required": ["doc_id"],
            })),
        ),
        Tool::new(
            "memory_list_memory_attachments",
            "List documents and assets attached to one memory. Use \
             after memory_inspect or memory_context when you need the \
             files that support a remembered fact, decision, or event.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "memory_id": {
                        "type": "string",
                        "description": "Memory id whose attachments should be listed (UUID v7).",
                    },
                },
                "required": ["memory_id"],
            })),
        ),
        Tool::new(
            "memory_forget_asset",
            "Delete one retained original-file asset by id. Use when \
             the user asks Solo to remove stored source-file bytes. \
             This marks the asset metadata deleted, removes the raw \
             content-addressed blob when present, and preserves \
             document/memory links as provenance records.",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "asset_id": {
                        "type": "string",
                        "description": "Asset id to forget/delete (UUID v7).",
                    },
                },
                "required": ["asset_id"],
            })),
        ),
        Tool::new(
            "memory_forget_document",
            "Drop one document from the user's library by id. Use \
             when the user asks you to forget a specific file \
             (\"forget my old runbook\"). The document's chunks stop \
             appearing in memory_search_docs and the vectors are \
             tombstoned in the index. The chunk rows themselves are \
             kept for forensic value (a future restore command can \
             undo this).",
            json_schema_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "doc_id": {
                        "type": "string",
                        "description": "Document id to forget (UUID v7).",
                    },
                },
                "required": ["doc_id"],
            })),
        ),
    ]
    .into_iter()
    .map(add_tool_metadata)
    .collect()
}

fn json_schema_object(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map,
        _ => panic!("json_schema_object: input must be an object"),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToolAccess {
    // Read-only in the MCP/model sense: no user memory or document
    // state changes. Read handlers may still emit operational audit
    // rows, the same way HTTP GETs commonly leave access logs.
    Read,
    AdditiveWrite,
    DestructiveWrite,
}

fn add_tool_metadata(tool: Tool) -> Tool {
    let access = tool_access_for(tool.name.as_ref());
    let output_schema = output_schema_for(tool.name.as_ref());
    let tool = tool
        .with_annotations(tool_annotations(access))
        .with_raw_output_schema(output_schema);
    if tool_supports_tasks(tool.name.as_ref()) {
        tool.with_execution(ToolExecution::new().with_task_support(TaskSupport::Optional))
    } else {
        tool
    }
}

fn tool_supports_tasks(name: &str) -> bool {
    matches!(
        name,
        "memory_remember_batch"
            | "memory_ingest_document"
            | "memory_ingest_staged_document"
            | "memory_import_documents"
            | "memory_search_docs"
    )
}

fn tool_access_for(name: &str) -> ToolAccess {
    match name {
        "memory_recall"
        | "memory_context"
        | "memory_inspect"
        | "memory_inbox"
        | "memory_themes"
        | "memory_facts_about"
        | "memory_entities"
        | "memory_graph_paths"
        | "memory_explain_provenance"
        | "memory_contradictions"
        | "memory_inspect_cluster"
        | "document_upload_status"
        | "memory_search_docs"
        | "memory_inspect_document"
        | "memory_list_documents"
        | "memory_list_assets"
        | "memory_inspect_asset"
        | "memory_prepare_asset_download"
        | "memory_prepare_document_source_download"
        | "memory_list_document_assets"
        | "memory_list_memory_attachments" => ToolAccess::Read,

        "memory_remember"
        | "memory_remember_batch"
        | "memory_review"
        | "memory_attach"
        | "memory_link_document_asset"
        | "memory_request_entity_split"
        | "memory_contradiction_resolve"
        | "memory_ingest_document"
        | "memory_import_documents"
        | "document_upload_prepare"
        | "document_upload_chunk_base64"
        | "document_upload_commit" => ToolAccess::AdditiveWrite,

        "memory_forget"
        | "memory_update"
        | "document_upload_abort"
        | "memory_ingest_staged_document"
        | "memory_forget_asset"
        | "memory_forget_document" => ToolAccess::DestructiveWrite,

        other => panic!("missing MCP tool access metadata for {other}"),
    }
}

fn tool_annotations(access: ToolAccess) -> ToolAnnotations {
    match access {
        ToolAccess::Read => ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false),
        ToolAccess::AdditiveWrite => ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(false),
        ToolAccess::DestructiveWrite => ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(false)
            .open_world(false),
    }
}

fn output_schema_for(name: &str) -> std::sync::Arc<serde_json::Map<String, serde_json::Value>> {
    match name {
        "memory_remember" => object_output_schema(
            serde_json::json!({
                "memory_id": { "type": "string" },
            }),
            &["memory_id"],
        ),
        "memory_remember_batch" => string_array_output_schema("memory_ids"),
        "memory_recall" => object_output_schema(
            serde_json::json!({
                "hits": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "index_len": { "type": "integer" },
            }),
            &["hits", "index_len"],
        ),
        "memory_context" => object_output_schema(
            serde_json::json!({
                "query": { "type": "string" },
                "subject": { "type": ["string", "null"] },
                "resolved_subject": { "type": ["string", "null"] },
                "sections": { "type": "object", "additionalProperties": true },
                "recall": { "type": "object", "additionalProperties": true },
                "themes": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "entities": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "facts": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "contradictions": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "graph": { "type": "object", "additionalProperties": true },
            }),
            &[
                "query",
                "sections",
                "recall",
                "themes",
                "entities",
                "facts",
                "contradictions",
                "graph",
            ],
        ),
        "memory_forget" => object_output_schema(
            serde_json::json!({
                "forgotten": { "type": "boolean" },
                "memory_id": { "type": "string" },
            }),
            &["forgotten", "memory_id"],
        ),
        "memory_inspect" => loose_object_output_schema(),
        "memory_update" => object_output_schema(
            serde_json::json!({
                "memory_id": { "type": "string" },
                "rowid": { "type": "integer" },
                "content": { "type": "string" },
                "updated_at_ms": { "type": "integer" },
            }),
            &["memory_id", "rowid", "content", "updated_at_ms"],
        ),
        "memory_inbox" => object_array_output_schema("items"),
        "memory_review" => object_output_schema(
            serde_json::json!({
                "memory_id": { "type": "string" },
                "state": { "type": ["string", "null"] },
                "reviewed_at_ms": { "type": ["integer", "null"] },
            }),
            &["memory_id", "state", "reviewed_at_ms"],
        ),
        "memory_attach" => object_output_schema(
            serde_json::json!({
                "attachment_id": { "type": "string" },
                "memory_id": { "type": "string" },
                "doc_id": { "type": ["string", "null"] },
                "asset_id": { "type": ["string", "null"] },
                "relation_type": { "type": "string" },
                "note": { "type": ["string", "null"] },
                "created_at_ms": { "type": "integer" },
            }),
            &[
                "attachment_id",
                "memory_id",
                "doc_id",
                "asset_id",
                "relation_type",
                "note",
                "created_at_ms",
            ],
        ),
        "memory_link_document_asset" => object_output_schema(
            serde_json::json!({
                "link_id": { "type": "string" },
                "doc_id": { "type": "string" },
                "asset_id": { "type": "string" },
                "relation_type": { "type": "string" },
                "created_at_ms": { "type": "integer" },
            }),
            &[
                "link_id",
                "doc_id",
                "asset_id",
                "relation_type",
                "created_at_ms",
            ],
        ),
        "memory_themes" => object_array_output_schema("themes"),
        "memory_facts_about" => object_array_output_schema("facts"),
        "memory_entities" => object_array_output_schema("entities"),
        "memory_request_entity_split" => object_output_schema(
            serde_json::json!({
                "op_id": { "type": "string" },
                "op_kind": { "type": "string" },
                "status": { "type": "string" },
                "source_entity_id": { "type": "string" },
                "target_entity_id": { "type": ["string", "null"] },
                "affected_aliases": {
                    "type": "array",
                    "items": { "type": "string" },
                },
                "reason": { "type": ["string", "null"] },
                "created_at_ms": { "type": "integer" },
            }),
            &[
                "op_id",
                "op_kind",
                "status",
                "source_entity_id",
                "target_entity_id",
                "affected_aliases",
                "reason",
                "created_at_ms",
            ],
        ),
        "memory_graph_paths" => object_output_schema(
            serde_json::json!({
                "from": { "type": "string" },
                "to": { "type": "string" },
                "max_hops": { "type": "integer" },
                "paths": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
            }),
            &["from", "to", "max_hops", "paths"],
        ),
        "memory_explain_provenance" => object_output_schema(
            serde_json::json!({
                "edge": { "type": "object", "additionalProperties": true },
                "evidence": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
            }),
            &["edge", "evidence"],
        ),
        "memory_contradictions" => object_array_output_schema("contradictions"),
        "memory_contradiction_resolve" => loose_object_output_schema(),
        "memory_inspect_cluster" => loose_object_output_schema(),
        "memory_ingest_document" => ingest_report_output_schema(),
        "document_upload_prepare" => object_output_schema(
            serde_json::json!({
                "upload_id": { "type": "string" },
                "upload_url": { "type": "string" },
                "upload_path": { "type": "string" },
                "route_kind": { "type": "string", "const": "direct_local" },
                "upload_method": { "type": "string", "const": "PATCH" },
                "upload_content_type": { "type": "string", "const": "application/octet-stream" },
                "upload_offset_header": { "type": "string" },
                "upload_length_header": { "type": "string" },
                "upload_status_header": { "type": "string" },
                "upload_headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                },
                "required_headers": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                },
                "upload_auth": {
                    "type": "object",
                    "properties": {
                        "mode": { "type": "string" },
                        "required": { "type": "string" },
                        "header": { "type": "string" },
                        "note": { "type": "string" },
                    },
                    "required": ["mode", "required", "header", "note"],
                    "additionalProperties": false,
                },
                "protocol": { "type": "string" },
                "max_file_bytes": { "type": "integer" },
                "max_chunk_bytes": { "type": "integer" },
                "recommended_chunk_bytes": { "type": "integer" },
                "mcp_fallback": { "type": "object", "additionalProperties": true },
                "expires_at_ms": { "type": "integer" },
                "commit_tool": { "type": "string" },
                "ingest_tool": { "type": "string" },
                "default_store_original_file": { "type": "boolean" },
                "next_actions": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "next_steps": {
                    "type": "array",
                    "items": { "type": "string" },
                },
            }),
            &[
                "upload_id",
                "upload_url",
                "upload_path",
                "route_kind",
                "upload_method",
                "upload_content_type",
                "upload_offset_header",
                "upload_length_header",
                "upload_status_header",
                "upload_headers",
                "required_headers",
                "upload_auth",
                "protocol",
                "max_file_bytes",
                "max_chunk_bytes",
                "recommended_chunk_bytes",
                "mcp_fallback",
                "expires_at_ms",
                "commit_tool",
                "ingest_tool",
                "default_store_original_file",
                "next_actions",
                "next_steps",
            ],
        ),
        "document_upload_chunk_base64" => object_output_schema(
            serde_json::json!({
                "upload_id": { "type": "string" },
                "status": {
                    "type": "string",
                    "enum": ["open", "busy", "committed", "ingested", "expired", "aborted"],
                },
                "bytes_received": { "type": "integer" },
                "size_bytes": { "type": "integer" },
                "next_offset": { "type": "integer" },
                "expires_at_ms": { "type": "integer" },
                "operation_in_progress": { "type": "boolean" },
                "active_operation": { "type": ["string", "null"] },
                "staged_uri": { "type": ["string", "null"] },
                "commit_result": { "type": ["object", "null"], "additionalProperties": true },
                "ingest_result": { "type": ["object", "null"], "additionalProperties": true },
                "terminal": { "type": "boolean" },
            }),
            &[
                "upload_id",
                "status",
                "bytes_received",
                "size_bytes",
                "next_offset",
                "expires_at_ms",
                "operation_in_progress",
                "active_operation",
                "staged_uri",
                "commit_result",
                "ingest_result",
                "terminal",
            ],
        ),
        "document_upload_status" => object_output_schema(
            serde_json::json!({
                "upload_id": { "type": "string" },
                "status": {
                    "type": "string",
                    "enum": ["open", "busy", "committed", "ingested", "expired", "aborted"],
                },
                "bytes_received": { "type": "integer" },
                "size_bytes": { "type": "integer" },
                "next_offset": { "type": "integer" },
                "expires_at_ms": { "type": "integer" },
                "operation_in_progress": { "type": "boolean" },
                "active_operation": { "type": ["string", "null"] },
                "staged_uri": { "type": ["string", "null"] },
                "commit_result": { "type": ["object", "null"], "additionalProperties": true },
                "ingest_result": { "type": ["object", "null"], "additionalProperties": true },
                "terminal": { "type": "boolean" },
            }),
            &[
                "upload_id",
                "status",
                "bytes_received",
                "size_bytes",
                "next_offset",
                "expires_at_ms",
                "operation_in_progress",
                "active_operation",
                "staged_uri",
                "commit_result",
                "ingest_result",
                "terminal",
            ],
        ),
        "document_upload_commit" => object_output_schema(
            serde_json::json!({
                "upload_id": { "type": "string" },
                "staged_uri": { "type": "string" },
                "filename": { "type": "string" },
                "mime_type": { "type": "string" },
                "size_bytes": { "type": "integer" },
                "sha256": { "type": "string" },
            }),
            &[
                "upload_id",
                "staged_uri",
                "filename",
                "mime_type",
                "size_bytes",
                "sha256",
            ],
        ),
        "document_upload_abort" => object_output_schema(
            serde_json::json!({
                "upload_id": { "type": "string" },
                "status": { "type": "string", "const": "aborted" },
                "cleanup_performed": { "type": "boolean" },
                "already_aborted": { "type": "boolean" },
                "removed_partial_file": { "type": "boolean" },
                "removed_staged_file": { "type": "boolean" },
            }),
            &[
                "upload_id",
                "status",
                "cleanup_performed",
                "already_aborted",
                "removed_partial_file",
                "removed_staged_file",
            ],
        ),
        "memory_ingest_staged_document" => object_output_schema(
            serde_json::json!({
                "staged_uri": { "type": "string" },
                "document_id": { "type": ["string", "null"] },
                "chunks_persisted": { "type": "integer" },
                "bytes_ingested": { "type": "integer" },
                "deduped": { "type": "boolean" },
                "stored_original_file": { "type": "boolean" },
                "asset": { "type": ["object", "null"], "additionalProperties": true },
                "document_asset_link": { "type": ["object", "null"], "additionalProperties": true },
                "extraction_status": { "type": "string", "enum": ["extracted", "stored_unparsed", "failed"] },
                "extraction_error": { "type": ["string", "null"] },
                "extraction": { "type": ["object", "null"], "additionalProperties": true },
                "deleted_staged_file": { "type": "boolean" },
                "retained_source_file": { "type": "boolean" },
                "report": { "type": ["object", "null"], "additionalProperties": true },
                "idempotent_replay": { "type": "boolean" },
                "ingest_completed_at_ms": { "type": "integer" },
            }),
            &[
                "staged_uri",
                "document_id",
                "chunks_persisted",
                "bytes_ingested",
                "deduped",
                "stored_original_file",
                "asset",
                "document_asset_link",
                "extraction_status",
                "extraction_error",
                "extraction",
                "deleted_staged_file",
                "retained_source_file",
                "report",
                "idempotent_replay",
                "ingest_completed_at_ms",
            ],
        ),
        "memory_import_documents" => object_output_schema(
            serde_json::json!({
                "path": { "type": "string" },
                "source": { "type": "string" },
                "source_label": { "type": "string" },
                "dry_run": { "type": "boolean" },
                "recursive": { "type": "boolean" },
                "truncated": { "type": "boolean" },
                "total_files": { "type": "integer" },
                "total_bytes": { "type": "integer" },
                "store_original_file": { "type": "boolean" },
                "imported": { "type": "integer" },
                "deduped": { "type": "integer" },
                "failed": { "type": "integer" },
                "chunks_persisted": { "type": "integer" },
                "assets_retained": { "type": "integer" },
                "assets_deduped": { "type": "integer" },
                "asset_links": { "type": "integer" },
                "asset_failed": { "type": "integer" },
                "workspace_roots": { "type": "object", "additionalProperties": true },
                "files": { "type": "array", "items": { "type": "object", "additionalProperties": true } },
                "results": { "type": "array", "items": { "type": "object", "additionalProperties": true } },
            }),
            &[
                "path",
                "source",
                "source_label",
                "dry_run",
                "recursive",
                "truncated",
                "total_files",
                "total_bytes",
                "store_original_file",
                "imported",
                "deduped",
                "failed",
                "chunks_persisted",
                "assets_retained",
                "assets_deduped",
                "asset_links",
                "asset_failed",
                "workspace_roots",
                "files",
                "results",
            ],
        ),
        "memory_search_docs" => object_array_output_schema("hits"),
        "memory_inspect_document" => object_output_schema(
            serde_json::json!({
                "document": { "type": "object", "additionalProperties": true },
                "chunks": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "linked_assets": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
            }),
            &["document", "chunks", "linked_assets"],
        ),
        "memory_list_documents" => object_array_output_schema("documents"),
        "memory_list_assets" => object_array_output_schema("assets"),
        "memory_inspect_asset" => object_output_schema(
            serde_json::json!({
                "asset": { "type": "object", "additionalProperties": true },
                "document_links": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
                "memory_attachments": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
            }),
            &["asset", "document_links", "memory_attachments"],
        ),
        "memory_prepare_asset_download" => object_output_schema(
            serde_json::json!({
                "asset_id": { "type": "string" },
                "download_url": { "type": "string" },
                "download_path": { "type": "string" },
                "route_kind": { "type": "string" },
                "download_method": { "type": "string" },
                "required_headers": { "type": "object", "additionalProperties": { "type": "string" } },
                "download_auth": { "type": "object", "additionalProperties": true },
                "filename": { "type": ["string", "null"] },
                "mime_type": { "type": "string" },
                "size_bytes": { "type": "integer" },
                "sha256": { "type": "string" },
                "etag": { "type": "string" },
                "expires_at_ms": { "type": ["integer", "null"] },
                "next_actions": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
            }),
            &[
                "asset_id",
                "download_url",
                "download_path",
                "route_kind",
                "download_method",
                "required_headers",
                "download_auth",
                "mime_type",
                "size_bytes",
                "sha256",
                "etag",
                "expires_at_ms",
                "next_actions",
            ],
        ),
        "memory_prepare_document_source_download" => object_output_schema(
            serde_json::json!({
                "doc_id": { "type": "string" },
                "source_asset_link": { "type": "object", "additionalProperties": true },
                "download": { "type": "object", "additionalProperties": true },
            }),
            &["doc_id", "source_asset_link", "download"],
        ),
        "memory_list_document_assets" => object_output_schema(
            serde_json::json!({
                "doc_id": { "type": "string" },
                "assets": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
            }),
            &["doc_id", "assets"],
        ),
        "memory_list_memory_attachments" => object_output_schema(
            serde_json::json!({
                "memory_id": { "type": "string" },
                "attachments": {
                    "type": "array",
                    "items": { "type": "object", "additionalProperties": true },
                },
            }),
            &["memory_id", "attachments"],
        ),
        "memory_forget_asset" => object_output_schema(
            serde_json::json!({
                "asset_id": { "type": "string" },
                "blob_deleted": { "type": "boolean" },
                "already_deleted": { "type": "boolean" },
                "document_links": { "type": "integer" },
                "memory_attachments": { "type": "integer" },
            }),
            &[
                "asset_id",
                "blob_deleted",
                "already_deleted",
                "document_links",
                "memory_attachments",
            ],
        ),
        "memory_forget_document" => object_output_schema(
            serde_json::json!({
                "doc_id": { "type": "string" },
                "chunks_tombstoned": { "type": "integer" },
            }),
            &["doc_id", "chunks_tombstoned"],
        ),
        other => panic!("missing MCP tool output schema for {other}"),
    }
}

fn loose_object_output_schema() -> std::sync::Arc<serde_json::Map<String, serde_json::Value>> {
    object_output_schema(serde_json::json!({}), &[])
}

fn object_array_output_schema(
    field: &'static str,
) -> std::sync::Arc<serde_json::Map<String, serde_json::Value>> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        field.to_string(),
        serde_json::json!({
            "type": "array",
            "items": { "type": "object", "additionalProperties": true },
        }),
    );
    object_output_schema(serde_json::Value::Object(properties), &[field])
}

fn string_array_output_schema(
    field: &'static str,
) -> std::sync::Arc<serde_json::Map<String, serde_json::Value>> {
    let mut properties = serde_json::Map::new();
    properties.insert(
        field.to_string(),
        serde_json::json!({
            "type": "array",
            "items": { "type": "string" },
        }),
    );
    object_output_schema(serde_json::Value::Object(properties), &[field])
}

fn ingest_report_output_schema() -> std::sync::Arc<serde_json::Map<String, serde_json::Value>> {
    object_output_schema(
        serde_json::json!({
            "doc_id": { "type": "string" },
            "chunks_persisted": { "type": "integer" },
            "bytes_ingested": { "type": "integer" },
            "deduped": { "type": "boolean" },
        }),
        &["doc_id", "chunks_persisted", "bytes_ingested", "deduped"],
    )
}

fn object_output_schema(
    properties: serde_json::Value,
    required: &[&str],
) -> std::sync::Arc<serde_json::Map<String, serde_json::Value>> {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": true,
    });
    if !required.is_empty() {
        schema.as_object_mut().unwrap().insert(
            "required".to_string(),
            serde_json::Value::Array(
                required
                    .iter()
                    .map(|field| serde_json::Value::String((*field).to_string()))
                    .collect(),
            ),
        );
    }
    std::sync::Arc::new(json_schema_object(schema))
}

fn structured_text_result(
    text: impl Into<String>,
    structured: serde_json::Value,
) -> CallToolResult {
    structured_content_result(vec![Content::text(text.into())], structured)
}

fn structured_content_result(
    content: Vec<Content>,
    structured: serde_json::Value,
) -> CallToolResult {
    let mut result = CallToolResult::structured(structured);
    result.content = content;
    result
}

fn json_tool_result<T: Serialize>(value: &T) -> std::result::Result<CallToolResult, McpError> {
    let structured = serde_json::to_value(value)
        .map_err(|e| McpError::internal_error(format!("serialize structured result: {e}"), None))?;
    json_value_tool_result(structured)
}

fn json_value_tool_result(
    structured: serde_json::Value,
) -> std::result::Result<CallToolResult, McpError> {
    let body = serde_json::to_string_pretty(&structured)
        .map_err(|e| McpError::internal_error(format!("serialize result text: {e}"), None))?;
    Ok(structured_text_result(body, structured))
}

fn json_value_tool_result_with_links(
    structured: serde_json::Value,
    links: Vec<Content>,
) -> std::result::Result<CallToolResult, McpError> {
    let body = serde_json::to_string_pretty(&structured)
        .map_err(|e| McpError::internal_error(format!("serialize result text: {e}"), None))?;
    let mut content = Vec::with_capacity(1 + links.len());
    content.push(Content::text(body));
    content.extend(links);
    Ok(structured_content_result(content, structured))
}

fn memory_resource_link(memory_id: &str, title: Option<String>) -> Content {
    let mut resource = RawResource::new(
        memory_resource_uri(memory_id),
        format!("memory {memory_id}"),
    )
    .with_mime_type("text/plain");
    if let Some(title) = title {
        resource = resource.with_title(title);
    }
    Content::resource_link(resource)
}

fn document_resource_link(doc_id: &str, title: Option<String>) -> Content {
    let mut resource = RawResource::new(
        document_resource_uri(doc_id),
        title
            .clone()
            .unwrap_or_else(|| format!("document {doc_id}")),
    )
    .with_mime_type("application/json");
    if let Some(title) = title {
        resource = resource.with_title(title);
    }
    Content::resource_link(resource)
}

fn document_chunk_resource_link(doc_id: &str, chunk_id: &str, title: Option<String>) -> Content {
    let mut resource = RawResource::new(
        document_chunk_resource_uri(doc_id, chunk_id),
        format!("chunk {chunk_id}"),
    )
    .with_mime_type("text/plain");
    if let Some(title) = title {
        resource = resource.with_title(title);
    }
    Content::resource_link(resource)
}

fn asset_resource_link(asset_id: &str, title: Option<String>) -> Content {
    let mut resource = RawResource::new(
        asset_resource_uri(asset_id),
        title.clone().unwrap_or_else(|| format!("asset {asset_id}")),
    )
    .with_mime_type("application/json");
    if let Some(title) = title {
        resource = resource.with_title(title);
    }
    Content::resource_link(resource)
}

fn import_response_resource_links(results: &[ImportResult]) -> Vec<Content> {
    let mut seen = std::collections::HashSet::new();
    let mut links = Vec::new();
    for result in results {
        if let Some(doc_id) = result.doc_id.as_deref() {
            let uri = document_resource_uri(doc_id);
            if seen.insert(uri) {
                links.push(document_resource_link(doc_id, None));
            }
        }
        if let Some(asset) = result.asset.as_ref() {
            let asset_id = asset.asset_id.to_string();
            let uri = asset_resource_uri(&asset_id);
            if seen.insert(uri) {
                links.push(asset_resource_link(&asset_id, asset.filename.clone()));
            }
        }
    }
    links
}

fn legacy_array_tool_result<T: Serialize>(
    items: &T,
    structured_field: &'static str,
) -> std::result::Result<CallToolResult, McpError> {
    let items_value = serde_json::to_value(items)
        .map_err(|e| McpError::internal_error(format!("serialize structured result: {e}"), None))?;
    let body = serde_json::to_string_pretty(&items_value)
        .map_err(|e| McpError::internal_error(format!("serialize result text: {e}"), None))?;
    let mut structured = serde_json::Map::new();
    structured.insert(structured_field.to_string(), items_value);
    Ok(structured_text_result(
        body,
        serde_json::Value::Object(structured),
    ))
}

/// Names of every tool this server exposes, in registration order.
///
/// Exposed for cross-crate consumers (notably `solo doctor
/// --check-mcp-compat`) that want the name list without paying the
/// cost of building full `rmcp::Tool` records (which allocate JSON
/// schemas). The registration order matches `build_tools()` so any
/// drift between the two would be caught by the cross-provider regex
/// test which iterates `build_tools()`.
pub fn tool_names() -> Vec<&'static str> {
    vec![
        "memory_remember",
        // v0.9.2 — batched-remember for agentic clients.
        "memory_remember_batch",
        "memory_recall",
        "memory_context",
        "memory_forget",
        "memory_inspect",
        "memory_update",
        "memory_inbox",
        "memory_review",
        "memory_attach",
        "memory_link_document_asset",
        "memory_themes",
        "memory_facts_about",
        "memory_entities",
        "memory_request_entity_split",
        "memory_graph_paths",
        "memory_explain_provenance",
        "memory_contradictions",
        "memory_contradiction_resolve",
        "memory_inspect_cluster",
        // Document tools added in v0.7.0:
        "memory_ingest_document",
        "document_upload_prepare",
        "document_upload_status",
        "document_upload_chunk_base64",
        "document_upload_commit",
        "document_upload_abort",
        "memory_ingest_staged_document",
        "memory_import_documents",
        "memory_search_docs",
        "memory_inspect_document",
        "memory_list_documents",
        "memory_list_assets",
        "memory_inspect_asset",
        "memory_prepare_asset_download",
        "memory_prepare_document_source_download",
        "memory_list_document_assets",
        "memory_list_memory_attachments",
        "memory_forget_asset",
        "memory_forget_document",
    ]
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

impl SoloMcpServer {
    async fn handle_remember(
        &self,
        args: RememberArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let content = args.content.trim_end().to_string();
        if content.is_empty() {
            return Err(McpError::invalid_params(
                "memory_remember: content must not be empty".to_string(),
                None,
            ));
        }
        validate_salience(args.salience)?;
        let embedding: solo_core::Embedding = self
            .inner
            .tenant
            .embedder()
            .embed(&content)
            .await
            .map_err(solo_to_mcp)?;
        let episode = Episode {
            memory_id: MemoryId::new(),
            ts_ms: chrono::Utc::now().timestamp_millis(),
            source_type: args.source_type.unwrap_or_else(|| "user_message".into()),
            source_id: args.source_id,
            content,
            encoding_context: EncodingContext::default(),
            provenance: None,
            confidence: Confidence::new(0.9).expect("0.9 is in [0.0, 1.0]"),
            strength: 0.5,
            // v0.9.2: caller-supplied salience overrides the default. The
            // `validate_salience` call above has already rejected NaN /
            // out-of-range values.
            salience: args.salience.unwrap_or(0.5),
            tier: Tier::Hot,
        };
        let mid = self
            .inner
            .tenant
            .write()
            .remember_as(self.inner.audit_principal.clone(), episode, embedding)
            .await
            .map_err(solo_to_mcp)?;
        Ok(structured_content_result(
            vec![
                Content::text(format!("remembered {mid}")),
                memory_resource_link(&mid.to_string(), None),
            ],
            serde_json::json!({ "memory_id": mid.to_string() }),
        ))
    }

    /// v0.9.2 — handler for `memory_remember_batch`.
    ///
    /// Pipeline (mirrors `handle_remember` over N items):
    ///   1. Validate batch (non-empty, ≤ `MAX_REMEMBER_BATCH_SIZE`,
    ///      per-item content non-empty, per-item salience in [0.0, 1.0]).
    ///   2. Embed all items sequentially via the tenant's embedder.
    ///      We don't `join_all` here because the in-process embedder
    ///      paths today (stub, local-Anthropic, OpenAI) are individually
    ///      fast and serial is robust against rate-limit surprises (per
    ///      dev-log 0120 §8 R2 mitigation: existing embedder
    ///      throttling guards parallel fan-out; serial gives identical
    ///      semantics with simpler error paths). Parallel fan-out is a
    ///      v0.9.3 optimization once the batch tool has live traffic.
    ///   3. Build `Vec<(Episode, Embedding)>` with default Confidence /
    ///      strength / tier — same shape as single-Remember.
    ///   4. Dispatch via `WriteHandle::remember_batch_as`, which wraps
    ///      every INSERT in ONE `BEGIN IMMEDIATE` tx (ADR-0003 invariant
    ///      preserved).
    ///   5. Reply is `Vec<MemoryId>` in input order; serialise to JSON.
    async fn handle_remember_batch(
        &self,
        args: RememberBatchArgs,
        progress: Option<crate::mcp_progress::ProgressReporter>,
        cancellation: crate::mcp_task::CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        cancellation.check()?;
        // 1. Batch-shape validation. The writer-actor will re-check
        //    `MAX_REMEMBER_BATCH_SIZE` (dev-log 0120 §3 Decision F) and
        //    reject with `InvalidInput` — we mirror the check here to
        //    avoid the round-trip into the writer + the embedder calls
        //    when the request is obviously over-cap.
        if args.items.is_empty() {
            return Err(McpError::invalid_params(
                "memory_remember_batch: items must not be empty".to_string(),
                None,
            ));
        }
        if args.items.len() > solo_storage::MAX_REMEMBER_BATCH_SIZE {
            return Err(McpError::invalid_params(
                format!(
                    "memory_remember_batch: {} items exceeds MAX_REMEMBER_BATCH_SIZE = {}",
                    args.items.len(),
                    solo_storage::MAX_REMEMBER_BATCH_SIZE,
                ),
                None,
            ));
        }
        for (i, item) in args.items.iter().enumerate() {
            if item.content.trim_end().is_empty() {
                return Err(McpError::invalid_params(
                    format!("memory_remember_batch: items[{i}].content must not be empty"),
                    None,
                ));
            }
            validate_salience(item.salience).map_err(|e| {
                // Re-wrap with the index so the caller can pinpoint
                // which item tripped the validator.
                McpError::invalid_params(
                    format!("memory_remember_batch: items[{i}].{}", e.message),
                    None,
                )
            })?;
        }

        // v0.11.0 P3: progress emission is gated on batch size — below
        // the threshold (50 items) the wire-overhead of progress
        // notifications outweighs the UX benefit. Above threshold +
        // client opted in (`reporter.is_some()`), emit one event per
        // `MCP_REMEMBER_BATCH_PROGRESS_EMIT_EVERY` items during the
        // embed loop + one terminal "embedded" + one "inserted" event.
        let total = args.items.len() as u64;
        let progress_active = progress.is_some()
            && args.items.len() > crate::mcp_progress::MCP_REMEMBER_BATCH_PROGRESS_ITEM_THRESHOLD;
        let progress_reporter = if progress_active {
            progress.as_ref()
        } else {
            None
        };

        // 2. Embed each item. Serial fan-out (see doc comment above).
        let embedder = self.inner.tenant.embedder();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut pairs: Vec<(Episode, solo_core::Embedding)> = Vec::with_capacity(args.items.len());
        for (i, item) in args.items.into_iter().enumerate() {
            cancellation.check()?;
            let content = item.content.trim_end().to_string();
            let embedding = embedder.embed(&content).await.map_err(solo_to_mcp)?;
            let episode = Episode {
                memory_id: MemoryId::new(),
                ts_ms: now_ms,
                source_type: item.source_type.unwrap_or_else(|| "user_message".into()),
                source_id: item.source_id,
                content,
                encoding_context: EncodingContext::default(),
                provenance: None,
                confidence: Confidence::new(0.9).expect("0.9 is in [0.0, 1.0]"),
                strength: 0.5,
                salience: item.salience.unwrap_or(0.5),
                tier: Tier::Hot,
            };
            pairs.push((episode, embedding));
            // v0.11.0 P3 checkpoint A — embed progress, every N items.
            // `(i + 1) % EMIT_EVERY == 0` emits at items 25, 50, 75, ...
            // The terminal "embedded" event below covers any remainder.
            let done = (i + 1) as u64;
            if (i + 1) % crate::mcp_progress::MCP_REMEMBER_BATCH_PROGRESS_EMIT_EVERY == 0 {
                crate::mcp_progress::report_if_some(
                    progress_reporter,
                    done,
                    Some(total),
                    Some("embedding"),
                );
            }
        }

        // v0.11.0 P3 checkpoint B — all items embedded; about to land
        // in writer-actor. Always-emitted (when progress_active) so a
        // batch that wasn't a multiple of EMIT_EVERY still gets a
        // final embed-phase event.
        crate::mcp_progress::report_if_some(
            progress_reporter,
            total,
            Some(total),
            Some("embedded"),
        );
        cancellation.check()?;

        // 3. Dispatch into the writer-actor. The batch lands as one tx.
        let memory_ids = self
            .inner
            .tenant
            .write()
            .remember_batch_as(self.inner.audit_principal.clone(), pairs)
            .await
            .map_err(solo_to_mcp)?;

        // v0.11.0 P3 checkpoint C — writer-actor committed. The reply
        // body below also lands in the POST response, but this event
        // gives a client subscribed to the GET stream early confirmation
        // that the row is committed without waiting for the POST to
        // return (network buffering can stall the POST response
        // marginally; the SSE event is immediate).
        crate::mcp_progress::report_if_some(
            progress_reporter,
            total,
            Some(total),
            Some("inserted"),
        );

        // 4. Reply: JSON-serialised array of memory ids in input order.
        //    Stringified so MCP clients see UUID strings (matches single
        //    `memory_remember`'s reply shape — both speak strings on
        //    the wire).
        let ids_as_strings: Vec<String> = memory_ids.iter().map(|m| m.to_string()).collect();
        let body = serde_json::to_string(&ids_as_strings)
            .map_err(|e| McpError::internal_error(format!("serialize batch reply: {e}"), None))?;
        Ok(structured_text_result(
            body,
            serde_json::json!({ "memory_ids": ids_as_strings }),
        ))
    }

    async fn handle_recall(
        &self,
        args: RecallArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        // Pipeline lives in solo-query; the transport just formats the
        // result. solo_query::run_recall validates empty queries
        // (returns InvalidInput → invalid_params via solo_to_mcp).
        let result = solo_query::run_recall(
            self.inner.tenant.as_ref(),
            self.inner.audit_principal.clone(),
            &args.query,
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;

        // Always return a JSON array of hits (possibly empty) so clients
        // can `JSON.parse` uniformly. The previous shape returned a
        // plain-English string ("no matches (index has N vectors)") on
        // empty results, which broke any client parsing recall as JSON.
        // The `index_len` diagnostic is preserved as an MCP `Content` text
        // alongside the JSON payload — agents see both; tooling parses the
        // first content as JSON.
        let hits_value = serde_json::to_value(&result.hits)
            .map_err(|e| McpError::internal_error(format!("serialize recall hits: {e}"), None))?;
        let body = serde_json::to_string_pretty(&hits_value)
            .map_err(|e| McpError::internal_error(format!("serialize recall text: {e}"), None))?;
        let mut contents = vec![Content::text(body)];
        if result.hits.is_empty() {
            contents.push(Content::text(format!(
                "(index has {} vectors)",
                result.index_len
            )));
        }
        Ok(structured_content_result(
            contents,
            serde_json::json!({
                "hits": hits_value,
                "index_len": result.index_len,
            }),
        ))
    }

    async fn handle_memory_context(
        &self,
        args: MemoryContextArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let result = solo_query::memory_context(
            self.inner.tenant.as_ref(),
            self.inner.audit_principal.clone(),
            &args.query,
            args.subject.as_deref(),
            &self.inner.user_aliases,
            args.window_days,
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;
        json_tool_result(&result)
    }

    async fn handle_forget(
        &self,
        args: ForgetArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mid = MemoryId::from_str(&args.memory_id)
            .map_err(|e| McpError::invalid_params(format!("invalid memory_id: {e}"), None))?;
        self.inner
            .tenant
            .write()
            .forget_as(self.inner.audit_principal.clone(), mid, args.reason)
            .await
            .map_err(solo_to_mcp)?;
        Ok(structured_text_result(
            format!("forgotten {mid}"),
            serde_json::json!({
                "forgotten": true,
                "memory_id": mid.to_string(),
            }),
        ))
    }

    async fn handle_inspect(
        &self,
        args: InspectArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mid = MemoryId::from_str(&args.memory_id)
            .map_err(|e| McpError::invalid_params(format!("invalid memory_id: {e}"), None))?;
        // Pipeline lives in solo-query::inspect; transports just format.
        let row = solo_query::inspect_one(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            mid,
        )
        .await
        .map_err(solo_to_mcp)?;
        json_tool_result(&row)
    }

    async fn handle_update(
        &self,
        args: UpdateArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mid = MemoryId::from_str(&args.memory_id)
            .map_err(|e| McpError::invalid_params(format!("invalid memory_id: {e}"), None))?;
        if args.content.trim().is_empty() {
            return Err(McpError::invalid_params(
                "memory_update: content must not be empty".to_string(),
                None,
            ));
        }
        let result = solo_query::memory_update(
            self.inner.tenant.as_ref(),
            self.inner.audit_principal.clone(),
            mid,
            &args.content,
        )
        .await
        .map_err(solo_to_mcp)?;
        json_tool_result(&result)
    }

    async fn handle_inbox(&self, args: InboxArgs) -> std::result::Result<CallToolResult, McpError> {
        let items = solo_query::memory_inbox(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;
        json_value_tool_result(serde_json::json!({ "items": items }))
    }

    async fn handle_review(
        &self,
        args: ReviewArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let mid = MemoryId::from_str(&args.memory_id)
            .map_err(|e| McpError::invalid_params(format!("invalid memory_id: {e}"), None))?;
        let state = parse_review_state(args.state.as_deref())?;
        let result = self
            .inner
            .tenant
            .write()
            .review_memory_as(self.inner.audit_principal.clone(), mid, state, args.note)
            .await
            .map_err(solo_to_mcp)?;
        json_tool_result(&result)
    }

    async fn handle_attach_memory(
        &self,
        args: AttachMemoryArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let memory_id = MemoryId::from_str(&args.memory_id)
            .map_err(|e| McpError::invalid_params(format!("invalid memory_id: {e}"), None))?;
        let doc_id = match args.doc_id {
            Some(raw) => Some(
                DocumentId::from_str(raw.trim())
                    .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?,
            ),
            None => None,
        };
        let asset_id =
            match args.asset_id {
                Some(raw) => Some(AssetId::from_str(raw.trim()).map_err(|e| {
                    McpError::invalid_params(format!("invalid asset_id: {e}"), None)
                })?),
                None => None,
            };
        if matches!((doc_id, asset_id), (Some(_), Some(_)) | (None, None)) {
            return Err(McpError::invalid_params(
                "memory_attach: provide exactly one of doc_id or asset_id".to_string(),
                None,
            ));
        }
        let relation_type = args.relation_type.unwrap_or_else(|| "related".to_string());
        let result = self
            .inner
            .tenant
            .write()
            .attach_memory_as(
                self.inner.audit_principal.clone(),
                memory_id,
                doc_id,
                asset_id,
                relation_type,
                args.note,
            )
            .await
            .map_err(solo_to_mcp)?;
        let mut links = vec![memory_resource_link(&result.memory_id.to_string(), None)];
        if let Some(doc_id) = result.doc_id.as_ref() {
            links.push(document_resource_link(&doc_id.to_string(), None));
        }
        if let Some(asset_id) = result.asset_id.as_ref() {
            links.push(asset_resource_link(&asset_id.to_string(), None));
        }
        let structured = serde_json::to_value(&result).map_err(|e| {
            McpError::internal_error(format!("serialize structured memory attachment: {e}"), None)
        })?;
        json_value_tool_result_with_links(structured, links)
    }

    async fn handle_link_document_asset(
        &self,
        args: LinkDocumentAssetArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let doc_id = DocumentId::from_str(args.doc_id.trim())
            .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?;
        let asset_id = AssetId::from_str(args.asset_id.trim())
            .map_err(|e| McpError::invalid_params(format!("invalid asset_id: {e}"), None))?;
        let relation_type = args
            .relation_type
            .unwrap_or_else(|| "source_upload".to_string());
        let result = self
            .inner
            .tenant
            .write()
            .link_document_asset_as(
                self.inner.audit_principal.clone(),
                doc_id,
                asset_id,
                relation_type,
                args.note,
            )
            .await
            .map_err(solo_to_mcp)?;
        let links = vec![
            document_resource_link(&result.doc_id.to_string(), None),
            asset_resource_link(&result.asset_id.to_string(), None),
        ];
        let structured = serde_json::to_value(&result).map_err(|e| {
            McpError::internal_error(
                format!("serialize structured document asset link: {e}"),
                None,
            )
        })?;
        json_value_tool_result_with_links(structured, links)
    }

    // Path 1 derived-layer handlers (v0.4.0+). Each one delegates to a
    // single solo-query::derived pipeline and serialises the result Vec
    // to pretty JSON for the MCP wire. Empty result → JSON empty array
    // `[]` (not a special-case "no matches" string) so MCP clients can
    // parse uniformly.

    async fn handle_themes(
        &self,
        args: ThemesArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let hits = solo_query::themes(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            args.window_days,
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;
        legacy_array_tool_result(&hits, "themes")
    }

    async fn handle_facts_about(
        &self,
        args: FactsAboutArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        if args.subject.trim().is_empty() {
            return Err(McpError::invalid_params(
                "memory_facts_about: subject must not be empty".to_string(),
                None,
            ));
        }
        let hits = solo_query::facts_about(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &args.subject,
            &self.inner.user_aliases,
            args.include_as_object,
            args.predicate.as_deref(),
            args.since_ms,
            args.until_ms,
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;
        legacy_array_tool_result(&hits, "facts")
    }

    async fn handle_entities(
        &self,
        args: EntitiesArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        if args.query.trim().is_empty() {
            return Err(McpError::invalid_params(
                "memory_entities: query must not be empty".to_string(),
                None,
            ));
        }
        let hits = solo_query::entities(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &args.query,
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;
        legacy_array_tool_result(&hits, "entities")
    }

    async fn handle_request_entity_split(
        &self,
        args: EntitySplitReviewArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let result = self
            .inner
            .tenant
            .write()
            .request_entity_split_as(
                self.inner.audit_principal.clone(),
                EntitySplitRequest {
                    entity_id: args.entity_id,
                    affected_aliases: args.affected_aliases,
                    reason: args.reason,
                },
            )
            .await
            .map_err(solo_to_mcp)?;
        json_tool_result(&result)
    }

    async fn handle_graph_paths(
        &self,
        args: GraphPathsArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let from_entity_id = crate::graph_paths::parse_graph_path_entity_param("from", &args.from)
            .map_err(|message| {
                McpError::invalid_params(format!("memory_graph_paths: {message}"), None)
            })?;
        let to_entity_id = crate::graph_paths::parse_graph_path_entity_param("to", &args.to)
            .map_err(|message| {
                McpError::invalid_params(format!("memory_graph_paths: {message}"), None)
            })?;
        if from_entity_id == to_entity_id {
            return Err(McpError::invalid_params(
                "memory_graph_paths: from and to must be different entities".to_string(),
                None,
            ));
        }

        let max_hops = args
            .max_hops
            .unwrap_or(crate::graph_paths::GRAPH_PATHS_DEFAULT_MAX_HOPS);
        if !(1..=crate::graph_paths::GRAPH_PATHS_MAX_HOPS).contains(&max_hops) {
            return Err(McpError::invalid_params(
                format!(
                    "memory_graph_paths: max_hops must be between 1 and {}",
                    crate::graph_paths::GRAPH_PATHS_MAX_HOPS
                ),
                None,
            ));
        }
        if args.as_of_ms.is_some_and(|as_of_ms| as_of_ms < 0) {
            return Err(McpError::invalid_params(
                "memory_graph_paths: as_of_ms must be >= 0".to_string(),
                None,
            ));
        }
        let as_of_ms = args.as_of_ms;
        let limit = args
            .limit
            .unwrap_or(crate::graph_paths::GRAPH_PATHS_DEFAULT_LIMIT)
            .clamp(1, crate::graph_paths::GRAPH_PATHS_MAX_LIMIT);
        let response_from = format!("ent:{from_entity_id}");
        let response_to = format!("ent:{to_entity_id}");

        let paths = self
            .inner
            .tenant
            .read()
            .interact(move |conn| {
                crate::graph_paths::fetch_graph_relationship_paths(
                    conn,
                    &from_entity_id,
                    &to_entity_id,
                    max_hops,
                    as_of_ms,
                    limit,
                )
            })
            .await
            .map_err(solo_to_mcp)?;

        json_value_tool_result(serde_json::json!({
            "from": response_from,
            "to": response_to,
            "max_hops": max_hops,
            "paths": paths,
        }))
    }

    async fn handle_explain_provenance(
        &self,
        args: ExplainProvenanceArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let edge_id = args.edge_id.trim().to_string();
        if edge_id.is_empty() {
            return Err(McpError::invalid_params(
                "memory_explain_provenance: edge_id must not be empty".to_string(),
                None,
            ));
        }
        let edge_id_for_err = edge_id.clone();
        let result = self
            .inner
            .tenant
            .read()
            .interact(move |conn| {
                crate::graph_relationships::inspect_graph_relationship(conn, &edge_id)
            })
            .await
            .map_err(solo_to_mcp)?;

        let result = result.ok_or_else(|| {
            McpError::invalid_params(
                format!("memory_explain_provenance: relationship edge {edge_id_for_err} not found"),
                None,
            )
        })?;
        json_tool_result(&result)
    }

    async fn handle_contradictions(
        &self,
        args: ContradictionsArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let hits = solo_query::contradictions(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;
        legacy_array_tool_result(&hits, "contradictions")
    }

    async fn handle_contradiction_resolve(
        &self,
        args: ContradictionResolveArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        if args.a_id.trim().is_empty() || args.b_id.trim().is_empty() || args.kind.trim().is_empty()
        {
            return Err(McpError::invalid_params(
                "memory_contradiction_resolve: a_id, b_id, and kind must not be empty".to_string(),
                None,
            ));
        }
        // Dev-log 0152 H1: routed through the writer actor so the
        // UPDATE + audit row are atomic. The signature still takes
        // reader-pool + audit for now (deprecated; ignored by the
        // function body).
        let result = solo_query::resolve_contradiction(
            self.inner.tenant.write(),
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &args.a_id,
            &args.b_id,
            &args.kind,
            &args.status,
            args.resolution_note.as_deref(),
            args.winning_triple_id.as_deref(),
        )
        .await
        .map_err(solo_to_mcp)?;
        json_tool_result(&result)
    }

    async fn handle_inspect_cluster(
        &self,
        args: InspectClusterArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        if args.cluster_id.trim().is_empty() {
            return Err(McpError::invalid_params(
                "memory_inspect_cluster: cluster_id must not be empty".to_string(),
                None,
            ));
        }
        // `solo_to_mcp` maps `Error::NotFound` → `invalid_params` for
        // MCP (the protocol does not have a separate "not found" error
        // shape; clients see the message verbatim, which includes the
        // cluster_id).
        let record = solo_query::inspect_cluster(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &args.cluster_id,
            args.full_content,
        )
        .await
        .map_err(solo_to_mcp)?;
        json_tool_result(&record)
    }

    // Document handlers (v0.7.0+). Each wraps the corresponding writer
    // / query API; the MCP wire shape is plain JSON serialisation of
    // the returned report / records.

    async fn handle_ingest_document(
        &self,
        args: IngestDocumentArgs,
        progress: Option<crate::mcp_progress::ProgressReporter>,
        cancellation: crate::mcp_task::CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        cancellation.check()?;
        if args.path.trim().is_empty() {
            return Err(McpError::invalid_params(
                "memory_ingest_document: path must not be empty".to_string(),
                None,
            ));
        }
        let path = std::path::PathBuf::from(args.path);
        let checked_path = self
            .inner
            .workspace_file_access
            .check_path(&path)
            .map_err(solo_to_mcp)?;
        let path = if self.inner.workspace_file_access.is_restricted() {
            checked_path
        } else {
            path
        };
        // Defaults match what the daemon uses today (target 500 tokens,
        // 50-token overlap). Future: thread a per-call override through
        // the args struct if a use case appears.
        let chunk_config = solo_storage::document::ChunkConfig::default();

        // v0.11.0 P3: ingest checkpoints. The writer-actor's
        // `ingest_document_as` is one opaque command that internally
        // performs parse → chunk → embed → SQL insert; we bookend it
        // with phase-marker progress events. The 4-phase taxonomy
        // matches the MCP spec brief — `total=4`, `progress` walks 1
        // → 4 — even though phases 1 and 2 (parse, chunk) emit before
        // the writer call and 3 and 4 (embed, insert) emit after.
        // Real chunk-by-chunk progress would require redesigning the
        // writer command shape (cross-cuts ADR-0003); P3's bookend
        // pattern stays additive without touching the writer.
        const INGEST_TOTAL_PHASES: u64 = 4;
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            1,
            Some(INGEST_TOTAL_PHASES),
            Some("parsed"),
        );
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            2,
            Some(INGEST_TOTAL_PHASES),
            Some("chunked"),
        );
        cancellation.check()?;

        let report = self
            .inner
            .tenant
            .write()
            .ingest_document_as(self.inner.audit_principal.clone(), path, chunk_config)
            .await
            .map_err(solo_to_mcp)?;
        cancellation.check()?;

        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            3,
            Some(INGEST_TOTAL_PHASES),
            Some("embedded"),
        );
        // Final event includes the real chunk count from the report;
        // the per-event `message` field carries it so clients can
        // surface "N chunks indexed" without parsing the POST reply
        // body.
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            INGEST_TOTAL_PHASES,
            Some(INGEST_TOTAL_PHASES),
            Some(&format!("inserted {} chunks", report.chunks_persisted)),
        );

        let structured = serde_json::to_value(&report).map_err(|e| {
            McpError::internal_error(format!("serialize structured result: {e}"), None)
        })?;
        json_value_tool_result_with_links(
            structured,
            vec![document_resource_link(&report.doc_id.to_string(), None)],
        )
    }

    async fn handle_document_upload_prepare(
        &self,
        args: DocumentUploadPrepareArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let request = crate::document_upload::UploadPrepareRequest {
            filename: args.filename,
            mime_type: args.mime_type,
            size_bytes: args.size_bytes,
            sha256: args.sha256,
        };
        let response = crate::document_upload::prepare_upload(
            self.inner.registry.data_dir(),
            request,
            &self.inner.tenant.config().documents.allowed_extensions,
            self.inner
                .tenant
                .config()
                .documents
                .store_original_files_by_default,
        )
        .map_err(solo_to_mcp)?;
        json_tool_result(&response)
    }

    async fn handle_document_upload_status(
        &self,
        args: DocumentUploadStatusArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let response =
            crate::document_upload::upload_status(self.inner.registry.data_dir(), &args.upload_id)
                .map_err(solo_to_mcp)?;
        json_tool_result(&response)
    }

    async fn handle_document_upload_chunk_base64(
        &self,
        args: DocumentUploadChunkBase64Args,
    ) -> std::result::Result<CallToolResult, McpError> {
        let status =
            crate::document_upload::upload_status(self.inner.registry.data_dir(), &args.upload_id)
                .map_err(solo_to_mcp)?;
        if status.size_bytes > crate::document_upload::MCP_BASE64_CHUNK_BYTES as u64 {
            return Err(McpError::invalid_params(
                format!(
                    "MCP base64 fallback only supports uploads <= {} bytes; use raw HTTP PATCH for larger files",
                    crate::document_upload::MCP_BASE64_CHUNK_BYTES
                ),
                None,
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(args.chunk_base64.trim())
            .map_err(|e| McpError::invalid_params(format!("invalid chunk_base64: {e}"), None))?;
        if bytes.len() > crate::document_upload::MCP_BASE64_CHUNK_BYTES {
            return Err(McpError::invalid_params(
                format!(
                    "decoded base64 chunk must be <= {} bytes",
                    crate::document_upload::MCP_BASE64_CHUNK_BYTES
                ),
                None,
            ));
        }
        let response = crate::document_upload::append_upload_chunk(
            self.inner.registry.data_dir(),
            &args.upload_id,
            args.offset,
            args.upload_length,
            &bytes,
        )
        .await
        .map_err(solo_to_mcp)?;
        json_tool_result(&response)
    }

    async fn handle_document_upload_commit(
        &self,
        args: DocumentUploadCommitArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let response = crate::document_upload::commit_upload(
            self.inner.registry.data_dir(),
            &args.upload_id,
            crate::document_upload::UploadCommitRequest {
                sha256: args.sha256,
            },
        )
        .await
        .map_err(solo_to_mcp)?;
        json_tool_result(&response)
    }

    async fn handle_document_upload_abort(
        &self,
        args: DocumentUploadAbortArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let response =
            crate::document_upload::abort_upload(self.inner.registry.data_dir(), &args.upload_id)
                .map_err(solo_to_mcp)?;
        json_tool_result(&response)
    }

    async fn handle_ingest_staged_document(
        &self,
        args: IngestStagedDocumentArgs,
        progress: Option<crate::mcp_progress::ProgressReporter>,
        cancellation: crate::mcp_task::CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        const INGEST_TOTAL_PHASES: u64 = 4;
        cancellation.check()?;
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            1,
            Some(INGEST_TOTAL_PHASES),
            Some("validated_staged_upload"),
        );
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            2,
            Some(INGEST_TOTAL_PHASES),
            Some("ingesting_staged_upload"),
        );
        cancellation.check()?;
        let response = crate::document_upload::ingest_staged_document(
            self.inner.registry.data_dir(),
            self.inner.tenant.as_ref(),
            self.inner.audit_principal.clone(),
            crate::document_upload::StagedIngestRequest {
                staged_uri: args.staged_uri,
                retain_source_file: args.retain_source_file,
                store_original_file: args.store_original_file,
            },
        )
        .await
        .map_err(solo_to_mcp)?;
        cancellation.check()?;
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            3,
            Some(INGEST_TOTAL_PHASES),
            Some(response.extraction_status.as_str()),
        );
        let completion_message = if response.document_id.is_some() {
            format!("inserted {} chunks", response.chunks_persisted)
        } else {
            format!("retained asset ({})", response.extraction_status)
        };
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            INGEST_TOTAL_PHASES,
            Some(INGEST_TOTAL_PHASES),
            Some(&completion_message),
        );
        let structured = serde_json::to_value(&response).map_err(|e| {
            McpError::internal_error(format!("serialize structured result: {e}"), None)
        })?;
        let mut links = Vec::new();
        if let Some(doc_id) = response.document_id.as_deref() {
            links.push(document_resource_link(doc_id, None));
        }
        if let Some(asset) = response.asset.as_ref() {
            links.push(asset_resource_link(
                &asset.asset_id.to_string(),
                asset.filename.clone(),
            ));
        }
        json_value_tool_result_with_links(structured, links)
    }

    async fn handle_import_documents(
        &self,
        args: ImportDocumentsArgs,
        progress: Option<crate::mcp_progress::ProgressReporter>,
        cancellation: crate::mcp_task::CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        cancellation.check()?;
        let raw_path = args.path.trim();
        if raw_path.is_empty() {
            return Err(McpError::invalid_params(
                "memory_import_documents: path must not be empty".to_string(),
                None,
            ));
        }
        if args.max_files == 0 {
            return Err(McpError::invalid_params(
                "memory_import_documents: max_files must be > 0".to_string(),
                None,
            ));
        }
        if args.max_files > MAX_IMPORT_MAX_FILES {
            return Err(McpError::invalid_params(
                format!("memory_import_documents: max_files must be <= {MAX_IMPORT_MAX_FILES}"),
                None,
            ));
        }

        let import_source = parse_import_source(args.source.as_deref())?;
        let requested_path = expand_home_prefix(raw_path, native_import_home_dir().as_deref());
        let checked_path = self
            .inner
            .workspace_file_access
            .check_path(&requested_path)
            .map_err(solo_to_mcp)?;
        let response_path = requested_path.display().to_string();
        let import_path = if self.inner.workspace_file_access.is_restricted() {
            checked_path
        } else {
            requested_path
        };

        let allowed_extensions = native_import_allowed_extensions(
            &self.inner.tenant.config().documents.allowed_extensions,
            import_source,
        );
        let scan_path = import_path.clone();
        let recursive = args.recursive;
        let max_files = args.max_files;
        crate::mcp_progress::report_if_some(progress.as_ref(), 1, Some(3), Some("scanning"));
        let scan_cancellation = cancellation.clone();
        let (files, truncated) = tokio::task::spawn_blocking(move || {
            collect_import_files(
                &scan_path,
                recursive,
                &allowed_extensions,
                max_files,
                &scan_cancellation,
            )
        })
        .await
        .map_err(|e| McpError::internal_error(format!("import scan task failed: {e}"), None))??;
        cancellation.check()?;
        crate::mcp_progress::report_if_some(
            progress.as_ref(),
            2,
            Some(3),
            Some(&format!("found {} files", files.len())),
        );

        let total_bytes = files.iter().map(|file| file.bytes).sum();
        let workspace_roots = workspace_roots_response(&self.inner.workspace_file_access);
        if args.dry_run {
            let response = ImportResponse {
                path: response_path,
                source: import_source.response_source().to_string(),
                source_label: import_source.response_label().to_string(),
                dry_run: true,
                recursive: args.recursive,
                truncated,
                total_files: files.len(),
                total_bytes,
                store_original_file: args.store_original_file,
                imported: 0,
                deduped: 0,
                failed: 0,
                chunks_persisted: 0,
                assets_retained: 0,
                assets_deduped: 0,
                asset_links: 0,
                asset_failed: 0,
                workspace_roots,
                files,
                results: Vec::new(),
            };
            return json_tool_result(&response);
        }

        let chunk_config =
            chunk_config_from_document_config_mcp(&self.inner.tenant.config().documents)?;
        let ingest = self
            .ingest_import_files(
                &files,
                chunk_config,
                progress.as_ref(),
                cancellation.clone(),
                args.store_original_file,
            )
            .await?;
        cancellation.check()?;
        crate::mcp_progress::report_if_some(progress.as_ref(), 3, Some(3), Some("imported"));

        let response = ImportResponse {
            path: response_path,
            source: import_source.response_source().to_string(),
            source_label: import_source.response_label().to_string(),
            dry_run: false,
            recursive: args.recursive,
            truncated,
            total_files: files.len(),
            total_bytes,
            store_original_file: args.store_original_file,
            imported: ingest.imported,
            deduped: ingest.deduped,
            failed: ingest.failed,
            chunks_persisted: ingest.chunks_persisted,
            assets_retained: ingest.assets_retained,
            assets_deduped: ingest.assets_deduped,
            asset_links: ingest.asset_links,
            asset_failed: ingest.asset_failed,
            workspace_roots,
            files,
            results: ingest.results,
        };
        let links = import_response_resource_links(&response.results);
        let structured = serde_json::to_value(&response).map_err(|e| {
            McpError::internal_error(format!("serialize import response: {e}"), None)
        })?;
        json_value_tool_result_with_links(structured, links)
    }

    async fn ingest_import_files(
        &self,
        files: &[ImportFile],
        chunk_config: solo_storage::ChunkConfig,
        progress: Option<&crate::mcp_progress::ProgressReporter>,
        cancellation: crate::mcp_task::CancellationToken,
        store_original_file: bool,
    ) -> std::result::Result<ImportIngestSummary, McpError> {
        let mut imported = 0u32;
        let mut deduped = 0u32;
        let mut failed = 0u32;
        let mut chunks_persisted = 0u32;
        let mut assets_retained = 0u32;
        let mut assets_deduped = 0u32;
        let mut asset_links = 0u32;
        let mut asset_failed = 0u32;
        let mut results = Vec::with_capacity(files.len());
        let total = files.len() as u64;

        for (idx, file) in files.iter().enumerate() {
            cancellation.check()?;
            crate::mcp_progress::report_if_some(
                progress,
                idx as u64,
                Some(total),
                Some(&format!("importing {}", file.path)),
            );
            match self
                .inner
                .tenant
                .write()
                .ingest_document_as(
                    self.inner.audit_principal.clone(),
                    file.path_buf.clone(),
                    chunk_config.clone(),
                )
                .await
            {
                Ok(report) => {
                    let mut asset = None;
                    let mut document_asset_link = None;
                    let mut asset_error = None;
                    if store_original_file {
                        match self.retain_import_original_file(file, report.doc_id).await {
                            Ok((stored_asset, link)) => {
                                assets_retained += 1;
                                if stored_asset.deduped {
                                    assets_deduped += 1;
                                }
                                asset_links += 1;
                                asset = Some(stored_asset);
                                document_asset_link = Some(link);
                            }
                            Err(err) => {
                                asset_failed += 1;
                                asset_error = Some(err.to_string());
                            }
                        }
                    }
                    if report.deduped {
                        deduped += 1;
                    } else {
                        imported += 1;
                    }
                    chunks_persisted += report.chunks_persisted;
                    results.push(ImportResult {
                        path: file.path.clone(),
                        bytes: file.bytes,
                        doc_id: Some(report.doc_id.to_string()),
                        chunks_persisted: report.chunks_persisted,
                        bytes_ingested: report.bytes_ingested,
                        deduped: report.deduped,
                        asset,
                        document_asset_link,
                        asset_error,
                        error: None,
                    });
                }
                Err(err) => {
                    failed += 1;
                    results.push(ImportResult {
                        path: file.path.clone(),
                        bytes: file.bytes,
                        doc_id: None,
                        chunks_persisted: 0,
                        bytes_ingested: 0,
                        deduped: false,
                        asset: None,
                        document_asset_link: None,
                        asset_error: None,
                        error: Some(err.to_string()),
                    });
                }
            }
        }

        Ok(ImportIngestSummary {
            imported,
            deduped,
            failed,
            chunks_persisted,
            assets_retained,
            assets_deduped,
            asset_links,
            asset_failed,
            results,
        })
    }

    async fn retain_import_original_file(
        &self,
        file: &ImportFile,
        doc_id: DocumentId,
    ) -> std::result::Result<
        (
            solo_storage::StoredAssetReport,
            solo_storage::DocumentAssetLinkReport,
        ),
        McpError,
    > {
        crate::document_upload::retain_original_file_for_document(
            self.inner.tenant.as_ref(),
            self.inner.audit_principal.clone(),
            doc_id,
            crate::document_upload::RetainOriginalFileRequest {
                path: file.path_buf.clone(),
                filename: file
                    .path_buf
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string),
                mime_type: crate::document_upload::document_mime_type_for_path(&file.path_buf),
                size_bytes: Some(file.bytes),
                sha256: None,
                source: Some(file.path.clone()),
                relation_type: "source_import".to_string(),
                note: Some("original local import file".to_string()),
            },
        )
        .await
        .map_err(solo_to_mcp)
    }

    async fn handle_search_docs(
        &self,
        args: SearchDocsArgs,
        progress: Option<crate::mcp_progress::ProgressReporter>,
        cancellation: crate::mcp_task::CancellationToken,
    ) -> std::result::Result<CallToolResult, McpError> {
        cancellation.check()?;
        // v0.11.0 P3: progress emission for search is gated on `top_k`
        // (passed via `args.limit`) — below 100 the search completes
        // fast enough that progress notifications add wire-overhead
        // with no UX benefit (Decision C). Above threshold + client
        // opted in, emit 3 phase-marker events around the query call.
        let top_k = args.limit as u32;
        let progress_active = progress.is_some()
            && top_k > crate::mcp_progress::MCP_SEARCH_DOCS_PROGRESS_TOP_K_THRESHOLD;
        let progress_reporter = if progress_active {
            progress.as_ref()
        } else {
            None
        };
        const SEARCH_TOTAL_PHASES: u64 = 3;
        crate::mcp_progress::report_if_some(
            progress_reporter,
            1,
            Some(SEARCH_TOTAL_PHASES),
            Some("hnsw_lookup"),
        );

        // `solo_query::run_doc_search` validates empty queries (returns
        // InvalidInput → invalid_params via solo_to_mcp) and clamps
        // limit upstream of the embedder call.
        let hits = solo_query::run_doc_search(
            self.inner.tenant.as_ref(),
            self.inner.audit_principal.clone(),
            &args.query,
            args.limit,
        )
        .await
        .map_err(solo_to_mcp)?;
        cancellation.check()?;

        crate::mcp_progress::report_if_some(
            progress_reporter,
            2,
            Some(SEARCH_TOTAL_PHASES),
            Some("reranked"),
        );
        crate::mcp_progress::report_if_some(
            progress_reporter,
            SEARCH_TOTAL_PHASES,
            Some(SEARCH_TOTAL_PHASES),
            Some(&format!("returning {} hits", hits.len())),
        );

        let hit_links = hits
            .iter()
            .flat_map(|hit| {
                [
                    document_resource_link(&hit.doc_id, hit.doc_title.clone()),
                    document_chunk_resource_link(
                        &hit.doc_id,
                        &hit.chunk_id,
                        hit.doc_title
                            .as_ref()
                            .map(|title| format!("{title} chunk {}", hit.chunk_index)),
                    ),
                ]
            })
            .collect::<Vec<_>>();
        let hits_value = serde_json::to_value(&hits)
            .map_err(|e| McpError::internal_error(format!("serialize doc hits: {e}"), None))?;
        json_value_tool_result_with_links(serde_json::json!({ "hits": hits_value }), hit_links)
    }

    async fn handle_inspect_document(
        &self,
        args: InspectDocumentArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let doc_id = DocumentId::from_str(&args.doc_id)
            .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?;
        let result_opt = solo_query::inspect_document(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &doc_id,
        )
        .await
        .map_err(solo_to_mcp)?;
        match result_opt {
            Some(record) => {
                let links = std::iter::once(document_resource_link(
                    &record.document.doc_id,
                    record.document.title.clone(),
                ))
                .chain(record.chunks.iter().map(|chunk| {
                    document_chunk_resource_link(
                        &record.document.doc_id,
                        &chunk.chunk_id,
                        record
                            .document
                            .title
                            .as_ref()
                            .map(|title| format!("{title} chunk {}", chunk.chunk_index)),
                    )
                }))
                .collect::<Vec<_>>();
                let structured = serde_json::to_value(&record).map_err(|e| {
                    McpError::internal_error(
                        format!("serialize structured document result: {e}"),
                        None,
                    )
                })?;
                json_value_tool_result_with_links(structured, links)
            }
            None => Err(McpError::invalid_params(
                format!("document {doc_id} not found"),
                None,
            )),
        }
    }

    async fn handle_list_documents(
        &self,
        args: ListDocumentsArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let rows = solo_query::list_documents(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            args.limit,
            args.offset,
            args.include_forgotten,
        )
        .await
        .map_err(solo_to_mcp)?;
        legacy_array_tool_result(&rows, "documents")
    }

    async fn handle_list_assets(
        &self,
        args: ListAssetsArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let rows = solo_query::list_assets(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            args.limit,
            args.offset,
            args.include_deleted,
        )
        .await
        .map_err(solo_to_mcp)?;
        let links = rows
            .iter()
            .map(|asset| asset_resource_link(&asset.asset_id, asset.filename.clone()))
            .collect::<Vec<_>>();
        json_value_tool_result_with_links(serde_json::json!({ "assets": rows }), links)
    }

    async fn handle_inspect_asset(
        &self,
        args: InspectAssetArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let asset_id = AssetId::from_str(&args.asset_id)
            .map_err(|e| McpError::invalid_params(format!("invalid asset_id: {e}"), None))?;
        let result_opt = solo_query::inspect_asset(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &asset_id,
        )
        .await
        .map_err(solo_to_mcp)?;
        let Some(record) = result_opt else {
            return Err(McpError::invalid_params(
                format!("asset {asset_id} not found"),
                None,
            ));
        };
        let mut links = vec![asset_resource_link(
            &record.asset.asset_id,
            record.asset.filename.clone(),
        )];
        links.extend(
            record
                .document_links
                .iter()
                .map(|link| document_resource_link(&link.doc_id, link.doc_title.clone())),
        );
        links.extend(record.memory_attachments.iter().filter_map(|link| {
            link.asset_id
                .as_ref()
                .map(|asset_id| asset_resource_link(asset_id, link.asset_filename.clone()))
                .or_else(|| {
                    link.doc_id
                        .as_ref()
                        .map(|doc_id| document_resource_link(doc_id, link.doc_title.clone()))
                })
        }));
        let structured = serde_json::to_value(&record).map_err(|e| {
            McpError::internal_error(format!("serialize structured asset result: {e}"), None)
        })?;
        json_value_tool_result_with_links(structured, links)
    }

    async fn handle_prepare_asset_download(
        &self,
        args: PrepareAssetDownloadArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let asset_id = AssetId::from_str(&args.asset_id)
            .map_err(|e| McpError::invalid_params(format!("invalid asset_id: {e}"), None))?;
        let target = solo_query::prepare_asset_download(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            self.inner.tenant.snapshot_dir(),
            &asset_id,
        )
        .await
        .map_err(solo_to_mcp)?;
        let contract = crate::asset_download::direct_asset_download_contract(&target);
        let structured = serde_json::to_value(&contract).map_err(|e| {
            McpError::internal_error(format!("serialize asset download contract: {e}"), None)
        })?;
        json_value_tool_result_with_links(
            structured,
            vec![asset_resource_link(
                &target.asset.asset_id,
                target.asset.filename.clone(),
            )],
        )
    }

    async fn handle_prepare_document_source_download(
        &self,
        args: PrepareDocumentSourceDownloadArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let doc_id = DocumentId::from_str(&args.doc_id)
            .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?;
        let result_opt = solo_query::list_document_assets(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &doc_id,
        )
        .await
        .map_err(solo_to_mcp)?;
        let Some(result) = result_opt else {
            return Err(McpError::invalid_params(
                format!("document {doc_id} not found"),
                None,
            ));
        };
        let source_asset_link = result
            .assets
            .into_iter()
            .find(|link| link.relation_type == "source_upload")
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("document {doc_id} has no linked source_upload asset"),
                    None,
                )
            })?;
        let asset_id = AssetId::from_str(&source_asset_link.asset_id)
            .map_err(|e| McpError::internal_error(format!("invalid linked asset_id: {e}"), None))?;
        let target = solo_query::prepare_asset_download(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            self.inner.tenant.snapshot_dir(),
            &asset_id,
        )
        .await
        .map_err(solo_to_mcp)?;
        let download = crate::asset_download::direct_asset_download_contract(&target);
        let structured = serde_json::json!({
            "doc_id": doc_id.to_string(),
            "source_asset_link": source_asset_link,
            "download": download,
        });
        json_value_tool_result_with_links(
            structured,
            vec![
                document_resource_link(&doc_id.to_string(), None),
                asset_resource_link(&target.asset.asset_id, target.asset.filename.clone()),
            ],
        )
    }

    async fn handle_list_document_assets(
        &self,
        args: ListDocumentAssetsArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let doc_id = DocumentId::from_str(&args.doc_id)
            .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?;
        let result_opt = solo_query::list_document_assets(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            &doc_id,
        )
        .await
        .map_err(solo_to_mcp)?;
        let Some(result) = result_opt else {
            return Err(McpError::invalid_params(
                format!("document {doc_id} not found"),
                None,
            ));
        };
        let links = std::iter::once(document_resource_link(&result.doc_id, None))
            .chain(
                result
                    .assets
                    .iter()
                    .map(|link| asset_resource_link(&link.asset_id, link.asset_filename.clone())),
            )
            .collect::<Vec<_>>();
        let structured = serde_json::to_value(&result).map_err(|e| {
            McpError::internal_error(
                format!("serialize structured document assets result: {e}"),
                None,
            )
        })?;
        json_value_tool_result_with_links(structured, links)
    }

    async fn handle_list_memory_attachments(
        &self,
        args: ListMemoryAttachmentsArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let memory_id = MemoryId::from_str(&args.memory_id)
            .map_err(|e| McpError::invalid_params(format!("invalid memory_id: {e}"), None))?;
        let result_opt = solo_query::list_memory_attachments(
            self.inner.tenant.read(),
            self.inner.tenant.audit(),
            self.inner.audit_principal.clone(),
            memory_id,
        )
        .await
        .map_err(solo_to_mcp)?;
        let Some(result) = result_opt else {
            return Err(McpError::invalid_params(
                format!("memory {memory_id} not found"),
                None,
            ));
        };
        let links = std::iter::once(memory_resource_link(&result.memory_id, None))
            .chain(result.attachments.iter().filter_map(|link| {
                link.asset_id
                    .as_ref()
                    .map(|asset_id| asset_resource_link(asset_id, link.asset_filename.clone()))
                    .or_else(|| {
                        link.doc_id
                            .as_ref()
                            .map(|doc_id| document_resource_link(doc_id, link.doc_title.clone()))
                    })
            }))
            .collect::<Vec<_>>();
        let structured = serde_json::to_value(&result).map_err(|e| {
            McpError::internal_error(
                format!("serialize structured memory attachments result: {e}"),
                None,
            )
        })?;
        json_value_tool_result_with_links(structured, links)
    }

    async fn handle_forget_asset(
        &self,
        args: ForgetAssetArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let asset_id = AssetId::from_str(&args.asset_id)
            .map_err(|e| McpError::invalid_params(format!("invalid asset_id: {e}"), None))?;
        let report = self
            .inner
            .tenant
            .write()
            .forget_asset_as(self.inner.audit_principal.clone(), asset_id)
            .await
            .map_err(solo_to_mcp)?;
        json_tool_result(&report)
    }

    async fn handle_forget_document(
        &self,
        args: ForgetDocumentArgs,
    ) -> std::result::Result<CallToolResult, McpError> {
        let doc_id = DocumentId::from_str(&args.doc_id)
            .map_err(|e| McpError::invalid_params(format!("invalid doc_id: {e}"), None))?;
        let report = self
            .inner
            .tenant
            .write()
            .forget_document_as(self.inner.audit_principal.clone(), doc_id)
            .await
            .map_err(solo_to_mcp)?;
        json_tool_result(&report)
    }
}

#[cfg(test)]
mod dispatch_tests {
    //! In-process integration tests for the MCP tool surface. We invoke
    //! `SoloMcpServer::dispatch_tool` directly (bypasses the rmcp
    //! protocol framing + `RequestContext`, which requires a `Peer`
    //! that's not constructible outside rmcp internals). The server is
    //! constructed against a real WriterActor + ReaderPool +
    //! StubEmbedder + StubVectorIndex from `solo_storage::test_support`.
    //!
    //! Tests live inline in this module rather than `tests/` because an
    //! external integration-test exe in `target/debug/deps/mcp_dispatch-*`
    //! tripped Windows UAC ERROR_ELEVATION_REQUIRED on the dev machine.
    //! The lib test binary doesn't have that issue.
    use super::*;
    use serde_json::json;
    use solo_core::VectorIndex;
    use solo_storage::test_support::StubVectorIndex;
    use solo_storage::{
        EmbedderConfig, IdentityConfig, KeyMaterial, LibraryHandle, MemoryLibrary, ReaderPool,
        SoloConfig, StubEmbedder, WriterActor, WriterSpawn,
    };
    use std::sync::Arc as StdArc;

    fn fake_config(dim: u32) -> SoloConfig {
        SoloConfig {
            schema_version: 1,
            salt_hex: "00000000000000000000000000000000".to_string(),
            embedder: EmbedderConfig {
                name: "stub".to_string(),
                version: "v1".to_string(),
                dim,
                dtype: "f32".to_string(),
            },
            identity: IdentityConfig::default(),
            documents: solo_storage::DocumentConfig::default(),
            workspace_file_access: solo_storage::WorkspaceFileAccessConfig::default(),
            auth: None,
            audit: solo_storage::AuditSettings::default(),
            redaction: solo_storage::RedactionConfig::default(),
            llm: None,
            triples: solo_storage::TriplesConfig::default(),
            sampling: solo_storage::SamplingConfig::default(),
            steward: solo_storage::StewardSettings::default(),
        }
    }

    struct Harness {
        server: SoloMcpServer,
        _tmp: tempfile::TempDir,
        db_path: std::path::PathBuf,
        write_handle_extra: Option<solo_storage::WriteHandle>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Harness {
        fn new(runtime: &tokio::runtime::Runtime) -> Self {
            Self::new_with_workspace_file_access(
                runtime,
                crate::WorkspaceFileAccessPolicy::unrestricted(),
            )
        }

        fn new_with_embedder(runtime: &tokio::runtime::Runtime) -> Self {
            Self::new_with_workspace_file_access_and_embedder(
                runtime,
                crate::WorkspaceFileAccessPolicy::unrestricted(),
                true,
            )
        }

        fn new_with_workspace_file_access(
            runtime: &tokio::runtime::Runtime,
            workspace_file_access: crate::WorkspaceFileAccessPolicy,
        ) -> Self {
            Self::new_with_workspace_file_access_and_embedder(runtime, workspace_file_access, false)
        }

        fn new_with_workspace_file_access_and_embedder(
            runtime: &tokio::runtime::Runtime,
            workspace_file_access: crate::WorkspaceFileAccessPolicy,
            writer_with_embedder: bool,
        ) -> Self {
            use solo_storage::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};

            let tmp = tempfile::TempDir::new().unwrap();
            let dim = 16usize;
            let hnsw: StdArc<dyn VectorIndex + Send + Sync> =
                StdArc::new(StubVectorIndex::new(dim));
            let embedder: StdArc<dyn solo_core::Embedder> =
                StdArc::new(StubEmbedder::new("stub", "v1", dim));

            let conn = solo_storage::test_support::open_test_db_at(&tmp.path().join("test.db"));
            let embedder_id = get_or_insert_embedder_id(
                &conn,
                &EmbedderIdentity {
                    name: "stub".into(),
                    version: "v1".into(),
                    dim: dim as u32,
                    dtype: "f32".into(),
                },
            )
            .expect("register stub embedder");
            let WriterSpawn { handle, join } = if writer_with_embedder {
                runtime.block_on(async {
                    WriterActor::spawn_full_with_embedder(
                        conn,
                        hnsw.clone(),
                        tmp.path().to_path_buf(),
                        embedder_id,
                        embedder.clone(),
                    )
                })
            } else {
                WriterActor::spawn_full(conn, hnsw.clone(), tmp.path().to_path_buf(), embedder_id)
            };

            // ReaderPool's deadpool::Pool needs a live tokio runtime for
            // both build + drop; build inside block_on.
            let path = tmp.path().join("test.db");
            let pool: ReaderPool =
                runtime.block_on(async { ReaderPool::new(&path, None, hnsw.clone()).unwrap() });

            let tenant_id = solo_core::LibraryId::default_tenant();
            let tenant_handle = StdArc::new(LibraryHandle::from_parts_for_tests(
                tenant_id.clone(),
                fake_config(dim as u32),
                path.clone(),
                tmp.path().to_path_buf(),
                embedder_id,
                hnsw,
                embedder.clone(),
                handle.clone(),
                std::thread::spawn(|| {}),
                pool,
            ));
            let key = KeyMaterial::from_bytes_for_tests([0u8; 32]);
            let registry = StdArc::new(MemoryLibrary::for_tests_with_single_tenant(
                tmp.path().to_path_buf(),
                key,
                embedder,
                tenant_handle.clone(),
            ));
            let server = SoloMcpServer::new_for_tenant_with_workspace_file_access(
                registry,
                tenant_handle,
                Vec::new(),
                workspace_file_access,
            );
            Harness {
                server,
                _tmp: tmp,
                db_path: path,
                write_handle_extra: Some(handle),
                join: Some(join),
            }
        }

        fn open_db(&self) -> rusqlite::Connection {
            solo_storage::test_support::open_test_db_at(&self.db_path)
        }

        fn shutdown(mut self, runtime: &tokio::runtime::Runtime) {
            // The whole shutdown runs inside block_on so deadpool-sqlite's
            // drop (which schedules cleanup on the active runtime) sees a
            // live reactor. Without this, dropping the SoloMcpServer
            // (which holds the ReaderPool through its Arc<Inner>) panics
            // with "no reactor running".
            let join = self.join.take();
            let extra = self.write_handle_extra.take();
            runtime.block_on(async move {
                drop(extra);
                drop(self.server);
                if let Some(join) = join {
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(join.join());
                    });
                    tokio::task::spawn_blocking(move || {
                        rx.recv_timeout(std::time::Duration::from_secs(5))
                    })
                    .await
                    .expect("blocking task")
                    .expect("writer thread did not exit within 5s")
                    .expect("writer thread panicked");
                }
                // Keep the temporary directory alive until SQLite and the
                // writer actor have released their files. Removing an open
                // database directory can block or fail on Windows.
                drop(self._tmp);
            });
        }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Pull the first Content::text body out of a CallToolResult. Use
    /// serde_json roundtrip as a robust extractor — `Content`'s public
    /// API doesn't directly expose the inner text without going through
    /// pattern-matching on RawContent.
    fn first_text(r: &rmcp::model::CallToolResult) -> String {
        let first = r.content.first().expect("at least one content item");
        let v = serde_json::to_value(first).expect("content serialises");
        v.get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("{v}"))
    }

    fn seed_episode(conn: &rusqlite::Connection, content: &str) -> (MemoryId, i64) {
        let memory_id = MemoryId::new();
        conn.execute(
            "INSERT INTO episodes
                (memory_id, ts_ms, source_type, content, confidence, strength,
                 salience, tier, status, created_at_ms, updated_at_ms)
             VALUES (?1, 0, 'test', ?2, 0.9, 0.5, 0.5, 'hot', 'active', 0, 0)",
            rusqlite::params![memory_id.to_string(), content],
        )
        .expect("seed episode");
        (memory_id, conn.last_insert_rowid())
    }

    fn seed_triple_row(
        conn: &rusqlite::Connection,
        triple_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        source_episode_rowid: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO triples
                 (triple_id, subject_id, predicate, object_id, object_kind,
                  valid_from_ms, valid_to_ms, confidence, provenance_json,
                  status, created_at_ms, updated_at_ms, source_episode_id)
                 VALUES (?1, ?2, ?3, ?4, 'literal', 0, NULL, 0.9, '{}',
                         'active', 0, 0, ?5)",
            rusqlite::params![triple_id, subject, predicate, object, source_episode_rowid],
        )
        .expect("seed triple");
    }

    fn seed_relationship_edge_for_triple(
        conn: &rusqlite::Connection,
        edge_id: &str,
        triple_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        source_episode_rowid: i64,
        source_memory_id: &str,
    ) {
        for entity in [subject, object] {
            conn.execute(
                "INSERT OR IGNORE INTO entities
                    (entity_id, canonical_name, entity_type, aliases_json,
                     confidence, first_seen_ms, last_seen_ms, status,
                     created_at_ms, updated_at_ms)
                 VALUES (?1, ?1, 'unknown', '[]',
                         0.9, 100, 500, 'active', 100, 500)",
                rusqlite::params![entity],
            )
            .expect("seed entity");
        }
        conn.execute(
            "INSERT INTO relationship_edges
                (edge_id, subject_entity_id, predicate, object_entity_id,
                 object_literal, object_kind, valid_from_ms, valid_to_ms,
                 confidence, strength, evidence_count, status,
                 created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4,
                     NULL, 'entity', 100, 500,
                     0.88, 0.77, 0, 'active',
                     100, 500)",
            rusqlite::params![edge_id, subject, predicate, object],
        )
        .expect("seed relationship edge");
        conn.execute(
            "INSERT INTO relationship_evidence
                (evidence_id, edge_id, triple_id, memory_id,
                 source_episode_id, extraction_confidence, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, 0.88, 789)",
            rusqlite::params![
                format!("ev-{edge_id}"),
                edge_id,
                triple_id,
                source_memory_id,
                source_episode_rowid
            ],
        )
        .expect("seed relationship evidence");
    }

    fn seed_contradiction_row(conn: &rusqlite::Connection, a_id: &str, b_id: &str, kind: &str) {
        conn.execute(
            "INSERT INTO contradictions
                 (a_memory_id, b_memory_id, kind, explanation, detected_at_ms,
                  status, resolved_at_ms, resolution_note, winning_triple_id)
                 VALUES (?1, ?2, ?3, 'test contradiction', 0,
                         'unresolved', NULL, NULL, NULL)",
            rusqlite::params![a_id, b_id, kind],
        )
        .expect("seed contradiction");
    }

    #[test]
    fn tools_list_returns_canonical_tools() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let tools = h.server.dispatch_list_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert_eq!(
            names,
            vec![
                "memory_remember",
                // v0.9.2 — batched-remember for agentic clients.
                "memory_remember_batch",
                "memory_recall",
                "memory_context",
                "memory_forget",
                "memory_inspect",
                "memory_update",
                "memory_inbox",
                "memory_review",
                "memory_attach",
                "memory_link_document_asset",
                // Derived-layer tools added in v0.4.0:
                "memory_themes",
                "memory_facts_about",
                "memory_entities",
                "memory_request_entity_split",
                "memory_graph_paths",
                "memory_explain_provenance",
                "memory_contradictions",
                "memory_contradiction_resolve",
                // Added in v0.5.0 (Priority 3):
                "memory_inspect_cluster",
                // Document tools added in v0.7.0:
                "memory_ingest_document",
                "document_upload_prepare",
                "document_upload_status",
                "document_upload_chunk_base64",
                "document_upload_commit",
                "document_upload_abort",
                "memory_ingest_staged_document",
                "memory_import_documents",
                "memory_search_docs",
                "memory_inspect_document",
                "memory_list_documents",
                "memory_list_assets",
                "memory_inspect_asset",
                "memory_prepare_asset_download",
                "memory_prepare_document_source_download",
                "memory_list_document_assets",
                "memory_list_memory_attachments",
                "memory_forget_asset",
                "memory_forget_document",
            ]
        );
        for t in &tools {
            // rmcp 1.x: Tool.description is Option<Cow<'static, str>>.
            let desc = t.description.as_deref().unwrap_or("");
            assert!(!desc.is_empty(), "{} description empty", t.name);
            let _schema = t.schema_as_json_value();
            // `required` is intentionally absent on memory_themes +
            // memory_contradictions + memory_list_documents (all args
            // optional with defaults). memory_facts_about has required
            // = ["subject"], etc. We don't assert per-tool 'required'
            // shape here; the schema's `properties` field is the more
            // important signal and is always present.
        }
        h.shutdown(&runtime);
    }

    #[test]
    fn tools_list_declares_output_schemas_and_access_annotations() {
        let tools = build_tools();
        assert_eq!(tools.len(), 39);

        for tool in &tools {
            let output_schema = tool
                .output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("{} missing outputSchema", tool.name));
            assert_eq!(
                output_schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "{} outputSchema root must be an object",
                tool.name
            );

            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} missing annotations", tool.name));
            assert_eq!(
                annotations.open_world_hint,
                Some(false),
                "{} should be closed-world local Solo state",
                tool.name
            );

            match tool_access_for(tool.name.as_ref()) {
                ToolAccess::Read => {
                    assert_eq!(
                        annotations.read_only_hint,
                        Some(true),
                        "{} should be read-only",
                        tool.name
                    );
                    assert_eq!(
                        annotations.destructive_hint,
                        Some(false),
                        "{} read-only tool cannot be destructive",
                        tool.name
                    );
                }
                ToolAccess::AdditiveWrite => {
                    assert_eq!(
                        annotations.read_only_hint,
                        Some(false),
                        "{} should be marked as a write",
                        tool.name
                    );
                    assert_eq!(
                        annotations.destructive_hint,
                        Some(false),
                        "{} should be marked additive, not destructive",
                        tool.name
                    );
                }
                ToolAccess::DestructiveWrite => {
                    assert_eq!(
                        annotations.read_only_hint,
                        Some(false),
                        "{} should be marked as a write",
                        tool.name
                    );
                    assert_eq!(
                        annotations.destructive_hint,
                        Some(true),
                        "{} should be marked destructive",
                        tool.name
                    );
                }
            }
        }

        let context = tools
            .iter()
            .find(|tool| tool.name == "memory_context")
            .expect("memory_context exists");
        assert_eq!(
            context.annotations.as_ref().and_then(|a| a.read_only_hint),
            Some(true)
        );
        assert!(context.output_schema.is_some());
    }

    #[test]
    fn task_invocation_runs_supported_tool_to_completion() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let server = h.server.clone();
        runtime.block_on(async move {
            let created = server
                .enqueue_tool_task(
                    "memory_search_docs",
                    json!({ "query": "task-smoke", "limit": 1 }),
                )
                .expect("task-capable tool should enqueue");
            assert_eq!(created.task.status, rmcp::model::TaskStatus::Working);
            let task_id = created.task.task_id.clone();

            let mut last_status = created.task.status;
            for _ in 0..20 {
                let info = server
                    .task_store()
                    .get(&task_id)
                    .expect("enqueued task should exist");
                last_status = info.task.status;
                if matches!(last_status, rmcp::model::TaskStatus::Completed) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert!(
                matches!(last_status, rmcp::model::TaskStatus::Completed),
                "task did not complete, last status: {last_status:?}"
            );

            let payload = server
                .task_store()
                .result(&task_id)
                .expect("completed task should expose its result");
            let payload = serde_json::to_value(payload).expect("task payload should serialize");
            let text = payload
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
                .unwrap_or_else(|| panic!("task result missing legacy text content: {payload}"));
            assert!(
                text.contains("\"hits\""),
                "search_docs task result should include structured hits text: {text}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn tool_calls_return_structured_content_with_legacy_text() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let remembered = h
                .server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "structured mcp result seed" }),
                    None,
                )
                .await
                .expect("remember succeeds");
            assert!(
                first_text(&remembered).starts_with("remembered "),
                "legacy remember text should remain"
            );
            assert!(
                remembered
                    .structured_content
                    .as_ref()
                    .and_then(|v| v.get("memory_id"))
                    .and_then(|v| v.as_str())
                    .is_some(),
                "remember should expose structured memory_id"
            );

            let themes = h
                .server
                .dispatch_tool("memory_themes", json!({}), None)
                .await
                .expect("themes succeeds");
            let text = first_text(&themes);
            let legacy: serde_json::Value =
                serde_json::from_str(&text).expect("legacy text remains a JSON array");
            assert!(legacy.is_array(), "themes text should remain array-shaped");
            assert!(
                themes
                    .structured_content
                    .as_ref()
                    .and_then(|v| v.get("themes"))
                    .and_then(|v| v.as_array())
                    .is_some(),
                "themes should expose root-object structuredContent"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn themes_returns_json_array_on_empty_db() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool("memory_themes", json!({}), None)
                .await
                .expect("themes succeeds");
            let text = first_text(&r);
            // Empty derived layer → empty array JSON. Parses cleanly.
            let v: serde_json::Value = serde_json::from_str(&text).expect("parses as json");
            assert!(v.is_array(), "expected array, got: {text}");
            assert_eq!(v.as_array().unwrap().len(), 0);
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn themes_passes_through_window_and_limit_args() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            // Should not crash with optional + integer args present.
            let r = h
                .server
                .dispatch_tool(
                    "memory_themes",
                    json!({ "window_days": 7, "limit": 20 }),
                    None,
                )
                .await
                .expect("themes with args succeeds");
            let text = first_text(&r);
            let v: serde_json::Value = serde_json::from_str(&text).expect("parses as json");
            assert!(v.is_array());
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn facts_about_rejects_empty_subject() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory_facts_about", json!({ "subject": "   " }), None)
                .await
                .expect_err("empty subject must error");
            // McpError doesn't expose a clean kind/message accessor; just
            // verify the error fires (validation path reached).
            let s = format!("{err:?}");
            assert!(
                s.to_lowercase().contains("subject") || s.to_lowercase().contains("invalid"),
                "got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn facts_about_returns_array_for_unknown_subject() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_facts_about",
                    json!({ "subject": "NobodyKnowsThisSubject" }),
                    None,
                )
                .await
                .expect("facts_about with unknown subject succeeds");
            let text = first_text(&r);
            let v: serde_json::Value = serde_json::from_str(&text).expect("parses as json");
            assert_eq!(v.as_array().unwrap().len(), 0);
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn facts_about_accepts_include_as_object_arg() {
        // Asserts the v0.5.1 P8 arg is parsed (serde default lets it
        // be omitted) and forwarded to the query lib without choking
        // the dispatcher. We don't seed triples — what we need to
        // verify is that the optional bool flows through. Both with
        // and without the arg, dispatch succeeds and returns an
        // empty array. (Functional coverage of the object-position
        // widening lives in the query-crate tests.)
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            // With include_as_object=true.
            let r = h
                .server
                .dispatch_tool(
                    "memory_facts_about",
                    json!({ "subject": "Maya", "include_as_object": true }),
                    None,
                )
                .await
                .expect("dispatch with include_as_object=true succeeds");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(v.as_array().unwrap().len(), 0);

            // Omitted entirely — must default to false (no error).
            let r = h
                .server
                .dispatch_tool("memory_facts_about", json!({ "subject": "Maya" }), None)
                .await
                .expect("dispatch without include_as_object succeeds (default false)");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(v.as_array().unwrap().len(), 0);
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn contradictions_returns_json_array_on_empty_db() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool("memory_contradictions", json!({}), None)
                .await
                .expect("contradictions succeeds");
            let text = first_text(&r);
            let v: serde_json::Value = serde_json::from_str(&text).expect("parses as json");
            assert!(v.is_array());
            assert_eq!(v.as_array().unwrap().len(), 0);
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn entities_returns_matching_graph_entities() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        {
            let conn = h.open_db();
            let (_memory_id, rowid) = seed_episode(&conn, "Alice graph seed");
            seed_triple_row(
                &conn,
                "t-mcp-entity-1",
                "Alice",
                "knows",
                "Bob",
                Some(rowid),
            );
        }
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool("memory_entities", json!({ "query": "Ali" }), None)
                .await
                .expect("entities succeeds");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert!(
                v.as_array()
                    .unwrap()
                    .iter()
                    .any(|row| row.get("entity_id").and_then(|id| id.as_str()) == Some("Alice")),
                "expected Alice entity, got {v}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn request_entity_split_records_review_op_and_revision() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let v = runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_request_entity_split",
                    json!({
                        "entity_id": "solo",
                        "affected_aliases": [" solo relay ", "solo relay", "Solo Desktop"],
                        "reason": "separate related products"
                    }),
                    None,
                )
                .await
                .expect("entity split request succeeds");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(v["op_kind"], "split");
            assert_eq!(v["status"], "needs_review");
            assert_eq!(v["source_entity_id"], "solo");
            assert_eq!(v["affected_aliases"], json!(["solo relay", "Solo Desktop"]));
            v
        });

        let op_id = v["op_id"].as_str().expect("op_id").to_string();
        let conn = h.open_db();
        let (op_count, revision_count): (i64, i64) = conn
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM entity_review_ops WHERE op_id = ?1 AND op_kind = 'split'),
                    (SELECT COUNT(*) FROM memory_revisions
                      WHERE revision_kind = 'entity_split_requested'
                        AND target_kind = 'entity'
                        AND previous_id = 'solo'
                        AND metadata_json LIKE '%' || ?1 || '%')",
                rusqlite::params![op_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query entity split side effects");
        assert_eq!(op_count, 1);
        assert_eq!(revision_count, 1);
        h.shutdown(&runtime);
    }

    #[test]
    fn graph_paths_returns_direct_and_two_hop_paths() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        {
            let conn = h.open_db();
            let (memory_id, rowid) = seed_episode(&conn, "Solo graph path seed");
            let memory_id = memory_id.to_string();
            seed_triple_row(
                &conn,
                "t-mcp-path-direct",
                "Solo",
                "uses",
                "Relay",
                Some(rowid),
            );
            seed_relationship_edge_for_triple(
                &conn,
                "edge-mcp-path-direct",
                "t-mcp-path-direct",
                "Solo",
                "uses",
                "Relay",
                rowid,
                &memory_id,
            );
            seed_triple_row(
                &conn,
                "t-mcp-path-first",
                "Solo",
                "stores",
                "Memory",
                Some(rowid),
            );
            seed_relationship_edge_for_triple(
                &conn,
                "edge-mcp-path-first",
                "t-mcp-path-first",
                "Solo",
                "stores",
                "Memory",
                rowid,
                &memory_id,
            );
            seed_triple_row(
                &conn,
                "t-mcp-path-second",
                "Memory",
                "syncs_via",
                "Relay",
                Some(rowid),
            );
            seed_relationship_edge_for_triple(
                &conn,
                "edge-mcp-path-second",
                "t-mcp-path-second",
                "Memory",
                "syncs_via",
                "Relay",
                rowid,
                &memory_id,
            );
        }
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_graph_paths",
                    json!({ "from": "ent:Solo", "to": "ent:Relay" }),
                    None,
                )
                .await
                .expect("graph paths succeeds");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(v["from"], "ent:Solo");
            assert_eq!(v["to"], "ent:Relay");
            assert_eq!(v["max_hops"], 2);
            let paths = v["paths"].as_array().expect("paths array");
            assert!(
                paths.iter().any(|path| {
                    path["hops"] == 1
                        && path["edges"]
                            .as_array()
                            .and_then(|edges| edges.first())
                            .and_then(|edge| edge.get("edge_id"))
                            .and_then(|id| id.as_str())
                            == Some("edge-mcp-path-direct")
                }),
                "expected direct path, got {v}"
            );
            assert!(
                paths.iter().any(|path| path["hops"] == 2
                    && path["nodes"] == serde_json::json!(["ent:Solo", "ent:Memory", "ent:Relay"])),
                "expected two-hop path, got {v}"
            );
            assert!(
                r.structured_content
                    .as_ref()
                    .and_then(|value| value.get("paths"))
                    .and_then(|value| value.as_array())
                    .is_some(),
                "structuredContent should expose paths"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn graph_paths_rejects_non_entity_node_ids() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_graph_paths",
                    json!({ "from": "doc:solo", "to": "ent:Relay" }),
                    None,
                )
                .await
                .expect_err("non-entity source must error");
            let s = format!("{err:?}");
            assert!(
                s.contains("memory_graph_paths") && s.contains("ent:<value>"),
                "got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn graph_paths_honors_max_hops_one() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        {
            let conn = h.open_db();
            let (memory_id, rowid) = seed_episode(&conn, "Solo graph one-hop seed");
            let memory_id = memory_id.to_string();
            seed_triple_row(
                &conn,
                "t-mcp-path-one-first",
                "Solo",
                "stores",
                "Memory",
                Some(rowid),
            );
            seed_relationship_edge_for_triple(
                &conn,
                "edge-mcp-path-one-first",
                "t-mcp-path-one-first",
                "Solo",
                "stores",
                "Memory",
                rowid,
                &memory_id,
            );
            seed_triple_row(
                &conn,
                "t-mcp-path-one-second",
                "Memory",
                "syncs_via",
                "Relay",
                Some(rowid),
            );
            seed_relationship_edge_for_triple(
                &conn,
                "edge-mcp-path-one-second",
                "t-mcp-path-one-second",
                "Memory",
                "syncs_via",
                "Relay",
                rowid,
                &memory_id,
            );
        }
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_graph_paths",
                    json!({ "from": "ent:Solo", "to": "ent:Relay", "max_hops": 1 }),
                    None,
                )
                .await
                .expect("graph paths succeeds");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(v["paths"].as_array().unwrap().len(), 0, "{v}");
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn explain_provenance_returns_relationship_edge_and_evidence() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        {
            let conn = h.open_db();
            let (memory_id, rowid) =
                seed_episode(&conn, "Solo uses Relay for secure remote memory access.");
            let memory_id = memory_id.to_string();
            seed_triple_row(
                &conn,
                "t-mcp-provenance",
                "Solo",
                "uses",
                "Relay",
                Some(rowid),
            );
            seed_relationship_edge_for_triple(
                &conn,
                "edge-mcp-provenance",
                "t-mcp-provenance",
                "Solo",
                "uses",
                "Relay",
                rowid,
                &memory_id,
            );
        }
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_explain_provenance",
                    json!({ "edge_id": "edge-mcp-provenance" }),
                    None,
                )
                .await
                .expect("provenance succeeds");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(v["edge"]["edge_id"], "edge-mcp-provenance");
            assert_eq!(v["edge"]["subject_entity_id"], "Solo");
            assert_eq!(v["edge"]["predicate"], "uses");
            assert_eq!(v["edge"]["object_entity_id"], "Relay");
            let evidence = v["evidence"].as_array().expect("evidence array");
            assert_eq!(evidence.len(), 1, "{v}");
            assert_eq!(evidence[0]["triple_id"], "t-mcp-provenance");
            assert!(
                evidence[0]["preview"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Solo uses Relay"),
                "expected active memory preview, got {v}"
            );
            assert!(
                r.structured_content
                    .as_ref()
                    .and_then(|value| value.get("edge"))
                    .and_then(|value| value.as_object())
                    .is_some(),
                "structuredContent should expose edge"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn explain_provenance_hides_forgotten_episode_preview() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        {
            let conn = h.open_db();
            let (memory_id, rowid) = seed_episode(&conn, "Forgotten provenance should not leak.");
            let memory_id = memory_id.to_string();
            seed_triple_row(
                &conn,
                "t-mcp-provenance-forgotten",
                "Solo",
                "uses",
                "Relay",
                Some(rowid),
            );
            seed_relationship_edge_for_triple(
                &conn,
                "edge-mcp-provenance-forgotten",
                "t-mcp-provenance-forgotten",
                "Solo",
                "uses",
                "Relay",
                rowid,
                &memory_id,
            );
            conn.execute(
                "UPDATE episodes SET status = 'forgotten' WHERE memory_id = ?1",
                rusqlite::params![memory_id],
            )
            .expect("forget seeded episode");
        }
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_explain_provenance",
                    json!({ "edge_id": "edge-mcp-provenance-forgotten" }),
                    None,
                )
                .await
                .expect("provenance succeeds");
            let v: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            let evidence = v["evidence"].as_array().expect("evidence array");
            assert_eq!(evidence.len(), 1, "{v}");
            assert!(
                evidence[0]["preview"].is_null(),
                "forgotten episode content must not appear in MCP provenance: {v}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn explain_provenance_rejects_unknown_edge() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_explain_provenance",
                    json!({ "edge_id": "edge-mcp-missing" }),
                    None,
                )
                .await
                .expect_err("unknown edge must error");
            let s = format!("{err:?}");
            assert!(
                s.contains("memory_explain_provenance") && s.contains("not found"),
                "got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn contradiction_resolve_updates_lifecycle() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        {
            let conn = h.open_db();
            let (_memory_id, rowid) = seed_episode(&conn, "contradiction seed");
            seed_triple_row(&conn, "t-mcp-a", "Alice", "likes", "tea", Some(rowid));
            seed_triple_row(&conn, "t-mcp-b", "Alice", "likes", "coffee", Some(rowid));
            seed_contradiction_row(&conn, "t-mcp-a", "t-mcp-b", "other");
        }
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_contradiction_resolve",
                    json!({
                        "a_id": "t-mcp-a",
                        "b_id": "t-mcp-b",
                        "kind": "other",
                        "resolution_note": "tea is current",
                        "winning_triple_id": "t-mcp-a"
                    }),
                    None,
                )
                .await
                .expect("resolve succeeds");
            let resolved: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(
                resolved.get("status").and_then(|v| v.as_str()),
                Some("resolved")
            );
            assert!(
                resolved
                    .get("resolved_at_ms")
                    .and_then(|v| v.as_i64())
                    .is_some()
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn remember_then_recall_round_trip() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        // Use &h.server directly (no clone) so the only outstanding
        // reference at shutdown time is the harness's own. The clone
        // path triggered a 5-second writer-thread timeout because the
        // local clone held an Arc<Inner> with its own WriteHandle past
        // h.shutdown().
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "the cat sat on the mat" }),
                    None,
                )
                .await
                .expect("remember succeeds");
            let text = first_text(&r);
            assert!(text.starts_with("remembered "), "got: {text}");

            let r = h
                .server
                .dispatch_tool(
                    "memory_recall",
                    json!({ "query": "the cat sat on the mat", "limit": 5 }),
                    None,
                )
                .await
                .expect("recall succeeds");
            let text = first_text(&r);
            assert!(text.contains("the cat sat on the mat"), "got: {text}");
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn update_rewrites_memory_content() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "old mcp transport memory" }),
                    None,
                )
                .await
                .expect("remember succeeds");
            let text = first_text(&r);
            let mid = text
                .strip_prefix("remembered ")
                .expect("remembered prefix")
                .to_string();

            let r = h
                .server
                .dispatch_tool(
                    "memory_update",
                    json!({
                        "memory_id": mid,
                        "content": "new mcp transport memory"
                    }),
                    None,
                )
                .await
                .expect("update succeeds");
            let updated: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("parses as json");
            assert_eq!(
                updated.get("content").and_then(|v| v.as_str()),
                Some("new mcp transport memory")
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn inbox_and_review_round_trip() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "mcp inbox review candidate" }),
                    None,
                )
                .await
                .expect("remember succeeds");
            let text = first_text(&r);
            let mid = text
                .strip_prefix("remembered ")
                .expect("remembered prefix")
                .to_string();

            let r = h
                .server
                .dispatch_tool("memory_inbox", json!({ "limit": 10 }), None)
                .await
                .expect("inbox succeeds");
            let inbox: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("inbox json");
            let items = inbox["items"].as_array().expect("items array");
            assert!(items.iter().any(|item| item["memory_id"] == mid));

            let r = h
                .server
                .dispatch_tool(
                    "memory_review",
                    json!({ "memory_id": mid, "state": "approved", "note": "checked" }),
                    None,
                )
                .await
                .expect("review succeeds");
            let reviewed: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("review json");
            assert_eq!(reviewed["state"], "approved");

            let r = h
                .server
                .dispatch_tool("memory_inbox", json!({ "limit": 10 }), None)
                .await
                .expect("inbox succeeds");
            let inbox: serde_json::Value =
                serde_json::from_str(&first_text(&r)).expect("inbox json");
            let item = inbox["items"]
                .as_array()
                .expect("items array")
                .iter()
                .find(|item| item["memory_id"] == mid)
                .expect("reviewed item");
            assert_eq!(item["review_state"], "approved");
            assert_eq!(item["review_note"], "checked");
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn memory_context_returns_json_bundle() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            h.server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "memory context round trip" }),
                    None,
                )
                .await
                .expect("remember succeeds");

            let r = h
                .server
                .dispatch_tool(
                    "memory_context",
                    json!({ "query": "memory context", "limit": 5 }),
                    None,
                )
                .await
                .expect("memory_context succeeds");
            let text = first_text(&r);
            let v: serde_json::Value = serde_json::from_str(&text).expect("parses as json");
            assert_eq!(v["query"], "memory context");
            assert!(
                v["recall"]["hits"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|h| h["content"] == "memory context round trip"),
                "context recall should include remembered content: {v}"
            );
            assert!(v["themes"].is_array());
            assert!(v["facts"].is_array());
            assert!(v["contradictions"].is_array());
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn forget_excludes_row_from_subsequent_recall() {
        let runtime = rt();
        let h = Harness::new(&runtime);

        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "to be forgotten" }),
                    None,
                )
                .await
                .unwrap();
            let text = first_text(&r);
            let mid = text.strip_prefix("remembered ").unwrap().to_string();

            h.server
                .dispatch_tool(
                    "memory_forget",
                    json!({ "memory_id": mid, "reason": "test" }),
                    None,
                )
                .await
                .expect("forget succeeds");

            let r = h
                .server
                .dispatch_tool(
                    "memory_recall",
                    json!({ "query": "to be forgotten", "limit": 5 }),
                    None,
                )
                .await
                .unwrap();
            let text = first_text(&r);
            assert!(
                !text.contains(r#""content": "to be forgotten""#),
                "forgotten row should be excluded; got: {text}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn empty_remember_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory_remember", json!({ "content": "" }), None)
                .await
                .unwrap_err();
            assert!(format!("{err:?}").contains("must not be empty"));
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn empty_recall_query_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory_recall", json!({ "query": "   " }), None)
                .await
                .unwrap_err();
            assert!(format!("{err:?}").contains("must not be empty"));
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn inspect_with_invalid_id_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory_inspect", json!({ "memory_id": "not-a-uuid" }), None)
                .await
                .unwrap_err();
            assert!(format!("{err:?}").contains("invalid memory_id"));
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn forget_unknown_id_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            // Valid UUID format but not in episodes — handle_forget
            // surfaces NotFound, mapped to invalid_params per
            // solo_to_mcp.
            let err = h
                .server
                .dispatch_tool(
                    "memory_forget",
                    json!({ "memory_id": "00000000-0000-7000-8000-000000000000" }),
                    None,
                )
                .await
                .unwrap_err();
            assert!(format!("{err:?}").contains("not found"));
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn unknown_tool_name_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory.summon", json!({}), None)
                .await
                .unwrap_err();
            assert!(format!("{err:?}").contains("unknown tool"));
        });
        h.shutdown(&runtime);
    }

    /// Regression guard for v0.4.1's MCP tool name fix, generalised
    /// in v0.5.0 Priority 4 to cover **all three** major LLM
    /// providers, not just Anthropic.
    ///
    /// Each provider enforces its own tool-name regex on the
    /// function-calling wire. A tool name has to satisfy ALL of them
    /// to be portable across clients:
    ///
    ///   - **Anthropic**: `^[a-zA-Z0-9_-]{1,64}$` (what shipped in
    ///     v0.4.1; failing this rejects the entire toolset on Claude
    ///     Desktop / Cursor / Claude Code with
    ///     `FrontendRemoteMcpToolDefinition.name: String should
    ///     match pattern ...`).
    ///   - **OpenAI** function-calling: `^[a-zA-Z_][a-zA-Z0-9_-]*$`
    ///     with length ≤ 64 (must start with letter or underscore).
    ///   - **Gemini** function-calling: documented as a-z, A-Z, 0-9,
    ///     underscores and dashes; some sources also allow dots. We
    ///     use the conservative intersection — must start with
    ///     letter or underscore, alphanumeric + underscore only (no
    ///     hyphen, no dot), length ≤ 63. This is the strictest of
    ///     the three patterns, so any tool that passes it also
    ///     passes the other two. Sources differ on whether Gemini
    ///     accepts dots or hyphens; the strictest reading guards us
    ///     against the future where one provider tightens the regex
    ///     (which is the failure mode v0.4.1 hit on Anthropic). See
    ///     <https://github.com/google-gemini/deprecated-generative-ai-python/blob/main/docs/api/google/generativeai/protos/FunctionDeclaration.md>
    ///     and <https://ai.google.dev/gemini-api/docs/function-calling>.
    ///
    /// Lesson banked v0.3 #8: rmcp framing tests pass dot-named
    /// tools fine because rmcp's own client-side validation is
    /// permissive. Only the downstream provider API enforces the
    /// regex. This test gates the names at `cargo test` time so any
    /// future tool-name change has to pass all three provider
    /// regexes before reaching real clients.
    #[test]
    fn tool_names_match_cross_provider_regex() {
        /// Anthropic API name regex: `^[a-zA-Z0-9_-]{1,64}$`.
        fn passes_anthropic(name: &str) -> bool {
            let len = name.len();
            if !(1..=64).contains(&len) {
                return false;
            }
            name.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }

        /// OpenAI function-calling name regex:
        /// `^[a-zA-Z_][a-zA-Z0-9_-]*$`, length ≤ 64.
        fn passes_openai(name: &str) -> bool {
            let len = name.len();
            if !(1..=64).contains(&len) {
                return false;
            }
            let mut chars = name.chars();
            let first = match chars.next() {
                Some(c) => c,
                None => return false,
            };
            if !(first.is_ascii_alphabetic() || first == '_') {
                return false;
            }
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        }

        /// Gemini function-calling name regex (conservative
        /// reading): `^[a-zA-Z_][a-zA-Z0-9_]*$`, length ≤ 63. No
        /// hyphen, no dot — strictest of the three so any name that
        /// passes this passes the other two.
        fn passes_gemini(name: &str) -> bool {
            let len = name.len();
            if !(1..=63).contains(&len) {
                return false;
            }
            let mut chars = name.chars();
            let first = match chars.next() {
                Some(c) => c,
                None => return false,
            };
            if !(first.is_ascii_alphabetic() || first == '_') {
                return false;
            }
            chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        }

        let tools = build_tools();
        assert_eq!(
            tools.len(),
            39,
            "expected 39 tools (context + update/inbox/entities/entity split/graph paths/provenance/resolve + v0.5.x + document/upload/import tools + remember_batch + attachment writes + asset lifecycle/download)"
        );
        // Sanity-check that tool_names() agrees with build_tools().
        let tool_name_strings: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        let public_names: Vec<String> = super::tool_names().iter().map(|s| s.to_string()).collect();
        assert_eq!(
            tool_name_strings, public_names,
            "tool_names() drifted from build_tools() — keep them in sync"
        );

        for t in tools {
            assert!(
                passes_anthropic(&t.name),
                "tool name {:?} fails Anthropic regex \
                 ^[a-zA-Z0-9_-]{{1,64}}$ — see v0.3 lesson #8",
                t.name
            );
            assert!(
                passes_openai(&t.name),
                "tool name {:?} fails OpenAI function-calling regex \
                 ^[a-zA-Z_][a-zA-Z0-9_-]*$ (len ≤ 64)",
                t.name
            );
            assert!(
                passes_gemini(&t.name),
                "tool name {:?} fails Gemini function-calling regex \
                 ^[a-zA-Z_][a-zA-Z0-9_]*$ (len ≤ 63, strict)",
                t.name
            );
        }
    }

    /// Regression guard (dev-log 0152 finding M3): the
    /// `memory_remember_batch` JSON Schema's `items.maxItems` must equal
    /// the runtime cap `solo_storage::MAX_REMEMBER_BATCH_SIZE`. The
    /// schema is now derived from the constant, but pin the literal so a
    /// future drift (someone hard-codes `200` again) is caught.
    #[test]
    fn remember_batch_maxitems_matches_max_batch_size() {
        let tools = build_tools();
        let batch = tools
            .iter()
            .find(|t| t.name == "memory_remember_batch")
            .expect("memory_remember_batch tool is missing");
        let schema =
            serde_json::to_value(&batch.input_schema).expect("input_schema serialises as JSON");
        let max_items = schema
            .get("properties")
            .and_then(|p| p.get("items"))
            .and_then(|i| i.get("maxItems"))
            .and_then(|n| n.as_u64())
            .expect("memory_remember_batch.items.maxItems missing or not a u64");
        assert_eq!(
            max_items as usize,
            solo_storage::MAX_REMEMBER_BATCH_SIZE,
            "memory_remember_batch schema maxItems ({}) must equal \
             solo_storage::MAX_REMEMBER_BATCH_SIZE ({}). If the cap \
             changed, update both — but you should never need to: the \
             schema now interpolates the constant directly.",
            max_items,
            solo_storage::MAX_REMEMBER_BATCH_SIZE,
        );
    }

    /// Regression guard for the v0.5.0 Priority 4 jargon pass.
    ///
    /// Tool descriptions and `get_info().instructions` are the first
    /// (and often only) thing a calling LLM reads when its
    /// tool-search mechanism decides whether Solo's tools are
    /// relevant. Earlier descriptions leaned on Solo-internal
    /// vocabulary (`SPO`, `Steward`, `LEFT JOIN`, `candidate pair`,
    /// `tagged_with`) which doesn't pattern-match natural-language
    /// agent queries like "what do you know about Alex?" — that's
    /// the load-bearing v0.5.0 finding from the 2026-05-14
    /// thesis-test in Claude Desktop.
    ///
    /// This test pins the de-jargoning by forbidding the old
    /// vocabulary from appearing in any user-facing text. Future
    /// contributors who reach for jargon trip the test and have to
    /// pick plain-English phrasing instead.
    #[test]
    fn tool_descriptions_avoid_internal_jargon() {
        // Case-insensitive substring match. Drawn from the
        // pre-Priority-4 descriptions; expand only if a new term
        // creeps in.
        const FORBIDDEN: &[&str] = &[
            "SPO",
            "Steward",
            "Steward-flagged",
            "LEFT JOIN",
            "candidate pair",
            "candidate_pair",
            "tagged_with",
        ];

        fn contains_case_insensitive(haystack: &str, needle: &str) -> bool {
            haystack.to_lowercase().contains(&needle.to_lowercase())
        }

        // 1. Each tool description.
        for t in build_tools() {
            let desc = t.description.as_deref().unwrap_or("");
            for term in FORBIDDEN {
                assert!(
                    !contains_case_insensitive(desc, term),
                    "tool {:?} description contains forbidden jargon \
                     {:?} — rewrite in plain English (see v0.5.0 \
                     Priority 4)",
                    t.name,
                    term,
                );
            }
        }

        // 2. The server-level instructions (what tool-search sees
        // first).
        let server_info = harness_server_info();
        let instructions = server_info
            .instructions
            .as_deref()
            .expect("get_info() must set instructions");
        for term in FORBIDDEN {
            assert!(
                !contains_case_insensitive(instructions, term),
                "get_info().instructions contains forbidden jargon \
                 {term:?} — rewrite in plain English",
            );
        }
    }

    /// Build a `ServerInfo` for the jargon test without spinning up
    /// the full harness (which needs tokio + tempdir). The
    /// `ServerHandler::get_info()` method doesn't take `&self` state
    /// in any meaningful way for our impl — it returns a static
    /// `ServerInfo` literal — so we construct a minimal-input server
    /// just to call it.
    fn harness_server_info() -> rmcp::model::ServerInfo {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let info = ServerHandler::get_info(&h.server);
        h.shutdown(&runtime);
        info
    }

    /// Regression guard for the v0.9.0 → v0.9.1 P1 Fix 1 MCP
    /// `serverInfo` identity regression.
    ///
    /// In v0.9.0, P0a's rmcp 0.1.5 → 1.7 bump replaced the explicit
    /// `Implementation::new("solo", "<version>")` constructor with
    /// `Implementation::from_build_env()`. That helper reads
    /// `CARGO_PKG_NAME` + `CARGO_PKG_VERSION` from **rmcp's own** build
    /// environment (the proc-macro expansion captures rmcp's
    /// `Cargo.toml`, not the consumer's). Every Solo MCP daemon on
    /// v0.9.0 self-identified as `{name: "rmcp", version: "1.7.0"}`
    /// instead of `{name: "solo", version: "<workspace.version>"}`.
    ///
    /// Pins:
    ///   - `name == "solo"` (the operator-facing binary name, not
    ///     `"solo-api"` which would come from
    ///     `env!("CARGO_PKG_NAME")` against this crate's manifest);
    ///   - `version == solo_core::build_info::version_with_build_metadata()`,
    ///     so MCP reports the same release+build identity as `solo --version`.
    #[test]
    fn server_info_identity_is_solo_not_rmcp_or_solo_api() {
        let info = harness_server_info();
        let name = info.server_info.name.as_str();
        let version = info.server_info.version.as_str();
        assert_eq!(
            name, "solo",
            "MCP serverInfo.name must be \"solo\" (not \"rmcp\" or \
             \"solo-api\"). got name={name:?} version={version:?}"
        );
        assert_eq!(
            version,
            solo_core::build_info::version_with_build_metadata(),
            "MCP serverInfo.version must match Solo's release+build identity; \
             a mismatch means we regressed back to rmcp's build env or a raw \
             CARGO_PKG_VERSION-only string. \
             got version={version:?}"
        );
    }

    #[test]
    fn server_info_includes_memory_guidance_instructions() {
        let info = harness_server_info();
        let instructions = info
            .instructions
            .as_deref()
            .expect("MCP initialize result should include server instructions");
        assert!(instructions.contains("persistent memory"));
        assert!(instructions.contains("memory_context"));
        assert!(instructions.contains("Do not store secrets"));
    }

    // ---- memory_inspect_cluster (v0.5.0 Priority 3) ----

    #[test]
    fn inspect_cluster_unknown_id_returns_invalid_params() {
        // NotFound from solo_query::inspect_cluster is mapped through
        // `solo_to_mcp` to `invalid_params` (MCP has no separate
        // not-found error shape). Error message should name the id.
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_inspect_cluster",
                    json!({ "cluster_id": "no-such-cluster" }),
                    None,
                )
                .await
                .expect_err("unknown cluster must error");
            let s = format!("{err:?}");
            assert!(
                s.contains("no-such-cluster") || s.to_lowercase().contains("not found"),
                "expected error to mention the missing cluster id; got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn inspect_cluster_rejects_empty_id() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_inspect_cluster",
                    json!({ "cluster_id": "   " }),
                    None,
                )
                .await
                .expect_err("blank cluster_id must error");
            let s = format!("{err:?}");
            assert!(
                s.to_lowercase().contains("cluster_id")
                    || s.to_lowercase().contains("must not be empty"),
                "got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    // ---- Document tools (v0.7.0 P5) ----
    //
    // The document handlers have arg-shape and dispatch coverage:
    //   - arg-struct parses from JSON (serde round-trip; defaults work).
    //   - dispatch arm routes to the handler (we observe behaviour via
    //     a known empty-DB response — bad routing surfaces as
    //     "unknown tool" or wrong shape).
    //
    // Functional coverage (ingest → search → inspect → forget) lives in
    // `crates/solo-cli/tests/mcp_smoke.rs` where a real subprocess + real
    // writer-with-embedder is wired up. The in-process Harness here uses
    // `WriterActor::spawn` which doesn't carry an embedder, so ingest /
    // search themselves return an error — but the dispatch + arg-parse
    // paths exercise correctly.

    #[test]
    fn ingest_document_args_parse_with_required_path() {
        let v: IngestDocumentArgs =
            serde_json::from_value(json!({ "path": "/tmp/notes.md" })).expect("parses");
        assert_eq!(v.path, "/tmp/notes.md");
        // path is required — missing must reject at deserialization.
        let err = serde_json::from_value::<IngestDocumentArgs>(json!({})).unwrap_err();
        assert!(format!("{err}").contains("path"));
    }

    #[test]
    fn search_docs_args_parse_with_default_limit() {
        let v: SearchDocsArgs =
            serde_json::from_value(json!({ "query": "backups" })).expect("parses");
        assert_eq!(v.query, "backups");
        assert_eq!(v.limit, 5, "default limit must be 5");
        let v: SearchDocsArgs =
            serde_json::from_value(json!({ "query": "backups", "limit": 20 })).expect("parses");
        assert_eq!(v.limit, 20);
    }

    #[test]
    fn inspect_document_args_parse_with_required_doc_id() {
        let v: InspectDocumentArgs =
            serde_json::from_value(json!({ "doc_id": "abc" })).expect("parses");
        assert_eq!(v.doc_id, "abc");
        let err = serde_json::from_value::<InspectDocumentArgs>(json!({})).unwrap_err();
        assert!(format!("{err}").contains("doc_id"));
    }

    #[test]
    fn list_documents_args_parse_with_all_defaults() {
        let v: ListDocumentsArgs = serde_json::from_value(json!({})).expect("parses");
        assert_eq!(v.limit, 20, "default limit must be 20");
        assert_eq!(v.offset, 0, "default offset must be 0");
        assert!(
            !v.include_forgotten,
            "default include_forgotten must be false"
        );
        let v: ListDocumentsArgs =
            serde_json::from_value(json!({ "limit": 5, "offset": 10, "include_forgotten": true }))
                .expect("parses");
        assert_eq!(v.limit, 5);
        assert_eq!(v.offset, 10);
        assert!(v.include_forgotten);
    }

    #[test]
    fn forget_document_args_parse_with_required_doc_id() {
        let v: ForgetDocumentArgs =
            serde_json::from_value(json!({ "doc_id": "abc" })).expect("parses");
        assert_eq!(v.doc_id, "abc");
        let err = serde_json::from_value::<ForgetDocumentArgs>(json!({})).unwrap_err();
        assert!(format!("{err}").contains("doc_id"));
    }

    #[test]
    fn document_upload_prepare_status_abort_round_trip() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let prepared = h
                .server
                .dispatch_tool(
                    "document_upload_prepare",
                    json!({
                        "filename": "mcp-upload.md",
                        "mime_type": "text/markdown",
                        "size_bytes": 3
                    }),
                    None,
                )
                .await
                .expect("prepare upload");
            let prepared: serde_json::Value =
                serde_json::from_str(&first_text(&prepared)).expect("prepare json");
            let upload_id = prepared
                .get("upload_id")
                .and_then(|v| v.as_str())
                .expect("upload_id");
            assert_eq!(
                prepared
                    .get("upload_url")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                Some(format!("/uploads/{upload_id}"))
            );
            assert_eq!(
                prepared
                    .get("upload_path")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                Some(format!("/uploads/{upload_id}"))
            );
            assert_eq!(
                prepared.get("route_kind").and_then(|v| v.as_str()),
                Some("direct_local")
            );
            assert_eq!(
                prepared.get("max_file_bytes").and_then(|v| v.as_u64()),
                Some(crate::document_upload::MAX_UPLOAD_BYTES)
            );
            assert_eq!(
                prepared.get("max_chunk_bytes").and_then(|v| v.as_u64()),
                Some(crate::document_upload::RECOMMENDED_CHUNK_BYTES as u64)
            );
            assert_eq!(
                prepared.get("upload_method").and_then(|v| v.as_str()),
                Some("PATCH")
            );
            assert_eq!(
                prepared
                    .get("upload_offset_header")
                    .and_then(|v| v.as_str()),
                Some(crate::document_upload::UPLOAD_OFFSET_HEADER)
            );
            assert_eq!(
                prepared
                    .pointer("/upload_headers/content-type")
                    .and_then(|v| v.as_str()),
                Some("application/octet-stream")
            );
            assert_eq!(
                prepared
                    .pointer("/upload_headers/upload-offset")
                    .and_then(|v| v.as_str()),
                Some("0")
            );
            assert_eq!(
                prepared
                    .pointer("/required_headers/upload-offset")
                    .and_then(|v| v.as_str()),
                Some("0")
            );
            assert_eq!(
                prepared
                    .pointer("/mcp_fallback/tool")
                    .and_then(|v| v.as_str()),
                Some("document_upload_chunk_base64")
            );
            assert_eq!(
                prepared
                    .pointer("/mcp_fallback/max_chunk_bytes")
                    .and_then(|v| v.as_u64()),
                Some(crate::document_upload::MCP_BASE64_CHUNK_BYTES as u64)
            );
            assert_eq!(
                prepared.get("commit_tool").and_then(|v| v.as_str()),
                Some("document_upload_commit")
            );
            assert_eq!(
                prepared.get("ingest_tool").and_then(|v| v.as_str()),
                Some("memory_ingest_staged_document")
            );
            assert_eq!(
                prepared
                    .get("default_store_original_file")
                    .and_then(|v| v.as_bool()),
                Some(true)
            );
            assert!(
                prepared
                    .get("next_actions")
                    .and_then(|v| v.as_array())
                    .is_some_and(|actions| actions
                        .iter()
                        .any(|action| action.get("tool").and_then(|v| v.as_str())
                            == Some("document_upload_chunk_base64"))),
                "prepare response should expose machine-readable fallback action"
            );
            assert!(
                prepared
                    .get("next_steps")
                    .and_then(|v| v.as_array())
                    .is_some_and(|steps| steps.iter().any(|step| step
                        .as_str()
                        .is_some_and(|text| text.contains("document_upload_commit")))),
                "prepare response should tell agents how to finish the workflow"
            );

            let status = h
                .server
                .dispatch_tool(
                    "document_upload_status",
                    json!({ "upload_id": upload_id }),
                    None,
                )
                .await
                .expect("upload status");
            let status: serde_json::Value =
                serde_json::from_str(&first_text(&status)).expect("status json");
            assert_eq!(status.get("status").and_then(|v| v.as_str()), Some("open"));
            assert_eq!(
                status.get("bytes_received").and_then(|v| v.as_u64()),
                Some(0)
            );

            let uploaded = h
                .server
                .dispatch_tool(
                    "document_upload_chunk_base64",
                    json!({
                        "upload_id": upload_id,
                        "offset": 0,
                        "upload_length": 3,
                        "chunk_base64": "YWJj"
                    }),
                    None,
                )
                .await
                .expect("base64 upload chunk");
            let uploaded: serde_json::Value =
                serde_json::from_str(&first_text(&uploaded)).expect("upload json");
            assert_eq!(
                uploaded.get("bytes_received").and_then(|v| v.as_u64()),
                Some(3)
            );
            assert_eq!(
                uploaded.get("next_offset").and_then(|v| v.as_u64()),
                Some(3)
            );

            let aborted = h
                .server
                .dispatch_tool(
                    "document_upload_abort",
                    json!({ "upload_id": upload_id }),
                    None,
                )
                .await
                .expect("abort upload");
            let aborted: serde_json::Value =
                serde_json::from_str(&first_text(&aborted)).expect("abort json");
            assert_eq!(
                aborted.get("status").and_then(|v| v.as_str()),
                Some("aborted")
            );
            assert_eq!(
                aborted.get("cleanup_performed").and_then(|v| v.as_bool()),
                Some(true)
            );
            assert_eq!(
                aborted.get("already_aborted").and_then(|v| v.as_bool()),
                Some(false)
            );

            let status = h
                .server
                .dispatch_tool(
                    "document_upload_status",
                    json!({ "upload_id": upload_id }),
                    None,
                )
                .await
                .expect("aborted upload retains terminal status");
            let status: serde_json::Value =
                serde_json::from_str(&first_text(&status)).expect("aborted status json");
            assert_eq!(
                status.get("status").and_then(|v| v.as_str()),
                Some("aborted")
            );
            assert_eq!(status.get("terminal").and_then(|v| v.as_bool()), Some(true));
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn document_upload_chunk_base64_rejects_oversized_decoded_chunk() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let prepared = h
                .server
                .dispatch_tool(
                    "document_upload_prepare",
                    json!({
                        "filename": "mcp-upload.md",
                        "mime_type": "text/markdown",
                        "size_bytes": crate::document_upload::MCP_BASE64_CHUNK_BYTES
                    }),
                    None,
                )
                .await
                .expect("prepare upload");
            let prepared: serde_json::Value =
                serde_json::from_str(&first_text(&prepared)).expect("prepare json");
            let upload_id = prepared
                .get("upload_id")
                .and_then(|v| v.as_str())
                .expect("upload_id");
            let too_large = vec![b'x'; crate::document_upload::MCP_BASE64_CHUNK_BYTES + 1];
            let chunk_base64 = base64::engine::general_purpose::STANDARD.encode(too_large);
            let err = h
                .server
                .dispatch_tool(
                    "document_upload_chunk_base64",
                    json!({
                        "upload_id": upload_id,
                        "offset": 0,
                        "chunk_base64": chunk_base64
                    }),
                    None,
                )
                .await
                .expect_err("oversized decoded fallback chunk should fail");
            assert!(
                format!("{err:?}").contains("decoded base64 chunk"),
                "expected decoded chunk limit error, got {err:?}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn document_upload_chunk_base64_rejects_large_prepared_upload() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let prepared = h
                .server
                .dispatch_tool(
                    "document_upload_prepare",
                    json!({
                        "filename": "mcp-upload.md",
                        "mime_type": "text/markdown",
                        "size_bytes": (crate::document_upload::MCP_BASE64_CHUNK_BYTES + 1)
                    }),
                    None,
                )
                .await
                .expect("prepare upload");
            let prepared: serde_json::Value =
                serde_json::from_str(&first_text(&prepared)).expect("prepare json");
            let upload_id = prepared
                .get("upload_id")
                .and_then(|v| v.as_str())
                .expect("upload_id");
            let err = h
                .server
                .dispatch_tool(
                    "document_upload_chunk_base64",
                    json!({
                        "upload_id": upload_id,
                        "offset": 0,
                        "chunk_base64": "YQ=="
                    }),
                    None,
                )
                .await
                .expect_err("fallback should reject large prepared uploads");
            assert!(
                format!("{err:?}").contains("MCP base64 fallback only supports uploads"),
                "expected fallback file limit error, got {err:?}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn ingest_document_rejects_empty_path() {
        // Reaches the dispatch arm → handle_ingest_document → empty
        // guard fires before the writer is touched. Proves routing.
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory_ingest_document", json!({ "path": "" }), None)
                .await
                .expect_err("empty path must error");
            let s = format!("{err:?}");
            assert!(
                s.to_lowercase().contains("path") || s.to_lowercase().contains("must not be empty"),
                "got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn ingest_document_rejects_path_outside_workspace_file_access_roots() {
        let runtime = rt();
        let allowed = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let outside_file = outside.path().join("outside.md");
        std::fs::write(&outside_file, "outside").unwrap();
        let policy = crate::WorkspaceFileAccessPolicy::restricted_to_roots(vec![
            allowed.path().to_path_buf(),
        ])
        .expect("policy");
        let h = Harness::new_with_workspace_file_access(&runtime, policy);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_ingest_document",
                    json!({ "path": outside_file.display().to_string() }),
                    None,
                )
                .await
                .expect_err("outside path must error");
            let s = format!("{err:?}");
            assert!(
                s.contains("workspace_file_access.allowed_roots"),
                "got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn import_file_scan_honors_cancellation() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("note.md"), "# note").unwrap();
        let flag = StdArc::new(std::sync::atomic::AtomicBool::new(true));
        let cancellation = crate::mcp_task::CancellationToken::from_task_flag(flag);

        let err = collect_import_files(
            tmp.path(),
            true,
            &["md".to_string()],
            DEFAULT_IMPORT_MAX_FILES,
            &cancellation,
        )
        .expect_err("cancelled scan must stop before returning files");
        assert!(
            err.message.contains("cancelled"),
            "expected cancellation error, got {err:?}"
        );
    }

    #[test]
    fn import_documents_stores_original_assets_and_resource_links() {
        let runtime = rt();
        let h = Harness::new_with_embedder(&runtime);
        let dir = h._tmp.path().join("mcp-retained-import");
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("source.md");
        let bytes = b"# MCP Source\n\nretain this original";
        std::fs::write(&file_path, bytes).unwrap();
        let asset_root = h._tmp.path().to_path_buf();

        runtime.block_on(async {
            let result = h
                .server
                .dispatch_tool(
                    "memory_import_documents",
                    json!({
                        "path": dir.display().to_string(),
                        "store_original_file": true,
                        "max_files": 10
                    }),
                    None,
                )
                .await
                .expect("import should retain source asset");
            let json: serde_json::Value =
                serde_json::from_str(&first_text(&result)).expect("import response json");
            assert_eq!(
                json.get("store_original_file").and_then(|v| v.as_bool()),
                Some(true)
            );
            assert_eq!(
                json.get("assets_retained").and_then(|v| v.as_u64()),
                Some(1),
                "body: {json}"
            );
            assert_eq!(json.get("asset_links").and_then(|v| v.as_u64()), Some(1));
            assert_eq!(json.get("asset_failed").and_then(|v| v.as_u64()), Some(0));
            let doc_id = json
                .pointer("/results/0/doc_id")
                .and_then(|v| v.as_str())
                .expect("doc_id");
            let asset_id = json
                .pointer("/results/0/asset/asset_id")
                .and_then(|v| v.as_str())
                .expect("asset_id");
            assert_eq!(
                json.pointer("/results/0/asset/filename")
                    .and_then(|v| v.as_str()),
                Some("source.md")
            );
            assert_eq!(
                json.pointer("/results/0/asset/mime_type")
                    .and_then(|v| v.as_str()),
                Some("text/markdown")
            );
            assert_eq!(
                json.pointer("/results/0/asset/size_bytes")
                    .and_then(|v| v.as_u64()),
                Some(bytes.len() as u64)
            );
            assert_eq!(
                json.pointer("/results/0/document_asset_link/relation_type")
                    .and_then(|v| v.as_str()),
                Some("source_import")
            );
            let storage_path = json
                .pointer("/results/0/asset/storage_path")
                .and_then(|v| v.as_str())
                .expect("storage_path");
            assert!(
                asset_root.join(storage_path).is_file(),
                "asset blob should exist in tenant asset store"
            );

            let wire = serde_json::to_string(&result).expect("serialize call result");
            assert!(
                wire.contains(&format!("solo://document/{doc_id}")),
                "document resource link missing from import result: {wire}"
            );
            assert!(
                wire.contains(&format!("solo://asset/{asset_id}")),
                "asset resource link missing from import result: {wire}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn import_documents_dispatch_honors_pre_cancelled_token() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let dir = h._tmp.path().join("mcp-cancelled-import");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("source.md"), "# Cancelled").unwrap();
        let flag = StdArc::new(std::sync::atomic::AtomicBool::new(true));
        let cancellation = crate::mcp_task::CancellationToken::from_task_flag(flag);

        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool_with_cancellation(
                    "memory_import_documents",
                    json!({ "path": dir.display().to_string(), "max_files": 10 }),
                    None,
                    cancellation,
                )
                .await
                .expect_err("pre-cancelled import must stop");
            assert!(
                err.message.contains("cancelled"),
                "expected cancellation error, got {err:?}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn task_invocation_runs_import_tool_to_completion() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let dir = h._tmp.path().join("mcp-task-import");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("source.md"), "# Task Import").unwrap();
        let server = h.server.clone();

        runtime.block_on(async move {
            let created = server
                .enqueue_tool_task(
                    "memory_import_documents",
                    json!({
                        "path": dir.display().to_string(),
                        "dry_run": true,
                        "max_files": 10
                    }),
                )
                .expect("import tool should enqueue as a task");
            assert_eq!(created.task.status, rmcp::model::TaskStatus::Working);
            let task_id = created.task.task_id.clone();

            let mut last_status = created.task.status;
            for _ in 0..20 {
                let info = server
                    .task_store()
                    .get(&task_id)
                    .expect("enqueued task should exist");
                last_status = info.task.status;
                if matches!(last_status, rmcp::model::TaskStatus::Completed) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert!(
                matches!(last_status, rmcp::model::TaskStatus::Completed),
                "task did not complete, last status: {last_status:?}"
            );

            let payload = server
                .task_store()
                .result(&task_id)
                .expect("completed import task should expose its result");
            let payload = serde_json::to_value(payload).expect("task payload should serialize");
            let text = payload
                .pointer("/content/0/text")
                .and_then(|value| value.as_str())
                .unwrap_or_else(|| panic!("task result missing legacy text content: {payload}"));
            assert!(
                text.contains("\"total_files\": 1"),
                "import task result should include dry-run import report: {text}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn search_docs_rejects_empty_query() {
        // Empty query trips solo_query::run_doc_search's validation
        // → InvalidInput → invalid_params.
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory_search_docs", json!({ "query": "   " }), None)
                .await
                .expect_err("empty query must error");
            let s = format!("{err:?}");
            assert!(
                s.to_lowercase().contains("must not be empty")
                    || s.to_lowercase().contains("invalid"),
                "got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn inspect_document_unknown_id_returns_invalid_params() {
        // Valid UUID format but no row exists → handler returns
        // invalid_params with the missing id in the message.
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_inspect_document",
                    json!({ "doc_id": "00000000-0000-7000-8000-000000000000" }),
                    None,
                )
                .await
                .expect_err("unknown doc must error");
            let s = format!("{err:?}");
            assert!(
                s.to_lowercase().contains("not found"),
                "expected 'not found' message; got: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn inspect_document_rejects_malformed_id() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_inspect_document",
                    json!({ "doc_id": "not-a-uuid" }),
                    None,
                )
                .await
                .expect_err("malformed doc_id must error");
            let s = format!("{err:?}");
            assert!(s.contains("invalid doc_id"), "got: {s}");
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn list_documents_returns_empty_array_on_empty_db() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool("memory_list_documents", json!({}), None)
                .await
                .expect("list succeeds");
            let text = first_text(&r);
            let v: serde_json::Value = serde_json::from_str(&text).expect("parses as json");
            assert!(v.is_array(), "expected array, got: {text}");
            assert_eq!(v.as_array().unwrap().len(), 0);
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn list_documents_passes_through_limit_offset_include_args() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_list_documents",
                    json!({ "limit": 5, "offset": 10, "include_forgotten": true }),
                    None,
                )
                .await
                .expect("list with args succeeds");
            let text = first_text(&r);
            let v: serde_json::Value = serde_json::from_str(&text).expect("parses as json");
            assert!(v.is_array());
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn attachment_write_tools_create_links_and_resource_links() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let asset_id = AssetId::new();
        let doc_id = DocumentId::new();
        let memory_id = MemoryId::new();
        {
            let conn = h.open_db();
            conn.execute(
                "INSERT INTO assets (
                    asset_id, sha256, mime_type, filename, size_bytes,
                    storage_path, source, status, created_by_principal,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, 'text/markdown', 'manual-source.md', 13,
                    ?3, 'manual-test', 'active', 'tester', 1_000, 1_000)",
                rusqlite::params![
                    asset_id.to_string(),
                    "d".repeat(64),
                    format!("assets/blobs/dd/{}", "d".repeat(64)),
                ],
            )
            .expect("seed asset");
            conn.execute(
                "INSERT INTO documents (
                    doc_id, source, title, mime_type, ingested_at_ms, status, chunk_count
                 ) VALUES (?1, '/tmp/manual-source.md', 'Manual Source', 'text/markdown', 1_001, 'active', 1)",
                rusqlite::params![doc_id.to_string()],
            )
            .expect("seed document");
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, content,
                    encoding_context_json, confidence, strength, salience,
                    tier, status, created_at_ms, updated_at_ms
                 ) VALUES (?1, 1_002, 'user_message', 'memory with manual source',
                    '{}', 0.9, 0.5, 0.5, 'hot', 'active', 1_002, 1_002)",
                rusqlite::params![memory_id.to_string()],
            )
            .expect("seed memory");
        }

        runtime.block_on(async {
            let linked = h
                .server
                .dispatch_tool(
                    "memory_link_document_asset",
                    json!({
                        "doc_id": doc_id.to_string(),
                        "asset_id": asset_id.to_string(),
                        "note": "manual provenance repair"
                    }),
                    None,
                )
                .await
                .expect("link document asset");
            let linked_json: serde_json::Value =
                serde_json::from_str(&first_text(&linked)).expect("link json");
            let link_id = linked_json
                .pointer("/link_id")
                .and_then(|v| v.as_str())
                .expect("link id")
                .to_string();
            assert_eq!(
                linked_json
                    .pointer("/relation_type")
                    .and_then(|v| v.as_str()),
                Some("source_upload")
            );
            let linked_wire = serde_json::to_string(&linked).expect("linked result json");
            assert!(linked_wire.contains(&format!("solo://document/{doc_id}")));
            assert!(linked_wire.contains(&format!("solo://asset/{asset_id}")));

            let duplicate = h
                .server
                .dispatch_tool(
                    "memory_link_document_asset",
                    json!({
                        "doc_id": doc_id.to_string(),
                        "asset_id": asset_id.to_string()
                    }),
                    None,
                )
                .await
                .expect("duplicate link document asset");
            let duplicate_json: serde_json::Value =
                serde_json::from_str(&first_text(&duplicate)).expect("duplicate link json");
            assert_eq!(
                duplicate_json.pointer("/link_id").and_then(|v| v.as_str()),
                Some(link_id.as_str()),
                "duplicate link should return the existing row"
            );

            let attached_asset = h
                .server
                .dispatch_tool(
                    "memory_attach",
                    json!({
                        "memory_id": memory_id.to_string(),
                        "asset_id": asset_id.to_string(),
                        "relation_type": "source_file",
                        "note": "supporting file"
                    }),
                    None,
                )
                .await
                .expect("attach memory to asset");
            let attached_asset_json: serde_json::Value =
                serde_json::from_str(&first_text(&attached_asset)).expect("asset attach json");
            assert_eq!(
                attached_asset_json
                    .pointer("/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );
            let attached_asset_wire =
                serde_json::to_string(&attached_asset).expect("attached asset result json");
            assert!(attached_asset_wire.contains(&format!("solo://memory/{memory_id}")));
            assert!(attached_asset_wire.contains(&format!("solo://asset/{asset_id}")));

            let attached_doc = h
                .server
                .dispatch_tool(
                    "memory_attach",
                    json!({
                        "memory_id": memory_id.to_string(),
                        "doc_id": doc_id.to_string(),
                        "relation_type": "evidence"
                    }),
                    None,
                )
                .await
                .expect("attach memory to document");
            let attached_doc_json: serde_json::Value =
                serde_json::from_str(&first_text(&attached_doc)).expect("doc attach json");
            assert_eq!(
                attached_doc_json
                    .pointer("/doc_id")
                    .and_then(|v| v.as_str()),
                Some(doc_id.to_string().as_str())
            );

            let doc_assets = h
                .server
                .dispatch_tool(
                    "memory_list_document_assets",
                    json!({ "doc_id": doc_id.to_string() }),
                    None,
                )
                .await
                .expect("list document assets");
            let doc_assets_json: serde_json::Value =
                serde_json::from_str(&first_text(&doc_assets)).expect("doc assets json");
            assert_eq!(
                doc_assets_json
                    .pointer("/assets/0/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );

            let memory_attachments = h
                .server
                .dispatch_tool(
                    "memory_list_memory_attachments",
                    json!({ "memory_id": memory_id.to_string() }),
                    None,
                )
                .await
                .expect("list memory attachments");
            let memory_attachments_json: serde_json::Value =
                serde_json::from_str(&first_text(&memory_attachments))
                    .expect("memory attachments json");
            let attachments = memory_attachments_json
                .pointer("/attachments")
                .and_then(|v| v.as_array())
                .expect("attachments array");
            assert!(attachments.iter().any(|attachment| {
                attachment.pointer("/asset_id").and_then(|v| v.as_str())
                    == Some(asset_id.to_string().as_str())
            }));
            assert!(attachments.iter().any(|attachment| {
                attachment.pointer("/doc_id").and_then(|v| v.as_str())
                    == Some(doc_id.to_string().as_str())
            }));
        });

        {
            let conn = h.open_db();
            let document_links: i64 = conn
                .query_row("SELECT COUNT(*) FROM document_assets", [], |r| r.get(0))
                .expect("count document links");
            assert_eq!(document_links, 1);
            let memory_attachments: i64 = conn
                .query_row("SELECT COUNT(*) FROM memory_attachments", [], |r| r.get(0))
                .expect("count memory attachments");
            assert_eq!(memory_attachments, 2);
        }
        h.shutdown(&runtime);
    }

    #[test]
    fn asset_read_tools_resolve_seeded_links() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        let asset_id = AssetId::new();
        let doc_id = DocumentId::new();
        let memory_id = MemoryId::new();
        {
            let conn = h.open_db();
            conn.execute(
                "INSERT INTO assets (
                    asset_id, sha256, mime_type, filename, size_bytes,
                    storage_path, source, status, created_by_principal,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, 'text/markdown', 'source.md', 13,
                    ?3, 'solo-staged://upload/test', 'active', 'tester', 1_000, 1_000)",
                rusqlite::params![
                    asset_id.to_string(),
                    "c".repeat(64),
                    format!("assets/blobs/cc/{}", "c".repeat(64)),
                ],
            )
            .expect("seed asset");
            conn.execute(
                "INSERT INTO documents (
                    doc_id, source, title, mime_type, ingested_at_ms, status, chunk_count
                 ) VALUES (?1, '/tmp/source.md', 'Source Doc', 'text/markdown', 1_001, 'active', 1)",
                rusqlite::params![doc_id.to_string()],
            )
            .expect("seed document");
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, content,
                    encoding_context_json, confidence, strength, salience,
                    tier, status, created_at_ms, updated_at_ms
                 ) VALUES (?1, 1_002, 'user_message', 'memory with source file',
                    '{}', 0.9, 0.5, 0.5, 'hot', 'active', 1_002, 1_002)",
                rusqlite::params![memory_id.to_string()],
            )
            .expect("seed memory");
            conn.execute(
                "INSERT INTO document_assets (
                    link_id, doc_id, asset_id, relation_type, note, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'source_upload', 'original', 1_003)",
                rusqlite::params![
                    AssetId::new().to_string(),
                    doc_id.to_string(),
                    asset_id.to_string(),
                ],
            )
            .expect("seed document asset link");
            conn.execute(
                "INSERT INTO memory_attachments (
                    attachment_id, memory_id, asset_id, relation_type, note, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'source_file', 'evidence', 1_004)",
                rusqlite::params![
                    AssetId::new().to_string(),
                    memory_id.to_string(),
                    asset_id.to_string(),
                ],
            )
            .expect("seed memory asset link");
        }
        let blob_hash = "c".repeat(64);
        let blob_path = h._tmp.path().join(format!("assets/blobs/cc/{blob_hash}"));
        std::fs::create_dir_all(blob_path.parent().expect("blob parent")).unwrap();
        std::fs::write(&blob_path, b"hello asset!!").unwrap();

        runtime.block_on(async {
            let listed = h
                .server
                .dispatch_tool("memory_list_assets", json!({}), None)
                .await
                .expect("list assets");
            let listed_json: serde_json::Value =
                serde_json::from_str(&first_text(&listed)).expect("list json");
            assert_eq!(
                listed_json
                    .pointer("/assets/0/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );
            let resources = h.server.dispatch_list_resources().await.expect("resources");
            let resources_json = serde_json::to_string(&resources).expect("resources json");
            assert!(
                resources_json.contains(&format!("solo://asset/{asset_id}")),
                "asset resource missing from resources/list: {resources_json}"
            );

            let inspected = h
                .server
                .dispatch_tool(
                    "memory_inspect_asset",
                    json!({ "asset_id": asset_id.to_string() }),
                    None,
                )
                .await
                .expect("inspect asset");
            let inspected_json: serde_json::Value =
                serde_json::from_str(&first_text(&inspected)).expect("inspect json");
            assert_eq!(
                inspected_json
                    .pointer("/asset/filename")
                    .and_then(|v| v.as_str()),
                Some("source.md")
            );
            assert_eq!(
                inspected_json
                    .pointer("/document_links/0/doc_id")
                    .and_then(|v| v.as_str()),
                Some(doc_id.to_string().as_str())
            );
            assert_eq!(
                inspected_json
                    .pointer("/memory_attachments/0/memory_id")
                    .and_then(|v| v.as_str()),
                Some(memory_id.to_string().as_str())
            );

            let prepared = h
                .server
                .dispatch_tool(
                    "memory_prepare_asset_download",
                    json!({ "asset_id": asset_id.to_string() }),
                    None,
                )
                .await
                .expect("prepare asset download");
            let prepared_json: serde_json::Value =
                serde_json::from_str(&first_text(&prepared)).expect("download contract json");
            assert_eq!(
                prepared_json
                    .pointer("/download_method")
                    .and_then(|v| v.as_str()),
                Some("GET")
            );
            let expected_download_path = format!("/memory/assets/{asset_id}/download");
            assert_eq!(
                prepared_json
                    .pointer("/download_path")
                    .and_then(|v| v.as_str()),
                Some(expected_download_path.as_str())
            );

            let source_download = h
                .server
                .dispatch_tool(
                    "memory_prepare_document_source_download",
                    json!({ "doc_id": doc_id.to_string() }),
                    None,
                )
                .await
                .expect("prepare document source download");
            let source_json: serde_json::Value =
                serde_json::from_str(&first_text(&source_download)).expect("source contract json");
            assert_eq!(
                source_json
                    .pointer("/source_asset_link/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );
            assert_eq!(
                source_json
                    .pointer("/download/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );

            let doc_assets = h
                .server
                .dispatch_tool(
                    "memory_list_document_assets",
                    json!({ "doc_id": doc_id.to_string() }),
                    None,
                )
                .await
                .expect("list document assets");
            let doc_assets_json: serde_json::Value =
                serde_json::from_str(&first_text(&doc_assets)).expect("doc assets json");
            assert_eq!(
                doc_assets_json
                    .pointer("/assets/0/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );

            let memory_attachments = h
                .server
                .dispatch_tool(
                    "memory_list_memory_attachments",
                    json!({ "memory_id": memory_id.to_string() }),
                    None,
                )
                .await
                .expect("list memory attachments");
            let memory_attachments_json: serde_json::Value =
                serde_json::from_str(&first_text(&memory_attachments))
                    .expect("memory attachments json");
            assert_eq!(
                memory_attachments_json
                    .pointer("/attachments/0/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );

            let resource = h
                .server
                .dispatch_read_resource(&format!("solo://asset/{asset_id}"))
                .await
                .expect("read asset resource");
            let resource_text = serde_json::to_value(&resource)
                .expect("resource json")
                .pointer("/contents/0/text")
                .and_then(|v| v.as_str())
                .expect("resource text")
                .to_string();
            let resource_json: serde_json::Value =
                serde_json::from_str(&resource_text).expect("resource metadata json");
            assert_eq!(
                resource_json
                    .pointer("/asset/asset_id")
                    .and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );

            let forgotten = h
                .server
                .dispatch_tool(
                    "memory_forget_asset",
                    json!({ "asset_id": asset_id.to_string() }),
                    None,
                )
                .await
                .expect("forget asset");
            let forgotten_json: serde_json::Value =
                serde_json::from_str(&first_text(&forgotten)).expect("forget asset json");
            assert_eq!(
                forgotten_json.pointer("/asset_id").and_then(|v| v.as_str()),
                Some(asset_id.to_string().as_str())
            );
            assert_eq!(
                forgotten_json
                    .pointer("/already_deleted")
                    .and_then(|v| v.as_bool()),
                Some(false)
            );
            assert_eq!(
                forgotten_json
                    .pointer("/document_links")
                    .and_then(|v| v.as_u64()),
                Some(1)
            );
            assert_eq!(
                forgotten_json
                    .pointer("/memory_attachments")
                    .and_then(|v| v.as_u64()),
                Some(1)
            );
        });
        {
            let conn = h.open_db();
            let status: String = conn
                .query_row(
                    "SELECT status FROM assets WHERE asset_id = ?",
                    rusqlite::params![asset_id.to_string()],
                    |r| r.get(0),
                )
                .expect("read asset status");
            assert_eq!(status, "deleted");
        }
        h.shutdown(&runtime);
    }

    #[test]
    fn forget_document_rejects_malformed_id() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_forget_document",
                    json!({ "doc_id": "not-a-uuid" }),
                    None,
                )
                .await
                .expect_err("malformed doc_id must error");
            let s = format!("{err:?}");
            assert!(s.contains("invalid doc_id"), "got: {s}");
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn forget_asset_rejects_malformed_id() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_forget_asset",
                    json!({ "asset_id": "not-a-uuid" }),
                    None,
                )
                .await
                .expect_err("malformed asset_id must error");
            let s = format!("{err:?}");
            assert!(s.contains("invalid asset_id"), "got: {s}");
        });
        h.shutdown(&runtime);
    }

    // -----------------------------------------------------------------
    // v0.9.2 — `memory_remember_batch` + `salience` MCP layer tests.
    // -----------------------------------------------------------------

    /// salience round-trip through `memory_remember`: an explicit
    /// in-range value reaches the writer; an absent value defaults
    /// to 0.5; an out-of-range value is rejected with invalid_params.
    #[test]
    fn remember_with_explicit_salience_round_trips() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let r = h
                .server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "with salience", "salience": 0.83 }),
                    None,
                )
                .await
                .expect("remember w/ salience succeeds");
            let text = first_text(&r);
            // Confirmation includes the new MemoryId.
            assert!(text.starts_with("remembered "), "got: {text}");
        });
        h.shutdown(&runtime);
    }

    #[test]
    fn remember_with_out_of_range_salience_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool(
                    "memory_remember",
                    json!({ "content": "out of range", "salience": 1.5 }),
                    None,
                )
                .await
                .unwrap_err();
            let s = format!("{err:?}");
            assert!(s.contains("salience must be"), "got: {s}");
        });
        h.shutdown(&runtime);
    }

    /// Salience boundary: 0.0 and 1.0 are both valid (inclusive range).
    #[test]
    fn remember_with_boundary_salience_succeeds() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            for s in [0.0_f64, 1.0_f64] {
                let r = h
                    .server
                    .dispatch_tool(
                        "memory_remember",
                        json!({ "content": format!("boundary-{s}"), "salience": s }),
                        None,
                    )
                    .await
                    .expect("boundary salience succeeds");
                assert!(first_text(&r).starts_with("remembered "));
            }
        });
        h.shutdown(&runtime);
    }

    /// Happy-path batch: 3 items go in, 3 memory_ids come out in order.
    #[test]
    fn remember_batch_returns_ids_in_order() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let items = json!([
                { "content": "batch-a" },
                { "content": "batch-b", "source_type": "user_preference", "salience": 0.9 },
                { "content": "batch-c", "salience": 0.1 },
            ]);
            let r = h
                .server
                .dispatch_tool("memory_remember_batch", json!({ "items": items }), None)
                .await
                .expect("batch succeeds");
            let text = first_text(&r);
            let parsed: serde_json::Value = serde_json::from_str(&text).expect("reply is JSON");
            let arr = parsed.as_array().expect("reply is array");
            assert_eq!(arr.len(), 3, "3 items in → 3 ids out: {text}");
            // Each entry must be a UUID-shaped string.
            for v in arr {
                let s = v.as_str().unwrap_or_else(|| panic!("non-string id: {v}"));
                assert_eq!(s.len(), 36, "UUID-shaped id expected: {s}");
            }
            // Distinct ids.
            let mut ids: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
            ids.sort();
            ids.dedup();
            assert_eq!(ids.len(), 3, "ids must be distinct: {text}");
        });
        h.shutdown(&runtime);
    }

    /// Empty items → invalid_params before any embedding work.
    #[test]
    fn remember_batch_empty_items_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let err = h
                .server
                .dispatch_tool("memory_remember_batch", json!({ "items": [] }), None)
                .await
                .unwrap_err();
            let s = format!("{err:?}");
            assert!(s.contains("must not be empty"), "got: {s}");
        });
        h.shutdown(&runtime);
    }

    /// Per-item validation: empty content trips invalid_params with the
    /// index of the offending item baked into the message.
    #[test]
    fn remember_batch_rejects_per_item_empty_content() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let items = json!([
                { "content": "ok-1" },
                { "content": "   " },
                { "content": "ok-3" },
            ]);
            let err = h
                .server
                .dispatch_tool("memory_remember_batch", json!({ "items": items }), None)
                .await
                .unwrap_err();
            let s = format!("{err:?}");
            assert!(s.contains("items[1]"), "must mention items[1]: {s}");
            assert!(s.contains("must not be empty"), "got: {s}");
        });
        h.shutdown(&runtime);
    }

    /// Per-item validation: out-of-range salience trips invalid_params
    /// with the item index in the message.
    #[test]
    fn remember_batch_rejects_per_item_salience_out_of_range() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let items = json!([
                { "content": "ok-1", "salience": 0.5 },
                { "content": "out-of-range", "salience": -0.1 },
            ]);
            let err = h
                .server
                .dispatch_tool("memory_remember_batch", json!({ "items": items }), None)
                .await
                .unwrap_err();
            let s = format!("{err:?}");
            assert!(s.contains("items[1]"), "must mention items[1]: {s}");
            assert!(s.contains("salience must be"), "got: {s}");
        });
        h.shutdown(&runtime);
    }

    /// Over-cap batch is rejected at the MCP layer so we never embed
    /// 201+ items. Pinned at the same constant as the writer-actor.
    #[test]
    fn remember_batch_over_cap_returns_invalid_params() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let items: Vec<serde_json::Value> = (0..(solo_storage::MAX_REMEMBER_BATCH_SIZE + 1))
                .map(|i| json!({ "content": format!("over-{i}") }))
                .collect();
            let err = h
                .server
                .dispatch_tool("memory_remember_batch", json!({ "items": items }), None)
                .await
                .unwrap_err();
            let s = format!("{err:?}");
            assert!(
                s.contains("MAX_REMEMBER_BATCH_SIZE"),
                "must mention the cap: {s}"
            );
        });
        h.shutdown(&runtime);
    }

    // -----------------------------------------------------------------
    // v0.11.0 P3: per-tool progress event tests.
    //
    // These tests invoke `dispatch_tool` with a real
    // `ProgressReporter` wired to a fresh `SessionState`, then drain
    // the session's broadcast receiver to observe the emitted events.
    // The pattern mirrors `mcp_progress::tests::progress_reporter_*`
    // but exercises the full handler call stack (including the writer
    // and query pipelines) end-to-end.
    // -----------------------------------------------------------------

    use crate::mcp_progress::{ProgressReporter, ProgressToken};
    use crate::mcp_session::SessionState;
    use std::sync::Arc as StdArc2;

    fn fresh_progress_session() -> StdArc2<SessionState> {
        StdArc2::new(SessionState::new(None))
    }

    fn drain_progress_events(
        rx: &mut tokio::sync::broadcast::Receiver<crate::mcp_session::McpStreamEvent>,
    ) -> Vec<crate::mcp_session::McpStreamEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    // v0.11.0 P3 note: `ingest_document_emits_progress_at_*` test lives
    // in `http::handler_tests` because the dispatch_tests harness uses
    // `WriterActor::spawn` (no embedder), so an end-to-end ingest panics
    // with "writer has no embedder". The handler_tests harness uses
    // `WriterActor::spawn_full` which carries an embedder; we exercise
    // the ingest progress checkpoints there.

    /// v0.11.0 P3: `memory_search_docs` emits 3 progress events when
    /// `top_k` exceeds the threshold (100).
    #[test]
    fn search_docs_emits_progress_only_when_top_k_above_100() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let session = fresh_progress_session();
            let mut rx = session.subscribe_events();
            let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!(42)));
            let _r = h
                .server
                .dispatch_tool(
                    "memory_search_docs",
                    json!({ "query": "anything", "limit": 150 }),
                    Some(reporter),
                )
                .await
                .expect("search succeeds");
            let events = drain_progress_events(&mut rx);
            assert_eq!(
                events.len(),
                3,
                "expected 3 search progress events at top_k=150, got {}",
                events.len()
            );
            // Spec shape: every event uses progressToken (echoed as
            // number 42) and walks progress 1..=3.
            for (i, ev) in events.iter().enumerate() {
                let params = &ev.data["params"];
                assert_eq!(params["progressToken"], json!(42));
                assert_eq!(params["total"], json!(3));
                assert_eq!(params["progress"], json!((i + 1) as u64));
            }
        });
        h.shutdown(&runtime);
    }

    /// v0.11.0 P3: `memory_search_docs` with `top_k <= 100` does NOT
    /// emit progress events even when a reporter is wired. Threshold
    /// gating per Decision C.
    #[test]
    fn search_docs_emits_no_progress_when_top_k_below_threshold() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let session = fresh_progress_session();
            let mut rx = session.subscribe_events();
            let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!("t")));
            let _r = h
                .server
                .dispatch_tool(
                    "memory_search_docs",
                    json!({ "query": "anything", "limit": 50 }),
                    Some(reporter),
                )
                .await
                .expect("search succeeds");
            let events = drain_progress_events(&mut rx);
            assert!(
                events.is_empty(),
                "expected no progress events at top_k=50, got {events:?}"
            );
        });
        h.shutdown(&runtime);
    }

    /// v0.11.0 P3: `memory_remember_batch` with > 50 items emits
    /// per-25-items embed progress + a final "embedded" + "inserted"
    /// event. A 51-item batch fires at items 25, 50, then embedded
    /// (51/51), then inserted (51/51) = 4 events.
    #[test]
    fn remember_batch_emits_progress_only_when_size_above_50() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let session = fresh_progress_session();
            let mut rx = session.subscribe_events();
            let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!("batch")));
            let items: Vec<serde_json::Value> = (0..51)
                .map(|i| json!({ "content": format!("item-{i}") }))
                .collect();
            let _r = h
                .server
                .dispatch_tool(
                    "memory_remember_batch",
                    json!({ "items": items }),
                    Some(reporter),
                )
                .await
                .expect("batch succeeds");
            let events = drain_progress_events(&mut rx);
            assert_eq!(
                events.len(),
                4,
                "expected 4 batch progress events for 51 items, got {}: {events:?}",
                events.len()
            );
            // First event = 25/51 "embedding"; second = 50/51 "embedding";
            // third = 51/51 "embedded"; fourth = 51/51 "inserted".
            let progresses: Vec<u64> = events
                .iter()
                .map(|e| e.data["params"]["progress"].as_u64().unwrap_or(0))
                .collect();
            assert_eq!(progresses, vec![25, 50, 51, 51]);
            assert_eq!(
                events.last().unwrap().data["params"]["message"],
                json!("inserted")
            );
            for ev in &events {
                assert_eq!(ev.data["params"]["progressToken"], json!("batch"));
                assert_eq!(ev.data["params"]["total"], json!(51));
            }
        });
        h.shutdown(&runtime);
    }

    /// v0.11.0 P3: small batches (<= 50) do NOT emit progress events
    /// even with a reporter wired. Wire-overhead gating per Decision C.
    #[test]
    fn remember_batch_emits_no_progress_when_size_below_threshold() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let session = fresh_progress_session();
            let mut rx = session.subscribe_events();
            let reporter = ProgressReporter::new(session.clone(), ProgressToken(json!("t")));
            // 5 items — well below the threshold.
            let items: Vec<serde_json::Value> = (0..5)
                .map(|i| json!({ "content": format!("small-{i}") }))
                .collect();
            let _r = h
                .server
                .dispatch_tool(
                    "memory_remember_batch",
                    json!({ "items": items }),
                    Some(reporter),
                )
                .await
                .expect("batch succeeds");
            let events = drain_progress_events(&mut rx);
            assert!(
                events.is_empty(),
                "expected no progress events for 5-item batch, got {events:?}"
            );
        });
        h.shutdown(&runtime);
    }

    /// v0.11.0 P3: stdio-style calls (no session = no progress reporter)
    /// must not panic and must produce no events. This pins the
    /// backward-compat invariant the rmcp `call_tool` path relies on.
    /// Uses `memory_search_docs` (no embedder dependency in the
    /// dispatch_tests harness) — the equivalent ingest_document
    /// "no progress" guarantee is asserted in `http::handler_tests`
    /// via the same `None` path.
    #[test]
    fn stdio_transport_does_not_emit_progress_events() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            // Construct a session purely for the rx end — the tool call
            // gets `None`, so the session must NOT receive anything.
            let session = fresh_progress_session();
            let mut rx = session.subscribe_events();
            let _r = h
                .server
                .dispatch_tool(
                    "memory_search_docs",
                    // Above the threshold so progress WOULD fire if a
                    // reporter were wired — but no reporter = no events.
                    json!({ "query": "anything", "limit": 200 }),
                    None, // stdio: no reporter
                )
                .await
                .expect("search succeeds without reporter");
            let events = drain_progress_events(&mut rx);
            assert!(
                events.is_empty(),
                "stdio path (no reporter) must not publish to ANY session: {events:?}"
            );
        });
        h.shutdown(&runtime);
    }

    /// v0.11.0 P3: emitted event ids are monotonically increasing per
    /// session across multiple tool calls. Pinned to surface any
    /// regression in `SessionState::publish_event`'s id allocator.
    #[test]
    fn progress_event_id_monotonic_per_session() {
        let runtime = rt();
        let h = Harness::new(&runtime);
        runtime.block_on(async {
            let session = fresh_progress_session();
            let mut rx = session.subscribe_events();
            // Two calls in sequence with progress; observe interleaved
            // ids stay strictly increasing.
            let r1 = ProgressReporter::new(session.clone(), ProgressToken(json!("a")));
            let r2 = ProgressReporter::new(session.clone(), ProgressToken(json!("b")));
            let _ = h
                .server
                .dispatch_tool(
                    "memory_search_docs",
                    json!({ "query": "q1", "limit": 150 }),
                    Some(r1),
                )
                .await;
            let _ = h
                .server
                .dispatch_tool(
                    "memory_search_docs",
                    json!({ "query": "q2", "limit": 150 }),
                    Some(r2),
                )
                .await;
            let events = drain_progress_events(&mut rx);
            assert!(events.len() >= 6, "expected at least 6 events: {events:?}");
            let ids: Vec<u64> = events.iter().map(|e| e.id).collect();
            for w in ids.windows(2) {
                assert!(w[0] < w[1], "event ids must be strictly monotonic: {ids:?}");
            }
        });
        h.shutdown(&runtime);
    }
}

// ===========================================================================
// v0.8.1 P2: MCP audit principal extraction
// ===========================================================================
//
// These tests live in their own module because they manipulate the
// `SOLO_MCP_PRINCIPAL_TOKEN` env var, which is process-global mutable
// state. Serialised via a static `Mutex` so cargo test's multi-threaded
// runner doesn't race. Pattern mirrors the env-guard discipline in
// `solo_cli::commands::common::ollama_overrides_tests`.

#[cfg(test)]
mod principal_extraction_tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialise tests that mutate `SOLO_MCP_PRINCIPAL_TOKEN`. Poisoned
    /// guards are recovered via `into_inner` so one panicking test
    /// doesn't sink the rest of the suite.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that unsets the env var on drop, so a panicking test
    /// doesn't leak state into the next case.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: every caller holds ENV_LOCK across construct + drop.
            unsafe { std::env::remove_var(ENV_MCP_PRINCIPAL_TOKEN) };
        }
    }

    fn set_principal_env(val: &str) -> EnvGuard {
        // SAFETY: ENV_LOCK held by caller.
        unsafe { std::env::set_var(ENV_MCP_PRINCIPAL_TOKEN, val) };
        EnvGuard
    }

    fn clear_principal_env() -> EnvGuard {
        // SAFETY: ENV_LOCK held by caller.
        unsafe { std::env::remove_var(ENV_MCP_PRINCIPAL_TOKEN) };
        EnvGuard
    }

    /// Stdio path: setting `SOLO_MCP_PRINCIPAL_TOKEN` produces a
    /// non-None principal at construction time.
    #[test]
    fn stdio_env_var_resolves_to_principal() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_principal_env("alice-token");
        let resolved = resolve_mcp_principal(None);
        assert_eq!(resolved.as_deref(), Some("alice-token"));
    }

    /// Stdio path: absent env var ⇒ `None` (regression — must preserve
    /// v0.8.0 behaviour for users without auth).
    #[test]
    fn stdio_no_env_var_resolves_to_none() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = clear_principal_env();
        assert_eq!(resolve_mcp_principal(None), None);
    }

    /// Stdio path: whitespace-only env var ⇒ `None` (don't pin every
    /// audit row to an empty/blank principal because of a launcher
    /// typo).
    #[test]
    fn stdio_whitespace_env_var_resolves_to_none() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_principal_env("   \t  ");
        assert_eq!(resolve_mcp_principal(None), None);
    }

    /// HTTP-MCP path: `Authorization: Bearer <token>` header resolves
    /// to the token as principal.
    #[test]
    fn http_header_resolves_to_bearer_token_principal() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = clear_principal_env();
        let resolved = resolve_mcp_principal(Some("Bearer api-token-xyz"));
        assert_eq!(resolved.as_deref(), Some("api-token-xyz"));
    }

    /// Precedence: when both env var AND header carry a token, the
    /// header wins (consistent with the rest of the auth stack — JWT
    /// claim beats `X-Solo-Tenant` header).
    #[test]
    fn http_header_beats_env_var() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_principal_env("env-token");
        let resolved = resolve_mcp_principal(Some("Bearer header-token"));
        assert_eq!(
            resolved.as_deref(),
            Some("header-token"),
            "header MUST win over env var per documented precedence"
        );
    }

    /// HTTP-MCP path: malformed header (no `Bearer ` prefix) ⇒ falls
    /// through to env-var path.
    #[test]
    fn http_malformed_header_falls_through_to_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_principal_env("env-fallback");
        let resolved = resolve_mcp_principal(Some("Basic dXNlcjpwYXNz"));
        assert_eq!(resolved.as_deref(), Some("env-fallback"));
    }

    /// HTTP-MCP path: empty bearer header (`Bearer ` with no token)
    /// falls through to env-var path. Matches the spirit of the
    /// whitespace-env-var rejection — don't credit a half-formed
    /// header.
    #[test]
    fn http_empty_bearer_header_falls_through_to_env() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_principal_env("env-fallback");
        let resolved = resolve_mcp_principal(Some("Bearer   "));
        assert_eq!(resolved.as_deref(), Some("env-fallback"));
    }

    /// Across N consecutive calls of `resolve_mcp_principal`, the
    /// resolved principal is stable for the same env-var setting
    /// (regression guard: an accidental thread-local cache would
    /// break the "stable across N tool calls in one session" contract
    /// the brief calls out).
    #[test]
    fn stable_across_multiple_resolutions() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = set_principal_env("stable-token");
        for _ in 0..5 {
            assert_eq!(resolve_mcp_principal(None).as_deref(), Some("stable-token"));
        }
    }
}

/// v0.9.0 P2 tests for the MCP-initialize-time LLM-config gate.
///
/// Pure-function tests of [`initialize_decision`]: no rmcp Peer is
/// constructed (the type's constructors are private), no MCP handshake
/// is driven. The wire-up between `initialize_decision` and the
/// side-effect path lives in [`SoloMcpServer::initialize`] and is
/// covered indirectly by the audit-row tests in
/// [`crate::llm::sampling::tests`] — those exercise the same
/// `SamplingLlmClient` + `WriteCommand::EmitLlmSamplingAudit` path
/// that `populate_sampling_steward` constructs.
#[cfg(test)]
mod initialize_decision_tests {
    use super::*;
    use solo_storage::LlmSettings;

    /// `[llm]` absent → always Allow (matches v0.8.x behaviour).
    #[test]
    fn no_llm_block_allows_initialize_regardless_of_sampling_capability() {
        assert_eq!(initialize_decision(&None, false), InitializeDecision::Allow);
        assert_eq!(initialize_decision(&None, true), InitializeDecision::Allow);
    }

    /// `[llm] mode = "none"` → always Allow.
    #[test]
    fn llm_none_allows_initialize_regardless_of_sampling_capability() {
        let s = Some(LlmSettings::None);
        assert_eq!(initialize_decision(&s, false), InitializeDecision::Allow);
        assert_eq!(initialize_decision(&s, true), InitializeDecision::Allow);
    }

    /// `[llm] mode = "anthropic"` → always Allow.
    #[test]
    fn llm_anthropic_allows_initialize_regardless_of_sampling_capability() {
        let s = Some(LlmSettings::Anthropic {
            api_key_env: "ANTHROPIC_API_KEY".into(),
            model: "claude-sonnet-4-6".into(),
            hosted_processing_consent: true,
        });
        assert_eq!(initialize_decision(&s, false), InitializeDecision::Allow);
        assert_eq!(initialize_decision(&s, true), InitializeDecision::Allow);
    }

    /// `[llm] mode = "ollama"` → always Allow.
    #[test]
    fn llm_ollama_allows_initialize_regardless_of_sampling_capability() {
        let s = Some(LlmSettings::Ollama {
            endpoint: solo_storage::OllamaEndpointKind::Local,
            base_url: "http://localhost:11434".into(),
            model: "qwen3:8b".into(),
            api_key_env: None,
            hosted_processing_consent: false,
        });
        assert_eq!(initialize_decision(&s, false), InitializeDecision::Allow);
        assert_eq!(initialize_decision(&s, true), InitializeDecision::Allow);
    }

    /// A peer advertising sampling does not re-enable the retired backend.
    #[test]
    fn llm_mcp_sampling_with_sampling_capability_populates_slot() {
        let s = Some(LlmSettings::McpSampling);
        assert_eq!(
            initialize_decision(&s, true),
            InitializeDecision::RejectDeprecatedSampling
        );
    }

    /// A peer without sampling gets the same explicit migration error.
    #[test]
    fn llm_mcp_sampling_without_sampling_capability_rejects() {
        let s = Some(LlmSettings::McpSampling);
        assert_eq!(
            initialize_decision(&s, false),
            InitializeDecision::RejectDeprecatedSampling
        );
    }

    /// The locked BLOCKER 2 error message body is byte-stable: a future
    /// audit-revision can grep these strings and confirm they still
    /// land.
    #[test]
    fn sampling_capability_missing_error_message_contains_all_alternatives() {
        let msg = sampling_capability_missing_error_message();
        // Banner + four alternative blocks.
        assert!(msg.contains("LLM backend `mcp_sampling`"));
        assert!(msg.contains("mode = \"anthropic\""));
        assert!(msg.contains("api_key_env = \"ANTHROPIC_API_KEY\""));
        assert!(msg.contains("mode = \"openai\""));
        assert!(msg.contains("api_key_env = \"OPENAI_API_KEY\""));
        assert!(msg.contains("mode = \"ollama\""));
        assert!(msg.contains("base_url = \"http://localhost:11434\""));
        assert!(msg.contains("mode = \"none\""));
        assert!(msg.contains("SEP-2577"));
    }
}

// fetch_recall_rows + RecallHit + RecallRow used to live here. Recall
// pipeline moved to solo_query::recall in commit (consolidate-recall);
// transports just call solo_query::run_recall and format the result.
