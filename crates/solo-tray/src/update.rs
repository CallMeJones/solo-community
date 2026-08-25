// SPDX-License-Identifier: Apache-2.0

//! Update checks and installer handoff for Solo Controls.
//!
//! The split with `solo-api`'s `update` module is deliberate. The daemon owns
//! the network side — listing releases, downloading, checking the digest —
//! because that is where the release logic already lives and where a 40 MB
//! stream can be written without blocking the UI thread. Controls owns the last
//! step, running the installer, because the installer has to overwrite binaries
//! the daemon is still executing and Controls is the supervisor that can stop it
//! first.
//!
//! Nothing here runs on a timer. Solo is local-first and its installers are
//! offline-first; the daemon reaches GitHub only when the operator presses a
//! button in this window.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Mirrors `solo_api::update::AvailableRelease`. Duplicated rather than shared:
/// solo-tray does not depend on solo-api (that would pull axum and the whole MCP
/// stack into the tray), and this is a stable wire shape between two binaries
/// shipped together in one installer.
#[derive(Debug, Clone, Deserialize)]
pub struct AvailableRelease {
    pub tag: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub published_at: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub asset_name: String,
    #[serde(default)]
    pub asset_size: u64,
    #[serde(default)]
    pub can_auto_install: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateCheck {
    #[serde(default)]
    pub current_version: String,
    #[serde(default)]
    pub current_ref: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    #[serde(default)]
    pub latest: Option<AvailableRelease>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStage {
    Idle,
    Downloading,
    Verifying,
    Ready,
    Failed,
    /// A stage this build does not know about — treated as "still working"
    /// rather than crashing the poll if the daemon is newer than Controls.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateStatus {
    pub stage: UpdateStage,
    #[serde(default)]
    pub downloaded_bytes: u64,
    #[serde(default)]
    pub total_bytes: u64,
    #[serde(default)]
    pub installer_path: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub note: String,
}

fn base_url(status_url: &str) -> String {
    status_url
        .strip_suffix("/v1/status")
        .map(str::to_string)
        .unwrap_or_else(|| "http://127.0.0.1:17821".to_string())
}

/// The daemon reaches GitHub for this, so it can take a while on a slow link.
async fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(45))
        .build()
        .map_err(|e| format!("Could not build an HTTP client: {e}"))
}

async fn read_error(response: reqwest::Response) -> String {
    let status = response.status();
    // The daemon's ApiError renders as {"error": "..."}; fall back to the raw
    // body so a proxy or a older daemon still produces something readable.
    let body = response.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.chars().take(200).collect());

    if status.as_u16() == 404 {
        return "This Solo daemon is too old to support updates. Restart Solo after installing a \
                build that includes the update endpoints."
            .to_string();
    }
    if detail.is_empty() {
        format!("Solo returned {status}.")
    } else {
        detail
    }
}

pub async fn check(status_url: String) -> Result<UpdateCheck, String> {
    let url = format!("{}/v1/update/check", base_url(&status_url));
    let response = client()
        .await?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Could not reach Solo: {e}"))?;
    if !response.status().is_success() {
        return Err(read_error(response).await);
    }
    response
        .json::<UpdateCheck>()
        .await
        .map_err(|e| format!("Could not read the update check: {e}"))
}

/// Asks the daemon to fetch and verify the package. Returns as soon as the work
/// is accepted; progress arrives through [`poll_status`].
pub async fn start_download(status_url: String) -> Result<(), String> {
    let url = format!("{}/v1/update/download", base_url(&status_url));
    let response = client()
        .await?
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("Could not reach Solo: {e}"))?;
    if !response.status().is_success() {
        return Err(read_error(response).await);
    }
    Ok(())
}

pub async fn poll_status(status_url: String) -> Result<UpdateStatus, String> {
    let url = format!("{}/v1/update/status", base_url(&status_url));
    let response = client()
        .await?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("Could not reach Solo: {e}"))?;
    if !response.status().is_success() {
        return Err(read_error(response).await);
    }
    response
        .json::<UpdateStatus>()
        .await
        .map_err(|e| format!("Could not read the update status: {e}"))
}

/// Start the downloaded installer and let it replace Solo.
///
/// The caller must have stopped the daemon first. Even so the handoff waits a
/// few seconds before Setup starts: the daemon's shutdown is asynchronous, and
/// Controls itself is still running and about to exit. `/CLOSEAPPLICATIONS`
/// covers anything still holding a file, and `/RESTARTAPPLICATIONS` brings Solo
/// back afterwards.
#[cfg(target_os = "windows")]
pub fn launch_installer(installer: &Path) -> Result<(), String> {
    use std::process::{Command, Stdio};

    if !installer.is_file() {
        return Err(format!("{} is not there any more.", installer.display()));
    }

    let script = format!(
        "timeout /T 4 /NOBREAK >nul & start \"\" /B \"{}\" \
         /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /RESTARTAPPLICATIONS",
        installer.display()
    );

    Command::new("cmd.exe")
        .args(["/C", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not start the installer: {e}"))
}

#[cfg(not(target_os = "windows"))]
pub fn launch_installer(_installer: &Path) -> Result<(), String> {
    Err(
        "Installing without a prompt is not supported on this platform. \
         Install the downloaded package manually."
            .to_string(),
    )
}

/// Reveal the verified package for the platforms that install by hand.
pub fn reveal_in_file_manager(path: &Path) {
    let dir = path.parent().unwrap_or(path);
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer.exe").arg(dir).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    let _ = dir;
}

pub fn installer_path(status: &UpdateStatus) -> Option<PathBuf> {
    status.installer_path.as_ref().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_strips_the_status_suffix() {
        assert_eq!(
            base_url("http://127.0.0.1:17821/v1/status"),
            "http://127.0.0.1:17821"
        );
    }

    #[test]
    fn base_url_falls_back_when_the_setting_is_not_a_status_url() {
        // Operators can repoint `status_url`; a surprising value must not
        // produce a nonsense update URL.
        assert_eq!(
            base_url("http://example.invalid/health"),
            "http://127.0.0.1:17821"
        );
    }

    #[test]
    fn unknown_stage_deserializes_instead_of_failing() {
        // Controls and the daemon ship together but can be mismatched mid-update;
        // an unrecognised stage must not break the poll.
        let parsed: UpdateStatus =
            serde_json::from_str(r#"{"stage":"teleporting"}"#).expect("should parse");
        assert_eq!(parsed.stage, UpdateStage::Unknown);
    }

    #[test]
    fn status_parses_the_daemon_shape() {
        let parsed: UpdateStatus = serde_json::from_str(
            r#"{"stage":"ready","downloaded_bytes":10,"total_bytes":10,
                "installer_path":"C:\\x\\SoloSetup.exe","error":null,"note":"ok"}"#,
        )
        .expect("should parse");
        assert_eq!(parsed.stage, UpdateStage::Ready);
        assert_eq!(
            installer_path(&parsed),
            Some(PathBuf::from("C:\\x\\SoloSetup.exe"))
        );
    }

    #[test]
    fn check_parses_a_no_update_response() {
        let parsed: UpdateCheck = serde_json::from_str(
            r#"{"current_version":"0.12.0","current_ref":"v0.12.0-test.13",
                "platform":"windows-x86_64","update_available":false,
                "latest":null,"note":"up to date"}"#,
        )
        .expect("should parse");
        assert!(!parsed.update_available);
        assert!(parsed.latest.is_none());
    }
}
