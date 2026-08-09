// SPDX-License-Identifier: Apache-2.0

//! `solo restore --from <path>` — restore Community's one encrypted Memory Library.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_storage::{
    BACKUP_STACK_SIZE_BYTES, KeyMaterial, Lockfile, SoloConfig, default_data_dir,
    paths_refer_to_same_file, restore_library,
};
use std::path::PathBuf;

use crate::commands::common::read_passphrase;

#[derive(Debug, Args)]
pub struct RestoreArgs {
    /// Encrypted backup database produced by `solo backup`.
    #[arg(long)]
    pub from: PathBuf,

    /// Confirm replacement of the current Community Memory Library.
    #[arg(long)]
    pub confirm: bool,

    /// Override the data dir (default: `~/.solo`, or `SOLO_DATA_DIR`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: RestoreArgs) -> Result<()> {
    std::thread::Builder::new()
        .name("solo-restore".into())
        .stack_size(BACKUP_STACK_SIZE_BYTES)
        .spawn(move || run_inner(args))
        .context("spawn restore worker")?
        .join()
        .map_err(|_| anyhow::anyhow!("restore worker panicked"))?
}

fn run_inner(args: RestoreArgs) -> Result<()> {
    if !args.confirm {
        bail!(
            "restore replaces the current Community Memory Library; re-run with --confirm after verifying the backup path"
        );
    }
    if !args.from.is_file() {
        bail!("restore source not found: {}", args.from.display());
    }

    let data_dir = match args.data_dir {
        Some(path) => path,
        None => default_data_dir()
            .context("could not resolve default data dir; pass --data-dir explicitly")?,
    };
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        bail!(
            "solo.config.toml not found at {}. Restore requires the config whose salt matches the backup.",
            config_path.display()
        );
    }
    let db_path = data_dir.join("solo.db");
    if paths_refer_to_same_file(&args.from, &db_path) {
        bail!(
            "restore source {} is the live solo.db; refusing to replace a database with itself",
            args.from.display()
        );
    }

    let _lock = Lockfile::acquire(&data_dir.join("solo.lock"))
        .context("acquire solo.lock — stop Solo before restoring")?;
    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    let salt = config.salt_bytes().context("decode salt from config")?;
    let passphrase = read_passphrase()?;
    let key = KeyMaterial::derive(&passphrase, &salt)
        .context("derive key from passphrase + persisted salt")?;
    drop(passphrase);

    let report = restore_library(&args.from, &db_path, &key, &data_dir, true)
        .context("restore Community Memory Library")?;

    println!(
        "✓ restored Community Memory Library from {}",
        report.from.display()
    );
    println!("  database: {}", db_path.display());
    println!("  bytes: {}", report.bytes_restored);
    println!(
        "  retained assets: {} files, {} bytes",
        report.asset_files_restored, report.asset_bytes_restored
    );
    Ok(())
}
