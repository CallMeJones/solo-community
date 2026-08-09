// SPDX-License-Identifier: Apache-2.0

//! [`SamplingCoordinator`] — coalesces N concurrent
//! [`SamplingClient::create_message`] calls into ⌈N/M⌉ calls within a
//! configurable time window or batch-size limit.
//!
//! Per the v0.9.0 design (`docs/dev-log/0098-v0.9.0-implementation-plan.md`
//! §4 P4 / Risk #7 / MAJOR 3 batching resolution):
//!
//! ## Why batch?
//!
//! Each `sampling/createMessage` call surfaces ONE approval prompt
//! in the user's MCP client (Claude Desktop / Claude Code / future
//! clients). When the daemon-side consolidate-timer fires
//! [`solo_storage::triples_batch::run_triples_batch_tick`], it
//! can produce N per-cluster sampling calls in quick succession —
//! N separate approval prompts spam the user.
//!
//! `SamplingCoordinator` collapses N calls within a `window` window
//! into ONE coalesced `peer.create_message` call. The user sees ONE
//! approval per coalesce window; the per-cluster results are
//! demultiplexed back to the individual callers via their oneshot
//! reply channels.
//!
//! ## When NOT to batch
//!
//! Coordinator is bypassed for non-sampling backends (Anthropic /
//! Ollama / None) — those don't surface approval prompts and have
//! their own rate-limiting concerns. The coordinator inserts itself
//! ONLY when wrapping [`PeerSamplingClient`] / a fake equivalent.
//!
//! ## Coalesce strategy
//!
//! - **Single-request batch**: passes through as a normal
//!   `create_message` call, with no prompt rewriting. Zero behaviour
//!   change from the v0.9.0 P2 path.
//! - **Multi-request batch (N > 1)**: wraps each request in a
//!   numbered JSON object, asks the LLM for a JSON array of
//!   responses, parses the array, demultiplexes per-task. The
//!   prompt template is documented in [`build_coalesced_request`].
//!
//! ## Privacy invariant
//!
//! The audit emit per logical request (one
//! `AuditOperation::LlmSamplingCall` row per submitted
//! [`SamplingLlmClient::complete`]) STAYS — the coordinator is an
//! optimisation on the wire, NOT a change to the audit shape. See
//! plan §11 Risk #8 — operators MUST be able to count per-logical-call
//! audit rows, not per-coalesce.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{
    CreateMessageRequestParams, CreateMessageResult, Role as RmcpRole, SamplingMessage,
    SamplingMessageContent,
};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::llm::sampling::{SamplingClient, SamplingError};

/// Default coalesce window: 5 seconds. Plan §4 P4c default —
/// chosen so the user's approval-prompt latency stays under typical
/// MCP-session "I'm doing work" tolerance.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(5000);

/// Default max-batch size: 10 logical requests per coalesced
/// `create_message`. Plan §4 P4c default — caps the rendered prompt
/// size + prevents one slow batch from holding the worker indefinitely.
pub const DEFAULT_COALESCE_MAX_BATCH: usize = 10;

