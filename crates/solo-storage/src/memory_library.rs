// SPDX-License-Identifier: Apache-2.0

//! The Community edition storage composition.
//!
//! Community owns exactly one encrypted database (`solo.db`), one vector
//! index, one writer, and one reader pool. There is no registry, selector,
//! tenant lifecycle, or alternate database path in this type.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use solo_core::{Embedder, Error, Result};
use tokio::runtime::Handle as TokioHandle;
use tokio::sync::{Mutex, RwLock};

use crate::key_material::KeyMaterial;
use crate::library::{LibraryHandle, LibraryOpenParams};
use crate::steward_factory::StewardFactory;
use crate::vector_index::HnswParams;
use crate::{init::open_sqlcipher, migration};

pub const COMMUNITY_DB_FILENAME: &str = "solo.db";
const LEGACY_INDEX_FILENAME: &str = "tenants_index.db";
const LEGACY_LIBRARY_SUBDIR: &str = "tenants";
const LEGACY_DEFAULT_DB_FILENAME: &str = "default.db";

#[derive(Debug)]
struct LegacyAdminAuditRow {
    audit_id: i64,
    ts_ms: i64,
    principal_subject: Option<String>,
    operation: String,
    target_tenant_id: Option<String>,
    result: String,
    details_json: Option<String>,
}

enum LegacyIndexRetirement {
    Remove,
    Quarantine,
}

struct MemoryLibraryDeps {
    data_dir: PathBuf,
    key: KeyMaterial,
    embedder: Arc<dyn Embedder>,
    hnsw_params: HnswParams,
    steward: Option<Arc<solo_steward::Steward>>,
    runtime_handle: Option<TokioHandle>,
    steward_factory: Option<Arc<dyn StewardFactory>>,
    triples_batch_signal: Option<Arc<crate::triples_batch::TriplesBatchSignal>>,
}

/// Parameters for the one Community Memory Library.
pub struct MemoryLibraryParams {
    pub data_dir: PathBuf,
    pub key: KeyMaterial,
    pub embedder: Arc<dyn Embedder>,
    pub hnsw_params: HnswParams,
    pub steward: Option<Arc<solo_steward::Steward>>,
    pub runtime_handle: Option<TokioHandle>,
    pub steward_factory: Option<Arc<dyn StewardFactory>>,
    pub triples_batch_signal: Option<Arc<crate::triples_batch::TriplesBatchSignal>>,
}

/// Owner of the single Community Memory Library runtime.
pub struct MemoryLibrary {
    handle: RwLock<Option<Arc<LibraryHandle>>>,
    open_lock: Mutex<()>,
    deps: MemoryLibraryDeps,
}

