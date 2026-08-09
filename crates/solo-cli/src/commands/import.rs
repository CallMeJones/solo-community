// SPDX-License-Identifier: Apache-2.0

//! `solo import ...`.
//!
//! `solo import markdown|text|json <path> --dry-run [--json]` scans files by
//! extension. Schema-aware importers (`chatgpt`, `claude`, `bookmarks`)
//! parse exports into stable Markdown documents under `.solo/imports/`
//! before reusing the normal document ingest pipeline.
//!
//! Non-dry-run imports use the standard one-shot CLI context and route every
//! candidate through the existing writer document-ingest command.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use solo_storage::{
    ChunkConfig, DocumentConfig, IngestReport, SchemaImportScan, SchemaImportSource, SoloConfig,
    default_data_dir, estimate_schema_chunks, materialize_schema_record, parse_schema_import,
};
use std::path::{Path, PathBuf};

use crate::commands::common::{OneShotContext, prepare_oneshot};

const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown"];
const TEXT_EXTENSIONS: &[&str] = &["txt"];
const JSON_EXTENSIONS: &[&str] = &["json", "jsonl", "ndjson"];
const ESTIMATED_BYTES_PER_TOKEN: u64 = 4;

#[derive(Debug, Subcommand)]
pub enum ImportCommand {
    /// Import markdown files, or scan them first with --dry-run.
    Markdown(DocumentSourceImportArgs),
    /// Import plain text .txt files, or scan them first with --dry-run.
    Text(DocumentSourceImportArgs),
    /// Import JSON/JSONL files, or scan them first with --dry-run.
    Json(DocumentSourceImportArgs),
    /// Import ChatGPT conversations.json as document memory.
    Chatgpt(SchemaImportArgs),
    /// Import Claude conversation exports as document memory.
    Claude(SchemaImportArgs),
    /// Import browser bookmarks HTML/JSON as document memory.
    Bookmarks(BookmarksImportArgs),
}

#[derive(Debug, Args)]
pub struct DocumentSourceImportArgs {
    /// File or directory to scan.
    pub path: PathBuf,

    /// Report what would be imported without ingesting anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit structured dry-run JSON.
    #[arg(long)]
    pub json: bool,

    /// Override the data dir used to read solo.config.toml for document
    /// allowed_extensions and chunk sizing. Defaults to `~/.solo`, or
    /// falls back to built-in document defaults when no config exists.
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SchemaImportArgs {
    /// Export file or directory to import.
    pub path: PathBuf,

    /// Report what would be imported without writing materialized docs or
    /// opening the encrypted database.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit structured dry-run JSON.
    #[arg(long)]
    pub json: bool,

    /// Select conversation by id or title substring. Repeat to import several.
    #[arg(long = "conversation")]
    pub conversations: Vec<String>,

    /// Override the data dir used for materialized import files and config.
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct BookmarksImportArgs {
    /// Browser bookmarks export file. Supports Netscape HTML and JSON trees.
    pub path: PathBuf,

    /// Report what would be imported without writing materialized docs or
    /// opening the encrypted database.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit structured dry-run JSON.
    #[arg(long)]
    pub json: bool,

    /// Override the data dir used for materialized import files and config.
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum DocumentImportSource {
    Markdown,
    Text,
    Json,
}

impl DocumentImportSource {
    fn command_name(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Text => "text",
            Self::Json => "json",
        }
    }

    fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Markdown => MARKDOWN_EXTENSIONS,
            Self::Text => TEXT_EXTENSIONS,
            Self::Json => JSON_EXTENSIONS,
        }
    }

    fn enabled_extensions_label(self) -> &'static str {
        match self {
            Self::Markdown => "markdown extensions enabled by documents.allowed_extensions",
            Self::Text => "plain text extensions enabled by documents.allowed_extensions",
            Self::Json => "JSON extensions enabled by documents.allowed_extensions",
        }
    }

    fn no_matches_message(self) -> &'static str {
        match self {
            Self::Markdown => "no markdown files matched the enabled document extensions",
            Self::Text => "no plain text files matched the enabled document extensions",
            Self::Json => "no JSON files matched the enabled document extensions",
        }
    }
}

