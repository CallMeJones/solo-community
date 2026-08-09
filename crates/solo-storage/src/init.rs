// SPDX-License-Identifier: Apache-2.0

//! `solo init`: create a fresh Solo data directory.
//!
//! The orchestrator validates the data directory, acquires `solo.lock`, derives
//! the SQLCipher key, creates the one Community database at `solo.db`, runs its
//! migrations, and writes `solo.config.toml`.
//!
//! A previous default-library layout is promoted only when it is unambiguous.
//! Evidence of an additional database makes initialization stop so Community
//! never guesses which user data to discard. `--force` removes only known
//! Solo-owned files and leaves unrelated data-directory contents untouched.

use rusqlite::Connection;
use solo_core::{Embedder, Error, Result};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

use crate::{
    config::{EmbedderConfig, LlmSettings, SoloConfig},
    key_material::KeyMaterial,
    library::{TENANTS_INDEX_FILENAME, TENANTS_SUBDIR},
    lockfile::Lockfile,
    memory_library::{COMMUNITY_DB_FILENAME, migrate_legacy_default_library},
    migration,
    path_validation::validate_data_dir,
};

/// Default data dir: `~/.solo/`. Honors the home-dir resolution `dirs` crate
/// performs (Windows: `%USERPROFILE%`; Unix: `$HOME`). Returns `None` if no
/// home directory can be found.
pub fn default_data_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".solo"))
}

/// File names at the data dir root that Solo owns. `--force` removes these;
/// previous-layout names remain only so upgrades and cleanup are safe. Anything
/// else in the directory is left untouched.
///
/// HNSW snapshot filenames are derived from the basenames in
/// `crate::snapshot` (`hnsw_episodes`, `hnsw_episodes_bak`, `hnsw_episodes_tmp`)
/// + the suffixes hnsw_rs's `file_dump` writes (`.hnsw.data`, `.hnsw.graph`).
/// Keep this list in sync with `snapshot::{LIVE_BASENAME, BAK_BASENAME,
/// TMP_BASENAME}` if those ever change.
///
/// **Note**: v0.7.1 `solo.db` and HNSW snapshots are listed for `--force` wipe
/// purposes (a `--force` re-init must clear them) AND for v0.7.1 install
/// detection (the legacy `solo.db` at the root is the v0.7.1 marker). The
/// v0.8.0 layout puts these files under `<data_dir>/tenants/` — they are
/// wiped via a directory-tree walk in `wipe_solo_owned_files`.
const SOLO_OWNED_FILES_ROOT: &[&str] = &[
    // v0.7.1 single-DB layout (legacy; only present pre-migration). Listed
    // first so a v0.7.1 install upgraded via mass-data-move clears any
    // stragglers if the upgrade had to be aborted and retried with --force.
    "solo.db",
    "solo.db-wal",
    "solo.db-shm",
    // v0.7.1 HNSW snapshots at root (live + bak + tmp pairs).
    "hnsw_episodes.hnsw.data",
    "hnsw_episodes.hnsw.graph",
    "hnsw_episodes_bak.hnsw.data",
    "hnsw_episodes_bak.hnsw.graph",
    "hnsw_episodes_tmp.hnsw.data",
    "hnsw_episodes_tmp.hnsw.graph",
    // Top-level Solo files (still at root in v0.8.0).
    "solo.config.toml",
    "solo.config.toml.tmp",
    "solo.lock",
    // v0.8.0 tenant registry.
    TENANTS_INDEX_FILENAME,
    "tenants_index.db-wal",
    "tenants_index.db-shm",
];

/// `solo init` parameters. Built by the CLI layer.
#[derive(Debug, Clone)]
pub struct InitParams {
    /// Where to put the data dir. Created if missing.
    pub data_dir: PathBuf,
    /// Resolved passphrase, wrapped in `Zeroizing` so the buffer is wiped
    /// when this struct drops. CLI layer reads it via prompt or env var.
    pub passphrase: Zeroizing<String>,
    /// If true, wipe Solo-owned files in `data_dir` before initializing.
    pub force: bool,
    /// Embedder identity to record in the config. For commit 1.1 this is the
    /// BGE-M3 default; commit 1.4 (embedder loader) will produce it from the
    /// loaded model.
    pub embedder: EmbedderConfig,
}

/// Default embedder identity recorded in `solo.config.toml` when the
/// CLI hasn't probed a real backend via
/// [`crate::embedder::probe_embedder_config_from_env`].
///
/// In production, `solo init` always calls `probe_embedder_config_from_env`,
/// which picks between Ollama (probes the real dim) and Stub (32-dim,
/// deterministic). This function exists for test fixtures + downstream
/// callers that want a parameterless identity for first-init flows; it
/// returns the Stub identity, matching `StubEmbedder::default_stub()`
/// (name=`stub`, version=`v1`, dim=32).
///
/// Historically this returned the BGE-M3 identity (BAAI/bge-m3, 1024-dim).
/// BGE-M3 was removed in v0.6.0 — see `docs/dev-log/0071-v0.5.x-roadmap.md`
/// Priority 9. Callers that need a deterministic non-stub identity for
/// tests should build an `EmbedderConfig` literal directly.
pub fn default_embedder() -> EmbedderConfig {
    let stub = crate::embedder::StubEmbedder::default_stub();
    EmbedderConfig {
        name: stub.name().to_string(),
        version: stub.version().to_string(),
        dim: stub.dim() as u32,
        dtype: "f32".into(),
    }
}

