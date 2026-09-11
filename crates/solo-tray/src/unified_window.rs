// SPDX-License-Identifier: Apache-2.0
//! One visible window from local unlock to the daemon-hosted workspace.
//! Privileged messages are accepted only from immutable bundled startup HTML.
//! The memory web app has no unlock IPC privilege, and no secret getter exists.

use crate::{daemon, secret_store, tray, window::AppState};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    borrow::Cow,
    time::{Duration, Instant, SystemTime},
};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    window::WindowBuilder,
};
use wry::{WebViewBuilder, http::Response};
use zeroize::Zeroizing;

const START_URL: &str = "solo://app/index.html";
const START_HTML: &str = include_str!("unified_start.html");
// Wry maps custom protocols to an HTTP origin on Windows. Its load_url API
// performs that mapping, but links clicked inside an HTTP document do not.
#[cfg(target_os = "windows")]
const DESKTOP_INIT: &str = r#"
window.__SOLO_DESKTOP__ = true;
document.addEventListener('click', event => {
    const link = event.target instanceof Element ? event.target.closest('a') : null;
    if (link?.getAttribute('href') === 'solo://app/index.html#settings') {
        event.preventDefault();
        window.location.assign('http://solo.app/index.html#settings');
    }
});
"#;
#[cfg(not(target_os = "windows"))]
const DESKTOP_INIT: &str = "window.__SOLO_DESKTOP__=true;";

enum Message {
    Command(Command),
    Initialized(Result<Zeroizing<String>, String>),
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Unlock {
        passphrase: String,
        #[serde(default)]
        confirm: String,
        #[serde(default)]
        create: bool,
        #[serde(default)]
        remember: bool,
    },
    Open {},
    Restart {},
    Forget {},
    Autostart {
        enabled: bool,
    },
    Quit {},
    CheckUpdate {},
    DownloadUpdate {},
    ApplyUpdate {},
    SetEdition {
        edition: String,
    },
    SignIn {},
}

fn trusted_start_url(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else {
        return false;
    };
    let authority = (url.scheme(), url.host_str(), url.port());
    matches!(
        authority,
        ("solo", Some("app"), None) | ("http", Some("solo.app"), None)
    ) && url.path() == "/index.html"
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
}

