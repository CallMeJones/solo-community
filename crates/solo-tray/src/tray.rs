// SPDX-License-Identifier: Apache-2.0

//! Tray-icon + menu wiring.
//!
//! The eframe app creates the `TrayIcon` once on first paint and
//! drains menu events from muda's global channel every frame.
//! `tray-icon` (via `muda`) handles the OS-native integration
//! (Win32 / NSStatusItem / AppIndicator). Because eframe does NOT
//! call `update()` on a timer when the primary viewport is hidden,
//! we run a background "repaint pump" thread that calls
//! `Context::request_repaint()` at ~4 Hz — without it, menu clicks
//! land in muda's channel and rot there forever while the user sees
//! a visually live tray icon that does nothing on click.

use crate::daemon::DaemonHandle;
use crate::desktop_window;
use crate::status::DaemonHealth;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// Stable string ids for menu items. We compare event.id against these
/// in the app's update loop.
pub const MENU_OPEN_DESKTOP: &str = "solo.open_desktop";
pub const MENU_OPEN_HEALTH: &str = "solo.open_health";
pub const MENU_OPEN_CONNECTIONS: &str = "solo.open_connections";
pub const MENU_OPEN_MEMORIES: &str = "solo.open_memories";
pub const MENU_OPEN_INBOX: &str = "solo.open_inbox";
pub const MENU_OPEN_IMPORT: &str = "solo.open_import";
pub const MENU_SHOW_LOGS: &str = "solo.show_logs";
pub const MENU_OPEN_WEB: &str = "solo.open_solo_web_browser";
pub const MENU_OPEN_DATA_DIR: &str = "solo.open_data_dir";
pub const MENU_RESTART_DAEMON: &str = "solo.restart_daemon";
pub const MENU_TOGGLE_AUTOSTART: &str = "solo.toggle_autostart";
pub const MENU_TOGGLE_NOTIFICATIONS: &str = "solo.toggle_notifications";
pub const MENU_TOGGLE_THEME: &str = "solo.toggle_theme";
pub const MENU_QUIT: &str = "solo.quit";

/// Side length of the embedded brand PNG. Asserted at decode time so
/// swapping the embedded asset for a differently-sized PNG fails fast
/// rather than producing a stretched icon.
const SOLO_ICON_SIZE: u32 = 32;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The Solo brand tray icon. Black filled circle with a white "S".
/// We tint the dark pixels at runtime to reflect daemon health; the
/// white "S" is preserved so the brand stays legible on every taskbar
/// theme. Source assets live in `crates/solo-tray/assets/`.
const SOLO_ICON_PNG: &[u8] = include_bytes!("../assets/s_tray_icon_32.png");

/// Decoded RGBA bytes for the brand icon. We decode once (PNG parse +
/// crate::image decode aren't free) and reuse the buffer for every
/// frame's tint pass.
fn base_icon_rgba() -> &'static Vec<u8> {
    static CACHE: OnceLock<Vec<u8>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let img = image::load_from_memory_with_format(SOLO_ICON_PNG, image::ImageFormat::Png)
            .expect("decode embedded solo tray icon");
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        assert_eq!(
            w, SOLO_ICON_SIZE,
            "embedded solo tray icon must be {SOLO_ICON_SIZE}x{SOLO_ICON_SIZE} (got {w}x{h})"
        );
        assert_eq!(
            h, SOLO_ICON_SIZE,
            "embedded solo tray icon must be {SOLO_ICON_SIZE}x{SOLO_ICON_SIZE} (got {w}x{h})"
        );
        rgba.into_raw()
    })
}

/// Build the tray icon bytes for `health`. The brand "S" stays white;
/// the dark background tints to green / amber / red. `pulse` (0.5..1.0)
/// modulates brightness during the Starting state for an
/// at-a-glance "I'm working on it" animation.
fn icon_bytes(health: DaemonHealth, pulse: f32) -> Vec<u8> {
    let pulse = pulse.clamp(0.5, 1.0);
    let (tr, tg, tb) = match health {
        DaemonHealth::Healthy => (40, 180, 80),
        DaemonHealth::Starting => (200, 160, 30),
        DaemonHealth::Down => (200, 50, 50),
    };
    let tr = (tr as f32 * pulse) as u8;
    let tg = (tg as f32 * pulse) as u8;
    let tb = (tb as f32 * pulse) as u8;

    let mut out = base_icon_rgba().clone();
    for chunk in out.chunks_exact_mut(4) {
        // Transparent pixels (outside the circle) → leave alone.
        if chunk[3] < 32 {
            continue;
        }
        // Brightness threshold: black background of the brand mark
        // (RGB ~ 0,0,0) gets tinted; the white "S" (RGB ~ 255,255,255)
        // and anti-aliased edges stay white so the letter remains
        // legible on every taskbar theme.
        let brightness = (chunk[0] as u16 + chunk[1] as u16 + chunk[2] as u16) / 3;
        if brightness < 128 {
            chunk[0] = tr;
            chunk[1] = tg;
            chunk[2] = tb;
            // Preserve original alpha for anti-aliased silhouette edges.
        }
    }
    out
}

