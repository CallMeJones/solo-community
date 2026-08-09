// SPDX-License-Identifier: Apache-2.0

//! `solo.config.toml` reader/writer.
//!
//! The config file lives alongside `solo.db` and stores everything Solo needs
//! to re-open the database on startup but does NOT need to keep secret. The
//! Argon2 salt is the load-bearing field — without it, the same passphrase
//! produces a different key, so the SQLCipher database becomes unreadable.
//!
//! Layout (TOML):
//! ```toml
//! schema_version = 1
//! salt_hex       = "0123456789abcdef0123456789abcdef"   # 16 bytes -> 32 hex
//!
//! [embedder]
//! name    = "ollama:nomic-embed-text"   # or "stub" for offline dev
//! version = "v1"                        # bump on any vector-shifting change
//! dim     = 768                         # probed at `solo init` for Ollama
//! dtype   = "f32"
//! ```
//!
//! Why TOML: human-readable for debugging + recovery. The whole file is small;
//! we don't need a more compact format.

use serde::{Deserialize, Serialize};
use solo_core::{Error, Result};
use std::path::Path;

use crate::key_material::SALT_LEN;

/// Current config schema version. Bump on any incompatible field change.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Auth-mode config persisted under `[auth]` in `solo.config.toml`
/// (v0.8.0 P3). Lives in `solo-storage` rather than `solo-api` because
/// `SoloConfig` is the canonical owner of all on-disk config blocks;
/// `solo-api::auth::AuthConfig` mirrors this shape and converts at the
/// transport boundary.
///
/// Two modes:
///   * `bearer` — `[auth] mode = "bearer", token = "…"`
///   * `oidc`   — `[auth] mode = "oidc", discovery_url = "…", audience = "…"`
///
/// Backward compatibility: when `[auth]` is absent, `solo http-serve`
/// continues to honor `--bearer-token-file` (v0.7.x behavior). Operators
/// migrate to config-driven auth by writing an `[auth]` block; the flag
/// stays as a runtime override for ad-hoc deployments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AuthSettings {
    Bearer {
        token: String,
    },
    Oidc {
        discovery_url: String,
        audience: String,
    },
}

/// LLM-backend config persisted under `[llm]` in `solo.config.toml`
/// (v0.9.0 P0b scaffold; wiring lands in v0.9.0 P1+).
///
/// Mirrors the [`AuthSettings`] shape: one enum, `#[serde(tag = "mode",
/// rename_all = "snake_case")]`, lives in `solo-storage` as the canonical
/// on-disk owner. The `solo-api`-side runtime mirror (used for transport-
/// adjacent wiring like the MCP-sampling capability gate) is added in
/// v0.9.0 P1 alongside the actual `build_llm_client_from_config` builder.
///
/// Five modes:
///   * **`none`** — Steward runs without an LLM. Clustering still
///     happens; abstractions + contradictions are skipped. Semantically
///     equivalent to the v0.8.x `NoopLlmClient` path with
///     `is_real_llm() == false`. Default when no env var hints at a
///     specific backend (v0.9.0 P1 implements the env-detected default
///     in `solo init`).
///   * **`anthropic`** — hosted Anthropic Claude via API key. The
///     `api_key_env` field names the env var that carries the key (so
///     the config file itself does NOT contain secrets); `model` selects
///     the model id used at request time.
///   * **`openai`** — hosted OpenAI Chat Completions; same env-var shape.
///   * **`ollama`** — local Ollama daemon at `base_url`; `model` is the
///     ollama model tag (e.g. `qwen3-coder:30b`).
///   * **`mcp_sampling`** — the LLM lives on the *connected MCP client*
///     and is called back via `sampling/createMessage`. v0.9.0 P0b
///     scaffolds the variant; v0.9.0 P2 wires the actual rmcp-backed
///     `LlmClient` impl, the capability gate at `mcp.initialize`, and
///     the daemon-mode validation that refuses-to-start when no MCP
///     peer is available.
///
/// Backward compatibility: when `[llm]` is absent, v0.9.x continues to
/// honor the v0.8.x env-var precedence (`ANTHROPIC_API_KEY`,
/// `OPENAI_API_KEY`) emitted with a one-time deprecation warning at
/// daemon start. v0.10.0 removes the env-var-only path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum LlmSettings {
    /// Cluster-only: the Steward runs but skips every LLM call.
    /// Default — fresh installs land here when no env-var hint
    /// surfaces a backend.
    #[default]
    None,
    /// Hosted Anthropic Claude via API key.
    Anthropic {
        #[serde(default = "default_anthropic_api_key_env")]
        api_key_env: String,
        #[serde(default = "default_anthropic_model")]
        model: String,
    },
    /// Hosted OpenAI Chat Completions via API key.
    Openai {
        #[serde(default = "default_openai_api_key_env")]
        api_key_env: String,
        #[serde(default = "default_openai_model")]
        model: String,
    },
    /// Local Ollama daemon.
    Ollama {
        #[serde(default = "default_ollama_base_url")]
        base_url: String,
        #[serde(default = "default_ollama_model")]
        model: String,
    },
    /// MCP-sampling — call back to the connected MCP client. Requires
    /// a peer that advertises the `sampling` capability at initialize;
    /// daemon-only deployments (no MCP peer at all) refuse to start
    /// when this variant is configured. v0.9.0 P2 implements both
    /// gates; this variant is scaffold-only at P0b.
    McpSampling,
}

fn default_anthropic_api_key_env() -> String {
    "ANTHROPIC_API_KEY".to_string()
}

fn default_anthropic_model() -> String {
    "claude-sonnet-4-6".to_string()
}

fn default_openai_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

fn default_openai_model() -> String {
    "gpt-5o".to_string()
}

fn default_ollama_base_url() -> String {
    "http://localhost:11434".to_string()
}

fn default_ollama_model() -> String {
    "qwen3-coder:30b".to_string()
}

impl LlmSettings {
    /// True iff this variant calls a real LLM backend. `None` and the
    /// "no peer attached yet" runtime state of `McpSampling` both
    /// short-circuit, but at config-load time only `None` is statically
    /// inert.
    pub fn is_real_llm(&self) -> bool {
        !matches!(self, LlmSettings::None)
    }

