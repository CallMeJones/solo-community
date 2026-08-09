// SPDX-License-Identifier: Apache-2.0

//! Online SQLCipher backup.
//!
//! Solo derives its 32-byte SQLCipher key on the fly via Argon2id from the
//! user's passphrase + the persisted salt in `solo.config.toml`. SQLCipher's
//! standard CLI `.backup` command uses PBKDF2 to turn a passphrase into a
//! key, which produces a different value than Solo's Argon2id derivation —
//! so the obvious `sqlcipher … PRAGMA key = 'passphrase'; .backup target.db`
//! recipe fails with "file is not a database" against a Solo data dir.
//!
//! This module exposes [`backup_database`] — a programmatic equivalent that
//! threads the raw key through SQLite's online backup API. Both source and
//! destination are opened with `PRAGMA key = "x'<hex>'"` (raw form), so the
//! resulting backup file is encrypted with the same key as the source and
//! restores cleanly when paired with a copy of `solo.config.toml`.
//!
//! ## Backup ownership
//!
//! [`backup_database`] is the lockfile-oriented, DB-only primitive used by
//! one-shot callers. Daemon hot backup uses `WriteCommand::Backup`, which
//! runs against the writer's existing connection and then packages retained
//! original-file assets into the encrypted backup DB.

use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use rusqlite::backup::Backup;
use rusqlite::blob::ZeroBlob;
use rusqlite::{Connection, MAIN_DB, OptionalExtension, params};

use crate::asset_blob::{ASSET_BLOB_PLAINTEXT_ALG, decode_asset_blob, expected_stored_size};
use crate::init::open_sqlcipher;
use crate::key_material::KeyMaterial;
use solo_core::{Error, Result};

/// Default page-step size for the backup loop. SQLCipher pages are 4 KiB by
/// default, so 100 pages = 400 KiB per step. Small enough that a SIGINT
/// during backup tears down quickly; large enough that the per-step
/// overhead is negligible for typical (single-digit GB) corpora.
pub const DEFAULT_BACKUP_PAGES_PER_STEP: i32 = 100;

const ASSET_BACKUP_TABLE: &str = "solo_backup_asset_blobs";
const ASSET_BACKUP_VERSION: i64 = 2;

/// Stack reserved for threads that execute SQLCipher's online-backup path.
///
/// The bundled SQLCipher/SQLite backup implementation exceeds Rust's default
/// Windows thread stack in optimized builds. Keep the requirement beside the
/// backup implementation so daemon and one-shot callers use the same proven
/// bound instead of relying on an operator-provided `RUST_MIN_STACK` value.
pub const BACKUP_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct AssetBackupReport {
    pub asset_files: u32,
    pub asset_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct StagedAssetRestore {
    staging_parent: PathBuf,
    staged_assets: PathBuf,
    report: AssetBackupReport,
}

impl StagedAssetRestore {
    pub(crate) fn report(&self) -> AssetBackupReport {
        self.report
    }

    pub(crate) fn promote(self, snapshot_dir: &Path) -> Result<AssetBackupReport> {
        let report = self.report;
        replace_assets_dir(snapshot_dir, &self.staged_assets)?;
        Ok(report)
    }
}

impl Drop for StagedAssetRestore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.staging_parent);
    }
}

/// Run an online SQLCipher backup of `src_path` to `dest_path`, encrypting
/// the destination with the same raw key.
///
/// Both source and destination are opened with `PRAGMA key = "x'<hex>'"`
/// (raw key form). The destination file is created if missing; if it
/// already exists, its contents are overwritten by the backup.
///
/// Returns `Err(Conflict)` if the source can't be opened with the supplied
/// key (typically a wrong passphrase / wrong salt — the source isn't
/// actually decryptable).
///
/// ## Lockfile responsibility
///
/// Callers must hold `solo.lock` around this call. The function does not
/// acquire it itself — that's a one-shot-vs-daemon coordination concern
/// best left to the caller.
pub fn backup_database(src_path: &Path, dest_path: &Path, key: &KeyMaterial) -> Result<()> {
    // Source: full Solo-style open (PRAGMA key + WAL + foreign_keys +
    // busy_timeout). open_sqlcipher's `PRAGMA journal_mode = wal` query
    // forces decryption — a wrong key surfaces here, before we touch
    // the destination.
    let src = open_sqlcipher(src_path, key)?;
    let result = backup_from_connection(&src, dest_path, key);
    // Close the source explicitly so any deferred error (e.g. WAL
    // checkpoint failure) surfaces here rather than on Drop.
    if let Err((_, e)) = src.close() {
        return Err(Error::storage(format!("close source after backup: {e}")));
    }
    result
}

