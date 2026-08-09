// SPDX-License-Identifier: Apache-2.0

//! Per-tenant handle: writer-actor + reader pool + HNSW + embedder bundled
//! into a single resource. v0.8.0 P2.
//!
//! ## Design
//!
//! Each tenant in the data dir has:
//!
//!   * Its own SQLCipher DB at `<data_dir>/tenants/<tenant_id>.db` (P1 layout).
//!   * Its own writer-actor on a dedicated OS thread (ADR-0003 model
//!     preserved per-tenant; see ADR-0004 for the multi-tenant invariants
//!     ADR-0004 adds on top of ADR-0003).
//!   * Its own reader pool (default size 2).
//!   * Its own HNSW index loaded from per-tenant snapshot files.
//!   * Its own resolved `embedder_id` for the persisted embedder identity.
//!   * A shared `Arc<dyn Embedder>` (a single embedder backend instance is
//!     re-used across tenants — embedders are stateless, no per-tenant
//!     state required).
//!
//! `LibraryHandle::open` runs the full per-tenant startup chain (open DB,
//! migrate schema, load HNSW snapshot with fallbacks, rebuild from SQL on
//! empty snapshot, rebuild tombstones, replay `pending_index`, spawn the
//! writer-actor, build the reader pool). On shutdown it saves a final
//! snapshot, drains the writer thread, and drops the pool.
//!
//! ## HNSW snapshot layout
//!
//! Per-tenant **subdir** layout: `<data_dir>/tenants/<tenant_id>/<basename>.hnsw.{data,graph}`.
//! Per-tenant DB stays as a flat-file in `<data_dir>/tenants/<tenant_id>.db`
//! (same shape as P1 left it). The subdir per tenant cleanly isolates the
//! snapshot files; a future `solo tenants backup <id>` can tarball
//! `<data_dir>/tenants/<id>/` plus `<data_dir>/tenants/<id>.db` (and
//! sidecars) without globbing by prefix.
//!
//! For the `default` tenant migrated from v0.7.1, the P1 helper placed
//! snapshots flat in `<data_dir>/tenants/`. `LibraryHandle::open` for
//! `default` upgrades that flat layout to `<data_dir>/tenants/default/`
//! lazily on first open (renames the four/six HNSW files into the subdir).
//! Idempotent.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::runtime::Handle as TokioHandle;
use tokio::sync::RwLock as AsyncRwLock;
use tokio::sync::broadcast;

use rusqlite::Connection;
use solo_core::{
    Embedder, Error, InvalidateEvent, LibraryId, Result, VectorIndex, VectorIndexFactory,
};

use crate::steward_factory::StewardFactory;

use crate::audit::{AuditWriter, AuditWriterShutdown, purge_older_than};
use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
use crate::hnsw_rebuild::{rebuild_chunk_tombstones_from_sql, rebuild_episode_tombstones_from_sql};
use crate::init::open_sqlcipher;
use crate::key_material::KeyMaterial;
use crate::migration;
use crate::reader::ReaderPool;
use crate::recovery::{
    DriftReport, RebuildReport, ReplayReport, detect_drift, rebuild_hnsw_from_sql,
    replay_pending_index,
};
use crate::snapshot;
use crate::vector_index::{HnswFactory, HnswIndex, HnswParams};
use crate::writer::{INVALIDATE_BROADCAST_CAPACITY, WriteHandle, WriterActor, WriterSpawn};

/// HNSW snapshot file suffixes (mirrors `snapshot::DATA_SUFFIX` / `GRAPH_SUFFIX`
/// which are private to that module).
const HNSW_DATA_SUFFIX: &str = ".hnsw.data";
const HNSW_GRAPH_SUFFIX: &str = ".hnsw.graph";

