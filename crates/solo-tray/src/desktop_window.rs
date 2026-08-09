// SPDX-License-Identifier: Apache-2.0

//! Owned Solo app webview window.

use anyhow::{Context, Result, bail};
use std::io::Write;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Icon, WindowBuilder};
use wry::WebViewBuilder;

pub const DESKTOP_WINDOW_ARG: &str = "--desktop-window";
pub const DESKTOP_URL_ARG: &str = "--desktop-url";
pub const DESKTOP_ROUTE_FILE_ARG: &str = "--desktop-route-file";
pub const DESKTOP_SMOKE_REPORT_ARG: &str = "--desktop-smoke-report";

#[derive(Debug, Clone, PartialEq, Eq)]
enum DesktopWindowEvent {
    Navigate(String),
}

pub fn args_request_desktop_window(args: &[String]) -> bool {
    args.iter().any(|arg| arg == DESKTOP_WINDOW_ARG)
}

pub fn url_from_args(args: &[String], default_url: &str) -> Result<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == DESKTOP_URL_ARG {
            let Some(url) = iter.next() else {
                bail!("{DESKTOP_URL_ARG} requires a URL");
            };
            return Ok(url.clone());
        }
    }
    Ok(default_url.to_string())
}

pub fn route_file_from_args(
    args: &[String],
    default_route_file: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == DESKTOP_ROUTE_FILE_ARG {
            let Some(path) = iter.next() else {
                bail!("{DESKTOP_ROUTE_FILE_ARG} requires a path");
            };
            return Ok(std::path::PathBuf::from(path));
        }
    }
    Ok(default_route_file.to_path_buf())
}

pub fn smoke_report_file_from_args(args: &[String]) -> Result<Option<std::path::PathBuf>> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == DESKTOP_SMOKE_REPORT_ARG {
            let Some(path) = iter.next() else {
                bail!("{DESKTOP_SMOKE_REPORT_ARG} requires a path");
            };
            return Ok(Some(std::path::PathBuf::from(path)));
        }
    }
    Ok(None)
}

pub fn run(
    url: String,
    route_file: std::path::PathBuf,
    smoke_report_file: Option<std::path::PathBuf>,
) -> Result<()> {
    validate_smoke_report_url(&url, smoke_report_file.as_deref())?;

    let event_loop = EventLoopBuilder::<DesktopWindowEvent>::with_user_event().build();
    spawn_route_watcher(route_file.clone(), url.clone(), event_loop.create_proxy());

    let mut window = WindowBuilder::new()
        .with_title("Solo")
        .with_inner_size(LogicalSize::new(1220.0, 820.0))
        .with_min_inner_size(LogicalSize::new(900.0, 620.0));
    if let Some(icon) = solo_window_icon() {
        window = window.with_window_icon(Some(icon));
    }
    let window = window
        .build(&event_loop)
        .context("create Solo app window")?;

    let mut builder = WebViewBuilder::new().with_url(&url);
    if let Some(report_file) = smoke_report_file.clone() {
        builder = builder
            .with_initialization_script(desktop_smoke_readiness_script())
            .with_ipc_handler(move |request| {
                let payload = request.body();
                if payload.contains("\"kind\":\"solo-desktop-smoke\"") {
                    if let Err(error) = append_smoke_report(&report_file, payload) {
                        tracing::warn!(
                            path = %report_file.display(),
                            error = %error,
                            "failed to write Solo smoke report"
                        );
                    }
                }
            });
    }
    #[cfg(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    ))]
    let webview = builder.build(&window).context("create Solo webview")?;
    #[cfg(not(any(
        target_os = "windows",
        target_os = "macos",
        target_os = "ios",
        target_os = "android"
    )))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;

        let vbox = window.default_vbox().context("create Solo GTK container")?;
        builder.build_gtk(vbox).context("create Solo GTK webview")?
    };

    let allowed_origin = url_origin(&url).unwrap_or_default();

    let smoke_report_display = smoke_report_file
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    tracing::info!(
        url = %url,
        route_file = %route_file.display(),
        smoke_report_file = %smoke_report_display,
        backend = desktop_webview_backend(),
        "Solo window opened"
    );
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(DesktopWindowEvent::Navigate(url)) => {
                if !is_allowed_route_url(&url, &allowed_origin) {
                    tracing::warn!(url = %url, "ignored unsafe Solo route request");
                    return;
                }
                let Ok(encoded_url) = serde_json::to_string(&url) else {
                    tracing::warn!(url = %url, "failed to encode Solo route request");
                    return;
                };
                let script = format!("window.location.href = {encoded_url};");
                if let Err(error) = webview.evaluate_script(&script) {
                    tracing::warn!(url = %url, error = %error, "failed to route Solo window");
                } else {
                    tracing::info!(url = %url, "routed Solo window");
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
    #[allow(unreachable_code)]
    Ok(())
}

fn desktop_smoke_readiness_script() -> &'static str {
    r#"
(() => {
  const report = (reason) => {
    try {
      const bodyText = document.body && document.body.innerText
        ? document.body.innerText.slice(0, 6000)
        : "";
      const payload = {
        kind: "solo-desktop-smoke",
        reason,
        href: window.location.href,
        hash: window.location.hash,
        title: document.title,
        readyState: document.readyState,
        bodyText,
        timestamp: new Date().toISOString()
      };
      if (window.ipc && typeof window.ipc.postMessage === "function") {
        window.ipc.postMessage(JSON.stringify(payload));
      }
    } catch (_) {}
  };
  const delayedReport = (reason) => {
    window.setTimeout(() => report(reason), 250);
    window.setTimeout(() => report(reason + ":settled"), 900);
  };
  window.addEventListener("load", () => delayedReport("load"));
  window.addEventListener("hashchange", () => delayedReport("hashchange"));
  document.addEventListener("DOMContentLoaded", () => delayedReport("domcontentloaded"));
  delayedReport("init");
})();
"#
}

