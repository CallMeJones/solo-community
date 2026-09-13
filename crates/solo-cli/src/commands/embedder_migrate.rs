// SPDX-License-Identifier: Apache-2.0

//! Safe embedder migrations.
//!
//! `solo migrate-embedder <target>` owns the full offline sequence that used
//! to be documented as a manual checklist: validate the destination embedder,
//! back up config and snapshots, clear the live snapshots, switch the
//! persisted embedder identity, re-embed the Community Memory Library,
//! garbage-collect stale embedding rows, and leave startup to rebuild HNSW
//! from SQL.
//!
//! Both targets run the identical sequence in [`apply_migration`]. `ollama`
//! moves a library onto an Ollama model; `bundled` re-embeds it with the
//! model packaged in this build, which is the way out for a library written
//! by an older Solo whose packaged model has since changed identity.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use solo_core::Embedder;
use solo_storage::{
    EmbedderConfig, HnswParams, KeyMaterial, Lockfile, MemoryLibrary, MemoryLibraryParams,
    OllamaEmbedder, ReembedReport, ReembedScope, SoloConfig, default_data_dir,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::commands::common::read_passphrase;

const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";
const DEFAULT_OLLAMA_MODEL: &str = "nomic-embed-text";

#[derive(Debug, Subcommand)]
pub enum MigrateEmbedderCommand {
    /// Switch Solo to an Ollama embedding model and re-embed the Memory Library.
    Ollama(OllamaEmbedderMigrationArgs),
    /// Re-embed the Memory Library with the model packaged in this Solo.
    ///
    /// The way to open a library written by an older Solo: the packaged model
    /// keeps its name and its 384 dimensions across versions but changes
    /// identity, and a library remembers the identity it was written with.
    Bundled(BundledEmbedderMigrationArgs),
}

#[derive(Debug, Args)]
pub struct OllamaEmbedderMigrationArgs {
    /// Ollama embedding model to use.
    #[arg(long, default_value = DEFAULT_OLLAMA_MODEL)]
    pub model: String,

    /// Expected embedding dimension. If omitted, Solo probes Ollama.
    #[arg(long)]
    pub dim: Option<u32>,

    /// Ollama base URL.
    #[arg(long, default_value = DEFAULT_OLLAMA_BASE_URL)]
    pub base_url: String,

    /// Data directory (defaults to `$SOLO_DATA_DIR` or `~/.solo`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Validate and print the migration plan without changing files or databases.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct BundledEmbedderMigrationArgs {
    /// Data directory (defaults to `$SOLO_DATA_DIR` or `~/.solo`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Validate and print the migration plan without changing files or databases.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Clone)]
struct LibraryMigrationReport {
    prepare: ReembedReport,
    gc: Option<ReembedReport>,
}

pub async fn run(cmd: MigrateEmbedderCommand) -> Result<()> {
    match cmd {
        MigrateEmbedderCommand::Ollama(args) => run_ollama(args).await,
        MigrateEmbedderCommand::Bundled(args) => run_bundled(args).await,
    }
}

async fn run_ollama(args: OllamaEmbedderMigrationArgs) -> Result<()> {
    let model = normalize_ollama_model(&args.model)?;
    let base_url = normalize_ollama_base_url(&args.base_url)?;
    let dim = probe_or_validate_dim(&base_url, &model, args.dim).await?;
    let data_dir = resolve_data_dir(args.data_dir)?;
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        bail!(
            "solo.config.toml not found at {}. Run `solo init` first.",
            config_path.display()
        );
    }

    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    let previous = config.embedder.clone();
    let next = EmbedderConfig {
        name: format!("ollama:{model}"),
        version: "v1".to_string(),
        dim,
        dtype: "f32".to_string(),
    };

    println!("Ollama embedder migration plan");
    println!("  data dir : {}", data_dir.display());
    println!(
        "  previous : {}@{} {}d {}",
        previous.name, previous.version, previous.dim, previous.dtype
    );
    println!(
        "  next     : {}@{} {}d {}",
        next.name, next.version, next.dim, next.dtype
    );
    println!("  base URL : {base_url}");
    println!("  library  : Community Memory Library");

    if args.dry_run {
        println!("dry-run: no config, database, or snapshot files were changed");
        return Ok(());
    }

    let embedder: Arc<dyn Embedder> = Arc::new(
        OllamaEmbedder::new(&base_url, &model, dim as usize).context("build Ollama embedder")?,
    );
    apply_migration(&data_dir, config, next, embedder).await
}

/// Re-embed the library with the model packaged in this build.
#[cfg(feature = "bundled-embedder")]
async fn run_bundled(args: BundledEmbedderMigrationArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir)?;
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        bail!(
            "solo.config.toml not found at {}. Run `solo init` first.",
            config_path.display()
        );
    }
    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    let previous = config.embedder.clone();
    let packaged = solo_storage::BundledEmbedder::new();
    let next = EmbedderConfig {
        name: packaged.name().to_string(),
        version: packaged.version().to_string(),
        dim: packaged.dim() as u32,
        dtype: "f32".to_string(),
    };

    println!("Packaged embedder migration plan");
    println!("  data dir : {}", data_dir.display());
    println!(
        "  previous : {}@{} {}d {}",
        previous.name, previous.version, previous.dim, previous.dtype
    );
    println!(
        "  next     : {}@{} {}d {}",
        next.name, next.version, next.dim, next.dtype
    );
    println!("  library  : Community Memory Library");
    if previous == next {
        println!(
            "  note     : the library already names this exact model; re-embedding it is harmless \
             but will not change anything."
        );
    }

    if args.dry_run {
        println!("dry-run: no config, database, or snapshot files were changed");
        return Ok(());
    }

    apply_migration(&data_dir, config, next, Arc::new(packaged)).await
}

