// SPDX-License-Identifier: Apache-2.0

//! `solo consolidate [--window-days N]` — one-shot consolidation pass.
//!
//! Triggers the SWS-equivalent clustering / merge pass. Idempotent —
//! re-running on the same data is a no-op for already-clustered
//! episodes unless `--force-merge` asks the merge path to revisit quiet
//! corpora.
//!
//! The daemon owns automatic derived-memory production. Its
//! triples-batch timer calls the Steward LLM later and writes
//! `semantic_abstractions`, `triples`, and contradictions when `[llm]`
//! and `[triples]` allow it. This one-shot can still use
//! `--ollama-model` (or hosted LLM env vars) for manual/local runs, but
//! operators should expect freshly clustered episodes to become triples
//! on the daemon batch cadence rather than immediately in every CLI
//! report.
//!
//! Useful right now for:
//!
//!   - Manual triggers when running `solo daemon` without
//!     `--consolidate-interval-secs` set.
//!   - Bulk-cluster after a large-batch import.
//!   - End-to-end smoke tests that need deterministic timing
//!     (don't want to wait on the daemon's interval).

use anyhow::{Context, Result};
use clap::Args;
use solo_storage::ConsolidationScope;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct ConsolidateArgs {
    /// Window (in days) for the consolidation pass. Only memories
    /// with `ts_ms >= now - window_days * 86_400_000` are considered.
    /// Default: unbounded (all active+hot, current-embedder, not-
    /// already-clustered memories).
    #[arg(long)]
    pub window_days: Option<i64>,

    /// Run the existing-vs-existing merge + abstraction-regen passes
    /// even when there are no new episodes to cluster. Useful for
    /// **drift catch-up** on a quiet corpus: pre-existing clusters
    /// can drift toward each other across runs (via repeated
    /// absorbs) and should occasionally coalesce regardless of
    /// fresh-memory volume. Default: off — empty-candidate runs
    /// short-circuit cheaply.
    #[arg(long)]
    pub force_merge: bool,

    /// Data directory (defaults to `$SOLO_DATA_DIR` or `~/.solo`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Use a local Ollama instance as the Steward LLM backend. Sets
    /// `OPENAI_API_KEY=ollama`, `OPENAI_BASE_URL=http://localhost:11434/v1`,
    /// and `OPENAI_MODEL=<MODEL>` for this consolidate run, without
    /// requiring the operator to set them manually. Override the base
    /// URL or API key by setting the corresponding env var explicitly
    /// (for non-default Ollama port, remote Ollama, or auth-proxy
    /// fronted Ollama). Takes precedence over `ANTHROPIC_API_KEY` —
    /// the explicit flag wins over Anthropic > OpenAI precedence.
    #[arg(long, value_name = "MODEL")]
    pub ollama_model: Option<String>,
}

pub async fn run(args: ConsolidateArgs) -> Result<()> {
    // `--ollama-model <MODEL>` shorthand: configure env vars BEFORE
    // `prepare_oneshot` builds the LLM client.
    if let Some(model) = args.ollama_model.as_deref() {
        let (model, base_url) = crate::commands::common::apply_ollama_overrides(model);
        tracing::info!(
            ollama_model = %model,
            ollama_base_url = %base_url,
            "Ollama backend configured via --ollama-model"
        );
    }

    let scope = ConsolidationScope {
        window_days: args.window_days,
        force_merge: args.force_merge,
    };

    let ctx = prepare_oneshot(args.data_dir).await?;

    let report = match ctx.write_handle().consolidate(scope).await {
        Ok(r) => r,
        Err(e) => {
            // Make sure shutdown still runs before bailing.
            ctx.shutdown().await.ok();
            anyhow::bail!("consolidate failed: {e}");
        }
    };

    println!(
        "consolidate complete: episodes_seen={} clusters_built={} \
         episodes_clustered={} abstractions_built={} triples_built={} \
         contradictions_found={}",
        report.episodes_seen,
        report.clusters_built,
        report.episodes_clustered,
        report.abstractions_built,
        report.triples_built,
        report.contradictions_found,
    );

    if report.abstractions_built == 0 && report.clusters_built > 0 {
        eprintln!(
            "(no abstractions produced in this one-shot. Clusters were persisted; \
             the daemon triples-batch timer writes abstractions/triples when a \
             real Steward LLM is configured and the batch cadence fires.)"
        );
    }

    ctx.shutdown().await.context("shutdown after consolidate")
}