fn validate_smoke_report_url(url: &str, smoke_report_file: Option<&std::path::Path>) -> Result<()> {
    if smoke_report_file.is_some() && url_origin(url).is_none() {
        bail!("{DESKTOP_SMOKE_REPORT_ARG} requires a loopback http Solo URL");
    }
    Ok(())
}

fn append_smoke_report(report_file: &std::path::Path, payload: &str) -> std::io::Result<()> {
    if let Some(parent) = report_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(report_file)?;
    writeln!(file, "{payload}")?;
    Ok(())
}

fn spawn_route_watcher(
    route_file: std::path::PathBuf,
    initial_url: String,
    proxy: EventLoopProxy<DesktopWindowEvent>,
) {
    std::thread::Builder::new()
        .name("solo-desktop-route-watcher".to_string())
        .spawn(move || {
            let mut last_url = initial_url;
            loop {
                match std::fs::read_to_string(&route_file) {
                    Ok(raw) => {
                        let url = route_file_url(&raw);
                        if !url.is_empty() && url != last_url {
                            last_url = url.to_string();
                            if proxy
                                .send_event(DesktopWindowEvent::Navigate(last_url.clone()))
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::debug!(
                            route_file = %route_file.display(),
                            error = %error,
                            "could not read Solo route file"
                        );
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        })
        .expect("spawn Solo route watcher");
}

fn route_file_url(raw: &str) -> &str {
    raw.trim_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
}

fn is_allowed_route_url(url: &str, allowed_origin: &str) -> bool {
    url_origin(url).is_some_and(|origin| origin == allowed_origin)
}

#[cfg(test)]
fn is_loopback_http_url(url: &str) -> bool {
    url_origin(url).is_some()
}

fn url_origin(url: &str) -> Option<String> {
    let Some(("http", rest)) = url.split_once("://") else {
        return None;
    };
    let authority_end = rest
        .char_indices()
        .find(|(_, ch)| matches!(ch, '/' | '?' | '#'))
        .map(|(idx, _)| idx)
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return None;
    }
    let (host, port) = if authority.starts_with('[') {
        let end = authority.find(']')?;
        let after = &authority[end + 1..];
        if !after.is_empty() && !after.starts_with(':') {
            return None;
        }
        (&authority[..=end], after)
    } else {
        authority
            .rsplit_once(':')
            .map_or((authority, ""), |(host, _port)| {
                (host, &authority[host.len()..])
            })
    };
    let host = host.to_ascii_lowercase();
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    Some(format!("http://{host}{port}"))
}

fn desktop_webview_backend() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "webview2"
    }
    #[cfg(target_os = "macos")]
    {
        "wkwebview"
    }
    #[cfg(target_os = "linux")]
    {
        "webkitgtk-gtk-container"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        "wry-native-webview"
    }
}

fn solo_window_icon() -> Option<Icon> {
    const ICON_PNG: &[u8] = include_bytes!("../assets/s_tray_icon_64.png");
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), width, height).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_window_mode_is_explicit() {
        assert!(args_request_desktop_window(&[
            "solo-tray".to_string(),
            DESKTOP_WINDOW_ARG.to_string()
        ]));
        assert!(!args_request_desktop_window(&["solo-tray".to_string()]));
    }

    #[test]
    fn desktop_url_arg_defaults_and_requires_value() {
        let args = vec!["solo-tray".to_string(), DESKTOP_WINDOW_ARG.to_string()];
        assert_eq!(
            url_from_args(&args, "http://127.0.0.1:17821/desktop/").unwrap(),
            "http://127.0.0.1:17821/desktop/"
        );

        let args = vec![
            "solo-tray".to_string(),
            DESKTOP_WINDOW_ARG.to_string(),
            DESKTOP_URL_ARG.to_string(),
            "http://127.0.0.1:17821/desktop/#memories".to_string(),
        ];
        assert_eq!(
            url_from_args(&args, "http://127.0.0.1:17821/desktop/").unwrap(),
            "http://127.0.0.1:17821/desktop/#memories"
        );

        let broken = vec![
            "solo-tray".to_string(),
            DESKTOP_WINDOW_ARG.to_string(),
            DESKTOP_URL_ARG.to_string(),
        ];
        assert!(url_from_args(&broken, "http://127.0.0.1:17821/desktop/").is_err());
    }

    #[test]
    fn desktop_route_file_arg_defaults_and_requires_value() {
        let default = std::path::PathBuf::from(r"C:\Users\Example\.solo\desktop-route.txt");
        let args = vec!["solo-tray".to_string(), DESKTOP_WINDOW_ARG.to_string()];
        assert_eq!(route_file_from_args(&args, &default).unwrap(), default);

        let route_file = std::path::PathBuf::from(r"C:\tmp\solo-route.txt");
        let args = vec![
            "solo-tray".to_string(),
            DESKTOP_WINDOW_ARG.to_string(),
            DESKTOP_ROUTE_FILE_ARG.to_string(),
            route_file.display().to_string(),
        ];
        assert_eq!(route_file_from_args(&args, &default).unwrap(), route_file);

        let broken = vec![
            "solo-tray".to_string(),
            DESKTOP_WINDOW_ARG.to_string(),
            DESKTOP_ROUTE_FILE_ARG.to_string(),
        ];
        assert!(route_file_from_args(&broken, &default).is_err());
    }

    #[test]
    fn desktop_smoke_report_arg_is_opt_in_and_requires_value() {
        let args = vec!["solo-tray".to_string(), DESKTOP_WINDOW_ARG.to_string()];
        assert_eq!(smoke_report_file_from_args(&args).unwrap(), None);

        let report_file = std::path::PathBuf::from(r"C:\tmp\solo-desktop-smoke.jsonl");
        let args = vec![
            "solo-tray".to_string(),
            DESKTOP_WINDOW_ARG.to_string(),
            DESKTOP_SMOKE_REPORT_ARG.to_string(),
            report_file.display().to_string(),
        ];
        assert_eq!(
            smoke_report_file_from_args(&args).unwrap(),
            Some(report_file)
        );

        let broken = vec![
            "solo-tray".to_string(),
            DESKTOP_WINDOW_ARG.to_string(),
            DESKTOP_SMOKE_REPORT_ARG.to_string(),
        ];
        assert!(smoke_report_file_from_args(&broken).is_err());
    }

    #[test]
    fn desktop_smoke_script_posts_page_readiness_payload() {
        let script = desktop_smoke_readiness_script();
        assert!(script.contains("solo-desktop-smoke"));
        assert!(script.contains("window.ipc.postMessage"));
        assert!(script.contains("bodyText"));
        assert!(script.contains("hashchange"));
    }

    #[test]
    fn desktop_smoke_report_requires_loopback_http_url() {
        let report_file = std::path::Path::new("desktop-smoke.jsonl");

        assert!(
            validate_smoke_report_url(
                "http://127.0.0.1:17821/desktop/#connections",
                Some(report_file)
            )
            .is_ok()
        );
        assert!(
            validate_smoke_report_url("https://127.0.0.1:17821/desktop/", Some(report_file))
                .is_err()
        );
        assert!(
            validate_smoke_report_url("http://example.com:17821/desktop/", Some(report_file))
                .is_err()
        );
        assert!(validate_smoke_report_url("http://example.com:17821/desktop/", None).is_ok());
    }

    #[test]
    fn desktop_smoke_report_appends_jsonl_payloads() {
        let temp = tempfile::tempdir().unwrap();
        let report_file = temp.path().join("nested").join("desktop-smoke.jsonl");

        append_smoke_report(
            &report_file,
            r##"{"kind":"solo-desktop-smoke","hash":"#home"}"##,
        )
        .unwrap();
        append_smoke_report(
            &report_file,
            r##"{"kind":"solo-desktop-smoke","hash":"#connections"}"##,
        )
        .unwrap();

        let report = std::fs::read_to_string(report_file).unwrap();
        assert!(report.contains("\"hash\":\"#home\""));
        assert!(report.contains("\"hash\":\"#connections\""));
        assert_eq!(report.lines().count(), 2);
    }

    #[test]
    fn desktop_route_requests_allow_only_loopback_http_urls() {
        assert!(is_loopback_http_url("http://127.0.0.1:17821/desktop/#home"));
        assert!(is_loopback_http_url("http://localhost:5173/#memories"));
        assert!(is_loopback_http_url("http://LOCALHOST:5173/#memories"));
        assert!(is_loopback_http_url("http://[::1]:17821/desktop/"));
        assert!(!is_loopback_http_url("https://127.0.0.1:17821/desktop/"));
        assert!(!is_loopback_http_url("javascript:alert(1)"));
        assert!(!is_loopback_http_url("http://example.com:17821/desktop/"));
        assert!(!is_loopback_http_url("http://[::1].example.com/desktop/"));
        assert!(!is_loopback_http_url(
            "http://user@127.0.0.1:17821/desktop/"
        ));
    }

    #[test]
    fn desktop_route_file_url_ignores_bom_and_whitespace() {
        assert_eq!(
            route_file_url("\u{feff}http://127.0.0.1:17821/desktop/#health\r\n"),
            "http://127.0.0.1:17821/desktop/#health"
        );
    }

    #[test]
    fn desktop_route_requests_must_match_initial_origin() {
        let origin = url_origin("http://127.0.0.1:17821/desktop/#home").unwrap();

        assert!(is_allowed_route_url(
            "http://127.0.0.1:17821/desktop/#memories",
            &origin
        ));
        assert!(!is_allowed_route_url(
            "http://localhost:17821/desktop/#memories",
            &origin
        ));
        assert!(!is_allowed_route_url(
            "http://127.0.0.1:5173/#memories",
            &origin
        ));
    }

    #[test]
    fn desktop_backend_label_is_platform_specific() {
        assert!(!desktop_webview_backend().is_empty());
        #[cfg(target_os = "linux")]
        assert_eq!(desktop_webview_backend(), "webkitgtk-gtk-container");
    }
}
