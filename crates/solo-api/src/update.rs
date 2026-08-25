// SPDX-License-Identifier: Apache-2.0

//! Self-update against the project's GitHub releases.
//!
//! Solo is local-first and its installers are offline-first, so this module
//! never polls on its own. Every network call here happens because the operator
//! pressed a button in Solo Controls: `check` lists releases, `download` fetches
//! one. Nothing runs on a timer and nothing is reported home.
//!
//! ## What this module does not do
//!
//! It stops at a verified file on disk. Running the installer is Solo Controls'
//! job, because the installer has to replace binaries this daemon is executing
//! and Controls is the supervisor that can stop it first. Splitting it that way
//! keeps the download in one place while the process lifecycle stays with the
//! thing that owns it.
//!
//! ## Why not `/releases/latest`
//!
//! GitHub's "latest" endpoint excludes pre-releases, and every Solo release so
//! far is marked pre-release — that endpoint currently 404s for this repo. It
//! is also the wrong shape regardless: Windows and Linux ship as *separate*
//! releases (`v0.12.0-test.13` vs `v0.12.0-linux-test.12`), so "latest" would
//! routinely name a release carrying no asset for the running platform.
//!
//! Instead this walks the release list (GitHub returns it newest-first) and
//! takes the first entry that actually carries an asset this platform can
//! install. Ordering comes from GitHub rather than from parsing versions, which
//! keeps the two tag schemes from having to agree on a comparison rule.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Release listing for the canonical repository. Pinned rather than derived
/// from Cargo's `repository` field: this decides what code gets executed on the
/// user's machine, so it should not follow a fork's metadata.
const RELEASES_URL: &str = "https://api.github.com/repos/CallMeJones/solo-community/releases";

/// GitHub rejects API requests without a User-Agent.
const USER_AGENT: &str = "solo-daemon-updater";

/// Enough releases to find a platform asset even when several consecutive
/// entries target the other OS.
const RELEASE_PAGE_SIZE: u32 = 30;

// ---------------------------------------------------------------- responses --

#[derive(Debug, Clone, Serialize)]
pub struct UpdateCheckResponse {
    pub current_version: String,
    pub current_ref: Option<String>,
    pub platform: String,
    pub update_available: bool,
    pub latest: Option<AvailableRelease>,
    pub note: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableRelease {
    pub tag: String,
    pub name: String,
    pub notes: String,
    pub published_at: String,
    pub prerelease: bool,
    pub html_url: String,
    pub asset_name: String,
    pub asset_size: u64,
    /// False when the platform's package cannot be installed unattended — a
    /// Linux `.deb` needs root, so there the flow stops at a verified download.
    pub can_auto_install: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStage {
    Idle,
    Downloading,
    Verifying,
    /// Downloaded and checksum-verified, sitting at `installer_path`. Terminal
    /// as far as the daemon is concerned; Solo Controls takes it from here.
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub stage: UpdateStage,
    pub tag: Option<String>,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub installer_path: Option<String>,
    pub error: Option<String>,
    pub note: String,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            stage: UpdateStage::Idle,
            tag: None,
            downloaded_bytes: 0,
            total_bytes: 0,
            installer_path: None,
            error: None,
            note: "No update in progress.".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateDownloadResponse {
    pub accepted: bool,
    pub tag: String,
    pub note: String,
}

// ------------------------------------------------------------- github types --

#[derive(Debug, Clone, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    html_url: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct GhAsset {
    name: String,
    #[serde(default)]
    size: u64,
    browser_download_url: String,
}

// ------------------------------------------------------------------ platform --

/// The installable package for this build's platform, if one is supported.
#[cfg(target_os = "windows")]
fn platform_asset(assets: &[GhAsset]) -> Option<&GhAsset> {
    assets
        .iter()
        .find(|a| a.name.ends_with(".exe") && a.name.contains("x86_64"))
}

#[cfg(target_os = "linux")]
fn platform_asset(assets: &[GhAsset]) -> Option<&GhAsset> {
    assets
        .iter()
        .find(|a| a.name.ends_with(".deb") && a.name.contains("amd64"))
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn platform_asset(_assets: &[GhAsset]) -> Option<&GhAsset> {
    None
}

/// Whether this platform's package can be installed without a prompt.
/// Windows ships an Inno Setup installer configured `PrivilegesRequired=lowest`
/// into `{localappdata}`, so it runs silently with no UAC. A `.deb` needs root
/// and cannot.
pub const fn can_auto_install() -> bool {
    cfg!(target_os = "windows")
}

pub fn platform_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows-x86_64"
    } else if cfg!(target_os = "linux") {
        "linux-amd64"
    } else {
        "unsupported"
    }
}

// -------------------------------------------------------------------- status --

fn status_cell() -> &'static Arc<RwLock<UpdateStatus>> {
    static CELL: OnceLock<Arc<RwLock<UpdateStatus>>> = OnceLock::new();
    CELL.get_or_init(|| Arc::new(RwLock::new(UpdateStatus::default())))
}

