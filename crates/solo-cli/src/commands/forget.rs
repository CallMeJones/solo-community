// SPDX-License-Identifier: Apache-2.0

//! `solo forget <memory-id> [--reason ...]` — soft-delete an episode.
//!
//! Per ADR-0003, `forget` flips `episodes.status` to `'forgotten'` but
//! leaves the HNSW vector in place — recall paths exclude the forgotten
//! row by SQL filter. The architecture preserves silent traces (the row
//! and its embedding stay for forensics + consolidation).

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_core::MemoryId;
use std::path::PathBuf;
use std::str::FromStr;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct ForgetArgs {
    /// MemoryId (UUID v7) of the episode to forget.
    pub memory_id: String,

    /// Free-form reason. Logged but not yet persisted (no schema column).
    /// A future schema bump can promote this to a structured forget_log
    /// table; v0.1 surfaces it through tracing only.
    #[arg(long, default_value = "user-initiated")]
    pub reason: String,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: ForgetArgs) -> Result<()> {
    let mid = MemoryId::from_str(&args.memory_id)
        .map_err(|e| anyhow::anyhow!("invalid memory_id `{}`: {e}", args.memory_id))?;

    let ctx = prepare_oneshot(args.data_dir).await?;
    match ctx.write_handle().forget(mid, args.reason).await {
        Ok(()) => {
            println!("✓ forgotten: {mid}");
        }
        Err(e) => {
            // Make sure shutdown still runs before bailing.
            ctx.shutdown().await.ok();
            bail!("forget failed: {e}");
        }
    }
    ctx.shutdown().await.context("shutdown after forget")
}
