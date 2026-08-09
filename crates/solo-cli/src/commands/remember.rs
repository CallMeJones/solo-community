// SPDX-License-Identifier: Apache-2.0

//! `solo remember <text>` — one-shot write.
//!
//! Embeds the text via the configured embedder (StubEmbedder until 1.4.b),
//! then dispatches `WriteCommand::Remember` to the writer actor. Prints
//! the resulting `MemoryId` on stdout.

use anyhow::{Context, Result};
use clap::Args;
use solo_core::{Confidence, EncodingContext, Episode, MemoryId, Tier};
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct RememberArgs {
    /// Text to remember. If omitted, read from stdin.
    pub text: Option<String>,

    /// Source-type tag. Defaults to "user_message". Free-form; `solo recall`
    /// can filter on this when scoping searches to a particular source.
    #[arg(long, default_value = "user_message")]
    pub source_type: String,

    /// Optional source ID — e.g., the upstream message ID for traceability.
    #[arg(long)]
    pub source_id: Option<String>,

    /// Override the data dir (default: `~/.solo`, override with
    /// `SOLO_DATA_DIR`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: RememberArgs) -> Result<()> {
    let raw_text = match args.text {
        Some(t) => t,
        None => read_stdin().context("read text from stdin")?,
    };
    // Strip trailing whitespace/newlines so `echo "foo" | solo remember`
    // and `solo remember foo` produce identical embeddings.
    let text = raw_text.trim_end().to_string();
    if text.trim().is_empty() {
        anyhow::bail!("text must not be empty");
    }

    let ctx = prepare_oneshot(args.data_dir).await?;
    let embedding = ctx.embedder.embed(&text).await.context("embed text")?;

    let episode = Episode {
        memory_id: MemoryId::new(),
        ts_ms: chrono::Utc::now().timestamp_millis(),
        source_type: args.source_type,
        source_id: args.source_id,
        content: text,
        encoding_context: EncodingContext::default(),
        provenance: None,
        // Default scoring values; commit 1.7+ exposes flags or computes.
        confidence: Confidence::new(0.9).unwrap(),
        strength: 0.5,
        salience: 0.5,
        tier: Tier::Hot,
    };

    let mid = ctx
        .write_handle()
        .remember(episode, embedding)
        .await
        .context("writer.remember")?;

    println!("✓ remembered: {mid}");

    ctx.shutdown().await.context("shutdown after remember")
}

fn read_stdin() -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}
