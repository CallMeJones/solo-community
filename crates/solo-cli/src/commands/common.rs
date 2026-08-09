// SPDX-License-Identifier: Apache-2.0

//! Shared setup for one-shot subcommands (`remember`, `recall`, `forget`,
//! `inspect`).
//!
//! One-shot commands have the same opening choreography as `solo daemon`:
//!
//!   1. Resolve `--data-dir`.
//!   2. Read `solo.config.toml` (must exist; refuses with a clean error
//!      if missing → "did you run `solo init`?").
//!   3. Resolve passphrase (env var or prompt).
//!   4. Derive `KeyMaterial` via Argon2id with the persisted salt.
//!   5. Acquire `solo.lock` (refuses if a daemon or another one-shot is
//!      running — ADR-0003 §P8-I multi-instance protection).
//!   6. Run the startup chain (`solo_storage::startup::run`).
//!   7. Build a writer + reader pool + embedder.
//!   8. Hand off to the command body.
//!   9. On completion: drop write handle → wait for writer thread → drop
//!      read pool → drop lockfile.
//!
//! The struct returned ([`OneShotContext`]) bundles everything the command
//! body needs. Call `context.shutdown().await` to flush + close cleanly;
//! the `Drop` implementation does best-effort teardown but doesn't wait
//! for the writer thread, so prefer the explicit shutdown.

use anyhow::{Context, Result, bail};
use solo_core::Embedder;
use solo_storage::{
    AuditOperation, AuditResult, HnswParams, KeyMaterial, LibraryHandle, Lockfile, MemoryLibrary,
    MemoryLibraryParams, SoloConfig, StubEmbedder, WriteHandle, build_embedder_from_env,
    default_data_dir, insert_audit_admin_row, open_sqlcipher,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const ENV_PASSPHRASE: &str = "SOLO_PASSPHRASE";

/// Build the embedder per the env precedence + persisted config.
///
/// Thin wrapper over [`solo_storage::build_embedder_from_env`]. The
/// storage builder applies the env precedence (Ollama → Stub; BGE-M3
/// was removed in v0.6.0 — see `docs/dev-log/0071-v0.5.x-roadmap.md`
/// Priority 9). This wrapper handles two CLI-specific concerns:
///
/// 1. **Stub identity continuity.** Storage's `default_stub()` is
///    `(stub, v1, 32)` — generic. The CLI persists the *real* embedder
///    identity (`config.embedder.name/version/dim`) into the
///    `embedder_registry` table and keys rows by `embedder_id`. If we
///    returned the generic stub here, every existing stub-mode data
///    directory from earlier releases would orphan its rows (different
///    identity → different `embedder_id`). So when storage hands us a
///    stub, we rebuild it with the persisted config identity.
///
/// 2. **Dim-vs-config defense in depth.** The chosen backend's dim
///    must match `config.embedder.dim` before we proceed. For stub the
///    rebuild above guarantees a match; for Ollama this guards against
///    a backend swap (or pointing at a different model with a
///    different dim) without running `solo init --force`. A mismatch
///    here would silently miscompare during HNSW recall.
///
/// ## Mixed-embedder warning
///
/// `StubEmbedder` and `OllamaEmbedder` produce vectors in **different
/// vector spaces**. Stub vectors are deterministic BLAKE3 hashes
/// (identity-match only). Ollama produces real semantic embeddings.
/// Switching backends against a non-empty database requires
/// `solo reembed` to regenerate every vector with the active embedder.
/// See ADR-0003 §"Forward-pointing notes — Reembed design".
pub fn build_embedder(config: &SoloConfig) -> Result<Arc<dyn Embedder>> {
    // v0.9.0 P3: if the persisted config says `bundled:*` AND env doesn't
    // override, build the BundledEmbedder directly — bypass the
    // env-precedence path which would otherwise hand back a stub for
    // an unset SOLO_EMBEDDER. Without this branch a `solo init` that
    // wrote `bundled:all-MiniLM-L6-v2` and dim=384 to the config would
    // run the next `solo daemon` against a stub rebuilt at dim=384
    // (the dim-vs-config check passes since the stub honours the
    // requested dim), and HNSW recall would silently miscompare
    // bundled-shaped queries against stub-hashed stored vectors.
    //
    // The bundled-feature gate happens at solo-storage's
    // `build_embedder_from_env` call site below for the explicit
    // SOLO_EMBEDDER=bundled path; this fast-path also goes through
    // that resolver but only when the env is unset.
    let env_override_active = std::env::var("SOLO_EMBEDDER")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if !env_override_active && config.embedder.name.starts_with("bundled:") {
        // Build the BundledEmbedder directly when the persisted config
        // says so. The Cargo feature gate splits compile-time builds:
        // on a feature-on build we have the constructor; on a feature-
        // off build (e.g., musl release shard) the cfg branch below
        // emits a clear operator-facing error pointing at the config
        // edit path.
        #[cfg(feature = "bundled-embedder")]
        {
            let embedder: Box<dyn Embedder> = Box::new(solo_storage::BundledEmbedder::new());
            tracing::info!(
                embedder = %embedder.name(),
                dim = embedder.dim(),
                "embedder constructed (bundled; via persisted [embedder] config)"
            );
            // The packaged model identity is indivisible: accepting an old
            // version or an arbitrary `bundled:*` name would compare vectors
            // from different spaces while appearing healthy.
            if embedder.name() != config.embedder.name
                || embedder.version() != config.embedder.version
                || embedder.dim() != config.embedder.dim as usize
            {
                anyhow::bail!(
                    "packaged embedder identity {}@{} ({}d) does not match \
                     persisted config identity {}@{} ({}d). Run \
                     `solo migrate-embedder` or, before first use, \
                     `solo init --force` to regenerate the library.",
                    embedder.name(),
                    embedder.version(),
                    embedder.dim(),
                    config.embedder.name,
                    config.embedder.version,
                    config.embedder.dim
                );
            }
            return Ok(Arc::from(embedder));
        }
        #[cfg(not(feature = "bundled-embedder"))]
        {
            anyhow::bail!(
                "persisted config requests `bundled:*` embedder but this \
                 binary was compiled without the `bundled-embedder` Cargo \
                 feature (likely a musl release shard). Edit \
                 solo.config.toml's `[embedder]` block to point at Ollama \
                 (see docs/releases/v0.9.0.md for guidance), or rebuild \
                 from source on a non-musl target."
            );
        }
    }

    if !env_override_active && let Some(model) = config.embedder.name.strip_prefix("ollama:") {
        let base_url = std::env::var("SOLO_OLLAMA_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        let embedder: Box<dyn Embedder> = Box::new(solo_storage::OllamaEmbedder::new(
            base_url,
            model,
            config.embedder.dim as usize,
        )?);
        tracing::info!(
            embedder = %embedder.name(),
            dim = embedder.dim(),
            "embedder constructed (Ollama; via persisted [embedder] config)"
        );
        return Ok(Arc::from(embedder));
    }

    // Delegate env-precedence resolution to storage.
    let mut embedder: Box<dyn Embedder> =
        build_embedder_from_env().context("build_embedder_from_env")?;

    // Preserve stub-identity continuity across runs (see concern #1
    // above). Storage hands back a generic `(stub, v1, 32)` when no
    // env is set; the CLI needs the persisted identity so
    // `embedder_id` resolution against `embedder_registry` finds the
    // existing row.
    if embedder.name() == "stub" {
        tracing::info!(
            embedder_kind = "stub",
            dim = config.embedder.dim,
            "embedder constructed (StubEmbedder with persisted config \
             identity; set SOLO_EMBEDDER=ollama for a real embedder). \
             Warning: stub vectors live in a separate space — switching \
             later requires `solo reembed`."
        );
        embedder = Box::new(StubEmbedder::new(
            &config.embedder.name,
            &config.embedder.version,
            config.embedder.dim as usize,
        ));
    } else {
        tracing::info!(
            embedder = %embedder.name(),
            dim = embedder.dim(),
            "embedder constructed (non-stub; via SOLO_EMBEDDER)"
        );
    }

    // Defense in depth: the chosen backend's dim must match the persisted
    // `config.embedder.dim` (see concern #2 above). If they diverge,
    // refuse — recall would silently miscompare against the wrong dim.
    let backend_dim = embedder.dim();
    if backend_dim != config.embedder.dim as usize {
        anyhow::bail!(
            "embedder backend dim ({}) does not match persisted config.embedder.dim ({}). \
             Run `solo init --force` if you really meant to switch embedders.",
            backend_dim,
            config.embedder.dim
        );
    }

    Ok(Arc::from(embedder))
}