    /// Canonical TOML `mode` value (matches `#[serde(rename_all =
    /// "snake_case")]`). Used by error messages so operators see the
    /// same spelling they wrote in the config file.
    pub fn mode_str(&self) -> &'static str {
        match self {
            LlmSettings::None => "none",
            LlmSettings::Anthropic { .. } => "anthropic",
            LlmSettings::Openai { .. } => "openai",
            LlmSettings::Ollama { .. } => "ollama",
            LlmSettings::McpSampling => "mcp_sampling",
        }
    }

    /// True iff this variant requires a runtime MCP peer to operate.
    /// Used by daemon-startup validation (v0.9.0 P2: refuses to start
    /// if the daemon has no MCP-stdio transport configured AND the
    /// `[llm]` block requests `mcp_sampling`).
    pub fn requires_mcp_peer(&self) -> bool {
        matches!(self, LlmSettings::McpSampling)
    }

    /// Reject the retired `mcp_sampling` backend on every transport while
    /// continuing to parse it for actionable upgrade diagnostics.
    pub fn validate_against_transport(&self, _mcp_transport_available: bool) -> Result<()> {
        if self.requires_mcp_peer() {
            return Err(Error::storage(
                "LLM backend `mcp_sampling` has been retired because MCP \
                 sampling was deprecated by SEP-2577. Solo no longer calls \
                 back into MCP clients for model inference. Configure \
                 `[llm] mode` to one of \
                 `anthropic`, `openai`, `ollama`, or `none` in \
                 `solo.config.toml`."
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// Cadence + batch knobs for v0.9.0's background triple-extraction
/// pipeline. Persisted under `[triples]` in `solo.config.toml`.
///
/// Note: the v0.9.0 plan §6 sketched this as `[llm.triples]`. P1 lifted
/// the block to top-level `[triples]` because nesting under `[llm]`
/// would have required reshaping the v0.9.0 P0b `LlmSettings` enum
/// (currently `#[serde(tag = "mode")]`) into a `flatten`-style struct,
/// which would have churned every existing P0b serde test. Lifting
/// preserves the P0b scaffold unchanged while still exposing the
/// configuration knobs the plan intended.
///
/// Defaults match the plan's MINOR 1 + MINOR 3 revision corrections:
/// `trigger_interval_secs = 3600` (was 300 pre-MINOR 1, aligned with
/// `consolidate_interval_secs`); `trigger_episode_count = 50`.
///
/// The actual writer.rs `block_on(steward.abstract_cluster)` removal +
/// daemon-driven Steward batch dispatch is plan §4 P4, gated on P2's
/// `SamplingLlmClient`. This struct lands the cadence knobs in P1 so
/// the P4 dispatcher reads its config from the right place when it
/// arrives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriplesConfig {
    /// Time (seconds) between background triple-extraction batches.
    /// Aligned with `consolidate_interval_secs` per plan MINOR 1
    /// correction (3600s = hourly).
    #[serde(default = "default_triples_trigger_interval_secs")]
    pub trigger_interval_secs: u64,
    /// Number of new episodes (since last batch) above which the
    /// extraction batch fires immediately, regardless of the timer.
    /// Whichever cadence fires first wins.
    #[serde(default = "default_triples_trigger_episode_count")]
    pub trigger_episode_count: u32,
    /// v0.9.0 P1 (plan NEW finding #7): TOML-level default for the
    /// SWS-equivalent clustering / consolidate cadence. The CLI's
    /// `--consolidate-interval-secs` flag default is still `0`
    /// (explicit "I want this off"), but when the operator omits the
    /// flag entirely, the daemon falls back to this value. Defaults
    /// to 3600s (hourly), matching MINOR 1's `trigger_interval_secs`
    /// for consistent user-facing cadence semantics across the two
    /// timers.
    #[serde(default = "default_triples_consolidate_interval_secs")]
    pub consolidate_interval_secs: u64,
    /// v0.10.1 (P4 audit m5): per-cluster timeout (seconds) applied
    /// to each `Steward::abstract_cluster` call inside
    /// `Steward::extract_triples_batch`. A hung LLM backend on one
    /// cluster no longer blocks subsequent clusters in the same
    /// batch tick — the timeout fires, the cluster is marked as
    /// "deferred" (skipped, will retry on the next tick), and the
    /// next cluster proceeds.
    ///
    /// Default 60 (matches `SamplingLlmClient::with_timeout`'s
    /// recommended ceiling for LLM completions; a coalesced
    /// per-cluster sampling call inside the writer-actor's batch
    /// tick should complete well within this).
    ///
    /// A value of `0` disables the timeout — every per-cluster call
    /// runs to natural completion. Useful for operators running on
    /// very slow local backends (large Ollama models on CPU) who
    /// would rather wait than defer the cluster. NOT recommended in
    /// production: a single hung peer can stall the batch
    /// indefinitely.
    #[serde(default = "default_triples_cluster_timeout_secs")]
    pub cluster_timeout_secs: u64,
}

/// v0.11.1: Steward clustering knobs persisted under `[steward]` in
/// `solo.config.toml`. Mirrors the runtime [`solo_steward::StewardConfig`]
/// fields that are tuneable at deploy time without an env-var.
///
/// Both fields are `Option<T>`: when omitted (or the whole block is
/// absent), the runtime falls back to `solo_steward::StewardConfig::default()`
/// values. This preserves zero-change behaviour for existing configs.
///
/// **Layering with env vars**: env vars
/// (`SOLO_CLUSTER_COSINE_THRESHOLD` + `SOLO_CLUSTER_MIN_SIZE`) WIN over
/// TOML. The order is `code default ← TOML ← env`, which keeps the
/// operator's runtime escape hatch alive (set the env var to override
/// without editing the config file). See
/// [`solo_steward::StewardConfig::from_settings_then_env`] for the
/// resolution path.
///
/// Why expose only these two knobs (not `abstraction_max_tokens` or
/// `contradiction_check_enabled`): the v0.11.1 carry-forward issue from
/// commit `6602386` is specifically about the small-corpus / bundled-
/// embedder tuning. The other two fields already have stable defaults and
/// env-var paths; growing the TOML surface for them when no operator has
/// asked is unwarranted. They remain reachable through their existing
/// `SOLO_ABSTRACTION_MAX_TOKENS` / `SOLO_CONTRADICTION_CHECK_ENABLED`
/// env vars.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct StewardSettings {
    /// Minimum cluster size (number of episodes) below which a candidate
    /// cluster is discarded by the SWS-equivalent clustering pass.
    /// `None` (block absent or field omitted) → uses
    /// `StewardConfig::default().cluster_min_size`. Must be `>= 1` if
    /// present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_min_size: Option<usize>,
    /// Centroid-cosine threshold used by every
    /// clustering / existing-merge / merge-candidate count site.
    /// `None` (block absent or field omitted) → uses
    /// `StewardConfig::default().cluster_cosine_threshold`. Must be a
    /// finite f32 in `(0.0, 1.0]` if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_cosine_threshold: Option<f32>,
}

fn default_triples_trigger_interval_secs() -> u64 {
    3600
}

fn default_triples_trigger_episode_count() -> u32 {
    50
}

fn default_triples_consolidate_interval_secs() -> u64 {
    3600
}

fn default_triples_cluster_timeout_secs() -> u64 {
    60
}

impl Default for TriplesConfig {
    fn default() -> Self {
        Self {
            trigger_interval_secs: default_triples_trigger_interval_secs(),
            trigger_episode_count: default_triples_trigger_episode_count(),
            consolidate_interval_secs: default_triples_consolidate_interval_secs(),
            cluster_timeout_secs: default_triples_cluster_timeout_secs(),
        }
    }
}

/// v0.9.0 P4d: coalesce knobs for `SamplingCoordinator` (in
/// `solo-api::llm::sampling_coordinator`). Persisted under
/// `[sampling]` in `solo.config.toml`.
///
/// Plan §4 P4d names: `coalesce_window_ms` (default 5000) +
/// `coalesce_max_requests` (default 10). These collapse N
/// concurrent per-cluster sampling calls (from the
/// `triples_batch_timer`) into ONE coalesced `peer.create_message`,
/// surfacing ONE approval prompt per coalesce window in the user's
/// MCP client instead of N.
///
/// Bypass for non-sampling backends: `SamplingCoordinator` is wired
/// only when `[llm] mode = "mcp_sampling"`. For Ollama / Anthropic /
/// None, requests pass through to the underlying `LlmClient`
/// unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SamplingConfig {
    /// Upper bound (in milliseconds) the coordinator waits before
    /// flushing a non-empty buffer. Plan §4 P4d default: 5000.
    #[serde(default = "default_sampling_coalesce_window_ms")]
    pub coalesce_window_ms: u64,
    /// Buffer size that triggers an immediate flush regardless of
    /// the window timer. Plan §4 P4d default: 10.
    #[serde(default = "default_sampling_coalesce_max_requests")]
    pub coalesce_max_requests: u32,
}

fn default_sampling_coalesce_window_ms() -> u64 {
    5000
}

fn default_sampling_coalesce_max_requests() -> u32 {
    10
}

impl Default for SamplingConfig {
    fn default() -> Self {
        Self {
            coalesce_window_ms: default_sampling_coalesce_window_ms(),
            coalesce_max_requests: default_sampling_coalesce_max_requests(),
        }
    }
}

/// Diagnostic classification for `SamplingConfig` edge values. v0.9.1
/// P1 Fix 5 (m3): split out from `warn_on_edge_values` so the
/// classification logic is independently testable without capturing
/// `tracing` output (the workspace doesn't carry `tracing-test`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SamplingConfigDiagnostic {
    /// Both bounds are healthy; no operator action needed.
    Ok,
    /// One zero bound; coordinator still coalesces via the other
    /// bound but the resolved behavior is worth logging.
    Info,
    /// `coalesce_window_ms == 0` AND `coalesce_max_requests <= 1` —
    /// coalescing is effectively disabled. Warn the operator.
    Warn,
}

