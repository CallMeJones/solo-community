// SPDX-License-Identifier: Apache-2.0

//! Linking this installation to a Solo account.
//!
//! Community needs no account: the whole product works without one, and
//! nothing here gates a feature. What an account gives Community is an answer
//! to one question — does this person hold a paid plan — so Solo Controls can
//! offer to update to the edition they actually have instead of advertising
//! one they do not.
//!
//! What this module deliberately does not do:
//!
//! * **It never verifies a licence.** Verification belongs to a host that
//!   grants paid features, and Community grants none. It asks the account
//!   service whether a paid plan exists and believes the answer, because the
//!   worst a wrong answer can do is show or hide a button.
//! * **The browser never holds the tokens.** The authorization code comes back
//!   to a loopback port this process opened and is exchanged with a PKCE
//!   verifier the browser never saw.
//! * **Nothing runs on a timer.** Every network call here happens because
//!   somebody pressed Sign in, or asked for the status while signed in with an
//!   expired session.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest as _, Sha256};

use crate::SoloHttpState;

/// Where accounts live unless `SOLO_ACCOUNT_URL` says otherwise.
pub const DEFAULT_ACCOUNT_BASE_URL: &str = "https://hextek.io";

const API_BASE: &str = "/solo/api/v1";
const CLIENT_ID: &str = "solo-desktop";
/// The one path the account service redirects a code to; it checks this
/// exactly, so the listener below serves precisely it.
const CALLBACK_PATH: &str = "/solo/callback";
const CONNECT_PAGE: &str = "/SoloCommunity/connect";
const ACCOUNT_FILE: &str = "account.json";
const SCHEMA_VERSION: u32 = 1;

/// Long enough to read the page before approving, short enough that a
/// forgotten browser tab does not hold a loopback port open all day.
const LINK_TIMEOUT: Duration = Duration::from_secs(900);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------ storage --

/// Credentials for one linked machine. Not a licence: Community holds none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub schema_version: u32,
    /// Kept so a machine linked against a staging service does not silently
    /// start talking to production after a config change.
    pub base_url: String,
    pub device_id: String,
    pub access_token: String,
    pub access_expires_at: i64,
    pub refresh_token: String,
    pub email: Option<String>,
    pub plan: Option<String>,
    /// What the account service last said about a paid plan. Shown, never
    /// enforced.
    #[serde(default)]
    pub paid: bool,
}

impl StoredAccount {
    #[must_use]
    pub fn access_token_expired(&self, now: i64) -> bool {
        // A minute of slack: a token about to expire mid-request is expired.
        self.access_expires_at <= now + 60
    }
}

#[must_use]
pub fn account_file(data_dir: &Path) -> PathBuf {
    data_dir.join("account").join(ACCOUNT_FILE)
}

pub fn load(data_dir: &Path) -> Option<StoredAccount> {
    let path = account_file(data_dir);
    let body = std::fs::read_to_string(path).ok()?;
    let account: StoredAccount = serde_json::from_str(&body).ok()?;
    (account.schema_version == SCHEMA_VERSION).then_some(account)
}

pub fn save(data_dir: &Path, account: &StoredAccount) -> Result<(), String> {
    let path = account_file(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("create {parent:?}: {error}"))?;
    }
    let body = serde_json::to_string_pretty(account).map_err(|error| error.to_string())?;
    std::fs::write(&path, body).map_err(|error| format!("write {path:?}: {error}"))?;
    // These are bearer tokens. On Unix say so explicitly; on Windows the data
    // directory is already inside the user's profile, which is the protection
    // every other Solo secret relies on.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("restrict {path:?}: {error}"))?;
    }
    Ok(())
}

pub fn clear(data_dir: &Path) {
    let _ = std::fs::remove_file(account_file(data_dir));
}

// --------------------------------------------------------------------- pkce --

/// A PKCE verifier and the challenge derived from it. The verifier stays in
/// this process; only the challenge reaches the browser, so an intercepted
/// code cannot be exchanged.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0_u8; 32];
        bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// Everything needed to send somebody to the browser and recognise them