/// All the pieces a one-shot command needs for the one Community Memory
/// Library.
///
/// Drop order: handle drops via `shutdown`, then library, then lock. The
/// lock outlives the writer thread per ADR-0003 §"Migration vs.
/// writer-thread connection lifecycle".
pub struct OneShotContext {
    pub data_dir: PathBuf,
    /// Owner of the one Community Memory Library runtime.
    pub library: Arc<MemoryLibrary>,
    /// Handle for the one Community Memory Library.
    pub library_handle: Arc<LibraryHandle>,
    /// Embedder. Same `Arc<dyn Embedder>` the library handle holds — kept
    /// as a separate field for one-shot callsites that need it without
    /// going through the handle (e.g., recall does its own embedding).
    pub embedder: Arc<dyn Embedder>,
    /// Per-data-dir lockfile. Released on `shutdown` (or Drop).
    ///
    /// `None` when the caller opted into proxy-friendly mode
    /// (`prepare_oneshot_opts(no_lockfile: true)`) — used by
    /// `solo mcp-stdio --no-lockfile` so a gateway can spawn multiple
    /// ephemeral subprocesses against one data dir. See dev-log 0155.
    pub lock: Option<Lockfile>,
}

impl OneShotContext {
    /// Convenience accessor for the WriteHandle — clone-cheap.
    pub fn write_handle(&self) -> WriteHandle {
        self.library_handle.write().clone()
    }

    /// Convenience accessor for the ReaderPool. Returns a reference; the
    /// pool itself is `Clone` so callers that need owned access can
    /// `.read_pool().clone()`.
    pub fn read_pool(&self) -> &solo_storage::ReaderPool {
        self.library_handle.read()
    }

    /// Convenience accessor for the Memory Library HNSW.
    pub fn hnsw(&self) -> &Arc<dyn solo_core::VectorIndex + Send + Sync> {
        self.library_handle.hnsw()
    }

    /// Convenience accessor for the Memory Library `SoloConfig`.
    pub fn config(&self) -> &SoloConfig {
        self.library_handle.config()
    }

    /// Convenience accessor for the canonical Community database path.
    pub fn db_path(&self) -> &std::path::Path {
        self.library_handle.db_path()
    }

    /// Graceful shutdown: save the current tenant's final snapshot, drain
    /// the writer, drop the registry (which closes every cached
    /// handle's pool), then release the lockfile.
    pub async fn shutdown(self) -> Result<()> {
        self.shutdown_inner(true).await
    }

    /// Graceful shutdown that does NOT call `save_snapshot`. The
    /// `solo reembed` flow needs this: after reembed, the in-memory
    /// HNSW still holds vectors from the prior embedder; saving it
    /// would re-write the on-disk snapshot we deliberately deleted to
    /// force a rebuild-from-SQL on next start.
    pub async fn shutdown_skip_snapshot_save(self) -> Result<()> {
        self.shutdown_inner(false).await
    }