/// v0.9.0 P1 (plan BLOCKER 2 resolution): pick the `[llm]` block default
/// for a freshly-initialised data dir based on the surrounding env.
///
/// Precedence:
///   1. `ANTHROPIC_API_KEY` non-empty → `Anthropic` variant with
///      `api_key_env = "ANTHROPIC_API_KEY"` and the plan's
///      `claude-sonnet-4-6` default model.
///   2. (Future P1 follow-up may add `OPENAI_API_KEY` here; for v0.9.0
///      P1 we keep the surface minimal — the operator edits the file if
///      they want OpenAI, Ollama, or MCP-sampling.)
///   3. otherwise → `None` variant. The Steward runs cluster-only.
///
/// Empty values are treated as unset — guards against shells that set
/// vars to the empty string to mean "leave default".
pub fn default_llm_settings_from_env() -> LlmSettings {
    fn env_non_empty(name: &str) -> bool {
        std::env::var(name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    }
    if env_non_empty("ANTHROPIC_API_KEY") {
        LlmSettings::Anthropic {
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            model: "claude-sonnet-4-6".to_string(),
        }
    } else {
        LlmSettings::None
    }
}

/// Outcome reported back to the CLI layer for human-readable success output.
#[derive(Debug)]
pub struct InitOutcome {
    pub data_dir: PathBuf,
    /// The one Community SQLCipher database at `<data_dir>/solo.db`.
    pub db_path: PathBuf,
    pub config_path: PathBuf,
    /// Highest applied Memory Library schema version.
    pub schema_version: u32,
    /// True when the previous `tenants/default.db` layout was promoted.
    pub upgraded_from_v071: bool,
}

/// Initialize or promote Solo Community's one Memory Library.
pub fn init(params: InitParams) -> Result<InitOutcome> {
    let InitParams {
        data_dir,
        passphrase,
        force,
        embedder,
    } = params;
    if passphrase.is_empty() {
        return Err(Error::invalid_input(
            "passphrase must not be empty (Solo uses it to derive the SQLCipher key)",
        ));
    }

    validate_data_dir(&data_dir)?;
    std::fs::create_dir_all(&data_dir)
        .map_err(|e| Error::storage(format!("create data dir {}: {e}", data_dir.display())))?;
    let config_path = data_dir.join("solo.config.toml");
    let db_path = data_dir.join(COMMUNITY_DB_FILENAME);
    let legacy_dir = data_dir.join(TENANTS_SUBDIR);
    let legacy_default_db = legacy_dir.join("default.db");
    let legacy_index = data_dir.join(TENANTS_INDEX_FILENAME);
    let _lock = Lockfile::acquire(&data_dir.join("solo.lock"))?;

    if force {
        wipe_solo_owned_files(&data_dir)?;
    } else if db_path.is_file() {
        return Err(Error::conflict(format!(
            "data directory is already initialized: {}\nRe-run with --force to wipe and re-initialize (DESTRUCTIVE).",
            data_dir.display()
        )));
    } else if legacy_dir.is_dir()
        || legacy_default_db.is_file()
        || legacy_index.is_file()
        || data_dir
            .join(format!("{TENANTS_INDEX_FILENAME}-wal"))
            .exists()
        || data_dir
            .join(format!("{TENANTS_INDEX_FILENAME}-shm"))
            .exists()
    {
        if !config_path.is_file() {
            return Err(Error::conflict(format!(
                "the previous Solo layout exists in {} but solo.config.toml is missing",
                data_dir.display()
            )));
        }
        let config = SoloConfig::read(&config_path)?;
        let key = KeyMaterial::derive(&passphrase, &config.salt_bytes()?)?;
        migrate_legacy_default_library(&data_dir, &key)?;
        let mut conn = open_sqlcipher(&db_path, &key)?;
        let schema_version = migration::run_migrations(&mut conn)?;
        return Ok(InitOutcome {
            data_dir,
            db_path,
            config_path,
            schema_version,
            upgraded_from_v071: true,
        });
    }

    let salt = KeyMaterial::fresh_salt()?;
    let key = KeyMaterial::derive(&passphrase, &salt)?;
    let mut conn = open_sqlcipher(&db_path, &key)?;
    let schema_version = migration::run_migrations(&mut conn)?;
    drop(conn);
    let verify = open_sqlcipher(&db_path, &key)?;
    let highest: u32 = verify
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .map_err(|e| Error::storage(format!("verify Community database: {e}")))?;
    if highest != schema_version {
        return Err(Error::storage(format!(
            "Community database migration drift: wrote {schema_version}, read {highest}"
        )));
    }

    let mut config = SoloConfig::new(salt, embedder);
    config.llm = Some(default_llm_settings_from_env());
    config.write(&config_path)?;
    Ok(InitOutcome {
        data_dir,
        db_path,
        config_path,
        schema_version,
        upgraded_from_v071: false,
    })
}

/// Open a SQLCipher database, bind the raw key, and set the journal-mode +
/// foreign-keys pragmas. Used by `init` and exposed for tests.
pub fn open_sqlcipher(db_path: &Path, key: &KeyMaterial) -> Result<Connection> {
    let conn = Connection::open(db_path)
        .map_err(|e| Error::storage(format!("open {}: {e}", db_path.display())))?;
    // PRAGMA key MUST be the first statement on a fresh connection.
    // `as_hex()` returns Zeroizing<String>; wrap the formatted PRAGMA in
    // Zeroizing<String> so the raw key bytes are wiped on drop rather
    // than lingering in the heap until the allocator reuses the region.
    let key_pragma: zeroize::Zeroizing<String> = {
        let hex = key.as_hex();
        zeroize::Zeroizing::new(format!("PRAGMA key = \"x'{}'\"", *hex))
    };
    conn.execute_batch(&key_pragma)
        .map_err(|e| Error::storage(format!("PRAGMA key: {e}")))?;
    // Standard pragmas. journal_mode=wal returns the new mode as a row, so we
    // use query_row; the others execute fine via execute_batch.
    let mode: String = conn
        .query_row("PRAGMA journal_mode = wal", [], |row| row.get(0))
        .map_err(|e| Error::storage(format!("set journal_mode=wal: {e}")))?;
    if mode.to_lowercase() != "wal" {
        return Err(Error::storage(format!(
            "expected WAL journal mode, got {mode}"
        )));
    }
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA synchronous = NORMAL;",
    )
    .map_err(|e| Error::storage(format!("set startup pragmas: {e}")))?;
    Ok(conn)
}