/// Without the packaged model there is nothing to migrate to, and saying so
/// beats failing later inside the library open.
#[cfg(not(feature = "bundled-embedder"))]
async fn run_bundled(_args: BundledEmbedderMigrationArgs) -> Result<()> {
    bail!(
        "this Solo was built without the packaged embedder, so it cannot re-embed a library with \
         one. Use `solo migrate-embedder ollama` instead, or install a build that packages the \
         model."
    )
}

/// Everything a migration does once the destination embedder is settled.
///
/// One sequence for every target, so a target added later cannot quietly get
/// a different -- and less safe -- order of operations.
async fn apply_migration(
    data_dir: &Path,
    mut config: SoloConfig,
    next: EmbedderConfig,
    embedder: Arc<dyn Embedder>,
) -> Result<()> {
    let config_path = data_dir.join("solo.config.toml");
    let changed = config.embedder != next;

    let lock_path = data_dir.join("solo.lock");
    let lock = Lockfile::acquire(&lock_path)
        .context("acquire solo.lock - stop Solo before migrating the embedder")?;
    let passphrase = read_passphrase()?;
    let salt = config.salt_bytes().context("decode salt from config")?;
    let key = KeyMaterial::derive(&passphrase, &salt)
        .context("derive key from passphrase + persisted salt")?;
    drop(passphrase);

    let stamp = unix_millis();
    let snapshot_backup = backup_hnsw_snapshots(data_dir, stamp)
        .with_context(|| format!("backup HNSW snapshots under {}", data_dir.display()))?;
    if let Some(path) = snapshot_backup.as_ref() {
        println!("  HNSW backup: {}", path.display());
    } else {
        println!("  HNSW backup: no existing snapshot files");
    }

    // Clear the live snapshots now that copies of them are safe.
    //
    // They were built by the OLD embedder, and the next thing this migration
    // does is open the library under the NEW one -- which refuses a snapshot
    // whose dimension disagrees with the config. Deleting them only after the
    // re-embed, as this used to, put the deletion behind the open it was
    // needed for: the migration failed with "HNSW snapshot dim (384) does not
    // match solo.config.toml embedder.dim (768)" and left the operator to
    // remove the files by hand. Rebuilding them from the re-embedded rows is
    // the whole point, and the backup remains for rollback.
    if snapshot_backup.is_some() {
        delete_hnsw_snapshots(data_dir).context("clear HNSW snapshots after backing them up")?;
        println!("  HNSW snapshots cleared; startup rebuilds them from SQL");
    }

    let config_backup = if changed {
        config.embedder = next;
        let backup = replace_config_with_backup(&config_path, &config, stamp)
            .with_context(|| format!("replace {}", config_path.display()))?;
        println!("  config backup: {}", backup.display());
        Some(backup)
    } else {
        println!("  config: already set to the requested embedder");
        None
    };

    let migration_result =
        run_reembed_phases(data_dir, key, config_backup.as_deref(), embedder).await;
    drop(lock);
    migration_result
}

