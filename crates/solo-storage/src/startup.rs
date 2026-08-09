// SPDX-License-Identifier: Apache-2.0

//! Daemon startup orchestration. Per ADR-0003 §O6 ("Startup ordering: linear
//! await chain in main()") and §"Startup file-existence decision tree".
//!
//! [`StartupChain::run`] is the canonical sequence used by `solo daemon`:
//!
//!   1. Read `solo.config.toml` (salt + embedder identity).
//!   2. Open the SQLCipher database with the supplied key (init connection).
//!   3. Run pending migrations (idempotent for already-migrated DBs).
//!   4. Load the HNSW snapshot from disk:
//!         a. Try `snapshot::load(dir)` (the live `_episodes.hnsw.{data,graph}`
//!            pair).
//!         b. On failure, try `snapshot::load_bak(dir)`.
//!         c. On both failures, fall through to a fresh empty index. (Rebuild
//!            from SQL is post-v0.1; the daemon logs a WARN so ops can act.)
//!   5. Validate dim consistency: HNSW snapshot dim, if non-zero, must equal
//!      `solo.config.toml.embedder.dim`. Refuse to start on mismatch — the
//!      embedder identity has shifted under the daemon and user data would
//!      be quietly miscompared.
//!   6. Replay `pending_index` rows into the HNSW.
//!   7. Detect drift (`hot episodes` vs `index.len()`); WARN on non-zero diff.
//!   8. Close the init connection. The WriterActor (spawned by the caller)
//!      opens its own long-lived connection on its dedicated thread per
//!      ADR-0003 §"Migration vs. writer-thread connection lifecycle".
//!
//! Returns a [`StartupOutcome`] with everything the daemon main needs to
//! spawn the writer actor + reader pool. The caller is responsible for the
//! lockfile (must outlive this call) and for spawning the writer thread.
//!
//! ## What this module does NOT do
//!
//! - Lockfile acquisition (caller's job; lock must outlive the daemon).
//! - Passphrase prompting (CLI's job).
//! - Spawning the writer thread (caller chooses the snapshot_dir + capacity).
//! - Building the read pool (caller chooses pool size).
//! - Constructing the embedder (caller wires Stub or BGE-M3 per config).
//! - Signal handling, panic hook, snapshot timer (commit 1.5 daemon main).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use solo_core::{Result, VectorIndex, VectorIndexFactory};

use crate::config::SoloConfig;
use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
use crate::hnsw_rebuild::{rebuild_chunk_tombstones_from_sql, rebuild_episode_tombstones_from_sql};
use crate::init::open_sqlcipher;
use crate::key_material::KeyMaterial;
use crate::memory_library::{COMMUNITY_DB_FILENAME, migrate_legacy_default_library};
use crate::migration;
use crate::recovery::{
    DriftReport, RebuildReport, ReplayReport, detect_drift, rebuild_hnsw_from_sql,
    replay_pending_index,
};
use crate::snapshot;
use crate::vector_index::{HnswFactory, HnswIndex, HnswParams};