pub async fn run(cmd: ImportCommand) -> Result<()> {
    match cmd {
        ImportCommand::Markdown(args) => run_markdown(args).await,
        ImportCommand::Text(args) => run_text(args).await,
        ImportCommand::Json(args) => run_json(args).await,
        ImportCommand::Chatgpt(args) => run_schema_import(args, SchemaImportSource::ChatGpt).await,
        ImportCommand::Claude(args) => run_schema_import(args, SchemaImportSource::Claude).await,
        ImportCommand::Bookmarks(args) => run_bookmarks_import(args).await,
    }
}

async fn run_markdown(args: DocumentSourceImportArgs) -> Result<()> {
    run_document_source(args, DocumentImportSource::Markdown).await
}

async fn run_text(args: DocumentSourceImportArgs) -> Result<()> {
    run_document_source(args, DocumentImportSource::Text).await
}

async fn run_json(args: DocumentSourceImportArgs) -> Result<()> {
    run_document_source(args, DocumentImportSource::Json).await
}

async fn run_bookmarks_import(args: BookmarksImportArgs) -> Result<()> {
    let args = SchemaImportArgs {
        path: args.path,
        dry_run: args.dry_run,
        json: args.json,
        conversations: Vec::new(),
        data_dir: args.data_dir,
    };
    run_schema_import(args, SchemaImportSource::Bookmarks).await
}

async fn run_document_source(
    args: DocumentSourceImportArgs,
    source: DocumentImportSource,
) -> Result<()> {
    if args.dry_run {
        return run_document_source_dry_run(args, source);
    }

    run_document_source_import(args, source).await
}

async fn run_schema_import(args: SchemaImportArgs, source: SchemaImportSource) -> Result<()> {
    if args.json && !args.dry_run {
        bail!(
            "--json is only supported with --dry-run for import {}",
            source.command_name()
        );
    }
    let document_config = load_document_config(args.data_dir.clone())?;
    let scan = parse_schema_import(&args.path, source, &args.conversations).with_context(|| {
        format!(
            "parse {} export {}",
            source.command_name(),
            args.path.display()
        )
    })?;
    let estimated_chunk_candidates =
        estimate_schema_chunks(&scan.records, document_config.chunk_token_target);

    if args.dry_run {
        return print_schema_dry_run(
            &args.path,
            source,
            &scan,
            estimated_chunk_candidates,
            args.json,
        );
    }

    let ctx = prepare_oneshot(args.data_dir).await?;
    let chunk_config = match chunk_config_from_document_config(&ctx.config().documents) {
        Ok(chunk_config) => chunk_config,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e);
        }
    };

    let result = ingest_schema_records(&ctx, &args.path, source, &scan, chunk_config).await;
    let shutdown_result = ctx.shutdown().await;
    match (result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(e)) => {
            Err(e).context(format!("shutdown after {} import", source.command_name()))
        }
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(s)) => {
            tracing::warn!(
                shutdown_error = %s,
                source = source.command_name(),
                "schema import failed; shutdown also errored"
            );
            Err(e)
        }
    }
}