async fn run_reembed_phases(
    data_dir: &Path,
    key: KeyMaterial,
    config_backup: Option<&Path>,
    embedder: Arc<dyn Embedder>,
) -> Result<()> {
    let config_path = data_dir.join("solo.config.toml");
    let runtime_handle = tokio::runtime::Handle::current();
    let library = Arc::new(
        MemoryLibrary::open(MemoryLibraryParams {
            data_dir: data_dir.to_path_buf(),
            key,
            embedder,
            hnsw_params: HnswParams::default(),
            steward: None,
            runtime_handle: Some(runtime_handle),
            steward_factory: None,
            triples_batch_signal: None,
        })
        .context("open Community Memory Library with new embedder")?,
    );

    let handle = library
        .handle()
        .await
        .context("open Community Memory Library")?;
    let prepare = handle
        .write()
        .reembed(ReembedScope {
            from: None,
            dry_run: false,
            gc: false,
        })
        .await
        .context("re-embed Community Memory Library")?;
    println!(
        "  Memory Library: prepared current rows seen={} reembedded={} failed={}",
        prepare.rows_seen, prepare.rows_reembedded, prepare.rows_failed
    );
    if prepare.rows_failed > 0 {
        drop(handle);
        library.shutdown_with_snapshot(false).await;
        if let Some(backup) = config_backup {
            std::fs::copy(backup, &config_path).with_context(|| {
                format!(
                    "restore config backup {} to {}",
                    backup.display(),
                    config_path.display()
                )
            })?;
        }
        bail!(
            "aborting before stale-row GC because the prepare phase had {} failures",
            prepare.rows_failed
        );
    }

    let gc = handle
        .write()
        .reembed(ReembedScope {
            from: None,
            dry_run: false,
            gc: true,
        })
        .await
        .context("garbage-collect stale Community Memory Library embeddings")?;
    println!(
        "  Memory Library: GC seen={} reembedded={} failed={} deleted={}",
        gc.rows_seen, gc.rows_reembedded, gc.rows_failed, gc.rows_gc_deleted
    );
    let report = LibraryMigrationReport {
        prepare,
        gc: Some(gc),
    };

    // Belt and braces: the live pair was already cleared before the open, so
    // this only catches a snapshot written during the run itself.
    delete_hnsw_snapshots(data_dir).context("delete stale HNSW snapshots")?;
    drop(handle);
    library.shutdown_with_snapshot(false).await;

    let prepared = report.prepare.rows_reembedded;
    let deleted = report.gc.as_ref().map_or(0, |gc| gc.rows_gc_deleted);
    println!("migration complete: prepared={prepared} stale_deleted={deleted}");
    println!("Start Solo again; startup will rebuild HNSW from SQL embeddings.");

    if report.gc.as_ref().is_some_and(|gc| gc.rows_failed > 0) {
        bail!(
            "migration wrote current vectors but stale-row GC had {} failures",
            report.gc.as_ref().map_or(0, |gc| gc.rows_failed)
        );
    }

    Ok(())
}

fn resolve_data_dir(data_dir: Option<PathBuf>) -> Result<PathBuf> {
    match data_dir {
        Some(path) => Ok(path),
        None => default_data_dir().context("could not resolve default data dir; pass --data-dir"),
    }
}

