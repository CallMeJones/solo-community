// SPDX-License-Identifier: Apache-2.0

//! Updates and the Solo account, for the unified window.
//!
//! The unified window's bundled page is the only surface with native
//! privileges, so the whole flow lives here: the page renders the state this
//! produces and sends back the operator's choices. The daemon does the
//! fetching and verification (see `crate::update`); this process runs the
//! installer, because it is the supervisor that can stop the daemon first.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::host_account::{AccountPanel, AccountStatus};
use crate::settings::Edition;
use crate::update::{AvailableRelease, UpdateCheck, UpdateStage, UpdateStatus};

/// The daemon has nothing new to say faster than this.
const STATUS_POLL: Duration = Duration::from_millis(500);

/// The POST that starts a download races the first status poll, so Idle
/// usually means "not picked up yet". Only after this long does it mean the
/// daemon restarted and lost the job.
const START_GRACE: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
enum Phase {
    Idle,
    Checking,
    UpToDate {
        note: String,
    },
    Available {
        release: AvailableRelease,
        note: String,
    },
    Downloading {
        tag: String,
        downloaded: u64,
        total: u64,
        note: String,
    },
    Ready {
        tag: String,
        installer: PathBuf,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug)]
pub struct Updates {
    phase: Phase,
    installed: Option<String>,
    /// The edition the running Solo reported on its last check.
    running_edition: Option<Edition>,
    check_rx: Option<Receiver<Result<UpdateCheck, String>>>,
    download_rx: Option<Receiver<Result<(), String>>>,
    status_rx: Option<Receiver<Result<UpdateStatus, String>>>,
    last_status_poll: Option<Instant>,
    download_started: Option<Instant>,
    account: AccountPanel,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            installed: None,
            running_edition: None,
            check_rx: None,
            download_rx: None,
            status_rx: None,
            last_status_poll: None,
            download_started: None,
            account: AccountPanel::default(),
        }
    }
}

impl Updates {
    fn busy(&self) -> bool {
        matches!(self.phase, Phase::Checking | Phase::Downloading { .. })
    }

    pub fn check(
        &mut self,
        runtime: &tokio::runtime::Handle,
        status_url: &str,
        edition: Option<Edition>,
    ) {
        if self.busy() {
            return;
        }
        self.phase = Phase::Checking;
        let (tx, rx) = std::sync::mpsc::channel();
        self.check_rx = Some(rx);
        let url = status_url.to_owned();
        tracing::info!(
            target: "solo::update",
            edition = edition.map_or("running", Edition::as_str),
            "operator requested an update check"
        );
        runtime.spawn(async move {
            let _ = tx.send(crate::update::check(url, edition).await);
        });
    }

    /// Fetch and verify the release on offer. The daemon does the work;
    /// progress arrives through the status poll in [`Self::tick`].
    pub fn download(
        &mut self,
        runtime: &tokio::runtime::Handle,
        status_url: &str,
        edition: Option<Edition>,
    ) {
        let Phase::Available { release, .. } = &self.phase else {
            return;
        };
        let tag = release.tag.clone();
        let total = release.asset_size;
        tracing::info!(target: "solo::update", tag = %tag, bytes = total, "operator requested a download");
        self.phase = Phase::Downloading {
            tag,
            downloaded: 0,
            total,
            note: "Starting download".to_owned(),
        };
        self.download_started = Some(Instant::now());
        self.last_status_poll = None;
        let (tx, rx) = std::sync::mpsc::channel();
        self.download_rx = Some(rx);
        let url = status_url.to_owned();
        runtime.spawn(async move {
            let _ = tx.send(crate::update::start_download(url, edition).await);
        });
    }

    /// The edition changed, so whatever is on screen was offered for the
    /// other one.
    pub fn reset(&mut self) {
        if !self.busy() {
            self.phase = Phase::Idle;
        }
    }

    pub fn ready_installer(&self) -> Option<PathBuf> {
        match &self.phase {
            Phase::Ready { installer, .. } => Some(installer.clone()),
            _ => None,
        }
    }

    pub fn fail(&mut self, message: String) {
        self.phase = Phase::Failed { message };
    }

    pub fn sign_in(&mut self, runtime: &tokio::runtime::Handle, status_url: &str) {
        self.account.start_sign_in(runtime, status_url);
    }