fn print_schema_dry_run(
    path: &Path,
    source: SchemaImportSource,
    scan: &SchemaImportScan,
    estimated_chunk_candidates: u64,
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": format!("import {}", source.command_name()),
                "path": path.display().to_string(),
                "source": source.display_name(),
                "dry_run": true,
                "records_scanned": scan.records_scanned,
                "candidate_records": scan.records.len(),
                "filtered_records": scan.filtered_records,
                "skipped_records": scan.skipped_records,
                "estimated_chunk_candidates": estimated_chunk_candidates,
                "materialized_format": "markdown",
            }))
            .context("serialize schema import dry-run JSON")?
        );
        return Ok(());
    }

    println!("import {} --dry-run", source.command_name());
    println!("path: {}", path.display());
    println!("source: {}", source.display_name());
    println!("records scanned: {}", scan.records_scanned);
    println!("candidate records: {}", scan.records.len());
    println!("filtered records: {}", scan.filtered_records);
    println!("skipped records: {}", scan.skipped_records);
    println!("estimated chunk candidates: {estimated_chunk_candidates}");
    println!("materialized format: markdown");

    if scan.records.is_empty() {
        println!("({})", source.no_records_message());
    }
    Ok(())
}

async fn ingest_schema_records(
    ctx: &OneShotContext,
    input_path: &Path,
    source: SchemaImportSource,
    scan: &SchemaImportScan,
    chunk_config: ChunkConfig,
) -> Result<()> {
    println!("import {}", source.command_name());
    println!("path: {}", input_path.display());
    println!("source: {}", source.display_name());
    println!("candidate records: {}", scan.records.len());
    println!("filtered records: {}", scan.filtered_records);
    println!("skipped records: {}", scan.skipped_records);

    if scan.records.is_empty() {
        println!("({})", source.no_records_message());
        return Ok(());
    }

    let materialized_dir = ctx.data_dir.join("imports").join(source.materialized_dir());
    std::fs::create_dir_all(&materialized_dir)
        .with_context(|| format!("create {}", materialized_dir.display()))?;

    let mut ingested = 0u32;
    let mut deduped = 0u32;
    let mut failed = 0u32;
    let mut total_chunks = 0u32;

    for record in &scan.records {
        let path = materialize_schema_record(&materialized_dir, record)?;
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
                eprintln!("failed {}: {e}", path.display());
                failed += 1;
            }
        }
    }

    println!(
        "\nSummary: imported {ingested} new, {deduped} deduped, \
         {failed} failed; {total_chunks} chunks persisted"
    );
    if failed > 0 {
        bail!(
            "{failed} {} record(s) failed to import from {}",
            source.command_name(),
            input_path.display()
        );
    }
    Ok(())
}

fn run_document_source_dry_run(
    args: DocumentSourceImportArgs,
    source: DocumentImportSource,
) -> Result<()> {
    let document_config = load_document_config(args.data_dir)?;
    let scan = scan_document_source_path(
        &args.path,
        &document_config.allowed_extensions,
        document_config.chunk_token_target,
        source,
    )
    .with_context(|| {
        format!(
            "scan {} import path {}",
            source.command_name(),
            args.path.display()
        )
    })?;

    print_document_source_dry_run(&args.path, source, &scan, args.json)?;

    Ok(())
}

async fn run_document_source_import(
    args: DocumentSourceImportArgs,
    source: DocumentImportSource,
) -> Result<()> {
    if args.json {
        bail!(
            "--json is only supported with --dry-run for import {}",
            source.command_name()
        );
    }
    let ctx = prepare_oneshot(args.data_dir).await?;
    let chunk_config = match chunk_config_from_document_config(&ctx.config().documents) {
        Ok(chunk_config) => chunk_config,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e);
        }
    };

    let scan = match scan_document_source_path(
        &args.path,
        &ctx.config().documents.allowed_extensions,
        ctx.config().documents.chunk_token_target,
        source,
    ) {
        Ok(scan) => scan,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e).with_context(|| {
                format!(
                    "scan {} import path {}",
                    source.command_name(),
                    args.path.display()
                )
            });
        }
    };

    let result =
        ingest_document_import_candidates(&ctx, &args.path, &scan, chunk_config, source).await;

    let shutdown_result = ctx.shutdown().await;
    match (result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(e)) => {
            Err(e).context(format!("shutdown after {} import", source.command_name()))
        }
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(s)) => {
            tracing::warn!(
                shutdown_error = %s,
                source = source.command_name(),
                "document import failed; shutdown also errored"
            );
            Err(e)
        }
    }
}