async fn probe_or_validate_dim(base_url: &str, model: &str, expected: Option<u32>) -> Result<u32> {
    let probe_dim = expected.unwrap_or(768);
    let embedder = OllamaEmbedder::new(base_url, model, probe_dim as usize)
        .context("build Ollama embedder probe")?;
    let actual = embedder
        .probe_dim()
        .await
        .with_context(|| format!("probe Ollama model `{model}` at {base_url}"))?;
    if let Some(expected) = expected
        && actual != expected as usize
    {
        bail!("Ollama model `{model}` returned {actual} dimensions, but --dim expected {expected}");
    }
    u32::try_from(actual).context("Ollama dimension does not fit u32")
}

fn normalize_ollama_model(model: &str) -> Result<String> {
    let model = model.trim().to_string();
    if model.is_empty() {
        bail!("model must not be empty");
    }
    if model.len() > 128 {
        bail!("model must be at most 128 characters");
    }
    if !model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '-' | '_' | '.' | '/'))
    {
        bail!(
            "model may contain only ASCII letters, numbers, colon, dash, underscore, dot, or slash"
        );
    }
    Ok(model)
}

fn normalize_ollama_base_url(base_url: &str) -> Result<String> {
    let base_url = base_url.trim().trim_end_matches('/').to_string();
    if base_url.is_empty() {
        bail!("base-url must not be empty");
    }
    if base_url.len() > 512 {
        bail!("base-url must be at most 512 characters");
    }
    if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
        bail!("base-url must start with http:// or https://");
    }
    if base_url.chars().any(char::is_whitespace) {
        bail!("base-url must not contain whitespace");
    }
    Ok(base_url)
}

fn replace_config_with_backup(
    config_path: &Path,
    config: &SoloConfig,
    stamp: u128,
) -> Result<PathBuf> {
    let parent = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent: {}", config_path.display()))?;
    let file_name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("solo.config.toml");
    let backup_path = parent.join(format!("{file_name}.bak-before-embedder-migrate-{stamp}"));
    let tmp_path = parent.join(format!("{file_name}.tmp-embedder-migrate-{stamp}"));
    std::fs::copy(config_path, &backup_path)
        .with_context(|| format!("backup {}", config_path.display()))?;
    let body = toml::to_string_pretty(config).context("serialize solo.config.toml")?;
    {
        let mut tmp_file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .with_context(|| format!("open temp config {}", tmp_path.display()))?;
        std::io::Write::write_all(&mut tmp_file, body.as_bytes())
            .with_context(|| format!("write temp config {}", tmp_path.display()))?;
        tmp_file
            .sync_all()
            .with_context(|| format!("sync temp config {}", tmp_path.display()))?;
    }
    match std::fs::rename(&tmp_path, config_path) {
        Ok(()) => Ok(backup_path),
        Err(first_error) => {
            let _ = std::fs::remove_file(config_path);
            match std::fs::rename(&tmp_path, config_path) {
                Ok(()) => Ok(backup_path),
                Err(second_error) => {
                    let _ = std::fs::copy(&backup_path, config_path);
                    Err(anyhow::anyhow!(
                        "replace {} failed: {first_error}; retry failed: {second_error}; attempted restore from {}",
                        config_path.display(),
                        backup_path.display()
                    ))
                }
            }
        }
    }
}

fn backup_hnsw_snapshots(data_dir: &Path, stamp: u128) -> Result<Option<PathBuf>> {
    let paths = existing_hnsw_snapshot_paths(data_dir);
    if paths.is_empty() {
        return Ok(None);
    }
    let backup_root = data_dir
        .join("backups")
        .join(format!("hnsw-before-embedder-migrate-{stamp}"));
    for path in paths {
        let relative = path
            .strip_prefix(data_dir)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| {
                path.file_name()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("snapshot"))
            });
        let dest = backup_root.join(relative);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::copy(&path, &dest)
            .with_context(|| format!("copy {} to {}", path.display(), dest.display()))?;
    }
    Ok(Some(backup_root))
}

