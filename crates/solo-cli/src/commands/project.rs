// SPDX-License-Identifier: Apache-2.0

//! `solo project ...` - minimal codebase memory mode.
//!
//! This is intentionally small: project identity, repo-doc ingestion, facts
//! lookup, and decision memories. The deeper project graph can build on these
//! stable source tags later.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use solo_core::{
    Confidence, Episode, MemoryId, ProjectMemoryDescriptor,
    ProjectPolicyClient as CoreProjectPolicyClient, Tier, project_decision_content,
    project_decision_encoding_context, project_decision_scope_matches, project_decision_source_id,
    render_project_policy,
};
use solo_query::{RecallHit, facts_about, run_recall};
use solo_storage::{ChunkConfig, DocumentConfig, IngestReport, ReaderPool};
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

use crate::commands::common::{OneShotContext, prepare_oneshot};

const PROJECT_CONFIG_REL: &str = ".solo/project.toml";
const PROJECT_DOC_EXTENSIONS: &[&str] = &["md", "markdown", "txt"];
const DOC_DIR_NAMES: &[&str] = &["docs", "doc", "adr", "adrs", "rfcs"];
const ROOT_DOC_STEMS: &[&str] = &[
    "readme",
    "changelog",
    "contributing",
    "architecture",
    "design",
    "decisions",
];
const DEFAULT_IGNORE_DIRS: &[&str] = &[
    ".git",
    ".solo",
    ".next",
    ".turbo",
    ".cache",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "target",
    "vendor",
];
const PROJECT_DECISION_SOURCE_TYPE: &str = "project_decision";

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    /// Create `.solo/project.toml` for the current repo.
    Init(ProjectInitArgs),
    /// Ingest README/docs/ADR files for this project.
    Ingest(ProjectIngestArgs),
    /// Query extracted facts about this project.
    Facts(ProjectFactsArgs),
    /// Store or recall durable project decisions.
    Decisions(ProjectDecisionsArgs),
    /// Print a repo-aware memory policy snippet for coding agents.
    Policy(ProjectPolicyArgs),
}

#[derive(Debug, Args)]
pub struct ProjectInitArgs {
    /// Project/repo root. Defaults to the current directory.
    pub path: Option<PathBuf>,

    /// Human-readable project name. Defaults to the directory name.
    #[arg(long)]
    pub name: Option<String>,

    /// Stable project id. Defaults to a slug of the name.
    #[arg(long)]
    pub id: Option<String>,

    /// Project tag. Repeat to add more tags.
    #[arg(long = "tag")]
    pub tags: Vec<String>,

    /// Overwrite an existing `.solo/project.toml`.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct ProjectIngestArgs {
    /// Project/repo root. Defaults to the current directory.
    pub path: Option<PathBuf>,

    /// Report candidate files without opening the encrypted database.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit structured dry-run JSON and exit without opening the encrypted database.
    #[arg(long)]
    pub json: bool,

    /// Maximum candidate files to ingest.
    #[arg(long, default_value_t = 500)]
    pub max_files: usize,

    /// Override the data dir (default: `~/.solo`, override with
    /// `SOLO_DATA_DIR`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ProjectFactsArgs {
    /// Project/repo root. Defaults to the current directory.
    pub path: Option<PathBuf>,

    /// Subject to query. Defaults to the project name.
    #[arg(long)]
    pub subject: Option<String>,

    /// Number of facts to return.
    #[arg(long, default_value_t = 10)]
    pub limit: usize,

    /// Emit structured JSON.
    #[arg(long)]
    pub json: bool,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("action")
        .required(true)
        .multiple(false)
        .args(["add", "query"])
))]
pub struct ProjectDecisionsArgs {
    /// Project/repo root. Defaults to the current directory.
    pub path: Option<PathBuf>,

    /// Store a durable decision for this project.
    #[arg(long)]
    pub add: Option<String>,

    /// Recall durable decisions for this project.
    #[arg(long)]
    pub query: Option<String>,

    /// Number of recall results to return.
    #[arg(long, default_value_t = 5)]
    pub limit: usize,