/// What the startup chain hands back to the daemon main. Mostly opaque
/// because the daemon doesn't need to inspect any of this — it just feeds
/// the writer actor + reader pool.
pub struct StartupOutcome {
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub config: SoloConfig,
    /// Schema version after migrations. May equal pre-startup version if
    /// nothing was pending.
    pub schema_version: u32,
    /// The HNSW. Wrapped in `Arc<dyn VectorIndex>` so it can be shared with
    /// the read pool per ADR-0003 §O2.
    pub hnsw: Arc<dyn VectorIndex + Send + Sync>,
    /// Resolved `embedders.embedder_id` for the persisted config's
    /// embedder identity. Lazy-inserted on first daemon start; cached
    /// in the WriterActor for every subsequent `INSERT INTO embeddings`.
    pub embedder_id: i64,
    /// Pending-index replay statistics.
    pub replay: ReplayReport,
    /// Drift between SQL `episodes WHERE tier='hot'` and HNSW length, after
    /// replay. Daemon decides what to do (warn / refuse / rebuild) — startup
    /// itself only reports.
    pub drift: DriftReport,
    /// True if we fell back to the `.bak` snapshot.
    pub used_bak_snapshot: bool,
    /// True if no usable snapshot was found and we started with a fresh index.
    /// (Independent of `rebuild` below — `started_fresh && rebuild.rows_added > 0`
    /// means the snapshot was missing but the index was rebuilt from the
    /// `embeddings` table.)
    pub started_fresh: bool,
    /// Stats from `rebuild_hnsw_from_sql` after a missing-snapshot
    /// fallback. All zeros in the steady state (snapshot loaded fine) and
    /// after `solo init` (no rows yet). Non-zero only after `solo reembed`
    /// has wiped the snapshots. `rows_skipped > 0` means the rebuild
    /// degraded around corrupt rows — recall coverage will be incomplete
    /// for those rowids until the user investigates and re-runs reembed.
    pub rebuild: RebuildReport,
}

impl std::fmt::Debug for StartupOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Arc<dyn VectorIndex> is not Debug; surface only summary-shaped fields.
        f.debug_struct("StartupOutcome")
            .field("data_dir", &self.data_dir)
            .field("schema_version", &self.schema_version)
            .field("hnsw_len", &self.hnsw.len())
            .field("hnsw_dim", &self.hnsw.dim())
            .field("embedder_id", &self.embedder_id)
            .field("replay", &self.replay)
            .field("drift", &self.drift)
            .field("used_bak_snapshot", &self.used_bak_snapshot)
            .field("started_fresh", &self.started_fresh)
            .field("rebuild", &self.rebuild)
            .finish()
    }
}

/// Tunable parameters for the startup chain. Everything has a sensible
/// default; the daemon main can override per environment.
#[derive(Debug, Clone)]
pub struct StartupParams {
    pub data_dir: PathBuf,
    pub key: KeyMaterial,
    pub hnsw_params: HnswParams,
}

impl StartupParams {
    pub fn new(data_dir: impl Into<PathBuf>, key: KeyMaterial) -> Self {
        Self {
            data_dir: data_dir.into(),
            key,
            hnsw_params: HnswParams::default(),
        }
    }

    pub fn with_hnsw_params(mut self, params: HnswParams) -> Self {
        self.hnsw_params = params;
        self
    }
}