/// Per-tenant handle. Cheap to clone via `Arc<LibraryHandle>` (the registry
/// owns the Arc; callers borrow `&LibraryHandle`).
pub struct LibraryHandle {
    tenant_id: LibraryId,
    config: crate::config::SoloConfig,
    key: Option<KeyMaterial>,
    db_path: PathBuf,
    snapshot_dir: PathBuf,
    embedder_id: i64,
    hnsw: Arc<dyn VectorIndex + Send + Sync>,
    embedder: Arc<dyn Embedder>,
    // Writer side: hold the WriteHandle (clone-cheap) and the OS-thread join
    // handle. On shutdown, drop the handle then join the thread.
    write: WriteHandle,
    /// Only `Some` between `open` and `shutdown_all`. `take()`-d during the
    /// shutdown sequence so the join runs to completion. After shutdown the
    /// LibraryHandle is consumed.
    writer_join: Option<std::thread::JoinHandle<()>>,
    read: ReaderPool,
    /// v0.8.0 P4: async audit writer for the query path. Cheap to clone
    /// (mpsc sender). The drainer's join handle lives in
    /// `audit_shutdown`.
    audit: AuditWriter,
    /// v0.8.0 P4: shutdown handle for the audit drainer task. `take()`-d
    /// during shutdown so we can `.join()` it after dropping every
    /// `AuditWriter` clone (i.e., closing the channel).
    audit_shutdown: Mutex<Option<AuditWriterShutdown>>,
    /// v0.8.0 P4: optional background retention-sweep task. Spawned only
    /// when both `[audit] retention_days` AND `[audit] purge_interval_secs`
    /// are set in `solo.config.toml`. Aborted on shutdown.
    audit_sweep_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Replay statistics from open (advisory; surfaced for logging).
    replay: ReplayReport,
    /// Drift report from open (advisory).
    drift: DriftReport,
    used_bak_snapshot: bool,
    started_fresh: bool,
    rebuild: RebuildReport,
    /// v0.9.0 P0c: lazily-populatable Steward slot (per plan §6
    /// "Steward placement" / MAJOR 1 resolution).
    ///
    /// Static backends (Anthropic / OpenAI / Ollama / `None`) populate
    /// this eagerly at `LibraryHandle::open` via the configured
    /// `StewardFactory::build()` — `slot.read()` always observes
    /// `Some(steward)` after open, with zero hot-path lock-contention
    /// cost (the `RwLock` is uncontested).
    ///
    /// The MCP-sampling backend leaves the slot `None` at open time
    /// (its factory's `build()` is a no-op that returns `Ok(None)`).
    /// v0.9.0 P2's `SoloMcpServer::initialize` hook bypasses the
    /// factory entirely on the sampling path: it builds the
    /// sampling-backed `Arc<Steward>` via
    /// `solo_api::llm::build_sampling_steward` and writes it into this
    /// slot directly. The writer-actor's consolidate path (currently a
    /// captured `Arc<Steward>` field; scheduled to read this slot per
    /// command in P4) falls through to "no LLM available" when the
    /// slot is empty — same path as v0.8.x's `LlmClient::is_real_llm()
    /// == false` short-circuit.
    ///
    /// Public via [`Self::steward_slot`] so callers (P2's MCP
    /// `initialize` hook + P4's writer-actor slot-reading path) can
    /// `.read()` / `.write()` on the slot.
    steward_slot: Arc<AsyncRwLock<Option<Arc<solo_steward::Steward>>>>,
    /// v0.10.0: per-tenant broadcast channel for graph-data
    /// invalidation events fanned out to `GET /v1/graph/stream` SSE
    /// subscribers in `solo-api`.
    ///
    /// Sender side is captured by the writer-actor (which calls
    /// `tx.send(...).ok()` AFTER every successful commit) AND held
    /// here for non-writer-actor mutation paths (notably
    /// `gdpr::forget_principal`, which goes around the writer-actor
    /// per its own docstring). SSE subscribers call
    /// `tenant.invalidate_sender().subscribe()` to get a `Receiver`.
    ///
    /// Capacity is [`crate::writer::INVALIDATE_BROADCAST_CAPACITY`].
    /// Lagged subscribers see `RecvError::Lagged(n)` and resync via
    /// the next event — see [`solo_core::InvalidateEvent`] for the
    /// "events are idempotent refetch signals" invariant.
    invalidate_tx: broadcast::Sender<InvalidateEvent>,
}

/// Snapshot layout for one tenant inside `<data_dir>/tenants/`.
///
/// As of v0.8.0 P2 each tenant gets its own subdir
/// `<data_dir>/tenants/<tenant_id>/` holding the HNSW snapshot pairs.
/// `LibraryHandle::open` creates the subdir on first use and (for the
/// `default` tenant migrated from v0.7.1) migrates any flat-layout
/// snapshots from `<data_dir>/tenants/` into the subdir on first open.
fn per_tenant_snapshot_dir(data_dir: &Path, _tenant_id: &LibraryId) -> PathBuf {
    data_dir.to_path_buf()
}

