// SPDX-License-Identifier: Apache-2.0

//! Community Memory Library SQLCipher backup + restore.
//!
//! These build on the existing `crate::backup` online-backup primitive
//! (which uses SQLite's `Backup::run_to_completion`) but operate on a
//! the whole library + emit admin-audit rows. The CLI / HTTP front-ends
//! drive these — daemon-side hot backup uses the writer-actor's
//! `WriteCommand::Backup` path, not this one.
//!
//! ## Backup
//!
//! `backup_library` writes
//! `<out>/.solo-backup.<RFC3339-ts>.db` using the SQLite
//! online backup API (page-level snapshot, safe against an active
//! writer). On success the destination file is encrypted with the same
//! key as the source. Retained original-file assets from the library
//! snapshot dir are embedded into a backup-only table in that same
//! encrypted DB, then the output is verified by re-opening it with the
//! same key and running `PRAGMA integrity_check`.
//!
//! ## Restore
//!
//! `restore_library` opens the supplied path with the library key — a wrong
//! key fails immediately with a clear error
//! (rather than a corrupt restore). Refuses to overwrite an existing
//! tenant DB unless `force == true`. The DB and retained assets are
//! prepared in staging locations first; the final DB swap keeps the
//! previous DB aside until asset promotion succeeds, so an asset
//! promotion failure can roll the DB back.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use solo_core::{Error, Result};

use crate::audit::{AuditOperation, AuditResult, insert_audit_admin_row};
use crate::backup::{
    backup_database, package_asset_blobs_into_backup, stage_asset_blobs_from_backup,
    strip_asset_blobs_from_backup_db,
};
use crate::init::open_sqlcipher;
use crate::key_material::KeyMaterial;

/// Outcome of `backup_library`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    /// Final path the backup file was written to.
    pub path: PathBuf,
    /// Bytes written to that path.
    pub bytes_written: u64,
    /// Number of retained original-file assets embedded into the backup.
    pub asset_files_written: u32,
    /// Raw retained-asset bytes embedded into the backup.
    pub asset_bytes_written: u64,
    /// Did `PRAGMA integrity_check` against the backup file return
    /// `'ok'`? Always `true` on `Ok` returns — a `false` here would
    /// have been surfaced as `Err`. Field is kept for callers that
    /// want to log it explicitly.
    pub integrity_ok: bool,
    /// `audit_id` of the row written to
    /// `audit_events_admin` (`operation='library.backup'`).
    pub audit_admin_row_id: i64,
}

/// Outcome of `restore_library`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    /// Path the restore was sourced from.
    pub from: PathBuf,
    /// Bytes copied into the destination.
    pub bytes_restored: u64,
    /// Number of retained original-file assets restored from the backup.
    pub asset_files_restored: u32,
    /// Raw retained-asset bytes restored from the backup.
    pub asset_bytes_restored: u64,
    /// `audit_id` of the row written to
    /// `audit_events_admin` (`operation='library.restore'`).
    pub audit_admin_row_id: i64,
}