/// Build a `tray_icon::Icon` for the given daemon health state.
/// `pulse` is the animation brightness factor (1.0 = full, 0.5 = dim).
pub fn icon_for(health: DaemonHealth, pulse: f32) -> tray_icon::Icon {
    let rgba = icon_bytes(health, pulse);
    tray_icon::Icon::from_rgba(rgba, SOLO_ICON_SIZE, SOLO_ICON_SIZE)
        .expect("embedded brand-icon RGBA must be a valid tray icon")
}

/// Construct the tray menu. The menu-event channel is global to the
/// process; `spawn_menu_dispatcher` is the sole receiver.
pub fn build_menu() -> Menu {
    let menu = Menu::new();
    menu.append(&MenuItem::with_id(
        MENU_OPEN_DESKTOP,
        "Open Solo",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(MENU_OPEN_HEALTH, "Health", true, None))
        .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_OPEN_CONNECTIONS,
        "Connected tools",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_OPEN_MEMORIES,
        "Open memories",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_OPEN_INBOX,
        "Review inbox",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_OPEN_IMPORT,
        "Import memory",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(MENU_SHOW_LOGS, "Show logs", true, None))
        .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_OPEN_WEB,
        "Open Desktop in browser (fallback)",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_OPEN_DATA_DIR,
        "Open data dir",
        true,
        None,
    ))
    .expect("menu append");
    // Separator-ish: two visual groups. tray-icon's API doesn't expose
    // a dedicated separator on every backend so we just leave a gap.
    menu.append(&MenuItem::with_id(
        MENU_RESTART_DAEMON,
        "Restart daemon",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_TOGGLE_AUTOSTART,
        "Toggle autostart on login",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_TOGGLE_NOTIFICATIONS,
        "Toggle notifications",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(
        MENU_TOGGLE_THEME,
        "Toggle theme (light/dark)",
        true,
        None,
    ))
    .expect("menu append");
    menu.append(&MenuItem::with_id(MENU_QUIT, "Quit", true, None))
        .expect("menu append");
    menu
}

/// Build the tray icon with the given menu + initial health colour.
/// Tooltip surfaces the daemon health for quick hover-reads.
pub fn build_tray(menu: Menu, health: DaemonHealth) -> Option<TrayIcon> {
    let tooltip = match health {
        DaemonHealth::Healthy => "Solo daemon: healthy",
        DaemonHealth::Starting => "Solo daemon: starting / reconnecting",
        DaemonHealth::Down => "Solo daemon: stopped",
    };
    match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip)
        .with_icon(icon_for(health, 1.0))
        .build()
    {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::error!(error = %e, "failed to build tray icon");
            None
        }
    }
}

/// Spawn a background thread that pokes the egui context every
/// `interval`. We need this because eframe does NOT call `update()`
/// on a timer when the primary viewport is hidden — and the tray's
/// viewport spends most of its life hidden. Without periodic pokes,
/// menu clicks accumulate in muda's channel forever and the user
/// sees nothing happen when they click items in the tray menu.
///
/// The thread runs for the lifetime of the process; the only cost is
/// one 250 ms `request_repaint` + the `update()` body (which short-
/// circuits early when the viewport is hidden — no painting cost).
///
/// Returns nothing; the thread is detached on purpose (it ends when
/// the process exits).
pub fn spawn_repaint_pump(ctx: egui::Context, interval: std::time::Duration) {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        // Idempotent — only the first call spawns the thread. Guards
        // against accidental double-installation in tests.
        return;
    }
    std::thread::Builder::new()
        .name("solo-tray-repaint-pump".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                ctx.request_repaint();
            }
        })
        .expect("spawn repaint pump thread");
}

/// Drain pending menu events for the eframe thread. Returns events
/// in the order they were dispatched. Non-blocking; returns
/// immediately if the queue is empty.
///
/// IMPORTANT: this drains from `FORWARDED_EVENTS`, NOT from muda's
/// own channel. The menu dispatcher thread is the sole consumer of
/// muda's channel — it forwards eframe-owned events into
/// `FORWARDED_EVENTS` for the eframe thread to pick up here.
pub fn drain_menu_events() -> Vec<MenuEvent> {
    drain_forwarded()
}

