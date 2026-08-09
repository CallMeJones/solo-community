// SPDX-License-Identifier: Apache-2.0

//! OIDC validation: discovery URL → JWKS → signature + claim checks.
//!
//! On the first request that arrives in OIDC mode, `OidcValidator`:
//!   1. Decodes the JWT header (no signature check) to extract `alg + kid`.
//!   2. Looks up the signing key in its in-memory JWKS cache. On cache
//!      miss (cold or unknown `kid` — key rotation), fetches the
//!      provider's discovery document, follows `jwks_uri`, and rebuilds
//!      the cache. The cache TTL respects `Cache-Control: max-age=N`
//!      from the discovery response, falling back to 1 hour if absent.
//!   3. Validates the signature + `aud` + `exp` via the `jsonwebtoken`
//!      crate.
//!   4. Packages the authenticated identity into an
//!      [`AuthenticatedPrincipal`]. OIDC claims cannot select a database.
//!
//! Failure modes (mapped to HTTP status in `middleware.rs`):
//!   * Missing/malformed Authorization header → 401 ([`AuthError::MissingAuthHeader`] /
//!     [`AuthError::MalformedAuthHeader`]).
//!   * Token rejected (bad signature, expired, wrong audience, unknown kid post-refetch)
//!     → 401 ([`AuthError::InvalidOidcToken`]).
//!   * Upstream IdP unreachable → 500 ([`AuthError::Discovery`] / [`AuthError::Jwks`]).

use super::{AuthError, AuthenticatedPrincipal};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, DecodingKey, TokenData, Validation, decode, decode_header};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Per-instance configuration for an OIDC validator. Mirrors the
/// `AuthConfig::Oidc { ... }` variant in `auth/mod.rs`.
#[derive(Debug, Clone)]
pub struct OidcConfig {
    pub discovery_url: String,
    pub audience: String,
}

/// Cached JWKS keys, indexed by `kid`. `fetched_at + ttl` together
/// govern eviction; a cache miss on an unknown `kid` triggers a
/// refetch regardless of TTL to handle IdP key rotation.
struct CachedJwks {
    keys: HashMap<String, KeyEntry>,
    fetched_at: Instant,
    ttl: Duration,
}

/// One JWKS entry. We retain the algorithm so we can pick the right
/// `Validation::new(Algorithm::*)` per token (each `kid` is bound to a
/// single algorithm by RFC 7517; trusting the token header's `alg`
/// without cross-checking against the key's `alg` is the
/// confused-deputy hole the validator below closes by always preferring
/// the JWK-side algorithm).
struct KeyEntry {
    key: DecodingKey,
    algorithm: Algorithm,
}

/// OIDC validator. Cheap-to-clone (one `Arc<RwLock<…>>` for the JWKS
/// cache; everything else is small).
#[derive(Clone)]
pub struct OidcValidator {
    config: OidcConfig,
    http_client: reqwest::Client,
    jwks_cache: Arc<RwLock<Option<CachedJwks>>>,
}

impl std::fmt::Debug for OidcValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcValidator")
            .field("config", &self.config)
            .field("jwks_cache", &"<RwLock>")
            .finish()
    }
}