/// Parameters for opening the one Community Memory Library runtime.
pub struct LibraryOpenParams {
    pub data_dir: PathBuf,
    pub key: KeyMaterial,
    pub embedder: Arc<dyn Embedder>,
    pub hnsw_params: HnswParams,
    /// Optional Steward (LLM-driven consolidation). Wired only when the
    /// daemon/CLI was started with a real `LlmClient` configured.
    pub steward: Option<Arc<solo_steward::Steward>>,
    /// Optional tokio runtime handle. Required when `embedder` is wired
    /// (the writer-actor's blocking thread `block_on`s embedder calls
    /// during reembed). For pure-storage tests that don't spawn a runtime
    /// inside the writer, this can be `None`.
    pub runtime_handle: Option<TokioHandle>,
    /// v0.9.0 P0c: optional `StewardFactory` used to eagerly populate
    /// the new `LibraryHandle::steward_slot` at open time (per plan §6
    /// "Steward placement" / MAJOR 1 resolution).
    ///
    /// When `Some(factory)`, `LibraryHandle::open` calls
    /// `factory.build()` and writes the result (which may be
    /// `Some(Arc<Steward>)` for static backends or `None` for
    /// MCP-sampling) into the slot. When `None`, the slot stays empty
    /// — backwards-compat path for v0.8.x callers that still pass
    /// `steward: Option<Arc<Steward>>` to the writer-actor directly.
    /// v0.9.0 P4 will replace the writer-actor's captured-Steward
    /// path with slot-reading; until then, both routes exist side-by-
    /// side.
    pub steward_factory: Option<Arc<dyn StewardFactory>>,
    /// v0.9.0 P4-revision (P4 audit M1): optional count-based trigger
    /// signal threaded into the writer-actor so it can ping the
    /// daemon's `triples_batch_timer` after every successful
    /// `Remember`. `None` for tests + paths that don't drive the
    /// count-based trigger; the daemon constructs one
    /// `Arc<TriplesBatchSignal>` and clones it into both
    /// `LibraryOpenParams::triples_batch_signal` AND its own
    /// `triples_batch_timer` select arm.
    pub triples_batch_signal: Option<Arc<crate::triples_batch::TriplesBatchSignal>>,
}

impl LibraryHandle {
    /// Open the Community Memory Library. Reads `solo.config.toml` for the
    /// embedder identity, applies migrations, loads the HNSW snapshot (with `.bak`
    /// fallback and SQL-rebuild fallback), replays `pending_index`, runs
    /// drift detection, and spawns the writer-actor.
    ///
    /// The returned `LibraryHandle` is ready for read + write requests.
    pub(crate) fn open(params: LibraryOpenParams) -> Result<Self> {
        let LibraryOpenParams {
            data_dir,
            key,
            embedder,
            hnsw_params,
            steward,
            runtime_handle,
            steward_factory,
            triples_batch_signal,
        } = params;

        // Read the canonical config from `<data_dir>/solo.config.toml`.
        // v0.8.0 P2: one config per data dir, not per tenant. The embedder
        // identity in the config is the deployment-wide identity; per-tenant
        // embedder swaps are a v0.8.1+ concern.
        let config_path = data_dir.join("solo.config.toml");
        let config = crate::config::SoloConfig::read(&config_path)?;
        let dim = config.embedder.dim as usize;
        if dim == 0 {
            return Err(Error::storage(format!(
                "solo.config.toml records embedder.dim=0 — corrupt config? at {config_path:?}"
            )));
        }

        let tenant_id = LibraryId::default_tenant();
        let db_path = data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME);
        let snapshot_dir = per_tenant_snapshot_dir(&data_dir, &tenant_id);
        std::fs::create_dir_all(&snapshot_dir).map_err(|e| {
            Error::storage(format!(
                "create Community Memory Library snapshot dir {}: {e}",
                snapshot_dir.display()
            ))
        })?;

        // The Community database must already exist after `solo init` or a
        // safe legacy-layout promotion. A missing file is a corruption or
        // setup signal, not a request to create another database.
        if !db_path.is_file() {
            return Err(Error::not_found(format!(
                "Community Memory Library database not found at {}. Run `solo init` \
                 or restore the library from backup.",
                db_path.display()
            )));
        }