/// Wrapper around a [`SamplingClient`] that coalesces concurrent
/// `create_message` calls into batched `create_message` calls
/// (within a configurable time window OR batch-size limit).
///
/// Construct via [`SamplingCoordinator::new`] or
/// [`SamplingCoordinator::with_settings`]. Drop the coordinator's
/// last `Arc` clone to shut down the worker task.
///
/// **Thread safety**: cheap to clone; every clone shares the same
/// underlying mpsc + worker task. Concurrent callers are serialised
/// by the worker.
pub struct SamplingCoordinator {
    /// Send-side of the worker mpsc. Cloning the coordinator clones
    /// this sender; dropping every clone closes the channel and the
    /// worker exits.
    tx: mpsc::Sender<Submission>,
    /// JoinHandle for the worker task. Stored in a Mutex so
    /// `shutdown_blocking` can `.take()` it on first call.
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl SamplingCoordinator {
    /// Build a coordinator wrapping the supplied [`SamplingClient`]
    /// with default settings (5s window, max-batch 10).
    pub fn new(inner: Arc<dyn SamplingClient>) -> Arc<Self> {
        Self::with_settings(inner, DEFAULT_COALESCE_WINDOW, DEFAULT_COALESCE_MAX_BATCH)
    }

    /// Build a coordinator with explicit settings. `window` is the
    /// upper bound the worker waits before flushing a non-empty
    /// buffer; `max_batch` is the buffer size that triggers an
    /// immediate flush regardless of `window`.
    pub fn with_settings(
        inner: Arc<dyn SamplingClient>,
        window: Duration,
        max_batch: usize,
    ) -> Arc<Self> {
        let (tx, rx) = mpsc::channel::<Submission>(max_batch.max(1) * 2 + 16);
        let worker = tokio::spawn(coordinator_worker(rx, inner, window, max_batch.max(1)));
        Arc::new(Self {
            tx,
            worker: Mutex::new(Some(worker)),
        })
    }

    /// Coalesced equivalent of `SamplingClient::create_message`. The
    /// returned future resolves when the worker has demultiplexed the
    /// coalesced batch's response back to this submission's slot.
    pub async fn submit(
        &self,
        params: CreateMessageRequestParams,
    ) -> Result<CreateMessageResult, SamplingError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Submission {
                params,
                reply: reply_tx,
            })
            .await
            .map_err(|_| {
                SamplingError::Service(rmcp::service::ServiceError::McpError(
                    rmcp::model::ErrorData::internal_error(
                        "sampling coordinator worker is gone (channel closed)",
                        None,
                    ),
                ))
            })?;
        reply_rx.await.map_err(|_| {
            SamplingError::Service(rmcp::service::ServiceError::McpError(
                rmcp::model::ErrorData::internal_error(
                    "sampling coordinator worker dropped reply channel",
                    None,
                ),
            ))
        })?
    }

    /// Drain the worker task. Called rarely in tests; production
    /// drops the coordinator on daemon shutdown.
    pub async fn shutdown(self: Arc<Self>) {
        // Drop the send-side to close the channel.
        let mut guard = self.worker.lock().await;
        if let Some(join) = guard.take() {
            join.abort();
            let _ = join.await;
        }
    }
}

/// One coordinator submission: the caller's `create_message` params
/// + the oneshot we send the demultiplexed reply back on.
struct Submission {
    params: CreateMessageRequestParams,
    reply: oneshot::Sender<Result<CreateMessageResult, SamplingError>>,
}

/// Worker task: loops, draining `rx` into batches bounded by
/// `window` (time) or `max_batch` (count), and dispatches each
/// batch to `inner` as ONE `create_message` call.
async fn coordinator_worker(
    mut rx: mpsc::Receiver<Submission>,
    inner: Arc<dyn SamplingClient>,
    window: Duration,
    max_batch: usize,
) {
    loop {
        // Block until at least one submission arrives or the
        // channel is closed (last sender dropped).
        let first = match rx.recv().await {
            Some(s) => s,
            None => return,
        };
        let mut buffer: Vec<Submission> = vec![first];

        // Drain additional submissions for up to `window` ms, or
        // until `max_batch` reached. `tokio::time::timeout(window,
        // rx.recv())` returns Ok(Some(_)) on incoming submission,
        // Ok(None) on channel close, Err(_) on timeout.
        let deadline = tokio::time::Instant::now() + window;
        while buffer.len() < max_batch {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(s)) => buffer.push(s),
                Ok(None) => {
                    // Channel closed; flush this last batch and exit.
                    flush_batch(&inner, buffer).await;
                    return;
                }
                Err(_) => break,
            }
        }

        flush_batch(&inner, buffer).await;
    }
}