impl SamplingConfig {
    /// v0.9.1 P1 Fix 5 (m3): classify the resolved settings without
    /// emitting any log line. Pure function; pinned by
    /// [`tests::sampling_config_diagnostic_classifies_edge_values`].
    pub fn diagnostic(&self) -> SamplingConfigDiagnostic {
        if self.coalesce_window_ms == 0 && self.coalesce_max_requests <= 1 {
            SamplingConfigDiagnostic::Warn
        } else if self.coalesce_window_ms == 0 || self.coalesce_max_requests == 0 {
            SamplingConfigDiagnostic::Info
        } else {
            SamplingConfigDiagnostic::Ok
        }
    }

    /// v0.9.1 P1 Fix 5 (m3): inspect the resolved settings and emit
    /// operator-visible warnings for edge values that disable
    /// coalescing without an outright error.
    ///
    /// The coordinator's `with_settings` constructor clamps
    /// `coalesce_max_requests` via `max_batch.max(1)`, and a zero
    /// `coalesce_window_ms` makes the buffered-timer flush
    /// immediately — together they collapse the coordinator to a
    /// pass-through. That's a legitimate operator choice (e.g. for
    /// debugging or to surface every approval prompt individually),
    /// so we don't reject — but it's surprising enough that v0.9.0
    /// shipped without any signal, which led to the m3 audit finding.
    ///
    /// Called from `SoloConfig::read` at startup so the warning lands
    /// in the daemon log once at process boot.
    pub fn warn_on_edge_values(&self) {
        match self.diagnostic() {
            SamplingConfigDiagnostic::Warn => {
                tracing::warn!(
                    coalesce_window_ms = self.coalesce_window_ms,
                    coalesce_max_requests = self.coalesce_max_requests,
                    "sampling coalescing disabled by config \
                     (window=0ms, max_requests<=1); each LLM call \
                     goes through the MCP client uncoalesced"
                );
            }
            SamplingConfigDiagnostic::Info => {
                // One zero, not both — operators may be using `=0` as
                // a sentinel for "flush by the OTHER bound only". Log
                // at info so they can confirm the resolved behavior.
                tracing::info!(
                    coalesce_window_ms = self.coalesce_window_ms,
                    coalesce_max_requests = self.coalesce_max_requests,
                    "sampling config has a zero-valued bound; \
                     resolved settings logged for operator \
                     visibility (coordinator clamps max_requests to \
                     max(1) internally)"
                );
            }
            SamplingConfigDiagnostic::Ok => {}
        }
    }
}

/// Audit log settings persisted under `[audit]` in `solo.config.toml`
/// (v0.8.0 P4).
///
/// `retention_days = None` (omitted block, or block without the field)
/// = keep audit rows forever. This is the default — compliance use cases
/// often demand unbounded retention, and Solo treats audit rows as cheap
/// (~80 bytes/row uncompressed).
///
/// `purge_interval_secs = None` = no background sweep. Operators who set
/// `retention_days` but omit `purge_interval_secs` can still purge
/// manually via `solo audit purge`. When `Some(N)`, `LibraryHandle::open`
/// spawns a per-tenant tokio task that calls `purge_older_than` every
/// `N` seconds.
///
/// Backward compatible: pre-v0.8.0-P4 configs that omit the block
/// deserialize as `None` (default = keep forever, no background sweep).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditSettings {
    /// Retain audit rows for this many days. `None` = forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
    /// Run a background sweep every `purge_interval_secs` seconds in
    /// every cached `LibraryHandle`. `None` (the default) = no background
    /// sweep. Honored only when `retention_days` is also set; without
    /// a retention bound a sweep would have nothing to delete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purge_interval_secs: Option<u64>,
}

/// PII redaction settings persisted under `[redaction]` in `solo.config.toml`
/// (v0.8.0 P5).
///
/// Default = `enabled = false` (opt-in per the locked v0.8.0 design).
/// With `enabled = true` the writer-actor runs every built-in detector
/// (`email`, `ssn`, `us_phone`, `credit_card`, `aws_access_key`,
/// `github_pat`) over `episodes.content` and `document_chunks.content`
/// before INSERT. Operators disable specific defaults via
/// `exclude_builtin = ["email", ...]`, and add their own under
/// `[[redaction.custom]]` blocks.
///
/// Per-tenant redaction overrides are deliberately NOT supported in
/// v0.8.0 P5 — the redaction block in `solo.config.toml` is the single
/// source of truth. Per-tenant config layering is a v0.8.1+ concern.
///
/// Backward compatible: pre-v0.8.0-P5 configs that omit the block
/// deserialize with `RedactionConfig::default()` (everything off).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RedactionConfig {
    /// Master switch. Default `false` (opt-in).
    #[serde(default)]
    pub enabled: bool,
    /// Names of built-in patterns to disable. Defaults to empty (all
    /// builtins active when `enabled = true`).
    #[serde(default)]
    pub exclude_builtin: Vec<String>,
    /// Operator-supplied custom patterns. Compiled at
    /// `RedactionRegistry::from_config` time; an invalid regex here
    /// surfaces as `LibraryHandle::open` error.
    #[serde(default)]
    pub custom: Vec<CustomRedactionPattern>,
}

/// One operator-supplied custom redaction pattern from a
/// `[[redaction.custom]]` block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomRedactionPattern {
    /// Stable identifier — used as the sentinel suffix
    /// (`[REDACTED:<name>]`) when `replacement` is omitted, and as the
    /// audit-row count key. Must be non-empty.
    pub name: String,
    /// Rust regex syntax. Compiled at registry build time.
    pub regex: String,
    /// Optional replacement string. Defaults to `[REDACTED:<name>]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
}