pub fn current_status() -> UpdateStatus {
    status_cell()
        .read()
        .map(|s| s.clone())
        .unwrap_or_default()
}

fn set_status(next: UpdateStatus) {
    if let Ok(mut slot) = status_cell().write() {
        *slot = next;
    }
}

fn update_status(f: impl FnOnce(&mut UpdateStatus)) {
    if let Ok(mut slot) = status_cell().write() {
        f(&mut slot);
    }
}

/// True when a download is already running, so a second press of Install is a
/// no-op rather than a second concurrent download of the same 40 MB.
fn download_in_flight() -> bool {
    matches!(
        current_status().stage,
        UpdateStage::Downloading | UpdateStage::Verifying
    )
}

// --------------------------------------------------------------------- check --

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("Could not build an HTTP client: {e}"))
}

async fn fetch_releases() -> Result<Vec<GhRelease>, String> {
    let client = http_client()?;
    let response = client
        .get(RELEASES_URL)
        .query(&[("per_page", RELEASE_PAGE_SIZE.to_string())])
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Could not reach GitHub: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        // 403 here is almost always the unauthenticated rate limit (60/hour per
        // IP); say so rather than surfacing a bare status code.
        let hint = if status.as_u16() == 403 {
            " GitHub's unauthenticated rate limit may have been reached; try again later."
        } else {
            ""
        };
        return Err(format!("GitHub returned {status} for the release list.{hint}"));
    }

    response
        .json::<Vec<GhRelease>>()
        .await
        .map_err(|e| format!("Could not read the release list: {e}"))
}

/// Pick the newest release carrying an asset for this platform, and decide
/// whether it is actually ahead of what is running.
fn evaluate(releases: &[GhRelease]) -> (Option<AvailableRelease>, bool, String) {
    let usable: Vec<(usize, &GhRelease, &GhAsset)> = releases
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.draft)
        .filter_map(|(i, r)| platform_asset(&r.assets).map(|a| (i, r, a)))
        .collect();

    let Some(&(candidate_idx, release, asset)) = usable.first() else {
        return (
            None,
            false,
            format!(
                "No release in the latest {RELEASE_PAGE_SIZE} carries a package for {}.",
                platform_label()
            ),
        );
    };

    let available = AvailableRelease {
        tag: release.tag_name.clone(),
        name: release.name.clone().unwrap_or_else(|| release.tag_name.clone()),
        notes: release.body.clone().unwrap_or_default(),
        published_at: release.published_at.clone().unwrap_or_default(),
        prerelease: release.prerelease,
        html_url: release.html_url.clone(),
        asset_name: asset.name.clone(),
        asset_size: asset.size,
        can_auto_install: can_auto_install(),
    };

    // Position the running build within the same list. Comparing list indices
    // rather than parsing versions sidesteps the two tag schemes: whatever
    // GitHub considers newer is newer.
    let current_ref = solo_core::build_info::build_ref();
    let current_idx = current_ref.and_then(|r| releases.iter().position(|rel| rel.tag_name == r));

    match current_idx {
        Some(i) if i <= candidate_idx => (
            Some(available),
            false,
            format!("Running {}, the newest build for this platform.", release.tag_name),
        ),
        Some(_) => {
            let note = format!("{} is available.", release.tag_name);
            (Some(available), true, note)
        }
        None if current_ref.is_none() => (
            Some(available),
            false,
            "This build reports no release tag, so it is probably a local build. \
             Updating would replace it with a published release."
                .to_string(),
        ),
        None => (
            Some(available),
            true,
            format!(
                "Running {}, which is not among the recent releases. {} is the newest published build.",
                current_ref.unwrap_or("an untagged build"),
                release.tag_name
            ),
        ),
    }
}

pub async fn check() -> Result<UpdateCheckResponse, String> {
    tracing::info!(target: "solo::update", platform = platform_label(), "update check requested");
    let releases = fetch_releases().await.inspect_err(|err| {
        tracing::warn!(target: "solo::update", error = %err, "release listing failed");
    })?;
    let (latest, update_available, note) = evaluate(&releases);
    tracing::info!(
        target: "solo::update",
        releases = releases.len(),
        update_available,
        candidate = latest.as_ref().map(|r| r.tag.as_str()).unwrap_or("none"),
        current = solo_core::build_info::build_ref().unwrap_or("untagged"),
        "update check complete: {note}"
    );

    Ok(UpdateCheckResponse {
        current_version: solo_core::build_info::version_with_build_metadata(),
        current_ref: solo_core::build_info::build_ref().map(str::to_string),
        platform: platform_label().to_string(),
        update_available,
        latest,
        note,
    })
}

