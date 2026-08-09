// SPDX-License-Identifier: Apache-2.0

//! `solo mcp-stdio` — run the MCP server over stdio.
//!
//! **Legacy / fallback transport.** The recommended path for v0.11.4+ is
//! to run `solo daemon` (one long-running process) and connect MCP
//! clients to its `/mcp` HTTP endpoint. That pattern lets multiple MCP
//! clients (Claude Code, Codex, ChatGPT, plus solo-web) share one
//! writer-actor and see each other's writes in real time. See
//! `docs/book/src/mcp-integration.md` for the per-client config matrix.
//!
//! `mcp-stdio` is still the right answer when:
//!   - you don't want a long-running daemon process;
//!   - you only ever use one MCP client and don't run solo-web;
//!   - the MCP host can ONLY spawn stdio subprocesses and you can't
//!     use the `npx mcp-remote` shim (e.g. offline / npx-blocked env).
//!
//! Spawned as a subprocess by stdio-only MCP clients. The client's
//! config looks like (Claude Desktop direct-spawn):
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "solo": {
//!       "command": "solo",
//!       "args": ["mcp-stdio"],
//!       "env": { "SOLO_PASSPHRASE": "..." }
//!     }
//!   }
//! }
//! ```
//!
//! Lifecycle is the same as the one-shot subcommands: prepare_oneshot
//! handles passphrase + lockfile + storage setup; we run the rmcp server
//! loop until stdin closes (parent disconnect), then shut down via
//! `OneShotContext::shutdown` to drain the writer + persist the snapshot.

use anyhow::{Context, Result};
use clap::Args;
use solo_api::SoloMcpServer;
use std::path::PathBuf;

use crate::commands::common::{OneShotOpts, prepare_oneshot_opts};

#[derive(Debug, Args)]
pub struct McpStdioArgs {
    /// Override the data dir (default: `~/.solo`, override with
    /// `SOLO_DATA_DIR`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Proxy-friendly mode: skip `solo.lock` acquisition so a gateway
    /// (Cloudflare Access, Pomerium, identity-aware proxy, etc.) can
    /// spawn multiple ephemeral `solo mcp-stdio` subprocesses against
    /// one shared data dir. **Dangerous**: breaks the writer-actor
    /// single-process invariant (ADR-0003); two processes writing
    /// concurrently can desync writer-actor state. Only safe when the
    /// gateway serialises writes externally, or all spawned subprocesses
    /// are read-only. Logs a `tracing::warn!` at startup. See
    /// `docs/dev-log/0155-mcp-stdio-proxy-mode.md` for rationale and
    /// the full safety analysis.
    #[arg(long, env = "SOLO_NO_LOCKFILE")]
    pub no_lockfile: bool,
}

pub async fn run(args: McpStdioArgs) -> Result<()> {
    let opts = OneShotOpts {
        no_lockfile: args.no_lockfile,
    };
    let ctx = prepare_oneshot_opts(args.data_dir, opts).await?;
    let workspace_file_access = solo_api::WorkspaceFileAccessPolicy::from_config_and_env(
        ctx.config().workspace_file_access.allowed_roots.as_deref(),
    )
    .context("build workspace file access policy")?;
    tracing::info!("solo mcp-stdio: serving over stdio");

    let server = SoloMcpServer::new_for_tenant_with_workspace_file_access(
        ctx.library.clone(),
        ctx.library_handle.clone(),
        ctx.config().identity.user_aliases.clone(),
        workspace_file_access,
    );

    let serve_result = solo_api::serve_stdio(server).await;
    tracing::info!("solo mcp-stdio: stdio loop exited; cleaning up");

    // Always run shutdown so we drain the writer + save snapshot, even if
    // the rmcp loop ended on error (parent crashed mid-message).
    ctx.shutdown().await.context("mcp-stdio shutdown")?;
    serve_result.context("rmcp serve loop")
}