    /// Emit structured JSON.
    #[arg(long)]
    pub json: bool,

    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ProjectPolicyArgs {
    /// Project/repo root. Defaults to the current directory.
    pub path: Option<PathBuf>,

    /// Target coding client.
    #[arg(long, value_enum, default_value = "generic")]
    pub client: ProjectPolicyClient,

    /// Emit structured JSON containing the policy text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ProjectPolicyClient {
    Generic,
    Codex,
    Claude,
    Cursor,
}

impl From<ProjectPolicyClient> for CoreProjectPolicyClient {
    fn from(client: ProjectPolicyClient) -> Self {
        match client {
            ProjectPolicyClient::Generic => Self::Generic,
            ProjectPolicyClient::Codex => Self::Codex,
            ProjectPolicyClient::Claude => Self::Claude,
            ProjectPolicyClient::Cursor => Self::Cursor,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectConfig {
    name: String,
    project_id: String,
    tags: Vec<String>,
    ignore_dirs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectDocScan {
    root: PathBuf,
    files_scanned: u64,
    candidate_files: u64,
    skipped_files: u64,
    skipped_ignored_dirs: u64,
    truncated: bool,
    candidate_paths: Vec<PathBuf>,
}

impl ProjectDocScan {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            files_scanned: 0,
            candidate_files: 0,
            skipped_files: 0,
            skipped_ignored_dirs: 0,
            truncated: false,
            candidate_paths: Vec::new(),
        }
    }
}

pub async fn run(cmd: ProjectCommand) -> Result<()> {
    match cmd {
        ProjectCommand::Init(args) => run_init(args),
        ProjectCommand::Ingest(args) => run_ingest(args).await,
        ProjectCommand::Facts(args) => run_facts(args).await,
        ProjectCommand::Decisions(args) => run_decisions(args).await,
        ProjectCommand::Policy(args) => run_policy(args),
    }
}

fn run_init(args: ProjectInitArgs) -> Result<()> {
    let root = resolve_project_root(args.path.as_deref())?;
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_project_name(&root));
    let project_id = args
        .id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| slugify_project_id(&name));
    let config = ProjectConfig {
        name,
        project_id,
        tags: normalize_tags(args.tags),
        ignore_dirs: DEFAULT_IGNORE_DIRS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    };

    let config_path = project_config_path(&root);
    if config_path.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to overwrite",
            display_path(&config_path)
        );
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&config_path, render_project_config(&config))
        .with_context(|| format!("write {}", config_path.display()))?;

    println!("created {}", display_path(&config_path));
    println!("project: {} ({})", config.name, config.project_id);
    Ok(())
}

async fn run_ingest(args: ProjectIngestArgs) -> Result<()> {
    let root = resolve_project_root(args.path.as_deref())?;
    let config = load_project_config_or_default(&root)?;
    let scan = scan_project_docs(&root, &config, args.max_files)
        .with_context(|| format!("scan project docs under {}", root.display()))?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&project_ingest_dry_run_json(&root, &config, &scan))
                .context("serialize project ingest dry-run JSON")?
        );
        return Ok(());
    }

    println!(
        "project ingest{}",
        if args.dry_run { " --dry-run" } else { "" }
    );
    println!("project: {} ({})", config.name, config.project_id);
    println!("root: {}", display_path(&root));
    println!("files scanned: {}", scan.files_scanned);
    println!("candidate files: {}", scan.candidate_files);
    println!("skipped files: {}", scan.skipped_files);
    println!("ignored dirs skipped: {}", scan.skipped_ignored_dirs);
    if scan.truncated {
        println!("truncated: true (increase --max-files to include more)");
    }

    if scan.candidate_paths.is_empty() {
        println!("(no project docs matched README/docs/ADR defaults)");
        return Ok(());
    }

    if args.dry_run {
        for path in &scan.candidate_paths {
            println!("candidate: {}", display_relative(&root, path));
        }
        return Ok(());
    }

    let ctx = prepare_oneshot(args.data_dir).await?;
    let chunk_config = match chunk_config_from_document_config(&ctx.config().documents) {
        Ok(config) => config,
        Err(e) => {
            ctx.shutdown().await.ok();
            return Err(e);
        }
    };

    let result = ingest_project_docs(&ctx, &root, &scan, chunk_config).await;
    let shutdown_result = ctx.shutdown().await;
    match (result, shutdown_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(e)) => Err(e).context("shutdown after project ingest"),
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(shutdown)) => {
            tracing::warn!(
                shutdown_error = %shutdown,
                "project ingest failed; shutdown also errored"
            );
            Err(e)
        }
    }
}

