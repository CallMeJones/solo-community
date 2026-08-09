// SPDX-License-Identifier: Apache-2.0

//! `solo documents list` — paginated browse.
//!
//! Thin wrapper over [`solo_query::list_documents`]. Defaults match the
//! library's defaults: active-only, newest-first, limit 20. Use
//! `--include-forgotten` for forensic listing.

use anyhow::{Context, Result};
use chrono::TimeZone;
use clap::Args;
use solo_query::list_documents;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Max rows to return. Library clamps to `[1, 100]`.
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Rows to skip (page through results by stepping `--offset`).
    #[arg(long, default_value_t = 0)]
    pub offset: usize,

    /// Include `status='forgotten'` documents in the output. Default
    /// hides forgotten docs (mirrors `solo recall`'s active-only
    /// filter for episodes).
    #[arg(long)]
    pub include_forgotten: bool,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: ListArgs) -> Result<()> {
    let ctx = prepare_oneshot(args.data_dir).await?;
    // CLI is implicitly trusted; pass `None` for the audit principal.
    let rows = match list_documents(
        ctx.read_pool(),
        ctx.library_handle.audit(),
        None,
        args.limit,
        args.offset,
        args.include_forgotten,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e.into());
        }
    };

    if rows.is_empty() {
        if args.offset == 0 && !args.include_forgotten {
            println!("(no documents — try `solo ingest <path>` or --include-forgotten)");
        } else {
            println!("(no documents in this page)");
        }
    } else {
        // Header. Widths picked so a typical row fits in ~110 cols.
        println!(
            "{:<8}  {:<25}  {:>6}  {:<9}  {}",
            "id", "title", "chunks", "status", "ingested"
        );
        println!(
            "{:<8}  {:<25}  {:>6}  {:<9}  {}",
            "--------", "-------------------------", "------", "---------", "--------"
        );
        for row in &rows {
            let title = row.title.as_deref().unwrap_or("(no title)");
            println!(
                "{:<8}  {:<25}  {:>6}  {:<9}  {}",
                short(&row.doc_id, 8),
                truncate(title, 25),
                row.chunk_count,
                fmt_status(&row.status),
                fmt_ms(row.ingested_at_ms),
            );
        }
    }

    ctx.shutdown()
        .await
        .context("shutdown after documents list")
}

fn short(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// UTF-8-safe truncation with trailing ellipsis. Mirrors the recall
/// command's helper but in this file's scope (small enough to inline).
fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

fn fmt_status(s: &solo_core::DocumentStatus) -> &'static str {
    match s {
        solo_core::DocumentStatus::Active => "active",
        solo_core::DocumentStatus::Forgotten => "forgotten",
    }
}

fn fmt_ms(ms: i64) -> String {
    chrono::Utc
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("invalid({ms})"))
}
