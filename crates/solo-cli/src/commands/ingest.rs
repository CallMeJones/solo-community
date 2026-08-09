// SPDX-License-Identifier: Apache-2.0

//! `solo ingest <path>` and `solo ingest --dir <path>` — one-shot document
//! ingest.
//!
//! Wraps `WriteHandle::ingest_document` (v0.7.0 P3). Same one-shot
//! choreography as `solo remember`: resolve data dir → acquire `solo.lock`
//! → run the startup chain → spawn the writer → dispatch one or more
//! `IngestDocument` commands → print per-file `IngestReport` lines →
//! shutdown.
//!
//! ## Single file vs directory
//!
//! `solo ingest <path>` ingests one file. `solo ingest --dir <path>`
//! walks the directory recursively and ingests every file whose
//! extension is in `[documents].allowed_extensions` (configured in
//! `solo.config.toml`; defaults to a curated list of text/markup
//! formats). Files with non-allowlisted extensions are skipped
//! silently — same shape as the MCP / HTTP allow-list behaviour, so
//! agents and CLI users see the same surface.
//!
//! ## Chunk config
//!
//! Defaults come from `SoloConfig.documents` (which `solo init` writes
//! with sensible defaults of 500 target tokens / 50 overlap). The CLI
//! flags `--chunk-target-tokens` and `--chunk-overlap-tokens` are
//! per-invocation overrides — they don't persist to the config file.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_storage::ChunkConfig;
use std::path::{Path, PathBuf};

use crate::commands::common::{OneShotContext, prepare_oneshot};

#[derive(Debug, Args)]
pub struct IngestArgs {
    /// Path to a single file to ingest. Mutually exclusive with `--dir`.
    /// One of `<path>` or `--dir` is required.
    #[arg(group = "target", required_unless_present = "dir")]
    pub path: Option<PathBuf>,

    /// Recursively ingest every file under this directory whose
    /// extension is in `[documents].allowed_extensions`. Other files
    /// are skipped silently.
    #[arg(long, group = "target")]
    pub dir: Option<PathBuf>,

    /// Per-invocation override for the chunk-token target. Defaults to
    /// `[documents].chunk_token_target` from `solo.config.toml`.
    #[arg(long)]
    pub chunk_target_tokens: Option<u32>,

    /// Per-invocation override for the inter-chunk overlap in tokens.
    /// Defaults to `[documents].chunk_overlap_tokens` from
    /// `solo.config.toml`. Must be strictly less than the target.
    #[arg(long)]
    pub chunk_overlap_tokens: Option<u32>,

    /// Override the data dir (default: `~/.solo`, override with
    /// `SOLO_DATA_DIR`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

/// Resolve the effective ChunkConfig from the persisted config plus
/// per-invocation overrides. Validates `overlap < target` so the
/// chunker doesn't infinite-loop / produce zero-length chunks.
fn resolve_chunk_config(
    config: &solo_storage::SoloConfig,
    target_override: Option<u32>,
    overlap_override: Option<u32>,
) -> Result<ChunkConfig> {
    let target_tokens = target_override.unwrap_or(config.documents.chunk_token_target);
    let overlap_tokens = overlap_override.unwrap_or(config.documents.chunk_overlap_tokens);
    if target_tokens == 0 {
        bail!("--chunk-target-tokens must be > 0");
    }
    if overlap_tokens >= target_tokens {
        bail!(
            "--chunk-overlap-tokens ({overlap_tokens}) must be strictly less \
             than --chunk-target-tokens ({target_tokens})"
        );
    }
    Ok(ChunkConfig {
        target_tokens,
        overlap_tokens,
    })
}

/// Recursively gather every regular file under `root` whose extension
/// (case-insensitive) is in `allowed`. Symlinks are followed for the
/// `read_dir` step but not into recursion (cycle safety). Hidden / dot
/// entries are skipped.
fn collect_files(root: &Path, allowed: &[String]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(root, allowed, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(dir: &Path, allowed: &[String], out: &mut Vec<PathBuf>) -> Result<()> {
    let read = std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in read {
        let entry = entry.with_context(|| format!("read_dir entry under {}", dir.display()))?;
        let path = entry.path();
        // Skip hidden files / dirs (Unix .dotfiles convention). Matches
        // the spirit of the MCP allow-list — implicit "don't slurp ~/.config".
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                continue;
            }
        }
        let file_type = entry
            .file_type()
            .with_context(|| format!("file_type for {}", path.display()))?;
        if file_type.is_dir() {
            collect_files_inner(&path, allowed, out)?;
        } else if file_type.is_file()
            && let Some(ext) = path.extension().and_then(|e| e.to_str())
        {
            let ext_lc = ext.to_ascii_lowercase();
            if allowed.iter().any(|a| a.eq_ignore_ascii_case(&ext_lc)) {
                out.push(path);
            }
        }
    }
    Ok(())
}

pub async fn run(args: IngestArgs) -> Result<()> {
    // Resolve target (single file or directory) up front so we can fail
    // the obvious mistakes before paying the Argon2id cost of
    // `prepare_oneshot`. clap's `group = "target"` already enforces
    // exactly one of `path` / `--dir` is set, so this match is
    // exhaustive.
    let target: IngestTarget = match (args.path, args.dir) {
        (Some(p), None) => IngestTarget::SingleFile(p),
        (None, Some(d)) => IngestTarget::Directory(d),
        // clap's `required_unless_present` + `group` rule out the other
        // two cases at parse time. If we ever reach here it's a clap
        // misconfiguration, not a user error.
        _ => unreachable!("clap group rule allows exactly one of <path> / --dir"),
    };

    let ctx = prepare_oneshot(args.data_dir).await?;
    let chunk_config = match resolve_chunk_config(
        ctx.config(),
        args.chunk_target_tokens,
        args.chunk_overlap_tokens,
    ) {
        Ok(cc) => cc,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e);
        }
    };

    let result = match target {
        IngestTarget::SingleFile(path) => run_single(&ctx, path, chunk_config).await,
        IngestTarget::Directory(dir) => run_directory(&ctx, dir, chunk_config).await,
    };

    // Always shutdown — even on error — so the writer thread flushes
    // and the lockfile releases. The shutdown result is downgraded to
    // a warning so the original error wins.
    let shutdown_result = ctx.shutdown().await;
    match (result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(e)) => Err(e).context("shutdown after ingest"),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(s)) => {
            tracing::warn!(shutdown_error = %s, "ingest failed; shutdown also errored");
            Err(e)
        }
    }
}