/// Spawn the background menu dispatcher.
///
/// This thread is the sole consumer of muda's `MenuEvent` channel.
/// It handles "fast-path" menu items DIRECTLY (no dependency on
/// eframe's `update()` loop), and forwards everything else into
/// `FORWARDED_EVENTS` for the eframe thread to pick up on its next
/// repaint tick.
///
/// Why direct dispatch matters: minimised windows on Windows don't
/// receive `WM_PAINT`, so eframe's `update()` stops ticking and
/// anything that depends on a forwarded event sits in the queue
/// until the user manually restores the window. That's why "Open
/// data dir" while minimised silently did nothing until a click on
/// the taskbar restored the window.
///
/// Fast-path items handled here:
///   - `MENU_QUIT` → request supervisor shutdown, wait, then exit
///   - `MENU_RESTART_DAEMON` → request supervisor restart
///   - `MENU_OPEN_WEB` → fallback/debug Desktop URL in browser
///   - Desktop route items → route the owned Solo window
///   - `MENU_OPEN_DATA_DIR` → spawn `explorer.exe` for the data dir
///   - `MENU_SHOW_LOGS` →
///     Win32 ShowWindow + SetForegroundWindow to restore the main
///     viewport without needing eframe to tick first
///
/// Settings toggles still go through the eframe forwarded queue
/// because they need AppState owned by the eframe thread.
///
/// One-shot via `OnceLock`. Subsequent calls are no-ops.
pub fn spawn_menu_dispatcher(
    solo_web_url: String,
    daemon_handle: Arc<tokio::sync::Mutex<DaemonHandle>>,
    runtime_handle: tokio::runtime::Handle,
) {
    static SPAWNED: OnceLock<()> = OnceLock::new();
    if SPAWNED.set(()).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("solo-tray-menu-dispatcher".to_string())
        .spawn(move || {
            let rx = MenuEvent::receiver();
            loop {
                let ev = match rx.recv() {
                    Ok(ev) => ev,
                    Err(_) => return, // muda channel closed
                };
                let id = ev.id.0.as_str();
                match id {
                    MENU_QUIT => {
                        request_graceful_quit(daemon_handle.clone(), runtime_handle.clone());
                    }
                    MENU_RESTART_DAEMON => {
                        let handle = daemon_handle.clone();
                        runtime_handle.spawn(async move {
                            handle.lock().await.request_restart();
                        });
                    }
                    id if let Some(route) = desktop_route_for_menu_id(id) => {
                        open_solo_desktop_route_async(solo_web_url.clone(), route);
                    }
                    MENU_OPEN_WEB => open_solo_web_async(solo_web_url.clone()),
                    MENU_OPEN_DATA_DIR => {
                        let dir = resolve_data_dir();
                        std::thread::spawn(move || {
                            let t = std::time::Instant::now();
                            let res = open_in_file_manager(&dir);
                            match res {
                                Ok(()) => tracing::info!(
                                    dir = %dir.display(),
                                    elapsed_ms = t.elapsed().as_millis() as u64,
                                    "opened data dir (direct)"
                                ),
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    dir = %dir.display(),
                                    "failed to open data dir"
                                ),
                            }
                        });
                    }
                    MENU_SHOW_LOGS => {
                        // Restore the main eframe window via raw
                        // Win32 — no dependency on eframe ticking.
                        #[cfg(windows)]
                        restore_main_window_via_win32();
                        // Forward to eframe so the corresponding
                        // state flag (e.g. `stats_visible`) gets
                        // set on the next tick. By now the window
                        // is no longer minimised, so `update()`
                        // resumes and picks this up almost
                        // immediately.
                        push_forwarded(ev);
                    }
                    _ => {
                        // Slow-path items (Restart, toggles): they
                        // need AppState only the eframe thread
                        // holds, so route them there. They'll fire
                        // once the user brings the window back —
                        // which is now a fast-path action above.
                        push_forwarded(ev);
                    }
                }
            }
        })
        .expect("spawn menu dispatcher thread");
}