// ------------------------------------------------------------------- install --

/// Download the newest platform package and verify it against the checksum
/// published beside it. Returns as soon as the work is accepted; progress is
/// reported through [`current_status`], ending at [`UpdateStage::Ready`] with
/// the verified path.
pub async fn start_download(data_dir: &Path) -> Result<UpdateDownloadResponse, String> {
    if download_in_flight() {
        return Err("An update is already in progress.".to_string());
    }

    let releases = fetch_releases().await?;
    let (latest, update_available, note) = evaluate(&releases);
    let Some(latest) = latest else {
        return Err(note);
    };
    if !update_available {
        return Err(note);
    }

    // Re-find the asset pair on the chosen release. The checksum has to come
    // from the same release as the package.
    let release = releases
        .iter()
        .find(|r| r.tag_name == latest.tag)
        .ok_or_else(|| "The chosen release disappeared from the listing.".to_string())?;
    let asset = platform_asset(&release.assets)
        .ok_or_else(|| "The chosen release has no package for this platform.".to_string())?;
    let checksum_url = release
        .assets
        .iter()
        .find(|a| a.name == format!("{}.sha256", asset.name))
        .map(|a| a.browser_download_url.clone());

    let dest_dir = data_dir.join("updates");
    let dest = dest_dir.join(&asset.name);
    let asset_url = asset.browser_download_url.clone();
    let expected_size = asset.size;
    let tag = latest.tag.clone();

    tracing::info!(
        target: "solo::update",
        tag = %latest.tag,
        asset = %asset.name,
        bytes = expected_size,
        checksum = checksum_url.is_some(),
        "starting update download"
    );

    set_status(UpdateStatus {
        stage: UpdateStage::Downloading,
        tag: Some(tag.clone()),
        downloaded_bytes: 0,
        total_bytes: expected_size,
        installer_path: None,
        error: None,
        note: format!("Downloading {}", asset.name),
    });

    let spawn_tag = tag.clone();
    tokio::spawn(async move {
        if let Err(err) = run_download(&asset_url, checksum_url, &dest_dir, &dest).await {
            tracing::warn!(target: "solo::update", tag = %spawn_tag, error = %err, "update failed");
            update_status(|s| {
                s.stage = UpdateStage::Failed;
                s.error = Some(err.clone());
                s.note = err;
            });
        }
    });

    Ok(UpdateDownloadResponse {
        accepted: true,
        tag,
        note: "Downloading the update.".to_string(),
    })
}

async fn run_download(
    asset_url: &str,
    checksum_url: Option<String>,
    dest_dir: &Path,
    dest: &Path,
) -> Result<(), String> {
    tokio::fs::create_dir_all(dest_dir)
        .await
        .map_err(|e| format!("Could not create {}: {e}", dest_dir.display()))?;

    let digest = download_verified(asset_url, dest).await?;
    tracing::info!(target: "solo::update", sha256 = %digest, path = %dest.display(), "download complete");

    // A checksum fetched from the same release as the package guards against a
    // truncated or corrupted transfer, not against a compromised release. It is
    // still worth doing — a half-downloaded installer is the likely failure.
    match checksum_url {
        Some(url) => {
            update_status(|s| {
                s.stage = UpdateStage::Verifying;
                s.note = "Verifying checksum".to_string();
            });
            let expected = fetch_checksum(&url).await?;
            if !expected.eq_ignore_ascii_case(&digest) {
                tracing::error!(
                    target: "solo::update",
                    expected = %expected,
                    actual = %digest,
                    "checksum mismatch; discarding download"
                );
                let _ = tokio::fs::remove_file(dest).await;
                return Err(format!(
                    "Checksum mismatch: expected {expected}, got {digest}. The download was discarded."
                ));
            }
        }
        None => {
            tracing::error!(target: "solo::update", "release publishes no .sha256; discarding download");
            let _ = tokio::fs::remove_file(dest).await;
            return Err(
                "The release publishes no .sha256 for this package, so the download could not be \
                 verified. It was discarded."
                    .to_string(),
            );
        }
    }

    let path_string = dest.display().to_string();
    tracing::info!(target: "solo::update", path = %path_string, "update verified and ready to install");
    update_status(|s| {
        s.stage = UpdateStage::Ready;
        s.installer_path = Some(path_string.clone());
        s.note = if can_auto_install() {
            "Verified and ready to install.".to_string()
        } else {
            format!("Verified. Install it with: sudo apt install {path_string}")
        };
    });
    Ok(())
}

