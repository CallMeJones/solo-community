// SPDX-License-Identifier: Apache-2.0

//! Persistent tray settings — `~/.solo/tray.toml`.
//!
//! Precedence: explicit URL env vars > config file > `SOLO_HTTP_PORT`
//! derived daemon defaults > built-in defaults.
//!
//! The file is human-editable TOML; the tray reads it on startup and
//! writes back when the user changes a setting via a menu item
//! (autostart, theme). Missing file or malformed file ⇒ silently use
//! defaults.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// File name under the Solo data dir.
pub const SETTINGS_FILE: &str = "tray.toml";
pub(crate) const DEFAULT_HTTP_PORT: u16 = 17821;
const ENV_HTTP_PORT: &str = "SOLO_HTTP_PORT";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct Settings {
    /// Base URL the Solo app webview/browser fallback points at.
    pub solo_web_url: String,
    /// `/v1/status` URL the health poller hits.
    pub status_url: String,
    /// HTTP port the tray asks the daemon to bind.
    pub http_port: u16,
    /// Show notification toasts on health transitions.
    pub notifications_enabled: bool,
    /// Light vs dark theme for the egui window.
    pub theme: Theme,
    /// Autostart on login — when true, the tray writes/preserves an
    /// OS-native autostart entry (Run key on Windows). UI never edits
    /// this directly; user toggles it from the tray menu, which calls
    /// into `autostart` module to enact the change AND persists this
    /// flag so we know whether to keep it in sync.
    pub autostart_on_login: bool,
    /// When true, the tray may read/write the daemon passphrase from
    /// the user's OS keychain. The passphrase itself never goes into
    /// this TOML file.
    pub remember_passphrase_in_keychain: bool,
    /// Hide the first-run Dashboard guide after the user finishes or
    /// skips it. All underlying controls remain available from their
    /// normal pages.
    pub setup_wizard_completed: bool,
    /// Project/workspace root surfaced in the Projects view. This is
    /// a non-secret path; project identity still lives in
    /// `.solo/project.toml`.
    pub project_root: Option<PathBuf>,
    /// Which memory scope the user wants one-click client setup/check actions
    /// to target by default from this desktop surface.
    pub workspace_access_scope: WorkspaceAccessScope,
    /// Last observed result for one-click connected-tool actions.
    /// This is non-secret UI state only; client configs are inspected
    /// live from their native paths.
    pub connected_tools: BTreeMap<String, ConnectedToolLastStatus>,
    /// Local Memory Inbox review state keyed by memory id.
    /// This is UI workflow state only; the memory database remains the
    /// source of truth for memory content and lifecycle.
    pub memory_reviews: BTreeMap<String, MemoryReviewStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct ConnectedToolLastStatus {
    pub status: String,
    pub detail: String,
    pub config_path: Option<String>,
    pub config_state: Option<String>,
    pub transport: Option<String>,
    pub profile_route: Option<String>,
    pub resolved_profile: Option<String>,
    pub updated_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct MemoryReviewStatus {
    pub state: String,
    pub reviewed_at_ms: Option<i64>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceAccessScope {
    GlobalOnly,
    ProjectOnly,
    GlobalAndProject,
}

impl WorkspaceAccessScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::GlobalOnly => "Global memory",
            Self::ProjectOnly => "Project memory",
            Self::GlobalAndProject => "Global + project",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            Self::GlobalOnly => "Offer user-level client setup for the Memory Library.",
            Self::ProjectOnly => "Offer project-scoped client setup from the selected root.",
            Self::GlobalAndProject => {
                "Offer both user-level memory and selected project setup actions."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Dark,
    Light,
    /// Match the system's dark/light preference (best-effort; egui
    /// only knows what eframe tells it).
    System,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            solo_web_url: super::DEFAULT_SOLO_WEB_URL.to_string(),
            status_url: super::DEFAULT_STATUS_URL.to_string(),
            http_port: DEFAULT_HTTP_PORT,
            notifications_enabled: true,
            theme: Theme::System,
            autostart_on_login: false,
            remember_passphrase_in_keychain: false,
            setup_wizard_completed: false,
            project_root: None,
            workspace_access_scope: WorkspaceAccessScope::GlobalAndProject,
            connected_tools: BTreeMap::new(),
            memory_reviews: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Load from disk; falls back to default on any error (missing
    /// file, parse error, IO error). Logs a `tracing::warn!` on parse
    /// failure so an operator who hand-edited the file sees a hint.
    pub fn load(path: &std::path::Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no tray settings file; using defaults");
                return Self::with_env_overrides(Self::default());
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "read tray settings failed; using defaults");
                return Self::with_env_overrides(Self::default());
            }
        };
        let parsed: Self = match toml::from_str(&raw) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "parse tray settings failed; using defaults");
                return Self::with_env_overrides(Self::default());
            }
        };
        Self::with_env_overrides(Self::with_default_migrations(parsed))
    }

    fn with_default_migrations(mut self) -> Self {
        if self.solo_web_url == super::LEGACY_DEV_SOLO_WEB_URL
            || self.solo_web_url == super::LEGACY_PACKAGED_SOLO_WEB_URL
        {
            self.solo_web_url = super::DEFAULT_SOLO_WEB_URL.to_string();
        }
        if self.http_port == 0 {
            self.http_port = DEFAULT_HTTP_PORT;
        }
        sync_default_urls_to_http_port(&mut self, DEFAULT_HTTP_PORT);
        self
    }

    /// Apply env-var overrides on top of file-loaded values. Explicit URL env
    /// vars win outright; `SOLO_HTTP_PORT` updates the daemon port and any
    /// URL that still follows the managed daemon default.
    fn with_env_overrides(mut self) -> Self {
        Self::with_env_overrides_from(&mut self, |key| std::env::var(key).ok());
        self
    }

    fn with_env_overrides_from<F>(settings: &mut Self, env: F)
    where
        F: Fn(&str) -> Option<String>,
    {
        if let Some(port) = env(ENV_HTTP_PORT).and_then(|v| parse_http_port(&v)) {
            let previous_port = settings.http_port;
            let web_url_follows_port = settings.solo_web_url == super::DEFAULT_SOLO_WEB_URL
                || settings.solo_web_url == solo_web_url_for_port(previous_port);
            let status_url_follows_port = settings.status_url == super::DEFAULT_STATUS_URL
                || settings.status_url == status_url_for_port(previous_port);
            settings.http_port = port;
            if web_url_follows_port {
                settings.solo_web_url = solo_web_url_for_port(port);
            }
            if status_url_follows_port {
                settings.status_url = status_url_for_port(port);
            }
        }
        if let Some(v) = env("SOLO_WEB_URL") {
            settings.solo_web_url = v;
        }
        if let Some(v) = env("SOLO_TRAY_STATUS_URL") {
            settings.status_url = v;
        }
        if let Some(v) = env("SOLO_TRAY_NOTIFICATIONS_ENABLED")
            && let Ok(b) = v.parse::<bool>()
        {
            settings.notifications_enabled = b;
        }
        if let Some(v) = env("SOLO_TRAY_THEME") {
            settings.theme = match v.to_lowercase().as_str() {
                "dark" => Theme::Dark,
                "light" => Theme::Light,
                _ => Theme::System,
            };
        }
    }

    /// Save to disk, creating parent dirs if needed.
    pub fn try_save(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(format!(
                "create parent dir for tray settings {}: {e}",
                parent.display()
            ));
        }
        let s =
            toml::to_string_pretty(self).map_err(|e| format!("serialise tray settings: {e}"))?;
        if path.is_dir() {
            return Err(format!(
                "write tray settings {}: target is a directory",
                path.display()
            ));
        }
        write_settings_atomic(path, s.as_bytes())?;
        Ok(())
    }

    /// Best-effort save for UI preferences where the action can still
    /// proceed without persistence. Use `try_save` when persistence is
    /// required for a following daemon restart.
    pub fn save(&self, path: &std::path::Path) {
        if let Err(e) = self.try_save(path) {
            tracing::warn!(path = %path.display(), error = %e, "save tray settings failed");
        }
    }
}

