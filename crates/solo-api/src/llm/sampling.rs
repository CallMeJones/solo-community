// SPDX-License-Identifier: Apache-2.0

//! Legacy sampling adapter retained for compatibility tests.
//!
//! The live MCP callback was retired after SEP-2577 deprecated sampling. No
//! production transport constructs this client; direct LLM backends are used
//! instead. The abstract adapter remains to preserve historical test coverage.
//!
//! Per the v0.9.0 design (`docs/dev-log/0098-v0.9.0-implementation-plan.md`
//! §6 "Sampling-backed LLM client" / MAJOR 1 + MAJOR 3 resolutions):
//!
//!   * Steward holds an `Arc<dyn LlmClient>`. When `LlmConfig::McpSampling`
//!     is configured, the Steward's `LlmClient` is a `SamplingLlmClient`
//!     constructed at MCP `initialize` time (when the live peer becomes
//!     available — the `LibraryHandle::steward_slot` LATE-population path).
//!
//!   * `SamplingLlmClient::complete()` translates the workspace's
//!     `Message` → `rmcp::SamplingMessage`, calls
//!     `peer.create_message(params).await`, extracts the assistant's
//!     text from the returned `CreateMessageResult`, and emits a
//!     per-call `AuditOperation::LlmSamplingCall` row through the
//!     tenant's `WriteHandle` (lesson #30: sync in writer-actor tx
//!     for ACID).
//!
//!   * **Privacy invariant**: the audit `details_json` carries metadata
//!     only — model hint, message count, max_tokens, duration_ms,
//!     total prompt character count, output character count. **The raw
//!     prompt content MUST NOT appear in the audit row**. Pinned by
//!     [`tests::audit_row_omits_raw_prompt_text`].
//!
//!   * Error paths land structured audit rows:
//!     - Client refusal → `result = "forbidden"`,
//!       `details_json.reason = "client_refused"`.
//!     - Timeout → `result = "error"`,
//!       `details_json.reason = "timeout"`.
//!     - Other transport / malformed-response → `result = "error"`,
//!       `details_json.reason = <category>`.
//!
//!   * Per-call rate-limit / coalescing is **deferred to v0.9.0 P4**
//!     (`SamplingCoordinator`). P2 wires the per-call path only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rmcp::model::{
    CreateMessageRequestParams, CreateMessageResult, ModelHint, ModelPreferences, Role as RmcpRole,
    SamplingMessage, SamplingMessageContent,
};
use rmcp::service::ServiceError;
use solo_core::{Error as CoreError, LlmClient, Message, Result as CoreResult, Role};
use solo_storage::{AuditEvent, AuditOperation, AuditResult, WriteHandle};

/// Default per-call timeout. Drives the bounded wait around
/// `peer.create_message`; if the client refuses or stalls, the caller
/// sees a structured timeout error instead of an indefinite hang.
///
/// 30 seconds matches the consolidate-timer's cadence margins: an
/// LLM call slower than this would already starve the Steward batch
/// in P4's coordinator. Configurable per-construct via
/// [`SamplingLlmClient::with_timeout`].
pub const DEFAULT_SAMPLING_TIMEOUT: Duration = Duration::from_secs(30);

/// Default max_tokens for sampling completions. Matches
/// `solo-steward::StewardConfig::default().abstraction_max_tokens`
/// so the wire shape is identical to what the Steward would have
/// requested from any other backend.
const DEFAULT_SAMPLING_MAX_TOKENS: u32 = 512;

/// Error surface for [`SamplingClient::create_message`]. Combines the
/// real rmcp `ServiceError` (when wrapping a live `Peer<RoleServer>`)
/// with [`super::super::test_support::fake_mcp_client::FakeSamplingError`]
/// (when driving the fixture from tests).
#[derive(Debug)]
pub enum SamplingError {
    /// Forwarded from `rmcp::Peer::create_message`.
    Service(ServiceError),
    /// Routed from [`super::super::test_support::fake_mcp_client::
    /// FakeSamplingError`] in test paths.
    #[cfg(any(test, feature = "test-support"))]
    Fake(crate::test_support::FakeSamplingError),
}

impl std::fmt::Display for SamplingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Service(e) => write!(f, "{e}"),
            #[cfg(any(test, feature = "test-support"))]
            Self::Fake(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SamplingError {}

impl SamplingError {
    /// Classifier used by [`SamplingLlmClient::complete`] to map the
    /// transport-level error to an audit-row category + a Solo
    /// [`CoreError`] variant.
    ///
    /// `(category_for_audit, treat_as_forbidden)` — `forbidden` becomes
    /// `AuditResult::Forbidden` + `CoreError::Forbidden`; everything
    /// else is `AuditResult::Error` + `CoreError::Llm`.
    pub fn classify(&self) -> (&'static str, bool) {
        match self {
            Self::Service(_) => ("transport_error", false),
            #[cfg(any(test, feature = "test-support"))]
            Self::Fake(e) => match e {
                crate::test_support::FakeSamplingError::Refused { .. } => ("client_refused", true),
                crate::test_support::FakeSamplingError::Transport { .. } => {
                    ("transport_error", false)
                }
                crate::test_support::FakeSamplingError::MalformedResponse { .. } => {
                    ("malformed_response", false)
                }
            },
        }
    }
}

/// Trait abstracting the `sampling/createMessage` RPC. The production
/// impl wraps `Arc<Peer<RoleServer>>`; the test impl is
/// [`super::super::test_support::fake_mcp_client::FakeMcpClient`].
///
/// Separating the trait from the concrete `Peer<RoleServer>` is the
/// way around rmcp's `Peer` having private constructors — we can't
/// build a fake `Peer` for tests, so we inject behind a trait.
#[async_trait]
pub trait SamplingClient: Send + Sync {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
    ) -> Result<CreateMessageResult, SamplingError>;
}