    async fn shutdown_inner(self, save_snapshot: bool) -> Result<()> {
        let OneShotContext {
            data_dir: _,
            library,
            library_handle,
            embedder: _,
            lock,
        } = self;
        // Drop our request handle so the owner can close the runtime.
        drop(library_handle);
        if save_snapshot {
            library.shutdown().await;
        } else {
            library.shutdown_with_snapshot(false).await;
        }
        drop(library);
        drop(lock);
        Ok(())
    }
}

/// Options for [`prepare_oneshot_opts`].
#[derive(Debug, Default, Clone, Copy)]
pub struct OneShotOpts {
    /// Skip lockfile acquisition. **Dangerous**: lets multiple solo
    /// processes share the same data dir, which breaks the writer-actor
    /// single-process invariant (ADR-0003). Only safe when:
    ///   - the caller is a read-only stdio MCP session, OR
    ///   - the gateway / proxy spawning the process can guarantee that
    ///     write operations are serialised externally (rare).
    ///
    /// Added for `solo mcp-stdio --no-lockfile` per dev-log 0155. The
    /// flag exists for gateway/proxy deployments where the lockfile
    /// blocks ephemeral per-session subprocesses against shared state.
    pub no_lockfile: bool,
}

/// Resolve + open everything needed for a one-shot against the Community
/// Memory Library. By default this acquires the lockfile and fails fast if
/// another Solo process holds it.
pub async fn prepare_oneshot(data_dir_arg: Option<PathBuf>) -> Result<OneShotContext> {
    prepare_oneshot_opts(data_dir_arg, OneShotOpts::default()).await
}

