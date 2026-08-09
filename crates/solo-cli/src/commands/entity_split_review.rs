// SPDX-License-Identifier: Apache-2.0

//! `solo request-entity-split --entity-id ID --alias ALIAS [...]` records
//! an entity split review request without rewriting graph data.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_storage::EntitySplitRequest;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct EntitySplitReviewArgs {
    /// Canonical entity id that currently owns the aliases.
    #[arg(long, value_name = "ENTITY_ID")]
    pub entity_id: String,

    /// Alias or label that should be split out for review. Repeatable.
    #[arg(long = "alias", value_name = "ALIAS", required = true)]
    pub aliases: Vec<String>,

    /// Optional reviewer-facing reason.
    #[arg(long)]
    pub reason: Option<String>,

    /// Data directory (defaults to `$SOLO_DATA_DIR` or `~/.solo`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: EntitySplitReviewArgs) -> Result<()> {
    let ctx = prepare_oneshot(args.data_dir).await?;

    let report = match ctx
        .write_handle()
        .request_entity_split_as(
            None,
            EntitySplitRequest {
                entity_id: args.entity_id,
                affected_aliases: args.aliases,
                reason: args.reason,
            },
        )
        .await
    {
        Ok(report) => report,
        Err(e) => {
            ctx.shutdown().await.ok();
            bail!("request-entity-split failed: {e}");
        }
    };

    println!(
        "request-entity-split recorded: op_id={} status={} entity={} aliases={}",
        report.op_id,
        report.status,
        report.source_entity_id,
        report.affected_aliases.join(", ")
    );

    ctx.shutdown()
        .await
        .context("shutdown after request-entity-split")
}