fn delete_hnsw_snapshots(data_dir: &Path) -> Result<()> {
    for dir in hnsw_snapshot_dirs(data_dir) {
        solo_storage::snapshot::delete_all_pairs(&dir)
            .with_context(|| format!("delete snapshot pairs in {}", dir.display()))?;
    }
    Ok(())
}

fn existing_hnsw_snapshot_paths(data_dir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for dir in hnsw_snapshot_dirs(data_dir) {
        for basename in [
            solo_storage::LIVE_BASENAME,
            solo_storage::BAK_BASENAME,
            solo_storage::TMP_BASENAME,
        ] {
            for suffix in [".hnsw.data", ".hnsw.graph"] {
                let path = dir.join(format!("{basename}{suffix}"));
                if path.is_file() {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn hnsw_snapshot_dirs(data_dir: &Path) -> Vec<PathBuf> {
    vec![data_dir.to_path_buf()]
}

fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_ollama_model_accepts_common_tags() {
        assert_eq!(
            normalize_ollama_model(" nomic-embed-text:v1 ").unwrap(),
            "nomic-embed-text:v1"
        );
        assert_eq!(
            normalize_ollama_model("library/model_1.2").unwrap(),
            "library/model_1.2"
        );
    }

    #[test]
    fn normalize_ollama_model_rejects_spaces() {
        let err = normalize_ollama_model("nomic embed")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ASCII letters"), "{err}");
    }

    #[test]
    fn normalize_ollama_base_url_trims_slashes() {
        assert_eq!(
            normalize_ollama_base_url(" http://localhost:11434/// ").unwrap(),
            "http://localhost:11434"
        );
    }

    #[test]
    fn normalize_ollama_base_url_requires_http() {
        let err = normalize_ollama_base_url("file:///tmp")
            .unwrap_err()
            .to_string();
        assert!(err.contains("http:// or https://"), "{err}");
    }

    #[test]
    fn backing_up_snapshots_then_clearing_them_leaves_only_the_copies() {
        // The migration opens the library under the NEW embedder immediately
        // after this pair of steps, and that open refuses a snapshot whose
        // dimension disagrees with the config. Deleting only after the
        // re-embed -- as this once did -- put the deletion behind the very
        // open it was needed for, so the migration died with "HNSW snapshot
        // dim (384) does not match solo.config.toml embedder.dim (768)" and
        // left the operator to remove the files by hand.
        let dir = tempfile::tempdir().expect("tempdir");
        let data = dir.path().join("hnsw_episodes.hnsw.data");
        let graph = dir.path().join("hnsw_episodes.hnsw.graph");
        std::fs::write(&data, b"old-vectors").expect("write data");
        std::fs::write(&graph, b"old-graph").expect("write graph");

        let backup = backup_hnsw_snapshots(dir.path(), 1234).expect("backup");
        let backup = backup.expect("a backup directory for existing snapshots");
        delete_hnsw_snapshots(dir.path()).expect("clear the live snapshots");

        assert!(
            !data.exists() && !graph.exists(),
            "the live snapshots must be gone before the library is reopened"
        );
        assert_eq!(
            std::fs::read(backup.join("hnsw_episodes.hnsw.data")).expect("backed-up data"),
            b"old-vectors",
            "the backup is the rollback path and must survive untouched"
        );
        assert!(
            backup.join("hnsw_episodes.hnsw.graph").is_file(),
            "both halves of the pair belong in the backup"
        );
    }

    #[test]
    fn a_library_with_no_snapshots_reports_nothing_to_back_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            backup_hnsw_snapshots(dir.path(), 1234)
                .expect("backup")
                .is_none(),
            "there is nothing to copy, and nothing to delete afterwards"
        );
    }

    #[test]
    fn hnsw_snapshot_paths_use_the_community_library_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("hnsw_episodes.hnsw.data"), b"data").expect("write data");
        std::fs::write(dir.path().join("hnsw_episodes.hnsw.graph"), b"graph").expect("write graph");
        let paths = existing_hnsw_snapshot_paths(dir.path());
        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with("hnsw_episodes.hnsw.data"))
        );
    }
}