impl MemoryLibrary {
    /// Build the Community composition. The heavy DB/index open remains lazy
    /// so construction is cheap and blocking startup work runs off-runtime.
    pub fn open(params: MemoryLibraryParams) -> Result<Self> {
        migrate_legacy_default_library(&params.data_dir, &params.key)?;
        Ok(Self {
            handle: RwLock::new(None),
            open_lock: Mutex::new(()),
            deps: MemoryLibraryDeps {
                data_dir: params.data_dir,
                key: params.key,
                embedder: params.embedder,
                hnsw_params: params.hnsw_params,
                steward: params.steward,
                runtime_handle: params.runtime_handle,
                steward_factory: params.steward_factory,
                triples_batch_signal: params.triples_batch_signal,
            },
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.deps.data_dir
    }

    /// Return the sole Community Memory Library handle.
    ///
    /// There is intentionally no selector argument. Paid editions compose
    /// additional library-management modules around Core; Community itself
    /// cannot be switched to a second database through this API.
    pub async fn handle(&self) -> Result<Arc<LibraryHandle>> {
        if let Some(handle) = self.handle.read().await.as_ref() {
            return Ok(Arc::clone(handle));
        }

        let _guard = self.open_lock.lock().await;
        if let Some(handle) = self.handle.read().await.as_ref() {
            return Ok(Arc::clone(handle));
        }

        let params = LibraryOpenParams {
            data_dir: self.deps.data_dir.clone(),
            key: self.deps.key.clone(),
            embedder: Arc::clone(&self.deps.embedder),
            hnsw_params: self.deps.hnsw_params.clone(),
            steward: self.deps.steward.clone(),
            runtime_handle: self.deps.runtime_handle.clone(),
            steward_factory: self.deps.steward_factory.clone(),
            triples_batch_signal: self.deps.triples_batch_signal.clone(),
        };
        let opened = tokio::task::spawn_blocking(move || LibraryHandle::open(params))
            .await
            .map_err(|e| Error::storage(format!("join Memory Library open task: {e}")))??;
        let opened = Arc::new(opened);
        *self.handle.write().await = Some(Arc::clone(&opened));
        Ok(opened)
    }

    pub async fn is_open(&self) -> bool {
        self.handle.read().await.is_some()
    }

    pub async fn shutdown(&self) {
        self.shutdown_with_snapshot(true).await;
    }

    pub async fn shutdown_with_snapshot(&self, save_snapshot: bool) {
        let handle = self.handle.write().await.take();
        let Some(handle) = handle else { return };
        match Arc::try_unwrap(handle) {
            Ok(handle) => {
                if let Err(error) = handle.shutdown(save_snapshot).await {
                    tracing::warn!(%error, "Community Memory Library shutdown failed");
                }
            }
            Err(_) => tracing::warn!(
                "Community Memory Library still has outstanding requests during shutdown"
            ),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(
        data_dir: PathBuf,
        key: KeyMaterial,
        embedder: Arc<dyn Embedder>,
        handle: Arc<LibraryHandle>,
    ) -> Self {
        Self {
            handle: RwLock::new(Some(handle)),
            open_lock: Mutex::new(()),
            deps: MemoryLibraryDeps {
                data_dir,
                key,
                embedder,
                hnsw_params: HnswParams::default(),
                steward: None,
                runtime_handle: None,
                steward_factory: None,
                triples_batch_signal: None,
            },
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_with_single_tenant(
        data_dir: PathBuf,
        key: KeyMaterial,
        embedder: Arc<dyn Embedder>,
        handle: Arc<LibraryHandle>,
    ) -> Self {
        Self::for_tests(data_dir, key, embedder, handle)
    }
}

/// Promote the previous single-default-library multi-tenant-era layout back
/// to the Community root layout. Any evidence of a second database makes the
/// migration stop; Community never guesses which data to discard.
pub fn migrate_legacy_default_library(data_dir: &Path, key: &KeyMaterial) -> Result<bool> {
    let root_db = data_dir.join(COMMUNITY_DB_FILENAME);
    let legacy_dir = data_dir.join(LEGACY_LIBRARY_SUBDIR);
    let legacy_default_db = legacy_dir.join(LEGACY_DEFAULT_DB_FILENAME);
    let legacy_index = data_dir.join(LEGACY_INDEX_FILENAME);

    let root_exists = root_db.is_file();
    let legacy_default_exists = legacy_default_db.is_file();
    if root_exists && legacy_default_exists {
        return Err(Error::conflict(format!(
            "both {} and {} exist; refusing an ambiguous Community migration",
            root_db.display(),
            legacy_default_db.display()
        )));
    }
    if !root_exists && !legacy_default_exists {
        let legacy_index_remnant = ["", "-wal", "-shm"]
            .iter()
            .map(|suffix| data_dir.join(format!("{LEGACY_INDEX_FILENAME}{suffix}")))
            .find(|path| path.exists());
        let legacy_library_remnant = if legacy_dir.is_dir() {
            std::fs::read_dir(&legacy_dir)
                .map_err(|e| {
                    Error::storage(format!(
                        "scan previous library directory {}: {e}",
                        legacy_dir.display()
                    ))
                })?
                .next()
                .transpose()
                .map_err(|e| Error::storage(format!("scan previous library entry: {e}")))?
                .map(|entry| entry.path())
        } else {
            None
        };
        if let Some(remnant) = legacy_index_remnant.or(legacy_library_remnant) {
            return Err(Error::conflict(format!(
                "previous Solo layout remnant {} exists but the default library is missing at {}",
                remnant.display(),
                legacy_default_db.display()
            )));
        }
        let _ = std::fs::remove_dir(&legacy_dir);
        return Ok(false);
    }

    let mut changed = legacy_default_exists;
    if legacy_dir.is_dir() {
        let mut extra_databases = Vec::new();
        for entry in std::fs::read_dir(&legacy_dir).map_err(|e| {
            Error::storage(format!(
                "scan previous library directory {}: {e}",
                legacy_dir.display()
            ))
        })? {
            let path = entry
                .map_err(|e| Error::storage(format!("scan previous library entry: {e}")))?
                .path();
            if path.is_file()
                && path.extension().is_some_and(|extension| extension == "db")
                && path
                    .file_name()
                    .is_some_and(|name| name != LEGACY_DEFAULT_DB_FILENAME)
            {
                extra_databases.push(path);
            }
        }
        if !extra_databases.is_empty() {
            let paths = extra_databases
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Error::conflict(format!(
                "Community migration found additional database files ({paths}). Export them before installing the single-library Community edition."
            )));
        }
    }

    if legacy_default_exists {
        validate_legacy_default_library(&legacy_default_db, key)?;
    }

    if legacy_default_exists {
        move_file(&legacy_default_db, &root_db)?;
    }
    for suffix in ["-wal", "-shm"] {
        let source = legacy_dir.join(format!("{LEGACY_DEFAULT_DB_FILENAME}{suffix}"));
        let destination = data_dir.join(format!("{COMMUNITY_DB_FILENAME}{suffix}"));
        if source.is_file() {
            if destination.exists() {
                return Err(Error::conflict(format!(
                    "both {} and {} exist; refusing an ambiguous Community migration",
                    source.display(),
                    destination.display()
                )));
            }
            move_file(&source, &destination)?;
            changed = true;
        }
    }

    let nested_default = legacy_dir.join("default");
    changed |= nested_default.is_dir() || legacy_dir.is_dir();
    promote_directory_contents(&nested_default, data_dir)?;
    let _ = std::fs::remove_dir(&nested_default);
    promote_directory_contents(&legacy_dir, data_dir)?;

    changed |= legacy_index.exists()
        || data_dir
            .join(format!("{LEGACY_INDEX_FILENAME}-wal"))
            .exists()
        || data_dir
            .join(format!("{LEGACY_INDEX_FILENAME}-shm"))
            .exists();
    retire_legacy_index(data_dir, key)?;
    let _ = std::fs::remove_dir(&legacy_dir);
    if changed {
        tracing::info!(data_dir = %data_dir.display(), "promoted the default library to Community solo.db");
    }
    Ok(changed)
}

fn validate_legacy_default_library(path: &Path, key: &KeyMaterial) -> Result<()> {
    let mut conn = open_sqlcipher(path, key).map_err(|e| {
        Error::storage(format!(
            "open previous default library {} before Community migration: {e}",
            path.display()
        ))
    })?;
    migration::run_migrations(&mut conn).map_err(|e| {
        Error::storage(format!(
            "migrate previous default library {} before Community promotion: {e}",
            path.display()
        ))
    })?;
    Ok(())
}

fn retire_legacy_index(data_dir: &Path, key: &KeyMaterial) -> Result<()> {
    let index_path = data_dir.join(LEGACY_INDEX_FILENAME);
    let retirement = if index_path.is_file() {
        let root_path = data_dir.join(COMMUNITY_DB_FILENAME);
        match read_legacy_admin_audit_rows(&index_path, key) {
            Ok(rows) => {
                if !rows.is_empty() {
                    preserve_legacy_admin_audit_rows(&root_path, key, rows)?;
                }
                LegacyIndexRetirement::Remove
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    index_path = %index_path.display(),
                    "could not read obsolete previous registry; quarantining it so Community can continue"
                );
                LegacyIndexRetirement::Quarantine
            }
        }
    } else {
        LegacyIndexRetirement::Remove
    };
    for suffix in ["", "-wal", "-shm"] {
        let path = data_dir.join(format!("{LEGACY_INDEX_FILENAME}{suffix}"));
        if path.is_file() {
            match retirement {
                LegacyIndexRetirement::Remove => std::fs::remove_file(&path).map_err(|e| {
                    Error::storage(format!("remove obsolete registry {}: {e}", path.display()))
                })?,
                LegacyIndexRetirement::Quarantine => {
                    quarantine_legacy_index_file(data_dir, &path)?;
                }
            }
        }
    }
    Ok(())
}

fn read_legacy_admin_audit_rows(
    index_path: &Path,
    key: &KeyMaterial,
) -> Result<Vec<LegacyAdminAuditRow>> {
    let index = open_sqlcipher(index_path, key).map_err(|e| {
        Error::storage(format!(
            "open previous registry {} for admin audit preservation: {e}",
            index_path.display()
        ))
    })?;
    let has_audit_table: bool = index
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='audit_events_admin'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| Error::storage(format!("inspect previous admin audit table: {e}")))?;
    if !has_audit_table {
        return Ok(Vec::new());
    }

    let mut select = index
        .prepare(
            "SELECT audit_id, ts_ms, principal_subject, operation, target_tenant_id, result, details_json
             FROM audit_events_admin ORDER BY audit_id",
        )
        .map_err(|e| Error::storage(format!("read previous admin audit history: {e}")))?;
    let rows = select
        .query_map([], |row| {
            Ok(LegacyAdminAuditRow {
                audit_id: row.get(0)?,
                ts_ms: row.get(1)?,
                principal_subject: row.get(2)?,
                operation: row.get(3)?,
                target_tenant_id: row.get(4)?,
                result: row.get(5)?,
                details_json: row.get(6)?,
            })
        })
        .map_err(|e| Error::storage(format!("scan previous admin audit history: {e}")))?;

    rows.map(|row| row.map_err(|e| Error::storage(format!("decode previous admin audit row: {e}"))))
        .collect()
}

fn preserve_legacy_admin_audit_rows(
    root_path: &Path,
    key: &KeyMaterial,
    rows: Vec<LegacyAdminAuditRow>,
) -> Result<()> {
    let mut root = open_sqlcipher(root_path, key)?;
    migration::run_migrations(&mut root)?;
    for row in rows {
        root.execute(
            "INSERT OR IGNORE INTO audit_events_admin
             (audit_id, ts_ms, principal_subject, operation, target_tenant_id, result, details_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                row.audit_id,
                row.ts_ms,
                row.principal_subject,
                row.operation,
                row.target_tenant_id,
                row.result,
                row.details_json
            ],
        )
        .map_err(|e| {
            Error::storage(format!(
                "preserve previous admin audit row {}: {e}",
                row.audit_id
            ))
        })?;
    }
    Ok(())
}

fn quarantine_legacy_index_file(data_dir: &Path, path: &Path) -> Result<()> {
    let quarantine_dir = data_dir.join("migration-retired");
    std::fs::create_dir_all(&quarantine_dir).map_err(|e| {
        Error::storage(format!(
            "create migration quarantine directory {}: {e}",
            quarantine_dir.display()
        ))
    })?;
    let destination = quarantine_dir.join(
        path.file_name()
            .ok_or_else(|| Error::storage("previous registry file has no filename"))?,
    );
    if destination.exists() {
        return Err(Error::conflict(format!(
            "cannot quarantine obsolete registry {} because {} already exists",
            path.display(),
            destination.display()
        )));
    }
    std::fs::rename(path, &destination).map_err(|e| {
        Error::storage(format!(
            "quarantine obsolete registry {} to {}: {e}",
            path.display(),
            destination.display()
        ))
    })
}

fn promote_directory_contents(source: &Path, destination_dir: &Path) -> Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(source)
        .map_err(|e| Error::storage(format!("read {}: {e}", source.display())))?
    {
        let source_path = entry
            .map_err(|e| Error::storage(format!("read migration entry: {e}")))?
            .path();
        if source_path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            name == LEGACY_DEFAULT_DB_FILENAME
                || name == format!("{LEGACY_DEFAULT_DB_FILENAME}-wal")
                || name == format!("{LEGACY_DEFAULT_DB_FILENAME}-shm")
        }) {
            continue;
        }
        let destination = destination_dir.join(
            source_path
                .file_name()
                .ok_or_else(|| Error::storage("previous library entry has no filename"))?,
        );
        if destination.exists() {
            return Err(Error::conflict(format!(
                "cannot promote {} because {} already exists",
                source_path.display(),
                destination.display()
            )));
        }
        std::fs::rename(&source_path, &destination).map_err(|e| {
            Error::storage(format!(
                "promote {} to {}: {e}",
                source_path.display(),
                destination.display()
            ))
        })?;
    }
    Ok(())
}