/// Dispatch one batch. Single-element batches pass through as a
/// normal `create_message`; multi-element batches go through the
/// coalesced JSON-array prompt path.
async fn flush_batch(inner: &Arc<dyn SamplingClient>, batch: Vec<Submission>) {
    if batch.is_empty() {
        return;
    }
    if batch.len() == 1 {
        // Pass-through. Zero behaviour change from the
        // unwrapped-coordinator path.
        let mut iter = batch.into_iter();
        let s = iter.next().unwrap();
        let result = inner.create_message(s.params).await;
        let _ = s.reply.send(result);
        return;
    }

    let coalesced = build_coalesced_request(&batch);
    let result = inner.create_message(coalesced).await;

    match result {
        Ok(rendered) => {
            // Try to demultiplex the rendered JSON array back into
            // per-task results. If parsing fails, surface the
            // error to EVERY submission — the caller will see a
            // structured `malformed_response` per audit row.
            match demux_coalesced(&rendered, &batch) {
                Ok(per_task) => {
                    for (sub, task_result) in batch.into_iter().zip(per_task) {
                        let _ = sub.reply.send(task_result);
                    }
                }
                Err(parse_err) => {
                    let err_msg = format!(
                        "sampling coordinator: failed to parse coalesced response: {parse_err}"
                    );
                    for sub in batch {
                        let _ = sub.reply.send(Err(SamplingError::Service(
                            rmcp::service::ServiceError::McpError(
                                rmcp::model::ErrorData::internal_error(err_msg.clone(), None),
                            ),
                        )));
                    }
                }
            }
        }
        Err(e) => {
            // The single coalesced RPC failed. Surface the failure
            // to EVERY submission so per-logical-call audit rows
            // correctly record the failure (lesson #30: every logical
            // request needs its audit trail).
            let err_msg = format!("{e}");
            for sub in batch {
                let _ = sub.reply.send(Err(SamplingError::Service(
                    rmcp::service::ServiceError::McpError(rmcp::model::ErrorData::internal_error(
                        format!("sampling coordinator: coalesced RPC failed: {err_msg}"),
                        None,
                    )),
                )));
            }
        }
    }
}

/// Build a coalesced request from N submissions.
///
/// Prompt template:
///
/// ```text
/// System:
///   You are a batch task processor. Process EVERY task listed
///   in the user message and reply with a JSON array of objects
///   where each object has shape:
///   { "task_index": <int starting from 0>, "response": "<string>" }
///   The array MUST have exactly N entries (one per task) in the
///   SAME ORDER. Do NOT include any prose outside the JSON.
///   [+ any system prompts from individual tasks, concatenated]
///
/// User:
///   {
///     "tasks": [
///       { "task_index": 0, "messages": [...messages from request 0...] },
///       { "task_index": 1, "messages": [...messages from request 1...] },
///       ...
///     ]
///   }
/// ```
///
/// The `max_tokens` of the coalesced request is the SUM of every
/// submission's `max_tokens`, capped at u32::MAX / 2. This gives
/// each per-task response room to be ~as long as its un-coalesced
/// counterpart would have been.
fn build_coalesced_request(batch: &[Submission]) -> CreateMessageRequestParams {
    let mut tasks: Vec<serde_json::Value> = Vec::with_capacity(batch.len());
    let mut system_parts: Vec<String> = vec![
        "You are a batch task processor. Process EVERY task listed in the \
         user message and reply with a JSON array of objects where each \
         object has shape: { \"task_index\": <int starting from 0>, \
         \"response\": \"<string>\" }. The array MUST have exactly N entries \
         (one per task) in the SAME ORDER. Do NOT include any prose outside \
         the JSON."
            .to_string(),
    ];

    for (idx, sub) in batch.iter().enumerate() {
        let mut task_messages: Vec<serde_json::Value> = Vec::new();
        if let Some(sys) = sub.params.system_prompt.as_ref() {
            // Hoist per-task system prompts so the LLM still sees
            // them inside its task context.
            system_parts.push(format!("Task-{idx} sub-system: {sys}"));
        }
        for sm in &sub.params.messages {
            let role_str = match sm.role {
                RmcpRole::User => "user",
                RmcpRole::Assistant => "assistant",
            };
            let mut text_parts: Vec<String> = Vec::new();
            for content in sm.content.iter() {
                if let SamplingMessageContent::Text(t) = content {
                    text_parts.push(t.text.clone());
                }
            }
            task_messages.push(serde_json::json!({
                "role": role_str,
                "content": text_parts.join("\n"),
            }));
        }
        tasks.push(serde_json::json!({
            "task_index": idx,
            "messages": task_messages,
        }));
    }

    let user_payload = serde_json::json!({ "tasks": tasks }).to_string();

    let max_tokens = batch
        .iter()
        .map(|s| s.params.max_tokens)
        .fold(0u32, |acc, n| acc.saturating_add(n));

    let mut params = CreateMessageRequestParams::new(
        vec![SamplingMessage::user_text(&user_payload)],
        max_tokens.max(1),
    );
    params = params.with_system_prompt(system_parts.join("\n\n"));
    // Carry over model_preferences from the first submission (if any).
    if let Some(prefs) = batch[0].params.model_preferences.as_ref() {
        params = params.with_model_preferences(prefs.clone());
    }
    params
}

