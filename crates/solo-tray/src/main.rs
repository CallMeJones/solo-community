#![cfg_attr(windows, windows_subsystem = "windows")]
// SPDX-License-Identifier: Apache-2.0

//! `solo-tray` — system-tray companion for `solo daemon`.
//!
//! Spawns `solo daemon` as a child process, captures its stderr into a
//! ring buffer the user can view, polls `/v1/status` for health, and
//! exposes a tray-icon menu for the most common operator actions
//! (show logs, open Solo, open data dir, restart, quit-gracefully).
//!
//! See `docs/dev-log/0157-solo-tray-scoping.md` for the design rationale
//! and the MVP feature list.

mod autostart;
mod daemon;
mod desktop_window;
mod logs;
mod notify;
mod secret_store;
mod settings;
mod single_instance;
mod status;
mod tray;
mod window;

use anyhow::{Context, Result, bail};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Capacity of the in-memory log ring buffer. 8 KiB worth of lines
/// (~200 lines at typical tracing length) is enough for "what happened
/// in the last minute" while staying bounded.
const LOG_BUFFER_LINES: usize = 200;

/// Default `/v1/status` URL the tray polls.
const DEFAULT_STATUS_URL: &str = "http://127.0.0.1:17821/v1/status";

/// Solo URL the tray opens from the tray and owned webview.
/// Configurable via `SOLO_WEB_URL` env var; defaults to the daemon-hosted
/// Solo app route so the UI and API share one local origin.
const DEFAULT_SOLO_WEB_URL: &str = "http://127.0.0.1:17821/desktop/";
const LEGACY_DEV_SOLO_WEB_URL: &str = "http://127.0.0.1:5173";
const LEGACY_PACKAGED_SOLO_WEB_URL: &str = "http://127.0.0.1:17822";

/// Status-poll cadence. 5 s is fast enough to feel live without burning
/// CPU on a Solo that's mostly idle.
const STATUS_POLL_SECS: u64 = 5;
const TRAY_LOG_MAX_BYTES: u64 = 5 * 1024 * 1024;