fn wipe_solo_owned_files(data_dir: &Path) -> Result<()> {
    if !data_dir.exists() {
        return Ok(());
    }
    // Root-level files (legacy v0.7.1 + v0.8.0 top-level).
    for name in SOLO_OWNED_FILES_ROOT {
        let p = data_dir.join(name);
        if p.is_file() {
            std::fs::remove_file(&p)
                .map_err(|e| Error::storage(format!("remove {}: {e}", p.display())))?;
        }
    }
    // v0.8.0 per-tenant subdir — everything inside, then the directory.
    // We don't use a recursive remove of arbitrary subdirs (defensive against
    // operator surgery that might have nested unrelated state under the
    // data dir); we only touch the explicit `tenants/` subdir Solo owns.
    let tenants = data_dir.join(TENANTS_SUBDIR);
    if tenants.is_dir() {
        for entry in std::fs::read_dir(&tenants)
            .map_err(|e| Error::storage(format!("read tenants dir {}: {e}", tenants.display())))?
        {
            let entry = entry.map_err(|e| {
                Error::storage(format!("scan tenants dir {}: {e}", tenants.display()))
            })?;
            let p = entry.path();
            if p.is_file() {
                std::fs::remove_file(&p)
                    .map_err(|e| Error::storage(format!("remove {}: {e}", p.display())))?;
            }
        }
        // Best-effort rmdir — leave the dir if some non-Solo content sneaked in.
        let _ = std::fs::remove_dir(&tenants);
    }
    Ok(())
}

#[cfg(test)]
mod community_tests {
    use super::*;
    use tempfile::TempDir;

    fn params(path: &Path) -> InitParams {
        InitParams {
            data_dir: path.to_path_buf(),
            passphrase: Zeroizing::new("community-test-passphrase".into()),
            force: false,
            embedder: default_embedder(),
        }
    }

    #[test]
    fn fresh_init_creates_exactly_one_database() {
        let temp = TempDir::new().unwrap();
        let outcome = init(params(temp.path())).unwrap();
        assert_eq!(outcome.db_path, temp.path().join(COMMUNITY_DB_FILENAME));
        assert!(outcome.db_path.is_file());
        assert!(!temp.path().join(TENANTS_INDEX_FILENAME).exists());
        assert!(!temp.path().join(TENANTS_SUBDIR).exists());
        assert_eq!(
            outcome.schema_version,
            migration::current_per_tenant_schema_version()
        );
    }

    #[test]
    fn second_init_refuses_without_force() {
        let temp = TempDir::new().unwrap();
        init(params(temp.path())).unwrap();
        assert!(matches!(init(params(temp.path())), Err(Error::Conflict(_))));
    }

    #[test]
    fn force_recreates_the_single_database() {
        let temp = TempDir::new().unwrap();
        let first = init(params(temp.path())).unwrap();
        let first_salt = SoloConfig::read(&first.config_path).unwrap().salt_hex;
        let mut forced = params(temp.path());
        forced.force = true;
        let second = init(forced).unwrap();
        let second_salt = SoloConfig::read(&second.config_path).unwrap().salt_hex;
        assert_ne!(first_salt, second_salt);
        assert!(second.db_path.is_file());
        assert!(!temp.path().join(TENANTS_INDEX_FILENAME).exists());
    }
}