    /// Collect finished requests and keep a running download's progress fresh.
    pub fn tick(&mut self, runtime: &tokio::runtime::Handle, status_url: &str, daemon_ready: bool) {
        if let Some(rx) = &self.check_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.check_rx = None;
                    self.absorb_check(result);
                }
                Err(TryRecvError::Disconnected) => self.check_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.download_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.download_rx = None;
                    if let Err(message) = result {
                        tracing::warn!(target: "solo::update", error = %message, "download request rejected");
                        self.phase = Phase::Failed { message };
                    }
                }
                Err(TryRecvError::Disconnected) => self.download_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.status_rx {
            match rx.try_recv() {
                Ok(Ok(status)) => {
                    self.status_rx = None;
                    self.absorb_status(status);
                }
                Ok(Err(message)) => {
                    self.status_rx = None;
                    self.phase = Phase::Failed { message };
                }
                Err(TryRecvError::Disconnected) => self.status_rx = None,
                Err(TryRecvError::Empty) => {}
            }
        }

        let due = self
            .last_status_poll
            .is_none_or(|at| at.elapsed() >= STATUS_POLL);
        if matches!(self.phase, Phase::Downloading { .. }) && self.status_rx.is_none() && due {
            self.last_status_poll = Some(Instant::now());
            let (tx, rx) = std::sync::mpsc::channel();
            self.status_rx = Some(rx);
            let url = status_url.to_owned();
            runtime.spawn(async move {
                let _ = tx.send(crate::update::poll_status(url).await);
            });
        }

        self.account.tick(runtime, status_url, daemon_ready);
    }

    fn absorb_check(&mut self, result: Result<UpdateCheck, String>) {
        match result {
            Ok(check) => {
                // A daemon too old to report its edition was Community.
                self.running_edition = Some(
                    check
                        .native_channel
                        .as_deref()
                        .and_then(Edition::parse)
                        .unwrap_or(Edition::Community),
                );
                self.installed = Some(installed_label(&check));
                self.phase = match (check.update_available, check.latest) {
                    (true, Some(release)) => Phase::Available {
                        release,
                        note: check.note,
                    },
                    _ => Phase::UpToDate { note: check.note },
                };
            }
            Err(message) => {
                tracing::warn!(target: "solo::update", error = %message, "update check failed");
                self.phase = Phase::Failed { message };
            }
        }
    }

    fn absorb_status(&mut self, status: UpdateStatus) {
        let Phase::Downloading { tag, .. } = &self.phase else {
            return;
        };
        let tag = tag.clone();
        self.phase = match status.stage {
            UpdateStage::Ready => match crate::update::installer_path(&status) {
                Some(installer) => {
                    tracing::info!(target: "solo::update", path = %installer.display(), "update verified");
                    Phase::Ready { tag, installer }
                }
                None => Phase::Failed {
                    message: "Solo reported the update as ready but gave no file path.".to_owned(),
                },
            },
            UpdateStage::Failed => Phase::Failed {
                message: status
                    .error
                    .unwrap_or_else(|| "The update failed.".to_owned()),
            },
            UpdateStage::Idle => {
                if self
                    .download_started
                    .is_none_or(|at| at.elapsed() > START_GRACE)
                {
                    Phase::Failed {
                        message:
                            "Solo is no longer tracking this download. Check for updates again."
                                .to_owned(),
                    }
                } else {
                    Phase::Downloading {
                        tag,
                        downloaded: 0,
                        total: 0,
                        note: "Waiting for Solo to start the download".to_owned(),
                    }
                }
            }
            UpdateStage::Downloading | UpdateStage::Verifying | UpdateStage::Unknown => {
                Phase::Downloading {
                    tag,
                    downloaded: status.downloaded_bytes,
                    total: status.total_bytes,
                    note: status.note,
                }
            }
        };
    }

    /// What the bundled page renders. Plain data only; the page writes every
    /// string with `textContent`.
    pub fn payload(&self, chosen: Option<Edition>) -> Value {
        let edition = chosen
            .or(self.running_edition)
            .unwrap_or(Edition::Community);
        let hint = match self.running_edition {
            Some(running) if running != edition => Some(switch_hint(edition)),
            _ => None,
        };
        let (phase, note, tag, detail, downloaded, total) = match &self.phase {
            Phase::Idle => ("idle", String::new(), None, None, 0, 0),
            Phase::Checking => (
                "checking",
                "Asking GitHub for the newest release…".to_owned(),
                None,
                None,
                0,
                0,
            ),
            Phase::UpToDate { note } => ("up_to_date", note.clone(), None, None, 0, 0),
            Phase::Available { release, note } => (
                "available",
                note.clone(),
                Some(release.tag.clone()),
                Some(format!(
                    "{} · {}",
                    release.name,
                    format_size(release.asset_size)
                )),
                0,
                0,
            ),
            Phase::Downloading {
                tag,
                downloaded,
                total,
                note,
            } => (
                "downloading",
                note.clone(),
                Some(tag.clone()),
                None,
                *downloaded,
                *total,
            ),
            Phase::Ready { tag, .. } => (
                "ready",
                "Downloaded and verified. Solo closes, updates and reopens.".to_owned(),
                Some(tag.clone()),
                None,
                0,
                0,
            ),
            Phase::Failed { message } => ("failed", message.clone(), None, None, 0, 0),
        };
        json!({
            "edition": edition.as_str(),
            "hint": hint,
            "installed": self.installed,
            "busy": self.busy(),
            "phase": phase,
            "note": note,
            "tag": tag,
            "detail": detail,
            "downloaded": downloaded,
            "total": total,
            "account": self.account.status.as_ref().map(|status| self.account_payload(status)),
        })
    }

    fn account_payload(&self, status: &AccountStatus) -> Value {
        let error = self.account.message.clone().or_else(|| {
            (status.sign_in.state == "failed")
                .then(|| status.sign_in.error.clone())
                .flatten()
        });
        json!({
            "linked": status.linked,
            "email": status.email,
            "plan": status.plan,
            "licensed": status.licensed_edition.is_some(),
            "waiting": status.waiting(),
            "starting": self.account.starting(),
            "error": error,
        })
    }
}