fn main() -> Result<()> {
    // Early --version / -V / --help short-circuit so the publish-pipeline
    // smoke step (and any scripted invocation) can ask "what is this
    // binary" without spinning up eframe + tray-icon. Hand-rolled rather
    // than clap because the tray's only "args" are these — pulling in
    // clap would add ~200 KB to the binary for two strings.
    let argv: Vec<String> = std::env::args().collect();
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!(
            "solo-tray {}",
            solo_core::build_info::version_with_build_metadata()
        );
        return Ok(());
    }
    if desktop_window::args_request_desktop_window(&argv) {
        init_tracing();
        detach_console_for_tray();
        let default_url = settings::Settings::load(&settings::settings_path()).solo_web_url;
        let url = desktop_window::url_from_args(&argv, &default_url)?;
        let route_file = desktop_window::route_file_from_args(&argv, &tray::desktop_route_file())?;
        let smoke_report_file = desktop_window::smoke_report_file_from_args(&argv)?;
        return desktop_window::run(url, route_file, smoke_report_file);
    }
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            concat!(
                "solo-tray {ver}\n",
                "System-tray companion for `solo daemon`.\n\n",
                "Usage: solo-tray [OPTIONS]\n\n",
                "Options:\n",
                "  --version, -V  Print version and exit\n",
                "  --help, -h     Print this help and exit\n",
                "  --desktop-window --desktop-url <URL> [--desktop-route-file <PATH>]\n",
                "                 Open Solo in an owned webview window\n\n",
                "  --desktop-smoke-report <PATH>\n",
                "                 Write smoke-only Solo readiness JSONL\n\n",
                "Settings are read from `<SOLO_DATA_DIR>/tray.toml`; env-var\n",
                "overrides include SOLO_HTTP_PORT, SOLO_WEB_URL,\n",
                "SOLO_TRAY_STATUS_URL, SOLO_TRAY_NOTIFICATIONS_ENABLED,\n",
                "SOLO_TRAY_THEME.\n"
            ),
            ver = solo_core::build_info::version_with_build_metadata()
        );
        return Ok(());
    }

    // Tracing for the tray itself (separate from the daemon child's
    // captured stderr). Routes to BOTH stderr (visible while the
    // console is still attached) and `~/.solo/tray.log` (the only
    // surviving destination after `detach_console_for_tray`
    // runs). Without the file sink, every tracing call below the
    // passphrase prompt — including the entire eframe lifecycle —
    // would be silently dropped, which made the "menu does nothing"
    // bug all but undebuggable on the first round.
    init_tracing();

    detach_console_for_tray();

    let _instance_guard = match single_instance::InstanceGuard::acquire()
        .context("acquire solo-tray single-instance guard")?
    {
        Some(guard) => guard,
        None => {
            tracing::warn!("another solo-tray instance is already running; exiting");
            return Ok(());
        }
    };

    let settings_path = settings::settings_path();
    let settings = settings::Settings::load(&settings_path);
    tracing::info!(
        path = %settings_path.display(),
        notifications = settings.notifications_enabled,
        autostart = settings.autostart_on_login,
        "tray settings loaded"
    );

    let mut initial_passphrase =
        initial_passphrase_from_env().context("read inherited daemon passphrase")?;
    if initial_passphrase.is_none() && settings.remember_passphrase_in_keychain {
        match secret_store::load_daemon_passphrase() {
            Ok(Some(passphrase)) => {
                tracing::info!(
                    backend = secret_store::backend_label(),
                    "using daemon passphrase from OS keychain"
                );
                initial_passphrase = Some(passphrase);
            }
            Ok(None) => {
                tracing::info!(
                    backend = secret_store::backend_label(),
                    "OS keychain daemon passphrase is not stored"
                );
            }
            Err(error) => {
                tracing::warn!(
                    backend = secret_store::backend_label(),
                    error = %error,
                    "read OS keychain daemon passphrase failed"
                );
            }
        }
    }

    let log_buffer = Arc::new(Mutex::new(logs::RingBuffer::new(LOG_BUFFER_LINES)));
    let daemon_handle = Arc::new(Mutex::new(daemon::DaemonHandle::default()));
    let status_state = Arc::new(Mutex::new(status::StatusState::default()));
    let notifier = Arc::new(Mutex::new(notify::Notifier::new(
        settings.notifications_enabled,
    )));

    // tokio runtime for the child-process supervisor + HTTP poller.
    // Held for the lifetime of the tray; spawned tasks own clones of
    // the shared state via Arc.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .context("build tokio runtime for solo-tray")?;

    // Spawn the /v1/status poller. It stays quiet while the tray is
    // still locked, then calls into the notifier on health transitions.
    {
        let status = status_state.clone();
        let notif = notifier.clone();
        let status_url = settings.status_url.clone();
        let daemon = daemon_handle.clone();
        runtime.spawn(async move {
            status::poll_loop_with_notify(status, &status_url, notif, daemon).await;
        });
    }

    // Enter the eframe event loop. The tray is created INSIDE the
    // eframe app's `new()` callback so the tray's event channel
    // dispatches on the same thread as egui. (tray-icon's menu events
    // and click events need a winit-compatible event loop, which
    // eframe provides under the hood.)
    let app_state = window::AppState {
        log_buffer,
        daemon_handle,
        status_state,
        notifier,
        settings,
        settings_path,
        runtime_handle: runtime.handle().clone(),
        initial_passphrase,
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 620.0])
            .with_title("Solo Controls")
            .with_icon(window_icon()),
        // NB: do NOT pass `with_visible(false)` here. winit's
        // `Window::request_redraw` on Windows is a no-op for windows
        // that are not currently visible (no `WM_PAINT` is issued),
        // so eframe's `update()` never fires for a hidden viewport
        // and the tray-menu channel never gets drained. Result:
        // every tray menu item silently does nothing. The log
        // viewer therefore starts visible; the user closes via the
        // X button (we trap close and minimise instead of quitting,
        // see `SoloTrayApp::update`).
        //
        // We previously tried `with_taskbar(false)` + an off-screen
        // hide position to avoid the minimised taskbar entry; the
        // combination set `WS_EX_TOOLWINDOW` and broke restore on
        // Show-logs. Standard minimise leaves a taskbar entry but
        // restore-from-taskbar-AND-from-tray both work reliably.
        ..Default::default()
    };

    // `tray-icon` is GTK-backed on Linux and requires GTK to be initialized
    // on the thread that creates the tray. `SoloTrayApp::new` builds the
    // tray, and eframe calls it on this thread, so GTK has to come up here
    // first. eframe wraps winit, which drives X11/Wayland directly and never
    // initializes GTK — without this, `Menu::new()` trips gtk-rs's
    // `assert_initialized_main_thread!()` and the process aborts on launch.
    // See docs/adr/0016-linux-gtk-initialization.md.
    #[cfg(target_os = "linux")]
    gtk::init().map_err(|e| {
        anyhow::anyhow!(
            "GTK initialization failed, so the Solo tray cannot start: {e}. \
             Solo needs a graphical session (DISPLAY or WAYLAND_DISPLAY)."
        )
    })?;

    eframe::run_native(
        "Solo Controls",
        native_options,
        Box::new(move |cc| Ok(Box::new(window::SoloTrayApp::new(cc, app_state)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe::run_native failed: {e}"))?;

    // Normal tray-menu Quit exits from the menu dispatcher after the
    // supervisor drains. If eframe returns by another path, let the
    // runtime drop and the child cleanup paths do their best.
    Ok(())
}

/// Initialise tracing for the tray process: stderr AND a file at
/// `<data_dir>/tray.log`. After `detach_console_for_tray`
/// runs (Windows), stderr writes go to a void — the file sink is the
/// only surviving destination, so we can still triage post-mortems.
///
/// Both sinks honor `RUST_LOG`; default level is `info`. The file is
/// appended (not truncated) so a tray that crash-restarts via the
/// installer's Run-key autostart preserves history across runs.
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let filter = || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stderr_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_filter(filter());

    // Try to open the log file. If we can't (no writable data dir,
    // disk full, etc.), fall back to stderr-only — better than
    // panicking before tracing is initialised.
    let path = logs::tray_log_path();
    let file_layer = {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        rotate_log_file(&path, TRAY_LOG_MAX_BYTES);
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|file| {
                fmt::layer()
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(file))
                    .with_filter(filter())
            })
    };

    let subscriber = tracing_subscriber::registry().with(stderr_layer);
    match file_layer {
        Some(file_layer) => subscriber.with(file_layer).init(),
        None => subscriber.init(),
    }
}