fn write_settings_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("tray settings path has no parent: {}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(SETTINGS_FILE);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp_path = parent.join(format!("{file_name}.{stamp}.{}.tmp", std::process::id()));

    if let Err(error) = write_temp_file(&tmp_path, contents) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    if let Err(error) = replace_file_atomic(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "replace tray settings {} with {}: {error}",
            path.display(),
            tmp_path.display()
        ));
    }
    Ok(())
}

fn write_temp_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("open temp tray settings {}: {error}", path.display()))?;
    file.write_all(contents)
        .map_err(|error| format!("write temp tray settings {}: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("flush temp tray settings {}: {error}", path.display()))?;
    Ok(())
}

#[cfg(windows)]
fn replace_file_atomic(tmp_path: &Path, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    fn to_wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let from = to_wide(tmp_path);
    let to = to_wide(path);
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomic(tmp_path: &Path, path: &Path) -> Result<(), String> {
    std::fs::rename(tmp_path, path).map_err(|error| error.to_string())
}

pub(crate) fn status_url_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1/status")
}

pub(crate) fn solo_web_url_for_port(port: u16) -> String {
    format!("http://127.0.0.1:{port}/desktop/")
}

fn parse_http_port(value: &str) -> Option<u16> {
    let port = value.parse::<u16>().ok()?;
    (port > 0).then_some(port)
}

fn sync_default_urls_to_http_port(settings: &mut Settings, previous_port: u16) {
    if settings.http_port == previous_port {
        return;
    }
    if settings.solo_web_url == super::DEFAULT_SOLO_WEB_URL
        || settings.solo_web_url == solo_web_url_for_port(previous_port)
    {
        settings.solo_web_url = solo_web_url_for_port(settings.http_port);
    }
    if settings.status_url == super::DEFAULT_STATUS_URL
        || settings.status_url == status_url_for_port(previous_port)
    {
        settings.status_url = status_url_for_port(settings.http_port);
    }
}

/// Resolve the canonical settings path: `<SOLO_DATA_DIR or ~/.solo>/tray.toml`.
pub fn settings_path() -> PathBuf {
    let data_dir = if let Ok(d) = std::env::var("SOLO_DATA_DIR") {
        PathBuf::from(d)
    } else if let Some(home) = home_dir() {
        home.join(".solo")
    } else {
        PathBuf::from(".")
    };
    data_dir.join(SETTINGS_FILE)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply_env(settings: &mut Settings, vars: &[(&str, &str)]) {
        Settings::with_env_overrides_from(settings, |key| {
            vars.iter()
                .find_map(|(name, value)| (*name == key).then(|| (*value).to_string()))
        });
    }

    #[test]
    fn round_trip_default_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.toml");
        let s = Settings {
            project_root: Some(dir.path().join("project")),
            ..Settings::default()
        };
        s.save(&path);
        let loaded = Settings::load(&path);
        assert_eq!(loaded.solo_web_url, s.solo_web_url);
        assert_eq!(loaded.status_url, s.status_url);
        assert_eq!(loaded.http_port, s.http_port);
        assert_eq!(loaded.theme, s.theme);
        assert_eq!(loaded.notifications_enabled, s.notifications_enabled);
        assert_eq!(
            loaded.remember_passphrase_in_keychain,
            s.remember_passphrase_in_keychain
        );
        assert_eq!(loaded.setup_wizard_completed, s.setup_wizard_completed);
        assert_eq!(loaded.project_root, s.project_root);
        assert_eq!(loaded.workspace_access_scope, s.workspace_access_scope);
        assert_eq!(loaded.connected_tools, s.connected_tools);
        assert_eq!(loaded.memory_reviews, s.memory_reviews);
    }

    #[test]
    fn try_save_replaces_settings_without_leaving_temp_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.toml");
        let first = Settings {
            http_port: 17849,
            ..Settings::default()
        };
        let second = Settings {
            http_port: 17850,
            ..Settings::default()
        };

        first.try_save(&path).expect("initial save");
        second.try_save(&path).expect("replacement save");

        let loaded = Settings::load(&path);
        assert_eq!(loaded.http_port, 17850);
        let temp_files = std::fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("tray.toml.")
            })
            .count();
        assert_eq!(temp_files, 0);
    }

    #[test]
    fn workspace_access_scope_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.toml");
        let s = Settings {
            workspace_access_scope: WorkspaceAccessScope::ProjectOnly,
            ..Settings::default()
        };

        s.save(&path);
        let loaded = Settings::load(&path);

        assert_eq!(
            loaded.workspace_access_scope,
            WorkspaceAccessScope::ProjectOnly
        );
    }

    #[test]
    fn connected_tool_status_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.toml");
        let mut s = Settings::default();
        s.connected_tools.insert(
            "codex-user".to_string(),
            ConnectedToolLastStatus {
                status: "verified".to_string(),
                detail: "ok".to_string(),
                config_path: Some("C:/Users/Example/.codex/config.toml".to_string()),
                config_state: Some("Verified".to_string()),
                transport: Some("HTTP".to_string()),
                profile_route: Some("profile `work`".to_string()),
                resolved_profile: Some("work".to_string()),
                updated_at_ms: Some(123),
            },
        );

        s.save(&path);
        let loaded = Settings::load(&path);

        assert_eq!(loaded.connected_tools, s.connected_tools);
    }

    #[test]
    fn memory_review_status_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.toml");
        let mut s = Settings::default();
        s.memory_reviews.insert(
            "mem_123".to_string(),
            MemoryReviewStatus {
                state: "approved".to_string(),
                reviewed_at_ms: Some(456),
                note: Some("looks right".to_string()),
            },
        );

        s.save(&path);
        let loaded = Settings::load(&path);

        assert_eq!(loaded.memory_reviews, s.memory_reviews);
    }

    #[test]
    fn try_save_reports_persistence_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = Settings::default()
            .try_save(dir.path())
            .expect_err("saving to a directory path should fail");

        assert!(err.contains("write tray settings"));
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does-not-exist.toml");
        let s = Settings::load(&path);
        // Defaults match Settings::default() (env overrides may have
        // adjusted them but in a clean test run they should be defaults).
        assert!(!s.autostart_on_login);
        assert!(!s.remember_passphrase_in_keychain);
        assert!(s.notifications_enabled);
    }

    #[test]
    fn malformed_file_returns_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("broken.toml");
        std::fs::write(&path, "this is not valid toml = = =").expect("write");
        let s = Settings::load(&path);
        assert!(s.notifications_enabled);
    }

    #[test]
    fn legacy_web_urls_migrate_to_daemon_desktop_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        for legacy in [
            super::super::LEGACY_DEV_SOLO_WEB_URL,
            super::super::LEGACY_PACKAGED_SOLO_WEB_URL,
        ] {
            let path = dir.path().join("tray.toml");
            std::fs::write(&path, format!("solo_web_url = {legacy:?}\n")).expect("write");

            let s = Settings::load(&path);

            assert_eq!(s.solo_web_url, super::super::DEFAULT_SOLO_WEB_URL);
        }
    }

    #[test]
    fn solo_http_port_derives_default_daemon_urls() {
        let mut s = Settings::default();

        apply_env(&mut s, &[("SOLO_HTTP_PORT", "17849")]);

        assert_eq!(s.status_url, "http://127.0.0.1:17849/v1/status");
        assert_eq!(s.solo_web_url, "http://127.0.0.1:17849/desktop/");
        assert_eq!(s.http_port, 17849);
    }

    #[test]
    fn invalid_solo_http_port_keeps_default_daemon_urls() {
        let mut s = Settings::default();

        apply_env(&mut s, &[("SOLO_HTTP_PORT", "not-a-port")]);

        assert_eq!(s.status_url, super::super::DEFAULT_STATUS_URL);
        assert_eq!(s.solo_web_url, super::super::DEFAULT_SOLO_WEB_URL);
        assert_eq!(s.http_port, DEFAULT_HTTP_PORT);
        assert_eq!(parse_http_port("0"), None);
        assert_eq!(parse_http_port("65536"), None);
    }

    #[test]
    fn explicit_url_env_overrides_win_over_solo_http_port() {
        let mut s = Settings::default();

        apply_env(
            &mut s,
            &[
                ("SOLO_HTTP_PORT", "17849"),
                ("SOLO_WEB_URL", "http://127.0.0.1:5173"),
                ("SOLO_TRAY_STATUS_URL", "http://127.0.0.1:9999/v1/status"),
            ],
        );

        assert_eq!(s.solo_web_url, "http://127.0.0.1:5173");
        assert_eq!(s.status_url, "http://127.0.0.1:9999/v1/status");
        assert_eq!(s.http_port, 17849);
    }

    #[test]
    fn solo_http_port_does_not_replace_explicit_file_urls() {
        let mut s = Settings {
            solo_web_url: "http://127.0.0.1:5173".to_string(),
            status_url: "http://127.0.0.1:9999/v1/status".to_string(),
            ..Settings::default()
        };

        apply_env(&mut s, &[("SOLO_HTTP_PORT", "17849")]);

        assert_eq!(s.solo_web_url, "http://127.0.0.1:5173");
        assert_eq!(s.status_url, "http://127.0.0.1:9999/v1/status");
        assert_eq!(s.http_port, 17849);
    }

    #[test]
    fn file_http_port_derives_default_daemon_urls() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tray.toml");
        std::fs::write(&path, "http_port = 17849\n").expect("write");

        let s = Settings::load(&path);

        assert_eq!(s.http_port, 17849);
        assert_eq!(s.status_url, "http://127.0.0.1:17849/v1/status");
        assert_eq!(s.solo_web_url, "http://127.0.0.1:17849/desktop/");
    }
}