/// Request a supervisor-owned daemon shutdown from the dispatcher
/// thread, wait briefly for it to drain, then exit the tray process.
/// This keeps Quit responsive even when the eframe viewport is
/// minimised, without bypassing the daemon supervisor's graceful stop
/// path.
fn request_graceful_quit(
    handle: Arc<tokio::sync::Mutex<DaemonHandle>>,
    runtime_handle: tokio::runtime::Handle,
) {
    std::thread::Builder::new()
        .name("solo-tray-graceful-quit".to_string())
        .spawn(move || {
            tracing::info!("quit requested; asking daemon supervisor to stop");
            let stopped = runtime_handle.block_on(async move {
                handle.lock().await.request_quit();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(12);
                loop {
                    if handle.lock().await.supervisor_exited {
                        return true;
                    }
                    if std::time::Instant::now() >= deadline {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            });
            if stopped {
                tracing::info!("daemon supervisor stopped; exiting tray");
            } else {
                tracing::warn!("daemon supervisor did not report stopped within 12s; exiting tray");
            }
            stop_launched_helpers();
            std::process::exit(0);
        })
        .expect("spawn graceful quit thread");
}

/// Resolve the Solo data dir from `SOLO_DATA_DIR` env or the
/// default ~/.solo path. Duplicated here (rather than calling into
/// `window::solo_data_dir`) so the dispatcher thread has no
/// dependency on the eframe app module.
pub fn resolve_data_dir() -> std::path::PathBuf {
    if let Ok(d) = std::env::var("SOLO_DATA_DIR") {
        return std::path::PathBuf::from(d);
    }
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    };
    home.map(|h| std::path::PathBuf::from(h).join(".solo"))
        .unwrap_or_else(|| std::path::PathBuf::from("."))
}

pub fn desktop_route_file() -> std::path::PathBuf {
    resolve_data_dir().join("desktop-route.txt")
}

/// Open `path` in the native file manager.
pub fn open_in_file_manager(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(path)
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        open::that_detached(path).map_err(|e| std::io::Error::other(e.to_string()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DesktopRoute {
    page: Option<&'static str>,
    event: &'static str,
}

pub(crate) fn desktop_route_for_menu_id(id: &str) -> Option<DesktopRoute> {
    let route = match id {
        MENU_OPEN_DESKTOP => DesktopRoute {
            page: Some("home"),
            event: "opened Solo",
        },
        MENU_OPEN_HEALTH => DesktopRoute {
            page: Some("health"),
            event: "opened Solo health",
        },
        MENU_OPEN_CONNECTIONS => DesktopRoute {
            page: Some("connections"),
            event: "opened Solo connections",
        },
        MENU_OPEN_MEMORIES => DesktopRoute {
            page: Some("memories"),
            event: "opened Solo memories",
        },
        MENU_OPEN_INBOX => DesktopRoute {
            page: Some("inbox"),
            event: "opened Solo inbox",
        },
        MENU_OPEN_IMPORT => DesktopRoute {
            page: Some("import"),
            event: "opened Solo import",
        },
        _ => return None,
    };
    Some(route)
}

pub(crate) fn open_solo_desktop_route_async(solo_web_url: String, route: DesktopRoute) {
    open_solo_desktop_page_async(solo_web_url, route.page, route.event);
}

/// Start a local dev checkout when `SOLO_WEB_URL` points outside the
/// daemon `/desktop/` route, then open the configured Solo URL in
/// an owned webview window. All slow work happens off the UI/menu
/// thread.
pub fn open_solo_desktop_async(solo_web_url: String) {
    open_solo_desktop_route_async(
        solo_web_url,
        desktop_route_for_menu_id(MENU_OPEN_DESKTOP).expect("desktop route"),
    );
}

pub fn open_solo_web_async(solo_web_url: String) {
    open_solo_web_page_async(solo_web_url, None, "opened Solo Web");
}

fn open_solo_web_page_async(solo_web_url: String, page: Option<&'static str>, event: &'static str) {
    spawn_service_worker("solo-tray-open-solo-web", move || {
        let started_at = std::time::Instant::now();
        let launched = match start_solo_web_surface_if_available(&solo_web_url) {
            Ok(Some(command)) => {
                log_solo_web_launch(&command, "started Solo web surface");
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "failed to start Solo web surface");
                false
            }
        };
        if launched {
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
        let url = solo_web_page_url(&solo_web_url, page);
        match open_url_detached(&url) {
            Ok(()) => tracing::info!(
                url = %url,
                surface = event,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                "opened Solo web surface"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                url = %url,
                "failed to open Solo Web"
            ),
        }
    });
}

fn open_solo_desktop_page_async(
    solo_web_url: String,
    page: Option<&'static str>,
    event: &'static str,
) {
    spawn_service_worker("solo-tray-open-solo-desktop", move || {
        let started_at = std::time::Instant::now();
        let launched = match start_solo_web_surface_if_available(&solo_web_url) {
            Ok(Some(command)) => {
                log_solo_web_launch(&command, "started Solo web surface");
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(error = %e, "failed to start Solo web surface");
                false
            }
        };
        if launched {
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
        let url = solo_web_page_url(&solo_web_url, page);
        match open_desktop_window_detached(&url) {
            Ok(Some(command)) => tracing::info!(
                command = %command.display(),
                url = %url,
                surface = event,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                "opened Solo window"
            ),
            Ok(None) => tracing::info!(
                url = %url,
                surface = event,
                elapsed_ms = started_at.elapsed().as_millis() as u64,
                "routed existing Solo window"
            ),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    url = %url,
                    "failed to open Solo window; falling back to browser"
                );
                if let Err(open_error) = open_url_detached(&url) {
                    tracing::warn!(
                        error = %open_error,
                        url = %url,
                        "browser fallback for Solo failed"
                    );
                }
            }
        }
    });
}

fn spawn_service_worker(name: &'static str, work: impl FnOnce() + Send + 'static) {
    if let Err(e) = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(work)
    {
        tracing::warn!(thread = name, error = %e, "failed to spawn tray service worker");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LaunchCommand {
    program: OsString,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
}

impl LaunchCommand {
    fn new(
        program: impl Into<OsString>,
        args: impl IntoIterator<Item = OsString>,
        current_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            current_dir,
        }
    }

    fn spawn_detached(&self) -> std::io::Result<Child> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = &self.current_dir {
            command.current_dir(dir);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        command.spawn()
    }

    fn display(&self) -> String {
        let mut parts = Vec::with_capacity(1 + self.args.len());
        parts.push(self.program.to_string_lossy().into_owned());
        parts.extend(
            self.args
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned()),
        );
        match &self.current_dir {
            Some(dir) => format!("{} (cwd {})", parts.join(" "), dir.display()),
            None => parts.join(" "),
        }
    }
}

fn start_solo_web_if_available(web_url: &str) -> Result<Option<LaunchCommand>, String> {
    launch_once(solo_web_child_slot(), "solo-web", || {
        resolve_solo_web_command(web_url)
    })
}

fn start_solo_web_surface_if_available(web_url: &str) -> Result<Option<LaunchCommand>, String> {
    if is_daemon_desktop_url(web_url) {
        return Ok(None);
    }
    start_solo_web_if_available(web_url)
}

fn log_solo_web_launch(command: &LaunchCommand, message: &'static str) {
    tracing::info!(
        command = %command.display(),
        event = message,
        "Solo web surface ready"
    );
}

fn open_desktop_window_detached(url: &str) -> Result<Option<LaunchCommand>, String> {
    let route_file = desktop_route_file();
    write_desktop_route_file(&route_file, url)?;

    let lock = solo_desktop_window_children_slot().get_or_init(|| Mutex::new(Vec::new()));
    let mut children = lock
        .lock()
        .map_err(|_| "desktop window launch state lock poisoned".to_string())?;
    retain_running_desktop_windows(&mut children);
    if let Some(tracked) = children.first_mut() {
        focus_desktop_window(&tracked.child);
        return Ok(None);
    }
    let command = resolve_solo_desktop_window_command(url, &route_file)?;
    let child = command
        .spawn_detached()
        .map_err(|e| format!("spawn {}: {e}", command.display()))?;
    children.push(TrackedDesktopWindow {
        url: url.to_string(),
        child,
    });
    Ok(Some(command))
}

fn write_desktop_route_file(path: &Path, url: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("desktop route path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create desktop route dir {}: {e}", parent.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let tmp = path.with_extension(format!("tmp-{stamp}"));
    std::fs::write(&tmp, url).map_err(|e| format!("write desktop route {}: {e}", tmp.display()))?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            if let Err(remove_error) = std::fs::remove_file(path)
                && remove_error.kind() != std::io::ErrorKind::NotFound
            {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "replace desktop route {} failed: {first_error}; remove existing route failed: {remove_error}",
                    path.display()
                ));
            }
            std::fs::rename(&tmp, path).map_err(|second_error| {
                let _ = std::fs::remove_file(&tmp);
                format!(
                    "replace desktop route {} failed: {second_error}",
                    path.display()
                )
            })
        }
    }
}