fn print_document_source_dry_run(
    path: &Path,
    source: DocumentImportSource,
    scan: &DocumentImportScan,
    json: bool,
) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": format!("import {}", source.command_name()),
                "path": path.display().to_string(),
                "source": source.command_name(),
                "dry_run": true,
                "files_scanned": scan.files_scanned,
                "candidate_files": scan.candidate_files,
                "skipped_files": scan.skipped_files,
                "skipped": {
                    "unsupported_extension": scan.skipped_unsupported,
                    "hidden_entries": scan.skipped_hidden,
                },
                "estimated_chunk_candidates": scan.estimated_chunk_candidates,
                "enabled_extensions": scan.enabled_extensions.clone(),
            }))
            .context("serialize document import dry-run JSON")?
        );
        return Ok(());
    }

    println!("import {} --dry-run", source.command_name());
    println!("path: {}", path.display());
    println!("files scanned: {}", scan.files_scanned);
    println!("candidate files: {}", scan.candidate_files);
    println!("skipped files: {}", scan.skipped_files);
    println!("  unsupported extension: {}", scan.skipped_unsupported);
    println!("  hidden entries: {}", scan.skipped_hidden);
    println!(
        "estimated chunk candidates: {}",
        scan.estimated_chunk_candidates
    );
    println!(
        "{}: {:?}",
        source.enabled_extensions_label(),
        scan.enabled_extensions
    );

    if scan.candidate_files == 0 {
        println!("({})", source.no_matches_message());
    }

    Ok(())
}

fn chunk_config_from_document_config(document_config: &DocumentConfig) -> Result<ChunkConfig> {
    let target_tokens = document_config.chunk_token_target;
    let overlap_tokens = document_config.chunk_overlap_tokens;
    if target_tokens == 0 {
        bail!("documents.chunk_token_target must be > 0");
    }
    if overlap_tokens >= target_tokens {
        bail!(
            "documents.chunk_overlap_tokens ({overlap_tokens}) must be strictly less \
             than documents.chunk_token_target ({target_tokens})"
        );
    }
    Ok(ChunkConfig {
        target_tokens,
        overlap_tokens,
    })
}

async fn ingest_document_import_candidates(
    ctx: &OneShotContext,
    root: &Path,
    scan: &DocumentImportScan,
    chunk_config: ChunkConfig,
    source: DocumentImportSource,
) -> Result<()> {
    println!("import {}", source.command_name());
    println!("path: {}", root.display());
    println!("candidate files: {}", scan.candidate_files);
    println!(
        "{}: {:?}",
        source.enabled_extensions_label(),
        scan.enabled_extensions
    );

    if scan.candidate_paths.is_empty() {
        println!("({})", source.no_matches_message());
        return Ok(());
    }

    let mut ingested = 0u32;
    let mut deduped = 0u32;
    let mut failed = 0u32;
    let mut total_chunks = 0u32;

    for path in &scan.candidate_paths {
        match ctx
            .write_handle()
            .ingest_document(path.clone(), chunk_config.clone())
            .await
        {
            Ok(report) => {
                print_report(path, &report);
                if report.deduped {
                    deduped += 1;
                } else {
                    ingested += 1;
                }
                total_chunks += report.chunks_persisted;
            }
            Err(e) => {
                eprintln!("failed {}: {e}", path.display());
                failed += 1;
            }
        }
    }

    println!(
        "\nSummary: imported {ingested} new, {deduped} deduped, \
         {failed} failed; {total_chunks} chunks persisted"
    );
    if failed > 0 {
        bail!("{failed} file(s) failed to import from {}", root.display());
    }
    Ok(())
}