/// coming back.
#[derive(Debug, Clone)]
pub struct LinkRequest {
    pub base_url: String,
    pub redirect_uri: String,
    pub state: String,
    pub pkce: Pkce,
    pub device_name: String,
}

impl LinkRequest {
    #[must_use]
    pub fn authorize_url(&self) -> String {
        let mut url = format!(
            "{}{CONNECT_PAGE}?client_id={CLIENT_ID}&code_challenge_method=S256",
            self.base_url.trim_end_matches('/')
        );
        for (key, value) in [
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_challenge", self.pkce.challenge.as_str()),
            ("state", self.state.as_str()),
            ("device_name", self.device_name.as_str()),
        ] {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&encode_component(value));
        }
        url
    }
}

/// Percent-encode conservatively: anything outside the unreserved set is
/// escaped, so a device name with a space or an ampersand cannot alter the
/// query it sits in.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char);
            }
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

#[must_use]
pub fn device_name() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(100).collect();
            }
        }
    }
    "This machine".to_string()
}

#[must_use]
pub fn account_base_url() -> String {
    std::env::var("SOLO_ACCOUNT_URL")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_ACCOUNT_BASE_URL.to_string())
}

// ---------------------------------------------------------------- the flow --

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    device_id: String,
    plan: Option<PlanResponse>,
}

#[derive(Debug, Deserialize)]
struct PlanResponse {
    name: String,
}

#[derive(Debug, Deserialize)]
struct LicenseResponse {
    licensed: bool,
    plan: Option<PlanResponse>,
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("solo/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("Could not build an HTTP client: {error}"))
}

async fn read_error(response: reqwest::Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("{status}"))
}

/// Exchange an authorization code for this machine's tokens.
pub async fn exchange_code(request: &LinkRequest, code: &str) -> Result<StoredAccount, String> {
    let url = format!(
        "{}{API_BASE}/desktop/token",
        request.base_url.trim_end_matches('/')
    );
    let response = client()?
        .post(&url)
        .json(&json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "code_verifier": request.pkce.verifier,
            "redirect_uri": request.redirect_uri,
        }))
        .send()
        .await
        .map_err(|error| format!("Could not reach the account service: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Signing in failed: {}", read_error(response).await));
    }
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|error| format!("Could not read the sign-in answer: {error}"))?;

    Ok(StoredAccount {
        schema_version: SCHEMA_VERSION,
        base_url: request.base_url.trim_end_matches('/').to_string(),
        device_id: tokens.device_id,
        access_token: tokens.access_token,
        access_expires_at: chrono::Utc::now().timestamp() + tokens.expires_in,
        refresh_token: tokens.refresh_token,
        email: None,
        plan: tokens.plan.map(|plan| plan.name),
        paid: false,
    })
}

/// Trade the refresh token for a new pair. The service rotates refresh tokens
/// and revokes the session if a spent one comes back, so the new one is stored
/// even when the caller then fails.
pub async fn refresh(account: &StoredAccount) -> Result<StoredAccount, String> {
    let url = format!("{}{API_BASE}/desktop/token/refresh", account.base_url);
    let response = client()?
        .post(&url)
        .json(&json!({
            "grant_type": "refresh_token",
            "refresh_token": account.refresh_token,
        }))
        .send()
        .await
        .map_err(|error| format!("Could not reach the account service: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "This machine is no longer signed in: {}",
            read_error(response).await
        ));
    }
    let tokens: TokenResponse = response
        .json()
        .await
        .map_err(|error| format!("Could not read the refreshed session: {error}"))?;

    Ok(StoredAccount {
        schema_version: SCHEMA_VERSION,
        base_url: account.base_url.clone(),
        device_id: tokens.device_id,
        access_token: tokens.access_token,
        access_expires_at: chrono::Utc::now().timestamp() + tokens.expires_in,
        refresh_token: tokens.refresh_token,
        email: account.email.clone(),
        plan: tokens
            .plan
            .map(|plan| plan.name)
            .or_else(|| account.plan.clone()),
        paid: account.paid,
    })
}