/// Parse a coalesced response back into per-task `CreateMessageResult`s.
///
/// Expects the rendered message text to be a JSON array of objects
/// shaped `{ "task_index": <int>, "response": "<string>" }`. The
/// `task_index` field is required and must match the submission
/// order. Extra fields are ignored.
///
/// Returns one `Result<CreateMessageResult, SamplingError>` per
/// submission, in the SAME ORDER as `batch`. Per-task entries
/// missing from the response surface as
/// `SamplingError::Service(...)` with a `malformed_response`-style
/// message; the per-call audit row carries the failure.
fn demux_coalesced(
    rendered: &CreateMessageResult,
    batch: &[Submission],
) -> Result<Vec<Result<CreateMessageResult, SamplingError>>, String> {
    let text = extract_text_from_result(rendered).map_err(|e| e.to_string())?;
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            // Tolerate fenced ```json ... ``` blocks the model may
            // wrap the array in (matches `solo_steward::abstraction`
            // tolerance).
            match extract_fenced_json(&text) {
                Some(inner) => {
                    serde_json::from_str(inner).map_err(|fe| format!("fenced parse: {fe}"))?
                }
                None => return Err(format!("top-level JSON parse: {e}")),
            }
        }
    };
    let arr = parsed
        .as_array()
        .ok_or_else(|| "response root is not a JSON array".to_string())?;

    let mut out: Vec<Result<CreateMessageResult, SamplingError>> = Vec::with_capacity(batch.len());
    for (idx, _sub) in batch.iter().enumerate() {
        let entry = arr.iter().find(|e| {
            e.get("task_index")
                .and_then(|v| v.as_i64())
                .map(|i| i as usize == idx)
                .unwrap_or(false)
        });
        match entry {
            Some(e) => {
                let response_text = e.get("response").and_then(|v| v.as_str()).unwrap_or("");
                out.push(Ok(make_assistant_result(response_text, &rendered.model)));
            }
            None => out.push(Err(SamplingError::Service(
                rmcp::service::ServiceError::McpError(rmcp::model::ErrorData::internal_error(
                    format!("sampling coordinator: response missing task_index {idx}"),
                    None,
                )),
            ))),
        }
    }
    Ok(out)
}

fn extract_fenced_json(text: &str) -> Option<&str> {
    let needle = "```json";
    let start = text.find(needle)?;
    let after = &text[start + needle.len()..];
    let end = after.find("```")?;
    Some(after[..end].trim())
}

fn extract_text_from_result(result: &CreateMessageResult) -> Result<String, &'static str> {
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

fn make_assistant_result(text: &str, model: &str) -> CreateMessageResult {
    CreateMessageResult::new(
        SamplingMessage::assistant_text(text.to_string()),
        model.to_string(),
    )
}

