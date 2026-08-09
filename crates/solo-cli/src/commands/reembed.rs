// SPDX-License-Identifier: Apache-2.0

//! `solo reembed [--from-name X --from-version Y] [--dry-run] [--gc]` —
//! regenerate stored embeddings for memories whose existing embedding
//! row was produced by a non-current embedder.
//!
//! Typical use: a user has been running Solo with `StubEmbedder` (no
//! `SOLO_BGE_M3_DIR` set), accumulates 1000 memories, then downloads
//! BGE-M3 and sets `SOLO_BGE_M3_DIR`. Without `solo reembed`, recall
//! searches the HNSW which holds stub-hash vectors — incoherent with
//! the BGE-M3 vector produced for the query at recall time. Running
//! `solo reembed --gc` re-embeds every stored memory with BGE-M3 and
//! drops the stub rows.
//!
//! After a successful, non-dry-run reembed, this command also wipes
//! the on-disk HNSW snapshot pairs so the next daemon / one-shot
//! startup falls through to rebuild-from-SQL (see
//! `solo_storage::startup`'s third fallback branch). Without that
//! wipe, the next start would reload the stale snapshot and the
//! in-memory index would still hold prior-embedder vectors.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_storage::ReembedScope;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct ReembedArgs {
    /// Filter: only reembed memories whose existing embedding came from
    /// this embedder name. Pair with `--from-version`. Without either
    /// flag, every memory whose embedding's `embedder_id` differs from
    /// the active embedder's is a candidate.
    #[arg(long, requires = "from_version")]
    pub from_name: Option<String>,

    /// Filter: pair with `--from-name` to specify which prior embedder
    /// to migrate from.
    #[arg(long, requires = "from_name")]
    pub from_version: Option<String>,

    /// Walk + count only; report what would happen, write nothing.
    /// Snapshot pairs are also left alone in dry-run mode.
    #[arg(long)]
    pub dry_run: bool,

    /// After re-embedding each memory, DELETE the prior `embeddings`
    /// rows for that memory whose `embedder_id` differs from the
    /// current. Without `--gc`, the stale rows are kept (useful for
    /// rollback or audit; `solo reembed` is itself idempotent).
    #[arg(long)]
    pub gc: bool,

    /// Data directory (defaults to `$SOLO_DATA_DIR` or `~/.solo`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: ReembedArgs) -> Result<()> {
    let from = match (args.from_name.clone(), args.from_version.clone()) {
        (Some(n), Some(v)) => Some((n, v)),
        (None, None) => None,
        // clap's `requires` should make these unreachable, but spelt out
        // anyway in case someone reaches the function via tests.
        (Some(_), None) | (None, Some(_)) => {
            bail!("--from-name and --from-version must be provided together");
        }
    };

    let scope = ReembedScope {
        from,
        dry_run: args.dry_run,
        gc: args.gc,
    };

    let ctx = prepare_oneshot(args.data_dir).await?;
    let data_dir = ctx.data_dir.clone();
    let snapshot_dir = ctx.library_handle.snapshot_dir().to_path_buf();

    let report = match ctx.write_handle().reembed(scope).await {
        Ok(r) => r,
        Err(e) => {
            // Make sure shutdown still runs before bailing.
            ctx.shutdown().await.ok();
            bail!("reembed failed: {e}");
        }
    };

    if report.dry_run {
        println!(
            "reembed --dry-run: {} memor{} would be re-embedded",
            report.rows_seen,
            if report.rows_seen == 1 { "y" } else { "ies" }
        );
        // Dry-run path leaves snapshots untouched.
        return ctx
            .shutdown()
            .await
            .context("shutdown after reembed --dry-run");
    }

    println!(
        "reembed complete: seen={} reembedded={} failed={} gc_deleted={}",
        report.rows_seen, report.rows_reembedded, report.rows_failed, report.rows_gc_deleted,
    );

    // Wipe the on-disk HNSW snapshot pairs. Next start will fall through
    // to rebuild-from-SQL. Skip when nothing was actually re-embedded
    // (zero-row migrations leave the snapshot fine).
    //
    // v0.8.0 P2: snapshots live in the per-tenant subdir
    // `<data_dir>/tenants/<tenant_id>/`, NOT the data dir root.
    //
    // If delete_all_pairs fails (e.g., transient sharing violation on
    // Windows), we still need to run shutdown_skip_snapshot_save so the
    // writer thread joins cleanly + the lockfile releases. Capture the
    // delete error, run shutdown, then surface the delete error.
    let _ = &data_dir; // referenced by shutdown messages
    let delete_err = if report.rows_reembedded > 0 {
        match solo_storage::snapshot::delete_all_pairs(&snapshot_dir) {
            Ok(()) => {
                eprintln!(
                    "(deleted HNSW snapshot files at {}; next start runs \
                     rebuild_hnsw_from_sql over the `embeddings` table to \
                     repopulate the index)",
                    snapshot_dir.display()
                );
                None
            }
            Err(e) => Some(e),
        }
    } else {
        None
    };

    // Skip the snapshot save in shutdown — the in-memory HNSW still
    // holds vectors from the prior embedder, and writing it back would
    // re-create the snapshot we just deleted (or would have, if the
    // delete succeeded).
    ctx.shutdown_skip_snapshot_save()
        .await
        .context("shutdown after reembed")?;

    if let Some(e) = delete_err {
        bail!(
            "reembed wrote new vectors successfully, but deleting HNSW snapshot \
             pairs failed: {e}. Manually delete `hnsw_episodes.hnsw.{{data,graph}}` \
             and `hnsw_episodes_bak.hnsw.{{data,graph}}` from the data directory, \
             then start the daemon to rebuild from `embeddings`."
        );
    }
    Ok(())
}