/// Does this account hold a paid plan?
///
/// The same endpoint a paid host fetches its licence from; Community reads
/// only whether one exists. A refusal is not an error worth showing: it means
/// no paid plan, which is the ordinary state.
pub async fn read_plan(account: &StoredAccount) -> (bool, Option<String>) {
    let url = format!("{}{API_BASE}/desktop/license", account.base_url);
    let Ok(client) = client() else {
        return (account.paid, account.plan.clone());
    };
    let Ok(response) = client
        .get(&url)
        .bearer_auth(&account.access_token)
        .send()
        .await
    else {
        return (account.paid, account.plan.clone());
    };
    if !response.status().is_success() {
        return (false, account.plan.clone());
    }
    match response.json::<LicenseResponse>().await {
        Ok(body) => (
            body.licensed,
            body.plan
                .map(|plan| plan.name)
                .or_else(|| account.plan.clone()),
        ),
        Err(_) => (account.paid, account.plan.clone()),
    }
}

/// Wait for the browser to bring the authorization code back.
///
/// Binds an ephemeral loopback port and answers exactly one request, so the
/// code never leaves the machine.
pub async fn await_callback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let accept = async {
        loop {
            let (mut stream, _) = listener
                .accept()
                .await
                .map_err(|error| format!("Could not accept the sign-in: {error}"))?;
            let mut buffer = vec![0_u8; 8192];
            let read = stream
                .read(&mut buffer)
                .await
                .map_err(|error| format!("Could not read the sign-in: {error}"))?;
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let Some(target) = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
            else {
                continue;
            };
            // Browsers ask for a favicon straight after; ignore anything that
            // is not the callback rather than calling it a failure.
            if !target.starts_with(CALLBACK_PATH) {
                let _ = stream.write_all(NOT_FOUND.as_bytes()).await;
                let _ = stream.shutdown().await;
                continue;
            }

            let params = query_pairs(target);
            let outcome = match (
                params.iter().find(|(key, _)| key == "code"),
                params.iter().find(|(key, _)| key == "state"),
                params.iter().find(|(key, _)| key == "error"),
            ) {
                (_, _, Some((_, message))) => Err(format!("The browser reported: {message}")),
                (Some((_, code)), Some((_, state)), _) if state == expected_state => {
                    Ok(code.clone())
                }
                (Some(_), Some(_), _) => {
                    Err("That sign-in did not match this request; start it again.".to_string())
                }
                _ => Err("The browser came back without an authorization code.".to_string()),
            };

            let page = if outcome.is_ok() {
                LINKED_PAGE
            } else {
                FAILED_PAGE
            };
            let _ = stream.write_all(page.as_bytes()).await;
            let _ = stream.shutdown().await;
            return outcome;
        }
    };

    tokio::time::timeout(LINK_TIMEOUT, accept)
        .await
        .map_err(|_| "Timed out waiting for the browser; start the sign-in again.".to_string())?
}

fn query_pairs(target: &str) -> Vec<(String, String)> {
    let Some((_, query)) = target.split_once('?') else {
        return Vec::new();
    };
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode_component(key), decode_component(value)))
        .collect()
}

fn decode_component(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                // A malformed escape is kept verbatim rather than dropped: it
                // should fail the state check loudly, not quietly become
                // something else.
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

const LINKED_PAGE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><meta charset=utf-8><title>Solo</title><body style=\"font:16px system-ui;background:#0a0908;color:#f2ede4;display:grid;place-items:center;height:100vh;margin:0\"><div style=\"text-align:center\"><h1 style=\"font-size:20px\">This machine is signed in</h1><p style=\"color:#a49b8b\">You can close this tab and go back to Solo.</p></div>";

const FAILED_PAGE: &str = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n\r\n<!doctype html><meta charset=utf-8><title>Solo</title><body style=\"font:16px system-ui;background:#0a0908;color:#f2ede4;display:grid;place-items:center;height:100vh;margin:0\"><div style=\"text-align:center\"><h1 style=\"font-size:20px\">Signing in did not finish</h1><p style=\"color:#a49b8b\">Go back to Solo and try again.</p></div>";

const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

/// Ask the desktop to open a URL. Best effort: the address is also reported in
/// the status, so a machine with no browser can still be signed in by hand.
pub fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let attempt = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let attempt = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let attempt = std::process::Command::new("xdg-open").arg(url).spawn();

    if let Err(error) = attempt {
        tracing::debug!(target: "solo::account", %error, "could not open a browser");
    }
}