/// Opts-taking variant of [`prepare_oneshot`]. Lets the caller opt out
/// of lockfile acquisition for proxy/gateway deployments.
pub async fn prepare_oneshot_opts(
    data_dir_arg: Option<PathBuf>,
    opts: OneShotOpts,
) -> Result<OneShotContext> {
    let data_dir = match data_dir_arg {
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
    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    let salt = config.salt_bytes().context("decode salt from config")?;

    // Acquire the lockfile BEFORE prompting for the passphrase — fail-fast
    // if a daemon or another one-shot is already running, rather than
    // making the user type their passphrase only to be turned away.
    //
    // Dev-log 0155: `--no-lockfile` opts out of acquisition entirely so a
    // gateway can spawn multiple ephemeral mcp-stdio subprocesses against
    // shared state. Loud `tracing::warn!` at startup makes the choice
    // visible in structured logs (operator runbooks: grep for
    // "no-lockfile" to find gateway-style deployments).
    let lock_path = data_dir.join("solo.lock");
    let lock = if opts.no_lockfile {
        tracing::warn!(
            lock_path = %lock_path.display(),
            "starting WITHOUT solo.lock acquisition (--no-lockfile). \
             Concurrent solo processes against the same data dir can \
             corrupt writer-actor state (ADR-0003). Only safe behind a \
             gateway that serialises writes externally."
        );
        None
    } else {
        Some(
            Lockfile::acquire(&lock_path)
                .context("acquire solo.lock — daemon or another one-shot already running?")?,
        )
    };

    let passphrase = read_passphrase()?;
    let key = KeyMaterial::derive(&passphrase, &salt)
        .context("derive key from passphrase + persisted salt")?;
    // passphrase is `Zeroizing<String>` — wipes its buffer on drop.
    drop(passphrase);

    // Build the embedder. Same env-driven resolution as before.
    let embedder = build_embedder(&config)?;

    // Optionally wire a real `Steward` from the persisted `[llm]`
    // settings. If the operator passed an explicit CLI override such
    // as `--ollama-model`, the process-local override marker keeps the
    // historical env-backed path for that one run.
    //
    // v0.11.1: `[steward]` TOML block layered under env-var overrides
    // so one-shot CLI runs honor the same clustering knobs the daemon
    // would. Operators get a single source of truth in `solo.config.toml`
    // without losing the `SOLO_CLUSTER_*` runtime escape hatch.
    let steward_config = solo_steward::StewardConfig::from_settings_then_env(
        config.steward.cluster_min_size,
        config.steward.cluster_cosine_threshold,
    )
    .context("parse [steward] config + SOLO_CLUSTER_* env vars for one-shot")?;
    let llm_client = build_llm_client_for_cli(&config)?;
    let steward = match llm_client {
        Some(llm) => {
            tracing::info!(
                model = %llm.name(),
                cluster_cosine_threshold = steward_config.cluster_cosine_threshold,
                cluster_min_size = steward_config.cluster_min_size,
                "LLM backend wired; consolidate will produce abstractions + contradictions"
            );
            Some(Arc::new(solo_steward::Steward::new(llm, steward_config)))
        }
        None => None,
    };

    // Open the one Community Memory Library. The constructor promotes the
    // previous default-only layout when that can be done without data loss.
    let runtime_handle = tokio::runtime::Handle::current();
    let library = MemoryLibrary::open(MemoryLibraryParams {
        data_dir: data_dir.clone(),
        key: key.clone(),
        embedder: embedder.clone(),
        hnsw_params: HnswParams::default(),
        steward,
        runtime_handle: Some(runtime_handle),
        // v0.9.0 P0c: no StewardFactory wired yet on the CLI path; the
        // per-tenant `steward_slot` falls back to mirroring the
        // captured `steward` parameter above. P1 plumbs the
        // config-driven factory through here.
        steward_factory: None,
        // v0.9.0 P4-revision (P4 audit M1): one-shot CLI commands don't
        // drive the daemon's `triples_batch_timer`, so no count-based
        // signal needs threading.
        triples_batch_signal: None,
    })
    .context("open Community Memory Library")?;
    let library = Arc::new(library);
    let library_handle = library
        .handle()
        .await
        .context("open Community Memory Library runtime")?;

    if library_handle.replay().rows_replayed > 0 {
        tracing::info!(
            replayed = library_handle.replay().rows_replayed,
            "pending_index replay applied at one-shot startup"
        );
    }
    if !library_handle.drift().is_clean() {
        let drift = library_handle.drift();
        tracing::warn!(
            hot_episodes = drift.hot_episodes,
            active_chunks = drift.active_chunks,
            expected = drift.expected_index_len(),
            index_len = drift.index_len,
            diff = drift.diff,
            "HNSW vs SQL drift detected"
        );
    }

    Ok(OneShotContext {
        data_dir,
        library,
        library_handle,
        embedder,
        lock,
    })
}

/// Administrative one-shot context for Community backup, restore, audit, and
/// privacy operations. It owns the one Memory Library, derived key, and
/// process lock.
pub struct AdminContext {
    pub data_dir: PathBuf,
    pub key: KeyMaterial,
    pub library: Arc<MemoryLibrary>,
    pub lock: Lockfile,
}

impl AdminContext {
    /// Shared bootstrap: resolve `--data-dir`, acquire the data-dir
    /// lockfile, prompt for the passphrase, derive the key, and build the
    /// Community Memory Library owner. The runtime handle opens lazily.
    pub fn bootstrap(data_dir_arg: Option<PathBuf>) -> Result<Self> {
        let data_dir = match data_dir_arg {
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
        let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
        let salt = config.salt_bytes().context("decode salt from config")?;

        // Acquire lock BEFORE prompting for the passphrase — fail-fast if
        // a daemon or another one-shot is already running.
        let lock_path = data_dir.join("solo.lock");
        let lock = Lockfile::acquire(&lock_path)
            .context("acquire solo.lock — daemon or another one-shot already running?")?;

        let passphrase = read_passphrase()?;
        let key = KeyMaterial::derive(&passphrase, &salt)
            .context("derive key from passphrase + persisted salt")?;
        drop(passphrase);

        // Build embedder + steward so the Memory Library runtime can open on
        // demand. Backup-only operations do not pay the database-open cost.
        let embedder = build_embedder(&config)?;
        // v0.11.1: layer `[steward]` TOML under `SOLO_CLUSTER_*` env vars
        // so admin one-shots see the same thresholds the daemon does.
        let steward_config = solo_steward::StewardConfig::from_settings_then_env(
            config.steward.cluster_min_size,
            config.steward.cluster_cosine_threshold,
        )
        .context("parse [steward] config + SOLO_CLUSTER_* env vars for admin one-shot")?;
        let steward = build_llm_client_for_cli(&config)?
            .map(|llm| Arc::new(solo_steward::Steward::new(llm, steward_config)));

        let runtime_handle = tokio::runtime::Handle::current();
        let library = MemoryLibrary::open(MemoryLibraryParams {
            data_dir: data_dir.clone(),
            key: key.clone(),
            embedder,
            hnsw_params: HnswParams::default(),
            steward,
            runtime_handle: Some(runtime_handle),
            // v0.9.0 P0c: no StewardFactory wired yet — admin one-shot
            // commands use the same backwards-compat path as the daemon
            // (slot mirrors the captured `steward` parameter).
            steward_factory: None,
            // v0.9.0 P4-revision (P4 audit M1): admin one-shot commands
            // don't drive the daemon's `triples_batch_timer`.
            triples_batch_signal: None,
        })
        .context("open Community Memory Library")?;
        let library = Arc::new(library);

        Ok(Self {
            data_dir,
            key,
            library,
            lock,
        })
    }

    /// Emit an admin-tier audit row to
    /// `tenants_index.db::audit_events_admin`. Re-uses the derived key,
    /// so the operator only types their passphrase once per admin op
    /// regardless of how many places need to read tenants_index.db. Returns
    /// the inserted row's audit_id.
    pub fn admin_emit(
        &self,
        principal_subject: Option<&str>,
        op: AuditOperation,
        target_tenant_id: Option<&str>,
        result: AuditResult,
        details: Option<&serde_json::Value>,
    ) -> Result<i64> {
        let admin_path = self.data_dir.join(solo_storage::COMMUNITY_DB_FILENAME);
        let admin_conn = open_sqlcipher(&admin_path, &self.key)
            .context("open Community Memory Library for admin-audit emit")?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let id = insert_audit_admin_row(
            &admin_conn,
            now_ms,
            principal_subject,
            op,
            target_tenant_id,
            result,
            details,
        )
        .context("insert audit_events_admin row")?;
        Ok(id)
    }

    /// Lazily open the Community Memory Library runtime.
    pub async fn open_library(&self) -> Result<Arc<LibraryHandle>> {
        self.library
            .handle()
            .await
            .context("open Community Memory Library runtime")
    }

    /// Borrow the derived SQLCipher key. Used by callers that need to
    /// open a per-tenant DB outside the registry's lazy-open path (e.g.
    /// backup/restore against a file on disk that hasn't been wired
    /// through the registry yet).
    pub fn key(&self) -> &KeyMaterial {
        &self.key
    }

    /// Borrow the data dir. Used by callers that need to build paths
    /// (e.g. backup destination, per-tenant DB path before/after delete).
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Graceful shutdown: drain the Memory Library runtime and release the
    /// lockfile.
    pub async fn shutdown(self) -> Result<()> {
        let AdminContext {
            data_dir: _,
            key: _,
            library,
            lock,
        } = self;
        library.shutdown().await;
        drop(library);
        drop(lock);
        Ok(())
    }
}

/// Read the passphrase from `SOLO_PASSPHRASE` env var (with stderr
/// warning) or interactive prompt. Returns `Zeroizing<String>` so the
/// underlying buffer is wiped on drop — defense against process-memory
/// inspection between use and free.
/// Pre-installed platform shutdown signals. Constructed up front in
/// `daemon::run` / `http_serve::run` (before any heavy startup) so the
/// SIGTERM handler is registered with tokio long before the test
/// process_lifecycle::graceful_shutdown_within_budget sends its
/// signal. The race we previously had: tokio's
/// `signal(SignalKind::terminate())` doesn't install the handler
/// until called, and the original `wait_for_shutdown_signal()` async
/// fn called it inline at await time — which was *after* writer
/// spawn + reader pool + HTTP bind on a slow Linux runner. SIGTERM
/// arriving in that window fell through to the OS-default
/// disposition (kill, exit status `unix_wait_status(15)`) instead of
/// being caught.
///
/// The `signal()` call itself is the install; only `recv()` blocks.
/// So we install at the top of `run()`, hold the receiver in this
/// struct, and `recv().await` it later when ready to shut down.
pub(crate) struct ShutdownSignals {
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    /// Install the platform-appropriate signal handlers. Call this as
    /// the first step in any long-running subcommand's `run()` —
    /// before lockfile, before key derivation, before anything else.
    pub(crate) fn install() -> Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            sigterm: {
                use tokio::signal::unix::{SignalKind, signal};
                signal(SignalKind::terminate()).context("install SIGTERM handler")?
            },
        })
    }

    /// Block until either Ctrl-C or (Unix) SIGTERM fires. Logs which
    /// signal was received. Consumes `self` since the inner Signal
    /// requires `&mut`.
    pub(crate) async fn await_any(self) {
        #[cfg(unix)]
        {
            let mut sigterm = self.sigterm;
            tokio::select! {
                _ = tokio::signal::ctrl_c() => tracing::info!("received Ctrl+C"),
                _ = sigterm.recv() => tracing::info!("received SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            // Suppress unused_variables / dead_code on Windows.
            let _ = self;
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("received Ctrl+C");
        }
    }
}

/// v0.11.2 process-local passphrase cache. First call resolves the
/// passphrase (env var or interactive prompt) and stores it here;
/// subsequent calls return the cached clone.
///
/// This serves two purposes:
///   1. **Test-friendliness**: integration tests invoke multiple cli
///      subcommands in one process — each one would otherwise re-read
///      the env var (or re-prompt, which fails in no-TTY CI). The
///      cache lets every call after the first reuse the resolved value.
///   2. **Security goal preservation** (dev-log 0152 low-sec finding):
///      we still remove `SOLO_PASSPHRASE` from our environ block
///      *after caching it*, so forked children do not inherit the
///      env-var copy. The kernel-side copy from before the remove is
///      still observable until process exit; the stderr warning at
///      first read documents that limitation.
///
/// The cache stores the value as `Zeroizing<String>` and is wrapped in
/// `Arc` so callers each get a cheap clone; the underlying bytes are
/// wiped once the last clone drops at process exit.
static PASSPHRASE_CACHE: std::sync::OnceLock<std::sync::Arc<zeroize::Zeroizing<String>>> =
    std::sync::OnceLock::new();

pub(crate) fn read_passphrase() -> Result<zeroize::Zeroizing<String>> {
    use zeroize::Zeroizing;

    // Fast path: serve the cached value.
    if let Some(cached) = PASSPHRASE_CACHE.get() {
        return Ok((**cached).clone());
    }

    // Slow path: resolve from env var or prompt.
    let resolved: Zeroizing<String> = if let Ok(env_pass) = std::env::var(ENV_PASSPHRASE) {
        if env_pass.is_empty() {
            bail!("{ENV_PASSPHRASE} is set but empty");
        }
        eprintln!(
            "warning: reading passphrase from {ENV_PASSPHRASE} process environment; \
             it may be visible to same-user processes or diagnostic tools"
        );
        // Cache FIRST, then remove the env var from our environ block.
        // Forked children after this point will not inherit the env
        // var; in-process `read_passphrase()` callers get the cached
        // value from the fast path above. (Removing before caching
        // would race with concurrent same-process callers — there is
        // only one today, but defensive sequencing costs nothing.)
        let zeroed = Zeroizing::new(env_pass);
        // SAFETY: read_passphrase runs early in single-threaded CLI
        // startup. No other thread is reading env at this point.
        unsafe {
            std::env::remove_var(ENV_PASSPHRASE);
        }
        zeroed
    } else {
        let p = rpassword::prompt_password("Enter passphrase (will not be echoed): ")
            .context("read passphrase")?;
        if p.is_empty() {
            bail!("passphrase must not be empty");
        }
        Zeroizing::new(p)
    };

    // Install into the cache. If another thread won the race, use theirs.
    let arc = std::sync::Arc::new(resolved);
    let stored = PASSPHRASE_CACHE.get_or_init(|| arc);
    Ok((**stored).clone())
}

/// `--ollama-model <MODEL>` shorthand: configure the env vars
/// `solo_storage::llm::build_llm_client_from_env()` reads so it builds
/// an OpenAI-protocol client pointed at a local Ollama instance with
/// the given model. Bypasses the multi-env-var dance documented in
/// the README's "Local LLM via Ollama" section.
///
/// Sets:
///   - `OPENAI_API_KEY=ollama`            (only if not already set)
///   - `OPENAI_BASE_URL=<localhost:11434/v1>` (only if not already set)
///   - `OPENAI_MODEL=<model>`             (always — the explicit
///                                         flag wins over any prior
///                                         OPENAI_MODEL the operator
///                                         left in their env)
///
/// Also unsets `ANTHROPIC_API_KEY` for this process. The default
/// precedence in `build_llm_client_from_env` is Anthropic > OpenAI;
/// without this unset, an operator who has both an Anthropic key in
/// their environment AND passes `--ollama-model` would get Anthropic
/// silently — surprising, since the explicit flag should win. The
/// unset is process-local; the user's shell env is unchanged.
///
/// Allowing `OPENAI_API_KEY` and `OPENAI_BASE_URL` overrides lets
/// operators run Ollama behind an auth proxy or on a non-default
/// host/port without losing the model-default convenience.
///
/// Returns the resolved (model, base_url) pair so the caller can
/// log + surface it.
pub(crate) fn apply_ollama_overrides(model: &str) -> (String, String) {
    const OLLAMA_DEFAULT_KEY: &str = "ollama";
    const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434/v1";

    // SAFETY for the four `unsafe` blocks below: this helper runs once
    // at the top of `daemon::run` / `consolidate::run`, on the main
    // tokio task, before any Steward construction or other env-reading
    // code. No other thread is concurrently reading or writing these
    // env vars at this point.

    if std::env::var_os("OPENAI_API_KEY").is_none() {
        unsafe { std::env::set_var("OPENAI_API_KEY", OLLAMA_DEFAULT_KEY) };
    }

    let base_url = std::env::var("OPENAI_BASE_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let url = OLLAMA_DEFAULT_BASE_URL.to_string();
            unsafe { std::env::set_var("OPENAI_BASE_URL", &url) };
            url
        });

    unsafe { std::env::set_var("OPENAI_MODEL", model) };
    unsafe { std::env::set_var("SOLO_LLM_ENV_OVERRIDE", "1") };

    if std::env::var_os("ANTHROPIC_API_KEY").is_some() {
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        tracing::info!(
            "--ollama-model overrides ANTHROPIC_API_KEY for this process \
             (explicit flag wins over Anthropic > OpenAI precedence)"
        );
    }

    (model.to_string(), base_url)
}