async fn run_facts(args: ProjectFactsArgs) -> Result<()> {
    let root = resolve_project_root(args.path.as_deref())?;
    let config = load_project_config_or_default(&root)?;
    let subject = args
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&config.name);
    let ctx = prepare_oneshot(args.data_dir).await?;
    let hits = facts_about(
        ctx.read_pool(),
        ctx.library_handle.audit(),
        None,
        subject,
        &ctx.config().identity.user_aliases,
        true,
        None,
        None,
        None,
        args.limit,
    )
    .await
    .context("project facts")?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": "project facts",
                "project": project_json(&root, &config),
                "subject": subject,
                "facts": hits,
            }))
            .context("serialize project facts JSON")?
        );
        return ctx.shutdown().await.context("shutdown after project facts");
    }

    if hits.is_empty() {
        println!("(no facts for {subject:?})");
    } else {
        for fact in &hits {
            println!(
                "{} --{}--> {}  conf={:.2}",
                fact.subject_id, fact.predicate, fact.object_id, fact.confidence
            );
        }
    }

    ctx.shutdown().await.context("shutdown after project facts")
}

async fn run_decisions(args: ProjectDecisionsArgs) -> Result<()> {
    let root = resolve_project_root(args.path.as_deref())?;
    let config = load_project_config_or_default(&root)?;
    if let Some(text) = args.add {
        return add_project_decision(args.data_dir, &root, &config, text, args.json).await;
    }
    if let Some(query) = args.query {
        return query_project_decisions(
            args.data_dir,
            &root,
            &config,
            query,
            args.limit,
            args.json,
        )
        .await;
    }
    unreachable!("clap requires --add or --query");
}

fn run_policy(args: ProjectPolicyArgs) -> Result<()> {
    let root = resolve_project_root(args.path.as_deref())?;
    let config = load_project_config_or_default(&root)?;
    let project = project_descriptor(&root, &config);
    let policy = render_project_policy(args.client.into(), &project);
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": "project policy",
                "client": args.client.as_str(),
                "project": project_json(&root, &config),
                "policy": policy,
            }))
            .context("serialize project policy JSON")?
        );
    } else {
        println!("{policy}");
    }
    Ok(())
}

impl ProjectPolicyClient {
    fn as_str(self) -> &'static str {
        match self {
            ProjectPolicyClient::Generic => "generic",
            ProjectPolicyClient::Codex => "codex",
            ProjectPolicyClient::Claude => "claude",
            ProjectPolicyClient::Cursor => "cursor",
        }
    }
}

async fn add_project_decision(
    data_dir: Option<PathBuf>,
    root: &Path,
    config: &ProjectConfig,
    raw_text: String,
    json_output: bool,
) -> Result<()> {
    let decision = raw_text.trim();
    if decision.is_empty() {
        bail!("decision text must not be empty");
    }
    let ctx = prepare_oneshot(data_dir).await?;
    let project = project_descriptor(root, config);
    let content = project_decision_content(&project, decision);
    let embedding = ctx
        .embedder
        .embed(&content)
        .await
        .context("embed decision")?;
    let now = chrono::Utc::now().timestamp_millis();
    let source_id = project_decision_source_id(&project.id, now);
    let episode = Episode {
        memory_id: MemoryId::new(),
        ts_ms: now,
        source_type: PROJECT_DECISION_SOURCE_TYPE.to_string(),
        source_id: Some(source_id.clone()),
        content: content.clone(),
        encoding_context: project_decision_encoding_context(&project),
        provenance: None,
        confidence: Confidence::new(0.95).unwrap(),
        strength: 0.7,
        salience: 0.8,
        tier: Tier::Hot,
    };
    let mid = ctx
        .write_handle()
        .remember(episode, embedding)
        .await
        .context("writer.remember project decision")?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": "project decisions",
                "action": "add",
                "project": project_json(root, config),
                "memory_id": mid.to_string(),
                "source_type": PROJECT_DECISION_SOURCE_TYPE,
                "source_id": source_id,
                "content": content,
            }))
            .context("serialize project decision add JSON")?
        );
    } else {
        println!("remembered project decision: {mid}");
    }
    ctx.shutdown()
        .await
        .context("shutdown after project decision")
}

