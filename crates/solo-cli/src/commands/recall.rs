// SPDX-License-Identifier: Apache-2.0

//! `solo recall <query> [--limit N]` — vector-search a query string.
//!
//! The recall pipeline (embed → HNSW search → SQL fetch → status
//! filter) lives in [`solo_query::run_recall`]; this command is just
//! the CLI's text formatter for the resulting `RecallResult`.

use anyhow::{Context, Result};
use clap::Args;
use solo_query::run_recall;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct RecallArgs {
    /// Query text. Read from stdin if omitted.
    pub query: Option<String>,

    /// Number of results to return.
    #[arg(long, default_value_t = 5)]
    pub limit: usize,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: RecallArgs) -> Result<()> {
    let raw_query = match args.query {
        Some(q) => q,
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read query from stdin")?;
            buf
        }
    };
    // Strip trailing whitespace/newlines so `echo "foo" | solo recall`
    // and `solo recall foo` produce identical embeddings (matches the
    // remember command's normalisation).
    let query = raw_query.trim_end().to_string();

    let ctx = prepare_oneshot(args.data_dir).await?;
    // Pipeline lives in solo-query::recall; transports just format.
    let result = run_recall(
        ctx.library_handle.as_ref(),
        // CLI is implicitly trusted; pass `None` for audit principal.
        None,
        &query,
        args.limit,
    )
    .await
    .context("recall")?;

    if result.hits.is_empty() {
        if result.index_len == 0 {
            println!("(no results — index has 0 vectors)");
        } else {
            println!(
                "(no results — index has {} vector(s); HNSW returned no hits or all were forgotten)",
                result.index_len
            );
        }
    } else {
        for h in &result.hits {
            // Label is `cos_dist=` (not `cos=`) so the value isn't
            // mistaken for a similarity score. 0.0 = identical, larger
            // = less similar. Matches the wire field name `cos_distance`
            // in HTTP/MCP JSON output.
            println!(
                "{:>6}  cos_dist={:>7.4}  {}  [{}/{}]",
                h.rowid,
                h.cos_distance,
                truncate(&h.content, 80),
                h.source_type,
                h.tier,
            );
        }
    }

    ctx.shutdown().await.context("shutdown after recall")
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s
    }
}