/// Online-encrypted backup of the Community Memory Library SQLCipher DB.
///
/// Returns the absolute path of the written file, the bytes written,
/// and the admin-audit row id. The output is `<out>/.solo-backup.<ts>.db`
/// where `<ts>` is `chrono::Utc::now().format("%Y%m%dT%H%M%SZ")` (an
/// RFC3339-ish form safe for filesystem paths on every OS).
///
/// `out` must be an existing directory. Errors with `Error::Storage`
/// if it isn't.
pub fn backup_library(
    db_path: &Path,
    out: &Path,
    key: &KeyMaterial,
    data_dir: &Path,
) -> Result<BackupReport> {
    if !out.is_dir() {
        return Err(Error::invalid_input(format!(
            "backup output directory does not exist: {}",
            out.display()
        )));
    }
    if !db_path.is_file() {
        return Err(Error::not_found(format!(
            "Memory Library database to back up not found: {}",
            db_path.display()
        )));
    }

    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let filename = format!(".solo-backup.{stamp}.db");
    let target = out.join(&filename);

    // Defense in depth: refuse to write the backup over the source.
    // `backup_database` enforces this internally too, but doing the
    // check up-front gives the operator a cleaner error message before
    // any I/O.
    if crate::backup::paths_refer_to_same_file(db_path, &target) {
        return Err(Error::invalid_input(format!(
            "backup target {} resolves to the source DB; refusing",
            target.display()
        )));
    }
    // Refuse to overwrite. `--force`-style overwrite is the caller's
    // problem (the CLI subcommand asks the operator before passing a
    // path that already exists).
    if target.exists() {
        return Err(Error::conflict(format!(
            "backup target {} already exists; choose a different out dir or remove the file first",
            target.display()
        )));
    }

    backup_database(db_path, &target, key)?;
    let snapshot_dir = library_snapshot_dir(data_dir);
    let asset_report =
        package_asset_blobs_into_backup(&target, key, &snapshot_dir).inspect_err(|_e| {
            let _ = std::fs::remove_file(&target);
        })?;

    // Verify: open the backup with the source's key + integrity_check.
    let verify_conn = open_sqlcipher(&target, key)?;
    verify_integrity(&verify_conn).inspect_err(|_e| {
        // Drop the corrupt backup file so the operator isn't tempted
        // to restore from it later. Best-effort.
        let _ = std::fs::remove_file(&target);
    })?;
    drop(verify_conn);

    let bytes_written = std::fs::metadata(&target)
        .map_err(|e| Error::storage(format!("stat backup file {}: {e}", target.display())))?
        .len();

    // Admin-audit emit to the Community database.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let admin_path = data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME);
    let admin_conn = open_sqlcipher(&admin_path, key)?;
    let details = serde_json::json!({
        "path": target.display().to_string(),
        "bytes": bytes_written,
        "asset_files": asset_report.asset_files,
        "asset_bytes": asset_report.asset_bytes,
    });
    // Community keeps administrative history in the Memory Library itself.
    // Copy the successful backup event into the backup image as well so a
    // later restore does not erase the fact that the backup occurred.
    let backup_audit_conn = open_sqlcipher(&target, key)?;
    insert_audit_admin_row(
        &backup_audit_conn,
        now_ms,
        None,
        AuditOperation::LibraryBackup,
        None,
        AuditResult::Ok,
        Some(&details),
    )?;
    let audit_admin_row_id = insert_audit_admin_row(
        &admin_conn,
        now_ms,
        None,
        AuditOperation::LibraryBackup,
        None,
        AuditResult::Ok,
        Some(&details),
    )?;

    Ok(BackupReport {
        path: target,
        bytes_written,
        asset_files_written: asset_report.asset_files,
        asset_bytes_written: asset_report.asset_bytes,
        integrity_ok: true,
        audit_admin_row_id,
    })
}