async fn query_project_decisions(
    data_dir: Option<PathBuf>,
    root: &Path,
    config: &ProjectConfig,
    raw_query: String,
    limit: usize,
    json_output: bool,
) -> Result<()> {
    let query = raw_query.trim();
    if query.is_empty() {
        bail!("query must not be empty");
    }
    let ctx = prepare_oneshot(data_dir).await?;
    let scoped_query = format!(
        "Project decision for {} (id: {}): {}",
        config.name, config.project_id, query
    );
    let display_limit = limit.clamp(1, 100);
    let result = run_recall(ctx.library_handle.as_ref(), None, &scoped_query, 100)
        .await
        .context("recall project decisions")?;
    let rowids = result.hits.iter().map(|hit| hit.rowid).collect::<Vec<_>>();
    let metadata = fetch_project_decision_hit_metadata(ctx.read_pool(), &rowids).await?;
    let scoped_hits = result
        .hits
        .iter()
        .filter(|hit| project_decision_hit_matches_project(hit, metadata.get(&hit.rowid), config))
        .take(display_limit)
        .collect::<Vec<_>>();

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "command": "project decisions",
                "action": "query",
                "project": project_json(root, config),
                "query": query,
                "limit": display_limit,
                "hits": scoped_hits,
            }))
            .context("serialize project decision query JSON")?
        );
        return ctx
            .shutdown()
            .await
            .context("shutdown after project decisions query");
    }

    if scoped_hits.is_empty() {
        println!("(no project decisions matched)");
    } else {
        for hit in scoped_hits {
            println!(
                "{:>6}  cos_dist={:>7.4}  {}  [{}]",
                hit.rowid,
                hit.cos_distance,
                truncate(&hit.content, 100),
                hit.source_type,
            );
        }
    }
    ctx.shutdown()
        .await
        .context("shutdown after project decisions query")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectDecisionHitMetadata {
    source_id: Option<String>,
    encoding_context_json: String,
}

