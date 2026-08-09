// SPDX-License-Identifier: Apache-2.0

//! `solo normalize-subjects --alias FROM=TO [...] [--dry-run]` — backfill
//! tool: rewrite historical `triples.subject_id` / `triples.object_id`
//! values per a caller-supplied alias map.
//!
//! Companion to v0.5.0 P1's read-path alias resolution
//! (`IdentityConfig.user_aliases`). The read path already bridges named
//! entities to the canonical id (`alex` → `user`) transparently at query
//! time. This subcommand is **opt-in**: it rewrites the underlying rows
//! so downstream consumers (third-party tools, JSON exports) see the
//! canonical identity directly.
//!
//! Each `--alias FROM=TO` pair applies to **both** the subject and object
//! columns of `triples`. A triple's subject or object that matches the
//! `FROM` side is rewritten to `TO`. Multiple `--alias` flags can be
//! passed in one invocation; they're applied within a single transaction.
//!
//! ## Why opt-in
//!
//! Read-path aliasing already gives users the query behaviour they want
//! without touching stored data. Rewriting historical rows is **not
//! reversible** — there's no `--undo`. Users who specifically need the
//! data to match the canonical identity (export pipelines, analytics
//! against the raw `triples` table, switching to a system that won't
//! honour `IdentityConfig`) run this once after consolidating identities.
//!
//! ## Why `--dry-run` exists
//!
//! Because the rewrite is irreversible, the tool defaults to running
//! the UPDATEs in a transaction that's rolled back at the end. The
//! report's `subject_rows_updated` / `object_rows_updated` counts are
//! the same in dry-run mode as they would be in a real run, so the
//! operator can preview the impact before committing.

use anyhow::{Context, Result, bail};
use clap::Args;
use std::path::PathBuf;

use crate::commands::common::prepare_oneshot;

#[derive(Debug, Args)]
pub struct NormalizeSubjectsArgs {
    /// Alias to apply, in `FROM=TO` form. Repeatable. Each pair
    /// rewrites both `subject_id` and `object_id` columns of `triples`
    /// from `FROM` to `TO`. Example:
    /// `--alias alex=user --alias bob=user`.
    #[arg(long = "alias", value_name = "FROM=TO", required = true)]
    pub aliases: Vec<String>,

    /// Preview mode: run the UPDATEs inside a transaction, then
    /// `ROLLBACK`. The printed row counts match what a real run would
    /// do — useful for confirming the impact before committing. The
    /// rewrite is irreversible, so dry-run before commit is recommended.
    #[arg(long)]
    pub dry_run: bool,

    /// Data directory (defaults to `$SOLO_DATA_DIR` or `~/.solo`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

/// Parse a single `FROM=TO` token into a `(from, to)` pair. Rejects
/// malformed input cleanly so the user gets a useful error rather than
/// an opaque "alias not found" surprise.
fn parse_alias(raw: &str) -> Result<(String, String)> {
    let (from, to) = raw
        .split_once('=')
        .with_context(|| format!("alias must be in FROM=TO form (got `{raw}`)"))?;
    let from = from.trim();
    let to = to.trim();
    if from.is_empty() {
        bail!("alias FROM side is empty (got `{raw}`)");
    }
    if to.is_empty() {
        bail!("alias TO side is empty (got `{raw}`)");
    }
    Ok((from.to_string(), to.to_string()))
}

pub async fn run(args: NormalizeSubjectsArgs) -> Result<()> {
    // Parse all aliases up front. Reject the whole invocation if any
    // pair is malformed — partial success ("we applied 2 of 3") is
    // surprising for an opt-in backfill.
    let mut aliases: Vec<(String, String)> = Vec::with_capacity(args.aliases.len());
    for raw in &args.aliases {
        aliases.push(parse_alias(raw)?);
    }

    let ctx = prepare_oneshot(args.data_dir).await?;

    let report = match ctx
        .write_handle()
        .normalize_subjects(aliases, args.dry_run)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            // Make sure shutdown still runs before bailing.
            ctx.shutdown().await.ok();
            bail!("normalize-subjects failed: {e}");
        }
    };

    if report.dry_run {
        println!(
            "normalize-subjects --dry-run: would update {} subject row{} and \
             {} object row{} across {} alias{} (changes NOT persisted)",
            report.subject_rows_updated,
            if report.subject_rows_updated == 1 {
                ""
            } else {
                "s"
            },
            report.object_rows_updated,
            if report.object_rows_updated == 1 {
                ""
            } else {
                "s"
            },
            report.aliases_processed,
            if report.aliases_processed == 1 {
                ""
            } else {
                "es"
            },
        );
    } else {
        println!(
            "normalize-subjects complete: updated {} subject row{} and \
             {} object row{} across {} alias{}",
            report.subject_rows_updated,
            if report.subject_rows_updated == 1 {
                ""
            } else {
                "s"
            },
            report.object_rows_updated,
            if report.object_rows_updated == 1 {
                ""
            } else {
                "s"
            },
            report.aliases_processed,
            if report.aliases_processed == 1 {
                ""
            } else {
                "es"
            },
        );
    }

    ctx.shutdown()
        .await
        .context("shutdown after normalize-subjects")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_alias_accepts_simple_pair() {
        let (from, to) = parse_alias("alex=user").unwrap();
        assert_eq!(from, "alex");
        assert_eq!(to, "user");
    }

    #[test]
    fn parse_alias_trims_whitespace() {
        let (from, to) = parse_alias(" alex = user ").unwrap();
        assert_eq!(from, "alex");
        assert_eq!(to, "user");
    }

    #[test]
    fn parse_alias_accepts_to_side_containing_extra_equals() {
        // `split_once` splits on the FIRST '=' — so `key=value=foo`
        // parses as from="key", to="value=foo". This is harmless and
        // actually useful if someone aliases to a literal that contains
        // an equals sign.
        let (from, to) = parse_alias("key=value=foo").unwrap();
        assert_eq!(from, "key");
        assert_eq!(to, "value=foo");
    }

    #[test]
    fn parse_alias_rejects_missing_equals() {
        let err = parse_alias("alex").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("FROM=TO"),
            "error must point at the form: {msg}"
        );
    }

    #[test]
    fn parse_alias_rejects_empty_from() {
        let err = parse_alias("=user").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_ascii_lowercase().contains("from side is empty"),
            "error must name the missing side: {msg}"
        );
    }

    #[test]
    fn parse_alias_rejects_empty_to() {
        let err = parse_alias("alex=").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_ascii_lowercase().contains("to side is empty"),
            "error must name the missing side: {msg}"
        );
    }

    #[test]
    fn parse_alias_rejects_whitespace_only_sides() {
        // Trim collapses to empty → rejected.
        assert!(parse_alias("   =user").is_err());
        assert!(parse_alias("alex=   ").is_err());
    }
}