enum IngestTarget {
    SingleFile(PathBuf),
    Directory(PathBuf),
}

async fn run_single(ctx: &OneShotContext, path: PathBuf, chunk_config: ChunkConfig) -> Result<()> {
    let report = ctx
        .write_handle()
        .ingest_document(path.clone(), chunk_config)
        .await
        .with_context(|| format!("ingest_document {}", path.display()))?;
    print_report(&path, &report);
    Ok(())
}

async fn run_directory(
    ctx: &OneShotContext,
    dir: PathBuf,
    chunk_config: ChunkConfig,
) -> Result<()> {
    if !dir.is_dir() {
        bail!("--dir argument is not a directory: {}", dir.display());
    }
    let allowed = &ctx.config().documents.allowed_extensions;
    let files = collect_files(&dir, allowed).with_context(|| format!("walk {}", dir.display()))?;
    if files.is_empty() {
        println!(
            "(no files under {} matched allowed_extensions={:?})",
            dir.display(),
            allowed
        );
        return Ok(());
    }

    let mut ingested = 0u32;
    let mut deduped = 0u32;
    let mut failed = 0u32;
    let mut total_chunks = 0u32;

    for path in files {
        match ctx
            .write_handle()
            .ingest_document(path.clone(), chunk_config.clone())
            .await
        {
            Ok(report) => {
                print_report(&path, &report);
                if report.deduped {
                    deduped += 1;
                } else {
                    ingested += 1;
                }
                total_chunks += report.chunks_persisted;
            }
            Err(e) => {
                eprintln!("✗ {}: {e}", path.display());
                failed += 1;
            }
        }
    }

    println!(
        "\nSummary: ingested {ingested} new, {deduped} deduped, \
         {failed} failed; {total_chunks} chunks persisted"
    );
    if failed > 0 {
        bail!("{failed} file(s) failed to ingest under {}", dir.display());
    }
    Ok(())
}

fn print_report(path: &Path, report: &solo_storage::IngestReport) {
    let short = short_doc_id(&report.doc_id.to_string());
    if report.deduped {
        println!(
            "↻ deduped {} → {short} ({} bytes)",
            path.display(),
            report.bytes_ingested
        );
    } else {
        println!(
            "✓ ingested {} → {short} ({} chunks, {} bytes)",
            path.display(),
            report.chunks_persisted,
            report.bytes_ingested,
        );
    }
}