fn build_llm_client_for_cli(
    config: &SoloConfig,
) -> solo_core::Result<Option<std::sync::Arc<dyn solo_core::LlmClient>>> {
    if std::env::var_os("SOLO_LLM_ENV_OVERRIDE").is_some() {
        solo_storage::llm::build_llm_client_from_env()
    } else {
        solo_storage::llm::build_llm_client_from_settings(config.llm.as_ref())
    }
}

#[cfg(test)]
mod ollama_overrides_tests {
    use super::*;
    use std::sync::Mutex;

    // Env vars are process-global mutable state — serialize the tests
    // here so cargo test's per-binary parallel runner doesn't race.
    // `unwrap_or_else(PoisonError::into_inner)` keeps a panicking test
    // from poisoning the lock for the rest of the suite.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Drops the env vars on Drop so a panicking test doesn't leak
    /// state into subsequent cases.
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: ENV_LOCK is held by the caller, so no other
            // thread is concurrently reading or writing these vars.
            for k in [
                "OPENAI_API_KEY",
                "OPENAI_BASE_URL",
                "OPENAI_MODEL",
                "ANTHROPIC_API_KEY",
                "SOLO_LLM_ENV_OVERRIDE",
            ] {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    fn fresh_env() -> EnvGuard {
        // Pre-clean: previous test may have panicked and leaked state
        // before its EnvGuard's Drop fired. SAFETY: ENV_LOCK held by
        // caller, so no concurrent reader/writer of these vars.
        for k in [
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "OPENAI_MODEL",
            "ANTHROPIC_API_KEY",
            "SOLO_LLM_ENV_OVERRIDE",
        ] {
            unsafe { std::env::remove_var(k) };
        }
        // Post-clean handled by the returned guard's Drop at end of test.
        EnvGuard
    }

    #[test]
    fn unset_env_picks_up_all_defaults() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        let (model, url) = apply_ollama_overrides("qwen2.5-coder:7b");
        assert_eq!(model, "qwen2.5-coder:7b");
        assert_eq!(url, "http://localhost:11434/v1");
        assert_eq!(std::env::var("OPENAI_API_KEY").unwrap(), "ollama");
        assert_eq!(std::env::var("OPENAI_BASE_URL").unwrap(), url);
        assert_eq!(std::env::var("OPENAI_MODEL").unwrap(), "qwen2.5-coder:7b");
        assert!(std::env::var_os("ANTHROPIC_API_KEY").is_none());
    }