/// Top-level config struct, serialized as TOML to `solo.config.toml`.
///
/// v0.11.1: `Eq` was dropped because the embedded [`StewardSettings`]
/// block carries an `Option<f32>` (cluster cosine threshold). `PartialEq`
/// stays — the only call site is the `roundtrip_via_disk` config test
/// which only needs partial equality.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SoloConfig {
    /// Version of the config schema itself (NOT the database schema). Bumping
    /// this lets future Solo versions migrate old config files in-place.
    pub schema_version: u32,
    /// 32-character lowercase hex string of the 16-byte Argon2 salt.
    pub salt_hex: String,
    /// Embedder identity: name, version, dim, dtype. The database holds
    /// embeddings tied to a specific `(name, version)`; if those change, the
    /// daemon refuses to start until `solo reembed` rebuilds them.
    pub embedder: EmbedderConfig,
    /// User-identity settings for the read-path. Default empty; backward-
    /// compatible with configs that don't declare an `[identity]` block.
    /// Today this carries `user_aliases` so `facts_about` can resolve a
    /// queried alias against historical triples whose `subject_id` was
    /// normalised to the canonical `"user"`. v0.5.0 Priority 1, sub-step
    /// 1C — see `docs/dev-log/0071-v0.5.x-roadmap.md`.
    #[serde(default)]
    pub identity: IdentityConfig,
    /// Document parser + chunker settings for the v0.7.0 RAG memory path.
    /// Default values match the v0.7.0 plan (target 500 tokens, 50-token
    /// overlap, the same allow-list as `document::parse::ALLOWED`).
    /// Backward-compatible: pre-v0.7.0 configs that omit the `[documents]`
    /// block deserialize cleanly with defaults.
    #[serde(default)]
    pub documents: DocumentConfig,
    /// Optional daemon-side allow-list for filesystem-reading document ingest
    /// operations. When `allowed_roots` is absent, legacy behavior remains
    /// unrestricted. When present, HTTP/MCP file-ingest paths must stay under
    /// one of the configured roots; an explicit empty list disables those file
    /// reads.
    #[serde(default)]
    pub workspace_file_access: WorkspaceFileAccessConfig,
    /// Auth-mode config for the HTTP transport (v0.8.0 P3). `None` =
    /// no `[auth]` block in the config file = fall through to the
    /// v0.7.x `--bearer-token-file` flag (loopback default still
    /// runs unauthenticated). Operators opt into config-driven auth
    /// by writing an `[auth]` block.
    #[serde(default)]
    pub auth: Option<AuthSettings>,
    /// Audit log settings (v0.8.0 P4). Default = `AuditSettings::default()`
    /// (retention_days=None → forever; purge_interval_secs=None → no
    /// background sweep). The audit table is always created via
    /// migration 0005 regardless of this config block; the block only
    /// controls retention behavior.
    #[serde(default)]
    pub audit: AuditSettings,
    /// PII redaction settings (v0.8.0 P5). Default = disabled. When
    /// enabled, the writer-actor runs the built-in detectors plus any
    /// `[[redaction.custom]]` patterns over text content before INSERT.
    /// Telemetry: `redaction.applied` audit rows record pattern-name
    /// match counts (never the matched substrings — strict).
    #[serde(default)]
    pub redaction: RedactionConfig,
    /// LLM-backend selection (v0.9.0 P0b scaffold; wiring lands in
    /// v0.9.0 P1). `None` = no `[llm]` block in the config file = fall
    /// through to the v0.8.x env-var precedence (Anthropic > OpenAI >
    /// none) with a one-time deprecation warning. Operators opt into
    /// config-driven LLM selection by writing an `[llm]` block. v0.10.0
    /// removes the env-var-only path.
    #[serde(default)]
    pub llm: Option<LlmSettings>,
    /// v0.9.0 P1: cadence + batch knobs for background triple
    /// extraction. Defaults match the plan's MINOR 1 + NEW finding #7
    /// corrections (`trigger_interval_secs = 3600`,
    /// `trigger_episode_count = 50`, `consolidate_interval_secs = 3600`).
    /// Pre-v0.9.0 configs without the `[triples]` block deserialize
    /// with these defaults — zero behaviour change on the v0.8.x path
    /// because the CLI's `--consolidate-interval-secs 0` flag default
    /// still wins when the operator passes it explicitly.
    #[serde(default)]
    pub triples: TriplesConfig,
    /// v0.9.0 P4d: coalesce knobs for the SamplingCoordinator. Default
    /// = 5000ms window + 10-request max-batch. Operators who hit
    /// approval-prompt fatigue can tighten the window; operators on
    /// fast clients can loosen it.
    ///
    /// Effectively inert for non-sampling backends (the coordinator
    /// inserts itself only when `[llm] mode = "mcp_sampling"`).
    #[serde(default)]
    pub sampling: SamplingConfig,
    /// v0.11.1: Steward clustering knobs. Both fields are `Option`;
    /// `None` (or whole block absent) means "use
    /// `solo_steward::StewardConfig::default()` for that field". Env
    /// vars `SOLO_CLUSTER_COSINE_THRESHOLD` + `SOLO_CLUSTER_MIN_SIZE`
    /// continue to override per-runtime.
    ///
    /// See [`StewardSettings`] for the rationale (the v0.11.0 carry-
    /// forward from commit `6602386` flagging the inline TODO for
    /// exposing both fields as TOML config).
    #[serde(default)]
    pub steward: StewardSettings,
}

/// User-identity settings persisted under `[identity]` in `solo.config.toml`.
///
/// `user_aliases` lets a user query `facts_about(subject = "alex")` and have
/// the read path also surface rows that were extracted historically with the
/// canonical `subject_id = "user"` (or vice-versa). The forward-going
/// extraction pipeline (Priority 1 sub-steps 1A + 1B) prefers named entities
/// over `"user"`, but historical triples written before 1A still use
/// `"user"` — read-side alias expansion bridges the two without rewriting
/// any data.
///
/// Default = empty — zero behaviour change for existing configs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityConfig {
    /// Names that should be treated as equivalent to the canonical `"user"`
    /// subject when querying `facts_about`. Lets a user query "facts about
    /// alex" and get rows that were historically extracted with
    /// `subject_id = "user"`. Case-sensitive — match the casing in the
    /// triples table.
    #[serde(default)]
    pub user_aliases: Vec<String>,
}

/// Document parser + chunker settings persisted under `[documents]` in
/// `solo.config.toml`. New in v0.7.0 (RAG / document-memory).
///
/// Defaults match the v0.7.0 implementation plan:
///   * `chunk_token_target = 500` — approx 2000 chars per chunk
///   * `chunk_overlap_tokens = 50` — ~10% overlap so cross-boundary
///     sentences survive into both neighbouring chunks
///   * `store_original_files_by_default = true` — staged uploads retain
///     the source file as a local asset unless a caller explicitly opts out
///   * `allowed_extensions` — see the document parser registry for the
///     canonical list (kept in sync; this field exists so operators
///     can disable searchable extraction for specific formats without
///     recompiling).
///
/// Backward compatible: pre-v0.7.0 configs that omit the block
/// deserialize with all defaults applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocumentConfig {
    #[serde(default = "default_chunk_token_target")]
    pub chunk_token_target: u32,
    #[serde(default = "default_chunk_overlap_tokens")]
    pub chunk_overlap_tokens: u32,
    #[serde(default = "default_store_original_files_by_default")]
    pub store_original_files_by_default: bool,
    #[serde(default = "default_allowed_extensions")]
    pub allowed_extensions: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceFileAccessConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_roots: Option<Vec<String>>,
}

fn default_chunk_token_target() -> u32 {
    500
}

fn default_chunk_overlap_tokens() -> u32 {
    50
}

fn default_store_original_files_by_default() -> bool {
    true
}