/// First 8 hex chars of a doc_id for pretty-print. UUIDv7 puts the
/// time-ordered prefix at the front, so an 8-char prefix is unique-
/// enough for a session's worth of ingests (collisions are statistically
/// implausible in a single user's data dir).
fn short_doc_id(full: &str) -> String {
    full.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::{DocumentConfig, EmbedderConfig, IdentityConfig, SoloConfig};

    fn fake_config(target: u32, overlap: u32, exts: Vec<&str>) -> SoloConfig {
        SoloConfig {
            schema_version: 1,
            salt_hex: "00000000000000000000000000000000".to_string(),
            embedder: EmbedderConfig {
                name: "stub".to_string(),
                version: "v1".to_string(),
                dim: 32,
                dtype: "f32".to_string(),
            },
            identity: IdentityConfig::default(),
            documents: DocumentConfig {
                chunk_token_target: target,
                chunk_overlap_tokens: overlap,
                store_original_files_by_default: true,
                allowed_extensions: exts.into_iter().map(String::from).collect(),
            },
            workspace_file_access: solo_storage::WorkspaceFileAccessConfig::default(),
            auth: None,
            audit: solo_storage::AuditSettings::default(),
            redaction: solo_storage::RedactionConfig::default(),
            llm: None,
            triples: solo_storage::TriplesConfig::default(),
            sampling: solo_storage::SamplingConfig::default(),
            steward: solo_storage::StewardSettings::default(),
        }
    }

    #[test]
    fn resolve_chunk_config_uses_config_defaults_when_no_overrides() {
        let cfg = fake_config(500, 50, vec!["md"]);
        let cc = resolve_chunk_config(&cfg, None, None).unwrap();
        assert_eq!(cc.target_tokens, 500);
        assert_eq!(cc.overlap_tokens, 50);
    }

    #[test]
    fn resolve_chunk_config_target_override_replaces_config() {
        let cfg = fake_config(500, 50, vec!["md"]);
        let cc = resolve_chunk_config(&cfg, Some(800), None).unwrap();
        assert_eq!(cc.target_tokens, 800);
        // Overlap still comes from config since not overridden.
        assert_eq!(cc.overlap_tokens, 50);
    }

    #[test]
    fn resolve_chunk_config_overlap_override_replaces_config() {
        let cfg = fake_config(500, 50, vec!["md"]);
        let cc = resolve_chunk_config(&cfg, None, Some(20)).unwrap();
        assert_eq!(cc.target_tokens, 500);
        assert_eq!(cc.overlap_tokens, 20);
    }

    #[test]
    fn resolve_chunk_config_rejects_zero_target() {
        let cfg = fake_config(500, 50, vec!["md"]);
        let err = resolve_chunk_config(&cfg, Some(0), None).unwrap_err();
        assert!(format!("{err}").contains("> 0"));
    }

    #[test]
    fn resolve_chunk_config_rejects_overlap_ge_target() {
        let cfg = fake_config(500, 50, vec!["md"]);
        // Equal overlap+target is invalid: would never advance.
        let err = resolve_chunk_config(&cfg, Some(100), Some(100)).unwrap_err();
        assert!(format!("{err}").contains("strictly less than"));
        // Overlap > target is also invalid.
        let err = resolve_chunk_config(&cfg, Some(100), Some(200)).unwrap_err();
        assert!(format!("{err}").contains("strictly less than"));
    }

    #[test]
    fn collect_files_filters_by_allowed_extensions() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("a.md"), "x").unwrap();
        std::fs::write(root.join("b.txt"), "x").unwrap();
        std::fs::write(root.join("c.bin"), "x").unwrap();
        let allowed = vec!["md".to_string(), "txt".to_string()];
        let files = collect_files(root, &allowed).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.md".to_string(), "b.txt".to_string()]);
    }

    #[test]
    fn collect_files_walks_recursively() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("top.md"), "x").unwrap();
        std::fs::write(root.join("sub/deep.md"), "x").unwrap();
        std::fs::write(root.join("sub/skip.bin"), "x").unwrap();
        let allowed = vec!["md".to_string()];
        let files = collect_files(root, &allowed).unwrap();
        assert_eq!(files.len(), 2, "got {files:?}");
        // Sort-stable: deep before top because path-string sort puts
        // "sub/deep.md" before "top.md".
        assert!(files[0].ends_with("sub/deep.md") || files[0].ends_with("sub\\deep.md"));
        assert!(files[1].ends_with("top.md"));
    }

    #[test]
    fn collect_files_skips_dotfiles_and_dotdirs() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join(".hidden.md"), "x").unwrap();
        std::fs::create_dir(root.join(".cache")).unwrap();
        std::fs::write(root.join(".cache/inside.md"), "x").unwrap();
        std::fs::write(root.join("visible.md"), "x").unwrap();
        let allowed = vec!["md".to_string()];
        let files = collect_files(root, &allowed).unwrap();
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("visible.md"));
    }

    #[test]
    fn collect_files_extension_match_is_case_insensitive() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        std::fs::write(root.join("upper.MD"), "x").unwrap();
        std::fs::write(root.join("lower.md"), "x").unwrap();
        let allowed = vec!["md".to_string()];
        let files = collect_files(root, &allowed).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn short_doc_id_returns_first_eight_chars() {
        assert_eq!(
            short_doc_id("01234567-89ab-cdef-0123-456789abcdef"),
            "01234567"
        );
    }
}
