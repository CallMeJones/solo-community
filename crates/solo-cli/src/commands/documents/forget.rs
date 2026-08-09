// SPDX-License-Identifier: Apache-2.0

//! `solo documents forget <doc-id> [--reason ...]` — soft-delete a
//! document.
//!
//! Wraps [`WriteHandle::forget_document`]. The document row's status
//! flips to `'forgotten'`, every chunk's HNSW rowid is tombstoned,
//! and the chunk rows themselves stay in `document_chunks` for forensic
//! value. Re-ingesting the same content later will dedup back to the
//! same `doc_id` (and reactivation is a future restore command).

use anyhow::{Context, Result, bail};
use clap::Args;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// Document id (full UUID or unique hex prefix; at least 4 hex
    /// chars for prefix lookup).
    pub doc_id: String,

    /// Free-form reason. Logged via tracing but not yet persisted
    /// (no `reason` column on `documents` — same shape as episode
    /// forget). A future schema bump may promote this to a structured
    /// `document_forget_log` table.
    #[arg(long)]
    pub reason: Option<String>,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: ForgetArgs) -> Result<()> {
    let ctx = prepare_oneshot(args.data_dir).await?;
    let doc_id = match super::resolve_doc_id(ctx.read_pool(), &args.doc_id).await {
        Ok(id) => id,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e);
        }
    };

    if let Some(reason) = &args.reason {
        tracing::info!(%doc_id, %reason, "forgetting document");
    } else {
        tracing::info!(%doc_id, "forgetting document (no reason given)");
    }

    let report = match ctx.write_handle().forget_document(doc_id).await {
        Ok(r) => r,
        Err(e) => {
            ctx.shutdown().await.ok();
            bail!("forget_document failed: {e}");
        }
    };

    println!(
        "✓ forgotten: {} ({} chunks tombstoned)",
        report.doc_id, report.chunks_tombstoned,
    );

    ctx.shutdown()
        .await
        .context("shutdown after documents forget")
}