impl OidcValidator {
    pub fn new(config: OidcConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self {
            config,
            http_client,
            jwks_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Construct with an explicit reqwest client. Used by tests so the
    /// timeout can be shorter / so the client is shared across the
    /// fake IdP fixture.
    #[cfg(test)]
    pub fn with_http_client(config: OidcConfig, http_client: reqwest::Client) -> Self {
        Self {
            config,
            http_client,
            jwks_cache: Arc::new(RwLock::new(None)),
        }
    }

    /// Validate the value of an Authorization header.
    pub async fn validate(
        &self,
        header: Option<&str>,
    ) -> Result<AuthenticatedPrincipal, AuthError> {
        let header = header.ok_or(AuthError::MissingAuthHeader)?;
        let token = header
            .strip_prefix("Bearer ")
            .ok_or(AuthError::MalformedAuthHeader)?;

        // Decode the header to discover `kid + alg`. No signature check yet.
        let jwt_header = decode_header(token).map_err(|e| AuthError::InvalidOidcToken {
            reason: format!("decode header: {e}"),
        })?;
        let kid = jwt_header
            .kid
            .clone()
            .ok_or_else(|| AuthError::InvalidOidcToken {
                reason: "missing kid in token header".to_string(),
            })?;

        // Look up the key (refresh cache on miss).
        let entry = self.get_key(&kid).await?;

        // Cross-check: the JWK's algorithm must match the token's `alg`.
        // Honoring the token-side `alg` alone would let an attacker
        // present an HS256 token signed with the public-key bytes of an
        // RS256 JWK. We pin to the JWK-side algorithm.
        if entry.algorithm != jwt_header.alg {
            return Err(AuthError::InvalidOidcToken {
                reason: format!(
                    "token alg {:?} does not match JWK alg {:?}",
                    jwt_header.alg, entry.algorithm
                ),
            });
        }

        let mut validation = Validation::new(entry.algorithm);
        validation.set_audience(&[&self.config.audience]);
        let token_data: TokenData<Value> =
            decode(token, &entry.key, &validation).map_err(|e| AuthError::InvalidOidcToken {
                reason: format!("{e}"),
            })?;

        let subject = token_data
            .claims
            .get("sub")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let scopes = token_data
            .claims
            .get("scope")
            .and_then(|v| v.as_str())
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default();

        Ok(AuthenticatedPrincipal {
            subject,
            scopes,
            claims: token_data.claims,
        })
    }

    /// Look up a JWKS key by `kid`. Returns a clone of the cached entry's
    /// decoding key + algorithm; refetches on cache miss (cold or
    /// unknown-kid).
    async fn get_key(&self, kid: &str) -> Result<KeyEntry, AuthError> {
        // Fast path: cache hit + within TTL + kid present.
        {
            let cache = self.jwks_cache.read().await;
            if let Some(c) = cache.as_ref()
                && c.fetched_at.elapsed() < c.ttl
                && let Some(entry) = c.keys.get(kid)
            {
                return Ok(KeyEntry {
                    key: entry.key.clone(),
                    algorithm: entry.algorithm,
                });
            }
            // Drop the read lock before taking write lock.
        }

        // Slow path: refetch + retry.
        self.refresh_cache().await?;
        let cache = self.jwks_cache.read().await;
        cache
            .as_ref()
            .and_then(|c| c.keys.get(kid))
            .map(|entry| KeyEntry {
                key: entry.key.clone(),
                algorithm: entry.algorithm,
            })
            .ok_or_else(|| AuthError::Jwks(format!("kid '{kid}' not found in JWKS")))
    }

    /// Fetch the OIDC discovery document, follow `jwks_uri`, rebuild the
    /// JWKS cache. TTL is sourced from `Cache-Control: max-age=N` on
    /// the discovery response (fallback 1 hour).
    async fn refresh_cache(&self) -> Result<(), AuthError> {
        let discovery_resp = self
            .http_client
            .get(&self.config.discovery_url)
            .send()
            .await
            .map_err(|e| AuthError::Discovery(format!("{e}")))?
            .error_for_status()
            .map_err(|e| AuthError::Discovery(format!("{e}")))?;

        // Determine TTL from Cache-Control max-age BEFORE consuming
        // the body (we lose access to headers after `.json()`).
        let ttl = parse_max_age(
            discovery_resp
                .headers()
                .get("cache-control")
                .and_then(|h| h.to_str().ok()),
        )
        .unwrap_or(Duration::from_secs(3600));

        let body: Value = discovery_resp
            .json()
            .await
            .map_err(|e| AuthError::Discovery(format!("{e}")))?;
        let jwks_uri = body
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AuthError::Discovery("discovery missing jwks_uri".to_string()))?;

        let jwks: JwkSet = self
            .http_client
            .get(jwks_uri)
            .send()
            .await
            .map_err(|e| AuthError::Jwks(format!("{e}")))?
            .error_for_status()
            .map_err(|e| AuthError::Jwks(format!("{e}")))?
            .json()
            .await
            .map_err(|e| AuthError::Jwks(format!("{e}")))?;

        let mut keys = HashMap::new();
        for jwk in jwks.keys.iter() {
            let Some(kid) = jwk.common.key_id.as_deref() else {
                continue;
            };
            let Some(algorithm) = jwk_algorithm(jwk) else {
                continue;
            };
            let key = match DecodingKey::from_jwk(jwk) {
                Ok(k) => k,
                Err(_) => continue,
            };
            keys.insert(kid.to_string(), KeyEntry { key, algorithm });
        }

        let mut cache = self.jwks_cache.write().await;
        *cache = Some(CachedJwks {
            keys,
            fetched_at: Instant::now(),
            ttl,
        });
        Ok(())
    }
}

/// Read the `alg` from the JWK's CommonParameters. The `key_algorithm`
/// field is the JWK side of the contract; we use that rather than the
/// token-header `alg` to avoid the confused-deputy hole described in
/// `OidcValidator::validate`.
fn jwk_algorithm(jwk: &Jwk) -> Option<Algorithm> {
    use jsonwebtoken::jwk::KeyAlgorithm;
    match jwk.common.key_algorithm? {
        KeyAlgorithm::HS256 => Some(Algorithm::HS256),
        KeyAlgorithm::HS384 => Some(Algorithm::HS384),
        KeyAlgorithm::HS512 => Some(Algorithm::HS512),
        KeyAlgorithm::RS256 => Some(Algorithm::RS256),
        KeyAlgorithm::RS384 => Some(Algorithm::RS384),
        KeyAlgorithm::RS512 => Some(Algorithm::RS512),
        KeyAlgorithm::PS256 => Some(Algorithm::PS256),
        KeyAlgorithm::PS384 => Some(Algorithm::PS384),
        KeyAlgorithm::PS512 => Some(Algorithm::PS512),
        KeyAlgorithm::ES256 => Some(Algorithm::ES256),
        KeyAlgorithm::ES384 => Some(Algorithm::ES384),
        KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
        // Other JWK algorithms (RSA-OAEP, etc.) are key-wrap, not JWT-sign.
        _ => None,
    }
}

/// Parse `max-age=N` (in seconds) out of a Cache-Control header. Returns
/// `None` if the header is absent / malformed / lacks `max-age`.
fn parse_max_age(header: Option<&str>) -> Option<Duration> {
    let h = header?;
    for part in h.split(',').map(str::trim) {
        if let Some(rest) = part.strip_prefix("max-age=")
            && let Ok(n) = rest.parse::<u64>()
        {
            return Some(Duration::from_secs(n));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ----------------------------------------------------------------------
    // Fake-IdP fixture (banked-lesson #26 candidate).
    //
    // We sign tokens with HMAC-SHA256 — both because it sidesteps RSA
    // key-generation in the test path (no `rsa` crate dep, no PEM
    // wrangling) and because the validator's contract is unchanged by
    // algorithm choice: the same `decode` + JWKS lookup path runs for
    // HMAC and RSA. The shared HMAC secret IS published in the JWK set
    // as an `oct` key; in production OIDC nobody would publish HMAC
    // secrets, but for in-process tests it's the cleanest path.
    // ----------------------------------------------------------------------

    /// Test IdP that stands up a wiremock server with an OIDC discovery
    /// document + a JWKS endpoint, holding an HMAC signing secret so
    /// tests can mint tokens.
    struct FakeIdp {
        server: MockServer,
        signing_secret: Vec<u8>,
        signing_kid: String,
    }

    impl FakeIdp {
        /// Stand up a fake IdP. `extra_response` lets a test override
        /// the Cache-Control on the discovery doc (returning `Some(secs)`
        /// adds `max-age=<secs>`; `None` omits the header).
        async fn start(signing_kid: &str, cache_max_age_secs: Option<u64>) -> Self {
            let server = MockServer::start().await;
            let secret = b"fixture-secret-bytes-for-hmac-tests".to_vec();
            let kid = signing_kid.to_string();

            let discovery_body = json!({
                "issuer": server.uri(),
                "jwks_uri": format!("{}/jwks", server.uri()),
            });
            let mut discovery_resp = ResponseTemplate::new(200).set_body_json(discovery_body);
            if let Some(secs) = cache_max_age_secs {
                discovery_resp = discovery_resp
                    .insert_header("cache-control", format!("max-age={secs}").as_str());
            }
            Mock::given(method("GET"))
                .and(path("/.well-known/openid-configuration"))
                .respond_with(discovery_resp)
                .mount(&server)
                .await;

            let jwks_body = json!({
                "keys": [
                    {
                        "kty": "oct",
                        "kid": &kid,
                        "alg": "HS256",
                        "k": base64_url(&secret),
                    }
                ]
            });
            Mock::given(method("GET"))
                .and(path("/jwks"))
                .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body))
                .mount(&server)
                .await;

            Self {
                server,
                signing_secret: secret,
                signing_kid: kid,
            }
        }

        /// Replace the JWKS endpoint with one that publishes a *new* kid.
        /// Used by `oidc_key_rotation` to simulate the IdP rotating keys
        /// faster than the cache TTL. The existing mock is left in place
        /// (wiremock falls through to the most recently mounted matcher).
        async fn rotate_to(&mut self, new_kid: &str, new_secret: &[u8]) {
            let jwks_body = json!({
                "keys": [
                    {
                        "kty": "oct",
                        "kid": new_kid,
                        "alg": "HS256",
                        "k": base64_url(new_secret),
                    }
                ]
            });
            self.server.reset().await;
            // Re-mount discovery (reset clears everything).
            let discovery_body = json!({
                "issuer": self.server.uri(),
                "jwks_uri": format!("{}/jwks", self.server.uri()),
            });
            Mock::given(method("GET"))
                .and(path("/.well-known/openid-configuration"))
                .respond_with(ResponseTemplate::new(200).set_body_json(discovery_body))
                .mount(&self.server)
                .await;
            Mock::given(method("GET"))
                .and(path("/jwks"))
                .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body))
                .mount(&self.server)
                .await;
            self.signing_secret = new_secret.to_vec();
            self.signing_kid = new_kid.to_string();
        }

