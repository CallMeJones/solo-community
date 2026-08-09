// SPDX-License-Identifier: Apache-2.0

//! `solo backup --to <path>` — encrypted online backup of the data directory.
//!
//! Solo derives its 32-byte SQLCipher key on the fly via Argon2id, so the
//! standard `sqlcipher … PRAGMA key = 'passphrase'; .backup …` recipe doesn't
//! work — that uses PBKDF2 and produces a different key. This subcommand
//! threads the raw Argon2id-derived key through SQLite's online backup API
//! and writes a destination file encrypted with the same key.
//!
//! The destination is a self-contained SQLCipher database. It includes the
//! live DB pages plus retained original-file asset blobs in a backup-only
//! table that restore strips before the DB becomes live. Restore it through
//! `solo restore` against a target data directory that uses the
//! original `solo.config.toml` (the salt is in the config; without it, the
//! same passphrase derives a different key and the backup won't open).
//!
//! ## Lockfile semantics
//!
//! Like every other one-shot, `solo backup` acquires `solo.lock` for the
//! duration of the run. If a daemon or another one-shot is touching the
//! data dir, the backup refuses with a clear error rather than racing.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_storage::{
    BACKUP_STACK_SIZE_BYTES, KeyMaterial, Lockfile, SoloConfig, backup_database, backup_temp_path,
    default_data_dir, package_asset_blobs_into_backup, paths_refer_to_same_file,
    replace_with_completed_backup,
};
use std::path::PathBuf;

use crate::commands::common::read_passphrase;

#[derive(Debug, Args)]
pub struct BackupArgs {
    /// Destination path for the encrypted backup. The parent directory
    /// must exist; the file itself is created by the backup.
    #[arg(long)]
    pub to: PathBuf,

    /// Overwrite `--to` if a file already exists at that path. The
    /// replacement is written to a sibling temp path before the existing
    /// destination is replaced.
    #[arg(long)]
    pub force: bool,

    /// Override the data dir (default: `~/.solo`, or `SOLO_DATA_DIR`).
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(args: BackupArgs) -> Result<()> {
    // The one-shot CLI normally executes on the process main thread, whose
    // Windows stack is too small for the bundled SQLCipher online-backup path
    // in optimized builds. Match the daemon writer's explicit backup stack.
    std::thread::Builder::new()
        .name("solo-backup".into())
        .stack_size(BACKUP_STACK_SIZE_BYTES)
        .spawn(move || run_inner(args))
        .context("spawn backup worker")?
        .join()
        .map_err(|_| anyhow::anyhow!("backup worker panicked"))?
}

fn run_inner(args: BackupArgs) -> Result<()> {
    let data_dir = match args.data_dir {
        Some(p) => p,
        None => default_data_dir()
            .context("could not resolve default data dir; pass --data-dir explicitly")?,
    };
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        bail!(
            "solo.config.toml not found at {}. Run `solo init` first.",
            config_path.display()
        );
    }
    // Community has one physical Memory Library database.
    let db_path = data_dir.join("solo.db");
    if !db_path.is_file() {
        bail!(
            "solo.db not found at {}. Has the data dir been initialised?",
            db_path.display()
        );
    }

    // Pre-flight on destination.
    //
    // CRITICAL ORDER: same-file refusal MUST come BEFORE force-overwrite
    // replacement.
    // Without this guard, `solo backup --to <data-dir>/solo.db --force`
    // would target the live `solo.db`; the helper from
    // solo-storage::backup canonicalises both paths so mixed slash
    // directions, redundant `./` segments, etc. are all caught. The
    // staged temp path for `--force` is allocated only after this check.
    // See v0.3.4 release notes.
    if paths_refer_to_same_file(&db_path, &args.to) {
        bail!(
            "backup destination {} is the same file as the source database; \
             refusing to run (would corrupt the live database)",
            args.to.display()
        );
    }
    let force_replace = args.to.exists() && args.force;
    if args.to.exists() && !args.force {
        bail!(
            "backup destination {} exists; pass --force to overwrite",
            args.to.display()
        );
    }
    if let Some(parent) = args.to.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            bail!(
                "backup destination parent directory {} does not exist",
                parent.display()
            );
        }
    }

    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    let salt = config.salt_bytes().context("decode salt from config")?;

    // Acquire the lockfile BEFORE prompting for the passphrase — fail-fast
    // if another Solo process holds the data dir.
    let lock_path = data_dir.join("solo.lock");
    let lock = Lockfile::acquire(&lock_path)
        .context("acquire solo.lock — daemon or another one-shot already running?")?;

    let passphrase = read_passphrase()?;
    let key = KeyMaterial::derive(&passphrase, &salt)
        .context("derive key from passphrase + persisted salt")?;
    drop(passphrase); // Zeroizing<String> wipes here.

    eprintln!(
        "Backing up {} to {} ...",
        db_path.display(),
        args.to.display()
    );
    let started = std::time::Instant::now();
    let backup_dest = if force_replace {
        backup_temp_path(&args.to).context("allocate temporary backup destination")?
    } else {
        args.to.clone()
    };
    if let Err(err) = backup_database(&db_path, &backup_dest, &key).context("run online backup") {
        if force_replace {
            let _ = std::fs::remove_file(&backup_dest);
        }
        return Err(err);
    }
    let snapshot_dir = data_dir.clone();
    let asset_report = match package_asset_blobs_into_backup(&backup_dest, &key, &snapshot_dir)
        .context("package retained asset blobs into backup")
    {
        Ok(report) => report,
        Err(err) => {
            let _ = std::fs::remove_file(&backup_dest);
            return Err(err);
        }
    };
    if force_replace {
        replace_with_completed_backup(&backup_dest, &args.to)
            .context("replace existing backup destination")?;
    }
    let elapsed = started.elapsed();

    drop(lock);

    println!(
        "✓ backed up to {} ({:.2}s)",
        args.to.display(),
        elapsed.as_secs_f64()
    );
    println!(
        "  retained assets: {} files, {} bytes",
        asset_report.asset_files, asset_report.asset_bytes
    );
    println!();
    println!("To restore on a target data dir:");
    println!("  1. Copy your existing solo.config.toml to the target data dir.");
    println!("  2. Stop Solo so the target data directory is unlocked.");
    println!(
        "  3. Run `solo restore --from {} --data-dir <target-data-dir> --confirm`.",
        args.to.display()
    );
    println!("     This extracts retained assets and strips backup-only tables.");
    Ok(())
}
