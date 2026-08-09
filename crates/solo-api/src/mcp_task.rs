// SPDX-License-Identifier: Apache-2.0

//! In-memory MCP task state and cooperative cancellation helpers.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use dashmap::DashMap;
use rmcp::ErrorData as McpError;
use rmcp::model::{
    CancelTaskResult, CreateTaskResult, ErrorCode, GetTaskPayloadResult, GetTaskResult,
    ListTasksResult, Task, TaskStatus,
};
use serde_json::Value;
use uuid::Uuid;

use crate::mcp_session::SessionState;

pub const TASK_RESULT_TTL_MS: u64 = 30 * 60 * 1000;
pub const TASK_POLL_INTERVAL_MS: u64 = 1000;

#[derive(Debug)]
struct TaskEntry {
    task_id: String,
    created_at: String,
    cancel_requested: Arc<AtomicBool>,
    state: std::sync::Mutex<TaskEntryState>,
}

#[derive(Debug)]
struct TaskEntryState {
    status: TaskStatus,
    status_message: Option<String>,
    last_updated_at: String,
    result: Option<Value>,
}

#[derive(Clone, Debug)]
pub struct TaskHandle {
    pub task_id: String,
    cancel_requested: Arc<AtomicBool>,
}

impl TaskHandle {
    pub fn cancellation_token(&self) -> CancellationToken {
        CancellationToken::from_task_flag(self.cancel_requested.clone())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TaskStore {
    inner: Arc<DashMap<String, Arc<TaskEntry>>>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(&self, status_message: impl Into<String>) -> (CreateTaskResult, TaskHandle) {
        self.prune_expired();
        let task_id = Uuid::now_v7().to_string();
        let now = now_iso8601();
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let entry = Arc::new(TaskEntry {
            task_id: task_id.clone(),
            created_at: now.clone(),
            cancel_requested: cancel_requested.clone(),
            state: std::sync::Mutex::new(TaskEntryState {
                status: TaskStatus::Working,
                status_message: Some(status_message.into()),
                last_updated_at: now,
                result: None,
            }),
        });
        let task = snapshot_task(&entry);
        self.inner.insert(task_id.clone(), entry);
        (
            CreateTaskResult::new(task),
            TaskHandle {
                task_id,
                cancel_requested,
            },
        )
    }

    pub fn list(&self) -> ListTasksResult {
        self.prune_expired();
        let mut tasks: Vec<Task> = self
            .inner
            .iter()
            .map(|entry| snapshot_task(entry.value()))
            .collect();
        tasks.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then(a.task_id.cmp(&b.task_id))
        });
        let mut result = ListTasksResult::new(tasks);
        result.total = Some(self.inner.len() as u64);
        result
    }

    pub fn get(&self, task_id: &str) -> std::result::Result<GetTaskResult, McpError> {
        self.prune_expired();
        let entry = self.get_entry(task_id)?;
        Ok(GetTaskResult {
            meta: None,
            task: snapshot_task(&entry),
        })
    }

    pub fn result(&self, task_id: &str) -> std::result::Result<GetTaskPayloadResult, McpError> {
        self.prune_expired();
        let entry = self.get_entry(task_id)?;
        let state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
        match state.status {
            TaskStatus::Completed => Ok(GetTaskPayloadResult::new(
                state
                    .result
                    .clone()
                    .unwrap_or_else(|| serde_json::json!({})),
            )),
            TaskStatus::Failed => Err(McpError::internal_error(
                state
                    .status_message
                    .clone()
                    .unwrap_or_else(|| format!("task {task_id} failed")),
                None,
            )),
            TaskStatus::Cancelled => Err(McpError::new(
                ErrorCode::INVALID_REQUEST,
                state
                    .status_message
                    .clone()
                    .unwrap_or_else(|| format!("task {task_id} was cancelled")),
                None,
            )),
            _ => Err(McpError::new(
                ErrorCode::INVALID_REQUEST,
                format!("task {task_id} is not complete"),
                None,
            )),
        }
    }

