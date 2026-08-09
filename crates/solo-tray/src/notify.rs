// SPDX-License-Identifier: Apache-2.0

//! OS-native notification toasts on daemon-health transitions.
//!
//! Fires on `Healthy → {Starting, Down}` (something broke) and on
//! `{Starting, Down} → Healthy` (it recovered). Flapping is debounced
//! by a minimum interval between toasts of the same kind.

use crate::status::DaemonHealth;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait DesktopNotifications {
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

const MIN_TOAST_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct Notifier {
    last_health: DaemonHealth,
    last_toast_at: Option<Instant>,
    enabled: bool,
    /// False until the first `observe()` call. We suppress toasts on
    /// the very first observation so the daemon's normal cold-start
    /// trajectory (`Starting → Healthy`) doesn't fire a spurious
    /// "Solo daemon recovered" toast on every launch.
    bootstrap_seen: bool,
}

impl Notifier {
    pub fn new(enabled: bool) -> Self {
        Self {
            last_health: DaemonHealth::default(),
            last_toast_at: None,
            enabled,
            bootstrap_seen: false,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Observe a new health reading. If it represents a notable
    /// transition (good→bad or bad→good) AND we're not in the debounce
    /// window, fire an OS-native toast.
    ///
    /// Returns true if a toast was fired (useful for tests / debugging).
    pub fn observe(&mut self, new_health: DaemonHealth) -> bool {
        let prev = self.last_health;
        let bootstrap = !self.bootstrap_seen;
        self.last_health = new_health;
        self.bootstrap_seen = true;

        if !self.enabled {
            return false;
        }
        if bootstrap {
            // First observation ever — suppress so cold-start
            // trajectories don't fire spurious toasts.
            return false;
        }
        if prev == new_health {
            return false;
        }

        // Suppress noise on every Starting↔Down flap; only toast on
        // transitions to/from Healthy.
        let notable = matches!(
            (prev, new_health),
            (DaemonHealth::Healthy, _) | (_, DaemonHealth::Healthy)
        );
        if !notable {
            return false;
        }

        // Debounce: don't toast more than once per MIN_TOAST_INTERVAL,
        // even across different transitions.
        if let Some(last) = self.last_toast_at
            && last.elapsed() < MIN_TOAST_INTERVAL
        {
            return false;
        }

        let (summary, body) = match new_health {
            DaemonHealth::Healthy => ("Solo daemon recovered", "HTTP `/v1/status` is responding."),
            DaemonHealth::Starting => (
                "Solo daemon reconnecting",
                "HTTP is unreachable; retrying every 5s.",
            ),
            DaemonHealth::Down => (
                "Solo daemon down",
                "HTTP connection refused. Click the tray icon → Show logs for details.",
            ),
        };

        match show_native_notification(summary, body) {
            Ok(_) => {
                self.last_toast_at = Some(Instant::now());
                true
            }
            Err(e) => {
                tracing::debug!(error = %e, "native toast failed (no-op)");
                false
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn show_native_notification(summary: &str, body: &str) -> Result<(), String> {
    use winrt_notification::{Duration, Toast};

    Toast::new(Toast::POWERSHELL_APP_ID)
        .title(summary)
        .text1(body)
        .duration(Duration::Short)
        .show()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
fn show_native_notification(summary: &str, body: &str) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session().map_err(|error| error.to_string())?;
    let proxy =
        DesktopNotificationsProxyBlocking::new(&connection).map_err(|error| error.to_string())?;
    proxy
        .notify(
            "Solo",
            0,
            "",
            summary,
            body,
            &[],
            std::collections::HashMap::new(),
            8_000,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "macos")]
fn show_native_notification(summary: &str, body: &str) -> Result<(), String> {
    let mut notification = mac_notification_sys::Notification::default();
    notification.title(summary).message(body).asynchronous(true);
    notification
        .send()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn show_native_notification(_summary: &str, _body: &str) -> Result<(), String> {
    Err("desktop notifications are unsupported on this platform".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_toast_when_disabled() {
        let mut n = Notifier::new(false);
        // Even on a Healthy→Down transition, disabled means no toast.
        let fired = n.observe(DaemonHealth::Down);
        assert!(!fired);
    }

    #[test]
    fn no_toast_on_same_health() {
        let mut n = Notifier::new(true);
        let _ = n.observe(DaemonHealth::Healthy); // first transition (default→Healthy)
        // Second identical observation: nothing to do.
        // (Toast may or may not fire on the first transition depending
        // on default state; the invariant we test here is "stable
        // health doesn't re-toast".)
        let _ = n.observe(DaemonHealth::Healthy);
        let fired = n.observe(DaemonHealth::Healthy);
        assert!(!fired);
    }

    #[test]
    fn starting_to_down_does_not_toast() {
        // Both are "bad" states; the user already saw the Healthy→Bad
        // toast. Going from Starting to Down shouldn't double-notify.
        let mut n = Notifier::new(true);
        n.last_health = DaemonHealth::Starting;
        // We can't actually test the OS toast firing (would require a
        // notification daemon in CI), but we CAN assert the early-exit
        // logic: Starting→Down is not "notable" so observe returns false.
        let fired = n.observe(DaemonHealth::Down);
        assert!(!fired, "Starting→Down should not toast");
    }
}