/// Return a hidden sibling path for writing a replacement backup before
/// touching the caller's requested destination.
pub fn backup_temp_path(dest_path: &Path) -> Result<PathBuf> {
    let parent = dest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = dest_path.file_name().ok_or_else(|| {
        Error::invalid_input(format!(
            "backup destination {} has no file name",
            dest_path.display()
        ))
    })?;
    let stem = file_name.to_string_lossy();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for attempt in 0..1000u32 {
        let candidate = parent.join(format!(
            ".{stem}.{}.{}.tmp",
            std::process::id(),
            nonce + u128::from(attempt)
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Error::storage(format!(
        "could not allocate temporary backup path next to {}",
        dest_path.display()
    )))
}

/// Move a fully-written temporary backup into place.
///
/// Unix can rename over an existing destination atomically. Windows cannot,
/// so the Windows branch first moves the old destination aside, then moves
/// the completed backup into place. If the final move fails, it attempts to
/// restore the previous backup before returning an error.
pub fn replace_with_completed_backup(temp_path: &Path, dest_path: &Path) -> Result<()> {
    if !temp_path.is_file() {
        return Err(Error::invalid_input(format!(
            "temporary backup {} does not exist",
            temp_path.display()
        )));
    }
    if let Some(parent) = dest_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            return Err(Error::invalid_input(format!(
                "backup destination parent directory {} does not exist",
                parent.display()
            )));
        }
    }

    #[cfg(windows)]
    {
        if dest_path.exists() {
            let old_path = backup_temp_path(dest_path)?;
            std::fs::rename(dest_path, &old_path).map_err(|e| {
                Error::storage(format!(
                    "move existing backup destination {} aside to {}: {e}",
                    dest_path.display(),
                    old_path.display()
                ))
            })?;

            match std::fs::rename(temp_path, dest_path) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&old_path);
                    return Ok(());
                }
                Err(replace_err) => match std::fs::rename(&old_path, dest_path) {
                    Ok(()) => {
                        return Err(Error::storage(format!(
                            "replace backup destination {} with {} failed; previous backup restored: {replace_err}",
                            dest_path.display(),
                            temp_path.display()
                        )));
                    }
                    Err(restore_err) => {
                        return Err(Error::storage(format!(
                            "replace backup destination {} with {} failed ({replace_err}); restoring previous backup from {} also failed: {restore_err}",
                            dest_path.display(),
                            temp_path.display(),
                            old_path.display()
                        )));
                    }
                },
            }
        }

        std::fs::rename(temp_path, dest_path).map_err(|e| {
            Error::storage(format!(
                "replace backup destination {} with {}: {e}",
                dest_path.display(),
                temp_path.display()
            ))
        })
    }

    #[cfg(not(windows))]
    std::fs::rename(temp_path, dest_path).map_err(|e| {
        Error::storage(format!(
            "replace backup destination {} with {}: {e}",
            dest_path.display(),
            temp_path.display()
        ))
    })
}

/// Compare two paths for "do they refer to the same file on disk." Both
/// paths are canonicalised; the destination may not exist yet, so its
/// parent is canonicalised and the filename reattached. Returns false
/// for any path that can't be canonicalised at all (don't infer
/// equality from a missing source).
///
/// Callers (CLI `solo backup`, HTTP `POST /backup`, the daemon's
/// `WriteCommand::Backup`) use this to refuse a backup that would
/// destroy the live source database. The `--force` flag's
/// `remove_file(dest)` step is destructive when `dest == source`, so
/// the check MUST run before that step — see the v0.3.4 release notes
/// for what happens when this guard is missing.
pub fn paths_refer_to_same_file(src: &Path, dest: &Path) -> bool {
    let src_canon = match std::fs::canonicalize(src) {
        Ok(p) => p,
        Err(_) => return false,
    };
    // `Path::parent` returns `Some("")` for a bare filename like
    // `solo.db`. Treat that as the current directory so
    // canonicalisation succeeds.
    let dest_parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let (Ok(dest_parent_canon), Some(dest_file)) =
        (std::fs::canonicalize(dest_parent), dest.file_name())
    else {
        return false;
    };
    let dest_canon = dest_parent_canon.join(dest_file);
    src_canon == dest_canon
}