fn print_report(path: &Path, report: &IngestReport) {
    let short = short_doc_id(&report.doc_id.to_string());
    if report.deduped {
        println!(
            "deduped {} -> {short} ({} bytes)",
            path.display(),
            report.bytes_ingested
        );
    } else {
        println!(
            "ingested {} -> {short} ({} chunks, {} bytes)",
            path.display(),
            report.chunks_persisted,
            report.bytes_ingested,
        );
    }
}

fn short_doc_id(full: &str) -> String {
    full.chars().take(8).collect()
}

fn load_document_config(data_dir_arg: Option<PathBuf>) -> Result<DocumentConfig> {
    let data_dir = match data_dir_arg {
        Some(path) => path,
        None => default_data_dir()
            .context("could not resolve default data dir; using document defaults failed")?,
    };
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        return Ok(DocumentConfig::default());
    }
    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    Ok(config.documents)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DocumentImportScan {
    files_scanned: u64,
    candidate_files: u64,
    skipped_files: u64,
    skipped_unsupported: u64,
    skipped_hidden: u64,
    estimated_chunk_candidates: u64,
    enabled_extensions: Vec<String>,
    candidate_paths: Vec<PathBuf>,
}

impl DocumentImportScan {
    fn new(enabled_extensions: Vec<String>) -> Self {
        Self {
            files_scanned: 0,
            candidate_files: 0,
            skipped_files: 0,
            skipped_unsupported: 0,
            skipped_hidden: 0,
            estimated_chunk_candidates: 0,
            enabled_extensions,
            candidate_paths: Vec::new(),
        }
    }
}

fn scan_document_source_path(
    root: &Path,
    allowed_extensions: &[String],
    chunk_token_target: u32,
    source: DocumentImportSource,
) -> Result<DocumentImportScan> {
    let enabled_extensions = enabled_source_extensions(allowed_extensions, source);
    let mut scan = DocumentImportScan::new(enabled_extensions);
    if root.is_file() {
        scan_file(root, &mut scan, chunk_token_target)?;
    } else if root.is_dir() {
        scan_dir(root, &mut scan, chunk_token_target)?;
    } else {
        bail!(
            "path is not a regular file or directory: {}",
            root.display()
        );
    }
    Ok(scan)
}

fn scan_dir(dir: &Path, scan: &mut DocumentImportScan, chunk_token_target: u32) -> Result<()> {
    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read_dir entries under {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        let path = entry.path();
        if is_hidden_entry(&path) {
            scan.skipped_hidden += 1;
            continue;
        }

        let file_type = entry
            .file_type()
            .with_context(|| format!("file_type for {}", path.display()))?;
        if file_type.is_dir() {
            scan_dir(&path, scan, chunk_token_target)?;
        } else if file_type.is_file() {
            scan_file(&path, scan, chunk_token_target)?;
        }
    }

    Ok(())
}

fn scan_file(path: &Path, scan: &mut DocumentImportScan, chunk_token_target: u32) -> Result<()> {
    scan.files_scanned += 1;
    if is_enabled_source_file(path, &scan.enabled_extensions) {
        let bytes = std::fs::metadata(path)
            .with_context(|| format!("metadata {}", path.display()))?
            .len();
        scan.candidate_files += 1;
        scan.estimated_chunk_candidates += estimate_chunks(bytes, chunk_token_target);
        scan.candidate_paths.push(path.to_path_buf());
    } else {
        scan.skipped_files += 1;
        scan.skipped_unsupported += 1;
    }
    Ok(())
}

fn enabled_source_extensions(
    allowed_extensions: &[String],
    source: DocumentImportSource,
) -> Vec<String> {
    let mut enabled = Vec::new();
    for ext in allowed_extensions {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        if source.extensions().contains(&ext.as_str()) && !enabled.contains(&ext) {
            enabled.push(ext);
        }
    }
    enabled.sort();
    enabled
}

fn is_enabled_source_file(path: &Path, enabled_extensions: &[String]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            enabled_extensions
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
}