fn default_allowed_extensions() -> Vec<String> {
    vec![
        "md", "markdown", "txt", "rs", "py", "toml", "yaml", "yml", "json", "jsonl", "ndjson",
        "pdf", "html", "htm", "csv", "tsv", "xlsx", "docx", "pptx", "png", "jpg", "jpeg", "webp",
        "tif", "tiff", "blend", "zip", "gltf", "glb", "obj", "stl",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

impl Default for DocumentConfig {
    fn default() -> Self {
        Self {
            chunk_token_target: default_chunk_token_target(),
            chunk_overlap_tokens: default_chunk_overlap_tokens(),
            store_original_files_by_default: default_store_original_files_by_default(),
            allowed_extensions: default_allowed_extensions(),
        }
    }
}

/// Embedder identity persisted to disk so startup can detect drift.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmbedderConfig {
    pub name: String,
    pub version: String,
    pub dim: u32,
    /// Serialized form of `solo_core::EmbeddingDtype`: "f32" | "f16" | "i8" | "binary".
    pub dtype: String,
}

impl SoloConfig {
    /// Build a fresh config for first-run setup. Caller supplies the salt
    /// (typically `KeyMaterial::fresh_salt()`). `identity` defaults to
    /// empty — `solo init` does not seed `user_aliases`; users opt in by
    /// editing `solo.config.toml`.
    pub fn new(salt: [u8; SALT_LEN], embedder: EmbedderConfig) -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            salt_hex: hex::encode(salt),
            embedder,
            identity: IdentityConfig::default(),
            documents: DocumentConfig::default(),
            workspace_file_access: WorkspaceFileAccessConfig::default(),
            auth: None,
            audit: AuditSettings::default(),
            redaction: RedactionConfig::default(),
            llm: None,
            triples: TriplesConfig::default(),
            sampling: SamplingConfig::default(),
            steward: StewardSettings::default(),
        }
    }

    /// Decode the persisted salt back to its 16-byte form.
    pub fn salt_bytes(&self) -> Result<[u8; SALT_LEN]> {
        let bytes = hex::decode(&self.salt_hex)
            .map_err(|e| Error::storage(format!("config salt_hex is not valid hex: {e}")))?;
        if bytes.len() != SALT_LEN {
            return Err(Error::storage(format!(
                "config salt_hex must decode to {} bytes, got {}",
                SALT_LEN,
                bytes.len()
            )));
        }
        let mut out = [0u8; SALT_LEN];
        out.copy_from_slice(&bytes);
        Ok(out)
    }

    /// Serialize to `solo.config.toml` at the given path. Atomic-writes via a
    /// `<path>.tmp` file + rename so a crash mid-write can't leave a partial
    /// config. Refuses to overwrite an existing file (caller must handle the
    /// already-initialized case).
    ///
    /// Durability ordering: write tmp → fsync tmp → rename → fsync parent dir
    /// (Unix only; Windows relies on NTFS's metadata journal). The salt
    /// stored here is the only path back into the SQLCipher database — a
    /// partial-write corruption locks the user out forever, so we pay the
    /// fsync cost (~1 ms) without compromise.
    pub fn write(&self, path: &Path) -> Result<()> {
        if path.exists() {
            return Err(Error::conflict(format!(
                "config already exists: {}",
                path.display()
            )));
        }
        let tmp_path = path.with_extension("toml.tmp");
        let body = toml::to_string_pretty(self)
            .map_err(|e| Error::storage(format!("toml serialize: {e}")))?;

        // Open + write + fsync the tmp file before exposing it via rename.
        {
            let mut tmp_file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)
                .map_err(|e| Error::storage(format!("open tmp {}: {e}", tmp_path.display())))?;
            std::io::Write::write_all(&mut tmp_file, body.as_bytes())
                .map_err(|e| Error::storage(format!("write {}: {e}", tmp_path.display())))?;
            tmp_file
                .sync_all()
                .map_err(|e| Error::storage(format!("fsync tmp {}: {e}", tmp_path.display())))?;
        }

        std::fs::rename(&tmp_path, path)
            .map_err(|e| Error::storage(format!("rename to {}: {e}", path.display())))?;

        // fsync the parent directory so the rename persists across a crash.
        // No-op on Windows — opening a directory for FlushFileBuffers requires
        // FILE_FLAG_BACKUP_SEMANTICS; NTFS's metadata journal handles this case.
        #[cfg(unix)]
        {
            if let Some(parent) = path.parent() {
                if let Ok(d) = std::fs::OpenOptions::new().read(true).open(parent) {
                    let _ = d.sync_all();
                }
            }
        }

        Ok(())
    }

    /// Read + parse from `solo.config.toml`. Validates schema_version.
    pub fn read(path: &Path) -> Result<Self> {
        let body = std::fs::read_to_string(path)
            .map_err(|e| Error::storage(format!("read {}: {e}", path.display())))?;
        let cfg: Self = toml::from_str(&body)
            .map_err(|e| Error::storage(format!("toml parse {}: {e}", path.display())))?;
        if cfg.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(Error::storage(format!(
                "config schema_version mismatch: file is v{}, this binary expects v{}",
                cfg.schema_version, CONFIG_SCHEMA_VERSION
            )));
        }
        // Validate salt_hex shape eagerly so callers see the error here, not
        // later at key-derive time.
        let _ = cfg.salt_bytes()?;
        // v0.9.1 P1 Fix 5 (m3): surface SamplingConfig edge values to
        // the operator log so a `coalesce_window_ms = 0,
        // coalesce_max_requests = 0` config doesn't silently disable
        // coalescing.
        cfg.sampling.warn_on_edge_values();
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_embedder() -> EmbedderConfig {
        EmbedderConfig {
            name: "bge-m3".into(),
            version: "v1.0".into(),
            dim: 1024,
            dtype: "f32".into(),
        }
    }

    #[test]
    fn roundtrip_via_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");

        let salt = [7u8; SALT_LEN];
        let cfg = SoloConfig::new(salt, fixture_embedder());
        cfg.write(&path).unwrap();

        let read_back = SoloConfig::read(&path).unwrap();
        assert_eq!(cfg, read_back);
        assert_eq!(read_back.salt_bytes().unwrap(), salt);
    }

    #[test]
    fn write_refuses_overwrite() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        let cfg = SoloConfig::new([0; SALT_LEN], fixture_embedder());
        cfg.write(&path).unwrap();
        let err = cfg.write(&path).unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
    }

    #[test]
    fn read_rejects_wrong_schema_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            r#"
schema_version = 99
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#,
        )
        .unwrap();
        let err = SoloConfig::read(&path).unwrap_err();
        assert!(
            err.to_string().contains("schema_version mismatch"),
            "got: {err}"
        );
    }

    #[test]
    fn read_rejects_non_hex_salt() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let err = SoloConfig::read(&path).unwrap_err();
        // hex::decode fails on non-hex chars → "not valid hex".
        assert!(err.to_string().contains("salt_hex"), "got: {err}");
    }

    #[test]
    fn read_rejects_missing_embedder_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"
"#
            ),
        )
        .unwrap();
        let err = SoloConfig::read(&path).unwrap_err();
        // serde error for missing field
        assert!(
            err.to_string().to_lowercase().contains("embedder")
                || err.to_string().contains("missing"),
            "got: {err}"
        );
    }

    #[test]
    fn read_loads_user_aliases_from_identity_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[identity]
user_aliases = ["alex", "alice"]
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(
            cfg.identity.user_aliases,
            vec!["alex".to_string(), "alice".to_string()]
        );
    }

    #[test]
    fn read_defaults_identity_when_block_absent() {
        // Backward compat: existing configs (pre-v0.5.0) have no
        // [identity] block. They must still deserialize cleanly, with
        // `user_aliases` defaulting to empty.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert!(cfg.identity.user_aliases.is_empty());
    }

    #[test]
    fn read_defaults_user_aliases_when_identity_block_empty() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[identity]
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert!(cfg.identity.user_aliases.is_empty());
    }

    #[test]
    fn read_defaults_documents_when_block_absent() {
        // Pre-v0.7.0 configs have no [documents] block. They must still
        // deserialize cleanly, with defaults applied.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.documents.chunk_token_target, 500);
        assert_eq!(cfg.documents.chunk_overlap_tokens, 50);
        assert!(cfg.documents.store_original_files_by_default);
        assert!(cfg.documents.allowed_extensions.contains(&"md".to_string()));
        assert!(
            cfg.documents
                .allowed_extensions
                .contains(&"pdf".to_string())
        );
    }

    #[test]
    fn read_loads_custom_documents_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[documents]