        // Open the init connection used for migrations + startup chain.
        let mut conn: Connection = open_sqlcipher(&db_path, &key)?;

        // Run per-tenant migrations idempotently.
        let _schema_version = migration::run_migrations(&mut conn)?;

        // Resolve embedder_id from the persisted config. The embedder
        // identity is the same across every tenant in v0.8.0 P2 (one
        // config per data dir), so this row gets the same id from each
        // tenant's `embedders` table.
        let embedder_identity = EmbedderIdentity {
            name: config.embedder.name.clone(),
            version: config.embedder.version.clone(),
            dim: config.embedder.dim,
            dtype: config.embedder.dtype.clone(),
        };
        let embedder_id = get_or_insert_embedder_id(&conn, &embedder_identity)?;

        // Load HNSW snapshot with the same three-way fallback as the
        // single-tenant startup chain.
        let factory = HnswFactory::with_params(hnsw_params);
        let (hnsw_index, used_bak_snapshot, started_fresh) =
            load_hnsw_with_fallback(&snapshot_dir, &factory, dim);

        if !started_fresh && hnsw_index.dim() != dim {
            return Err(Error::storage(format!(
                "tenant {tenant_id}: HNSW snapshot dim ({}) does not match \
                 solo.config.toml embedder.dim ({dim}). Embedder identity has \
                 shifted under the daemon. Run `solo reembed` to rebuild.",
                hnsw_index.dim()
            )));
        }

        // Rebuild from SQL when no snapshot was loadable.
        let rebuild = if started_fresh {
            let started = std::time::Instant::now();
            let r = rebuild_hnsw_from_sql(&conn, &hnsw_index, embedder_id)?;
            if r.rows_seen > 0 {
                tracing::info!(
                    tenant = %tenant_id,
                    rows_seen = r.rows_seen,
                    rows_added = r.rows_added,
                    rows_skipped = r.rows_skipped,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "library: rebuilt HNSW from embeddings after empty-snapshot fallback"
                );
            }
            r
        } else {
            RebuildReport::default()
        };

        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(hnsw_index);

        let (forgotten, forgotten_chunks) = if started_fresh {
            (0, 0)
        } else {
            // Dev-log 0154: shared with the default-data-dir startup
            // path via `crate::hnsw_rebuild`.
            let eps = rebuild_episode_tombstones_from_sql(&conn, hnsw.as_ref())?;
            let chunks = rebuild_chunk_tombstones_from_sql(&conn, hnsw.as_ref())?;
            (eps, chunks)
        };
        if forgotten_chunks > 0 {
            tracing::info!(
                tenant = %tenant_id,
                forgotten_chunks,
                "library: rebuilt HNSW tombstones from chunks of forgotten documents"
            );
        }
        if forgotten > 0 {
            tracing::info!(
                tenant = %tenant_id,
                forgotten,
                "library: rebuilt HNSW tombstones from forgotten episodes"
            );
        }

        let replay = replay_pending_index(&mut conn, hnsw.as_ref())?;
        let drift = detect_drift(&conn, hnsw.as_ref())?;
        drop(conn);

        // Build the reader pool.
        let pool = ReaderPool::new(&db_path, Some(key.clone()), hnsw.clone())?;

        // v0.8.0 P5: build the redaction registry from the per-data-dir
        // config. Disabled by default; the writer-actor's per-write
        // path short-circuits via `RedactionRegistry::is_enabled`. Invalid
        // custom regexes here surface as `LibraryHandle::open` errors so
        // operators see the problem at startup, not at first write.
        let redactor = Arc::new(crate::redaction::RedactionRegistry::from_config(
            &config.redaction,
        )?);