fn rotate_log_file(path: &std::path::Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let rotated = path.with_extension("log.1");
    let _ = std::fs::remove_file(&rotated);
    if let Err(e) = std::fs::rename(path, &rotated) {
        tracing::warn!(
            path = %path.display(),
            rotated = %rotated.display(),
            error = %e,
            "rotate tray log failed"
        );
    }
}

/// Detach the tray process from its console (Windows only).
///
/// Release Windows builds use the GUI subsystem, but this is still
/// harmless in debug/custom console builds. Detaching here closes any
/// inherited console because the passphrase prompt lives in egui.
///
/// Stderr output may be dropped after this call (no console to write
/// to), but tray-process tracing also goes to `<data_dir>/tray.log`.
/// The daemon's stderr remains visible through "Show logs".
#[cfg(windows)]
fn detach_console_for_tray() {
    use windows_sys::Win32::System::Console::FreeConsole;
    // SAFETY: `FreeConsole` has no preconditions and is safe to call
    // even when no console is attached (it just returns 0 with a
    // last-error of "no console allocated"). We ignore the return.
    unsafe {
        let _ = FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_console_for_tray() {
    // No-op on non-Windows: Unix-style terminals don't auto-close
    // when the foreground process disconnects, and we'd need a
    // setsid + double-fork to achieve a similar effect. The user can
    // background the process themselves (`solo-tray &` + `disown`).
}

/// Decode the embedded brand PNG into an `egui::IconData` for the
/// eframe window. Shown in the taskbar / task switcher when the log
/// viewer is open.
fn window_icon() -> egui::IconData {
    const ICON_PNG: &[u8] = include_bytes!("../assets/s_tray_icon_64.png");
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png)
        .expect("decode embedded solo window icon");
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    egui::IconData {
        rgba: rgba.into_raw(),
        width: w,
        height: h,
    }
}

fn initial_passphrase_from_env() -> Result<Option<zeroize::Zeroizing<String>>> {
    let Ok(existing) = std::env::var(daemon::ENV_PASSPHRASE) else {
        return Ok(None);
    };
    let existing = zeroize::Zeroizing::new(existing);
    if existing.is_empty() {
        bail!(
            "{} is set but empty; unset it or set a real passphrase before launching solo-tray",
            daemon::ENV_PASSPHRASE
        );
    }
    unsafe {
        std::env::remove_var(daemon::ENV_PASSPHRASE);
    }
    tracing::info!(
        "using inherited {} once; removed it from tray environment",
        daemon::ENV_PASSPHRASE
    );
    Ok(Some(existing))
}