chunk_token_target = 250
chunk_overlap_tokens = 25
store_original_files_by_default = false
allowed_extensions = ["md", "txt"]
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.documents.chunk_token_target, 250);
        assert_eq!(cfg.documents.chunk_overlap_tokens, 25);
        assert!(!cfg.documents.store_original_files_by_default);
        assert_eq!(
            cfg.documents.allowed_extensions,
            vec!["md".to_string(), "txt".to_string()]
        );
    }

    #[test]
    fn document_config_default_matches_plan() {
        let d = DocumentConfig::default();
        assert_eq!(d.chunk_token_target, 500);
        assert_eq!(d.chunk_overlap_tokens, 50);
        assert!(d.store_original_files_by_default);
        // Sanity: the allow-list mirrors the parser's. If parse::ALLOWED
        // grows, this default + the test below should be kept in sync.
        for ext in &[
            "md", "markdown", "txt", "rs", "py", "toml", "yaml", "yml", "json", "jsonl", "ndjson",
            "pdf", "html", "htm", "csv", "tsv", "xlsx", "docx", "pptx", "png", "jpg", "jpeg",
            "webp", "tif", "tiff", "blend", "zip", "gltf", "glb", "obj", "stl",
        ] {
            assert!(
                d.allowed_extensions.iter().any(|e| e == ext),
                "default allowed_extensions missing {ext}"
            );
        }
    }

    #[test]
    fn read_defaults_auth_when_block_absent() {
        // Pre-v0.8.0 configs (or operators sticking with the
        // `--bearer-token-file` CLI flag) have no `[auth]` block.
        // They must deserialize cleanly with `auth = None`.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert!(cfg.auth.is_none());
    }

    #[test]
    fn read_loads_bearer_auth_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[auth]
mode = "bearer"
token = "s3cr3t"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        match cfg.auth {
            Some(AuthSettings::Bearer { token }) => assert_eq!(token, "s3cr3t"),
            other => panic!("expected bearer, got {other:?}"),
        }
    }

    #[test]
    fn read_loads_oidc_auth_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[auth]
mode = "oidc"
discovery_url = "https://idp.example.com/.well-known/openid-configuration"
audience = "solo-prod"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        match cfg.auth {
            Some(AuthSettings::Oidc {
                discovery_url,
                audience,
            }) => {
                assert_eq!(
                    discovery_url,
                    "https://idp.example.com/.well-known/openid-configuration"
                );
                assert_eq!(audience, "solo-prod");
            }
            other => panic!("expected oidc, got {other:?}"),
        }
    }

    #[test]
    fn read_ignores_legacy_oidc_tenant_claim_field() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[auth]
mode = "oidc"
discovery_url = "https://idp.example.com/.well-known/openid-configuration"
audience = "solo-prod"
tenant_claim_name = "org_id"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        match cfg.auth {
            Some(AuthSettings::Oidc {
                discovery_url,
                audience,
            }) => {
                assert_eq!(
                    discovery_url,
                    "https://idp.example.com/.well-known/openid-configuration"
                );
                assert_eq!(audience, "solo-prod");
            }
            other => panic!("expected oidc, got {other:?}"),
        }
    }

    #[test]
    fn read_defaults_audit_when_block_absent() {
        // Pre-v0.8.0-P4 configs (and the typical fresh init) have no
        // `[audit]` block. They must deserialize cleanly with default
        // AuditSettings (retention_days=None, purge_interval_secs=None).
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert!(cfg.audit.retention_days.is_none());
        assert!(cfg.audit.purge_interval_secs.is_none());
    }

    #[test]
    fn read_loads_audit_block_with_retention_only() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[audit]
retention_days = 30
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.audit.retention_days, Some(30));
        assert!(cfg.audit.purge_interval_secs.is_none());
    }

    #[test]
    fn read_loads_audit_block_with_purge_interval() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[audit]