        // v0.9.0 P0c: build the Steward slot before spawning the writer-
        // actor. The slot's initial value is sourced from the
        // StewardFactory when one is wired (preferred v0.9.0 path); the
        // backwards-compat v0.8.x path (no factory passed in) seeds the
        // slot from the eager `steward: Option<Arc<Steward>>` parameter
        // so existing callers see consistent behavior between the
        // writer-actor's captured Steward and the new slot.
        //
        // v0.9.0 P4a: the writer-actor now reads `steward_slot` per
        // command (via `WriterActor::current_steward`) so the slot is
        // the canonical source of truth. The eagerly-captured
        // `self.steward` field is the FALLBACK — used only when the
        // slot is empty AND we're on the v0.8.x callers' path that
        // doesn't plumb the slot through `spawn_full_with_quota_and_slot`.
        let initial_slot_value: Option<Arc<solo_steward::Steward>> =
            if let Some(factory) = steward_factory.as_ref() {
                factory.build()?
            } else {
                steward.clone()
            };
        let steward_slot = Arc::new(AsyncRwLock::new(initial_slot_value));

        // v0.10.0: build the per-tenant invalidation broadcast channel
        // BEFORE spawning the writer-actor so the writer can capture
        // the Sender. The LibraryHandle keeps a clone of the Sender so
        // non-writer-actor mutation paths (notably
        // `gdpr::forget_principal`) can also emit invalidations.
        let (invalidate_tx, _initial_rx) =
            broadcast::channel::<InvalidateEvent>(INVALIDATE_BROADCAST_CAPACITY);
        // The initial Receiver is dropped immediately — it exists only
        // to let `broadcast::channel` succeed; real subscribers come
        // from SSE handlers calling `invalidate_tx.subscribe()`.

        // Spawn the writer-actor. We always wire embedder + (optional)
        // steward + key + runtime handle when one is available. For pure
        // tests that pass `runtime_handle: None`, we fall back to the
        // simpler spawn variant that doesn't try to capture a runtime.
        let writer_conn = open_sqlcipher(&db_path, &key)?;

        let WriterSpawn {
            handle: write,
            join,
        } = if let Some(rt) = runtime_handle.clone() {
            // v0.8.1 P3 + v0.9.0 P4a + v0.10.0: pass the cached quota +
            // db_path so the writer can enforce per-write, PLUS the
            // steward_slot so the writer-actor's consolidate path can
            // observe late-bound sampling-backed Stewards (populated by
            // the MCP-initialize hook after writer spawn), PLUS the
            // per-tenant invalidate broadcast Sender so post-commit
            // mutations fan out to SSE subscribers. When quota_bytes is
            // None (the common case for v0.8.0 tenants), the writer's
            // per-write check short-circuits on the
            // QuotaDecision::Unlimited branch in one Option compare.
            WriterActor::spawn_full_with_invalidate(
                writer_conn,
                hnsw.clone(),
                snapshot_dir.clone(),
                embedder_id,
                embedder.clone(),
                steward,
                key.clone(),
                rt,
                redactor,
                None,
                db_path.clone(),
                steward_slot.clone(),
                triples_batch_signal,
                invalidate_tx.clone(),
                tenant_id.to_string(),
            )
        } else {
            WriterActor::spawn_full(writer_conn, hnsw.clone(), snapshot_dir.clone(), embedder_id)
        };

        // v0.8.0 P4: spawn the async audit drainer. Uses the same key as
        // the writer; opens its own SQLCipher connection lazily on first
        // event. Requires a tokio runtime to be live when this is called
        // (every prod path goes through `TenantRegistry::get_or_open`
        // which calls this inside `spawn_blocking` on a runtime).
        let (audit, audit_shutdown) = AuditWriter::spawn(db_path.clone(), Some(key.clone()));

        // v0.8.0 P4: optional background retention sweep. Spawned only if
        // both `retention_days` and `purge_interval_secs` are configured.
        let audit_sweep_handle = spawn_audit_sweep(
            &tenant_id,
            &db_path,
            &key,
            &config.audit,
            runtime_handle.clone(),
        );