/// `LlmClient` impl whose `complete()` calls back via the connected
/// MCP client's sampling capability.
///
/// Construct via [`SamplingLlmClient::new`] (production path: wraps a
/// real `Peer<RoleServer>`) or [`SamplingLlmClient::with_sampling_client`]
/// (test path: takes the abstracted [`SamplingClient`] trait object
/// directly so [`super::super::test_support::fake_mcp_client::
/// FakeMcpClient`] can drive it).
///
/// Cheap to clone — every field is `Arc`-shared.
#[derive(Clone)]
pub struct SamplingLlmClient {
    /// The RPC channel back to the MCP client.
    sampling_client: Arc<dyn SamplingClient>,
    /// Per-tenant `WriteHandle` for the synchronous audit emit. Routes
    /// through the writer-actor's mpsc so the
    /// `AuditOperation::LlmSamplingCall` INSERT lands in a dedicated
    /// `BEGIN IMMEDIATE` transaction on the writer's connection.
    write_handle: WriteHandle,
    /// Cached audit `principal_subject` for the MCP session. Resolved
    /// at session init time (see `mcp::resolve_mcp_principal`); `None`
    /// for unauthenticated stdio sessions.
    audit_principal: Option<String>,
    /// `max_tokens` value sent on every `sampling/createMessage`.
    /// Defaults to [`DEFAULT_SAMPLING_MAX_TOKENS`]; configurable via
    /// [`Self::with_max_tokens`].
    max_tokens: u32,
    /// Bounded wait on `create_message`. See
    /// [`DEFAULT_SAMPLING_TIMEOUT`].
    timeout: Duration,
}

impl SamplingLlmClient {
    /// Compatibility-test constructor accepting any [`SamplingClient`]
    /// implementation. Pair with
    /// [`super::super::test_support::fake_mcp_client::FakeMcpClient`]
    /// in tests.
    pub fn with_sampling_client(
        sampling_client: Arc<dyn SamplingClient>,
        write_handle: WriteHandle,
        audit_principal: Option<String>,
    ) -> Self {
        Self {
            sampling_client,
            write_handle,
            audit_principal,
            max_tokens: DEFAULT_SAMPLING_MAX_TOKENS,
            timeout: DEFAULT_SAMPLING_TIMEOUT,
        }
    }

    /// Override the per-call `max_tokens` cap.
    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n.max(1);
        self
    }

    /// Override the per-call timeout.
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Build the `CreateMessageRequestParams` from Solo's `Message`
    /// vec. Splits out `Role::System` into the `system_prompt` field
    /// (rmcp's `SamplingMessage::role` is only User / Assistant) and
    /// hints the user's MCP client toward a Claude-class model.
    fn build_request(&self, messages: &[Message]) -> CreateMessageRequestParams {
        // Split system messages out of the conversation history; the
        // sampling protocol carries the system prompt as a top-level
        // field rather than inline.
        let mut system_parts: Vec<String> = Vec::new();
        let mut samp_messages: Vec<SamplingMessage> = Vec::new();
        for m in messages {
            match m.role {
                Role::System => system_parts.push(m.content.clone()),
                Role::User => {
                    samp_messages.push(SamplingMessage::user_text(&m.content));
                }
                Role::Assistant => {
                    samp_messages.push(SamplingMessage::assistant_text(&m.content));
                }
            }
        }
        // rmcp 1.7's struct literals are non-exhaustive across crate
        // boundaries; build via the typed constructors + builders.
        let preferences = ModelPreferences::new()
            .with_hints(vec![ModelHint::new("claude")])
            .with_intelligence_priority(0.7)
            .with_speed_priority(0.3)
            .with_cost_priority(0.4);
        let mut params = CreateMessageRequestParams::new(samp_messages, self.max_tokens)
            .with_model_preferences(preferences);
        if !system_parts.is_empty() {
            params = params.with_system_prompt(system_parts.join("\n\n"));
        }
        params
    }

    /// Build the audit `AuditEvent` carrying ONLY metadata. No raw
    /// prompt content lands in `details_json`.
    ///
    /// Pinned by [`tests::audit_row_omits_raw_prompt_text`].
    ///
    /// v0.9.1 P1 Fix 4 (F6 privacy bucketing): the raw character count
    /// of the prompt is itself a side-channel — a 6-char prompt
    /// uniquely identifies very-short refusal paths (e.g. a leaked
    /// password length). `prompt_chars` and `input_tokens_est` are
    /// rounded up to the next power of two before persistence. This
    /// preserves operator capacity-planning (the bucket is within ~2x
    /// of the real size for any sufficiently large prompt) while
    /// removing the per-character precision.
    ///
    /// Buckets: `0, 1, 2, 4, 8, 16, 32, 64, ..., 1024, 2048, ...`
    /// (next power of two `>= n`). 0 stays 0.
    fn audit_event(
        &self,
        params: &CreateMessageRequestParams,
        outcome: SamplingOutcome,
    ) -> AuditEvent {
        let raw_prompt_chars: usize = params
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|c| c.as_text().map(|t| t.text.len()))
            .sum::<usize>()
            + params.system_prompt.as_ref().map(|s| s.len()).unwrap_or(0);
        // v0.9.1 P1 Fix 4: bucket the raw count to the next power of
        // two. Pinned by `tests::audit_row_bucket_prompt_chars_to_pow2`.
        let prompt_chars = next_pow2_bucket(raw_prompt_chars);
        // ~4 chars per token for the rough English-text estimate used
        // by `solo doctor --check-llm` and Anthropic's docs. Recorded
        // for operator capacity-planning. Bucketed for the same
        // privacy reason — and to stay consistent with `prompt_chars`.
        let input_tokens_est = next_pow2_bucket(raw_prompt_chars / 4) as u64;
        let model_hint = params
            .model_preferences
            .as_ref()
            .and_then(|p| p.hints.as_ref())
            .and_then(|h| h.first())
            .and_then(|h| h.name.clone())
            .unwrap_or_else(|| "(none)".to_string());

        let mut details = serde_json::Map::new();
        details.insert(
            "model_hint".to_string(),
            serde_json::Value::String(model_hint),
        );
        details.insert(
            "messages_count".to_string(),
            serde_json::Value::Number(params.messages.len().into()),
        );
        details.insert(
            "max_tokens".to_string(),
            serde_json::Value::Number(params.max_tokens.into()),
        );
        details.insert(
            "prompt_chars".to_string(),
            serde_json::Value::Number(prompt_chars.into()),
        );
        details.insert(
            "input_tokens_est".to_string(),
            serde_json::Value::Number(input_tokens_est.into()),
        );

        let result = match &outcome {
            SamplingOutcome::Ok {
                duration_ms,
                model,
                output_chars,
            } => {
                // v0.9.1 P1 Fix 4: same power-of-2 bucketing as
                // `prompt_chars` for the output side. A model that
                // always replies with a one-token refusal (e.g. an
                // assistant trained to say "no.") would otherwise leak
                // the response-length distribution; bucketing
                // collapses 1-2-3-4 chars all into bucket 4.
                let bucketed_output_chars = next_pow2_bucket(*output_chars);
                let output_tokens_est = next_pow2_bucket(*output_chars / 4) as u64;
                details.insert(
                    "duration_ms".to_string(),
                    serde_json::Value::Number((*duration_ms).into()),
                );
                details.insert(
                    "model".to_string(),
                    serde_json::Value::String(model.clone()),
                );
                details.insert(
                    "output_chars".to_string(),
                    serde_json::Value::Number(bucketed_output_chars.into()),
                );
                details.insert(
                    "output_tokens_est".to_string(),
                    serde_json::Value::Number(output_tokens_est.into()),
                );
                AuditResult::Ok
            }
            SamplingOutcome::Forbidden {
                reason,
                duration_ms,
            } => {
                details.insert(
                    "duration_ms".to_string(),
                    serde_json::Value::Number((*duration_ms).into()),
                );
                details.insert(
                    "reason".to_string(),
                    serde_json::Value::String(reason.to_string()),
                );
                AuditResult::Forbidden
            }
            SamplingOutcome::Error {
                reason,
                duration_ms,
            } => {
                details.insert(
                    "duration_ms".to_string(),
                    serde_json::Value::Number((*duration_ms).into()),
                );
                details.insert(
                    "reason".to_string(),
                    serde_json::Value::String(reason.to_string()),
                );
                AuditResult::Error
            }
        };

        AuditEvent {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            principal_subject: self.audit_principal.clone(),
            operation: AuditOperation::LlmSamplingCall,
            target_id: None,
            result,
            details: Some(serde_json::Value::Object(details)),
        }
    }
}