// ------------------------------------------------------------------ routes --

#[derive(Debug, Clone, Serialize)]
struct SignIn {
    /// `idle`, `waiting`, `done` or `failed`.
    state: &'static str,
    error: Option<String>,
    authorize_url: Option<String>,
}

impl Default for SignIn {
    fn default() -> Self {
        Self {
            state: "idle",
            error: None,
            authorize_url: None,
        }
    }
}

fn sign_in_cell() -> &'static Mutex<SignIn> {
    static CELL: OnceLock<Mutex<SignIn>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(SignIn::default()))
}

fn sign_in_state() -> SignIn {
    sign_in_cell()
        .lock()
        .map(|state| state.clone())
        .unwrap_or_default()
}

fn set_sign_in(next: SignIn) {
    if let Ok(mut slot) = sign_in_cell().lock() {
        *slot = next;
    }
}

/// `GET /host/v1/account` and `POST /host/v1/account/sign-in`.
///
/// The path is deliberately neutral rather than edition-specific: Solo
/// Controls asks every host the same question, and a paid host answers it with
/// more (the edition its licence grants).
pub fn routes() -> Router<SoloHttpState> {
    Router::new()
        .route("/host/v1/account", get(status_handler))
        .route("/host/v1/account/sign-in", post(sign_in_handler))
}

async fn status_handler(State(state): State<SoloHttpState>) -> Response {
    let data_dir = state.registry.data_dir().to_path_buf();
    let mut account = load(&data_dir);

    // Refresh a stale session so the plan shown is the plan held, not the plan
    // held when somebody last pressed Sign in.
    if let Some(stored) = &account
        && stored.access_token_expired(chrono::Utc::now().timestamp())
        && let Ok(refreshed) = refresh(stored).await
    {
        let _ = save(&data_dir, &refreshed);
        account = Some(refreshed);
    }

    if let Some(stored) = &account {
        let (paid, plan) = read_plan(stored).await;
        if paid != stored.paid || plan != stored.plan {
            let updated = StoredAccount {
                paid,
                plan: plan.clone(),
                ..stored.clone()
            };
            let _ = save(&data_dir, &updated);
            account = Some(updated);
        }
    }

    let account = account.as_ref();
    Json(json!({
        "linked": account.is_some(),
        "email": account.and_then(|stored| stored.email.clone()),
        "plan": account.and_then(|stored| stored.plan.clone()),
        "paid": account.is_some_and(|stored| stored.paid),
        // Community verifies no licence and so grants no edition. A paid host
        // answers this with what its licence actually says.
        "licensed_edition": serde_json::Value::Null,
        "sign_in": sign_in_state(),
    }))
    .into_response()
}

async fn sign_in_handler(State(state): State<SoloHttpState>) -> Response {
    if sign_in_state().state == "waiting" {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!({ "error": "A sign-in is already waiting on the browser." })),
        )
            .into_response();
    }

    let data_dir = state.registry.data_dir().to_path_buf();
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) => {
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Could not open a port for the sign-in: {error}") })),
            )
                .into_response();
        }
    };
    let port = match listener.local_addr() {
        Ok(address) => address.port(),
        Err(error) => {
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                Json(json!({ "error": format!("Could not open a port for the sign-in: {error}") })),
            )
                .into_response();
        }
    };

    let request = LinkRequest {
        base_url: account_base_url(),
        redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
        state: uuid::Uuid::new_v4().to_string(),
        pkce: Pkce::generate(),
        device_name: device_name(),
    };
    let url = request.authorize_url();
    set_sign_in(SignIn {
        state: "waiting",
        error: None,
        authorize_url: Some(url.clone()),
    });
    tracing::info!(target: "solo::account", device = %request.device_name, "Solo account sign-in started");
    open_in_browser(&url);

    tokio::spawn(async move {
        match finish(&data_dir, listener, &request).await {
            Ok(()) => set_sign_in(SignIn {
                state: "done",
                ..SignIn::default()
            }),
            Err(error) => {
                tracing::warn!(target: "solo::account", %error, "Solo account sign-in failed");
                set_sign_in(SignIn {
                    state: "failed",
                    error: Some(error),
                    authorize_url: None,
                });
            }
        }
    });

    Json(json!({ "authorize_url": url })).into_response()
}

