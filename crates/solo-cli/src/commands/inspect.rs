// SPDX-License-Identifier: Apache-2.0

//! `solo inspect <memory-id>` — read-only fetch of an episode's full record.

use anyhow::{Context, Result};
use clap::Args;
use solo_core::MemoryId;
use solo_query::inspect_one;
use std::path::PathBuf;
use std::str::FromStr;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct InspectArgs {
    /// MemoryId to inspect.
    pub memory_id: String,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: InspectArgs) -> Result<()> {
    let mid = MemoryId::from_str(&args.memory_id)
        .map_err(|e| anyhow::anyhow!("invalid memory_id `{}`: {e}", args.memory_id))?;

    let ctx = prepare_oneshot(args.data_dir).await?;
    // CLI is implicitly trusted; pass `None` for the audit principal.
    let row = match inspect_one(ctx.read_pool(), ctx.library_handle.audit(), None, mid).await {
        Ok(r) => r,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e.into());
        }
    };

    println!("memory_id     : {}", row.memory_id);
    println!("ts_ms         : {} ({})", row.ts_ms, fmt_ms(row.ts_ms));
    println!(
        "source        : {} / {}",
        row.source_type,
        row.source_id.as_deref().unwrap_or("-")
    );
    println!("tier          : {}", row.tier);
    println!("status        : {}", row.status);
    println!("confidence    : {:.3}", row.confidence);
    println!("strength      : {:.3}", row.strength);
    println!("salience      : {:.3}", row.salience);
    println!("created       : {}", fmt_ms(row.created_at_ms));
    println!("updated       : {}", fmt_ms(row.updated_at_ms));
    println!("encoding ctx  : {}", row.encoding_context_json);
    if let Some(p) = &row.provenance_json {
        println!("provenance    : {p}");
    }
    println!();
    println!("content       :");
    println!("{}", indent(&row.content, "  "));

    ctx.shutdown().await.context("shutdown after inspect")
}

fn fmt_ms(ms: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("invalid({ms})"))
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
