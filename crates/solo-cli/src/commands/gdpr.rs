// SPDX-License-Identifier: Apache-2.0

//! `solo gdpr forget --subject <subject>` — hard-delete every row in the
//! Community Memory Library attributed to one principal subject.
//!
//! v0.8.0 P6. The CLI is the only surface exposing this — admin-tier
//! by design (not routed through MCP or HTTP).
//!
//! ## Confirmation gates
//!
//! `--confirm` is required (refuses without it). If the pre-scan
//! estimates more than 100 episodes, also requires `--double-confirm`.
//! Order matters: estimate FIRST, then check gates, then run.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use solo_core::LibraryId;
use std::path::PathBuf;

use crate::commands::common::AdminContext;

/// `solo gdpr` subcommand tree. v0.8.0 P6 ships `forget`; future
/// GDPR-related operators (e.g. `export`) can land here without
/// changing the top-level CLI surface.
#[derive(Debug, Subcommand)]
pub enum GdprCommand {
    /// Hard-delete every row tied to `--subject`.
    /// Irreversible. Requires `--confirm`; large scopes also require
    /// `--double-confirm`.
    Forget(ForgetArgs),
}

#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// Override the data dir (default: `~/.solo`, or `SOLO_DATA_DIR`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Principal subject (typically a JWT `sub` claim) whose data
    /// should be erased. Exact match against
    /// `episodes.principal_subject` and
    /// `document_chunks.ingested_by_principal`.
    #[arg(long)]
    pub subject: String,

    /// Required: confirm the destructive action.
    #[arg(long)]
    pub confirm: bool,

    /// Required when the pre-scan estimates > 100 affected episodes.
    /// The CLI prints the estimated scope before running and refuses
    /// without this gate if the threshold is exceeded.
    #[arg(long)]
    pub double_confirm: bool,
}

/// Threshold above which `--double-confirm` is required.
const DOUBLE_CONFIRM_EPISODE_THRESHOLD: u64 = 100;

pub async fn run(cmd: GdprCommand) -> Result<()> {
    match cmd {
        GdprCommand::Forget(args) => run_forget(args).await,
    }
}

async fn run_forget(args: ForgetArgs) -> Result<()> {
    if !args.confirm {
        bail!(
            "refusing to forget without --confirm. \
             Re-run with `solo gdpr forget --subject <subject> --confirm` \
             (this is irreversible)."
        );
    }

    let subject_trimmed = args.subject.trim();
    if subject_trimmed.is_empty() {
        bail!("--subject must not be empty");
    }

    let tenant_id = LibraryId::default_tenant();

    // Bootstrap: lockfile + key + registry. v0.8.0 P7 consolidates the
    // duplicated bootstrap into AdminContext — single passphrase prompt,
    // single derived key, threading into both registry open and any
    // downstream admin-audit emit. The forget_principal helper writes
    // the admin-audit row itself (using the same derived key, via
    // library_backup-style insert_audit_admin_row), so AdminContext just
    // owns the bootstrap here.
    let admin = AdminContext::bootstrap(args.data_dir)?;

    // Pre-scan ESTIMATE first — on a separate connection so it
    // doesn't open the writer-actor needlessly.
    let estimate_db_path = admin.data_dir().join(solo_storage::COMMUNITY_DB_FILENAME);
    let (estimated_episodes, estimated_chunks) = if estimate_db_path.is_file() {
        solo_storage::estimate_forget_scope(&estimate_db_path, admin.key(), subject_trimmed)
            .context("estimate forget scope")?
    } else {
        (0u64, 0u64)
    };

    eprintln!(
        "About to forget subject=`{subject_trimmed}` in the Memory Library: ~{estimated_episodes} episodes, ~{estimated_chunks} chunks. \
         HNSW will be rebuilt."
    );

    if estimated_episodes > DOUBLE_CONFIRM_EPISODE_THRESHOLD && !args.double_confirm {
        admin.shutdown().await?;
        bail!(
            "scope exceeds {DOUBLE_CONFIRM_EPISODE_THRESHOLD} episodes (estimated: {estimated_episodes}). Pass --double-confirm to proceed."
        );
    }

    let tenant_handle = admin.open_library().await?;

    // Run the forget under spawn_blocking — the SQL + HNSW rebuild is
    // synchronous CPU + I/O work.
    let key_for_forget = admin.key().clone();
    let data_dir_for_forget = admin.data_dir().to_path_buf();
    let subject_owned = subject_trimmed.to_string();
    let handle_for_forget = tenant_handle.clone();
    let report = tokio::task::spawn_blocking(move || {
        solo_storage::forget_principal(
            handle_for_forget,
            &subject_owned,
            None,
            &data_dir_for_forget,
            &key_for_forget,
        )
    })
    .await
    .context("spawn_blocking forget_principal")??;

    println!("✓ forgot subject=`{subject_trimmed}` in tenant=`{tenant_id}`");
    println!(
        "  episodes_deleted = {}, triples_deleted = {}, chunks_deleted = {}, hnsw_rebuilt = {}",
        report.episodes_deleted, report.triples_deleted, report.chunks_deleted, report.hnsw_rebuilt
    );
    println!(
        "  admin audit row id = {} (in tenants_index.db::audit_events_admin)",
        report.audit_admin_row_id
    );

    // Cleanup. Drop our extra Arc first so shutdown sees a single owner
    // per handle.
    drop(tenant_handle);
    admin.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn forget_without_confirm_refuses() {
        let args = ForgetArgs {
            data_dir: None,
            subject: "alice".to_string(),
            confirm: false,
            double_confirm: false,
        };
        let err = run_forget(args)
            .await
            .expect_err("must refuse without --confirm");
        let msg = err.to_string();
        assert!(msg.contains("--confirm"), "got `{msg}`");
        assert!(msg.contains("irreversible"), "got `{msg}`");
    }

    #[tokio::test]
    async fn forget_with_empty_subject_refuses() {
        let args = ForgetArgs {
            data_dir: None,
            subject: "   ".to_string(),
            confirm: true,
            double_confirm: false,
        };
        let err = run_forget(args)
            .await
            .expect_err("empty subject must refuse");
        let msg = err.to_string();
        assert!(msg.contains("--subject"), "got `{msg}`");
    }
}
