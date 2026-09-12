// SPDX-License-Identifier: Apache-2.0

//! Solo account sign-in, for a host that offers one.
//!
//! Community has no account and serves no account endpoint, so for it this
//! panel never appears. A host built on Community that links to a Solo account
//! (Solo Pro) answers `GET /host/v1/account`; when it does, Controls shows who
//! the machine is signed in as and offers to sign in. The path is a neutral
//! host-extension one on purpose: Community names no edition's private API.
//!
//! The host runs the whole sign-in itself: it opens the browser, receives the
//! code on its own loopback port, stores what the account service issues and
//! restarts to apply it. Controls only starts it and reports progress, so no
//! token ever passes through this process.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// How often to ask while nothing is happening, and while a sign-in waits on
/// the browser.
const IDLE_POLL: Duration = Duration::from_secs(10);
const WAITING_POLL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AccountStatus {
    #[serde(default)]
    pub linked: bool,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    /// The edition the licence on this machine grants, if it holds one.
    #[serde(default)]
    pub licensed_edition: Option<String>,
    #[serde(default)]
    pub sign_in: SignIn,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SignIn {
    /// `idle`, `waiting`, `done` or `failed`.
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub authorize_url: Option<String>,
}

impl AccountStatus {
    pub fn waiting(&self) -> bool {
        self.sign_in.state == "waiting"
    }

    /// Signed in, and the host is restarting to apply what it was given. Until
    /// that restart lands this status still describes the host as it started,
    /// so it must not be read as "this account has no licence".
    pub fn finishing(&self) -> bool {
        self.sign_in.state == "done"
    }
}

#[derive(Debug, Default)]
pub struct AccountPanel {
    /// `None` until a host has answered, and for a host with no account.
    pub status: Option<AccountStatus>,
    /// Why the last sign-in could not start.
    pub message: Option<String>,
    last_poll: Option<Instant>,
    poll_rx: Option<Receiver<Result<Option<AccountStatus>, String>>>,
    sign_in_rx: Option<Receiver<Result<(), String>>>,
}

impl AccountPanel {
    pub fn starting(&self) -> bool {
        self.sign_in_rx.is_some()
    }

    /// Collect finished requests and, when due, ask the host again.
    pub fn tick(&mut self, runtime: &tokio::runtime::Handle, status_url: &str, daemon_ready: bool) {
        if let Some(rx) = &self.poll_rx {
            match rx.try_recv() {
                Ok(Ok(status)) => {
                    self.status = status;
                    self.poll_rx = None;
                }
                // The host is restarting or briefly unreachable: keep showing
                // what it last said rather than flickering the panel away.
                Ok(Err(_)) | Err(TryRecvError::Disconnected) => self.poll_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.sign_in_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.message = result.err();
                    self.sign_in_rx = None;
                    // Ask straight away so the panel shows "waiting".
                    self.last_poll = None;
                }
                Err(TryRecvError::Disconnected) => self.sign_in_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }

        if !daemon_ready || self.poll_rx.is_some() {
            return;
        }
        let interval = if self.status.as_ref().is_some_and(AccountStatus::waiting) {
            WAITING_POLL
        } else {
            IDLE_POLL
        };
        if self.last_poll.is_some_and(|at| at.elapsed() < interval) {
            return;
        }
        self.last_poll = Some(Instant::now());
        let (tx, rx) = std::sync::mpsc::channel();
        self.poll_rx = Some(rx);
        let url = account_url(status_url, "");
        runtime.spawn(async move {
            let _ = tx.send(fetch(&url).await);
        });
    }

    pub fn start_sign_in(&mut self, runtime: &tokio::runtime::Handle, status_url: &str) {
        if self.sign_in_rx.is_some() {
            return;
        }
        self.message = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.sign_in_rx = Some(rx);
        let url = account_url(status_url, "/sign-in");
        tracing::info!(target: "solo::account", "operator started Solo account sign-in");
        runtime.spawn(async move {
            let _ = tx.send(start(&url).await);
        });
    }
}

fn account_url(status_url: &str, suffix: &str) -> String {
    format!(
        "{}/host/v1/account{suffix}",
        crate::update::base_url(status_url)
    )
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| format!("Could not build an HTTP client: {e}"))
}

/// `Ok(None)` for a host with no account: a 404, or anything that is not the
/// account shape (a web fallback answering every path with a page).
async fn fetch(url: &str) -> Result<Option<AccountStatus>, String> {
    let response = client()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Could not reach Solo: {e}"))?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let body = response
        .text()
        .await
        .map_err(|e| format!("Could not read the account status: {e}"))?;
    Ok(parse_status(&body))
}

fn parse_status(body: &str) -> Option<AccountStatus> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    // Require the field every account answer carries, so an unrelated JSON
    // body is not mistaken for one.
    value.get("linked")?.as_bool()?;
    serde_json::from_value(value).ok()
}

async fn start(url: &str) -> Result<(), String> {
    let response = client()?
        .post(url)
        .send()
        .await
        .map_err(|e| format!("Could not reach Solo: {e}"))?;
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
    Err(detail.unwrap_or_else(|| format!("Solo returned {status}.")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_without_an_account_is_not_mistaken_for_one() {
        assert!(parse_status("<!doctype html><html></html>").is_none());
        assert!(parse_status(r#"{"error":"not found"}"#).is_none());
    }

    #[test]
    fn the_account_shape_parses() {
        let status = parse_status(
            r#"{"linked":true,"email":"a@example.test","plan":"Solo Pro",
                "licensed_edition":"pro","sign_in":{"state":"idle"}}"#,
        )
        .expect("an account answer");
        assert!(status.linked);
        assert_eq!(status.email.as_deref(), Some("a@example.test"));
        assert!(!status.waiting());
    }

    #[test]
    fn account_urls_follow_the_daemon_address() {
        assert_eq!(
            account_url("http://127.0.0.1:17849/v1/status", "/sign-in"),
            "http://127.0.0.1:17849/host/v1/account/sign-in"
        );
    }
}
