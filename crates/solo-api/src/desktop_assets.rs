// SPDX-License-Identifier: Apache-2.0

//! Embedded Solo Web assets served by the daemon at `/desktop/`.

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;

const INDEX_PATH: &str = "index.html";

#[derive(Debug, Clone, Copy)]
pub struct WebAsset {
    pub path: &'static str,
    pub mime: &'static str,
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/solo_web_assets.rs"));

pub async fn desktop_index_handler() -> Response {
    asset_response(index_asset())
}

pub async fn desktop_asset_handler(Path(path): Path<String>) -> Response {
    match asset_for_path(&path) {
        Some(asset) => asset_response(asset),
        None => text_response(StatusCode::NOT_FOUND, "not found"),
    }
}

fn asset_for_path(path: &str) -> Option<&'static WebAsset> {
    let path = normalize_path(path)?;
    if let Some(asset) = find_asset(path) {
        return Some(asset);
    }
    if path.starts_with("assets/") || has_extension(path) {
        return None;
    }
    Some(index_asset())
}

fn index_asset() -> &'static WebAsset {
    find_asset(INDEX_PATH).expect("build script always embeds index.html or fallback")
}

fn normalize_path(path: &str) -> Option<&str> {
    let path = path.trim_start_matches('/');
    let path = if path.is_empty() { INDEX_PATH } else { path };
    if path.contains('\\') || path.split('/').any(|part| part == "..") {
        return None;
    }
    Some(path)
}

fn find_asset(path: &str) -> Option<&'static WebAsset> {
    EMBEDDED_SOLO_WEB_ASSETS
        .iter()
        .find(|asset| asset.path == path)
}

fn has_extension(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|leaf| leaf.contains('.'))
}

fn asset_response(asset: &'static WebAsset) -> Response {
    let mut response = Response::new(Body::from(asset.bytes));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(asset.mime));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control_for_asset(asset.path)),
    );
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; connect-src 'self' http://127.0.0.1:* http://localhost:*; worker-src 'self' blob:; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    headers.insert(
        HeaderName::from_static("x-solo-web-assets"),
        HeaderValue::from_static(asset_source()),
    );
    response
}

fn text_response(status: StatusCode, body: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn cache_control_for_asset(path: &str) -> &'static str {
    if path == INDEX_PATH {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    }
}

fn asset_source() -> &'static str {
    if EMBEDDED_SOLO_WEB_REAL_DIST {
        "dist"
    } else {
        "fallback"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_assets_include_index() {
        assert!(
            find_asset(INDEX_PATH).is_some(),
            "build script must always embed index.html or fallback"
        );
    }

    #[test]
    fn embedded_assets_are_real_dist() {
        assert!(
            EMBEDDED_SOLO_WEB_REAL_DIST,
            "repo builds must embed the real solo-web dist, not the fallback page"
        );
    }

    #[test]
    fn asset_router_handles_root_spa_and_asset_paths() {
        assert_eq!(asset_for_path("").expect("root").path, INDEX_PATH);
        assert_eq!(asset_for_path("memories").expect("spa").path, INDEX_PATH);
        assert_eq!(asset_for_path("inbox").expect("spa").path, INDEX_PATH);
        assert!(asset_for_path("assets/missing.js").is_none());
        assert!(asset_for_path("../index.html").is_none());
        assert!(asset_for_path(r"assets\index.js").is_none());
    }

    #[test]
    fn cache_policy_keeps_index_fresh_and_assets_cached() {
        assert_eq!(cache_control_for_asset(INDEX_PATH), "no-cache");
        assert_eq!(
            cache_control_for_asset("assets/index-abc.js"),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn embedded_desktop_assets_set_browser_security_headers() {
        let response = asset_response(index_asset());
        let headers = response.headers();
        assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
        assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
        assert!(
            headers
                .get("content-security-policy")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("object-src 'none'")
        );
        assert_eq!(
            headers.get("permissions-policy").unwrap(),
            "camera=(), microphone=(), geolocation=()"
        );
    }
}