/// Run the startup sequence. Linear await chain; each step gates the next.
pub fn run(params: StartupParams) -> Result<StartupOutcome> {
    let StartupParams {
        data_dir,
        key,
        hnsw_params,
    } = params;

    // Step 1 — read solo.config.toml.
    let config_path = data_dir.join("solo.config.toml");
    let config = SoloConfig::read(&config_path)?;
    let dim = config.embedder.dim as usize;
    if dim == 0 {
        return Err(solo_core::Error::storage(format!(
            "solo.config.toml records embedder.dim=0 — corrupt config? at {config_path:?}"
        )));
    }

    migrate_legacy_default_library(&data_dir, &key)?;
    let db_path = data_dir.join(COMMUNITY_DB_FILENAME);
    if !db_path.is_file() {
        return Err(solo_core::Error::not_found(format!(
            "Solo database not found in {}; run `solo init` first",
            data_dir.display()
        )));
    }
    let mut conn: Connection = open_sqlcipher(&db_path, &key)?;

    // Step 3 — run migrations idempotently.
    let schema_version = migration::run_migrations(&mut conn)?;

    // Step 3b — resolve embedder_id from the persisted config. Lazy-
    // inserts the row in `embedders` on first daemon start. Verifies
    // dim/dtype consistency against any prior row → Conflict if the
    // user changed embedder dim under the same name+version.
    let embedder_identity = EmbedderIdentity {
        name: config.embedder.name.clone(),
        version: config.embedder.version.clone(),
        dim: config.embedder.dim,
        dtype: config.embedder.dtype.clone(),
    };
    let embedder_id = get_or_insert_embedder_id(&conn, &embedder_identity)?;

    // Step 4 — load HNSW snapshot.
    //
    // v0.8.0 layout: snapshots live in `<data_dir>/tenants/` alongside
    // the per-tenant DB. The migrate helper moves them there as part of
    // the v0.7.1 upgrade. We pass that subdir to `load_hnsw_with_fallback`
    // for the default-tenant single-tenant case in P1; P2 introduces
    // per-tenant snapshot subdirs for multi-tenant deployments.
    let snapshot_dir = data_dir.clone();
    let factory = HnswFactory::with_params(hnsw_params);
    let (hnsw_index, used_bak_snapshot, started_fresh) =
        load_hnsw_with_fallback(&snapshot_dir, &factory, dim);

    // Step 5 — dim consistency check (only if we loaded a non-empty snapshot).
    if !started_fresh && hnsw_index.dim() != dim {
        return Err(solo_core::Error::storage(format!(
            "HNSW snapshot dim ({}) does not match solo.config.toml embedder.dim ({}). \
             Embedder identity has shifted under the daemon. Run `solo reembed` to rebuild.",
            hnsw_index.dim(),
            dim
        )));
    }

    // Step 5b — rebuild from SQL when no snapshot was loadable. The
    // `solo reembed` flow wipes the snapshot pairs deliberately so that
    // this branch picks up the freshly-written embeddings rows; without
    // it, recall would return zero hits until the user remembers enough
    // new content to repopulate the graph.
    //
    // Done BEFORE wrapping `hnsw_index` in Arc (cheaper to take a `&dyn
    // VectorIndex` from the owned value) and BEFORE `replay_pending_index`
    // / `rebuild_tombstones_from_sql` so those steps see the rebuilt
    // index in its expected state.
    let rebuild = if started_fresh {
        let started = std::time::Instant::now();
        let r = rebuild_hnsw_from_sql(&conn, &hnsw_index, embedder_id)?;
        if r.rows_seen > 0 {
            tracing::info!(
                rows_seen = r.rows_seen,
                rows_added = r.rows_added,
                rows_skipped = r.rows_skipped,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "rebuilt HNSW from `embeddings` after empty-snapshot fallback"
            );
        }
        r
    } else {
        RebuildReport::default()
    };

    let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(hnsw_index);

    // Step 6 — rebuild tombstones from SQL (only when we LOADED a
    // snapshot).
    //
    // `HnswIndex::tombstones` is in-memory only — a snapshot reload comes
    // back with an empty set, so previously-forgotten vectors would
    // technically reappear in the HNSW graph. The SQL `status='active'`
    // filter on the recall path masks this from users, but it would
    // produce spurious drift-detected warnings (`index.len()` would
    // count forgotten vectors as live) and would mean re-add of a
    // previously-forgotten id wouldn't lift a stale tombstone.
    //
    // When we REBUILT from SQL (Step 5b), this step is unnecessary AND
    // counterproductive: rebuild already excludes `status='forgotten'`
    // rows, so the forgotten rowids aren't in the graph. Adding them to
    // the tombstone set anyway would skew `len()` (which reports
    // `raw_len - tombstones.len()`) and produce false-positive drift
    // warnings. So we only run the tombstone rebuild on the
    // snapshot-loaded path.
    let (forgotten, forgotten_chunks) = if started_fresh {
        (0, 0)
    } else {
        // Dev-log 0154: both passes live in the shared `hnsw_rebuild`
        // module so the per-tenant copies in `tenants/handle.rs` can't
        // drift from this default-data-dir one.
        let eps = rebuild_episode_tombstones_from_sql(&conn, hnsw.as_ref())?;
        let chunks = rebuild_chunk_tombstones_from_sql(&conn, hnsw.as_ref())?;
        (eps, chunks)
    };
    if forgotten > 0 {
        tracing::info!(
            forgotten,
            "rebuilt HNSW tombstones from episodes.status='forgotten'"
        );
    }
    if forgotten_chunks > 0 {
        tracing::info!(
            forgotten_chunks,
            "rebuilt HNSW tombstones from document_chunks of forgotten documents"
        );
    }

    // Step 7 — replay pending_index.
    let replay = replay_pending_index(&mut conn, hnsw.as_ref())?;

    // Step 8 — drift detection (advisory).
    let drift = detect_drift(&conn, hnsw.as_ref())?;

    // Step 9 — close init connection. The writer actor will open its own.
    drop(conn);

    Ok(StartupOutcome {
        data_dir,
        db_path,
        config,
        schema_version,
        hnsw,
        embedder_id,
        replay,
        drift,
        used_bak_snapshot,
        started_fresh,
        rebuild,
    })
}