#[async_trait]
impl SamplingClient for SamplingCoordinator {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
    ) -> Result<CreateMessageResult, SamplingError> {
        self.submit(params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{FakeMcpClient, FakeResponse};

    fn mk_params(prompt: &str) -> CreateMessageRequestParams {
        CreateMessageRequestParams::new(vec![SamplingMessage::user_text(prompt)], 128)
    }

    fn coalesced_response_for(n_tasks: usize) -> String {
        let mut arr = Vec::with_capacity(n_tasks);
        for i in 0..n_tasks {
            arr.push(serde_json::json!({
                "task_index": i,
                "response": format!("response-{i}"),
            }));
        }
        serde_json::to_string(&arr).unwrap()
    }

    /// P4d coalesce window: when N submissions arrive within the
    /// coalesce window, the underlying SamplingClient sees ONE
    /// `create_message` call carrying the coalesced prompt.
    #[tokio::test]
    async fn coalesces_n_concurrent_submissions_into_one_create_message_call() {
        let response = coalesced_response_for(3);
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text(&response)));
        let coord =
            SamplingCoordinator::with_settings(fake.clone(), Duration::from_millis(100), 10);

        // Submit 3 concurrent requests within the window.
        let c1 = coord.clone();
        let c2 = coord.clone();
        let c3 = coord.clone();
        let h1 = tokio::spawn(async move { c1.submit(mk_params("task-A")).await });
        let h2 = tokio::spawn(async move { c2.submit(mk_params("task-B")).await });
        let h3 = tokio::spawn(async move { c3.submit(mk_params("task-C")).await });

        let r1 = h1.await.unwrap().expect("submission 1 ok");
        let r2 = h2.await.unwrap().expect("submission 2 ok");
        let r3 = h3.await.unwrap().expect("submission 3 ok");

        // Inner saw EXACTLY one call.
        let recorded = fake.record_requests();
        assert_eq!(
            recorded.len(),
            1,
            "coordinator must coalesce 3 submissions into 1 inner call"
        );

        // Each submission got its task's text demultiplexed.
        assert_eq!(
            extract_text_from_result(&r1).unwrap(),
            "response-0",
            "task-0 response routed to first submission"
        );
        assert_eq!(extract_text_from_result(&r2).unwrap(), "response-1");
        assert_eq!(extract_text_from_result(&r3).unwrap(), "response-2");
    }

    /// P4d max-batch trigger: when `max_batch` submissions arrive
    /// FASTER than the window, the coordinator flushes immediately
    /// without waiting out the window.
    #[tokio::test]
    async fn flushes_at_max_batch_before_window_expires() {
        let response = coalesced_response_for(2);
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text(&response)));
        // 5-second window, 2-request max-batch.
        let coord = SamplingCoordinator::with_settings(fake.clone(), Duration::from_secs(5), 2);

        let started = tokio::time::Instant::now();
        let c1 = coord.clone();
        let c2 = coord.clone();
        let h1 = tokio::spawn(async move { c1.submit(mk_params("task-A")).await });
        let h2 = tokio::spawn(async move { c2.submit(mk_params("task-B")).await });

        let _ = h1.await.unwrap();
        let _ = h2.await.unwrap();

        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "max_batch must flush before window expires; took {elapsed:?}"
        );
        assert_eq!(fake.record_requests().len(), 1);
    }

    /// P4d single-request pass-through: a lone submission within
    /// the window goes through unchanged (no coalesce wrapping).
    /// Caller's prompt reaches the inner client verbatim.
    #[tokio::test]
    async fn single_submission_passes_through_unwrapped() {
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("direct-response")));
        let coord = SamplingCoordinator::with_settings(fake.clone(), Duration::from_millis(50), 10);

        let result = coord
            .submit(mk_params("lonely-task"))
            .await
            .expect("submission ok");

        // Inner saw the SAME prompt the caller submitted — no
        // "tasks" JSON wrapper.
        let recorded = fake.record_requests();
        assert_eq!(recorded.len(), 1);
        let inner_text = extract_first_user_text(&recorded[0]);
        assert_eq!(
            inner_text, "lonely-task",
            "single-batch path must NOT wrap the prompt"
        );
        // Caller got the inner response verbatim.
        assert_eq!(
            extract_text_from_result(&result).unwrap(),
            "direct-response"
        );
    }

    /// P4d window expiry: submissions trickle in slower than
    /// `window`, so each flush carries exactly one task.
    /// Verifies that the window timer is the trigger (not the
    /// max_batch), and that demux works correctly for 1-task
    /// batches.
    #[tokio::test]
    async fn window_expiry_flushes_each_submission_individually() {
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text("r-first")));
        fake.respond_each(vec![
            FakeResponse::text("r-first"),
            FakeResponse::text("r-second"),
        ]);
        let coord = SamplingCoordinator::with_settings(fake.clone(), Duration::from_millis(20), 10);

        let r1 = coord
            .submit(mk_params("first"))
            .await
            .expect("submission 1");
        // Wait past the window so the second submission lands in a
        // fresh batch.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let r2 = coord
            .submit(mk_params("second"))
            .await
            .expect("submission 2");

        assert_eq!(fake.record_requests().len(), 2);
        assert_eq!(extract_text_from_result(&r1).unwrap(), "r-first");
        assert_eq!(extract_text_from_result(&r2).unwrap(), "r-second");
    }

    /// P4d demux fault tolerance: when the LLM omits a task_index,
    /// only that submission gets the malformed-response error; the
    /// others land successfully.
    #[tokio::test]
    async fn demux_propagates_per_request_failures() {
        // Coalesced response with task 0 + task 2 only (task 1 missing).
        let response = serde_json::json!([
            { "task_index": 0, "response": "ok-0" },
            { "task_index": 2, "response": "ok-2" },
        ])
        .to_string();
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::text(&response)));
        let coord =
            SamplingCoordinator::with_settings(fake.clone(), Duration::from_millis(100), 10);

        let c1 = coord.clone();
        let c2 = coord.clone();
        let c3 = coord.clone();
        let h1 = tokio::spawn(async move { c1.submit(mk_params("t0")).await });
        let h2 = tokio::spawn(async move { c2.submit(mk_params("t1")).await });
        let h3 = tokio::spawn(async move { c3.submit(mk_params("t2")).await });

        let r1 = h1.await.unwrap();
        let r2 = h2.await.unwrap();
        let r3 = h3.await.unwrap();

        assert!(r1.is_ok());
        assert!(r2.is_err(), "missing task_index must surface as error");
        assert!(r3.is_ok());
    }

    /// P4d coalesced-RPC-failure surfaces to EVERY submission. If
    /// the inner peer.create_message call fails, all coalesced
    /// callers see an error (so each per-logical-call audit row
    /// records the failure).
    #[tokio::test]
    async fn coalesced_rpc_failure_surfaces_to_every_submission() {
        let fake = Arc::new(FakeMcpClient::new(FakeResponse::Error(
            crate::test_support::FakeSamplingError::Transport {
                message: "simulated transport failure".into(),
            },
        )));
        let coord =
            SamplingCoordinator::with_settings(fake.clone(), Duration::from_millis(100), 10);

        let c1 = coord.clone();
        let c2 = coord.clone();
        let h1 = tokio::spawn(async move { c1.submit(mk_params("a")).await });
        let h2 = tokio::spawn(async move { c2.submit(mk_params("b")).await });

        assert!(h1.await.unwrap().is_err());
        assert!(h2.await.unwrap().is_err());
    }

    fn extract_first_user_text(params: &CreateMessageRequestParams) -> String {
        for m in &params.messages {
            if m.role == RmcpRole::User {
                for c in m.content.iter() {
                    if let SamplingMessageContent::Text(t) = c {
                        return t.text.clone();
                    }
                }
            }
        }
        String::new()
    }
}