fn estimate_chunks(bytes: u64, chunk_token_target: u32) -> u64 {
    if bytes == 0 {
        return 0;
    }
    let target_bytes = u64::from(chunk_token_target.max(1)) * ESTIMATED_BYTES_PER_TOKEN;
    bytes.div_ceil(target_bytes)
}

fn is_hidden_entry(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, bytes: usize) {
        std::fs::write(path, "x".repeat(bytes)).expect("write fixture");
    }

    fn write_text(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write fixture");
    }

    #[test]
    fn enabled_source_extensions_intersects_document_allow_list() {
        let allowed = vec![
            "txt".to_string(),
            "json".to_string(),
            "jsonl".to_string(),
            "ndjson".to_string(),
            ".MD".to_string(),
            "markdown".to_string(),
            "md".to_string(),
        ];
        assert_eq!(
            enabled_source_extensions(&allowed, DocumentImportSource::Markdown),
            vec!["markdown".to_string(), "md".to_string()]
        );
        assert_eq!(
            enabled_source_extensions(&allowed, DocumentImportSource::Text),
            vec!["txt".to_string()]
        );
        assert_eq!(
            enabled_source_extensions(&allowed, DocumentImportSource::Json),
            vec![
                "json".to_string(),
                "jsonl".to_string(),
                "ndjson".to_string()
            ]
        );
    }

    #[test]
    fn scan_markdown_path_counts_candidates_and_skips() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join("a.md"), 8);
        write(&root.join("b.markdown"), 8);
        write(&root.join("c.txt"), 8);

        let scan = scan_document_source_path(
            root,
            &["md".into(), "markdown".into()],
            1,
            DocumentImportSource::Markdown,
        )
        .unwrap();

        assert_eq!(scan.files_scanned, 3);
        assert_eq!(scan.candidate_files, 2);
        assert_eq!(scan.skipped_files, 1);
        assert_eq!(scan.skipped_unsupported, 1);
        assert_eq!(scan.estimated_chunk_candidates, 4);
        let names: Vec<_> = scan
            .candidate_paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.md".to_string(), "b.markdown".to_string()]);
    }

    #[test]
    fn scan_markdown_path_walks_directories_and_skips_hidden_entries() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::create_dir(root.join(".cache")).unwrap();
        write(&root.join("top.md"), 4);
        write(&root.join("sub").join("deep.md"), 4);
        write(&root.join(".hidden.md"), 4);
        write(&root.join(".cache").join("inside.md"), 4);

        let scan =
            scan_document_source_path(root, &["md".into()], 500, DocumentImportSource::Markdown)
                .unwrap();

        assert_eq!(scan.files_scanned, 2);
        assert_eq!(scan.candidate_files, 2);
        assert_eq!(scan.skipped_hidden, 2);
        assert_eq!(scan.skipped_files, 0);
    }

    #[test]
    fn scan_markdown_path_honors_allowed_extensions() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join("a.md"), 4);
        write(&root.join("b.markdown"), 4);

        let scan =
            scan_document_source_path(root, &["md".into()], 500, DocumentImportSource::Markdown)
                .unwrap();

        assert_eq!(scan.candidate_files, 1);
        assert_eq!(scan.skipped_unsupported, 1);
        assert_eq!(scan.enabled_extensions, vec!["md".to_string()]);
    }

    #[test]
    fn scan_text_path_counts_only_txt_candidates() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join("a.txt"), 8);
        write(&root.join("b.md"), 8);
        write(&root.join("c.json"), 8);

        let scan = scan_document_source_path(
            root,
            &["txt".into(), "md".into(), "json".into()],
            1,
            DocumentImportSource::Text,
        )
        .unwrap();

        assert_eq!(scan.files_scanned, 3);
        assert_eq!(scan.candidate_files, 1);
        assert_eq!(scan.skipped_files, 2);
        assert_eq!(scan.skipped_unsupported, 2);
        assert_eq!(scan.estimated_chunk_candidates, 2);
        assert_eq!(scan.enabled_extensions, vec!["txt".to_string()]);
        assert_eq!(
            scan.candidate_paths[0]
                .file_name()
                .unwrap()
                .to_string_lossy(),
            "a.txt"
        );
    }

    #[test]
    fn scan_text_path_honors_allowed_extensions() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join("a.txt"), 4);

        let scan = scan_document_source_path(root, &["md".into()], 500, DocumentImportSource::Text)
            .unwrap();

        assert_eq!(scan.files_scanned, 1);
        assert_eq!(scan.candidate_files, 0);
        assert_eq!(scan.skipped_unsupported, 1);
        assert_eq!(scan.enabled_extensions, Vec::<String>::new());
    }

    #[test]
    fn scan_json_path_counts_json_jsonl_and_ndjson_candidates() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let root = tmp.path();
        write(&root.join("a.json"), 8);
        write(&root.join("b.jsonl"), 8);
        write(&root.join("c.ndjson"), 8);
        write(&root.join("d.txt"), 8);

        let scan = scan_document_source_path(
            root,
            &["json".into(), "jsonl".into(), "ndjson".into(), "txt".into()],
            1,
            DocumentImportSource::Json,
        )
        .unwrap();

        assert_eq!(scan.files_scanned, 4);
        assert_eq!(scan.candidate_files, 3);
        assert_eq!(scan.skipped_files, 1);
        assert_eq!(scan.skipped_unsupported, 1);
        assert_eq!(
            scan.enabled_extensions,
            vec![
                "json".to_string(),
                "jsonl".to_string(),
                "ndjson".to_string()
            ]
        );
        let names: Vec<_> = scan
            .candidate_paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "a.json".to_string(),
                "b.jsonl".to_string(),
                "c.ndjson".to_string()
            ]
        );
    }

    #[test]
    fn parse_chatgpt_conversations_json_extracts_transcripts() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        let fixture = serde_json::json!([
            {
                "id": "conv-1",
                "title": "Release plan",
                "create_time": 1710000000,
                "mapping": {
                    "a": {
                        "message": {
                            "author": { "role": "user" },
                            "create_time": 1710000001,
                            "content": { "parts": ["What ships next?"] }
                        }
                    },
                    "b": {
                        "message": {
                            "author": { "role": "assistant" },
                            "create_time": 1710000002,
                            "content": { "parts": ["Schema-aware importers."] }
                        }
                    }
                }
            },
            {
                "id": "empty",
                "title": "Empty",
                "mapping": {}
            }
        ]);
        write_text(&export, &serde_json::to_string(&fixture).unwrap());

        let scan = parse_schema_import(&export, SchemaImportSource::ChatGpt, &[]).unwrap();

        assert_eq!(scan.records_scanned, 2);
        assert_eq!(scan.skipped_records, 1);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].source_id, "chatgpt:conv-1");
        assert!(scan.records[0].body.contains("## Transcript"));
        assert!(scan.records[0].body.contains("What ships next?"));
        assert!(scan.records[0].body.contains("Schema-aware importers."));
    }

    #[test]
    fn parse_chatgpt_conversations_accepts_utf8_bom() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        write_text(
            &export,
            "\u{feff}[{\"id\":\"conv-1\",\"title\":\"BOM\",\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}]",
        );

        let scan = parse_schema_import(&export, SchemaImportSource::ChatGpt, &[]).unwrap();

        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].title, "BOM");
    }

    #[test]
    fn parse_chatgpt_conversations_honors_title_filter() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        let fixture = serde_json::json!([
            {
                "id": "conv-1",
                "title": "Release plan",
                "messages": [{ "role": "user", "content": "hello" }]
            },
            {
                "id": "conv-2",
                "title": "Dinner",
                "messages": [{ "role": "user", "content": "pizza" }]
            }
        ]);
        write_text(&export, &serde_json::to_string(&fixture).unwrap());

        let scan = parse_schema_import(
            &export,
            SchemaImportSource::ChatGpt,
            &["release".to_string()],
        )
        .unwrap();

        assert_eq!(scan.records_scanned, 2);
        assert_eq!(scan.filtered_records, 1);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].title, "Release plan");
    }

    #[test]
    fn parse_claude_conversations_json_extracts_chat_messages() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("conversations.json");
        let fixture = serde_json::json!({
            "conversations": [
                {
                    "uuid": "claude-1",
                    "name": "Architecture",
                    "created_at": "2026-05-27T10:00:00Z",
                    "chat_messages": [
                        { "sender": "human", "text": "Use one product surface." },
                        { "sender": "assistant", "text": "Tray plus web UI." }
                    ]
                }
            ]
        });
        write_text(&export, &serde_json::to_string(&fixture).unwrap());

        let scan = parse_schema_import(&export, SchemaImportSource::Claude, &[]).unwrap();

        assert_eq!(scan.records_scanned, 1);
        assert_eq!(scan.records.len(), 1);
        assert_eq!(scan.records[0].source_id, "claude:claude-1");
        assert!(scan.records[0].body.contains("Use one product surface."));
        assert!(scan.records[0].body.contains("Tray plus web UI."));
    }

    #[test]
    fn parse_bookmarks_html_extracts_links_without_crawling() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let export = tmp.path().join("bookmarks.html");
        write_text(
            &export,
            r#"
            <!DOCTYPE NETSCAPE-Bookmark-file-1>
            <DL><p>
              <DT><A HREF="https://solo.dev/docs?token=abc&amp;view=1" ADD_DATE="1710000000">Solo Docs</A>
              <DT><A HREF="https://example.com">Example</A>
            </DL><p>
            "#,
        );

        let scan = parse_schema_import(&export, SchemaImportSource::Bookmarks, &[]).unwrap();

        assert_eq!(scan.records_scanned, 2);
        assert_eq!(scan.records.len(), 2);
        assert!(scan.records[0].body.contains("Source: Browser bookmarks"));
        assert!(scan.records[0].body.contains("Solo did not crawl the page"));
        assert!(
            scan.records[0]
                .body
                .contains("https://solo.dev/docs?token=abc&view=1")
        );
    }

    #[test]
    fn materialized_record_path_is_stable_and_markdown() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let record = solo_storage::ImportRecord {
            source_id: "chatgpt:abc".to_string(),
            title: "Release Plan!".to_string(),
            created_at: None,
            updated_at: None,
            body: "# Release Plan\n".to_string(),
        };

        let first = materialize_schema_record(tmp.path(), &record).unwrap();
        let second = materialize_schema_record(tmp.path(), &record).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("md"));
        assert_eq!(std::fs::read_to_string(first).unwrap(), "# Release Plan\n");
    }

    #[test]
    fn estimate_chunks_is_byte_based_and_allows_empty_files() {
        assert_eq!(estimate_chunks(0, 500), 0);
        assert_eq!(estimate_chunks(1, 1), 1);
        assert_eq!(estimate_chunks(5, 1), 2);
    }

    #[test]
    fn chunk_config_from_document_config_validates_progress() {
        let mut config = DocumentConfig {
            chunk_token_target: 0,
            ..DocumentConfig::default()
        };
        assert!(
            chunk_config_from_document_config(&config)
                .unwrap_err()
                .to_string()
                .contains("> 0")
        );

        config.chunk_token_target = 10;
        config.chunk_overlap_tokens = 10;
        assert!(
            chunk_config_from_document_config(&config)
                .unwrap_err()
                .to_string()
                .contains("strictly less")
        );

        config.chunk_overlap_tokens = 2;
        let chunk_config = chunk_config_from_document_config(&config).unwrap();
        assert_eq!(chunk_config.target_tokens, 10);
        assert_eq!(chunk_config.overlap_tokens, 2);
    }
}