/// Run an online SQLCipher backup using an already-open source connection.
///
/// The daemon-side hot-backup path uses this: the writer's existing
/// connection is the source (so the backup runs against live in-flight
/// writer state via SQLite's page-level snapshot), and we open + key the
/// destination fresh. Callers that don't have an open connection can use
/// [`backup_database`] instead.
///
/// `key` is the same raw `KeyMaterial` the source connection was opened
/// with — used to encrypt the destination so it restores under the same
/// passphrase + salt.
pub fn backup_from_connection(src: &Connection, dest_path: &Path, key: &KeyMaterial) -> Result<()> {
    // Defense-in-depth: refuse if dest is the same file as src. SQLite's
    // online backup is undefined behavior when source and destination
    // are the same database. Note: the CLI / HTTP layers check this
    // BEFORE any destructive `remove_file(dest)` for `--force`. By the
    // time we reach this function, that pre-flight has already passed
    // (or there was no `--force`); this is the second line of defense.
    if let Some(src_str) = src.path() {
        if paths_refer_to_same_file(Path::new(src_str), dest_path) {
            return Err(Error::invalid_input(format!(
                "backup destination {} is the same file as the source database; \
                 refusing to overwrite (would corrupt the live database)",
                dest_path.display()
            )));
        }
    }

    // Destination: minimal open. We don't run startup pragmas; the
    // backup overwrites the entire database (header + pages), so any
    // pragma we set here would be discarded. We DO need PRAGMA key
    // upfront so SQLCipher writes encrypted pages.
    let mut dst = Connection::open(dest_path).map_err(|e| {
        Error::storage(format!(
            "open backup destination {}: {e}",
            dest_path.display()
        ))
    })?;
    // Wrap the formatted PRAGMA in Zeroizing<String> so the raw key bytes
    // are wiped on drop rather than lingering in the heap. The `hex`
    // source is already Zeroizing<String>; this closes the parallel gap.
    let key_pragma: zeroize::Zeroizing<String> = {
        let hex = key.as_hex();
        zeroize::Zeroizing::new(format!("PRAGMA key = \"x'{}'\"", *hex))
    };
    dst.execute_batch(&key_pragma)
        .map_err(|e| Error::storage(format!("PRAGMA key on backup destination: {e}")))?;

    // SQLite's online backup. `Backup::new` borrows both connections;
    // `run_to_completion` drives the page-copy loop in-process. SQLite
    // takes a page-level snapshot of `src`, so concurrent writes on
    // the source are safe — the backup sees a consistent view as of
    // `Backup::new` time. The `pause_between_pages_ms = 0` argument
    // means "no throttle" — for a personal-scale corpus the backup
    // finishes in well under a second per GB of source.
    let backup =
        Backup::new(src, &mut dst).map_err(|e| Error::storage(format!("Backup::new: {e}")))?;
    backup
        .run_to_completion(
            DEFAULT_BACKUP_PAGES_PER_STEP,
            std::time::Duration::from_millis(0),
            None,
        )
        .map_err(|e| Error::storage(format!("Backup::run_to_completion: {e}")))?;

    // Drop the backup struct first (releases its borrows on src + dst),
    // then close the destination explicitly so any deferred error
    // surfaces here rather than on Drop.
    drop(backup);
    dst.close()
        .map_err(|(_, e)| Error::storage(format!("close destination after backup: {e}")))?;

    Ok(())
}

