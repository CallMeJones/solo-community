// SPDX-License-Identifier: Apache-2.0

//! Poll `/v1/status` to drive the tray-icon colour state.

use crate::STATUS_POLL_SECS;
use crate::daemon::{DaemonHandle, SupervisorState};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DaemonHealth {
    /// `/v1/status` returned 200 + `ok: true` on the last poll.
    Healthy,
    /// HTTP is reachable but reporting `ok: false` or non-200, OR
    /// child process is running but HTTP not yet up (typical during
    /// startup).
    #[default]
    Starting,
    /// HTTP is not reachable while the supervisor expects a daemon, or
    /// the supervisor is in a terminal failure/stopped state.
    Down,
}

#[derive(Debug, Default, Clone)]
pub struct StatusState {
    pub health: DaemonHealth,
    /// Raw JSON from the last successful /v1/status response. Surfaced
    /// in the runtime status panel.
    pub last_payload: Option<serde_json::Value>,
    /// Wall-time of the last successful poll, for the UI's "last
    /// updated" label.
    pub last_ok_at: Option<std::time::SystemTime>,
    /// Last error message from a failed poll. None = last poll succeeded
    /// or no polls yet.
    pub last_error: Option<String>,
}

/// Polling loop that also drives the notification toaster on health
/// transitions. The notifier debounces internally; here we just
/// forward every observation.
pub async fn poll_loop_with_notify(
    state: Arc<Mutex<StatusState>>,
    url: &str,
    notifier: Arc<Mutex<crate::notify::Notifier>>,
    daemon_handle: Arc<Mutex<DaemonHandle>>,
) {
    let client = match build_client() {
        Some(c) => c,
        None => return,
    };
    loop {
        if should_poll_http(&daemon_handle, &state, Some(&notifier)).await {
            poll_once(&client, url, &state, Some(&notifier)).await;
        }
        tokio::time::sleep(Duration::from_secs(STATUS_POLL_SECS)).await;
    }
}

/// Polling loop without notifications (kept for tests / future
/// no-notifier paths).
#[allow(dead_code)]
pub async fn poll_loop(state: Arc<Mutex<StatusState>>, url: &str) {
    let client = match build_client() {
        Some(c) => c,
        None => return,
    };
    loop {
        poll_once(&client, url, &state, None).await;
        tokio::time::sleep(Duration::from_secs(STATUS_POLL_SECS)).await;
    }
}

fn build_client() -> Option<reqwest::Client> {
    match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::error!(error = %e, "build reqwest client failed; status polling disabled");
            None
        }
    }
}

async fn should_poll_http(
    daemon_handle: &Arc<Mutex<DaemonHandle>>,
    state: &Arc<Mutex<StatusState>>,
    notifier: Option<&Arc<Mutex<crate::notify::Notifier>>>,
) -> bool {
    let supervisor_state = daemon_handle.lock().await.state.clone();
    match supervisor_state {
        SupervisorState::Locked => {
            set_health(state, DaemonHealth::Starting, None, notifier).await;
            false
        }
        SupervisorState::StartupFailed(msg) => {
            set_health(state, DaemonHealth::Down, Some(msg), notifier).await;
            false
        }
        SupervisorState::Stopped => {
            set_health(
                state,
                DaemonHealth::Down,
                Some("daemon supervisor stopped".to_string()),
                notifier,
            )
            .await;
            false
        }
        SupervisorState::Starting
        | SupervisorState::Running
        | SupervisorState::Restarting
        | SupervisorState::Crashed(_) => true,
    }
}

async fn set_health(
    state: &Arc<Mutex<StatusState>>,
    health: DaemonHealth,
    last_error: Option<String>,
    notifier: Option<&Arc<Mutex<crate::notify::Notifier>>>,
) {
    let mut s = state.lock().await;
    s.health = health;
    s.last_error = last_error;
    if health != DaemonHealth::Healthy {
        s.last_payload = None;
        s.last_ok_at = None;
    }
    drop(s);

    if let Some(notifier) = notifier {
        let mut n = notifier.lock().await;
        n.observe(health);
    }
}

async fn poll_once(
    client: &reqwest::Client,
    url: &str,
    state: &Arc<Mutex<StatusState>>,
    notifier: Option<&Arc<Mutex<crate::notify::Notifier>>>,
) {
    let result = client.get(url).send().await;
    let mut last_payload = None;
    let mut last_ok_at = None;
    let (health, last_error) = match result {
        Ok(resp) if resp.status().is_success() => match resp.json::<serde_json::Value>().await {
            Ok(json) => {
                let ok = json.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
                last_payload = Some(json);
                last_ok_at = Some(std::time::SystemTime::now());
                (
                    if ok {
                        DaemonHealth::Healthy
                    } else {
                        DaemonHealth::Starting
                    },
                    None,
                )
            }
            Err(e) => (
                DaemonHealth::Starting,
                Some(format!("decode /v1/status JSON: {e}")),
            ),
        },
        Ok(resp) => (
            DaemonHealth::Starting,
            Some(format!("/v1/status returned {}", resp.status())),
        ),
        Err(e) => (
            if e.is_connect() {
                DaemonHealth::Down
            } else {
                DaemonHealth::Starting
            },
            Some(format!("/v1/status request: {e}")),
        ),
    };

    let mut s = state.lock().await;
    s.health = health;
    if let Some(payload) = last_payload {
        s.last_payload = Some(payload);
        s.last_ok_at = last_ok_at;
    } else if health != DaemonHealth::Healthy {
        s.last_payload = None;
        s.last_ok_at = None;
    }
    s.last_error = last_error;
    let new_health = health;
    drop(s);

    if let Some(notifier) = notifier {
        let mut n = notifier.lock().await;
        n.observe(new_health);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_healthy_status_clears_stale_payload() {
        let state = Arc::new(Mutex::new(StatusState {
            health: DaemonHealth::Healthy,
            last_payload: Some(serde_json::json!({
                "ok": true,
                "tenant": { "id": "work" }
            })),
            last_ok_at: Some(std::time::SystemTime::now()),
            last_error: None,
        }));

        set_health(
            &state,
            DaemonHealth::Down,
            Some("daemon supervisor stopped".to_string()),
            None,
        )
        .await;

        let status = state.lock().await;
        assert_eq!(status.health, DaemonHealth::Down);
        assert!(status.last_payload.is_none());
        assert!(status.last_ok_at.is_none());
        assert_eq!(
            status.last_error.as_deref(),
            Some("daemon supervisor stopped")
        );
    }
}