/// Internal outcome category for the audit-row builder.
enum SamplingOutcome {
    Ok {
        duration_ms: u64,
        model: String,
        output_chars: usize,
    },
    Forbidden {
        reason: &'static str,
        duration_ms: u64,
    },
    Error {
        reason: &'static str,
        duration_ms: u64,
    },
}

#[async_trait]
impl LlmClient for SamplingLlmClient {
    fn name(&self) -> &str {
        "mcp-sampling"
    }

    async fn complete(&self, messages: &[Message]) -> CoreResult<Message> {
        let params = self.build_request(messages);
        let start = Instant::now();

        // Bounded wait on `peer.create_message`. The fold of (rmcp
        // ServiceError | FakeError | tokio timeout) into the
        // `Outcome` enum keeps the audit path single-sourced.
        let rpc = tokio::time::timeout(
            self.timeout,
            self.sampling_client.create_message(params.clone()),
        )
        .await;
        let duration_ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

        let (core_result, outcome): (CoreResult<Message>, SamplingOutcome) = match rpc {
            Ok(Ok(result)) => match extract_text(&result) {
                Ok(text) => {
                    let output_chars = text.len();
                    let outcome = SamplingOutcome::Ok {
                        duration_ms,
                        model: result.model.clone(),
                        output_chars,
                    };
                    (Ok(Message::assistant(text)), outcome)
                }
                Err(reason) => (
                    Err(CoreError::llm(format!(
                        "mcp sampling: malformed response: {reason}"
                    ))),
                    SamplingOutcome::Error {
                        reason: "malformed_response",
                        duration_ms,
                    },
                ),
            },
            Ok(Err(e)) => {
                let (category, is_forbidden) = e.classify();
                let outcome = if is_forbidden {
                    SamplingOutcome::Forbidden {
                        reason: category,
                        duration_ms,
                    }
                } else {
                    SamplingOutcome::Error {
                        reason: category,
                        duration_ms,
                    }
                };
                let err = if is_forbidden {
                    CoreError::forbidden(format!("mcp sampling: {e}"))
                } else {
                    CoreError::llm(format!("mcp sampling: {e}"))
                };
                (Err(err), outcome)
            }
            Err(_elapsed) => (
                Err(CoreError::llm(format!(
                    "mcp sampling: timeout after {duration_ms}ms"
                ))),
                SamplingOutcome::Error {
                    reason: "timeout",
                    duration_ms,
                },
            ),
        };

        // Synchronous audit emit through the writer-actor (lesson #30).
        // Failure to land the audit row is operator-visible: the
        // sampling call's caller sees the storage error and can decide
        // whether to abort (we DO abort here — without the audit row
        // we have no record of the call).
        //
        // v0.9.1 P1 Fix 3 (F4 Result-shadowing): when BOTH `core_result`
        // is `Err(..)` AND the audit emit also fails, return the
        // ORIGINAL LLM-side error (more actionable for callers — they
        // can retry the LLM call, or decide whether the upstream
        // refusal/timeout is recoverable). Surface the audit failure
        // via `tracing::error!` for operator visibility — operators
        // alarming on storage errors see it; callers see the actionable
        // error.
        //
        // Policy summary:
        //   * RPC Ok  + audit Ok  → return Ok(text)
        //   * RPC Ok  + audit Err → return Err(storage) [audit failure
        //                            wins — no undocumented sampling
        //                            calls per lesson #30]
        //   * RPC Err + audit Ok  → return Err(llm/forbidden) [unchanged]
        //   * RPC Err + audit Err → return Err(llm/forbidden) AND log
        //                            audit failure at error level
        //                            [v0.9.1 P1 Fix 3]
        let event = self.audit_event(&params, outcome);
        match (
            core_result,
            self.write_handle.emit_llm_sampling_audit(event).await,
        ) {
            (Ok(text), Ok(())) => Ok(text),
            (Ok(_text), Err(audit_err)) => {
                // RPC succeeded but the audit row didn't land. Drop
                // the success — without a durable audit row we can't
                // honor the "every sampling call leaves a trace"
                // invariant.
                Err(CoreError::storage(format!(
                    "mcp sampling: audit emit failed: {audit_err}"
                )))
            }
            (Err(core_err), Ok(())) => Err(core_err),
            (Err(core_err), Err(audit_err)) => {
                // Both failed. Return the LLM-side error (the caller's
                // most actionable signal); log the audit failure so an
                // operator who alarms on storage errors still sees it.
                tracing::error!(
                    audit_error = %audit_err,
                    core_error = %core_err,
                    "mcp sampling: audit emit failed alongside core \
                     error; surfacing core error to caller"
                );
                Err(core_err)
            }
        }
    }
}