/// Restore the Community SQLCipher DB from a backup file produced by
/// `backup_library` (or another SQLCipher backup encrypted with the
/// destination library key).
///
/// `dest_db_path` is the live Community database path inside
/// the data dir. On success the DB staging file is promoted and embedded
/// asset blobs, when present, are extracted into the tenant asset store.
/// On wrong-key / integrity failure the destination is left untouched.
///
/// `force == true` allows overwriting an existing destination file.
/// `force == false` refuses with `Error::Conflict`.
pub fn restore_library(
    from: &Path,
    dest_db_path: &Path,
    key: &KeyMaterial,
    data_dir: &Path,
    force: bool,
) -> Result<RestoreReport> {
    if !from.is_file() {
        return Err(Error::not_found(format!(
            "restore source not found: {}",
            from.display()
        )));
    }

    // Key check: open the source with the destination library key.
    // `open_sqlcipher`'s `PRAGMA journal_mode = wal` forces decryption
    // so a wrong key surfaces here, BEFORE any swap.
    let src_conn = open_sqlcipher(from, key).map_err(|_| {
        Error::invalid_input(format!(
            "restore: source {} fails to decrypt under the destination library key; \
             refusing to restore",
            from.display()
        ))
    })?;
    verify_integrity(&src_conn)?;
    drop(src_conn);

    if dest_db_path.exists() && !force {
        return Err(Error::conflict(format!(
            "destination {} exists; pass --confirm to overwrite",
            dest_db_path.display()
        )));
    }
    // Write to `<dest>.new`, fsync, rename. SQLite's online backup
    // gives us a clean point-in-time copy regardless of source state
    // (page-level snapshot). For the restore swap we just want
    // atomicity of the file replacement.
    let staging = staging_path(dest_db_path);
    if staging.exists() {
        std::fs::remove_file(&staging).map_err(|e| {
            Error::storage(format!(
                "remove pre-existing staging file {}: {e}",
                staging.display()
            ))
        })?;
    }
    std::fs::copy(from, &staging).map_err(|e| {
        Error::storage(format!(
            "copy {} → {}: {e}",
            from.display(),
            staging.display()
        ))
    })?;
    strip_asset_blobs_from_backup_db(&staging, key)?;

    let snapshot_dir = library_snapshot_dir(data_dir);
    let staged_assets = stage_asset_blobs_from_backup(from, key, &snapshot_dir)?;
    let asset_report = staged_assets
        .as_ref()
        .map(|staged| staged.report())
        .unwrap_or_default();

    // fsync the staging file so it's durable on disk before we swap.
    // On Windows, `sync_all` requires the file be opened with write
    // access (FILE_GENERIC_WRITE for FlushFileBuffers). Open
    // `read=true, write=true` rather than `append=true` so we don't
    // mutate the staging contents — `sync_all` only flushes the
    // existing buffer.
    {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&staging)
            .map_err(|e| Error::storage(format!("open staging for fsync: {e}")))?;
        f.sync_all()
            .map_err(|e| Error::storage(format!("fsync staging: {e}")))?;
    }

    let retired_db = promote_staged_db(&staging, dest_db_path)?;
    if let Some(staged_assets) = staged_assets {
        if let Err(asset_err) = staged_assets.promote(&snapshot_dir) {
            let rollback_result = rollback_promoted_db(dest_db_path, &retired_db);
            let rollback_note = match rollback_result {
                Ok(()) => "rolled back database restore".to_string(),
                Err(rollback_err) => format!("database rollback also failed: {rollback_err}"),
            };
            return Err(Error::storage(format!(
                "restore asset blobs after database swap failed: {asset_err}; {rollback_note}"
            )));
        }
    }
    retired_db.remove();

    let bytes_restored = std::fs::metadata(dest_db_path)
        .map_err(|e| Error::storage(format!("stat dest after restore: {e}")))?
        .len();

    // Admin-audit emit.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let admin_path = data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME);
    let admin_conn = open_sqlcipher(&admin_path, key)?;
    let details = serde_json::json!({
        "from": from.display().to_string(),
        "bytes_restored": bytes_restored,
        "asset_files": asset_report.asset_files,
        "asset_bytes": asset_report.asset_bytes,
    });
    let audit_admin_row_id = insert_audit_admin_row(
        &admin_conn,
        now_ms,
        None,
        AuditOperation::LibraryRestore,
        None,
        AuditResult::Ok,
        Some(&details),
    )?;

    Ok(RestoreReport {
        from: from.to_path_buf(),
        bytes_restored,
        asset_files_restored: asset_report.asset_files,
        asset_bytes_restored: asset_report.asset_bytes,
        audit_admin_row_id,
    })
}

fn library_snapshot_dir(data_dir: &Path) -> PathBuf {
    data_dir.to_path_buf()
}

#[derive(Debug)]
struct RetiredDbFamily {
    files: Vec<(PathBuf, PathBuf)>,
}

impl RetiredDbFamily {
    fn restore(&self) -> Result<()> {
        for (live, retired) in self.files.iter().rev() {
            if retired.exists() {
                std::fs::rename(retired, live).map_err(|e| {
                    Error::storage(format!(
                        "restore previous database component {} from {}: {e}",
                        live.display(),
                        retired.display()
                    ))
                })?;
            }
        }
        Ok(())
    }