    #[test]
    fn preserves_explicit_openai_api_key() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // SAFETY: lock held; serialized within this test binary.
        unsafe { std::env::set_var("OPENAI_API_KEY", "auth-proxy-token-xyz") };
        let _ = apply_ollama_overrides("qwen2.5-coder:7b");
        assert_eq!(
            std::env::var("OPENAI_API_KEY").unwrap(),
            "auth-proxy-token-xyz"
        );
    }

    #[test]
    fn preserves_explicit_openai_base_url() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var("OPENAI_BASE_URL", "http://my-ollama-host:31000/v1") };
        let (_, url) = apply_ollama_overrides("qwen2.5-coder:7b");
        assert_eq!(url, "http://my-ollama-host:31000/v1");
        assert_eq!(
            std::env::var("OPENAI_BASE_URL").unwrap(),
            "http://my-ollama-host:31000/v1"
        );
    }

    #[test]
    fn empty_base_url_falls_back_to_default() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var("OPENAI_BASE_URL", "") };
        let (_, url) = apply_ollama_overrides("qwen2.5-coder:7b");
        assert_eq!(url, "http://localhost:11434/v1");
    }

    #[test]
    fn explicit_model_always_overrides_existing_openai_model() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var("OPENAI_MODEL", "gpt-4o-mini") };
        let _ = apply_ollama_overrides("qwen2.5-coder:14b");
        assert_eq!(std::env::var("OPENAI_MODEL").unwrap(), "qwen2.5-coder:14b");
    }

    #[test]
    fn unsets_anthropic_key_for_explicit_flag_precedence() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-...") };
        let _ = apply_ollama_overrides("qwen2.5-coder:7b");
        assert!(std::env::var_os("ANTHROPIC_API_KEY").is_none());
    }
}

