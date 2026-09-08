// SPDX-License-Identifier: Apache-2.0

//! Reject browser/DNS-rebinding requests before CORS or route dispatch.
//! CORS response headers alone do not prevent same-origin rebinding reads.

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header, uri::Authority};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

pub(crate) async fn validate_request(
    State(authenticated): State<bool>,
    request: Request,
    next: Next,
) -> Response {
    if !is_trusted_request(&request, authenticated) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "untrusted_request",
                "message": "request host or origin is not allowed"
            })),
        )
            .into_response();
    }
    next.run(request).await
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn parse_authority(value: &str) -> Option<Authority> {
    let authority = value.parse::<Authority>().ok()?;
    if value.contains('@') || authority.host().is_empty() {
        return None;
    }
    // Reject malformed/non-numeric ports, including an empty trailing port.
    if value.len() != authority.host().len() && authority.port_u16().is_none() {
        return None;
    }
    Some(authority)
}

fn is_trusted_request(request: &Request, authenticated: bool) -> bool {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = match hosts.next() {
        Some(value) => match value.to_str().ok().and_then(parse_authority) {
            Some(host) => Some(host),
            None => return false,
        },
        None => None,
    };
    if hosts.next().is_some() {
        return false;
    }
    let uri_authority = request.uri().authority();
    if let Some(authority) = uri_authority {
        if parse_authority(authority.as_str()).is_none() {
            return false;
        }
        if let Some(host) = &host {
            if !host.as_str().eq_ignore_ascii_case(authority.as_str()) {
                return false;
            }
        }
    }
    let authority = host.as_ref().or(uri_authority);
    if !authenticated && authority.is_some_and(|value| !is_loopback_host(value.host())) {
        return false;
    }
    // Native HTTP/1.0 and in-process requests may have no authority. Network
    // HTTP/1.1 validation is also enforced by Hyper; browsers send Host (or
    // HTTP/2 :authority). Never use Forwarded/X-Forwarded-Host as authority.
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else {
        return true;
    };
    if origins.next().is_some() {
        return false;
    }
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if crate::http::is_localhost_origin(origin) {
        return true;
    }
    // Authenticated deployments can serve their own remote browser UI. This
    // does not relax the bearer/OIDC check on their data routes.
    if !authenticated {
        return false;
    }
    let (Ok(origin), Some(authority)) = (reqwest::Url::parse(origin), authority) else {
        return false;
    };
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
        || request
            .uri()
            .scheme_str()
            .is_some_and(|scheme| scheme != origin.scheme())
    {
        return false;
    }
    let default_port = if origin.scheme() == "https" { 443 } else { 80 };
    origin
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case(authority.host()))
        && origin.port_or_known_default() == Some(authority.port_u16().unwrap_or(default_port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::{Router, middleware, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    fn request(host: Option<&str>, origin: Option<&str>) -> Request {
        let mut builder = Request::builder().uri("/memory");
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn rejects_rebinding_authorities_with_or_without_origin() {
        for host in [
            "attacker.invalid:17821",
            "127.0.0.1.nip.io:17821",
            "localhost.evil",
            "localhost@evil",
            "localhost:bad",
            "localhost:",
        ] {
            for origin in [
                None,
                Some("http://attacker.invalid:17821"),
                Some("http://localhost:5173"),
            ] {
                assert!(
                    !is_trusted_request(&request(Some(host), origin), false),
                    "{host} {origin:?}"
                );
            }
        }
        let absolute = Request::builder()
            .uri("http://attacker.invalid/memory")
            .body(Body::empty())
            .unwrap();
        assert!(!is_trusted_request(&absolute, false));
        let conflicting = Request::builder()
            .uri("http://attacker.invalid/memory")
            .header(header::HOST, "localhost")
            .body(Body::empty())
            .unwrap();
        assert!(!is_trusted_request(&conflicting, true));
    }

    #[test]
    fn validates_origins_and_preserves_native_and_authenticated_clients() {
        for host in [
            "localhost:17821",
            "LOCALHOST:17821",
            "127.0.0.1:17821",
            "[::1]:17821",
        ] {
            assert!(is_trusted_request(&request(Some(host), None), false));
            assert!(is_trusted_request(
                &request(Some(host), Some("http://localhost:5173")),
                false
            ));
            for origin in [
                "null",
                "https://evil.invalid",
                "http://localhost.evil",
                "http://localhost/?x=1",
            ] {
                assert!(!is_trusted_request(
                    &request(Some(host), Some(origin)),
                    false
                ));
            }
        }
        assert!(is_trusted_request(&request(None, None), false));
        assert!(is_trusted_request(
            &request(Some("solo.example:443"), Some("https://solo.example")),
            true
        ));
        assert!(is_trusted_request(
            &request(Some("solo.example:17821"), None),
            true
        ));
        assert!(!is_trusted_request(
            &request(Some("solo.example:443"), Some("https://evil.example")),
            true
        ));
        assert!(!is_trusted_request(
            &request(Some("solo.example:443"), Some("https://solo.example:444")),
            true
        ));
        let mut duplicate = request(Some("localhost"), None);
        duplicate
            .headers_mut()
            .append(header::HOST, "attacker.invalid".parse().unwrap());
        assert!(!is_trusted_request(&duplicate, false));
        let mut duplicate = request(Some("localhost"), Some("http://localhost"));
        duplicate
            .headers_mut()
            .append(header::ORIGIN, "http://localhost".parse().unwrap());
        assert!(!is_trusted_request(&duplicate, false));
    }

    #[tokio::test]
    async fn rejects_before_cors_and_before_handler_side_effects() {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler_calls = calls.clone();
        let app = Router::new()
            .route(
                "/memory",
                post(move || async move {
                    handler_calls.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }),
            )
            .layer(tower_http::cors::CorsLayer::permissive())
            .layer(middleware::from_fn_with_state(false, validate_request));
        for method in ["POST", "OPTIONS"] {
            let mut req = request(Some("attacker.invalid"), Some("http://attacker.invalid"));
            *req.method_mut() = method.parse().unwrap();
            assert_eq!(
                app.clone().oneshot(req).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut req = request(Some("localhost"), None);
        *req.method_mut() = "POST".parse().unwrap();
        assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::OK);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