/// Embed retained original-file assets into a SQLCipher backup DB.
///
/// The backup remains a single encrypted `.db` file. Asset rows are written
/// into a backup-only table that restore strips out after extracting bytes.
pub fn package_asset_blobs_into_backup(
    backup_path: &Path,
    key: &KeyMaterial,
    snapshot_dir: &Path,
) -> Result<AssetBackupReport> {
    let conn = open_sqlcipher(backup_path, key)?;
    let result = package_asset_blobs_into_connection(&conn, key, snapshot_dir)
        .and_then(|report| checkpoint_backup_db(&conn).map(|()| report));
    let close_result = conn
        .close()
        .map_err(|(_, e)| Error::storage(format!("close backup after asset packaging: {e}")));
    match (result, close_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Restore backup-embedded asset blobs into `snapshot_dir/assets`.
///
/// Backups created before asset embedding do not contain the backup-only table;
/// those restore as DB-only and leave the existing asset directory untouched.
pub fn restore_asset_blobs_from_backup(
    backup_path: &Path,
    key: &KeyMaterial,
    snapshot_dir: &Path,
) -> Result<AssetBackupReport> {
    let Some(staged) = stage_asset_blobs_from_backup(backup_path, key, snapshot_dir)? else {
        return Ok(AssetBackupReport::default());
    };
    staged.promote(snapshot_dir)
}

pub(crate) fn stage_asset_blobs_from_backup(
    backup_path: &Path,
    key: &KeyMaterial,
    snapshot_dir: &Path,
) -> Result<Option<StagedAssetRestore>> {
    let conn = open_sqlcipher(backup_path, key)?;
    let result = stage_asset_blobs_from_connection(&conn, key, snapshot_dir);
    let close_result = conn
        .close()
        .map_err(|(_, e)| Error::storage(format!("close backup after asset staging: {e}")));
    match (result, close_result) {
        (Ok(staged), Ok(())) => Ok(staged),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

/// Drop the backup-only asset table from a restored DB file before it becomes
/// the live tenant database.
pub fn strip_asset_blobs_from_backup_db(db_path: &Path, key: &KeyMaterial) -> Result<()> {
    let conn = open_sqlcipher(db_path, key)?;
    let result = if table_exists(&conn, ASSET_BACKUP_TABLE)? {
        conn.execute_batch(&format!("DROP TABLE {ASSET_BACKUP_TABLE};"))
            .map_err(|e| Error::storage(format!("drop {ASSET_BACKUP_TABLE}: {e}")))
            .and_then(|()| checkpoint_backup_db(&conn))
    } else {
        checkpoint_backup_db(&conn)
    };
    let close_result = conn
        .close()
        .map_err(|(_, e)| Error::storage(format!("close restored DB after asset strip: {e}")));
    match (result, close_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e),
    }
}

fn package_asset_blobs_into_connection(
    conn: &Connection,
    key: &KeyMaterial,
    snapshot_dir: &Path,
) -> Result<AssetBackupReport> {
    if !table_exists(conn, "assets")? {
        return Ok(AssetBackupReport::default());
    }
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS {ASSET_BACKUP_TABLE};
         CREATE TABLE {ASSET_BACKUP_TABLE} (
             storage_path TEXT PRIMARY KEY NOT NULL,
             sha256 TEXT NOT NULL,
             size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
             encryption_alg TEXT NOT NULL DEFAULT 'none',
             backup_version INTEGER NOT NULL,
             content BLOB NOT NULL
         );"
    ))
    .map_err(|e| Error::storage(format!("create {ASSET_BACKUP_TABLE}: {e}")))?;

    let mut stmt = conn
        .prepare(
            "SELECT storage_path, sha256, size_bytes, encryption_alg,
                    encryption_nonce, encrypted_size_bytes
               FROM assets
              WHERE status <> 'deleted'
              ORDER BY storage_path",
        )
        .map_err(|e| Error::storage(format!("prepare active assets for backup: {e}")))?;
    let asset_rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<Vec<u8>>>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })
        .map_err(|e| Error::storage(format!("query active assets for backup: {e}")))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| Error::storage(format!("read active assets for backup: {e}")))?;
    drop(stmt);

    let mut report = AssetBackupReport::default();
    for (
        storage_path,
        sha256,
        plaintext_size_bytes,
        encryption_alg,
        encryption_nonce,
        encrypted_size_bytes,
    ) in asset_rows
    {
        let sha256 = normalize_sha256_hex(&sha256)?;
        let plaintext_size = nonnegative_i64_to_u64(plaintext_size_bytes, "asset size_bytes")?;
        let encrypted_size = encrypted_size_bytes
            .map(|value| nonnegative_i64_to_u64(value, "asset encrypted_size_bytes"))
            .transpose()?;
        let expected_stored_size =
            expected_stored_size(&encryption_alg, plaintext_size, encrypted_size)?;
        if expected_stored_size > i32::MAX as u64 {
            return Err(Error::invalid_input(format!(
                "asset {storage_path} is too large for SQLite zeroblob packaging: {expected_stored_size} bytes"
            )));
        }
        let blob_path = safe_asset_storage_path(snapshot_dir, &storage_path, &sha256)?;
        let blob_meta = std::fs::symlink_metadata(&blob_path)
            .map_err(|e| Error::storage(format!("stat asset blob {}: {e}", blob_path.display())))?;
        if !blob_meta.file_type().is_file() {
            return Err(Error::not_found(format!(
                "active asset blob is not a regular file during backup: {}",
                blob_path.display()
            )));
        }
        let actual_size = blob_meta.len();
        if actual_size != expected_stored_size {
            return Err(Error::storage(format!(
                "asset blob size mismatch for {}: metadata says {expected_stored_size}, file is {actual_size}",
                blob_path.display()
            )));
        }
        verify_stored_asset_blob_file(
            key,
            &blob_path,
            &encryption_alg,
            encryption_nonce.as_deref(),
            &sha256,
            plaintext_size,
            expected_stored_size,
        )?;
        conn.execute(
            &format!(
                "INSERT INTO {ASSET_BACKUP_TABLE} (
                    storage_path, sha256, size_bytes, encryption_alg, backup_version, content
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            params![
                &storage_path,
                &sha256,
                expected_stored_size as i64,
                &encryption_alg,
                ASSET_BACKUP_VERSION,
                ZeroBlob(expected_stored_size as i32),
            ],
        )
        .map_err(|e| Error::storage(format!("insert asset backup row {storage_path}: {e}")))?;
        let rowid = conn.last_insert_rowid();
        copy_file_into_blob(
            conn,
            rowid,
            &blob_path,
            &sha256,
            expected_stored_size,
            encryption_alg == ASSET_BLOB_PLAINTEXT_ALG,
        )?;
        report.asset_files = report.asset_files.saturating_add(1);
        report.asset_bytes = report.asset_bytes.saturating_add(expected_stored_size);
    }
    Ok(report)
}