fn retain_running_desktop_windows(children: &mut Vec<TrackedDesktopWindow>) {
    children.retain_mut(|tracked| match tracked.child.try_wait() {
        Ok(None) => true,
        Ok(Some(status)) => {
            tracing::debug!(url = %tracked.url, ?status, "Solo window exited");
            false
        }
        Err(e) => {
            tracing::warn!(
                url = %tracked.url,
                error = %e,
                "could not inspect Solo window; dropping tracked handle"
            );
            false
        }
    });
}

#[cfg(windows)]
fn focus_desktop_window(child: &Child) {
    use std::sync::atomic::{AtomicIsize, Ordering};
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, SW_RESTORE, SetForegroundWindow,
        ShowWindow,
    };

    static FOUND_HWND: AtomicIsize = AtomicIsize::new(0);
    FOUND_HWND.store(0, Ordering::SeqCst);

    extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let target_pid = lparam as u32;
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if pid != target_pid {
            return 1;
        }
        let mut buf = [0u16; 64];
        let len = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if len > 0 {
            let title = String::from_utf16_lossy(&buf[..len as usize]);
            if title == "Solo" {
                FOUND_HWND.store(hwnd as isize, Ordering::SeqCst);
                return 0;
            }
        }
        1
    }

    unsafe {
        EnumWindows(Some(enum_cb), child.id() as LPARAM);
    }
    let hwnd = FOUND_HWND.load(Ordering::SeqCst) as HWND;
    if hwnd.is_null() {
        tracing::debug!(pid = child.id(), "could not find Solo window to focus");
        return;
    }
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }
}

#[cfg(not(windows))]
fn focus_desktop_window(_child: &Child) {
    // Cross-platform focus requires platform-specific IPC/window APIs.
}

fn resolve_solo_desktop_window_command(
    url: &str,
    route_file: &Path,
) -> Result<LaunchCommand, String> {
    let exe = std::env::current_exe().map_err(|e| format!("resolve current executable: {e}"))?;
    Ok(solo_desktop_window_command(exe, url, route_file))
}

fn solo_desktop_window_command(exe: PathBuf, url: &str, route_file: &Path) -> LaunchCommand {
    LaunchCommand::new(
        exe.into_os_string(),
        [
            OsString::from(desktop_window::DESKTOP_WINDOW_ARG),
            OsString::from(desktop_window::DESKTOP_URL_ARG),
            OsString::from(url),
            OsString::from(desktop_window::DESKTOP_ROUTE_FILE_ARG),
            route_file.as_os_str().to_os_string(),
        ],
        None,
    )
}