    fn remove(self) {
        for (_, retired) in self.files {
            let _ = std::fs::remove_file(retired);
        }
    }
}

fn promote_staged_db(staging: &Path, dest: &Path) -> Result<RetiredDbFamily> {
    let retired = retire_live_db_family(dest)?;
    match std::fs::rename(staging, dest) {
        Ok(()) => Ok(retired),
        Err(e) => {
            let rollback = retired.restore();
            let rollback_note = rollback.err().map_or_else(String::new, |error| {
                format!("; rollback also failed: {error}")
            });
            Err(Error::storage(format!(
                "rename {} → {}: {e}{rollback_note}",
                staging.display(),
                dest.display()
            )))
        }
    }
}

fn retire_live_db_family(dest: &Path) -> Result<RetiredDbFamily> {
    let retired_base = retired_path(dest);
    let candidates = [
        (dest.to_path_buf(), retired_base.clone()),
        (
            path_with_suffix(dest, "-wal"),
            path_with_suffix(&retired_base, "-wal"),
        ),
        (
            path_with_suffix(dest, "-shm"),
            path_with_suffix(&retired_base, "-shm"),
        ),
    ];
    let mut retired = RetiredDbFamily { files: Vec::new() };
    for (live, old) in candidates {
        if !live.exists() {
            continue;
        }
        if let Err(error) = std::fs::rename(&live, &old) {
            let rollback = retired.restore();
            let rollback_note = rollback.err().map_or_else(String::new, |error| {
                format!("; rollback also failed: {error}")
            });
            return Err(Error::storage(format!(
                "move existing database component {} aside to {}: {error}{rollback_note}",
                live.display(),
                old.display()
            )));
        }
        retired.files.push((live, old));
    }
    Ok(retired)
}

fn rollback_promoted_db(dest: &Path, retired: &RetiredDbFamily) -> Result<()> {
    if dest.exists() {
        std::fs::remove_file(dest).map_err(|e| {
            Error::storage(format!(
                "remove restored dest {} during rollback: {e}",
                dest.display()
            ))
        })?;
    }
    retired.restore()
}

fn retired_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "tenant.db".into());
    dest.with_file_name(format!(
        ".{name}.restore-old-{}",
        uuid::Uuid::now_v7().as_simple()
    ))
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().map(ToOwned::to_owned).unwrap_or_default();
    name.push(suffix);
    if let Some(parent) = path.parent() {
        parent.join(&name)
    } else {
        PathBuf::from(name)
    }
}

/// Run `PRAGMA integrity_check` and bubble a clean error if it doesn't
/// return exactly `'ok'`.
fn verify_integrity(conn: &Connection) -> Result<()> {
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| Error::storage(format!("PRAGMA integrity_check: {e}")))?;
    if result != "ok" {
        return Err(Error::storage(format!("integrity_check failed: {result}")));
    }
    Ok(())
}

