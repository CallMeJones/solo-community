// SPDX-License-Identifier: Apache-2.0

//! Solo CLI: subcommand dispatch.
//!
//! Subcommand implementations live in `crate::commands::*`. This file is just
//! the clap definition + dispatch.

use anyhow::Result;
use clap::{Parser, Subcommand};

pub mod commands;

#[derive(Debug, Parser)]
#[command(
    name = "solo",
    version = solo_core::build_info::version_with_build_metadata_static(),
    about = "Local-first personal memory for AI assistants",
    long_about = "\
Solo is a single-binary daemon that owns your long-term LLM memory locally.\n\
Run `solo init` to set up a fresh data directory, then either:\n\
  - `solo daemon [--http-port N]` to keep it warm + serve HTTP, or\n\
  - `solo mcp-stdio` for spawned-by-LLM-client subprocess use."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize the Solo data directory (creates ~/.solo/solo.db).
    Init(commands::init::InitArgs),
    /// Run the Solo daemon (writer + reader pool; MCP/HTTP transports follow).
    Daemon(commands::daemon::DaemonArgs),
    /// One-shot: store a memory.
    Remember(commands::remember::RememberArgs),
    /// One-shot: query memory.
    Recall(commands::recall::RecallArgs),
    /// Trigger the consolidation pass (clustering + abstraction)
    /// once. Idempotent. Useful for manual runs, bulk imports, or
    /// deterministic-timing tests when the daemon's
    /// `--consolidate-interval-secs` isn't set.
    Consolidate(commands::consolidate::ConsolidateArgs),
    /// Re-embed stored memories with the current embedder. Low-level;
    /// prefer `migrate-embedder` for persisted backend changes.
    Reembed(commands::reembed::ReembedArgs),
    /// Safely migrate the persisted embedder and re-embed the Memory Library.
    MigrateEmbedder {
        #[command(subcommand)]
        cmd: commands::embedder_migrate::MigrateEmbedderCommand,
    },
    /// Soft-delete a memory by ID.
    Forget(commands::forget::ForgetArgs),
    /// Inspect a memory by ID (full record + status + provenance).
    Inspect(commands::inspect::InspectArgs),
    /// Run the MCP server over stdio (for use by LLM clients).
    McpStdio(commands::mcp_stdio::McpStdioArgs),
    /// Run the HTTP/JSON server. Defaults to 127.0.0.1; use `--bind`
    /// + `--bearer-token-file` for trusted-LAN deployments.
    HttpServe(commands::http_serve::HttpServeArgs),
    /// Print version, data-dir state, file presence, embedder identity,
    /// and (with --with-stats) live database statistics.
    Doctor(commands::doctor::DoctorArgs),
    /// Online encrypted backup. Writes a self-contained SQLCipher
    /// database to `--to <path>`, encrypted with the same Argon2id-
    /// derived key as the source. Holds `solo.lock` for the duration —
    /// daemon must be stopped (or use a different `--data-dir`).
    Backup(commands::backup::BackupArgs),
    /// Restore the one Community Memory Library from an encrypted backup.
    /// Solo must be stopped; replacement requires `--confirm`.
    Restore(commands::restore::RestoreArgs),
    /// Rewrite historical `triples.subject_id` / `triples.object_id`
    /// values per `--alias FROM=TO` pairs. Opt-in backfill that
    /// complements read-path alias resolution
    /// (`IdentityConfig.user_aliases`). Defaults to recommending
    /// `--dry-run` first because the rewrite is irreversible.
    NormalizeSubjects(commands::normalize_subjects::NormalizeSubjectsArgs),
    /// Request review for splitting aliases out of one canonical entity.
    RequestEntitySplit(commands::entity_split_review::EntitySplitReviewArgs),
    /// Import external data sources, including markdown/Obsidian vaults.
    Import {
        #[command(subcommand)]
        cmd: commands::import::ImportCommand,
    },
    /// Codebase memory helpers for local projects.
    Project {
        #[command(subcommand)]
        cmd: commands::project::ProjectCommand,
    },
    /// Ingest a document (or every allowed-extension file under a
    /// directory) into Solo's document memory. New in v0.7.0.
    Ingest(commands::ingest::IngestArgs),
    /// Manage ingested documents (list / inspect / forget).
    Documents {
        #[command(subcommand)]
        cmd: commands::documents::DocumentsCommand,
    },
    /// Run deterministic offline memory-quality eval fixtures.
    Eval {
        #[command(subcommand)]
        cmd: commands::eval::EvalCommand,
    },
    /// Preview, write, or verify MCP client setup for local tools.
    SetupClient {
        #[command(subcommand)]
        cmd: commands::setup_client::SetupClientCommand,
    },
    /// Manage the audit log (list / purge / export). New in v0.8.0 P4.
    Audit {
        #[command(subcommand)]
        cmd: commands::audit::AuditCommand,
    },
    /// GDPR right-to-erasure operations. New in v0.8.0 P6.
    Gdpr {
        #[command(subcommand)]
        cmd: commands::gdpr::GdprCommand,
    },
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Init(args)) => commands::init::run(args).await,
        Some(Command::Daemon(args)) => commands::daemon::run(args).await,
        Some(Command::Remember(args)) => commands::remember::run(args).await,
        Some(Command::Recall(args)) => commands::recall::run(args).await,
        Some(Command::Consolidate(args)) => commands::consolidate::run(args).await,
        Some(Command::Reembed(args)) => commands::reembed::run(args).await,
        Some(Command::MigrateEmbedder { cmd }) => commands::embedder_migrate::run(cmd).await,
        Some(Command::Forget(args)) => commands::forget::run(args).await,
        Some(Command::Inspect(args)) => commands::inspect::run(args).await,
        Some(Command::McpStdio(args)) => commands::mcp_stdio::run(args).await,
        Some(Command::HttpServe(args)) => commands::http_serve::run(args).await,
        Some(Command::Doctor(args)) => commands::doctor::run(args).await,
        Some(Command::Backup(args)) => commands::backup::run(args).await,
        Some(Command::Restore(args)) => commands::restore::run(args).await,
        Some(Command::NormalizeSubjects(args)) => commands::normalize_subjects::run(args).await,
        Some(Command::RequestEntitySplit(args)) => commands::entity_split_review::run(args).await,
        Some(Command::Import { cmd }) => commands::import::run(cmd).await,
        Some(Command::Project { cmd }) => commands::project::run(cmd).await,
        Some(Command::Ingest(args)) => commands::ingest::run(args).await,
        Some(Command::Documents { cmd }) => commands::documents::run(cmd).await,
        Some(Command::Eval { cmd }) => commands::eval::run(cmd).await,
        Some(Command::SetupClient { cmd }) => commands::setup_client::run(cmd).await,
        Some(Command::Audit { cmd }) => commands::audit::run(cmd).await,
        Some(Command::Gdpr { cmd }) => commands::gdpr::run(cmd).await,
        None => {
            println!(
                "solo {}",
                solo_core::build_info::version_with_build_metadata()
            );
            println!();
            println!("Solo is a command-line app. If you double-clicked solo.exe,");
            println!("the window may close after this help text prints.");
            println!();
            println!("Open PowerShell in this folder and run:");
            println!("  .\\solo.exe init");
            println!("  .\\solo.exe doctor");
            println!("  .\\solo.exe daemon --http-port 17821");
            println!();
            println!("Commands:");
            println!("  solo init        - initialize a new data directory");
            println!("  solo daemon      - run the daemon (writer + reader pool)");
            println!("  solo remember    - one-shot write");
            println!("  solo recall      - one-shot vector search");
            println!("  solo consolidate - run a consolidation pass (clustering + abstraction)");
            println!("  solo reembed     - regenerate stored embeddings with the current model");
            println!("  solo migrate-embedder - safely switch embedder backend and reembed");
            println!("  solo forget      - soft-delete by id");
            println!("  solo inspect     - show a memory's full record");
            println!("  solo mcp-stdio   - run the MCP server over stdio (for LLM clients)");
            println!(
                "  solo http-serve  - run the HTTP/JSON server (--bind/--bearer-token-file for LAN)"
            );
            println!("  solo doctor      - diagnostics (--with-stats for live db counts)");
            println!("  solo backup      - encrypted online backup to --to <path>");
            println!(
                "  solo normalize-subjects - rewrite historical triple subjects/objects per --alias FROM=TO"
            );
            println!("  solo request-entity-split - record an entity split review request");
            println!("  solo import      - preview/import external sources");
            println!("  solo project     - codebase memory helpers for local projects");
            println!("  solo ingest      - ingest a document or directory of documents");
            println!("  solo documents   - list/inspect/forget ingested documents");
            println!("  solo eval        - run deterministic offline memory-quality fixtures");
            println!("  solo setup-client - configure MCP clients for local tools");
            println!("  solo audit       - manage the audit log (list/purge/export)");
            println!("  solo gdpr        - GDPR right-to-erasure operations");
            Ok(())
        }
    }
}
