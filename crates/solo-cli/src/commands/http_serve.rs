// SPDX-License-Identifier: Apache-2.0

//! `solo http-serve` — run the HTTP/JSON transport.
//!
//! Binds to `127.0.0.1:<port>` (loopback only) and serves four endpoints
//! over JSON:
//!
//!   - `POST   /memory`           — remember
//!   - `POST   /memory/search`    — recall
//!   - `GET    /memory/{id}`      — inspect
//!   - `DELETE /memory/{id}`      — forget
//!   - `GET    /health`           — liveness probe
//!
//! Lifecycle is the same as `solo mcp-stdio` — same `OneShotContext`
//! lockfile-and-startup, same shutdown choreography. Awaits Ctrl+C +
//! SIGTERM; on signal, axum's `with_graceful_shutdown` drains in-flight
//! handlers, then `OneShotContext::shutdown` flushes the writer + saves
//! the snapshot.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_api::{AuthConfig, SoloHttpState};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct HttpServeArgs {
    /// IP address to bind. Defaults to 127.0.0.1 (loopback / local-only).
    /// Setting this to anything else (e.g. 0.0.0.0 or a LAN IP)
    /// REQUIRES `--bearer-token-file` — the daemon refuses to start
    /// otherwise to prevent accidental open exposure.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: IpAddr,

    /// TCP port to bind. Defaults to 17821.
    #[arg(long, default_value_t = 17821)]
    pub port: u16,

    /// Path to a file containing the bearer token. The first line of
    /// the file (whitespace-trimmed) is used. Every request except
    /// `GET /health` must carry `Authorization: Bearer <token>`.
    /// Required when `--bind` is non-loopback.
    #[arg(long)]
    pub bearer_token_file: Option<PathBuf>,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: HttpServeArgs) -> Result<()> {
    // Install shutdown signals BEFORE any heavy work, same race
    // mitigation as `solo daemon` — tokio's
    // `signal(SignalKind::terminate())` only installs the handler at
    // call time, so if we waited until after `prepare_oneshot` (key
    // derivation + writer spawn + reader pool) on a slow Linux runner,
    // a SIGTERM in that window would fall through to OS-default kill.
    // See `commands::common::ShutdownSignals`.
    let shutdown_signals = crate::commands::common::ShutdownSignals::install()
        .context("install shutdown signal handlers")?;

    // Refuse to start if we'd bind to a non-loopback address without a
    // bearer token. Prevents the "I just changed --bind" foot-gun from
    // accidentally exposing the data dir.
    let is_loopback = args.bind.is_loopback();
    if !is_loopback && args.bearer_token_file.is_none() {
        bail!(
            "binding to {} (non-loopback) requires --bearer-token-file. \
             Refusing to expose the API without authentication.",
            args.bind
        );
    }

    let cli_bearer_token = match &args.bearer_token_file {
        Some(p) => Some(read_bearer_token_file(p)?),
        None => None,
    };

    let ctx = prepare_oneshot(args.data_dir).await?;
    let workspace_file_access = solo_api::WorkspaceFileAccessPolicy::from_config_and_env(
        ctx.config().workspace_file_access.allowed_roots.as_deref(),
    )
    .context("build workspace file access policy")?;

    // v0.8.0 P3 auth resolution order:
    //   1. `[auth]` block in solo.config.toml wins if present.
    //   2. `--bearer-token-file <path>` falls through to a bearer config.
    //   3. Otherwise the server runs unauthenticated (loopback default).
    //
    // The non-loopback bind guard above already refuses (2) without (1)
    // OR an explicit `--bearer-token-file`.
    let auth = match ctx.config().auth.clone() {
        Some(settings) => Some(AuthConfig::from(settings)),
        None => cli_bearer_token.map(|token| AuthConfig::Bearer { token }),
    };

    let state = SoloHttpState {
        registry: ctx.library.clone(),
        user_aliases: Arc::new(ctx.config().identity.user_aliases.clone()),
        workspace_file_access,
        // v0.11.0 P1: per-process MCP session store. The background
        // sweep task is spawned on the surrounding tokio runtime
        // (`tokio::main` in solo-cli).
        mcp_sessions: solo_api::mcp_session::SessionStore::new(),
        mcp_tasks: solo_api::mcp_task::TaskStore::new(),
        steward_runtime: solo_api::StewardRuntimeStatus::new(),
        runtime_control: solo_api::RuntimeControl::unavailable(),
    };

    let _ = Ipv4Addr::LOCALHOST;
    let addr = SocketAddr::new(args.bind, args.port);
    let auth_kind = match &auth {
        Some(AuthConfig::Bearer { .. }) => "bearer",
        Some(AuthConfig::Oidc { .. }) => "oidc",
        None => "none",
    };
    tracing::info!(%addr, auth = auth_kind,
                   "solo http-serve: starting (Ctrl+C to stop)");

    let serve_result = solo_api::http::serve_http_with_auth_config(
        addr,
        state,
        auth,
        shutdown_signals.await_any(),
    )
    .await;

    tracing::info!("solo http-serve: server stopped; cleaning up");
    ctx.shutdown().await.context("http-serve shutdown")?;
    serve_result.context("axum serve")
}

/// Read the first line of `path`, trim ASCII whitespace, return as the
/// bearer token. Empty / missing file → clean error.
fn read_bearer_token_file(path: &std::path::Path) -> Result<String> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("read bearer token file {}", path.display()))?;
    let token = body.lines().next().unwrap_or("").trim().to_string();
    if token.is_empty() {
        bail!(
            "bearer token file {} is empty (first line was blank)",
            path.display()
        );
    }
    Ok(token)
}