fn move_file(source: &Path, destination: &Path) -> Result<()> {
    std::fs::rename(source, destination).map_err(|e| {
        Error::storage(format!(
            "move {} to {}: {e}",
            source.display(),
            destination.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SoloConfig;
    use crate::init::{InitParams, default_embedder, init};
    use zeroize::Zeroizing;

    #[test]
    fn promotes_only_default_database_to_root() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = legacy_default_from_fresh_init(&temp, "migration-test");
        let tenants = temp.path().join(LEGACY_LIBRARY_SUBDIR);
        std::fs::write(
            tenants.join("default").join("hnsw_episodes.hnsw.data"),
            b"hnsw",
        )
        .unwrap();

        assert!(migrate_legacy_default_library(temp.path(), &key).unwrap());
        assert!(temp.path().join(COMMUNITY_DB_FILENAME).is_file());
        assert!(temp.path().join("hnsw_episodes.hnsw.data").is_file());
        assert!(!temp.path().join(LEGACY_INDEX_FILENAME).exists());
        assert!(!tenants.exists());
    }

    #[test]
    fn refuses_layout_with_additional_database() {
        let temp = tempfile::TempDir::new().unwrap();
        let tenants = temp.path().join(LEGACY_LIBRARY_SUBDIR);
        std::fs::create_dir_all(&tenants).unwrap();
        std::fs::write(tenants.join(LEGACY_DEFAULT_DB_FILENAME), b"default").unwrap();
        std::fs::write(tenants.join("work.db"), b"work").unwrap();

        let key =
            KeyMaterial::derive("migration-test", &[7u8; crate::key_material::SALT_LEN]).unwrap();
        let error = migrate_legacy_default_library(temp.path(), &key).unwrap_err();
        assert!(matches!(error, Error::Conflict(_)));
        assert!(!temp.path().join(COMMUNITY_DB_FILENAME).exists());
    }

    #[test]
    fn promotes_real_legacy_layout_and_preserves_admin_audit() {
        let temp = tempfile::TempDir::new().unwrap();
        let passphrase = "real-legacy-layout-test";
        let key = legacy_default_from_fresh_init(&temp, passphrase);
        let tenants = temp.path().join(LEGACY_LIBRARY_SUBDIR);
        std::fs::write(
            tenants.join("default").join("hnsw_episodes.hnsw.data"),
            b"hnsw",
        )
        .unwrap();

        {
            let index_path = temp.path().join(LEGACY_INDEX_FILENAME);
            let index = open_sqlcipher(&index_path, &key).unwrap();
            index
                .execute_batch(
                    "CREATE TABLE audit_events_admin (
                        audit_id          INTEGER PRIMARY KEY AUTOINCREMENT,
                        ts_ms             INTEGER NOT NULL,
                        principal_subject TEXT,
                        operation         TEXT    NOT NULL,
                        target_tenant_id  TEXT,
                        result            TEXT    NOT NULL,
                        details_json      TEXT
                    );",
                )
                .unwrap();
            index
                .execute(
                    "INSERT INTO audit_events_admin
                     (audit_id, ts_ms, principal_subject, operation, target_tenant_id, result, details_json)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                    rusqlite::params![
                        42i64,
                        1234i64,
                        "admin",
                        "tenant.create",
                        "default",
                        "ok",
                        "{}"
                    ],
                )
                .unwrap();
        }

        assert!(migrate_legacy_default_library(temp.path(), &key).unwrap());
        assert!(temp.path().join(COMMUNITY_DB_FILENAME).is_file());
        assert!(!temp.path().join(LEGACY_INDEX_FILENAME).exists());
        assert!(!tenants.exists());
        assert!(temp.path().join("hnsw_episodes.hnsw.data").is_file());

        let root = open_sqlcipher(&temp.path().join(COMMUNITY_DB_FILENAME), &key).unwrap();
        let copied: i64 = root
            .query_row(
                "SELECT COUNT(*) FROM audit_events_admin WHERE audit_id = 42",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(copied, 1);
    }

    #[test]
    fn leaves_previous_default_in_place_when_key_is_wrong() {
        let temp = tempfile::TempDir::new().unwrap();
        legacy_default_from_fresh_init(&temp, "correct-passphrase");
        let tenants = temp.path().join(LEGACY_LIBRARY_SUBDIR);
        let wrong_key =
            KeyMaterial::derive("wrong-passphrase", &[7u8; crate::key_material::SALT_LEN]).unwrap();

        let error = migrate_legacy_default_library(temp.path(), &wrong_key).unwrap_err();

        assert!(matches!(error, Error::Storage(_)));
        assert!(tenants.join(LEGACY_DEFAULT_DB_FILENAME).is_file());
        assert!(!temp.path().join(COMMUNITY_DB_FILENAME).exists());
    }

    #[test]
    fn promotes_default_library_and_quarantines_unreadable_legacy_index() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = legacy_default_from_fresh_init(&temp, "unreadable-index-test");
        std::fs::write(temp.path().join(LEGACY_INDEX_FILENAME), b"not a database").unwrap();

        assert!(migrate_legacy_default_library(temp.path(), &key).unwrap());

        assert!(temp.path().join(COMMUNITY_DB_FILENAME).is_file());
        assert!(!temp.path().join(LEGACY_INDEX_FILENAME).exists());
        assert!(
            temp.path()
                .join("migration-retired")
                .join(LEGACY_INDEX_FILENAME)
                .is_file()
        );
    }

    #[test]
    fn resumes_interrupted_promotion_after_main_database_move() {
        let temp = tempfile::TempDir::new().unwrap();
        let tenants = temp.path().join(LEGACY_LIBRARY_SUBDIR);
        std::fs::create_dir_all(tenants.join("default")).unwrap();
        std::fs::write(temp.path().join(COMMUNITY_DB_FILENAME), b"db").unwrap();
        std::fs::write(
            tenants.join(format!("{LEGACY_DEFAULT_DB_FILENAME}-wal")),
            b"wal",
        )
        .unwrap();
        std::fs::write(
            tenants.join("default").join("hnsw_episodes.hnsw.data"),
            b"hnsw",
        )
        .unwrap();

        let key =
            KeyMaterial::derive("migration-test", &[7u8; crate::key_material::SALT_LEN]).unwrap();
        assert!(migrate_legacy_default_library(temp.path(), &key).unwrap());
        assert_eq!(
            std::fs::read(temp.path().join(format!("{COMMUNITY_DB_FILENAME}-wal"))).unwrap(),
            b"wal"
        );
        assert!(temp.path().join("hnsw_episodes.hnsw.data").is_file());
        assert!(!tenants.exists());
    }

    #[test]
    fn refuses_legacy_assets_when_default_database_is_missing() {
        let temp = tempfile::TempDir::new().unwrap();
        let tenants = temp.path().join(LEGACY_LIBRARY_SUBDIR);
        std::fs::create_dir_all(tenants.join("default")).unwrap();
        std::fs::write(tenants.join("default").join("asset.bin"), b"asset").unwrap();

        let key =
            KeyMaterial::derive("migration-test", &[7u8; crate::key_material::SALT_LEN]).unwrap();
        let error = migrate_legacy_default_library(temp.path(), &key).unwrap_err();
        assert!(matches!(error, Error::Conflict(_)));
        assert!(tenants.join("default").join("asset.bin").is_file());
    }

    #[tokio::test]
    async fn runtime_can_only_open_the_one_root_database() {
        let temp = tempfile::TempDir::new().unwrap();
        let passphrase = "one-library-runtime-test";
        let outcome = init(InitParams {
            data_dir: temp.path().to_path_buf(),
            passphrase: Zeroizing::new(passphrase.into()),
            force: false,
            embedder: default_embedder(),
        })
        .unwrap();
        let config = SoloConfig::read(&outcome.config_path).unwrap();
        let key = KeyMaterial::derive(passphrase, &config.salt_bytes().unwrap()).unwrap();
        let embedder: Arc<dyn Embedder> = Arc::new(crate::embedder::StubEmbedder::default_stub());
        let library = MemoryLibrary::open(MemoryLibraryParams {
            data_dir: temp.path().to_path_buf(),
            key,
            embedder,
            hnsw_params: HnswParams::default(),
            steward: None,
            runtime_handle: Some(tokio::runtime::Handle::current()),
            steward_factory: None,
            triples_batch_signal: None,
        })
        .unwrap();

        let handle = library.handle().await.unwrap();
        assert_eq!(handle.db_path(), temp.path().join(COMMUNITY_DB_FILENAME));
        assert!(Arc::ptr_eq(&handle, &library.handle().await.unwrap()));
        assert!(!temp.path().join(LEGACY_INDEX_FILENAME).exists());
        drop(handle);
        library.shutdown().await;
    }

    fn legacy_default_from_fresh_init(temp: &tempfile::TempDir, passphrase: &str) -> KeyMaterial {
        let outcome = init(InitParams {
            data_dir: temp.path().to_path_buf(),
            passphrase: Zeroizing::new(passphrase.into()),
            force: false,
            embedder: default_embedder(),
        })
        .unwrap();
        let config = SoloConfig::read(&outcome.config_path).unwrap();
        let key = KeyMaterial::derive(passphrase, &config.salt_bytes().unwrap()).unwrap();

        let tenants = temp.path().join(LEGACY_LIBRARY_SUBDIR);
        std::fs::create_dir_all(tenants.join("default")).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            move_if_exists(
                &temp.path().join(format!("{COMMUNITY_DB_FILENAME}{suffix}")),
                &tenants.join(format!("{LEGACY_DEFAULT_DB_FILENAME}{suffix}")),
            );
        }
        key
    }

    fn move_if_exists(source: &Path, destination: &Path) {
        if source.exists() {
            std::fs::rename(source, destination).unwrap();
        }
    }
}