    pub fn cancel(&self, task_id: &str) -> std::result::Result<CancelTaskResult, McpError> {
        self.prune_expired();
        let entry = self.get_entry(task_id)?;
        entry.cancel_requested.store(true, Ordering::SeqCst);
        {
            let mut state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
            if !matches!(state.status, TaskStatus::Completed | TaskStatus::Failed) {
                state.status = TaskStatus::Cancelled;
                state.status_message = Some("cancel requested".to_string());
                state.last_updated_at = now_iso8601();
            }
        }
        Ok(CancelTaskResult {
            meta: None,
            task: snapshot_task(&entry),
        })
    }

    pub fn complete(&self, handle: &TaskHandle, result: Value) {
        let Some(entry) = self.inner.get(&handle.task_id).map(|e| e.value().clone()) else {
            return;
        };
        let mut state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(state.status, TaskStatus::Cancelled) || handle.is_cancelled() {
            state.status = TaskStatus::Cancelled;
            state.status_message = Some("cancelled".to_string());
            state.last_updated_at = now_iso8601();
            return;
        }
        state.status = TaskStatus::Completed;
        state.status_message = Some("completed".to_string());
        state.last_updated_at = now_iso8601();
        state.result = Some(result);
    }

    pub fn fail(&self, handle: &TaskHandle, message: impl Into<String>) {
        let Some(entry) = self.inner.get(&handle.task_id).map(|e| e.value().clone()) else {
            return;
        };
        let mut state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
        if matches!(state.status, TaskStatus::Cancelled) || handle.is_cancelled() {
            state.status = TaskStatus::Cancelled;
            state.status_message = Some("cancelled".to_string());
            state.last_updated_at = now_iso8601();
            return;
        }
        state.status = TaskStatus::Failed;
        state.status_message = Some(message.into());
        state.last_updated_at = now_iso8601();
    }

    fn get_entry(&self, task_id: &str) -> std::result::Result<Arc<TaskEntry>, McpError> {
        self.inner
            .get(task_id)
            .map(|entry| entry.value().clone())
            .ok_or_else(|| McpError::invalid_params(format!("task {task_id} not found"), None))
    }

    fn prune_expired(&self) {
        let now = chrono::Utc::now();
        let ttl = chrono::Duration::milliseconds(TASK_RESULT_TTL_MS as i64);
        let expired = self
            .inner
            .iter()
            .filter_map(|entry| {
                let state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
                let terminal = matches!(
                    state.status,
                    TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
                );
                if !terminal {
                    return None;
                }
                let last_updated = chrono::DateTime::parse_from_rfc3339(&state.last_updated_at)
                    .ok()?
                    .with_timezone(&chrono::Utc);
                if now.signed_duration_since(last_updated) > ttl {
                    Some(entry.key().clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for task_id in expired {
            self.inner.remove(&task_id);
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    task_cancelled: Option<Arc<AtomicBool>>,
    request_cancelled: Option<(Arc<SessionState>, String)>,
}

impl CancellationToken {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn from_task_flag(flag: Arc<AtomicBool>) -> Self {
        Self {
            task_cancelled: Some(flag),
            request_cancelled: None,
        }
    }

    pub fn from_request(session: Arc<SessionState>, request_id: String) -> Self {
        Self {
            task_cancelled: None,
            request_cancelled: Some((session, request_id)),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.task_cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::SeqCst))
            || self
                .request_cancelled
                .as_ref()
                .is_some_and(|(session, id)| session.is_request_cancelled(id))
    }

    pub fn check(&self) -> std::result::Result<(), McpError> {
        if self.is_cancelled() {
            Err(McpError::new(
                ErrorCode::INVALID_REQUEST,
                "request cancelled",
                None,
            ))
        } else {
            Ok(())
        }
    }
}

fn snapshot_task(entry: &TaskEntry) -> Task {
    let state = entry.state.lock().unwrap_or_else(|p| p.into_inner());
    let mut task = Task::new(
        entry.task_id.clone(),
        state.status.clone(),
        entry.created_at.clone(),
        state.last_updated_at.clone(),
    )
    .with_ttl(TASK_RESULT_TTL_MS)
    .with_poll_interval(TASK_POLL_INTERVAL_MS);
    if let Some(message) = state.status_message.clone() {
        task = task.with_status_message(message);
    }
    task
}

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