        Ok(LibraryHandle {
            tenant_id,
            config,
            key: Some(key.clone()),
            db_path,
            snapshot_dir,
            embedder_id,
            hnsw,
            embedder,
            write,
            writer_join: Some(join),
            read: pool,
            audit,
            audit_shutdown: Mutex::new(Some(audit_shutdown)),
            audit_sweep_handle: Mutex::new(audit_sweep_handle),
            replay,
            drift,
            used_bak_snapshot,
            started_fresh,
            rebuild,
            steward_slot,
            invalidate_tx,
        })
    }

    pub fn tenant_id(&self) -> &LibraryId {
        &self.tenant_id
    }
    pub fn config(&self) -> &crate::config::SoloConfig {
        &self.config
    }
    pub fn key(&self) -> Option<&KeyMaterial> {
        self.key.as_ref()
    }
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }
    pub fn snapshot_dir(&self) -> &Path {
        &self.snapshot_dir
    }
    pub fn embedder_id(&self) -> i64 {
        self.embedder_id
    }
    pub fn write(&self) -> &WriteHandle {
        &self.write
    }
    pub fn read(&self) -> &ReaderPool {
        &self.read
    }
    pub fn hnsw(&self) -> &Arc<dyn VectorIndex + Send + Sync> {
        &self.hnsw
    }
    pub fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }
    pub fn replay(&self) -> &ReplayReport {
        &self.replay
    }
    pub fn drift(&self) -> &DriftReport {
        &self.drift
    }
    pub fn used_bak_snapshot(&self) -> bool {
        self.used_bak_snapshot
    }
    pub fn started_fresh(&self) -> bool {
        self.started_fresh
    }
    pub fn rebuild(&self) -> &RebuildReport {
        &self.rebuild
    }
    /// v0.8.0 P4: cloneable async audit writer for query paths.
    pub fn audit(&self) -> &AuditWriter {
        &self.audit
    }

    /// v0.10.0: per-tenant invalidation broadcast sender.
    ///
    /// SSE handlers (`GET /v1/graph/stream`) call
    /// `tenant.invalidate_sender().subscribe()` to get a
    /// `broadcast::Receiver<InvalidateEvent>` that fires whenever this
    /// tenant's writer-actor commits a mutating write (or `gdpr.forget_user`
    /// completes its non-writer-actor delete cascade). The wire shape is
    /// defined by [`solo_core::InvalidateEvent`]; capacity is
    /// [`crate::writer::INVALIDATE_BROADCAST_CAPACITY`].
    ///
    /// Returns a reference; callers clone the inner `Sender` (cheap —
    /// it's an `Arc`-backed handle) only if they need to hold one across
    /// an async boundary that drops the borrow. Most callers just
    /// `.subscribe()` and consume the `Receiver`.
    pub fn invalidate_sender(&self) -> &broadcast::Sender<InvalidateEvent> {
        &self.invalidate_tx
    }

    /// v0.9.0 P0c: lazily-populatable Steward slot.
    ///
    /// Returns the slot's `Arc` so callers can `.read()` / `.write()`
    /// on the inner `tokio::sync::RwLock<Option<Arc<Steward>>>`.
    ///
    /// Static backends (Anthropic / OpenAI / Ollama / None) populate
    /// this eagerly at open time — `slot.read().await.is_some()`
    /// returns `true` immediately. The MCP-sampling backend leaves the
    /// slot empty until v0.9.0 P2's `SoloMcpServer::initialize` hook
    /// writes a peer-bound Steward into it.
    ///
    /// See `crates/solo-storage/src/steward_factory.rs` for the trait
    /// and the locked plan
    /// (`docs/dev-log/0098-v0.9.0-implementation-plan.md` §6 "Steward
    /// placement") for the full design rationale.
    pub fn steward_slot(&self) -> &Arc<AsyncRwLock<Option<Arc<solo_steward::Steward>>>> {
        &self.steward_slot
    }

    /// Assemble a `LibraryHandle` from already-constructed parts. Used by
    /// test harnesses (in solo-api / solo-query) that build a writer +
    /// reader pool + HNSW manually against a non-SQLCipher test DB and
    /// don't want to go through `LibraryHandle::open` (which assumes a
    /// real SQLCipher-encrypted file).
    ///
    /// Production callers MUST go through `LibraryHandle::open` via
    /// `TenantRegistry::get_or_open`.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts_for_tests(
        tenant_id: LibraryId,
        config: crate::config::SoloConfig,
        db_path: PathBuf,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        embedder: Arc<dyn Embedder>,
        write: WriteHandle,
        writer_join: std::thread::JoinHandle<()>,
        read: ReaderPool,
    ) -> Self {
        // v0.8.0 P4: test harnesses get a no-op audit writer by default.
        // Tests that need real audit emission can `assemble_for_tests`
        // with `with_audit` afterwards.
        let audit = AuditWriter::noop();
        // v0.10.0: test harness gets its own broadcast Sender; the
        // writer-actor used by these harness paths is spawned through
        // the legacy `WriterActor::spawn_full` (no invalidate plumb), so
        // no events flow from the writer side. SSE handler-level tests
        // that exercise the broadcast path use the from_parts_for_tests
        // shape but emit invalidations explicitly via
        // `tenant.invalidate_sender().send(...)`.
        let (invalidate_tx, _initial_rx) =
            broadcast::channel::<InvalidateEvent>(INVALIDATE_BROADCAST_CAPACITY);
        Self {
            tenant_id,
            config,
            key: None,
            db_path,
            snapshot_dir,
            embedder_id,
            hnsw,
            embedder,
            write,
            writer_join: Some(writer_join),
            read,
            audit,
            audit_shutdown: Mutex::new(None),
            audit_sweep_handle: Mutex::new(None),
            replay: ReplayReport::default(),
            drift: DriftReport::default(),
            used_bak_snapshot: false,
            started_fresh: true,
            rebuild: RebuildReport::default(),
            // v0.9.0 P0c: test harnesses get an empty Steward slot by
            // default. Tests that need a populated slot can write into
            // it via `tenant_handle.steward_slot().write().await`.
            steward_slot: Arc::new(AsyncRwLock::new(None)),
            invalidate_tx,
        }
    }

    /// Test-support variant that carries tenant key material so HTTP/API
    /// tests can exercise encrypted sidecar decode paths without opening a
    /// full SQLCipher-backed tenant.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts_for_tests_with_key(
        tenant_id: LibraryId,
        config: crate::config::SoloConfig,
        db_path: PathBuf,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        embedder: Arc<dyn Embedder>,
        write: WriteHandle,
        writer_join: std::thread::JoinHandle<()>,
        read: ReaderPool,
        key: KeyMaterial,
    ) -> Self {
        let mut handle = Self::from_parts_for_tests(
            tenant_id,
            config,
            db_path,
            snapshot_dir,
            embedder_id,
            hnsw,
            embedder,
            write,
            writer_join,
            read,
        );
        handle.key = Some(key);
        handle
    }

    /// Graceful shutdown:
    /// 1. Save a final HNSW snapshot (best-effort; logged on failure).
    /// 2. Drop the WriteHandle to close the mpsc channel.
    /// 3. Join the writer-actor's OS thread so it completes
    ///    `wal_checkpoint(TRUNCATE)` before this returns.
    /// 4. Drop the reader pool (must happen inside a tokio runtime).
    ///
    /// Optionally skip the snapshot save (used by `solo reembed`, which
    /// deliberately wipes snapshots).
    pub async fn shutdown(mut self, save_snapshot: bool) -> Result<()> {
        if save_snapshot && let Err(e) = self.write.save_snapshot().await {
            tracing::warn!(
                tenant = %self.tenant_id,
                error = %e,
                "library shutdown: final snapshot save failed (continuing)"
            );
        }
        // v0.8.0 P4: abort the background retention sweep task (if any).
        // Survive poisoned mutex: we're tearing down anyway, so a prior
        // panic on the lock-holding path shouldn't escalate to a second
        // panic during shutdown.
        if let Some(handle) = self
            .audit_sweep_handle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            handle.abort();
        }
        // Drop the AuditWriter and wait for the drainer to flush + exit.
        // Order matters: drop the writer first (so the mpsc channel
        // closes after the in-flight events drain), then join the drainer.
        let audit_shutdown = self
            .audit_shutdown
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        // The handle's own audit clone is implicitly dropped when `self`
        // drops, but we drop it explicitly here so the drainer sees the
        // close-signal before we await the join below.
        // Replace `self.audit` with a noop so the field stays valid for
        // the rest of `self`'s drop sequence.
        let _ = std::mem::replace(&mut self.audit, AuditWriter::noop());
        if let Some(shutdown) = audit_shutdown {
            shutdown.join().await;
        }

        // Drop the handle so the actor exits.
        let write = self.write;
        drop(write);
        if let Some(join) = self.writer_join.take() {
            // Join on a blocking task so we don't hold the tokio runtime
            // off its workers while the OS thread is closing files.
            tokio::task::spawn_blocking(move || {
                if let Err(panic) = join.join() {
                    tracing::error!(?panic, "library: writer thread panicked on shutdown");
                }
            })
            .await
            .ok();
        }
        // ReaderPool drops here when self drops; explicit no-op so the
        // intent is documented.
        drop(self.read);
        Ok(())
    }
}