pub fn run(mut state: AppState) -> Result<()> {
    let mut desktop_url = reqwest::Url::parse(&state.settings.solo_web_url)?;
    if desktop_url.scheme() != "http"
        || !matches!(
            desktop_url.host_str(),
            Some("127.0.0.1" | "localhost" | "[::1]")
        )
        || !desktop_url.username().is_empty()
        || desktop_url.password().is_some()
    {
        bail!("Solo desktop requires a loopback HTTP app URL");
    }
    desktop_url.set_fragment(Some("memories"));
    let origin = desktop_url.origin();
    let status_url = reqwest::Url::parse(&state.settings.status_url)?;
    if status_url.origin() != origin
        || desktop_url.port_or_known_default() != Some(state.settings.http_port)
        || desktop_url.path() != "/desktop/"
        || status_url.path() != "/v1/status"
    {
        bail!(
            "The unified window requires the app and status URLs of its local daemon. Use --legacy-controls for custom development endpoints."
        );
    }
    let desktop_url = desktop_url.to_string();
    let event_loop = EventLoopBuilder::<Message>::with_user_event().build();
    let icon = crate::window_icon();
    let window = WindowBuilder::new()
        .with_title("Solo")
        .with_window_icon(
            tao::window::Icon::from_rgba(icon.rgba.clone(), icon.width, icon.height).ok(),
        )
        .with_inner_size(LogicalSize::new(1440.0, 1024.0))
        .with_min_inner_size(LogicalSize::new(900.0, 620.0))
        .build(&event_loop)?;
    let proxy = event_loop.create_proxy();
    let ipc_proxy = proxy.clone();
    let builder = WebViewBuilder::new()
        .with_custom_protocol("solo".into(), |_id, request| {
            let valid = trusted_start_url(&request.uri().to_string());
            Response::builder().status(if valid {200} else {404})
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Content-Security-Policy", "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; form-action 'none'; frame-src 'none'; base-uri 'none'")
                .body(Cow::Borrowed(if valid {START_HTML.as_bytes()} else {b"Not found"})).expect("static response")
        })
        .with_navigation_handler(move |url| trusted_start_url(&url) || reqwest::Url::parse(&url).is_ok_and(|u|u.origin()==origin && u.path().starts_with("/desktop/") && u.username().is_empty() && u.password().is_none()))
        .with_new_window_req_handler(|_,_|wry::NewWindowResponse::Deny)
        .with_initialization_script(DESKTOP_INIT)
        .with_ipc_handler(move |request| {
            if !trusted_start_url(&request.uri().to_string()) || request.body().len()>32768 { return; }
            let body = Zeroizing::new(request.into_body());
            if let Ok(command) = serde_json::from_str::<Command>(&body) { let _ = ipc_proxy.send_event(Message::Command(command)); }
        }).with_url(START_URL);
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let webview = builder
        .build(&window)
        .context("create unified Solo window")?;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder
            .build_gtk(window.default_vbox().context("Solo GTK container")?)
            .context("create unified Solo window")?
    };
    let menu = tray_icon::menu::Menu::new();
    for (id, label) in [
        (tray::MENU_OPEN_DESKTOP, "Open Solo"),
        (tray::MENU_OPEN_MEMORIES, "Memories"),
        (tray::MENU_OPEN_INBOX, "Inbox"),
        (tray::MENU_OPEN_CONNECTIONS, "Connected apps"),
        (tray::MENU_OPEN_HEALTH, "Diagnostics"),
        (tray::MENU_SHOW_LOGS, "Logs"),
        ("solo.device_settings", "Updates & device settings"),
        (tray::MENU_QUIT, "Quit Solo"),
    ] {
        menu.append(&tray_icon::menu::MenuItem::with_id(id, label, true, None))?;
    }
    let tray_icon = tray::build_tray(menu, crate::status::DaemonHealth::Starting);
    let data_dir = tray::resolve_data_dir();
    let mut busy = false;
    let mut remember_after_unlock = false;
    let mut pending_secret: Option<Zeroizing<String>> = None;
    let mut message = String::new();
    let mut entered_workspace = false;
    let mut quitting = false;
    let mut initializing = false;
    let mut ready = false;
    let mut observed_pid = None;
    let mut last_tray_health = crate::status::DaemonHealth::Starting;
    let mut ready_after = SystemTime::now();
    let mut last_tick = Instant::now();
    let mut updates = crate::unified_updates::Updates::default();
    if let Some(secret) = state.initial_passphrase.take() {
        start(&state, secret);
        busy = true;
    }
    event_loop.run(move |event,_,control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now()+Duration::from_millis(200));
        match event {
            Event::UserEvent(Message::Command(command)) => match command {
                Command::Unlock { passphrase, confirm, create, remember } => {
                    let secret=Zeroizing::new(passphrase); let confirm=Zeroizing::new(confirm);
                    if busy { return; }
                    if secret.is_empty() || secret.contains(['\n','\r']) { message="Enter a passphrase on a single line.".into();return; }
                    if create && secret.as_str()!=confirm.as_str() {message="The passphrases don’t match.".into();return;}
                    busy=true; message.clear(); remember_after_unlock=remember;
                    if remember { pending_secret=Some(Zeroizing::new(secret.to_string())); }
                    if create {
                        initializing=true;
                        let path=data_dir.clone();let sender=proxy.clone();
                        state.runtime_handle.spawn(async move {
                            let result=initialize(path,secret).await;
                            let _=sender.send_event(Message::Initialized(result));
                        });
                    } else { start(&state,secret); }
                },
                Command::Open {} => { if ready { let _=webview.load_url(&desktop_url);entered_workspace=true; } },
                Command::Restart {} => {if ready {state.daemon_handle.blocking_lock().request_restart(); ready=false; entered_workspace=false;busy=true;message.clear();}},
                Command::Forget {} => { match secret_store::forget_daemon_passphrase(){Ok(())=>{state.settings.remember_passphrase_in_keychain=false;state.settings.save(&state.settings_path);message="Saved unlock removed from this device.".into();},Err(e)=>message=e} },
                Command::Autostart{enabled}=>match crate::autostart::set_enabled(enabled){Ok(())=>{state.settings.autostart_on_login=enabled;state.settings.save(&state.settings_path);},Err(e)=>message=e.to_string()},
                Command::Quit {}=>{quitting=true;state.daemon_handle.blocking_lock().request_quit();},
                Command::CheckUpdate {} => { if ready { updates.check(&state.runtime_handle, &state.settings.status_url, state.settings.edition); } },
                Command::DownloadUpdate {} => { if ready { updates.download(&state.runtime_handle, &state.settings.status_url, state.settings.edition); } },
                Command::ApplyUpdate {} => {
                    // Setup waits a few seconds before it starts, so launching
                    // it first and then stopping the daemon leaves Solo running
                    // if the handoff itself fails.
                    if let Some(installer) = updates.ready_installer() {
                        tracing::info!(target: "solo::update", installer = %installer.display(), "handing off to the installer");
                        match crate::update::launch_installer(&installer) {
                            Ok(()) => { quitting=true; state.daemon_handle.blocking_lock().request_quit(); },
                            Err(error) => updates.fail(error),
                        }
                    }
                },
                Command::SetEdition { edition } => {
                    if let Some(edition) = crate::settings::Edition::parse(&edition) && state.settings.edition != Some(edition) {
                        state.settings.edition = Some(edition);
                        state.settings.save(&state.settings_path);
                        tracing::info!(target: "solo::update", edition = edition.as_str(), "update edition changed");
                        updates.reset();
                        if ready { updates.check(&state.runtime_handle, &state.settings.status_url, state.settings.edition); }
                    }
                },
                Command::SignIn {} => { if ready { updates.sign_in(&state.runtime_handle, &state.settings.status_url); } },
            },
            Event::UserEvent(Message::Initialized(result))=>{ initializing=false; if quitting {busy=false;pending_secret=None;} else {match result {Ok(secret)=>start(&state,secret),Err(error)=>{message=error;busy=false;pending_secret=None;}}} },
            Event::WindowEvent{event:WindowEvent::CloseRequested,..}=>{if tray_icon.is_some(){window.set_visible(false);}else{quitting=true;state.daemon_handle.blocking_lock().request_quit();}},
            _=>{},
        }
        if last_tick.elapsed()<Duration::from_millis(200){return;}
        last_tick=Instant::now();
        while let Ok(event)=tray_icon::menu::MenuEvent::receiver().try_recv() {
            let id=event.id.0.as_str();
            if id==tray::MENU_QUIT {quitting=true;state.daemon_handle.blocking_lock().request_quit();continue;}
            window.set_visible(true);window.set_minimized(false);window.set_focus();
            if id=="solo.device_settings" {let _=webview.load_url("solo://app/index.html#settings");continue;}
            let route=match id {tray::MENU_OPEN_DESKTOP|tray::MENU_OPEN_MEMORIES=>"memories",tray::MENU_OPEN_INBOX=>"inbox",tray::MENU_OPEN_CONNECTIONS=>"connections",tray::MENU_OPEN_IMPORT=>"import",tray::MENU_OPEN_HEALTH=>"health",tray::MENU_SHOW_LOGS=>"logs",_=>"settings"};
            if id==tray::MENU_RESTART_DAEMON {if let Ok(mut h)=state.daemon_handle.try_lock(){h.request_restart();}}
            if entered_workspace {let base=desktop_url.split('#').next().unwrap_or(&desktop_url);let _=webview.load_url(&format!("{base}#{route}"));}
        }
        if quitting {
            if !initializing && state.daemon_handle.try_lock().is_ok_and(|h|h.supervisor_exited) {*control_flow=ControlFlow::Exit;}
            return;
        }
        let mut running=false;
        let mut supervisor_state=None;
        if let Ok(handle)=state.daemon_handle.try_lock() {
            supervisor_state=Some(handle.state.clone());
            running=handle.state==daemon::SupervisorState::Running && handle.command==daemon::Command_::Run;
            if observed_pid!=handle.pid {observed_pid=handle.pid;ready_after=SystemTime::now();}
            if let daemon::SupervisorState::StartupFailed(ref error)=handle.state {
                message=error.clone();busy=false;pending_secret=None;
                if entered_workspace {let _=webview.load_url(START_URL);entered_workspace=false;}
            } else if entered_workspace && matches!(handle.state,daemon::SupervisorState::Crashed(_)|daemon::SupervisorState::Restarting) {
                busy=true;let _=webview.load_url(START_URL);entered_workspace=false;
            }
        }
        let status_snapshot=state.status_state.try_lock().ok().map(|s|(s.health,s.last_ok_at));
        ready=running && status_snapshot.is_some_and(|(health,last_ok_at)|health==crate::status::DaemonHealth::Healthy && last_ok_at.is_some_and(|at|at>=ready_after));
        let tray_health=match supervisor_state {
            Some(daemon::SupervisorState::Locked|daemon::SupervisorState::Starting|daemon::SupervisorState::Restarting)=>crate::status::DaemonHealth::Starting,
            Some(daemon::SupervisorState::StartupFailed(_)|daemon::SupervisorState::Stopped)=>crate::status::DaemonHealth::Down,
            Some(daemon::SupervisorState::Running|daemon::SupervisorState::Crashed(_))=>status_snapshot.map_or(last_tray_health,|(health,_)|health),
            None=>last_tray_health,
        };
        if tray_health!=last_tray_health {
            if let Some(icon)=tray_icon.as_ref() {
                let _=icon.set_icon(Some(tray::icon_for(tray_health,1.0)));
                let tooltip=match tray_health {
                    crate::status::DaemonHealth::Healthy=>"Solo daemon: healthy",
                    crate::status::DaemonHealth::Starting=>"Solo daemon: starting / reconnecting",
                    crate::status::DaemonHealth::Down=>"Solo daemon: stopped",
                };
                let _=icon.set_tooltip(Some(tooltip));
            }
            last_tray_health=tray_health;
        }
        if ready && busy {
            busy=false;
            if remember_after_unlock && let Some(secret)=pending_secret.take() {
                match secret_store::store_daemon_passphrase(&secret) {Ok(())=>{state.settings.remember_passphrase_in_keychain=true;state.settings.save(&state.settings_path);},Err(error)=>message=error}
            }
            if message.is_empty() {let _=webview.load_url(&desktop_url);entered_workspace=true;}
        }
        updates.tick(&state.runtime_handle, &state.settings.status_url, ready);
        let payload=serde_json::json!({"ready":ready,"busy":busy,"create":!data_dir.join("solo.config.toml").is_file(),"message":message,"remember":state.settings.remember_passphrase_in_keychain,"autostart":state.settings.autostart_on_login,"updates":updates.payload(state.settings.edition)});
        // Only the bundled page implements this callback. No secret is emitted.
        let _=webview.evaluate_script(&format!("window.soloState?.({payload});"));
    });
    #[allow(unreachable_code)]
    Ok(())
}