/// Build the `<dest>.new` staging path. Lives in the same parent dir so DB
/// promotion does not cross filesystem boundaries.
fn staging_path(dest: &Path) -> PathBuf {
    let mut fname = dest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    fname.push(".new");
    match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(fname),
        _ => PathBuf::from(fname),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_path_is_sibling_of_dest() {
        use std::path::PathBuf;
        let dest = PathBuf::from("C:/data/tenants/default.db");
        let staged = staging_path(&dest);
        let staged_str = staged.to_string_lossy().replace('\\', "/");
        assert!(staged_str.ends_with("default.db.new"), "got `{staged_str}`");
        // Same parent.
        assert_eq!(staged.parent(), dest.parent());
    }

    #[test]
    fn database_promotion_retires_and_rolls_back_wal_and_shm() {
        let temp = tempfile::TempDir::new().unwrap();
        let dest = temp.path().join("solo.db");
        let staging = staging_path(&dest);
        let wal = path_with_suffix(&dest, "-wal");
        let shm = path_with_suffix(&dest, "-shm");
        std::fs::write(&dest, b"old-db").unwrap();
        std::fs::write(&wal, b"old-wal").unwrap();
        std::fs::write(&shm, b"old-shm").unwrap();
        std::fs::write(&staging, b"restored-db").unwrap();

        let retired = promote_staged_db(&staging, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"restored-db");
        assert!(!wal.exists());
        assert!(!shm.exists());

        rollback_promoted_db(&dest, &retired).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"old-db");
        assert_eq!(std::fs::read(&wal).unwrap(), b"old-wal");
        assert_eq!(std::fs::read(&shm).unwrap(), b"old-shm");
    }

    #[test]
    fn verify_integrity_accepts_fresh_in_memory_db() {
        let conn = Connection::open_in_memory().unwrap();
        // PRAGMA integrity_check on an empty DB returns 'ok'.
        verify_integrity(&conn).expect("empty DB must be integrity-ok");
    }

    fn seed_encrypted_asset(
        conn: &Connection,
        data_dir: &std::path::Path,
        key: &KeyMaterial,
        asset_id: &str,
        plaintext: &[u8],
    ) -> String {
        use sha2::{Digest, Sha256};

        let sha256 = hex::encode(Sha256::digest(plaintext));
        let encrypted =
            crate::asset_blob::encrypt_asset_blob(key, plaintext, &sha256, plaintext.len() as u64)
                .expect("encrypt asset fixture");
        let storage_path = format!("assets/blobs/{}/{}", &sha256[..2], sha256);
        let asset_path = library_snapshot_dir(data_dir).join(&storage_path);
        std::fs::create_dir_all(asset_path.parent().unwrap()).unwrap();
        std::fs::write(&asset_path, &encrypted.ciphertext).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO assets (
                asset_id, sha256, mime_type, filename, size_bytes,
                storage_path, source, status, created_by_principal,
                created_at_ms, updated_at_ms, encryption_alg,
                encryption_nonce, encrypted_size_bytes
             ) VALUES (?1, ?2, 'application/octet-stream', 'encrypted.bin', ?3,
                ?4, 'test', 'active', 'tester', ?5, ?5,
                'xchacha20poly1305-blake3-v1', ?6, ?7)",
            rusqlite::params![
                asset_id,
                sha256,
                plaintext.len() as i64,
                storage_path,
                now,
                encrypted.nonce,
                encrypted.ciphertext.len() as i64,
            ],
        )
        .unwrap();
        asset_path
            .strip_prefix(library_snapshot_dir(data_dir))
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    }

    #[test]
    fn backup_restore_round_trip_preserves_user_rows() {
        use crate::init::{InitParams, init};
        use rusqlite::params;
        use sha2::{Digest, Sha256};
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let out_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&out_dir).unwrap();

        let pass = "round-trip backup test";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .unwrap();
        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let salt = cfg.salt_bytes().unwrap();
        let key = KeyMaterial::derive(pass, &salt).unwrap();

        // Seed an episode so we can verify the round-trip.
        {
            let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, content,
                    encoding_context_json, confidence, strength, salience,
                    tier, created_at_ms, updated_at_ms
                 ) VALUES (?, ?, 'user_message', 'sentinel', '{}', 0.9, 0.5, 0.5,
                           'hot', ?, ?)",
                params!["01900000-0000-7000-8000-000000000001", now, now, now],
            )
            .unwrap();
            let asset_bytes = b"retained original asset";
            let asset_sha = hex::encode(Sha256::digest(asset_bytes));
            let storage_path = format!("assets/blobs/{}/{}", &asset_sha[..2], asset_sha);
            let asset_path = library_snapshot_dir(&data_dir).join(&storage_path);
            std::fs::create_dir_all(asset_path.parent().unwrap()).unwrap();
            std::fs::write(&asset_path, asset_bytes).unwrap();
            conn.execute(
                "INSERT INTO assets (
                    asset_id, sha256, mime_type, filename, size_bytes,
                    storage_path, source, status, created_by_principal,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, 'application/octet-stream', 'asset.bin', ?3,
                    ?4, 'test', 'active', 'tester', ?5, ?5)",
                params![
                    "01900000-0000-7000-8000-0000000000aa",
                    asset_sha,
                    asset_bytes.len() as i64,
                    storage_path,
                    now,
                ],
            )
            .unwrap();
        }

        let report =
            backup_library(&outcome.db_path, &out_dir, &key, &data_dir).expect("backup_library");
        assert!(report.integrity_ok);
        assert!(report.path.is_file());
        assert_eq!(report.asset_files_written, 1);
        assert_eq!(report.asset_bytes_written, 23);

        // Take a hash of the seeded row's content before destruction so
        // we can verify the round-trip preserves *data*, not bytes
        // (SQLCipher salt/IV differ across freshly-written files).
        let hash_before: String = {
            let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
            conn.query_row(
                "SELECT content FROM episodes WHERE memory_id = ?",
                params!["01900000-0000-7000-8000-000000000001"],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hash_before, "sentinel");

        // Destroy the source.
        std::fs::remove_file(&outcome.db_path).unwrap();
        std::fs::remove_dir_all(library_snapshot_dir(&data_dir).join("assets")).unwrap();
        // Restore.
        let restore_report =
            restore_library(&report.path, &outcome.db_path, &key, &data_dir, false)
                .expect("restore_library");
        assert!(restore_report.bytes_restored > 0);
        assert_eq!(restore_report.asset_files_restored, 1);
        assert_eq!(restore_report.asset_bytes_restored, 23);

        // Round-trip verification.
        let hash_after: String = {
            let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
            conn.query_row(
                "SELECT content FROM episodes WHERE memory_id = ?",
                params!["01900000-0000-7000-8000-000000000001"],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(hash_after, hash_before);
        let asset_sha = hex::encode(Sha256::digest(b"retained original asset"));
        let restored_asset_path = library_snapshot_dir(&data_dir)
            .join("assets")
            .join("blobs")
            .join(&asset_sha[..2])
            .join(&asset_sha);
        assert_eq!(
            std::fs::read(restored_asset_path).unwrap(),
            b"retained original asset"
        );
        {
            let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
            let backup_table_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'solo_backup_asset_blobs'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(backup_table_count, 0);
        }

        // Admin audit rows: one for backup, one for restore.
        let admin = crate::init::open_sqlcipher(
            &data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME),
            &key,
        )
        .unwrap();
        let n_backup: i64 = admin
            .query_row(
                "SELECT COUNT(*) FROM audit_events_admin WHERE operation = 'library.backup'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let n_restore: i64 = admin
            .query_row(
                "SELECT COUNT(*) FROM audit_events_admin WHERE operation = 'library.restore'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_backup, 1);
        assert_eq!(n_restore, 1);
    }

    #[test]
    fn backup_rejects_corrupt_encrypted_asset_sidecar() {
        use crate::init::{InitParams, init};
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let out_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&out_dir).unwrap();
        let pass = "encrypted backup corruption test";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .unwrap();
        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let key = KeyMaterial::derive(pass, &cfg.salt_bytes().unwrap()).unwrap();

        let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
        let storage_path = seed_encrypted_asset(
            &conn,
            &data_dir,
            &key,
            "01900000-0000-7000-8000-0000000000bb",
            b"encrypted retained asset",
        );
        drop(conn);
        let blob_path = library_snapshot_dir(&data_dir).join(storage_path);
        let mut corrupt = std::fs::read(&blob_path).unwrap();
        corrupt[0] ^= 0x80;
        std::fs::write(&blob_path, corrupt).unwrap();

        let err = backup_library(&outcome.db_path, &out_dir, &key, &data_dir)
            .expect_err("backup must reject corrupt encrypted sidecar");
        assert!(
            err.to_string().contains("decrypt asset blob"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn restore_rejects_corrupt_encrypted_asset_backup_blob() {
        use crate::init::{InitParams, init};
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let out_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&out_dir).unwrap();
        let pass = "encrypted restore corruption test";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .unwrap();
        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let key = KeyMaterial::derive(pass, &cfg.salt_bytes().unwrap()).unwrap();

        {
            let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
            seed_encrypted_asset(
                &conn,
                &data_dir,
                &key,
                "01900000-0000-7000-8000-0000000000cc",
                b"encrypted retained asset",
            );
        }
        let report =
            backup_library(&outcome.db_path, &out_dir, &key, &data_dir).expect("backup_library");
        {
            let conn = crate::init::open_sqlcipher(&report.path, &key).unwrap();
            let mut content: Vec<u8> = conn
                .query_row("SELECT content FROM solo_backup_asset_blobs", [], |r| {
                    r.get(0)
                })
                .unwrap();
            content[0] ^= 0x80;
            conn.execute(
                "UPDATE solo_backup_asset_blobs SET content = ?1",
                rusqlite::params![content],
            )
            .unwrap();
        }

        std::fs::remove_file(&outcome.db_path).unwrap();
        std::fs::remove_dir_all(library_snapshot_dir(&data_dir).join("assets")).unwrap();
        let err = restore_library(&report.path, &outcome.db_path, &key, &data_dir, false)
            .expect_err("restore must reject corrupt encrypted backup asset");
        assert!(
            err.to_string().contains("decrypt asset blob"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn restore_refuses_wrong_key() {
        use crate::init::{InitParams, init};
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let out_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&out_dir).unwrap();

        // Init under one passphrase.
        let pass = "right passphrase";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .unwrap();
        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let salt = cfg.salt_bytes().unwrap();
        let key = KeyMaterial::derive(pass, &salt).unwrap();

        // Backup with the real key.
        let report = backup_library(&outcome.db_path, &out_dir, &key, &data_dir).unwrap();

        // Try to restore with a wrong key.
        let wrong_key = KeyMaterial::derive("WRONG PASSPHRASE", &salt).unwrap();
        let err = restore_library(&report.path, &outcome.db_path, &wrong_key, &data_dir, true)
            .expect_err("wrong key must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("fails to decrypt") || msg.contains("key"),
            "got `{msg}`"
        );
    }

    #[test]
    fn restore_refuses_existing_destination_without_confirm() {
        use crate::init::{InitParams, init};
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().join("data");
        let out_dir = tmp.path().join("backups");
        std::fs::create_dir_all(&out_dir).unwrap();

        let pass = "existing-dest test";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .unwrap();
        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let salt = cfg.salt_bytes().unwrap();
        let key = KeyMaterial::derive(pass, &salt).unwrap();

        let report = backup_library(&outcome.db_path, &out_dir, &key, &data_dir).unwrap();

        // Destination still exists (we didn't remove the source).
        let err = restore_library(
            &report.path,
            &outcome.db_path,
            &key,
            &data_dir,
            false, // force=false
        )
        .expect_err("existing dest without confirm must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("destination") && msg.contains("exists"),
            "got `{msg}`"
        );

        // With confirm=true it succeeds.
        let r = restore_library(&report.path, &outcome.db_path, &key, &data_dir, true)
            .expect("existing dest with confirm must succeed");
        assert!(r.bytes_restored > 0);
    }

    #[test]
    fn backup_to_missing_out_dir_errors_cleanly() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let nonexistent = tmp.path().join("does-not-exist");
        let db_path = tmp.path().join("source.db");
        std::fs::write(&db_path, b"placeholder").unwrap();
        let key = KeyMaterial::derive("p", &[0u8; 16]).unwrap();

        let err = backup_library(&db_path, &nonexistent, &key, &data_dir)
            .expect_err("missing out dir must error");
        let msg = err.to_string();
        assert!(msg.contains("does not exist"), "got `{msg}`");
    }

    #[test]
    fn restore_refuses_missing_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let from = tmp.path().join("does-not-exist.db");
        let dest = tmp.path().join("dest.db");
        let key = KeyMaterial::derive("p", &[0u8; 16]).unwrap();

        let err = restore_library(&from, &dest, &key, &data_dir, false)
            .expect_err("missing source must error");
        let msg = err.to_string();
        assert!(msg.contains("not found"), "got `{msg}`");
    }
}