        /// Mint a token signed by this fake IdP. `extra_claims` are
        /// merged into the claim set after the defaults so tests can
        /// override `exp`, `aud`, `solo_tenant`, etc.
        fn mint(&self, claims_override: Value) -> String {
            self.mint_with_kid(&self.signing_kid, &self.signing_secret, claims_override)
        }

        /// Mint a token signed by an explicit `kid`/secret pair —
        /// lets tests sign with an old key after the JWKS has rotated.
        fn mint_with_kid(&self, kid: &str, secret: &[u8], claims_override: Value) -> String {
            let mut header = Header::new(Algorithm::HS256);
            header.kid = Some(kid.to_string());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let mut claims = json!({
                "iss": self.server.uri(),
                "sub": "test-subject",
                "aud": "test-audience",
                "exp": now + 600,
                "iat": now,
                "solo_tenant": "default",
            });
            if let (Value::Object(c), Value::Object(o)) = (&mut claims, claims_override) {
                for (k, v) in o {
                    c.insert(k, v);
                }
            }
            jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(secret))
                .expect("encode")
        }
    }

    /// Base64-URL no-pad (matches JWK spec for the `k` parameter).
    fn base64_url(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn make_validator(server_uri: &str) -> OidcValidator {
        OidcValidator::with_http_client(
            OidcConfig {
                discovery_url: format!("{server_uri}/.well-known/openid-configuration"),
                audience: "test-audience".to_string(),
            },
            reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
        )
    }

    #[tokio::test]
    async fn oidc_happy_path() {
        let idp = FakeIdp::start("test-kid-1", None).await;
        let validator = make_validator(&idp.server.uri());

        let token = idp.mint(json!({ "solo_tenant": "tenant-a" }));
        let principal = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("validate");
        assert_eq!(principal.subject, "test-subject");
        assert_eq!(principal.claims["solo_tenant"], "tenant-a");
    }

    #[tokio::test]
    async fn oidc_key_rotation() {
        let mut idp = FakeIdp::start("old-kid", None).await;
        let validator = make_validator(&idp.server.uri());

        // Warm the cache with the old kid.
        let warmup_token = idp.mint(json!({}));
        let _ = validator
            .validate(Some(&format!("Bearer {warmup_token}")))
            .await
            .expect("warmup");

        // IdP rotates. Mint a token under the NEW kid.
        let new_secret = b"new-rotated-secret-32-bytes--here".to_vec();
        idp.rotate_to("new-kid", &new_secret).await;
        let token = idp.mint(json!({}));

        // Validator's cache has old-kid only — should refetch on
        // unknown-kid miss + accept the new token.
        let principal = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("post-rotation");
        assert_eq!(principal.subject, "test-subject");
    }

    #[tokio::test]
    async fn oidc_invalid_audience() {
        let idp = FakeIdp::start("kid-aud", None).await;
        let validator = make_validator(&idp.server.uri());

        let token = idp.mint(json!({ "aud": "wrong-audience" }));
        let err = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidOidcToken { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn oidc_expired_token() {
        let idp = FakeIdp::start("kid-exp", None).await;
        let validator = make_validator(&idp.server.uri());

        // exp = 5 min in the past, well outside the default 60s leeway.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = idp.mint(json!({ "exp": now - 300, "iat": now - 600 }));
        let err = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .unwrap_err();
        assert!(
            matches!(err, AuthError::InvalidOidcToken { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn oidc_does_not_require_a_database_routing_claim() {
        let idp = FakeIdp::start("kid-no-tenant", None).await;
        let validator = make_validator(&idp.server.uri());

        let token = idp.mint(json!({ "solo_tenant": null }));
        let principal = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("valid identity without routing claim");
        assert_eq!(principal.subject, "test-subject");
    }

    #[tokio::test]
    async fn oidc_database_like_claim_cannot_affect_authentication() {
        let idp = FakeIdp::start("kid-bad-tenant", None).await;
        let validator = make_validator(&idp.server.uri());

        let token = idp.mint(json!({ "solo_tenant": "INVALID UPPERCASE" }));
        let principal = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("routing-shaped claims are inert");
        assert_eq!(principal.subject, "test-subject");
    }

    #[tokio::test]
    async fn oidc_jwks_cache_within_ttl_no_refetch() {
        // No max-age = 1hr fallback TTL → second validate should NOT
        // re-hit discovery / jwks.
        let server = MockServer::start().await;
        let secret = b"counted-secret-32-bytes--padding".to_vec();
        let kid = "counted-kid";

        let discovery_body = json!({
            "issuer": server.uri(),
            "jwks_uri": format!("{}/jwks", server.uri()),
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(discovery_body))
            .expect(1) // hit exactly once across both validate() calls
            .mount(&server)
            .await;
        let jwks_body = json!({
            "keys": [
                {
                    "kty": "oct",
                    "kid": kid,
                    "alg": "HS256",
                    "k": base64_url(&secret),
                }
            ]
        });
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body))
            .expect(1) // also exactly once
            .mount(&server)
            .await;

        let validator = make_validator(&server.uri());

        // Mint two tokens with the same kid (would normally each cost a
        // network round-trip if uncached).
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(kid.to_string());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({
            "iss": server.uri(),
            "sub": "subj",
            "aud": "test-audience",
            "exp": now + 600,
            "iat": now,
            "solo_tenant": "default",
        });
        let token =
            jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(&secret)).unwrap();

        let _ = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("first");
        let _ = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("second");
        // Drop the validator; wiremock asserts the .expect(1) counts on drop.
    }

    #[tokio::test]
    async fn oidc_jwks_cache_respects_cache_control_max_age() {
        // Cache-Control: max-age=0 → second validate MUST refetch.
        let server = MockServer::start().await;
        let secret = b"max-age-secret-bytes-for-tests--".to_vec();
        let kid = "max-age-kid";

        let discovery_body = json!({
            "issuer": server.uri(),
            "jwks_uri": format!("{}/jwks", server.uri()),
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=0")
                    .set_body_json(discovery_body),
            )
            .expect(2) // two validate() calls each trigger a refetch
            .mount(&server)
            .await;
        let jwks_body = json!({
            "keys": [
                {
                    "kty": "oct",
                    "kid": kid,
                    "alg": "HS256",
                    "k": base64_url(&secret),
                }
            ]
        });
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(jwks_body))
            .expect(2)
            .mount(&server)
            .await;

        let validator = make_validator(&server.uri());

        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(kid.to_string());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims = json!({
            "iss": server.uri(),
            "sub": "subj",
            "aud": "test-audience",
            "exp": now + 600,
            "iat": now,
            "solo_tenant": "default",
        });
        let token =
            jsonwebtoken::encode(&header, &claims, &EncodingKey::from_secret(&secret)).unwrap();

        let _ = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("first");
        let _ = validator
            .validate(Some(&format!("Bearer {token}")))
            .await
            .expect("second");
    }

    #[test]
    fn parse_max_age_handles_typical_headers() {
        // bare max-age
        assert_eq!(
            parse_max_age(Some("max-age=300")),
            Some(Duration::from_secs(300))
        );
        // combined directives, whitespace-tolerant
        assert_eq!(
            parse_max_age(Some("public, max-age=86400, must-revalidate")),
            Some(Duration::from_secs(86400))
        );
        // missing → None → caller uses fallback
        assert_eq!(parse_max_age(Some("no-cache, no-store")), None);
        assert_eq!(parse_max_age(None), None);
    }
}