retention_days = 7
purge_interval_secs = 3600
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.audit.retention_days, Some(7));
        assert_eq!(cfg.audit.purge_interval_secs, Some(3600));
    }

    #[test]
    fn read_rejects_short_salt_hex() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "deadbeef"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let err = SoloConfig::read(&path).unwrap_err();
        assert!(err.to_string().contains("salt_hex"), "got: {err}");
    }

    // ----------------------------------------------------------------
    // v0.9.0 P0b — LlmSettings enum scaffold tests
    // ----------------------------------------------------------------
    //
    // The enum is the on-disk shape for the `[llm]` block in
    // `solo.config.toml`. v0.9.0 P1 wires the builder
    // (`build_llm_client_from_config`); P2 wires the MCP-sampling
    // capability gate. These tests cover *only* the serde + variant
    // semantics so the scaffold lands stable.

    /// Anthropic-mode round-trip: TOML → enum → TOML with custom + default
    /// fields, verifying field-level deserialization works and the model
    /// + env-var defaults activate when fields are omitted.
    #[test]
    fn llm_settings_anthropic_round_trip_with_defaults() {
        let toml_in = r#"mode = "anthropic""#;
        let parsed: LlmSettings = toml::from_str(toml_in).expect("parse");
        match parsed {
            LlmSettings::Anthropic {
                ref api_key_env,
                ref model,
            } => {
                assert_eq!(api_key_env, "ANTHROPIC_API_KEY");
                assert_eq!(model, "claude-sonnet-4-6");
            }
            other => panic!("expected Anthropic, got {other:?}"),
        }
        let serialized = toml::to_string(&parsed).expect("serialize");
        // round-trip stability: re-parse what we serialized.
        let reparsed: LlmSettings = toml::from_str(&serialized).expect("reparse");
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn llm_settings_openai_with_custom_model_and_env() {
        let toml_in = r#"
mode = "openai"
api_key_env = "MY_OAI_KEY"
model = "gpt-5o-mini"
"#;
        let parsed: LlmSettings = toml::from_str(toml_in).expect("parse");
        assert_eq!(
            parsed,
            LlmSettings::Openai {
                api_key_env: "MY_OAI_KEY".into(),
                model: "gpt-5o-mini".into(),
            }
        );
        assert_eq!(parsed.mode_str(), "openai");
        assert!(parsed.is_real_llm());
        assert!(!parsed.requires_mcp_peer());
    }

    #[test]
    fn llm_settings_ollama_round_trip_with_defaults() {
        let toml_in = r#"mode = "ollama""#;
        let parsed: LlmSettings = toml::from_str(toml_in).expect("parse");
        match parsed {
            LlmSettings::Ollama {
                ref base_url,
                ref model,
            } => {
                assert_eq!(base_url, "http://localhost:11434");
                assert_eq!(model, "qwen3-coder:30b");
            }
            other => panic!("expected Ollama, got {other:?}"),
        }
    }

    #[test]
    fn llm_settings_none_round_trips_and_short_circuits() {
        let toml_in = r#"mode = "none""#;
        let parsed: LlmSettings = toml::from_str(toml_in).expect("parse");
        assert_eq!(parsed, LlmSettings::None);
        assert!(!parsed.is_real_llm());
        assert!(!parsed.requires_mcp_peer());
        assert_eq!(parsed.mode_str(), "none");
        // Default is `none` — fresh installs land here when no env-var
        // hint surfaces a backend.
        assert_eq!(LlmSettings::default(), LlmSettings::None);
    }

    #[test]
    fn llm_settings_mcp_sampling_parses_and_requires_peer() {
        let toml_in = r#"mode = "mcp_sampling""#;
        let parsed: LlmSettings = toml::from_str(toml_in).expect("parse");
        assert_eq!(parsed, LlmSettings::McpSampling);
        assert!(parsed.is_real_llm());
        assert!(parsed.requires_mcp_peer());
        assert_eq!(parsed.mode_str(), "mcp_sampling");
    }

    #[test]
    fn llm_settings_unknown_mode_rejects() {
        let toml_in = r#"mode = "qwerty""#;
        let err = toml::from_str::<LlmSettings>(toml_in).unwrap_err();
        // serde tag mismatch → "unknown variant"
        let s = err.to_string();
        assert!(
            s.contains("unknown variant") || s.contains("qwerty"),
            "expected unknown-variant error; got: {s}"
        );
    }

    #[test]
    fn llm_settings_validate_against_transport_rejects_sampling_without_mcp() {
        // BLOCKER 2 (resolved in plan §3 Decision 4): daemon-mode with
        // `mode = "mcp_sampling"` must refuse to start with a clear
        // error pointing at the four alternative backends.
        let cfg = LlmSettings::McpSampling;
        let err = cfg
            .validate_against_transport(false)
            .expect_err("mcp_sampling without MCP transport must reject");
        let msg = err.to_string();
        assert!(
            msg.contains("mcp_sampling"),
            "error must name the offending mode; got: {msg}"
        );
        assert!(
            msg.contains("anthropic")
                && msg.contains("openai")
                && msg.contains("ollama")
                && msg.contains("none"),
            "error must list all 4 alternative modes for actionable recovery; got: {msg}"
        );
    }

    #[test]
    fn llm_settings_validate_against_transport_rejects_sampling_when_mcp_available() {
        let cfg = LlmSettings::McpSampling;
        let err = cfg
            .validate_against_transport(true)
            .expect_err("deprecated mcp_sampling must reject on every transport");
        assert!(err.to_string().contains("SEP-2577"));
    }

    #[test]
    fn llm_settings_validate_against_transport_no_op_for_static_backends() {
        // None, Anthropic, OpenAI, Ollama don't require a peer; the
        // gate is a no-op for them regardless of mcp_transport_available.
        for cfg in [
            LlmSettings::None,
            LlmSettings::Anthropic {
                api_key_env: "X".into(),
                model: "y".into(),
            },
            LlmSettings::Openai {
                api_key_env: "X".into(),
                model: "y".into(),
            },
            LlmSettings::Ollama {
                base_url: "http://x".into(),
                model: "y".into(),
            },
        ] {
            cfg.validate_against_transport(false)
                .expect("static backend must validate without MCP transport");
            cfg.validate_against_transport(true)
                .expect("static backend must validate with MCP transport too");
        }
    }

    /// `SoloConfig` round-trips with an `[llm]` block on disk — this
    /// is the integration shape `solo init` + `daemon` will care about.
    #[test]
    fn solo_config_round_trips_with_llm_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[llm]
mode = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-4-6"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(
            cfg.llm,
            Some(LlmSettings::Anthropic {
                api_key_env: "ANTHROPIC_API_KEY".into(),
                model: "claude-sonnet-4-6".into(),
            })
        );
    }

    /// Backward compat: pre-v0.9.0 configs without the `[llm]` block
    /// must still parse — the env-var precedence path remains the
    /// fallback at this layer.
    #[test]
    fn solo_config_defaults_llm_to_none_when_block_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert!(
            cfg.llm.is_none(),
            "missing [llm] block must deserialize as None (env-var fallback path)"
        );
    }

    #[test]
    fn solo_config_loads_workspace_file_access_roots() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[workspace_file_access]
allowed_roots = ["/work/solo", "/work/docs"]
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(
            cfg.workspace_file_access.allowed_roots,
            Some(vec!["/work/solo".to_string(), "/work/docs".to_string()])
        );
    }

    #[test]
    fn solo_config_defaults_workspace_file_access_to_unrestricted() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert!(cfg.workspace_file_access.allowed_roots.is_none());
    }

    // ----------------------------------------------------------------
    // v0.9.0 P1 — TriplesConfig defaults + serde shape
    // ----------------------------------------------------------------
    //
    // The block lives at top-level `[triples]` (not `[llm.triples]` as
    // the plan's TOML sketch suggested) — see TriplesConfig docstring
    // for the Decision-During-Implementation rationale.

    /// Default `TriplesConfig` reflects plan MINOR 1 + NEW finding #7:
    /// hourly cadence, 50-episode burst threshold, hourly consolidate.
    #[test]
    fn triples_config_default_matches_plan_defaults() {
        let t = TriplesConfig::default();
        assert_eq!(t.trigger_interval_secs, 3600, "MINOR 1: hourly cadence");
        assert_eq!(t.trigger_episode_count, 50);
        assert_eq!(
            t.consolidate_interval_secs, 3600,
            "NEW finding #7: TOML-level default flipped to 3600 for new installs"
        );
        assert_eq!(
            t.cluster_timeout_secs, 60,
            "v0.10.1 m5: per-cluster LLM call inside extract_triples_batch \
             gets a 60-second default ceiling"
        );
    }

    /// v0.10.1 m5: operators can override the per-cluster timeout
    /// from TOML.
    #[test]
    fn solo_config_loads_custom_cluster_timeout_secs() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[triples]
cluster_timeout_secs = 10
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.triples.cluster_timeout_secs, 10);
        // Other knobs fall back to defaults.
        assert_eq!(cfg.triples.trigger_interval_secs, 3600);
        assert_eq!(cfg.triples.trigger_episode_count, 50);
    }

    /// Backward compat: pre-v0.9.0 configs without the `[triples]`
    /// block must deserialize with the plan's defaults applied.
    #[test]
    fn solo_config_defaults_triples_block_when_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.triples, TriplesConfig::default());
    }

    /// Operator-supplied `[triples]` overrides each default
    /// independently.
    #[test]
    fn solo_config_loads_custom_triples_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[triples]
trigger_interval_secs = 900
trigger_episode_count = 25
consolidate_interval_secs = 1800
cluster_timeout_secs = 45
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.triples.trigger_interval_secs, 900);
        assert_eq!(cfg.triples.trigger_episode_count, 25);
        assert_eq!(cfg.triples.consolidate_interval_secs, 1800);
        assert_eq!(cfg.triples.cluster_timeout_secs, 45);
    }

    /// Partial `[triples]` keeps unrelated defaults — each field
    /// has its own `#[serde(default = "...")]`.
    #[test]
    fn solo_config_triples_partial_keeps_other_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[triples]