#[cfg(test)]
mod build_embedder_tests {
    //! Tests for [`build_embedder`] as a thin wrapper over
    //! [`solo_storage::build_embedder_from_env`].
    //!
    //! The wrapper handles two coordination concerns that the storage
    //! builder cannot:
    //!
    //! 1. Rebuild a generic `(stub, v1, 32)` from storage into a stub
    //!    matching the persisted `config.embedder` identity, so
    //!    `embedder_id` stays stable across upgrades.
    //! 2. Verify that the chosen backend's dim matches the persisted
    //!    `config.embedder.dim` — guards against silent backend swaps.
    //!
    //! Env vars are process-global mutable state. Both this module and
    //! solo-storage's `build_from_env_tests` have their own `Mutex` —
    //! they're crate-local but don't deadlock with one another because
    //! the CLI tests don't transitively trigger storage's tests (they
    //! exercise the same env-driven `build_embedder_from_env` function,
    //! but each crate's test binary runs in its own process).
    use super::*;
    use solo_storage::{EmbedderConfig, IdentityConfig};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    const ALL_VARS: &[&str] = &[
        "SOLO_EMBEDDER",
        "SOLO_OLLAMA_BASE_URL",
        "SOLO_OLLAMA_EMBED_MODEL",
        // Legacy v0.5.x BGE-M3 env var — still listed here so the
        // EnvGuard pre-cleans any stale value from the surrounding
        // shell, even though v0.6.0+ ignores it.
        "SOLO_BGE_M3_DIR",
    ];

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: ENV_LOCK is held by the caller for the duration
            // of the test; no other thread is concurrently reading or
            // writing these vars.
            for k in ALL_VARS {
                unsafe { std::env::remove_var(k) };
            }
        }
    }

    fn fresh_env() -> EnvGuard {
        for k in ALL_VARS {
            unsafe { std::env::remove_var(k) };
        }
        EnvGuard
    }

    fn fake_config(name: &str, version: &str, dim: u32) -> SoloConfig {
        SoloConfig {
            schema_version: 1,
            // 16 zero bytes as a placeholder salt — `build_embedder`
            // doesn't read salt, only `embedder`.
            salt_hex: "00000000000000000000000000000000".to_string(),
            embedder: EmbedderConfig {
                name: name.to_string(),
                version: version.to_string(),
                dim,
                dtype: "f32".to_string(),
            },
            identity: IdentityConfig::default(),
            documents: solo_storage::DocumentConfig::default(),
            workspace_file_access: solo_storage::WorkspaceFileAccessConfig::default(),
            auth: None,
            audit: solo_storage::AuditSettings::default(),
            redaction: solo_storage::RedactionConfig::default(),
            llm: None,
            triples: solo_storage::TriplesConfig::default(),
            sampling: solo_storage::SamplingConfig::default(),
            steward: solo_storage::StewardSettings::default(),
        }
    }

    #[test]
    fn cli_build_embedder_returns_ollama_when_env_set() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // SAFETY: lock held; serialized within this test binary.
        unsafe { std::env::set_var("SOLO_EMBEDDER", "ollama") };
        // Persisted config asserts dim=768 (the Ollama default for
        // nomic-embed-text). Real fix in 6D probes at `solo init`.
        let config = fake_config("ollama:nomic-embed-text", "v1", 768);
        let e = build_embedder(&config).expect("ollama branch builds");
        assert!(
            e.name().starts_with("ollama:"),
            "expected ollama:* name, got {}",
            e.name()
        );
        assert_eq!(e.dim(), 768);
    }

    #[test]
    fn cli_build_embedder_uses_persisted_ollama_config_when_env_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        let config = fake_config("ollama:nomic-embed-text", "v1", 768);
        let e = build_embedder(&config).expect("persisted ollama config builds");
        assert_eq!(e.name(), "ollama:nomic-embed-text");
        assert_eq!(e.version(), "v1");
        assert_eq!(e.dim(), 768);
    }

    #[test]
    fn cli_build_embedder_returns_stub_with_config_identity_when_nothing_set() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // Nothing in env → storage hands back a generic (stub, v1, 32).
        // The CLI wrapper must rebuild it with the persisted identity
        // so `embedder_id` resolution against `embedder_registry`
        // finds the existing row instead of orphaning data dirs from
        // earlier releases. This is concern #1.
        //
        // The persisted name doesn't have to be a real backend — what
        // we're proving is that the wrapper threads the config's
        // `(name, version, dim)` through to the rebuilt stub. Use a
        // sentinel name + non-default dim so a regression would
        // surface as a mismatch in either field.
        let config = fake_config("custom-test-name", "v1", 128);
        let e = build_embedder(&config).expect("stub fallback");
        // Note: backend "name" stays "stub" (StubEmbedder is the impl);
        // the *identity* is the persisted config's name/version/dim,
        // which is what `embedder_registry` keys on. The stub-rebuild
        // branch uses `StubEmbedder::new(&config.embedder.name, ...)`
        // so `e.name()` returns the config's name, not the literal
        // "stub".
        assert_eq!(e.name(), "custom-test-name");
        assert_eq!(e.version(), "v1");
        assert_eq!(e.dim(), 128);
    }

    #[test]
    fn cli_build_embedder_dim_mismatch_errors_cleanly() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // Force the Ollama branch (dim=768) but persist a config with
        // dim=512. The wrapper's dim-vs-config check must fire. Stub
        // path can't trigger this because the rebuild branch matches
        // dims by construction.
        unsafe { std::env::set_var("SOLO_EMBEDDER", "ollama") };
        let config = fake_config("ollama:nomic-embed-text", "v1", 512);
        // `Arc<dyn Embedder>` doesn't implement Debug, so we can't use
        // `expect_err`. Match manually.
        let err = match build_embedder(&config) {
            Ok(_) => panic!("dim mismatch must error, not silently miscompare"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("768") && msg.contains("512"),
            "error must name both dims: {msg}"
        );
        assert!(
            msg.to_ascii_lowercase().contains("solo init --force"),
            "error must point at the fix: {msg}"
        );
    }

    #[test]
    fn cli_build_embedder_does_not_reintroduce_bge_m3_branch() {
        // v0.6.0 P9 hard-removed the BGE-M3 backend. As a regression
        // guard, assert that `common.rs` no longer references the
        // BGE-M3 tracing-tag value `embedder_kind = "bge-m3"` (the
        // distinctive marker of the historical CLI-side
        // Once-guarded deprecation warn). Stderr capture in unit
        // tests is awkward, so this structural check stands in for
        // a "no BGE-M3 emission" assertion.
        //
        // We assemble the probe from chunks at runtime so this
        // test's own source doesn't contain the literal needle and
        // self-trip.
        let source = include_str!("common.rs");
        let probe = ["embedder_kind", "=", "\"bge-m3\""].concat();
        assert!(
            !source.contains(&probe),
            "CLI must not reintroduce a BGE-M3 branch. Found the historical \
             tracing tag value in common.rs."
        );
    }

    /// v0.9.0 P3: when the persisted config says `bundled:*` AND
    /// SOLO_EMBEDDER is unset, the CLI wrapper builds the
    /// BundledEmbedder directly — NOT a stub rebuilt with the bundled
    /// identity. Without this branch the daemon would silently embed
    /// queries with hash-vectors against bundled-vectored stored rows
    /// and recall would miscompare.
    #[cfg(feature = "bundled-embedder")]
    #[test]
    fn cli_build_embedder_routes_to_bundled_when_config_says_bundled() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        // SOLO_EMBEDDER unset; config persists the bundled identity.
        let config = fake_config("bundled:all-MiniLM-L6-v2", "v2", 384);
        let e = build_embedder(&config).expect("bundled config path builds");
        assert_eq!(e.name(), "bundled:all-MiniLM-L6-v2");
        assert_eq!(e.version(), "v2");
        assert_eq!(e.dim(), 384);
        // The runtime impl is BundledEmbedder, NOT a renamed StubEmbedder.
        // We can't easily downcast (Arc<dyn Embedder>), but the dim
        // check above implicitly verifies it — a 384-dim StubEmbedder
        // would still embed via BLAKE3 hashing, which would pass the
        // dim check but produce vectors in a completely different space
        // from a real bundled inference. The integration test for
        // semantic-recall in the daemon suite is the end-to-end pin.
    }

    #[cfg(feature = "bundled-embedder")]
    #[test]
    fn cli_build_embedder_rejects_outdated_bundled_vector_identity() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        let config = fake_config("bundled:all-MiniLM-L6-v2", "v1", 384);
        let error = match build_embedder(&config) {
            Ok(_) => panic!("v1 and v2 vectors must never be mixed"),
            Err(error) => error,
        };
        let message = error.to_string();
        assert!(message.contains("v1"));
        assert!(message.contains("v2"));
        assert!(message.contains("migrate-embedder"));
    }

    /// v0.9.0 P3: on a feature-OFF build (musl release shard), the
    /// CLI must refuse to silently fall back to a stub when the
    /// persisted config says `bundled:*`. The error message points at
    /// the config edit path so operators know what to do.
    #[cfg(not(feature = "bundled-embedder"))]
    #[test]
    fn cli_build_embedder_rejects_bundled_config_when_feature_off() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        let config = fake_config("bundled:all-MiniLM-L6-v2", "v2", 384);
        let err = match build_embedder(&config) {
            Ok(_) => panic!(
                "feature-off binary with bundled-config must error, not silently \
                 fall back to stub (recall would miscompare)"
            ),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("bundled-embedder"),
            "error must name the feature: {msg}"
        );
        assert!(
            msg.contains("solo.config.toml") || msg.contains("[embedder]"),
            "error must point at the config edit path: {msg}"
        );
    }

    /// v0.9.0 P3: SOLO_EMBEDDER=ollama (or any non-empty value)
    /// overrides the bundled-from-config branch. Operators who want to
    /// temporarily test Ollama against a config that says bundled MUST
    /// be able to do so without editing the TOML.
    #[cfg(feature = "bundled-embedder")]
    #[test]
    fn cli_build_embedder_env_override_beats_bundled_config() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();
        unsafe { std::env::set_var("SOLO_EMBEDDER", "ollama") };
        // Config says bundled (dim=384) but env says ollama (dim=768).
        // The dim mismatch will trigger the defense-in-depth check
        // because we expect the operator to also bump the config when
        // switching backends. The error message is what we're testing.
        let config = fake_config("bundled:all-MiniLM-L6-v2", "v2", 384);
        let err = match build_embedder(&config) {
            Ok(_) => panic!("dim mismatch must error"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        // The env-override path goes through `build_embedder_from_env`
        // → ollama branch → dim=768. The mismatch with config's
        // dim=384 surfaces from the wrapper's dim-vs-config guard.
        assert!(msg.contains("768"), "msg should name ollama dim: {msg}");
        assert!(msg.contains("384"), "msg should name config dim: {msg}");
    }
}