fn launch_once(
    slot: &'static OnceLock<Mutex<Option<Child>>>,
    service: &'static str,
    resolve: impl FnOnce() -> Option<LaunchCommand>,
) -> Result<Option<LaunchCommand>, String> {
    let lock = slot.get_or_init(|| Mutex::new(None));
    let mut child_slot = lock
        .lock()
        .map_err(|_| "service launch state lock poisoned".to_string())?;
    if let Some(child) = child_slot.as_mut() {
        match child.try_wait() {
            Ok(None) => return Ok(None),
            Ok(Some(status)) => {
                tracing::info!(service, ?status, "previous helper exited; allowing restart");
                *child_slot = None;
            }
            Err(e) => {
                tracing::warn!(
                    service,
                    error = %e,
                    "could not inspect previous helper; allowing restart"
                );
                *child_slot = None;
            }
        }
    }
    let Some(command) = resolve() else {
        return Ok(None);
    };
    let child = command
        .spawn_detached()
        .map_err(|e| format!("spawn {}: {e}", command.display()))?;
    *child_slot = Some(child);
    Ok(Some(command))
}

/// Stop helpers the tray started. Best-effort; external processes are untouched.
pub fn stop_launched_helpers() {
    stop_launched_helper(solo_web_child_slot(), "solo-web");
    stop_desktop_windows();
}

fn solo_web_child_slot() -> &'static OnceLock<Mutex<Option<Child>>> {
    static SLOT: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
    &SLOT
}

struct TrackedDesktopWindow {
    url: String,
    child: Child,
}

fn solo_desktop_window_children_slot() -> &'static OnceLock<Mutex<Vec<TrackedDesktopWindow>>> {
    static SLOT: OnceLock<Mutex<Vec<TrackedDesktopWindow>>> = OnceLock::new();
    &SLOT
}

fn stop_desktop_windows() {
    let Some(lock) = solo_desktop_window_children_slot().get() else {
        return;
    };
    let Ok(mut children) = lock.lock() else {
        tracing::warn!("could not lock Solo child list during shutdown");
        return;
    };
    for mut tracked in children.drain(..) {
        match tracked.child.try_wait() {
            Ok(Some(status)) => {
                tracing::debug!(
                    url = %tracked.url,
                    ?status,
                    "Solo window already exited"
                );
            }
            Ok(None) => match tracked.child.kill() {
                Ok(()) => tracing::info!(url = %tracked.url, "stopped Solo window"),
                Err(e) => tracing::warn!(
                    url = %tracked.url,
                    error = %e,
                    "failed to stop Solo window"
                ),
            },
            Err(e) => tracing::warn!(
                url = %tracked.url,
                error = %e,
                "failed to inspect Solo window"
            ),
        }
    }
}

fn stop_launched_helper(slot: &'static OnceLock<Mutex<Option<Child>>>, service: &'static str) {
    let Some(lock) = slot.get() else {
        return;
    };
    let Ok(mut child_slot) = lock.lock() else {
        tracing::warn!(service, "could not lock helper child slot during shutdown");
        return;
    };
    let Some(mut child) = child_slot.take() else {
        return;
    };
    match child.try_wait() {
        Ok(Some(status)) => {
            tracing::debug!(service, ?status, "helper already exited");
        }
        Ok(None) => match child.kill() {
            Ok(()) => tracing::info!(service, "stopped tray-started helper"),
            Err(e) => tracing::warn!(service, error = %e, "failed to stop tray-started helper"),
        },
        Err(e) => tracing::warn!(service, error = %e, "failed to inspect tray-started helper"),
    }
}

fn resolve_solo_web_command(web_url: &str) -> Option<LaunchCommand> {
    let dir = find_monorepo_web_project()?;
    let vite = dir
        .join("node_modules")
        .join("vite")
        .join("bin")
        .join("vite.js");
    if !vite.is_file() {
        tracing::info!(
            dir = %dir.display(),
            "apps/web source found but Vite entrypoint is missing"
        );
        return None;
    }
    Some(solo_web_dev_command(dir, web_url))
}

fn solo_web_dev_command(project_dir: PathBuf, web_url: &str) -> LaunchCommand {
    let mut args = vec![
        project_dir
            .join("node_modules")
            .join("vite")
            .join("bin")
            .join("vite.js")
            .into_os_string(),
        OsString::from("--host"),
        OsString::from("127.0.0.1"),
    ];
    if let Some(port) = port_from_url(web_url) {
        args.push(OsString::from("--port"));
        args.push(OsString::from(port.to_string()));
    }
    LaunchCommand::new(node_program(), args, Some(project_dir))
}

fn node_program() -> OsString {
    OsString::from(if cfg!(windows) { "node.exe" } else { "node" })
}

fn open_url_detached(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        open_url_command(url).spawn_detached().map(|_| ())
    }
    #[cfg(not(windows))]
    {
        open::that_detached(url).map_err(|e| std::io::Error::other(e.to_string()))
    }
}

#[cfg(windows)]
fn open_url_command(url: &str) -> LaunchCommand {
    LaunchCommand::new(OsString::from("explorer.exe"), [OsString::from(url)], None)
}

fn find_monorepo_web_project() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
        && let Some(found) = find_monorepo_web_project_from(parent)
    {
        return Some(found);
    }
    std::env::current_dir()
        .ok()
        .and_then(|dir| find_monorepo_web_project_from(&dir))
}

fn find_monorepo_web_project_from(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let candidate = ancestor.join("apps").join("web");
        if candidate.join("package.json").is_file() {
            return Some(candidate);
        }
    }
    None
}