/// Round `n` up to the next power of two. Used to bucket
/// `prompt_chars` / `output_chars` / `*_tokens_est` in the
/// `LlmSamplingCall` audit row's `details_json` (v0.9.1 P1 Fix 4
/// "F6" — `prompt_chars` was a privacy side-channel for short
/// prompts).
///
/// Buckets: `0 → 0`, `1 → 1`, `2 → 2`, `3 → 4`, `4 → 4`, `5..=8 → 8`,
/// `9..=16 → 16`, `17..=32 → 32`, ... — within a bucket all values
/// collapse to the same persisted number. The worst-case fidelity
/// loss is just under 2x (e.g. 9 chars persists as 16) which is well
/// within the precision capacity-planning needs.
///
/// Pinned by [`tests::next_pow2_bucket_*`] and
/// [`tests::audit_row_bucket_prompt_chars_to_pow2`].
fn next_pow2_bucket(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    // `next_power_of_two` saturates at `usize::MAX` if `n` is past the
    // last representable power. For our use (char counts on a Solo
    // prompt) the absolute upper bound is the LLM model's context
    // window — well below `usize::MAX` on every Solo-supported target.
    n.next_power_of_two()
}

/// Pull the assistant's text out of the rmcp result. Walks every text
/// content block in the message (the spec allows either a single
/// `SamplingContent::Single` or a `SamplingContent::Multiple`) and
/// concatenates them with newlines. Returns `Err(reason)` if no text
/// blocks were present — the malformed-response path.
fn extract_text(result: &CreateMessageResult) -> Result<String, &'static str> {
    if result.message.role != RmcpRole::Assistant {
        return Err("response role was not Assistant");
    }
    let mut out = String::new();
    for content in result.message.content.iter() {
        if let SamplingMessageContent::Text(text) = content {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    if out.is_empty() {
        Err("no text content blocks")
    } else {
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeMcpClient, FakeResponse, FakeSamplingError};
    use rmcp::model::CreateMessageResult;
    use solo_storage::{
        EmbedderConfig, HnswParams, InitParams, KeyMaterial, LibraryHandle, MemoryLibrary,
        MemoryLibraryParams, StubEmbedder, init, open_sqlcipher,
    };
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use zeroize::Zeroizing;

    const TEST_PASSPHRASE: &str = "v0.9.0-p2-sampling-tests";

    /// Bootstrap a per-tenant `LibraryHandle` whose writer-actor accepts
    /// the new `WriteCommand::EmitLlmSamplingAudit` variant.
    ///
    /// Mirrors the v0.8.x test discipline (see
    /// `crates/solo-storage/src/tenants/handle_registry_tests.rs`'s
    /// `fresh_init_dir`): build a real tenant DB on disk via the same
    /// `init()` helper users invoke, wrap in a `TenantRegistry`, and
    /// surface the `WriteHandle` for direct `SamplingLlmClient`
    /// wiring.
    struct Harness {
        _tmp: TempDir,
        _registry: Arc<MemoryLibrary>,
        _tenant: Arc<LibraryHandle>,
        write_handle: solo_storage::WriteHandle,
        db_path: PathBuf,
        key: KeyMaterial,
    }

    async fn harness() -> Harness {
        let tmp = TempDir::new().expect("tempdir");
        let data_dir = tmp.path().to_path_buf();
        let _ = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(TEST_PASSPHRASE.into()),
            force: false,
            embedder: EmbedderConfig {
                name: "stub".into(),
                version: "v1".into(),
                dim: 32,
                dtype: "f32".into(),
            },
        })
        .expect("init");

        let cfg =
            solo_storage::SoloConfig::read(&data_dir.join("solo.config.toml")).expect("read cfg");
        let key = KeyMaterial::derive(TEST_PASSPHRASE, &cfg.salt_bytes().expect("salt"))
            .expect("derive key");

        let embedder: Arc<dyn solo_core::Embedder> = Arc::new(StubEmbedder::new("stub", "v1", 32));
        let registry = Arc::new(
            MemoryLibrary::open(MemoryLibraryParams {
                data_dir: data_dir.clone(),
                key: key.clone(),
                embedder: embedder.clone(),
                hnsw_params: HnswParams::default(),
                steward: None,
                runtime_handle: Some(tokio::runtime::Handle::current()),
                steward_factory: None,
                triples_batch_signal: None,
            })
            .expect("open registry"),
        );

        let tenant = registry.handle().await.expect("open Community library");
        let write_handle = tenant.write().clone();
        let db_path = tenant.db_path().to_path_buf();

        Harness {
            _tmp: tmp,
            _registry: registry,
            _tenant: tenant,
            write_handle,
            db_path,
            key,
        }
    }

    /// Helper: count the `audit_events` rows whose `operation` is the
    /// given string. Opens a fresh connection to the tenant DB so we
    /// avoid contention with the writer-actor's own connection.
    fn count_audit_rows(db_path: &std::path::Path, key: &KeyMaterial, op: &str) -> i64 {
        let conn = open_sqlcipher(db_path, key).expect("open db");
        conn.query_row(
            "SELECT COUNT(*) FROM audit_events WHERE operation = ?",
            rusqlite::params![op],
            |r| r.get(0),
        )
        .expect("count")
    }

    /// Helper: load the most-recent `llm.sampling_call` audit row and
    /// return `(result, principal_subject, details_json)`.
    fn latest_sampling_audit_details(
        db_path: &std::path::Path,
        key: &KeyMaterial,
    ) -> (String, Option<String>, serde_json::Value) {
        let conn = open_sqlcipher(db_path, key).expect("open db");
        let (result, principal, details_str): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT result, principal_subject, details_json
                 FROM audit_events
                 WHERE operation = 'llm.sampling_call'
                 ORDER BY ts_ms DESC, rowid DESC
                 LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("query");
        let details: serde_json::Value =
            serde_json::from_str(&details_str.expect("details_json present"))
                .expect("parse details");
        (result, principal, details)
    }

    /// Happy path: a successful `create_message` round-trip returns
    /// the assistant text wrapped in a `Message::assistant`, and lands
    /// exactly one `llm.sampling_call` audit row with `result = 'ok'`.
    #[tokio::test]
    async fn sampling_complete_happy_path_returns_text() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("derived theme")));
        let client = SamplingLlmClient::with_sampling_client(
            fake.clone(),
            h.write_handle.clone(),
            Some("alice".into()),
        );
        let messages = vec![Message::user("summarise these episodes")];
        let result = client.complete(&messages).await.expect("ok");
        assert_eq!(result.role, Role::Assistant);
        assert_eq!(result.content, "derived theme");

        // Exactly one audit row landed.
        assert_eq!(count_audit_rows(&h.db_path, &h.key, "llm.sampling_call"), 1);
        let (result_str, principal, details) = latest_sampling_audit_details(&h.db_path, &h.key);
        assert_eq!(result_str, "ok");
        assert_eq!(principal.as_deref(), Some("alice"));
        assert_eq!(details["model_hint"], "claude");
        assert_eq!(details["model"], "fake-claude");
        assert_eq!(details["messages_count"], 1);
        assert_eq!(details["max_tokens"], 512);
    }

    /// Privacy invariant: the audit row's `details_json` MUST NOT
    /// contain the raw prompt content. Pinned by string inspection of
    /// the persisted JSON.
    #[tokio::test]
    async fn audit_row_omits_raw_prompt_text() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client = SamplingLlmClient::with_sampling_client(fake, h.write_handle.clone(), None);
        let secret = "THE-USER-ID-IS-bobby-1234";
        let messages = vec![
            Message::system("you are a friendly assistant"),
            Message::user(secret),
        ];
        client.complete(&messages).await.expect("ok");

        let (_, _, details) = latest_sampling_audit_details(&h.db_path, &h.key);
        let serialised = serde_json::to_string(&details).expect("serialise details");
        assert!(
            !serialised.contains(secret),
            "audit details must not carry raw prompt content; was: {serialised}"
        );
        assert!(
            !serialised.contains("you are a friendly assistant"),
            "audit details must not carry system prompt; was: {serialised}"
        );
        // Metadata IS present, even though the prompt is not.
        assert_eq!(details["messages_count"], 1);
        assert!(details["prompt_chars"].as_u64().unwrap() > 0);
    }

    /// v0.9.1 P1 Fix 4 (F6 privacy bucketing): the audit row's
    /// `prompt_chars` MUST be the power-of-2 bucket, never the raw
    /// character count. Pins the bucketing behavior end-to-end (raw
    /// `audit_event` → SQLite → re-read).
    ///
    /// Test recipe: drive a prompt with a known raw length (6 chars
    /// total, `"hello "` system + `"x"` user → 6+1 = 7) and assert the
    /// audit row carries `8` (next pow2 ≥ 7), not 7.
    #[tokio::test]
    async fn audit_row_bucket_prompt_chars_to_pow2() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client = SamplingLlmClient::with_sampling_client(fake, h.write_handle.clone(), None);
        // System: 6 chars + user: 1 char = 7 chars raw → bucket 8.
        client
            .complete(&[Message::system("hello "), Message::user("x")])
            .await
            .expect("ok");
        let (_, _, details) = latest_sampling_audit_details(&h.db_path, &h.key);
        assert_eq!(
            details["prompt_chars"].as_u64().unwrap(),
            8,
            "prompt_chars must be bucketed to next pow2 (7 → 8). \
             raw count is a privacy side-channel; see Fix 4 F6 in \
             v0.9.1 P1 dev log. got details={details}"
        );
    }

    /// Stability invariant: two prompts that fall in the SAME bucket
    /// must persist identical `prompt_chars`. Distinguishes "the
    /// implementation buckets" from "the implementation hashes/leaks
    /// raw values".
    ///
    /// 5 chars and 7 chars both round to 8 → must persist identically.
    /// (Mirrors the brief's "test that bucketed values are stable
    /// across exact-character variations within the same bucket".)
    #[tokio::test]
    async fn audit_row_bucket_prompt_chars_is_stable_within_bucket() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client = SamplingLlmClient::with_sampling_client(fake, h.write_handle.clone(), None);
        // 5 chars raw → bucket 8.
        client
            .complete(&[Message::user("hello")])
            .await
            .expect("ok");
        let (_, _, details_5) = latest_sampling_audit_details(&h.db_path, &h.key);
        // 7 chars raw → bucket 8.
        client
            .complete(&[Message::user("hellooo")])
            .await
            .expect("ok");
        let (_, _, details_7) = latest_sampling_audit_details(&h.db_path, &h.key);
        assert_eq!(
            details_5["prompt_chars"], details_7["prompt_chars"],
            "5 chars and 7 chars must hash to the same bucket (8) — \
             otherwise the bucketing is leaking raw fidelity. \
             5-char details: {details_5}, 7-char details: {details_7}"
        );
        assert_eq!(details_5["prompt_chars"].as_u64().unwrap(), 8);
    }

    /// Unit-level pins for the bucketing helper. Catches a regression
    /// where someone "simplifies" `next_pow2_bucket` into a no-op.
    #[test]
    fn next_pow2_bucket_table() {
        assert_eq!(next_pow2_bucket(0), 0, "0 stays 0");
        assert_eq!(next_pow2_bucket(1), 1, "1 stays 1");
        assert_eq!(next_pow2_bucket(2), 2, "2 stays 2");
        assert_eq!(next_pow2_bucket(3), 4, "3 rounds up to 4");
        assert_eq!(next_pow2_bucket(4), 4, "4 stays 4");
        assert_eq!(next_pow2_bucket(5), 8);
        assert_eq!(next_pow2_bucket(6), 8, "6-char prompt (brief case) → 8");
        assert_eq!(next_pow2_bucket(7), 8);
        assert_eq!(next_pow2_bucket(8), 8);
        assert_eq!(next_pow2_bucket(9), 16);
        assert_eq!(next_pow2_bucket(1023), 1024);
        assert_eq!(next_pow2_bucket(1024), 1024);
        assert_eq!(next_pow2_bucket(1025), 2048);
    }

    /// Client refusal: maps to `CoreError::Forbidden` + audit row
    /// `result = 'forbidden'` + `details_json.reason = 'client_refused'`.
    #[tokio::test]
    async fn client_refusal_returns_forbidden_and_audits() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ignored")));
        fake.reject_with("user dismissed approval");
        let client = SamplingLlmClient::with_sampling_client(
            fake,
            h.write_handle.clone(),
            Some("alice".into()),
        );
        let err = client
            .complete(&[Message::user("anything")])
            .await
            .unwrap_err();
        match err {
            CoreError::Forbidden(_) => {}
            other => panic!("expected Forbidden, got {other:?}"),
        }
        let (result_str, _, details) = latest_sampling_audit_details(&h.db_path, &h.key);
        assert_eq!(result_str, "forbidden");
        assert_eq!(details["reason"], "client_refused");
    }

    /// Timeout: tokio::time::timeout fires before the fake's `Slow`
    /// response resolves; client returns `CoreError::Llm` + audit row
    /// `result = 'error'` + `details_json.reason = 'timeout'`.
    ///
    /// Real wall-clock: 80ms slow response vs 30ms client timeout.
    /// Margin is loose enough for slow CI without making the test
    /// drag.
    #[tokio::test]
    async fn timeout_returns_error_with_timeout_reason() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::slow(
            "late",
            Duration::from_millis(800),
        )));
        let client = SamplingLlmClient::with_sampling_client(fake, h.write_handle.clone(), None)
            .with_timeout(Duration::from_millis(30));
        let err = client
            .complete(&[Message::user("hello")])
            .await
            .unwrap_err();
        match err {
            CoreError::Llm(msg) => assert!(msg.contains("timeout")),
            other => panic!("expected Llm, got {other:?}"),
        }
        let (result_str, _, details) = latest_sampling_audit_details(&h.db_path, &h.key);
        assert_eq!(result_str, "error");
        assert_eq!(details["reason"], "timeout");
    }

    /// Malformed response: the fake returns a result with zero text
    /// content blocks; client surfaces `CoreError::Llm` + audit row
    /// `result = 'error'` + `details_json.reason = 'malformed_response'`.
    #[tokio::test]
    async fn malformed_response_returns_error_with_reason() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::EmptyContent));
        let client = SamplingLlmClient::with_sampling_client(fake, h.write_handle.clone(), None);
        let err = client.complete(&[Message::user("hi")]).await.unwrap_err();
        assert!(matches!(err, CoreError::Llm(_)));
        let (result_str, _, details) = latest_sampling_audit_details(&h.db_path, &h.key);
        assert_eq!(result_str, "error");
        assert_eq!(details["reason"], "malformed_response");
    }

    /// `principal_subject = None` works — audit row still emits with
    /// NULL.
    #[tokio::test]
    async fn no_principal_emits_audit_with_null_principal() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client = SamplingLlmClient::with_sampling_client(fake, h.write_handle.clone(), None);
        client.complete(&[Message::user("hi")]).await.expect("ok");
        let (_, principal, _) = latest_sampling_audit_details(&h.db_path, &h.key);
        assert_eq!(principal, None);
    }

    /// Concurrency: 8 parallel `complete()` calls land 8 audit rows.
    /// Audit IDs (autoincrement rowid) must be distinct — verifies the
    /// writer-actor serialises the per-call audit emit (no
    /// interleaving / dropped rows).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn parallel_completes_serialise_audit_rows() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client = SamplingLlmClient::with_sampling_client(
            fake.clone(),
            h.write_handle.clone(),
            Some("alice".into()),
        );
        let mut futs = Vec::new();
        for _ in 0..8 {
            let c = client.clone();
            futs.push(tokio::spawn(async move {
                c.complete(&[Message::user("hi")]).await
            }));
        }
        for f in futs {
            f.await.expect("join").expect("ok");
        }
        assert_eq!(
            count_audit_rows(&h.db_path, &h.key, "llm.sampling_call"),
            8,
            "8 parallel calls must land 8 audit rows"
        );

        // Each was a separate request to the fake.
        assert_eq!(fake.record_requests().len(), 8);
    }

    /// `complete` translates the workspace's `Message::system` into the
    /// `system_prompt` top-level field; user/assistant roles map to
    /// rmcp's `SamplingMessage::user_text` / `assistant_text`.
    #[tokio::test]
    async fn build_request_splits_system_from_messages() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client =
            SamplingLlmClient::with_sampling_client(fake.clone(), h.write_handle.clone(), None);
        client
            .complete(&[
                Message::system("be terse"),
                Message::user("question"),
                Message::assistant("answer"),
            ])
            .await
            .expect("ok");
        let recorded = fake.record_requests();
        assert_eq!(recorded.len(), 1);
        let req = &recorded[0];
        assert_eq!(
            req.system_prompt.as_deref(),
            Some("be terse"),
            "Role::System must map to system_prompt"
        );
        assert_eq!(req.messages.len(), 2);
        // The remaining two messages are the user + assistant turns.
        assert_eq!(req.messages[0].role, RmcpRole::User);
        assert_eq!(req.messages[1].role, RmcpRole::Assistant);
    }

    /// `model_preferences` carries the `claude` hint per plan §6.
    /// Pins the wire shape so a future change is a conscious decision.
    #[tokio::test]
    async fn build_request_includes_claude_model_hint() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client =
            SamplingLlmClient::with_sampling_client(fake.clone(), h.write_handle.clone(), None);
        client.complete(&[Message::user("hi")]).await.expect("ok");
        let recorded = fake.record_requests();
        let prefs = recorded[0].model_preferences.as_ref().expect("prefs");
        let hint = prefs
            .hints
            .as_ref()
            .and_then(|h| h.first())
            .and_then(|h| h.name.clone())
            .expect("hint name");
        assert_eq!(hint, "claude");
    }

    /// `with_max_tokens(n)` propagates to the request's
    /// `max_tokens` field.
    #[tokio::test]
    async fn with_max_tokens_overrides_default() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client =
            SamplingLlmClient::with_sampling_client(fake.clone(), h.write_handle.clone(), None)
                .with_max_tokens(2048);
        client.complete(&[Message::user("hi")]).await.expect("ok");
        let recorded = fake.record_requests();
        assert_eq!(recorded[0].max_tokens, 2048);
    }

    /// Reconfiguring the fake mid-test produces distinct audit rows
    /// for each call (positive then negative).
    #[tokio::test]
    async fn reconfigurable_fake_distinguishes_audit_rows() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let client = SamplingLlmClient::with_sampling_client(
            fake.clone(),
            h.write_handle.clone(),
            Some("alice".into()),
        );

        client.complete(&[Message::user("a")]).await.expect("ok");
        fake.reject_with("user said no");
        let _ = client.complete(&[Message::user("b")]).await;

        let conn = open_sqlcipher(&h.db_path, &h.key).expect("open");
        let mut stmt = conn
            .prepare(
                "SELECT result FROM audit_events WHERE operation = 'llm.sampling_call' ORDER BY ts_ms ASC, rowid ASC",
            )
            .expect("prepare");
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect();
        assert_eq!(rows, vec!["ok".to_string(), "forbidden".to_string()]);
    }

    /// `extract_text` walks single-block content.
    #[test]
    fn extract_text_pulls_text_from_single_block() {
        let result =
            CreateMessageResult::new(SamplingMessage::assistant_text("hello"), "fake".into());
        assert_eq!(extract_text(&result).unwrap(), "hello");
    }

    /// `extract_text` rejects an empty-content response.
    #[test]
    fn extract_text_rejects_empty_content() {
        let result = CreateMessageResult::new(
            SamplingMessage::new_multiple(RmcpRole::Assistant, Vec::new()),
            "fake".into(),
        );
        assert!(extract_text(&result).is_err());
    }

    /// `extract_text` rejects a User-role response (impossible per
    /// spec but pinning the defensive check).
    #[test]
    fn extract_text_rejects_non_assistant_role() {
        let result = CreateMessageResult::new(SamplingMessage::user_text("hello"), "fake".into());
        assert!(extract_text(&result).is_err());
    }

    // ---- v0.10.1 F5 audit-minor closure: pin multi-block concat
    //      semantics (deferred from v0.9.0 P2). ----
    //
    // `extract_text` walks `result.message.content` and pushes a `\n`
    // between successive `SamplingMessageContent::Text` blocks. The
    // exact semantics are wire-shape decisions that downstream Steward
    // parsers depend on; we pin them here so a future refactor can't
    // drift silently. The audit minor (v0.9.0 P2 §F5) flagged that the
    // existing tests only exercised single-block + empty + non-
    // assistant cases — not the multi-block join behavior. Closure
    // checklist:
    //
    //   1. join inserts ONE `\n` between non-trailing-newline blocks
    //   2. trailing `\n` in a block is preserved (so a `"abc\n"` block
    //      + a `"def"` block produces `"abc\n\ndef"`)
    //   3. single block returns verbatim (no leading/trailing newline
    //      added)
    //
    // These tests are intentionally identical-output to what the code
    // produces today. They're a regression net, not a behavior change.
    //
    // Grep terms: F5, extract_text_joins_multi_block, extract_text_preserves_trailing_newlines.

    /// F5 pin: a two-block message joins with exactly ONE `\n` between
    /// blocks when neither block ends in a newline.
    #[test]
    fn extract_text_joins_multi_block_with_newline_separator() {
        let blocks = vec![
            SamplingMessageContent::text("abc"),
            SamplingMessageContent::text("def"),
        ];
        let result = CreateMessageResult::new(
            SamplingMessage::new_multiple(RmcpRole::Assistant, blocks),
            "fake".into(),
        );
        // Exact value pinned: "abc" + "\n" + "def".
        assert_eq!(
            extract_text(&result).unwrap(),
            "abc\ndef",
            "two non-newline-terminated blocks must join with a single newline"
        );
    }

    /// F5 pin: a trailing newline in a content block is preserved
    /// verbatim AND a join newline is still inserted, so the result
    /// contains `\n\n` between such a block and the next. This is the
    /// "honest current behavior" pin — a future refactor that strips
    /// trailing newlines must explicitly update this test.
    #[test]
    fn extract_text_preserves_trailing_newlines_in_blocks() {
        let blocks = vec![
            SamplingMessageContent::text("abc\n"),
            SamplingMessageContent::text("def"),
        ];
        let result = CreateMessageResult::new(
            SamplingMessage::new_multiple(RmcpRole::Assistant, blocks),
            "fake".into(),
        );
        assert_eq!(
            extract_text(&result).unwrap(),
            "abc\n\ndef",
            "trailing newline in block 1 + join newline => '\\n\\n' between blocks"
        );
    }

    /// F5 pin: a single block returns verbatim — no leading or
    /// trailing newline added by the helper. Already covered by
    /// `extract_text_pulls_text_from_single_block` for the "hello"
    /// case; this variant pins the negative space for inputs with
    /// internal newlines (the helper does NOT mutate inner whitespace).
    #[test]
    fn extract_text_single_block_returns_verbatim_including_inner_newlines() {
        let blocks = vec![SamplingMessageContent::text("line1\nline2")];
        let result = CreateMessageResult::new(
            SamplingMessage::new_multiple(RmcpRole::Assistant, blocks),
            "fake".into(),
        );
        assert_eq!(
            extract_text(&result).unwrap(),
            "line1\nline2",
            "single block must return verbatim, no extra newlines added"
        );
    }

    /// F5 pin: three blocks, second is empty-string. The first block
    /// emits its text + `\n`; the empty middle pushes nothing but
    /// `out.is_empty()` is false so the NEXT iteration's pre-newline
    /// fires again. Result: `"a\n\nb"`. This pins the "empty middle
    /// block adds a blank line" semantic — surprising at first read
    /// but consistent with the join-between-non-empty rule applied
    /// uniformly to every iteration.
    #[test]
    fn extract_text_empty_middle_block_inserts_blank_line() {
        let blocks = vec![
            SamplingMessageContent::text("a"),
            SamplingMessageContent::text(""),
            SamplingMessageContent::text("b"),
        ];
        let result = CreateMessageResult::new(
            SamplingMessage::new_multiple(RmcpRole::Assistant, blocks),
            "fake".into(),
        );
        // The implementation's exact behavior: after "a" is pushed,
        // out = "a". For block 2 (empty): out.is_empty() is false, so
        // push '\n' → out = "a\n"; then push_str("") → out = "a\n".
        // For block 3 ("b"): out.is_empty() is false, so push '\n' →
        // out = "a\n\n"; then push_str("b") → out = "a\n\nb".
        assert_eq!(
            extract_text(&result).unwrap(),
            "a\n\nb",
            "empty middle block leaves a blank line between non-empty blocks"
        );
    }

    /// `SamplingError::classify` maps each fake variant to the right
    /// audit category.
    #[test]
    fn sampling_error_classify_maps_fake_variants() {
        let refused = SamplingError::Fake(FakeSamplingError::Refused { reason: "x".into() });
        let (cat, forb) = refused.classify();
        assert_eq!(cat, "client_refused");
        assert!(forb);

        let transport = SamplingError::Fake(FakeSamplingError::Transport {
            message: "x".into(),
        });
        let (cat, forb) = transport.classify();
        assert_eq!(cat, "transport_error");
        assert!(!forb);

        let malformed = SamplingError::Fake(FakeSamplingError::MalformedResponse {
            message: "x".into(),
        });
        let (cat, forb) = malformed.classify();
        assert_eq!(cat, "malformed_response");
        assert!(!forb);
    }

    // -------- v0.9.0 P5a (M3 wiring) — SamplingCoordinator integration --------
    //
    // These tests pin the contract that `build_sampling_steward` wraps the
    // live peer in a `SamplingCoordinator` before handing it to
    // `SamplingLlmClient`. They cannot call `build_sampling_steward`
    // directly (it takes a real `Peer<RoleServer>` whose constructors are
    // private inside rmcp), but they exercise the **exact same wiring
    // shape** by substituting `FakeMcpClient` for `PeerSamplingClient`.
    // The production code path is:
    //
    //     PeerSamplingClient -> SamplingCoordinator -> SamplingLlmClient
    //
    // The tested shape is:
    //
    //     FakeMcpClient      -> SamplingCoordinator -> SamplingLlmClient
    //
    // Only the leaf `SamplingClient` impl differs; the
    // `SamplingClient` trait is the same Arc-of-dyn in both paths.

    /// SamplingCoordinator wrapping a `FakeMcpClient` and feeding
    /// `SamplingLlmClient::with_sampling_client` is the same Arc-of-dyn
    /// shape `build_sampling_steward` constructs at MCP-initialize
    /// time. Single-element flushes pass through unwrapped, so a lone
    /// `complete()` call still emits one audit row and produces the
    /// expected text.
    #[tokio::test]
    async fn sampling_llm_client_uses_coordinator_in_production_path() {
        let h = harness().await;
        let fake: Arc<dyn SamplingClient> = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let coord: Arc<dyn SamplingClient> = super::super::SamplingCoordinator::with_settings(
            fake.clone(),
            Duration::from_millis(50),
            10,
        );
        let client = SamplingLlmClient::with_sampling_client(
            coord,
            h.write_handle.clone(),
            Some("alice".into()),
        );
        let result = client.complete(&[Message::user("test")]).await.expect("ok");
        assert_eq!(result.role, Role::Assistant);
        assert_eq!(result.content, "ok");
        // Single audit row landed — per-call audit semantics
        // unchanged by the coordinator wrap.
        assert_eq!(
            count_audit_rows(&h.db_path, &h.key, "llm.sampling_call"),
            1,
            "one logical call → one audit row, even through coordinator"
        );
    }

    /// End-to-end batching pin: N concurrent `complete()` calls within
    /// the coalesce window resolve as ONE inner `create_message` RPC
    /// on the underlying `FakeMcpClient`, but N audit rows still land
    /// (one per logical call — the privacy + audit invariants from P2
    /// hold).
    ///
    /// This is the v0.9.0 release notes' "⌈N/M⌉ peer.create_message
    /// calls per coalesce window" claim, exercised through the same
    /// trait-object chain that `build_sampling_steward` constructs in
    /// production.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn coordinator_coalesces_concurrent_calls_into_one_inner_rpc() {
        // Coalesced JSON response for 5 tasks — matches the
        // `[{task_index, response}]` shape `flush_batch` demuxes
        // multi-element batches into.
        let response = serde_json::to_string(
            &(0..5)
                .map(|i| {
                    serde_json::json!({
                        "task_index": i,
                        "response": format!("response-{i}"),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();

        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text(&response)));
        let coord: Arc<dyn SamplingClient> = super::super::SamplingCoordinator::with_settings(
            fake.clone(),
            // Wide window so all 5 submissions land in one batch.
            Duration::from_secs(5),
            10,
        );
        let client = SamplingLlmClient::with_sampling_client(
            coord,
            h.write_handle.clone(),
            Some("alice".into()),
        );

        // Fire 5 concurrent `complete()` calls; the coordinator should
        // coalesce them into ONE `FakeMcpClient::create_message` call.
        let mut futs = Vec::new();
        for i in 0..5 {
            let c = client.clone();
            futs.push(tokio::spawn(async move {
                c.complete(&[Message::user(format!("task-{i}"))]).await
            }));
        }
        for f in futs {
            f.await.expect("join").expect("ok");
        }

        // EXACTLY one inner RPC.
        assert_eq!(
            fake.record_requests().len(),
            1,
            "5 logical calls within window must coalesce to 1 inner RPC"
        );
        // BUT 5 audit rows — per-logical-call audit invariant preserved.
        assert_eq!(
            count_audit_rows(&h.db_path, &h.key, "llm.sampling_call"),
            5,
            "5 logical calls → 5 audit rows (coordinator doesn't merge audits)"
        );
    }

    /// Edge case: `coalesce_max_requests = 1` reduces the coordinator
    /// to pass-through (each submit flushes a 1-element batch
    /// immediately). With max_batch=1 and a wide window, 3 concurrent
    /// calls land 3 inner RPCs — coordinator is operating as if no
    /// batching were configured.
    ///
    /// Pins the brief's documented edge-case: zero / one-valued config
    /// reduces to pass-through, never panics or deadlocks. Mirrors
    /// `SamplingCoordinator::with_settings`'s `max_batch.max(1)`
    /// clamping for the `coalesce_max_requests = 0` case.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn coordinator_max_batch_one_acts_as_passthrough() {
        let h = harness().await;
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("ok")));
        let coord: Arc<dyn SamplingClient> = super::super::SamplingCoordinator::with_settings(
            fake.clone(),
            Duration::from_secs(5),
            // max_batch=1 → every submission flushes immediately as
            // a 1-element batch; pass-through behaviour.
            1,
        );
        let client = SamplingLlmClient::with_sampling_client(coord, h.write_handle.clone(), None);
        let mut futs = Vec::new();
        for _ in 0..3 {
            let c = client.clone();
            futs.push(tokio::spawn(async move {
                c.complete(&[Message::user("hi")]).await
            }));
        }
        for f in futs {
            f.await.expect("join").expect("ok");
        }
        // 3 logical calls → 3 inner RPCs (no coalescing).
        assert_eq!(
            fake.record_requests().len(),
            3,
            "max_batch=1 must pass through every submission as its own RPC"
        );
        assert_eq!(count_audit_rows(&h.db_path, &h.key, "llm.sampling_call"), 3);
    }
}