fn switch_hint(edition: Edition) -> &'static str {
    match edition {
        Edition::Pro => {
            "Updates now offer Solo Pro. It installs over this Solo and keeps your memories; \
             sign in afterwards to unlock Pro."
        }
        Edition::Community => {
            "Updates now offer Solo Community. It installs over this Solo and keeps your \
             memories; Pro features stop."
        }
    }
}

fn installed_label(check: &UpdateCheck) -> String {
    match &check.current_ref {
        Some(tag) if !tag.is_empty() => format!("Installed {} ({tag})", check.current_version),
        _ => format!("Installed {}", check.current_version),
    }
}

fn format_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    #[allow(clippy::cast_precision_loss)]
    let mib = bytes as f64 / MIB;
    format!("{mib:.1} MB")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(available: bool, native: &str) -> UpdateCheck {
        serde_json::from_value(json!({
            "current_version": "0.12.5",
            "current_ref": "main",
            "native_channel": native,
            "update_available": available,
            "latest": if available {
                json!({"tag": "v0.13.0-pro.2", "name": "Solo Pro 0.13.0-pro.2",
                       "asset_name": "SoloSetup-pro-0.13.0-pro.2-x86_64.exe",
                       "asset_size": 43_423_650, "can_auto_install": true})
            } else { Value::Null },
            "note": "note",
        }))
        .unwrap()
    }

    #[test]
    fn an_offered_release_is_shown_with_its_install_button() {
        let mut updates = Updates::default();
        updates.absorb_check(Ok(check(true, "community")));
        let payload = updates.payload(Some(Edition::Pro));
        assert_eq!(payload["phase"], "available");
        assert_eq!(payload["tag"], "v0.13.0-pro.2");
        assert_eq!(payload["edition"], "pro");
        assert!(
            payload["hint"].as_str().unwrap().contains("Solo Pro"),
            "switching from Community to Pro explains itself"
        );
        assert!(
            payload["account"].is_null(),
            "Community hosts show no account"
        );
    }

    #[test]
    fn with_no_choice_the_edition_follows_the_running_solo() {
        let mut updates = Updates::default();
        updates.absorb_check(Ok(check(false, "pro")));
        let payload = updates.payload(None);
        assert_eq!(payload["edition"], "pro");
        assert!(payload["hint"].is_null());
        assert_eq!(payload["phase"], "up_to_date");
        assert_eq!(payload["installed"], "Installed 0.12.5 (main)");
    }

    #[test]
    fn a_verified_download_is_ready_to_install() {
        let mut updates = Updates::default();
        updates.absorb_check(Ok(check(true, "community")));
        updates.phase = Phase::Downloading {
            tag: "v0.13.0-pro.2".to_owned(),
            downloaded: 0,
            total: 0,
            note: String::new(),
        };
        let status: UpdateStatus = serde_json::from_value(json!({
            "stage": "ready", "installer_path": "C:\\x\\SoloSetup.exe"
        }))
        .unwrap();
        updates.absorb_status(status);
        assert_eq!(
            updates.ready_installer(),
            Some(PathBuf::from("C:\\x\\SoloSetup.exe"))
        );
        assert_eq!(updates.payload(None)["phase"], "ready");
    }

    #[test]
    fn a_failed_check_is_reported() {
        let mut updates = Updates::default();
        updates.absorb_check(Err("GitHub returned 403".to_owned()));
        let payload = updates.payload(None);
        assert_eq!(payload["phase"], "failed");
        assert_eq!(payload["note"], "GitHub returned 403");
        assert_eq!(payload["edition"], "community");
    }
}
