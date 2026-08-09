// SPDX-License-Identifier: Apache-2.0

//! Axum middleware: dispatch to the configured `AuthValidator`, insert
//! the resulting `AuthenticatedPrincipal` into request extensions, or
//! short-circuit with the appropriate HTTP status.
//!
//! Status-code mapping:
//!   * `MissingAuthHeader`, `MalformedAuthHeader`, `InvalidBearer`,
//!     `InvalidOidcToken` → 401 (operator/client supplied wrong credentials)
//!   * `Discovery`, `Jwks` → 500 (upstream IdP is unreachable / misbehaving)

use super::{
    AuthConfig, AuthError, bearer::BearerValidator, oidc::OidcConfig, oidc::OidcValidator,
};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

/// Resolves either to a [`BearerValidator`] or an [`OidcValidator`]
/// depending on the `[auth]` block in the config. Built once at server
/// start; cloned cheaply on every request.
#[derive(Debug, Clone)]
pub enum AuthValidator {
    Bearer(BearerValidator),
    Oidc(OidcValidator),
}

impl AuthValidator {
    /// Build from a config block. Auth validates identity only and cannot
    /// select a Community database.
    ///
    /// **Operator foot-gun guard**: bearer mode with an empty token
    /// would silently accept `Authorization: Bearer ` (no actual
    /// secret). The daemon refuses this at boot by panicking — better
    /// to fail loudly than to ship a misconfigured `[auth]` block to
    /// production. The CLI `--bearer-token-file` path already refuses
    /// empty files in `http_serve.rs::read_bearer_token_file`; this
    /// closes the equivalent hole on the config-driven path.
    pub fn from_config(config: &AuthConfig) -> Self {
        match config {
            AuthConfig::Bearer { token } => {
                if token.is_empty() {
                    panic!(
                        "auth: bearer mode requires a non-empty token in [auth].token. \
                         Set a real token or remove the [auth] block to use \
                         --bearer-token-file instead."
                    );
                }
                Self::Bearer(BearerValidator::new(token.clone()))
            }
            AuthConfig::Oidc {
                discovery_url,
                audience,
            } => Self::Oidc(OidcValidator::new(OidcConfig {
                discovery_url: discovery_url.clone(),
                audience: audience.clone(),
            })),
        }
    }
}

/// Axum middleware. Reads the `Authorization` header, dispatches to the
/// configured validator, attaches the principal to the request, or
/// returns the appropriate error response.
pub async fn auth_middleware(
    State(validator): State<Arc<AuthValidator>>,
    mut req: Request,
    next: Next,
) -> Response {
    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string());

    let principal_result = match validator.as_ref() {
        AuthValidator::Bearer(v) => v.validate(auth_header.as_deref()),
        AuthValidator::Oidc(v) => v.validate(auth_header.as_deref()).await,
    };

    let principal = match principal_result {
        Ok(p) => p,
        Err(e) => return error_response(&e),
    };

    req.extensions_mut().insert(principal);
    next.run(req).await
}

/// Map an `AuthError` to an HTTP response. 401 responses carry a
/// `WWW-Authenticate: Bearer` hint so well-behaved clients learn the
/// challenge scheme.
fn error_response(err: &AuthError) -> Response {
    let status = match err {
        AuthError::MissingAuthHeader
        | AuthError::MalformedAuthHeader
        | AuthError::InvalidBearer
        | AuthError::InvalidOidcToken { .. } => StatusCode::UNAUTHORIZED,
        AuthError::Discovery(_) | AuthError::Jwks(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = axum::Json(serde_json::json!({
        "error": err.to_string(),
        "status": status.as_u16(),
    }));
    let mut resp = (status, body).into_response();
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Bearer realm="solo""#),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthConfig, AuthenticatedPrincipal};
    use axum::Extension;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn echo_principal(Extension(p): Extension<AuthenticatedPrincipal>) -> String {
        format!("subject={}", p.subject)
    }

    fn router_with_validator(validator: Arc<AuthValidator>) -> Router {
        Router::new().route("/echo", get(echo_principal)).layer(
            axum::middleware::from_fn_with_state(validator, auth_middleware),
        )
    }

    #[tokio::test]
    async fn bearer_inserts_principal_into_extension() {
        let cfg = AuthConfig::Bearer {
            token: "abc".to_string(),
        };
        let v = Arc::new(AuthValidator::from_config(&cfg));
        let router = router_with_validator(v);

        let req = Request::builder()
            .uri("/echo")
            .header("authorization", "Bearer abc")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let s = String::from_utf8_lossy(&body);
        assert_eq!(s, "subject=bearer");
    }

    #[tokio::test]
    async fn bearer_missing_returns_401_with_www_authenticate() {
        let cfg = AuthConfig::Bearer {
            token: "abc".to_string(),
        };
        let v = Arc::new(AuthValidator::from_config(&cfg));
        let router = router_with_validator(v);

        let req = Request::builder().uri("/echo").body(Body::empty()).unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(www.starts_with("Bearer"), "got {www}");
    }

    #[tokio::test]
    async fn bearer_wrong_token_returns_401() {
        let cfg = AuthConfig::Bearer {
            token: "abc".to_string(),
        };
        let v = Arc::new(AuthValidator::from_config(&cfg));
        let router = router_with_validator(v);

        let req = Request::builder()
            .uri("/echo")
            .header("authorization", "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