fn start(state: &AppState, secret: Zeroizing<String>) {
    // Claim the supervisor before returning to the UI event loop. Quit must not
    // race a queued task which would otherwise reset the command back to Run.
    if !state.daemon_handle.blocking_lock().prepare_start() {
        return;
    }
    let daemon = state.daemon_handle.clone();
    let logs = state.log_buffer.clone();
    state.runtime_handle.spawn(async move {
        let _ = daemon::supervise(daemon, logs, secret).await;
    });
}

async fn initialize(
    path: std::path::PathBuf,
    secret: Zeroizing<String>,
) -> Result<Zeroizing<String>, String> {
    let embedder = solo_storage::probe_embedder_config_from_env()
        .await
        .map_err(|e| e.to_string())?;
    let returned = Zeroizing::new(secret.to_string());
    tokio::task::spawn_blocking(move || {
        solo_storage::init(solo_storage::InitParams {
            data_dir: path,
            passphrase: secret,
            force: false,
            embedder,
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(returned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_bundled_start_page_has_unlock_privileges() {
        assert!(trusted_start_url("solo://app/index.html"));
        assert!(trusted_start_url("http://solo.app/index.html#settings"));
        for url in [
            "http://localhost:17821/desktop/",
            "https://solo.app/index.html",
            "http://solo.app.evil/index.html",
            "solo://other/index.html",
            "solo://app/other.html",
            "solo://user@app/index.html",
            "solo://app/index.html?script=x",
        ] {
            assert!(!trusted_start_url(url), "{url}");
        }
    }
    #[test]
    fn ipc_accepts_the_update_and_account_actions() {
        for body in [
            r#"{"action":"check_update"}"#,
            r#"{"action":"download_update"}"#,
            r#"{"action":"apply_update"}"#,
            r#"{"action":"set_edition","edition":"pro"}"#,
            r#"{"action":"sign_in"}"#,
        ] {
            assert!(serde_json::from_str::<Command>(body).is_ok(), "{body}");
        }
        // The installer path is never taken from the page: the daemon's own
        // verified status names it.
        assert!(
            serde_json::from_str::<Command>(r#"{"action":"apply_update","path":"C:\\x.exe"}"#)
                .is_err()
        );
        assert!(serde_json::from_str::<Command>(r#"{"action":"set_edition"}"#).is_err());
    }
    #[test]
    fn ipc_rejects_unknown_actions_and_fields() {
        assert!(serde_json::from_str::<Command>(r#"{"action":"get_secret"}"#).is_err());
        assert!(
            serde_json::from_str::<Command>(r#"{"action":"open","url":"https://example.com"}"#)
                .is_err()
        );
    }
}