async fn finish(
    data_dir: &Path,
    listener: tokio::net::TcpListener,
    request: &LinkRequest,
) -> Result<(), String> {
    let code = await_callback(listener, &request.state).await?;
    let mut stored = exchange_code(request, &code).await?;
    let (paid, plan) = read_plan(&stored).await;
    stored.paid = paid;
    stored.plan = plan;
    save(data_dir, &stored)?;
    tracing::info!(target: "solo::account", paid, "this machine is signed in to a Solo account");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_verifier_and_its_challenge_are_what_the_service_expects() {
        let pkce = Pkce::generate();
        // The service's schema is [A-Za-z0-9_-]{43,128} for both.
        assert_eq!(pkce.verifier.len(), 43);
        assert_eq!(pkce.challenge.len(), 43);
        assert_ne!(pkce.verifier, pkce.challenge);
        assert_eq!(
            pkce.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier.as_bytes()))
        );
    }

    #[test]
    fn the_authorize_url_escapes_what_a_person_named_their_machine() {
        let request = LinkRequest {
            base_url: "https://hextek.io/".to_string(),
            redirect_uri: "http://127.0.0.1:53219/solo/callback".to_string(),
            state: "state-1".to_string(),
            pkce: Pkce::generate(),
            device_name: "Nate's PC & laptop".to_string(),
        };
        let url = request.authorize_url();
        assert!(url.starts_with("https://hextek.io/SoloCommunity/connect?"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A53219%2Fsolo%2Fcallback"));
        assert!(url.contains("device_name=Nate%27s%20PC%20%26%20laptop"));
        assert!(!url.contains("PC & laptop"));
    }

    #[test]
    fn an_account_from_another_schema_is_ignored_rather_than_guessed_at() {
        let temporary = tempfile::tempdir().unwrap();
        let path = account_file(temporary.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"schema_version":99}"#).unwrap();
        assert!(load(temporary.path()).is_none());
    }

    #[test]
    fn what_was_saved_comes_back() {
        let temporary = tempfile::tempdir().unwrap();
        let account = StoredAccount {
            schema_version: SCHEMA_VERSION,
            base_url: "https://hextek.io".to_string(),
            device_id: "dev_1".to_string(),
            access_token: "access".to_string(),
            access_expires_at: 1_800_000_000,
            refresh_token: "refresh".to_string(),
            email: Some("a@example.test".to_string()),
            plan: Some("Solo Pro".to_string()),
            paid: true,
        };
        save(temporary.path(), &account).unwrap();
        let loaded = load(temporary.path()).expect("an account");
        assert!(loaded.paid);
        assert_eq!(loaded.plan.as_deref(), Some("Solo Pro"));
        assert_eq!(loaded.device_id, "dev_1");
    }

    #[test]
    fn an_expiring_token_counts_as_expired() {
        let account = StoredAccount {
            schema_version: SCHEMA_VERSION,
            base_url: String::new(),
            device_id: String::new(),
            access_token: String::new(),
            access_expires_at: 1_000,
            refresh_token: String::new(),
            email: None,
            plan: None,
            paid: false,
        };
        assert!(account.access_token_expired(1_000));
        assert!(account.access_token_expired(950));
        assert!(!account.access_token_expired(800));
    }
}