fn port_from_url(url: &str) -> Option<u16> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let port = if authority.starts_with('[') {
        let after_bracket = authority.split_once(']')?.1;
        after_bracket.strip_prefix(':')?
    } else {
        authority.rsplit_once(':')?.1
    };
    port.parse().ok()
}

fn is_daemon_desktop_url(url: &str) -> bool {
    let Some(("http", rest)) = url.split_once("://") else {
        return false;
    };
    if port_from_url(url).is_none() {
        return false;
    }
    let authority_end = rest
        .char_indices()
        .find(|(_, ch)| matches!(ch, '/' | '?' | '#'))
        .map(|(idx, _)| idx)
        .unwrap_or(rest.len());
    let authority = rest[..authority_end]
        .rsplit('@')
        .next()
        .unwrap_or(&rest[..authority_end]);
    let host = if authority.starts_with('[') {
        authority
            .find(']')
            .map(|idx| &authority[..=idx])
            .unwrap_or(authority)
    } else {
        authority
            .rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(authority)
    };
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return false;
    }
    let suffix = &rest[authority_end..];
    let without_hash = suffix
        .split_once('#')
        .map(|(before_hash, _)| before_hash)
        .unwrap_or(suffix);
    let path = without_hash
        .split_once('?')
        .map(|(before_query, _)| before_query)
        .unwrap_or(without_hash);
    path == "/desktop" || path.starts_with("/desktop/")
}

fn solo_web_page_url(base_url: &str, page: Option<&str>) -> String {
    let base = base_url
        .split_once('#')
        .map(|(before_hash, _)| before_hash)
        .unwrap_or(base_url)
        .trim_end_matches('/');
    match page {
        Some(page) => format!("{base}/#{page}"),
        None => base.to_string(),
    }
}

/// Walk this process's top-level windows looking for the eframe
/// main viewport (matched by exact title "Solo Controls"), then restore
/// it from a minimised state and bring it to the foreground. Uses
/// raw Win32 because eframe's own `ViewportCommand::Minimized(false)`
/// is processed inside `update()`, which doesn't run when the
/// viewport is currently minimised.
#[cfg(windows)]
fn restore_main_window_via_win32() {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, SW_RESTORE, SetForegroundWindow,
        ShowWindow,
    };

    // Use a static slot so the C callback can publish the HWND back
    // to us without capturing state. `i64` so we can stuff a HWND
    // pointer in losslessly on both x86_64 and aarch64.
    use std::sync::atomic::{AtomicIsize, Ordering};
    static FOUND_HWND: AtomicIsize = AtomicIsize::new(0);
    FOUND_HWND.store(0, Ordering::SeqCst);

    extern "system" fn enum_cb(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let target_pid = lparam as u32;
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if pid != target_pid {
            return 1; // continue
        }
        // Match by exact title — tray-icon's internal message
        // window has no title, so this filters cleanly.
        let mut buf = [0u16; 64];
        let len = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if len > 0 {
            let title = String::from_utf16_lossy(&buf[..len as usize]);
            if title == "Solo Controls" {
                FOUND_HWND.store(hwnd as isize, Ordering::SeqCst);
                return 0; // stop enum
            }
        }
        1 // continue
    }

    let pid = unsafe { GetCurrentProcessId() };
    unsafe {
        EnumWindows(Some(enum_cb), pid as LPARAM);
    }
    let hwnd = FOUND_HWND.load(Ordering::SeqCst) as HWND;
    if hwnd.is_null() {
        tracing::warn!("could not find main viewport HWND for restore");
        return;
    }
    // SW_RESTORE = 9. Restores from minimised AND brings the
    // window to its previous size/position.
    unsafe {
        ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd);
    }
}

/// Queue of menu events the dispatcher forwards to the eframe thread
/// after consuming them from muda's global receiver.
static FORWARDED_EVENTS: OnceLock<std::sync::Mutex<Vec<MenuEvent>>> = OnceLock::new();

fn push_forwarded(ev: MenuEvent) {
    let q = FORWARDED_EVENTS.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    if let Ok(mut g) = q.lock() {
        g.push(ev);
    }
}