fn stage_asset_blobs_from_connection(
    conn: &Connection,
    key: &KeyMaterial,
    snapshot_dir: &Path,
) -> Result<Option<StagedAssetRestore>> {
    if !table_exists(conn, ASSET_BACKUP_TABLE)? {
        return Ok(None);
    }
    let staging_parent = snapshot_dir.join(format!(
        ".assets-restore-{}",
        uuid::Uuid::now_v7().as_simple()
    ));
    let staging_assets = staging_parent.join("assets");
    std::fs::create_dir_all(&staging_assets).map_err(|e| {
        Error::storage(format!(
            "create asset restore staging {}: {e}",
            staging_assets.display()
        ))
    })?;

    let result = (|| {
        let has_encryption_alg = table_column_exists(conn, ASSET_BACKUP_TABLE, "encryption_alg")?;
        let select_sql = if has_encryption_alg {
            format!(
                "SELECT rowid, storage_path, sha256, size_bytes, encryption_alg
                   FROM {ASSET_BACKUP_TABLE}
                  ORDER BY storage_path"
            )
        } else {
            format!(
                "SELECT rowid, storage_path, sha256, size_bytes, 'none' AS encryption_alg
                   FROM {ASSET_BACKUP_TABLE}
                  ORDER BY storage_path"
            )
        };
        let mut stmt = conn
            .prepare(&select_sql)
            .map_err(|e| Error::storage(format!("prepare backup asset rows: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })
            .map_err(|e| Error::storage(format!("query backup asset rows: {e}")))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::storage(format!("read backup asset rows: {e}")))?;
        drop(stmt);

        let mut report = AssetBackupReport::default();
        for (rowid, storage_path, sha256, size_bytes, encryption_alg) in rows {
            let sha256 = normalize_sha256_hex(&sha256)?;
            let expected_size = nonnegative_i64_to_u64(size_bytes, "backup asset size_bytes")?;
            let final_path = safe_asset_storage_path(&staging_parent, &storage_path, &sha256)?;
            if let Some(parent) = final_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::storage(format!(
                        "create restored asset dir {}: {e}",
                        parent.display()
                    ))
                })?;
            }
            copy_blob_to_file(
                conn,
                rowid,
                &final_path,
                &sha256,
                expected_size,
                encryption_alg == ASSET_BLOB_PLAINTEXT_ALG,
            )?;
            if encryption_alg != ASSET_BLOB_PLAINTEXT_ALG {
                let (plaintext_size, encryption_nonce) =
                    asset_plaintext_metadata_for_restore(conn, &storage_path, &sha256)?;
                verify_stored_asset_blob_file(
                    key,
                    &final_path,
                    &encryption_alg,
                    encryption_nonce.as_deref(),
                    &sha256,
                    plaintext_size,
                    expected_size,
                )?;
            }
            report.asset_files = report.asset_files.saturating_add(1);
            report.asset_bytes = report.asset_bytes.saturating_add(expected_size);
        }
        Ok(StagedAssetRestore {
            staging_parent: staging_parent.clone(),
            staged_assets: staging_assets.clone(),
            report,
        })
    })();

    match result {
        Ok(staged) => Ok(Some(staged)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging_parent);
            Err(e)
        }
    }
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
            params![table],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| Error::storage(format!("lookup table {table}: {e}")))?
        .is_some();
    Ok(exists)
}