trigger_episode_count = 10
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.triples.trigger_episode_count, 10);
        // Other knobs fall back to defaults.
        assert_eq!(cfg.triples.trigger_interval_secs, 3600);
        assert_eq!(cfg.triples.consolidate_interval_secs, 3600);
        assert_eq!(cfg.triples.cluster_timeout_secs, 60);
    }

    // ----------------------------------------------------------------
    // v0.9.0 P4d — SamplingConfig defaults + serde shape
    // ----------------------------------------------------------------
    //
    // The `[sampling]` block carries the `SamplingCoordinator`'s
    // coalesce knobs (`coalesce_window_ms` + `coalesce_max_requests`).
    // Defaults match plan §4 P4d (5000ms window + 10-request max-batch).

    /// Default `SamplingConfig` matches plan §4 P4d.
    #[test]
    fn sampling_config_default_matches_plan_defaults() {
        let s = SamplingConfig::default();
        assert_eq!(s.coalesce_window_ms, 5000);
        assert_eq!(s.coalesce_max_requests, 10);
    }

    /// Backward compat: pre-P4d configs without the `[sampling]`
    /// block deserialize to defaults (zero behaviour change for
    /// every v0.8.x config).
    #[test]
    fn solo_config_defaults_sampling_block_when_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.sampling, SamplingConfig::default());
    }

    /// Operator-supplied `[sampling]` overrides each knob.
    #[test]
    fn solo_config_loads_custom_sampling_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[sampling]
coalesce_window_ms = 1500
coalesce_max_requests = 5
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.sampling.coalesce_window_ms, 1500);
        assert_eq!(cfg.sampling.coalesce_max_requests, 5);
    }

    /// Partial `[sampling]` keeps unrelated defaults — each field
    /// has its own `#[serde(default = "...")]`.
    #[test]
    fn solo_config_sampling_partial_keeps_other_defaults() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[sampling]
coalesce_max_requests = 25
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.sampling.coalesce_max_requests, 25);
        assert_eq!(
            cfg.sampling.coalesce_window_ms, 5000,
            "unset knob falls back to default"
        );
    }

    // ----------------------------------------------------------------
    // v0.9.1 P1 Fix 5 (m3) — SamplingConfig edge value diagnostic
    // ----------------------------------------------------------------
    //
    // The coordinator clamps `coalesce_max_requests.max(1)` internally
    // (so 0 → 1 in practice), and `coalesce_window_ms = 0` flushes the
    // buffered timer immediately. Together these two edge values make
    // the coordinator a pass-through. We don't reject (operators may
    // want pass-through), but we do surface the resolved settings.

    /// `diagnostic()` returns `Warn` when both bounds disable
    /// coalescing, `Info` when only one is zero, and `Ok` for the
    /// default + every healthy combination.
    #[test]
    fn sampling_config_diagnostic_classifies_edge_values() {
        // Default — healthy.
        assert_eq!(
            SamplingConfig::default().diagnostic(),
            SamplingConfigDiagnostic::Ok
        );

        // Both bounds zeroed (operator opted into pass-through).
        let disabled = SamplingConfig {
            coalesce_window_ms: 0,
            coalesce_max_requests: 0,
        };
        assert_eq!(disabled.diagnostic(), SamplingConfigDiagnostic::Warn);

        // Window=0 + max_requests=1 (post-clamp equivalent) — same
        // pass-through behaviour, classified the same way.
        let disabled_clamped = SamplingConfig {
            coalesce_window_ms: 0,
            coalesce_max_requests: 1,
        };
        assert_eq!(
            disabled_clamped.diagnostic(),
            SamplingConfigDiagnostic::Warn
        );

        // Only window zero — coordinator still coalesces up to
        // max_requests=10. Worth logging at info, not a warning.
        let info_window = SamplingConfig {
            coalesce_window_ms: 0,
            coalesce_max_requests: 10,
        };
        assert_eq!(info_window.diagnostic(), SamplingConfigDiagnostic::Info);

        // Only max_requests zero — coordinator flushes when the
        // window timer fires.
        let info_max = SamplingConfig {
            coalesce_window_ms: 5000,
            coalesce_max_requests: 0,
        };
        assert_eq!(info_max.diagnostic(), SamplingConfigDiagnostic::Info);

        // Healthy non-default — Ok.
        let custom = SamplingConfig {
            coalesce_window_ms: 250,
            coalesce_max_requests: 3,
        };
        assert_eq!(custom.diagnostic(), SamplingConfigDiagnostic::Ok);
    }

    /// `warn_on_edge_values()` does not panic for any classification
    /// (including the `Ok` and `Info` no-op paths). Trust the
    /// classification test above for behavior; this test just pins
    /// that wiring the warn-emitter into `read` is safe even when the
    /// daemon has no tracing subscriber installed.
    #[test]
    fn sampling_config_warn_on_edge_values_does_not_panic() {
        SamplingConfig::default().warn_on_edge_values();
        SamplingConfig {
            coalesce_window_ms: 0,
            coalesce_max_requests: 0,
        }
        .warn_on_edge_values();
        SamplingConfig {
            coalesce_window_ms: 0,
            coalesce_max_requests: 10,
        }
        .warn_on_edge_values();
        SamplingConfig {
            coalesce_window_ms: 5000,
            coalesce_max_requests: 0,
        }
        .warn_on_edge_values();
    }

    /// End-to-end pin: a `[sampling]` block with both bounds zeroed
    /// parses cleanly through `SoloConfig::read` (no rejection — the
    /// validation is informational only).
    #[test]
    fn solo_config_read_accepts_disabled_coalescing_block() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[sampling]
coalesce_window_ms = 0
coalesce_max_requests = 0
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.sampling.coalesce_window_ms, 0);
        assert_eq!(cfg.sampling.coalesce_max_requests, 0);
        assert_eq!(
            cfg.sampling.diagnostic(),
            SamplingConfigDiagnostic::Warn,
            "0/0 must classify Warn — the warning gets emitted to \
             tracing during read()"
        );
    }

    // ----------------------------------------------------------------
    // v0.11.1 — `[steward]` TOML block parsing
    // ----------------------------------------------------------------
    //
    // Both fields are `Option<T>`; an absent block (or absent field)
    // surfaces as `None`, which the daemon-side resolution
    // (`StewardConfig::from_settings_then_env`) maps to "use the
    // built-in default for that field". Backward-compatible with every
    // existing solo.config.toml — no migration required.

    /// Pre-v0.11.1 configs (or operators sticking with env vars only)
    /// have no `[steward]` block. They must deserialize cleanly with
    /// `cluster_min_size` and `cluster_cosine_threshold` both `None`.
    #[test]
    fn read_defaults_steward_when_block_absent() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert!(cfg.steward.cluster_min_size.is_none());
        assert!(cfg.steward.cluster_cosine_threshold.is_none());
    }

    /// Both fields explicit + valid: the parsed values surface unchanged
    /// (validation happens at the daemon-side `from_settings_then_env`
    /// step). This pins the type + name spelling expected at the wire
    /// level: `cluster_min_size = <int>`, `cluster_cosine_threshold = <float>`.
    #[test]
    fn read_loads_steward_block_with_both_overrides() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[steward]
cluster_min_size = 4
cluster_cosine_threshold = 0.7
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.steward.cluster_min_size, Some(4));
        assert_eq!(cfg.steward.cluster_cosine_threshold, Some(0.7));
    }

    /// Empty `[steward]` block (operator added the header but no fields)
    /// or a partial block must also deserialize cleanly. This guards
    /// against the common "I'll come back and fill this in later" path.
    #[test]
    fn read_loads_steward_block_with_partial_overrides() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("solo.config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
schema_version = {CONFIG_SCHEMA_VERSION}
salt_hex = "00000000000000000000000000000000"

[embedder]
name = "bge-m3"
version = "v1.0"
dim = 1024
dtype = "f32"

[steward]
cluster_min_size = 5
"#
            ),
        )
        .unwrap();
        let cfg = SoloConfig::read(&path).expect("read ok");
        assert_eq!(cfg.steward.cluster_min_size, Some(5));
        assert!(
            cfg.steward.cluster_cosine_threshold.is_none(),
            "field omitted from block — should be None, daemon resolves to default"
        );
    }
}