/// Drain any events the dispatcher forwarded. Called by
/// `drain_menu_events`.
fn drain_forwarded() -> Vec<MenuEvent> {
    match FORWARDED_EVENTS.get() {
        Some(q) => match q.lock() {
            Ok(mut g) => std::mem::take(&mut *g),
            Err(_) => Vec::new(),
        },
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn solo_web_dev_command_uses_node_entrypoint_and_url_port() {
        let project_dir = PathBuf::from(r"C:\dev\solo-community\apps\web");
        let command = solo_web_dev_command(project_dir.clone(), "http://localhost:5179");

        assert_eq!(command.program, node_program());
        assert_eq!(command.current_dir, Some(project_dir.clone()));
        assert_eq!(
            arg_strings(&command.args),
            vec![
                project_dir
                    .join("node_modules")
                    .join("vite")
                    .join("bin")
                    .join("vite.js")
                    .display()
                    .to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "5179".to_string(),
            ]
        );
    }

    #[test]
    fn monorepo_web_search_climbs_out_of_target_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let nested = root.join("repo").join("target").join("debug");
        let web = root.join("repo").join("apps").join("web");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::create_dir_all(&web).expect("web");
        std::fs::write(web.join("package.json"), "{}").expect("package");

        assert_eq!(find_monorepo_web_project_from(&nested), Some(web));
    }

    #[test]
    fn monorepo_web_search_ignores_sibling_checkout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let nested = root.join("repo").join("target").join("debug");
        let wrong = root.join("solo-web");
        std::fs::create_dir_all(&nested).expect("nested");
        std::fs::create_dir_all(&wrong).expect("wrong");
        std::fs::write(wrong.join("package.json"), "{}").expect("package");

        assert_eq!(find_monorepo_web_project_from(&nested), None);
    }

    #[test]
    fn port_parser_handles_loopback_urls() {
        assert_eq!(port_from_url("http://127.0.0.1:5173"), Some(5173));
        assert_eq!(port_from_url("http://localhost:7438/health"), Some(7438));
        assert_eq!(port_from_url("http://[::1]:5173/app"), Some(5173));
        assert_eq!(port_from_url("https://solo.dev/web"), None);
    }

    #[test]
    fn daemon_desktop_url_detection_is_loopback_desktop_path() {
        assert!(is_daemon_desktop_url("http://127.0.0.1:17821/desktop/"));
        assert!(is_daemon_desktop_url(
            "http://localhost:17821/desktop/#home"
        ));
        assert!(is_daemon_desktop_url("http://[::1]:17821/desktop/memories"));
        assert!(is_daemon_desktop_url("http://127.0.0.1:17849/desktop/"));
        assert!(!is_daemon_desktop_url("http://127.0.0.1:17821/desktopish"));
        assert!(!is_daemon_desktop_url("https://127.0.0.1:17821/desktop/"));
        assert!(!is_daemon_desktop_url("http://example.com:17821/desktop/"));
    }

    #[test]
    fn solo_desktop_window_command_uses_current_exe_and_url_arg_vector() {
        let exe = PathBuf::from(r"C:\dev\solo-tray.exe");
        let url = "http://127.0.0.1:17821/desktop/#home";
        let route_file = PathBuf::from(r"C:\Users\Example\.solo\desktop-route.txt");
        let command = solo_desktop_window_command(exe.clone(), url, &route_file);

        assert_eq!(command.program, exe.into_os_string());
        assert_eq!(command.current_dir, None);
        assert_eq!(
            arg_strings(&command.args),
            vec![
                desktop_window::DESKTOP_WINDOW_ARG.to_string(),
                desktop_window::DESKTOP_URL_ARG.to_string(),
                url.to_string(),
                desktop_window::DESKTOP_ROUTE_FILE_ARG.to_string(),
                route_file.display().to_string(),
            ]
        );
    }

    #[test]
    fn desktop_route_file_write_replaces_existing_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let route_file = temp.path().join("nested").join("desktop-route.txt");

        write_desktop_route_file(&route_file, "http://127.0.0.1:17821/desktop/#home")
            .expect("write first route");
        write_desktop_route_file(&route_file, "http://127.0.0.1:17821/desktop/#inbox")
            .expect("replace route");

        assert_eq!(
            std::fs::read_to_string(route_file).expect("read route"),
            "http://127.0.0.1:17821/desktop/#inbox"
        );
    }

    #[test]
    fn solo_web_page_urls_replace_existing_hash() {
        assert_eq!(
            solo_web_page_url("http://localhost:5173", Some("memories")),
            "http://localhost:5173/#memories"
        );
        assert_eq!(
            solo_web_page_url("http://localhost:5173/#inbox", Some("home")),
            "http://localhost:5173/#home"
        );
        assert_eq!(
            solo_web_page_url("http://127.0.0.1:17821/desktop/", Some("connections")),
            "http://127.0.0.1:17821/desktop/#connections"
        );
        assert_eq!(
            solo_web_page_url("http://localhost:5173/", None),
            "http://localhost:5173"
        );
    }

    #[test]
    fn desktop_route_menu_items_map_to_pages() {
        let cases = [
            (MENU_OPEN_DESKTOP, Some("home")),
            (MENU_OPEN_HEALTH, Some("health")),
            (MENU_OPEN_CONNECTIONS, Some("connections")),
            (MENU_OPEN_MEMORIES, Some("memories")),
            (MENU_OPEN_INBOX, Some("inbox")),
            (MENU_OPEN_IMPORT, Some("import")),
        ];

        for (id, expected_page) in cases {
            let route = desktop_route_for_menu_id(id).expect("desktop route");
            assert_eq!(route.page, expected_page, "{id}");
        }

        assert!(desktop_route_for_menu_id(MENU_OPEN_WEB).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_url_open_command_uses_explorer_arg_vector() {
        let command = open_url_command("http://localhost:5173/?q=a&x=b");

        assert_eq!(command.program, OsString::from("explorer.exe"));
        assert_eq!(
            arg_strings(&command.args),
            vec!["http://localhost:5173/?q=a&x=b".to_string()]
        );
    }
}