// Dev-log 0154: per-tenant copies of `rebuild_*tombstones_from_sql`
// were lifted into `crate::hnsw_rebuild` and the default-data-dir
// `startup.rs` path now shares the same impl.

/// Try the live snapshot, then `.bak`, then fall back to a fresh empty
/// index. Same as `startup::load_hnsw_with_fallback`; the per-tenant copy
/// uses the same logic.
fn load_hnsw_with_fallback(
    snapshot_dir: &Path,
    factory: &HnswFactory,
    dim: usize,
) -> (HnswIndex, bool, bool) {
    match snapshot::load(snapshot_dir) {
        Ok(idx) => {
            tracing::info!(
                snapshot_kind = "live",
                dim = idx.dim(),
                len = idx.len(),
                "library HNSW loaded from live snapshot"
            );
            (idx, false, false)
        }
        Err(primary_err) => {
            tracing::warn!(error = %primary_err, "library: live HNSW snapshot failed; trying .bak");
            match snapshot::load_bak(snapshot_dir) {
                Ok(idx) => {
                    tracing::warn!(
                        snapshot_kind = "bak",
                        dim = idx.dim(),
                        len = idx.len(),
                        "library HNSW loaded from backup snapshot — investigate the live pair"
                    );
                    (idx, true, false)
                }
                Err(bak_err) => {
                    tracing::warn!(
                        primary = %primary_err,
                        bak = %bak_err,
                        dim,
                        "library: no HNSW snapshot available; starting fresh empty index"
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

/// v0.8.0 P4: spawn a per-tenant background retention sweep, gated on
/// `[audit] retention_days` + `[audit] purge_interval_secs` both being
/// set. Returns `None` when:
///
///   * Either knob is `None` (sweep is opt-in).
///   * `runtime_handle` is `None` (test harnesses without a tokio runtime).
///
/// The spawned task wakes every `purge_interval_secs`, opens its own
/// SQLCipher connection (separate from the writer-actor's), and calls
/// `purge_older_than(now - retention_days * 86400_000)`. Failures are
/// logged + retried at the next tick; we don't crash the task on a
/// transient SQLite error.
///
/// Aborted by `LibraryHandle::shutdown`.
fn spawn_audit_sweep(
    tenant_id: &LibraryId,
    db_path: &Path,
    key: &KeyMaterial,
    audit_cfg: &crate::config::AuditSettings,
    runtime_handle: Option<TokioHandle>,
) -> Option<tokio::task::JoinHandle<()>> {
    let retention_days = audit_cfg.retention_days?;
    let interval_secs = audit_cfg.purge_interval_secs?;
    let rt = runtime_handle?;

    let tenant = tenant_id.clone();
    let path = db_path.to_path_buf();
    let key = key.clone();
    let interval = std::time::Duration::from_secs(interval_secs);

    Some(rt.spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately; consume + discard so the first
        // real sweep happens AFTER `interval` (not at-startup, when there
        // typically isn't anything past retention yet).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let cutoff_ms =
                chrono::Utc::now().timestamp_millis() - i64::from(retention_days) * 86_400_000;
            let path = path.clone();
            let key = key.clone();
            let tenant = tenant.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                let mut conn = match open_sqlcipher(&path, &key) {
                    Ok(c) => c,
                    Err(e) => return Err(e),
                };
                purge_older_than(&mut conn, cutoff_ms)
            })
            .await;
            match outcome {
                Ok(Ok(deleted)) if deleted > 0 => tracing::info!(
                    tenant = %tenant,
                    deleted,
                    cutoff_ms,
                    "audit retention sweep purged rows"
                ),
                Ok(Ok(_)) => tracing::debug!(
                    tenant = %tenant,
                    "audit retention sweep ran (nothing to purge)"
                ),
                Ok(Err(e)) => tracing::warn!(
                    tenant = %tenant,
                    error = %e,
                    "audit retention sweep failed (will retry next interval)"
                ),
                Err(e) => tracing::warn!(
                    tenant = %tenant,
                    error = %e,
                    "audit retention sweep join failed"
                ),
            }
        }
    }))
}