// Dev-log 0154: the previous local copies of
// `rebuild_tombstones_from_sql` + `rebuild_chunk_tombstones_from_sql`
// were lifted into `crate::hnsw_rebuild` so the per-tenant copies in
// `tenants/handle.rs` use the same implementation. See that module for
// the rationale + commentary.

/// Try the live snapshot, then `.bak`, then fall back to a fresh empty
/// index of the configured dim. Logging communicates which path was taken
/// so ops can investigate `.bak` falls back without surprise.
fn load_hnsw_with_fallback(
    data_dir: &Path,
    factory: &HnswFactory,
    dim: usize,
) -> (HnswIndex, bool, bool) {
    match snapshot::load(data_dir) {
        Ok(idx) => {
            tracing::info!(
                snapshot_kind = "live",
                dim = idx.dim(),
                len = idx.len(),
                "HNSW loaded from live snapshot"
            );
            (idx, false, false)
        }
        Err(primary_err) => {
            tracing::warn!(error = %primary_err, "live HNSW snapshot failed; trying .bak");
            match snapshot::load_bak(data_dir) {
                Ok(idx) => {
                    tracing::warn!(
                        snapshot_kind = "bak",
                        dim = idx.dim(),
                        len = idx.len(),
                        "HNSW loaded from backup snapshot — investigate the live pair"
                    );
                    (idx, true, false)
                }
                Err(bak_err) => {
                    tracing::warn!(
                        primary = %primary_err,
                        bak = %bak_err,
                        dim,
                        "no HNSW snapshot available; starting fresh empty index. \
                         The startup chain will attempt rebuild_hnsw_from_sql next; \
                         if the `embeddings` table is also empty, recall will return \
                         no hits until new content is remembered."
                    );
                    let empty = factory
                        .create(dim)
                        .expect("HnswFactory::create with valid dim must succeed");
                    (empty, false, true)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmbedderConfig;
    use crate::init::{InitParams, init};
    use crate::key_material::KeyMaterial;
    use rusqlite::params;
    use solo_core::{Confidence, EncodingContext, Episode, MemoryId, Tier};

    fn fresh_init_dir() -> (tempfile::TempDir, KeyMaterial) {
        let tmp = tempfile::TempDir::new().unwrap();
        // Use init() to lay down a real db + config for startup to pick up.
        // init() generates its own salt; we re-derive the key from the
        // persisted salt afterwards.
        let _ = init(InitParams {
            data_dir: tmp.path().to_path_buf(),
            passphrase: zeroize::Zeroizing::new("password-123".into()),
            force: false,
            embedder: EmbedderConfig {
                name: "stub".into(),
                version: "v1".into(),
                dim: 32,
                dtype: "f32".into(),
            },
        })
        .unwrap();
        // Re-derive key with the salt that init() persisted.
        let cfg = SoloConfig::read(&tmp.path().join("solo.config.toml")).unwrap();
        let key = KeyMaterial::derive("password-123", &cfg.salt_bytes().unwrap()).unwrap();
        (tmp, key)
    }

    /// v0.8.0 layout: snapshots live under `<data_dir>/tenants/`. Tests
    /// that plant snapshots / open the per-tenant DB directly use these
    /// helpers so the path-resolution logic is in one place.
    fn snapshot_dir(data_dir: &Path) -> PathBuf {
        data_dir.to_path_buf()
    }
    fn per_tenant_db(data_dir: &Path) -> PathBuf {
        data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME)
    }

    fn enqueue_pending(conn: &Connection, memory_id: &str, dim: usize) {
        let zeros = vec![0u8; dim * 4];
        conn.execute(
            "INSERT INTO pending_index (memory_id, embedding, embedding_dim, enqueued_at)
             VALUES (?, ?, ?, ?)",
            params![memory_id, &zeros[..], dim as i64, 0i64],
        )
        .unwrap();
    }

    fn insert_hot_episode(conn: &Connection, content: &str) -> String {
        let mid = MemoryId::new();
        let ep = Episode {
            memory_id: mid,
            ts_ms: chrono::Utc::now().timestamp_millis(),
            source_type: "user_message".into(),
            source_id: None,
            content: content.into(),
            encoding_context: EncodingContext::default(),
            provenance: None,
            confidence: Confidence::new(0.9).unwrap(),
            strength: 0.5,
            salience: 0.5,
            tier: Tier::Hot,
        };
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tier = match ep.tier {
            Tier::Hot => "hot",
            Tier::Warm => "warm",
            Tier::Cold => "cold",
        };
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, source_id, content,
                encoding_context_json, provenance_json, confidence,
                strength, salience, tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                ep.memory_id.to_string(),
                ep.ts_ms,
                ep.source_type,
                ep.source_id,
                ep.content,
                "{}",
                Option::<String>::None,
                ep.confidence.0,
                ep.strength,
                ep.salience,
                tier,
                now_ms,
                now_ms,
            ],
        )
        .unwrap();
        mid.to_string()
    }

    #[test]
    fn run_starts_fresh_when_no_snapshot_exists() {
        let (tmp, key) = fresh_init_dir();
        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        assert!(outcome.started_fresh);
        assert!(!outcome.used_bak_snapshot);
        assert_eq!(outcome.hnsw.len(), 0);
        assert_eq!(outcome.hnsw.dim(), 32);
        assert_eq!(outcome.replay.rows_seen, 0);
        assert!(outcome.drift.is_clean());
    }

    #[test]
    fn run_replays_pending_index_into_fresh_hnsw() {
        let (tmp, key) = fresh_init_dir();
        // Pre-populate: insert an episode + queue it in pending_index.
        let cfg = SoloConfig::read(&tmp.path().join("solo.config.toml")).unwrap();
        let conn = open_sqlcipher(&per_tenant_db(tmp.path()), &key).unwrap();
        let mid = insert_hot_episode(&conn, "hello startup");
        enqueue_pending(&conn, &mid, cfg.embedder.dim as usize);
        drop(conn);

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        assert!(outcome.started_fresh);
        assert_eq!(outcome.replay.rows_seen, 1);
        assert_eq!(outcome.replay.rows_replayed, 1);
        assert_eq!(outcome.hnsw.len(), 1);
        assert!(outcome.drift.is_clean(), "drift: {:?}", outcome.drift);
    }

    #[test]
    fn run_loads_persisted_snapshot_when_present() {
        let (tmp, key) = fresh_init_dir();
        let dim = 32usize;
        // Build + save a snapshot manually to simulate a daemon shutdown/restart.
        {
            use solo_core::VectorIndex;
            let factory = HnswFactory::default();
            let idx = factory.create(dim).unwrap();
            for i in 1..=5 {
                let v = vec![0.1f32 * i as f32; dim];
                idx.add(i as i64, &v).unwrap();
            }
            snapshot::save(&idx, &snapshot_dir(tmp.path())).unwrap();
        }

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        assert!(!outcome.started_fresh);
        assert!(!outcome.used_bak_snapshot);
        assert_eq!(outcome.hnsw.len(), 5);
        assert_eq!(outcome.hnsw.dim(), dim);
    }

    #[test]
    fn run_falls_back_to_bak_when_live_corrupt() {
        let (tmp, key) = fresh_init_dir();
        let dim = 32usize;
        {
            use solo_core::VectorIndex;
            let factory = HnswFactory::default();
            // Save 1 → live exists, no bak.
            let idx1 = factory.create(dim).unwrap();
            for i in 1..=3 {
                idx1.add(i, &vec![0.0f32; dim]).unwrap();
            }
            snapshot::save(&idx1, &snapshot_dir(tmp.path())).unwrap();
            // Save 2 → live = idx2 (5 elements), bak = idx1 (3 elements).
            let idx2 = factory.create(dim).unwrap();
            for i in 1..=5 {
                idx2.add(i, &vec![0.0f32; dim]).unwrap();
            }
            snapshot::save(&idx2, &snapshot_dir(tmp.path())).unwrap();
        }
        // Corrupt the live graph file (v0.8.0 layout: under tenants/).
        std::fs::write(
            snapshot_dir(tmp.path()).join("hnsw_episodes.hnsw.graph"),
            b"GARBAGE",
        )
        .unwrap();

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        assert!(!outcome.started_fresh);
        assert!(outcome.used_bak_snapshot);
        assert_eq!(outcome.hnsw.len(), 3); // bak's value
    }

    #[test]
    fn run_refuses_when_db_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Write only a config; no db file.
        let cfg = SoloConfig::new(
            [0u8; crate::key_material::SALT_LEN],
            EmbedderConfig {
                name: "stub".into(),
                version: "v1".into(),
                dim: 32,
                dtype: "f32".into(),
            },
        );
        cfg.write(&tmp.path().join("solo.config.toml")).unwrap();
        let key = KeyMaterial::derive("password-123", &cfg.salt_bytes().unwrap()).unwrap();
        let err = run(StartupParams::new(tmp.path(), key)).unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn run_refuses_when_dim_mismatches_snapshot() {
        let (tmp, key) = fresh_init_dir();
        // Save a snapshot with the WRONG dim (config says 32, snapshot says 8).
        {
            use solo_core::VectorIndex;
            let factory = HnswFactory::default();
            let idx = factory.create(8).unwrap();
            idx.add(1, &vec![0.0f32; 8]).unwrap();
            snapshot::save(&idx, &snapshot_dir(tmp.path())).unwrap();
        }
        let err = run(StartupParams::new(tmp.path(), key)).unwrap_err();
        assert!(err.to_string().contains("does not match"), "got: {err}");
    }

    /// Helper: seed `episodes` + `embeddings` rows under the persisted
    /// config's embedder identity. Used by the rebuild-from-SQL tests
    /// to populate the embeddings table without needing a real writer
    /// actor / runtime. Returns `(memory_id, rowid)` per content.
    fn seed_embeddings_for_current_embedder(
        tmp_path: &Path,
        key: &KeyMaterial,
        contents: &[&str],
    ) -> Vec<(String, i64)> {
        let cfg_path = tmp_path.join("solo.config.toml");
        let cfg = SoloConfig::read(&cfg_path).unwrap();
        let conn = open_sqlcipher(&per_tenant_db(tmp_path), key).unwrap();
        let identity = EmbedderIdentity {
            name: cfg.embedder.name.clone(),
            version: cfg.embedder.version.clone(),
            dim: cfg.embedder.dim,
            dtype: cfg.embedder.dtype.clone(),
        };
        let embedder_id = get_or_insert_embedder_id(&conn, &identity).unwrap();
        let dim = cfg.embedder.dim as usize;
        let now_ms = chrono::Utc::now().timestamp_millis();

        let mut out = Vec::new();
        for content in contents {
            let mid = insert_hot_episode(&conn, content);
            let rowid: i64 = conn
                .query_row(
                    "SELECT rowid FROM episodes WHERE memory_id = ?",
                    params![mid],
                    |r| r.get(0),
                )
                .unwrap();
            // Distinct vector per row so search ordering is well-defined.
            let mut bytes = vec![0u8; dim * 4];
            // Stamp the rowid into the first 8 bytes so vectors aren't all-zero.
            bytes[..8].copy_from_slice(&rowid.to_le_bytes());
            conn.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![mid, embedder_id, "f32", dim as i64, &bytes[..], now_ms],
            )
            .unwrap();
            out.push((mid, rowid));
        }
        drop(conn);
        out
    }

    /// Rebuild-from-SQL: when both snapshot pairs are missing, the
    /// startup chain populates the HNSW from the `embeddings` table for
    /// the active embedder. End state: `outcome.hnsw.len()` matches the
    /// number of active rows; drift is clean.
    #[test]
    fn run_rebuilds_hnsw_from_sql_when_no_snapshot() {
        let (tmp, key) = fresh_init_dir();
        seed_embeddings_for_current_embedder(tmp.path(), &key, &["a", "b", "c"]);

        // No snapshots exist (init doesn't write any).
        assert!(!snapshot::pair_exists(
            &snapshot_dir(tmp.path()),
            snapshot::LIVE_BASENAME
        ));

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();

        assert!(outcome.started_fresh, "no snapshot → started_fresh");
        assert_eq!(outcome.rebuild.rows_seen, 3);
        assert_eq!(outcome.rebuild.rows_added, 3, "all 3 active rows rebuilt");
        assert_eq!(outcome.rebuild.rows_skipped, 0);
        assert_eq!(outcome.hnsw.len(), 3);
        assert!(outcome.drift.is_clean(), "drift: {:?}", outcome.drift);
    }

    /// Rebuild excludes `episodes WHERE status = 'forgotten'`. Tombstone
    /// rebuild then adds the forgotten rowid to the in-memory tombstone
    /// set (no-op for our HNSW since we never added it, but the
    /// post-rebuild tombstone count matches SQL).
    #[test]
    fn run_rebuild_excludes_forgotten_episodes() {
        let (tmp, key) = fresh_init_dir();
        let seeded =
            seed_embeddings_for_current_embedder(tmp.path(), &key, &["keep1", "drop", "keep2"]);

        let conn = open_sqlcipher(&per_tenant_db(tmp.path()), &key).unwrap();
        conn.execute(
            "UPDATE episodes SET status = 'forgotten' WHERE memory_id = ?",
            params![seeded[1].0],
        )
        .unwrap();
        drop(conn);

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        assert_eq!(outcome.rebuild.rows_added, 2, "forgotten row skipped");
        assert_eq!(outcome.rebuild.rows_skipped, 0);
        assert_eq!(outcome.hnsw.len(), 2);
    }

    /// Corrupt-row resilience: a single bad embedding row (size mismatch)
    /// must NOT abort the rebuild. The healthy rows still land in the
    /// graph; the bad one is logged and counted in `rows_skipped`. Lets
    /// `solo doctor` and `solo reembed` keep running so the user can
    /// investigate from inside the product.
    #[test]
    fn run_rebuild_skips_corrupt_rows_and_continues() {
        let (tmp, key) = fresh_init_dir();
        let _seeded =
            seed_embeddings_for_current_embedder(tmp.path(), &key, &["good1", "bad", "good2"]);

        // Corrupt the middle memory's embedding: write a 4-byte blob
        // where dim*4 = 32*4 = 128 bytes are expected.
        let conn = open_sqlcipher(&per_tenant_db(tmp.path()), &key).unwrap();
        conn.execute(
            "UPDATE embeddings SET vector = ?, dim = ?
             WHERE memory_id = ?",
            params![&vec![0u8; 4][..], 32i64, _seeded[1].0],
        )
        .unwrap();
        drop(conn);

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        assert_eq!(outcome.rebuild.rows_seen, 3);
        assert_eq!(outcome.rebuild.rows_added, 2, "two healthy rows added");
        assert_eq!(outcome.rebuild.rows_skipped, 1, "corrupt row skipped");
        assert_eq!(outcome.hnsw.len(), 2);
    }

    /// Rebuild only walks rows where `embeddings.embedder_id` matches
    /// the active embedder. Stale rows under a previously-registered
    /// embedder (e.g. stub before BGE-M3 was wired in but the user
    /// hasn't run `solo reembed` yet) do NOT get added.
    #[test]
    fn run_rebuild_skips_rows_for_non_current_embedder() {
        let (tmp, key) = fresh_init_dir();
        // 1 row under the current embedder.
        seed_embeddings_for_current_embedder(tmp.path(), &key, &["ours"]);

        // Register a *second* embedder and write a stray embedding row
        // for a separate episode under it. Rebuild should ignore that.
        let conn = open_sqlcipher(&per_tenant_db(tmp.path()), &key).unwrap();
        let other_id = get_or_insert_embedder_id(
            &conn,
            &EmbedderIdentity {
                name: "other".into(),
                version: "v1".into(),
                dim: 32,
                dtype: "f32".into(),
            },
        )
        .unwrap();
        let stray_mid = insert_hot_episode(&conn, "stray");
        let zeros = vec![0u8; 32 * 4];
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![stray_mid, other_id, "f32", 32i64, &zeros[..], now],
        )
        .unwrap();
        drop(conn);

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        assert_eq!(
            outcome.rebuild.rows_added, 1,
            "only the row under the current embedder is rebuilt"
        );
        assert_eq!(outcome.rebuild.rows_skipped, 0);
        assert_eq!(outcome.hnsw.len(), 1);
    }

    /// Regression test for the post-reload tombstone bug: snapshot a
    /// non-empty HNSW, mark some episodes as `status='forgotten'` in SQL,
    /// then re-run startup. The `rebuild_tombstones_from_sql` step must
    /// re-insert those rowids into HnswIndex's tombstone set so drift
    /// detection stays clean and forgotten ids don't accidentally surface.
    #[test]
    fn run_rebuilds_tombstones_from_forgotten_episodes() {
        use solo_core::VectorIndex;
        let (tmp, key) = fresh_init_dir();
        let dim = 32usize;

        // Lay down a snapshot containing 3 vectors at rowids 1, 2, 3.
        {
            let factory = HnswFactory::default();
            let idx = factory.create(dim).unwrap();
            for i in 1..=3 {
                idx.add(i as i64, &vec![0.1f32; dim]).unwrap();
            }
            snapshot::save(&idx, &snapshot_dir(tmp.path())).unwrap();
        }

        // Insert 3 episodes (so SQL rowids 1, 2, 3 align with the HNSW
        // entries). Mark rowid=2 as forgotten.
        let conn = open_sqlcipher(&per_tenant_db(tmp.path()), &key).unwrap();
        let _ = insert_hot_episode(&conn, "first");
        let mid2 = insert_hot_episode(&conn, "second");
        let _ = insert_hot_episode(&conn, "third");
        conn.execute(
            "UPDATE episodes SET status='forgotten' WHERE memory_id = ?",
            params![mid2],
        )
        .unwrap();
        drop(conn);

        let outcome = run(StartupParams::new(tmp.path(), key)).unwrap();
        // 3 vectors total - 1 tombstoned = 2 visible.
        assert_eq!(outcome.hnsw.len(), 2);
        // Drift: hot+active in SQL is 2, hnsw.len() is 2 → clean.
        assert!(
            outcome.drift.is_clean(),
            "expected clean drift after tombstone rebuild, got: {:?}",
            outcome.drift
        );
        // Search for vector 2 must exclude rowid 2.
        let hits = outcome.hnsw.search(&vec![0.1f32; dim], 5).unwrap();
        assert!(
            !hits.iter().any(|(r, _)| *r == 2),
            "rowid 2 should be tombstoned: hits={hits:?}"
        );
    }
}