async fn fetch_project_decision_hit_metadata(
    pool: &ReaderPool,
    rowids: &[i64],
) -> Result<std::collections::HashMap<i64, ProjectDecisionHitMetadata>> {
    if rowids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rowids = rowids.to_vec();
    let rows = pool
        .interact(move |conn| {
            let placeholders = std::iter::repeat("?")
                .take(rowids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT rowid, source_id, encoding_context_json
                   FROM episodes
                  WHERE rowid IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(rusqlite::params_from_iter(rowids.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    ProjectDecisionHitMetadata {
                        source_id: row.get(1)?,
                        encoding_context_json: row.get(2)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .context("fetch project decision hit metadata")?;
    Ok(rows.into_iter().collect())
}

fn project_decision_hit_matches_project(
    hit: &RecallHit,
    metadata: Option<&ProjectDecisionHitMetadata>,
    config: &ProjectConfig,
) -> bool {
    if hit.source_type != PROJECT_DECISION_SOURCE_TYPE {
        return false;
    }
    let source_id = metadata.and_then(|metadata| metadata.source_id.as_deref());
    let encoding_context_json = metadata
        .map(|metadata| metadata.encoding_context_json.as_str())
        .unwrap_or("{}");
    project_decision_scope_matches(
        source_id,
        encoding_context_json,
        &hit.content,
        &config.project_id,
    )
}

async fn ingest_project_docs(
    ctx: &OneShotContext,
    root: &Path,
    scan: &ProjectDocScan,
    chunk_config: ChunkConfig,
) -> Result<()> {
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
                print_project_ingest_report(root, path, &report);
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
        bail!("{failed} project doc(s) failed to import");
    }
    Ok(())
}

fn print_project_ingest_report(root: &Path, path: &Path, report: &IngestReport) {
    let short = report
        .doc_id
        .to_string()
        .chars()
        .take(8)
        .collect::<String>();
    let path = display_relative(root, path);
    if report.deduped {
        println!(
            "deduped {path} -> {short} ({} bytes)",
            report.bytes_ingested
        );
    } else {
        println!(
            "ingested {path} -> {short} ({} chunks, {} bytes)",
            report.chunks_persisted, report.bytes_ingested
        );
    }
}

fn project_ingest_dry_run_json(
    root: &Path,
    config: &ProjectConfig,
    scan: &ProjectDocScan,
) -> serde_json::Value {
    serde_json::json!({
        "command": "project ingest",
        "dry_run": true,
        "root": display_path(root),
        "project_name": &config.name,
        "project_id": &config.project_id,
        "files_scanned": scan.files_scanned,
        "candidates_found": scan.candidate_files,
        "skipped_files": scan.skipped_files,
        "skipped_ignored_dirs": scan.skipped_ignored_dirs,
        "truncated": scan.truncated,
        "project": {
            "name": &config.name,
            "id": &config.project_id,
            "root": display_path(root),
            "tags": &config.tags,
        },
        "counts": {
            "files_scanned": scan.files_scanned,
            "candidate_files": scan.candidate_files,
            "skipped_files": scan.skipped_files,
            "skipped_ignored_dirs": scan.skipped_ignored_dirs,
            "truncated": scan.truncated,
        },
        "candidates": scan
            .candidate_paths
            .iter()
            .map(|path| {
                serde_json::json!({
                    "path": display_path(path),
                    "relative_path": display_relative(root, path).replace('\\', "/"),
                })
            })
            .collect::<Vec<_>>(),
        "candidate_paths": scan
            .candidate_paths
            .iter()
            .map(|path| display_relative(root, path).replace('\\', "/"))
            .collect::<Vec<_>>(),
    })
}

fn project_json(root: &Path, config: &ProjectConfig) -> serde_json::Value {
    serde_json::json!({
        "name": &config.name,
        "id": &config.project_id,
        "root": display_path(root),
        "tags": &config.tags,
    })
}

fn project_descriptor(root: &Path, config: &ProjectConfig) -> ProjectMemoryDescriptor {
    ProjectMemoryDescriptor {
        name: config.name.clone(),
        id: config.project_id.clone(),
        root: display_path(root),
        tags: config.tags.clone(),
    }
}

fn resolve_project_root(path: Option<&Path>) -> Result<PathBuf> {
    let root = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("resolve current directory")?,
    };
    if !root.is_dir() {
        bail!("project root is not a directory: {}", root.display());
    }
    root.canonicalize()
        .with_context(|| format!("canonicalize {}", root.display()))
}

fn project_config_path(root: &Path) -> PathBuf {
    root.join(PROJECT_CONFIG_REL)
}

fn load_project_config_or_default(root: &Path) -> Result<ProjectConfig> {
    let path = project_config_path(root);
    if path.is_file() {
        return parse_project_config(
            &std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?,
            root,
        )
        .with_context(|| format!("parse {}", path.display()));
    }
    Ok(default_project_config(root))
}

fn default_project_config(root: &Path) -> ProjectConfig {
    let name = default_project_name(root);
    ProjectConfig {
        project_id: slugify_project_id(&name),
        name,
        tags: Vec::new(),
        ignore_dirs: DEFAULT_IGNORE_DIRS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
    }
}

fn parse_project_config(text: &str, root: &Path) -> Result<ProjectConfig> {
    let doc = text
        .parse::<DocumentMut>()
        .context("project.toml is not valid TOML")?;
    let project = doc
        .get("project")
        .and_then(|item| item.as_table())
        .context("project.toml must contain a [project] table")?;
    let fallback = default_project_config(root);
    let name = project
        .get("name")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or(fallback.name);
    let project_id = project
        .get("id")
        .and_then(|item| item.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| slugify_project_id(&name));
    let tags = table_string_array(project, "tags").unwrap_or_default();
    let ignore_dirs = table_string_array(project, "ignore_dirs").unwrap_or(fallback.ignore_dirs);

    Ok(ProjectConfig {
        name,
        project_id,
        tags,
        ignore_dirs,
    })
}

fn table_string_array(table: &toml_edit::Table, key: &str) -> Option<Vec<String>> {
    table
        .get(key)
        .and_then(|item| item.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
}

fn render_project_config(config: &ProjectConfig) -> String {
    format!(
        "schema_version = 1\n\n[project]\nname = {}\nid = {}\nroot = \".\"\ntags = {}\nignore_dirs = {}\n",
        toml_string(&config.name),
        toml_string(&config.project_id),
        toml_array(&config.tags),
        toml_array(&config.ignore_dirs),
    )
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).expect("serialize string")
}

fn toml_array(values: &[String]) -> String {
    let body = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{body}]")
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    for tag in tags {
        let tag = tag.trim();
        if !tag.is_empty() && !out.iter().any(|known| known == tag) {
            out.push(tag.to_string());
        }
    }
    out
}

fn default_project_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("project")
        .to_string()
}

fn slugify_project_id(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "project".to_string()
    } else {
        out
    }
}

fn scan_project_docs(
    root: &Path,
    config: &ProjectConfig,
    max_files: usize,
) -> Result<ProjectDocScan> {
    let mut scan = ProjectDocScan::new(root.to_path_buf());
    scan_project_dir(root, root, config, max_files, &mut scan)?;
    scan.candidate_paths.sort();
    Ok(scan)
}

fn scan_project_dir(
    root: &Path,
    dir: &Path,
    config: &ProjectConfig,
    max_files: usize,
    scan: &mut ProjectDocScan,
) -> Result<()> {
    if scan.candidate_paths.len() >= max_files {
        scan.truncated = true;
        return Ok(());
    }

    let mut entries = std::fs::read_dir(dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("read_dir entries under {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.path());

    for entry in entries {
        if scan.candidate_paths.len() >= max_files {
            scan.truncated = true;
            return Ok(());
        }

        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("file_type for {}", path.display()))?;
        if file_type.is_dir() {
            if should_ignore_dir(&path, config) {
                scan.skipped_ignored_dirs += 1;
                continue;
            }
            scan_project_dir(root, &path, config, max_files, scan)?;
        } else if file_type.is_file() {
            scan.files_scanned += 1;
            if is_project_doc_file(root, &path) {
                scan.candidate_files += 1;
                scan.candidate_paths.push(path);
            } else {
                scan.skipped_files += 1;
            }
        }
    }
    Ok(())
}

fn should_ignore_dir(path: &Path, config: &ProjectConfig) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name.starts_with('.') && name != ".github" {
        return true;
    }
    config.ignore_dirs.iter().any(|ignore| ignore == name)
}

fn is_project_doc_file(root: &Path, path: &Path) -> bool {
    if !has_project_doc_extension(path) {
        return false;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(|component| component.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if components.is_empty() {
        return false;
    }
    if components.len() == 1 {
        return path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| ROOT_DOC_STEMS.contains(&stem.to_ascii_lowercase().as_str()))
            .unwrap_or(false);
    }
    components
        .first()
        .map(|first| DOC_DIR_NAMES.contains(&first.as_str()))
        .unwrap_or(false)
}

fn has_project_doc_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            PROJECT_DOC_EXTENSIONS
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(ext))
        })
        .unwrap_or(false)
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

fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn display_path(path: &Path) -> String {
    strip_windows_verbatim_prefix(&path.display().to_string())
}

fn strip_windows_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "..."
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn slugify_project_id_is_stable_and_shell_safe() {
        assert_eq!(slugify_project_id("Solo Desktop"), "solo-desktop");
        assert_eq!(slugify_project_id("  !!!  "), "project");
        assert_eq!(slugify_project_id("API_v2"), "api-v2");
    }

    #[test]
    fn render_and_parse_project_config_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = ProjectConfig {
            name: "Solo".to_string(),
            project_id: "solo".to_string(),
            tags: vec!["memory".to_string()],
            ignore_dirs: vec!["target".to_string(), "node_modules".to_string()],
        };

        let parsed = parse_project_config(&render_project_config(&config), tmp.path()).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn scan_project_docs_keeps_docs_and_skips_generated_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("README.md"), "# hello");
        write(&root.join("docs").join("plan.md"), "# plan");
        write(&root.join("adr").join("0001.md"), "# decision");
        write(&root.join("src").join("main.rs"), "fn main() {}");
        write(&root.join("target").join("generated.md"), "# nope");
        write(&root.join(".hidden").join("secret.md"), "# nope");
        let config = default_project_config(root);

        let scan = scan_project_docs(root, &config, 100).unwrap();
        let names = scan
            .candidate_paths
            .iter()
            .map(|path| display_relative(root, path).replace('\\', "/"))
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "README.md".to_string(),
                "adr/0001.md".to_string(),
                "docs/plan.md".to_string()
            ]
        );
        assert_eq!(scan.skipped_ignored_dirs, 2);
        assert_eq!(scan.skipped_files, 1);
    }

    #[test]
    fn scan_project_docs_honors_max_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("README.md"), "# hello");
        write(&root.join("docs").join("a.md"), "# a");
        write(&root.join("docs").join("b.md"), "# b");
        let config = default_project_config(root);

        let scan = scan_project_docs(root, &config, 2).unwrap();

        assert_eq!(scan.candidate_paths.len(), 2);
        assert!(scan.truncated);
    }

    #[test]
    fn root_doc_detection_is_narrow() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let readme = root.join("README.md");
        let source = root.join("main.md");
        let nested = root.join("src").join("README.md");
        write(&readme, "# ok");
        write(&source, "# not automatically");
        write(&nested, "# not docs");

        assert!(is_project_doc_file(root, &readme));
        assert!(!is_project_doc_file(root, &source));
        assert!(!is_project_doc_file(root, &nested));
    }

    #[test]
    fn project_decision_scope_requires_selected_project() {
        let content =
            "Project decision for Solo (id: solo, root: C:\\repo): Use ADRs for architecture.";
        assert!(project_decision_scope_matches(
            Some("project:solo:decision:123"),
            r#"{"extra":{"project_id":"wrong-project"}}"#,
            content,
            "solo"
        ));
        assert!(project_decision_scope_matches(
            None,
            r#"{"extra":{"project_id":"solo"}}"#,
            "unstructured legacy content",
            "solo"
        ));
        assert!(project_decision_scope_matches(None, "{}", content, "solo"));
        assert!(!project_decision_scope_matches(
            Some("project:other:decision:123"),
            r#"{"extra":{"project_id":"other"}}"#,
            content,
            "solo-api"
        ));
        assert!(!project_decision_scope_matches(
            Some("project:other:decision:123"),
            "{}",
            content,
            "solo"
        ));
        assert!(!project_decision_scope_matches(
            None,
            r#"{"extra":{"project_id":"solo-api"}}"#,
            content,
            "solo"
        ));
    }

    #[test]
    fn project_policy_mentions_project_identity_and_safety_rules() {
        let root = PathBuf::from(r"C:\repo\solo");
        let config = ProjectConfig {
            name: "Solo".to_string(),
            project_id: "solo".to_string(),
            tags: vec!["memory".to_string()],
            ignore_dirs: Vec::new(),
        };

        let project = project_descriptor(&root, &config);
        let policy = render_project_policy(CoreProjectPolicyClient::Codex, &project);

        assert!(policy.contains("Solo Project Memory Policy - Codex"));
        assert!(policy.contains("Project id: solo"));
        assert!(policy.contains("Project tags: memory"));
        assert!(policy.contains("Do not store secrets"));
        assert!(policy.contains("memory_remember_batch"));
    }

    #[test]
    fn display_path_strips_windows_verbatim_prefix_for_humans() {
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\C:\Users\Solo"),
            r"C:\Users\Solo"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"\\?\UNC\server\share"),
            r"\\server\share"
        );
        assert_eq!(
            strip_windows_verbatim_prefix(r"C:\Users\Solo"),
            r"C:\Users\Solo"
        );
    }
}