/// Stream to disk while hashing, so a 40 MB installer never sits in memory and
/// the caller gets byte-level progress.
async fn download_verified(url: &str, dest: &Path) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;

    let client = http_client()?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Could not start the download: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("Download failed with {}.", response.status()));
    }
    if let Some(len) = response.content_length() {
        update_status(|s| s.total_bytes = len);
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("Could not write {}: {e}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("The download was interrupted: {e}"))?;
        hasher.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Could not write {}: {e}", dest.display()))?;
        written += chunk.len() as u64;
        update_status(|s| s.downloaded_bytes = written);
    }

    file.flush()
        .await
        .map_err(|e| format!("Could not finish writing {}: {e}", dest.display()))?;

    Ok(hex::encode(hasher.finalize()))
}

/// `.sha256` files are `<hex>  <filename>`; take the first field.
async fn fetch_checksum(url: &str) -> Result<String, String> {
    let client = http_client()?;
    let body = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("Could not fetch the checksum: {e}"))?
        .text()
        .await
        .map_err(|e| format!("Could not read the checksum: {e}"))?;

    body.split_whitespace()
        .next()
        .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .map(str::to_string)
        .ok_or_else(|| "The published checksum file was not a SHA-256 digest.".to_string())
}

/// Where a verified-but-not-installed package was left, for the UI to show.
pub fn staged_installer_path() -> Option<PathBuf> {
    current_status().installer_path.map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, assets: &[(&str, u64)]) -> GhRelease {
        GhRelease {
            tag_name: tag.to_string(),
            name: Some(tag.to_string()),
            body: Some("notes".to_string()),
            published_at: Some("2026-08-13T21:56:40Z".to_string()),
            prerelease: true,
            draft: false,
            html_url: format!("https://example.invalid/{tag}"),
            assets: assets
                .iter()
                .map(|(name, size)| GhAsset {
                    name: (*name).to_string(),
                    size: *size,
                    browser_download_url: format!("https://example.invalid/{name}"),
                })
                .collect(),
        }
    }

    fn win(tag: &str) -> GhRelease {
        release(tag, &[("SoloSetup-x86_64.exe", 42), ("SoloSetup-x86_64.exe.sha256", 1)])
    }

    fn linux(tag: &str) -> GhRelease {
        release(tag, &[("solo-ubuntu24.04-amd64.deb", 42)])
    }

    #[test]
    fn skips_releases_without_a_package_for_this_platform() {
        // The real repo interleaves Windows-only and Linux-only releases, so the
        // newest release is routinely not the newest *installable* one.
        let releases = if cfg!(target_os = "windows") {
            vec![linux("v0.12.0-linux-test.14"), win("v0.12.0-test.13")]
        } else {
            vec![win("v0.12.0-test.14"), linux("v0.12.0-linux-test.13")]
        };
        let (latest, _, _) = evaluate(&releases);

        if cfg!(any(target_os = "windows", target_os = "linux")) {
            let latest = latest.expect("a platform release should be selected");
            assert!(
                !latest.tag.ends_with("14") || cfg!(target_os = "linux"),
                "should skip the release that carries no asset for this platform",
            );
        } else {
            assert!(latest.is_none(), "unsupported platforms offer nothing");
        }
    }

    #[test]
    fn draft_releases_are_ignored() {
        let mut draft = win("v9.9.9-draft");
        draft.draft = true;
        let releases = vec![draft, win("v0.12.0-test.13")];
        let (latest, _, _) = evaluate(&releases);

        if cfg!(target_os = "windows") {
            assert_eq!(
                latest.expect("a release should be selected").tag,
                "v0.12.0-test.13"
            );
        }
    }

    #[test]
    fn no_platform_asset_means_no_update() {
        // A listing of releases that carry nothing installable here.
        let releases = vec![release("v0.0.1", &[("solo-macos.tar.gz", 1)])];
        let (latest, available, note) = evaluate(&releases);
        assert!(latest.is_none());
        assert!(!available);
        assert!(note.contains("carries a package"), "note was: {note}");
    }

    #[test]
    fn empty_listing_is_not_an_update() {
        let (latest, available, _) = evaluate(&[]);
        assert!(latest.is_none());
        assert!(!available);
    }

    #[test]
    fn checksum_parser_accepts_the_published_shape() {
        // sha256sum output: digest, two spaces, filename.
        let digest = "a".repeat(64);
        let body = format!("{digest}  SoloSetup-0.12.0-test.13-x86_64.exe\n");
        let parsed = body
            .split_whitespace()
            .next()
            .filter(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(parsed, Some(digest.as_str()));
    }

    #[test]
    fn status_starts_idle() {
        let status = UpdateStatus::default();
        assert_eq!(status.stage, UpdateStage::Idle);
        assert!(status.error.is_none());
    }
}