fn table_column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| Error::storage(format!("prepare table_info({table}): {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| Error::storage(format!("query table_info({table}): {e}")))?;
    for row in rows {
        let name = row.map_err(|e| Error::storage(format!("read table_info({table}): {e}")))?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn checkpoint_backup_db(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
        .map_err(|e| Error::storage(format!("checkpoint backup database: {e}")))
}

fn verify_stored_asset_blob_file(
    key: &KeyMaterial,
    path: &Path,
    encryption_alg: &str,
    encryption_nonce: Option<&[u8]>,
    plaintext_sha256: &str,
    plaintext_size_bytes: u64,
    expected_stored_size: u64,
) -> Result<()> {
    let stored_bytes = std::fs::read(path)
        .map_err(|e| Error::storage(format!("read asset blob {}: {e}", path.display())))?;
    if stored_bytes.len() as u64 != expected_stored_size {
        return Err(Error::storage(format!(
            "asset blob size mismatch for {}: metadata says {expected_stored_size}, file is {}",
            path.display(),
            stored_bytes.len()
        )));
    }
    decode_asset_blob(
        Some(key),
        encryption_alg,
        encryption_nonce,
        plaintext_sha256,
        plaintext_size_bytes,
        &stored_bytes,
    )
    .map(|_| ())
}

fn asset_plaintext_metadata_for_restore(
    conn: &Connection,
    storage_path: &str,
    sha256: &str,
) -> Result<(u64, Option<Vec<u8>>)> {
    if !table_exists(conn, "assets")? || !table_column_exists(conn, "assets", "encryption_nonce")? {
        return Err(Error::storage(format!(
            "encrypted backup asset {storage_path} is missing asset encryption metadata"
        )));
    }
    let row = conn
        .query_row(
            "SELECT size_bytes, encryption_nonce
               FROM assets
              WHERE storage_path = ?1 AND sha256 = ?2
              LIMIT 1",
            params![storage_path, sha256],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?)),
        )
        .optional()
        .map_err(|e| Error::storage(format!("read asset metadata for restore: {e}")))?
        .ok_or_else(|| {
            Error::storage(format!(
                "encrypted backup asset {storage_path} has no matching assets row"
            ))
        })?;
    Ok((nonnegative_i64_to_u64(row.0, "asset size_bytes")?, row.1))
}

fn copy_file_into_blob(
    conn: &Connection,
    rowid: i64,
    path: &Path,
    expected_sha256: &str,
    expected_size: u64,
    verify_sha256: bool,
) -> Result<()> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::storage(format!("open asset blob {}: {e}", path.display())))?;
    let mut blob = conn
        .blob_open(MAIN_DB, ASSET_BACKUP_TABLE, "content", rowid, false)
        .map_err(|e| Error::storage(format!("open backup blob row {rowid}: {e}")))?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| Error::storage(format!("read asset blob {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        blob.write_all(&buf[..n])
            .map_err(|e| Error::storage(format!("write backup blob row {rowid}: {e}")))?;
        hasher.update(&buf[..n]);
        copied = copied.saturating_add(n as u64);
    }
    if copied != expected_size {
        return Err(Error::storage(format!(
            "asset blob copy size mismatch for {}: expected {expected_size}, copied {copied}",
            path.display()
        )));
    }
    if verify_sha256 {
        let actual_sha = hex::encode(hasher.finalize());
        if actual_sha != expected_sha256 {
            return Err(Error::storage(format!(
                "asset blob sha256 mismatch for {}: expected {expected_sha256}, got {actual_sha}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn copy_blob_to_file(
    conn: &Connection,
    rowid: i64,
    path: &Path,
    expected_sha256: &str,
    expected_size: u64,
    verify_sha256: bool,
) -> Result<()> {
    use sha2::{Digest, Sha256};

    let mut blob = conn
        .blob_open(MAIN_DB, ASSET_BACKUP_TABLE, "content", rowid, true)
        .map_err(|e| Error::storage(format!("open backup blob row {rowid}: {e}")))?;
    if blob.len() as u64 != expected_size {
        return Err(Error::storage(format!(
            "backup blob row {rowid} size mismatch: expected {expected_size}, got {}",
            blob.len()
        )));
    }
    let mut file = std::fs::File::create(path)
        .map_err(|e| Error::storage(format!("create restored asset {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = blob
            .read(&mut buf)
            .map_err(|e| Error::storage(format!("read backup blob row {rowid}: {e}")))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .map_err(|e| Error::storage(format!("write restored asset {}: {e}", path.display())))?;
        hasher.update(&buf[..n]);
        copied = copied.saturating_add(n as u64);
    }
    file.sync_all()
        .map_err(|e| Error::storage(format!("fsync restored asset {}: {e}", path.display())))?;
    if copied != expected_size {
        return Err(Error::storage(format!(
            "restored asset size mismatch for {}: expected {expected_size}, copied {copied}",
            path.display()
        )));
    }
    if verify_sha256 {
        let actual_sha = hex::encode(hasher.finalize());
        if actual_sha != expected_sha256 {
            return Err(Error::storage(format!(
                "restored asset sha256 mismatch for {}: expected {expected_sha256}, got {actual_sha}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn replace_assets_dir(snapshot_dir: &Path, new_assets_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(snapshot_dir).map_err(|e| {
        Error::storage(format!(
            "create library snapshot dir {}: {e}",
            snapshot_dir.display()
        ))
    })?;
    let live_assets = snapshot_dir.join("assets");
    let old_assets = snapshot_dir.join(format!(".assets-old-{}", uuid::Uuid::now_v7().as_simple()));
    let had_old = live_assets.exists();
    if had_old {
        std::fs::rename(&live_assets, &old_assets).map_err(|e| {
            Error::storage(format!(
                "move existing asset dir {} aside to {}: {e}",
                live_assets.display(),
                old_assets.display()
            ))
        })?;
    }
    match std::fs::rename(new_assets_dir, &live_assets) {
        Ok(()) => {
            if had_old {
                let _ = std::fs::remove_dir_all(&old_assets);
            }
            Ok(())
        }
        Err(e) => {
            if had_old {
                let _ = std::fs::rename(&old_assets, &live_assets);
            }
            Err(Error::storage(format!(
                "promote restored asset dir {} to {}: {e}",
                new_assets_dir.display(),
                live_assets.display()
            )))
        }
    }
}

fn safe_asset_storage_path(
    snapshot_dir: &Path,
    storage_path: &str,
    expected_sha256: &str,
) -> Result<PathBuf> {
    let rel = Path::new(storage_path);
    if rel.is_absolute() {
        return Err(Error::storage(format!(
            "asset storage_path must be relative: {storage_path:?}"
        )));
    }
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    Error::storage(format!(
                        "asset storage_path component must be UTF-8: {storage_path:?}"
                    ))
                })?;
                parts.push(part);
            }
            _ => {
                return Err(Error::storage(format!(
                    "asset storage_path must contain only normal relative components: {storage_path:?}"
                )));
            }
        }
    }
    if parts.len() != 4 || parts[0] != "assets" || parts[1] != "blobs" {
        return Err(Error::storage(format!(
            "asset storage_path must use assets/blobs/<prefix>/<sha256>: {storage_path:?}"
        )));
    }
    let sha256 = normalize_sha256_hex(expected_sha256)?;
    if parts[2].len() != 2 || parts[2] != &sha256[..2] || parts[3] != sha256 {
        return Err(Error::storage(format!(
            "asset storage_path must match sha256: {storage_path:?}"
        )));
    }
    Ok(snapshot_dir.join(rel))
}

fn normalize_sha256_hex(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid_input(
            "sha256 must be 64 lowercase or uppercase hex characters",
        ));
    }
    Ok(value)
}

fn nonnegative_i64_to_u64(value: i64, label: &str) -> Result<u64> {
    if value < 0 {
        return Err(Error::storage(format!("{label} must be non-negative")));
    }
    Ok(value as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmbedderConfig, SoloConfig};
    use crate::init::{InitParams, init};
    use tempfile::TempDir;
    use zeroize::Zeroizing;

    fn fresh_init(dir: &Path, passphrase: &str) -> SoloConfig {
        let outcome = init(InitParams {
            data_dir: dir.to_path_buf(),
            passphrase: Zeroizing::new(passphrase.to_string()),
            force: false,
            embedder: EmbedderConfig {
                name: "test-embedder".into(),
                version: "v1".into(),
                dim: 1024,
                dtype: "f32".into(),
            },
        })
        .expect("init");
        SoloConfig::read(&outcome.config_path).expect("read config")
    }

    fn default_tenant_db(data_dir: &Path) -> PathBuf {
        data_dir.join("tenants").join("default.db")
    }

    #[test]
    fn temp_backup_path_is_hidden_sibling() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("solo-backup.db");

        let temp = backup_temp_path(&dest).expect("temp backup path");

        assert_eq!(temp.parent(), Some(dir.path()));
        let name = temp.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with(".solo-backup.db."));
        assert!(name.ends_with(".tmp"));
    }

    #[test]
    fn replace_with_completed_backup_replaces_existing_file() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("solo-backup.db");
        let temp = backup_temp_path(&dest).expect("temp backup path");
        std::fs::write(&dest, b"old backup").unwrap();
        std::fs::write(&temp, b"new backup").unwrap();

        replace_with_completed_backup(&temp, &dest).expect("replace backup");

        assert_eq!(std::fs::read(&dest).unwrap(), b"new backup");
        assert!(!temp.exists());
    }

    #[test]
    fn replace_with_missing_temp_preserves_existing_file() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("solo-backup.db");
        let temp = backup_temp_path(&dest).expect("temp backup path");
        std::fs::write(&dest, b"old backup").unwrap();

        let err = replace_with_completed_backup(&temp, &dest).expect_err("missing temp rejected");

        assert!(err.to_string().contains("temporary backup"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"old backup");
    }

    #[test]
    #[ignore = "requires SQLCipher: under plain bundled SQLite, PRAGMA key is a no-op so wrong keys silently succeed. Run with the workspace's bundled-sqlcipher-vendored-openssl feature: `cargo test -p solo-storage -- --include-ignored`"]
    fn backup_round_trip_preserves_database() {
        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let passphrase = "round-trip test passphrase";

        let cfg = fresh_init(src_dir.path(), passphrase);
        let salt = cfg.salt_bytes().unwrap();
        let key = KeyMaterial::derive(passphrase, &salt).unwrap();

        // Insert a sentinel row so we can verify the backup carried
        // it across.
        {
            let conn = open_sqlcipher(&default_tenant_db(src_dir.path()), &key).unwrap();
            conn.execute(
                "INSERT INTO episodes (memory_id, ts_ms, source_type, content,
                                       encoding_context_json, status, tier,
                                       confidence, strength, salience,
                                       created_at_ms, updated_at_ms)
                 VALUES (?, ?, 'test', 'sentinel', '{}', 'active', 'hot',
                         0.9, 0.5, 0.5, ?, ?)",
                rusqlite::params!["01900000-0000-7000-8000-000000000001", 0i64, 0i64, 0i64],
            )
            .expect("insert sentinel");
        }

        // Run the backup.
        let dest_path = dest_dir.path().join("solo-backup.db");
        backup_database(&default_tenant_db(src_dir.path()), &dest_path, &key)
            .expect("backup_database");

        // Open the backup with the SAME key — should succeed and the
        // sentinel row should be present.
        let dst = open_sqlcipher(&dest_path, &key).expect("open backup with same key");
        let row_count: i64 = dst
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE memory_id = ?",
                rusqlite::params!["01900000-0000-7000-8000-000000000001"],
                |row| row.get(0),
            )
            .expect("query backup");
        assert_eq!(row_count, 1, "sentinel row should be present in backup");

        // Opening with a DIFFERENT key should fail (wrong-key →
        // SQLCipher refuses to decrypt the header).
        let bad_key = KeyMaterial::derive("WRONG PASSPHRASE", &salt).unwrap();
        let bad_open = open_sqlcipher(&dest_path, &bad_key);
        assert!(
            bad_open.is_err(),
            "opening backup with wrong key should fail"
        );
    }

    #[test]
    #[ignore = "requires SQLCipher (see backup_round_trip_preserves_database)"]
    fn hot_backup_via_writer_round_trip() {
        // Daemon-side hot backup path: writer is alive, backup runs
        // through `WriteHandle::backup` against the writer's existing
        // connection.
        use crate::embedder::StubEmbedder;
        use crate::embedder_registry::get_or_insert_embedder_id;
        use crate::vector_index::HnswIndex;
        use crate::writer::{WriterActor, WriterSpawn};
        use std::sync::Arc;

        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let passphrase = "hot-backup test passphrase";

        let cfg = fresh_init(src_dir.path(), passphrase);
        let salt = cfg.salt_bytes().unwrap();
        let key = KeyMaterial::derive(passphrase, &salt).unwrap();

        // Insert a sentinel so we can verify it traveled.
        {
            let conn = open_sqlcipher(&default_tenant_db(src_dir.path()), &key).unwrap();
            conn.execute(
                "INSERT INTO episodes (memory_id, ts_ms, source_type, content,
                                       encoding_context_json, status, tier,
                                       confidence, strength, salience,
                                       created_at_ms, updated_at_ms)
                 VALUES (?, ?, 'test', 'hot-sentinel', '{}', 'active', 'hot',
                         0.9, 0.5, 0.5, ?, ?)",
                rusqlite::params!["01900000-0000-7000-8000-000000000002", 0i64, 0i64, 0i64],
            )
            .unwrap();
        }

        // Spawn a key-aware writer.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let conn = open_sqlcipher(&default_tenant_db(src_dir.path()), &key).unwrap();
            let mut conn_for_id = open_sqlcipher(&default_tenant_db(src_dir.path()), &key).unwrap();
            let identity = crate::embedder_registry::EmbedderIdentity {
                name: cfg.embedder.name.clone(),
                version: cfg.embedder.version.clone(),
                dim: cfg.embedder.dim,
                dtype: cfg.embedder.dtype.clone(),
            };
            let embedder_id = get_or_insert_embedder_id(&mut conn_for_id, &identity).unwrap();
            drop(conn_for_id);
            let hnsw = Arc::new(HnswIndex::new(
                cfg.embedder.dim as usize,
                crate::vector_index::HnswParams::default(),
            ));
            let embedder: Arc<dyn solo_core::Embedder> = Arc::new(StubEmbedder::new(
                &cfg.embedder.name,
                &cfg.embedder.version,
                cfg.embedder.dim as usize,
            ));

            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_key_and_optional_steward(
                    conn,
                    hnsw,
                    src_dir.path().to_path_buf(),
                    embedder_id,
                    embedder,
                    None,
                    key.clone(),
                );

            let dest_path = dest_dir.path().join("solo-hot-backup.db");
            handle.backup(dest_path.clone()).await.expect("hot backup");

            // Drop handle, wait for writer thread to settle.
            drop(handle);
            tokio::task::spawn_blocking(move || join.join().ok())
                .await
                .ok();

            // Open backup with the same key and verify the sentinel.
            let dst = open_sqlcipher(&dest_path, &key).unwrap();
            let n: i64 = dst
                .query_row(
                    "SELECT COUNT(*) FROM episodes WHERE memory_id = ?",
                    rusqlite::params!["01900000-0000-7000-8000-000000000002"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "hot-backup sentinel should be present");
        });
    }

    #[test]
    #[ignore = "requires SQLCipher (see backup_round_trip_preserves_database)"]
    fn backup_to_same_file_as_source_refused() {
        // Pre-flight check: if `to` resolves to the same file as the
        // live `solo.db`, refuse with InvalidInput (HTTP-layer 400).
        // SQLite's online backup is undefined behavior in this case —
        // the safety check exists so a careless config doesn't corrupt
        // the source.
        let src_dir = TempDir::new().unwrap();
        let passphrase = "same-file refusal test";

        let cfg = fresh_init(src_dir.path(), passphrase);
        let salt = cfg.salt_bytes().unwrap();
        let key = KeyMaterial::derive(passphrase, &salt).unwrap();

        let live_db = default_tenant_db(src_dir.path());
        let result = backup_database(&live_db, &live_db, &key);
        let err = result.expect_err("must refuse same-file backup");
        let msg = err.to_string();
        assert!(
            msg.contains("same file") && msg.contains("refusing"),
            "error should explain why: got `{msg}`"
        );

        // Also catches the Path-equivalence case with redundant
        // separators / `.` segments. Canonicalisation handles this.
        let live_db_alt = src_dir.path().join("tenants").join("./default.db");
        let result2 = backup_database(&live_db, &live_db_alt, &key);
        assert!(
            result2.is_err(),
            "redundant ./ in dest path should still be caught"
        );
    }

    #[test]
    #[ignore = "requires SQLCipher (see backup_round_trip_preserves_database)"]
    fn backup_with_wrong_source_key_fails() {
        let src_dir = TempDir::new().unwrap();
        let dest_dir = TempDir::new().unwrap();
        let passphrase = "real passphrase";

        let cfg = fresh_init(src_dir.path(), passphrase);
        let salt = cfg.salt_bytes().unwrap();
        let wrong_key = KeyMaterial::derive("not the real one", &salt).unwrap();

        let dest_path = dest_dir.path().join("solo-backup.db");
        let result = backup_database(&default_tenant_db(src_dir.path()), &dest_path, &wrong_key);
        assert!(
            result.is_err(),
            "backup with wrong source key should fail at open"
        );
    }
}
