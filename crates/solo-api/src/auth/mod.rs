// SPDX-License-Identifier: Apache-2.0

//! Pluggable auth for Solo's HTTP transport (v0.8.0 P3).
//!
//! Two modes (configured via `[auth]` block in `solo.config.toml`):
//!   * **Bearer** — single shared token for the Community Memory Library. Identical
//!     wire-behavior to v0.7.x bearer auth, re-implemented here so the
//!     middleware that emits `AuthenticatedPrincipal` covers both modes.
//!   * **OIDC** — standard OpenID Connect; any provider via discovery URL.
//!     JWKS keys are cached (TTL honors `Cache-Control: max-age=` from the
//!     discovery doc, falls back to 1 hour). A cache miss on an unknown
//!     `kid` triggers an immediate refetch (handles IdP key rotation
//!     without operator intervention).
//!
//! **MCP uses bearer-only at v0.8.0** — the MCP spec has no story for OIDC.
//! **CLI is implicitly trusted** (no auth — admin tier).
//!
//! Axum middleware (`auth_middleware`) validates the `Authorization`
//! header and inserts an [`AuthenticatedPrincipal`] into request extensions.
//! Authentication never selects a database in Community.
//!
//! See `docs/dev-log/0090-v0.8.0-implementation-plan.md` Section 2 P3
//! for the spec. ADR-0004 (added in P7) documents how auth ties into
//! per-tenant writer-actor isolation.

pub mod bearer;
pub mod middleware;
pub mod oidc;

use serde::{Deserialize, Serialize};

/// Configuration for one auth mode. Stored in [`solo_storage::SoloConfig`]
/// under the `[auth]` block.
///
/// Backward compatibility: when the `[auth]` block is absent from
/// `solo.config.toml`, the runtime falls through to the
/// `--bearer-token-file` CLI flag (v0.7.x behavior). Operators opt into
/// the v0.8.0 config-driven path by writing an `[auth]` block.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AuthConfig {
    /// Single shared bearer token for the Community Memory Library.
    Bearer { token: String },
    /// OIDC via any provider's discovery URL (`https://<host>/.well-known/openid-configuration`).
    /// `audience` matches the JWT `aud` claim.
    Oidc {
        discovery_url: String,
        audience: String,
    },
}

impl Default for AuthConfig {
    fn default() -> Self {
        // Default = empty-bearer (effectively a "no-auth" mode for dev).
        // Operators must opt in explicitly by setting an `[auth]` block;
        // the daemon's `--bearer-token-file` flag still works for the
        // v0.7.x path when the config block is absent.
        AuthConfig::Bearer {
            token: String::new(),
        }
    }
}

impl From<solo_storage::AuthSettings> for AuthConfig {
    /// Convert the on-disk config block (`SoloConfig.auth`) into the
    /// transport-side `AuthConfig`. Same wire shape — the duplication
    /// is intentional so `solo-storage` doesn't depend on `solo-api`.
    fn from(s: solo_storage::AuthSettings) -> Self {
        match s {
            solo_storage::AuthSettings::Bearer { token } => AuthConfig::Bearer { token },
            solo_storage::AuthSettings::Oidc {
                discovery_url,
                audience,
            } => AuthConfig::Oidc {
                discovery_url,
                audience,
            },
        }
    }
}

/// Result of a successful auth check, attached to the request as an
/// `axum::Extension`. P4 (audit log) reads `principal.subject` for the
/// audit-log "who" field. The principal cannot select a database.
#[derive(Debug, Clone)]
pub struct AuthenticatedPrincipal {
    /// JWT `sub` claim, or `"bearer"` for bearer-mode requests.
    pub subject: String,
    /// Scopes advertised by the JWT (`scope` claim, space-split).
    /// Empty for bearer-mode principals.
    pub scopes: Vec<String>,
    /// Raw JWT claims (`serde_json::Value`) for downstream inspection.
    /// `Null` for bearer-mode principals.
    pub claims: serde_json::Value,
}

impl AuthenticatedPrincipal {
    /// For bearer mode: synthesize a principal. No JWT, claims, or scopes.
    pub fn bearer() -> Self {
        Self {
            subject: "bearer".to_string(),
            scopes: Vec::new(),
            claims: serde_json::Value::Null,
        }
    }
}

/// Failure modes for both bearer and OIDC validation. The middleware
/// maps these to HTTP status codes (401 for client-supplied-credential
/// failures and 500 for upstream IdP issues).
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing Authorization header")]
    MissingAuthHeader,
    #[error("malformed Authorization header (expected `Bearer <token>`)")]
    MalformedAuthHeader,
    #[error("invalid bearer token")]
    InvalidBearer,
    #[error("invalid OIDC token: {reason}")]
    InvalidOidcToken { reason: String },
    #[error("OIDC discovery error: {0}")]
    Discovery(String),
    #[error("JWKS error: {0}")]
    Jwks(String),
}
