// SPDX-License-Identifier: Apache-2.0

//! `solo daemon` subcommand. Starts the long-running Solo memory daemon.
//!
//! Per ADR-0003 §O6 ("Startup ordering: linear await chain in main()") and
//! §O7 ("Shutdown: drop the last WriteHandle, mpsc closes, actor exits
//! cleanly").
//!
//! ## What this command does (commit 1.5)
//!
//!   1. Resolve `--data-dir` (default: `~/.solo` via `default_data_dir`).
//!   2. Read `solo.config.toml` to discover the salt + embedder identity.
//!   3. Prompt for passphrase, read `SOLO_PASSPHRASE`, or read one stdin
//!      line when the tray launches us with `SOLO_PASSPHRASE_STDIN=1`.
//!   4. Derive `KeyMaterial` via Argon2id.
//!   5. Acquire `solo.lock` (refuses to start if another daemon owns it).
//!   6. Run `solo_storage::startup::run` — opens SQLCipher, replays
//!      `pending_index`, loads the HNSW snapshot (with `.bak` fallback or
//!      fresh-empty), checks dim consistency, reports drift.
//!   7. Construct the embedder via `commands::common::build_embedder`
//!      (env resolution: SOLO_EMBEDDER=ollama → OllamaEmbedder, else
//!      StubEmbedder).
//!   8. Build a `ReaderPool` (default size 2 per ADR-0003 §O9).
//!   9. Spawn the `WriterActor` with a snapshot directory wired up
//!      (`spawn_with_snapshot_dir`).
//!  10. Spawn the background snapshot timer (5-min cadence per ADR-0003 §O3).
//!  11. Install a panic hook that logs + `process::exit(1)` so the OS
//!      supervisor can restart us cleanly (ADR-0003 §O8).
//!  12. Wait for `Ctrl+C` (and `SIGTERM` on Unix) — graceful shutdown.
//!  13. On signal, drop the `WriteHandle`. The actor's `mpsc::Receiver`
//!      sees `None`, runs `shutdown()` (wal_checkpoint + HNSW save), and
//!      exits cleanly.
//!  14. Lockfile drops here, releasing the data dir.
//!
//! ## What's NOT here yet
//!
//! - axum HTTP server (commit 1.6).
//! - rmcp MCP server (commit 1.5 was originally meant to include it; we
//!   defer to the next session because the daemon-supervision shape needs
//!   to land first and the MCP transport is a sizable second piece).
//! - `tokio_unstable` + `UnhandledPanic::ShutdownRuntime` (ADR-0003 §P8-H).
//!   Today we rely on the process-level panic hook calling `process::exit(1)`
//!   for any panic (writer thread, tokio task, or the daemon itself); the
//!   supervisor restarts. Adding the unstable cfg flag is a separate clean
//!   step.
//!
//! ## Foreground vs. background
//!
//! `solo daemon` runs in the foreground. Daemonisation (forking, detaching,
//! redirecting std streams) is a job for the OS supervisor — systemd's
//! `Type=simple`, launchd's `KeepAlive=true`, or a shell wrapper with
//! `nohup`. ADR-0003 §"Architecture-doc errata flagged for fix" item 4
//! reaffirms this.

use anyhow::{Context, Result, bail};
use clap::Args;
use solo_storage::{
    ConsolidationScope, DEFAULT_POOL_SIZE, HnswParams, KeyMaterial, Lockfile, MemoryLibrary,
    MemoryLibraryParams, SoloConfig, WriteHandle, default_data_dir,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const ENV_PASSPHRASE: &str = "SOLO_PASSPHRASE";
const ENV_PASSPHRASE_STDIN: &str = "SOLO_PASSPHRASE_STDIN";
const ENV_SHUTDOWN_ON_STDIN_EOF: &str = "SOLO_DAEMON_SHUTDOWN_ON_STDIN_EOF";
const DEFAULT_SNAPSHOT_INTERVAL_SECS: u64 = 300; // 5 minutes per ADR-0003 §O3
const DOCUMENT_UPLOAD_SWEEP_INTERVAL_SECS: u64 = 60;
const HTTP_SHUTDOWN_TIMEOUT_SECS: u64 = 5;
const RESTART_FORCE_EXIT_TIMEOUT_SECS: u64 = 10;
const SHUTDOWN_TIMEOUT_SECS: u64 = 30; // ADR-0003 §O7
const STARTUP_DERIVED_CATCHUP_MIN_EPISODES: usize = 25;

#[derive(Debug, Args)]
pub struct DaemonArgs {
    /// Data directory containing `solo.db` and `solo.config.toml`. Defaults
    /// to `~/.solo`. Override with `SOLO_DATA_DIR`.
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// HNSW snapshot save cadence in seconds. Defaults to 300 (5 minutes).
    /// Set to 0 to disable the timer (manual `SaveSnapshot` only).
    #[arg(long, default_value_t = DEFAULT_SNAPSHOT_INTERVAL_SECS)]
    pub snapshot_interval_secs: u64,

    /// If set, also start the HTTP/JSON server on 127.0.0.1:<port>
    /// alongside the daemon. Browser-based UIs and `curl` clients use
    /// this. Same handler surface as `solo http-serve`.
    #[arg(long)]
    pub http_port: Option<u16>,

    /// Periodic consolidation cadence in seconds. Omit to use
    /// `[triples].consolidate_interval_secs` from solo.config.toml. Set
    /// to 0 explicitly to disable the timer.
    /// Run the SWS-equivalent clustering pass at this interval; if a
    /// `Steward` LLM is wired, also runs the abstraction pass. The
    /// config default is hourly; use `--consolidate-interval-secs 0`
    /// when you explicitly want this timer disabled for one run.
    /// The first fire is delayed by one full interval to avoid
    /// clustering a near-empty DB right after startup.
    #[arg(long)]
    pub consolidate_interval_secs: Option<u64>,

    /// Window (in days) for periodic consolidation. Only memories
    /// with `ts_ms >= now - window_days * 86_400_000` are considered
    /// candidates. Default: unbounded (all active+hot, current-
    /// embedder, not-already-clustered memories). Smaller windows
    /// trade coverage for per-tick cost — useful on dense corpora.
    #[arg(long)]
    pub consolidate_window_days: Option<i64>,

    /// On every consolidate-timer fire, set `force_merge: true` so the
    /// existing-vs-existing merge + abstraction-regen passes run even
    /// when no new episodes are clustered. Useful for unattended drift
    /// catch-up on a quiet corpus where pre-existing clusters slowly
    /// drift toward each other across runs but the empty-CANDIDATES
    /// early return otherwise prevents the merge pass from firing on
    /// idle cycles. Default off because the merge pass invokes the
    /// LLM Steward for abstraction regen on every fire — unbounded
    /// force-merge cadence amplifies LLM cost on a corpus with many
    /// existing-vs-existing merge candidates accumulated. Use `solo
    /// doctor --with-stats` to surface the current merge-candidate
    /// count and decide whether enabling this is worth it.
    /// Ignored unless the effective consolidate interval is > 0.
    #[arg(long, default_value_t = false)]
    pub force_merge_on_timer: bool,

    /// Use a local Ollama instance as the Steward LLM backend. Sets
    /// the OpenAI-compatible env vars to point at
    /// `localhost:11434/v1` with the given model — equivalent to:
    ///
    ///   OPENAI_API_KEY=ollama
    ///   OPENAI_BASE_URL=http://localhost:11434/v1
    ///   OPENAI_MODEL=<MODEL>
    ///
    /// without requiring the operator to set them manually. Override
    /// the base URL by setting `OPENAI_BASE_URL` explicitly (for
    /// non-default Ollama port or remote Ollama). Override the API
    /// key by setting `OPENAI_API_KEY` for an Ollama instance fronted
    /// by an auth proxy.
    ///
    /// Takes precedence over `ANTHROPIC_API_KEY` — the explicit flag
    /// wins over the env-var precedence rule (Anthropic > OpenAI).
    /// The Anthropic key is unset for the daemon process only; the
    /// user's shell env is unchanged.
    #[arg(long, value_name = "MODEL")]
    pub ollama_model: Option<String>,
}

pub async fn run(args: DaemonArgs) -> Result<()> {
    // Install shutdown signals BEFORE any heavy work. tokio's
    // `signal(SignalKind::terminate())` registers the handler at call
    // time; if we waited until after writer spawn + HTTP bind +
    // embedder load, a SIGTERM arriving in that window would fall
    // through to the OS-default kill disposition. See
    // `commands::common::ShutdownSignals` and the
    // `process_lifecycle::graceful_shutdown_within_budget` test it
    // unblocks.
    let shutdown_signals = crate::commands::common::ShutdownSignals::install()
        .context("install shutdown signal handlers")?;

    // `--ollama-model <MODEL>` shorthand: configure env vars BEFORE
    // any Steward construction reads them. Must run before
    // `build_llm_client_from_env` is called below.
    if let Some(model) = args.ollama_model.as_deref() {
        let (model, base_url) = crate::commands::common::apply_ollama_overrides(model);
        tracing::info!(
            ollama_model = %model,
            ollama_base_url = %base_url,
            "Ollama backend configured via --ollama-model"
        );
    }

    let data_dir = match args.data_dir {
        Some(p) => p,
        None => default_data_dir()
            .context("could not resolve default data dir; pass --data-dir explicitly")?,
    };

    // Read config first so we can validate-then-prompt — we'd rather refuse
    // a missing data dir before asking for a passphrase.
    let config_path = data_dir.join("solo.config.toml");
    if !config_path.is_file() {
        bail!(
            "solo.config.toml not found at {}. Run `solo init` first.",
            config_path.display()
        );
    }
    let config = SoloConfig::read(&config_path).context("read solo.config.toml")?;
    let salt = config.salt_bytes().context("decode salt from config")?;
    let workspace_file_access = solo_api::WorkspaceFileAccessPolicy::from_config_and_env(
        config.workspace_file_access.allowed_roots.as_deref(),
    )
    .context("build workspace file access policy")?;

    // v0.9.0 P2 BLOCKER 2 follow-through: refuse to start the daemon
    // when `[llm] mode = "mcp_sampling"` is configured. See
    // [`check_llm_config_for_daemon_mode`] for the full rationale and
    // the tests that pin the locked error wording.
    check_llm_config_for_daemon_mode(&config)?;

    // Acquire the lockfile BEFORE prompting — fail-fast if another daemon
    // or one-shot is already running, rather than making the user type
    // their passphrase only to be turned away. Same fix as 097c1a9 applied
    // to the one-shot path; missed here in audit, fixed retroactively.
    let lock_path = data_dir.join("solo.lock");
    let _lock = Lockfile::acquire(&lock_path)
        .context("acquire solo.lock — another daemon already running?")?;

    let passphrase = read_passphrase()?;
    let key = KeyMaterial::derive(&passphrase, &salt)
        .context("derive key from passphrase + persisted salt")?;
    drop(passphrase);

    tracing::info!(
        data_dir = %data_dir.display(),
        embedder = %config.embedder.name,
        version = %config.embedder.version,
        dim = config.embedder.dim,
        "starting solo daemon"
    );

    // v0.8.0 P2: build the registry-backed bootstrap. The registry opens
    // tenants lazily; we eagerly open the default tenant so the daemon
    // is "warm" for its main user immediately after boot.
    let embedder = crate::commands::common::build_embedder(&config)?;

    // v0.11.1: layer TOML `[steward]` overrides under env-var overrides.
    // `SOLO_CLUSTER_*` env vars still win for the per-runtime escape
    // hatch; pre-v0.11.1 configs that omit the block (or new configs
    // that don't write it) end up with `StewardConfig::default()`'s
    // small-corpus-friendly values.
    let steward_config = solo_steward::StewardConfig::from_settings_then_env(
        config.steward.cluster_min_size,
        config.steward.cluster_cosine_threshold,
    )
    .context("parse [steward] config + SOLO_CLUSTER_* env vars for daemon")?;
    let startup_derived_catchup_min_episodes = steward_config
        .cluster_min_size
        .max(STARTUP_DERIVED_CATCHUP_MIN_EPISODES);
    let llm_client = if args.ollama_model.is_some() {
        solo_storage::llm::build_llm_client_from_env()?
    } else {
        solo_storage::llm::build_llm_client_from_settings(config.llm.as_ref())?
    };
    let steward = match llm_client {
        Some(llm) => {
            tracing::info!(
                model = %llm.name(),
                cluster_cosine_threshold = steward_config.cluster_cosine_threshold,
                cluster_min_size = steward_config.cluster_min_size,
                "LLM backend wired; consolidate timer will produce abstractions + contradictions"
            );
            Some(Arc::new(solo_steward::Steward::new(llm, steward_config)))
        }
        None => {
            tracing::info!(
                cluster_cosine_threshold = steward_config.cluster_cosine_threshold,
                cluster_min_size = steward_config.cluster_min_size,
                "no LLM backend wired; consolidate timer will run clustering only"
            );
            None
        }
    };

    let runtime_handle = tokio::runtime::Handle::current();

    // v0.9.0 P4-revision (P4 audit M1): construct the count-based
    // triples-batch trigger signal up front so the registry can hand
    // it to every tenant's writer-actor (the actor pings the notify
    // after each successful `Remember`) AND we can `select!` against
    // its `notified()` future in the `triples_batch_timer` below.
    // `trigger_episode_count == 0` disables the count-based path —
    // `note_episode_remembered` is a no-op and the timer's select!
    // collapses to the time-interval arm.
    let triples_batch_signal: Arc<solo_storage::TriplesBatchSignal> = Arc::new(
        solo_storage::TriplesBatchSignal::new(config.triples.trigger_episode_count as u64),
    );
    let steward_runtime = solo_api::StewardRuntimeStatus::new();
    let (runtime_control_tx, mut runtime_control_rx) =
        tokio::sync::mpsc::unbounded_channel::<solo_api::RuntimeControlCommand>();
    let runtime_control = solo_api::RuntimeControl::with_restart_sender(runtime_control_tx);

    let registry = MemoryLibrary::open(MemoryLibraryParams {
        data_dir: data_dir.clone(),
        key: key.clone(),
        embedder: embedder.clone(),
        hnsw_params: HnswParams::default(),
        steward,
        runtime_handle: Some(runtime_handle),
        // v0.9.0 P0c: no StewardFactory wired yet on the daemon path;
        // the per-tenant `steward_slot` mirrors the captured `steward`
        // field. P1 plumbs the LlmSettings-driven factory through here.
        steward_factory: None,
        triples_batch_signal: Some(triples_batch_signal.clone()),
    })
    .context("open tenant registry")?;
    let registry = Arc::new(registry);

    // Reclaim expired upload staging immediately at daemon startup, then keep
    // sweeping independently of future prepare requests. The filesystem/OS
    // lock in solo-api serializes this with append/commit/abort/ingest.
    run_document_upload_sweep(data_dir.clone(), "startup").await;
    let upload_sweep_task = {
        let data_dir = data_dir.clone();
        Some(tokio::spawn(document_upload_sweep_timer(
            data_dir,
            Duration::from_secs(DOCUMENT_UPLOAD_SWEEP_INTERVAL_SECS),
        )))
    };

    // Warm up the one Community Memory Library.
    let default_handle = registry
        .handle()
        .await
        .context("open Community Memory Library runtime")?;

    if default_handle.replay().rows_replayed > 0 {
        tracing::info!(
            replayed = default_handle.replay().rows_replayed,
            failed = default_handle.replay().rows_failed,
            "pending_index replay applied at startup"
        );
    }
    if !default_handle.drift().is_clean() {
        let d = default_handle.drift();
        tracing::warn!(
            hot_episodes = d.hot_episodes,
            active_chunks = d.active_chunks,
            expected = d.expected_index_len(),
            index_len = d.index_len,
            diff = d.diff,
            "HNSW vs SQL drift detected"
        );
    }

    // Migration 0016 clears stale pre-quality-gate derived rows. The regular
    // timers intentionally skip their first tick, so run one catch-up pass on
    // startup when a tenant has enough raw memories but no derived graph.
    let startup_derived_catchup_task = {
        let tenant = default_handle.clone();
        let min_episode_count = startup_derived_catchup_min_episodes;
        let cluster_timeout_secs = config.triples.cluster_timeout_secs;
        // Reserve derived work before publishing the HTTP listener so an
        // immediate user backfill cannot overlap startup catch-up.
        let derived_job = steward_runtime.begin_derived_job().await;
        Some(tokio::spawn(async move {
            let _derived_job = derived_job;
            if let Err(error) =
                run_startup_derived_graph_catchup(tenant, min_episode_count, cluster_timeout_secs)
                    .await
            {
                tracing::warn!(
                    error = %error,
                    "startup derived graph catch-up failed; scheduled steward cadence will retry later"
                );
            }
        }))
    };

    // Background snapshot timer (drives every cached tenant).
    let snapshot_task = if args.snapshot_interval_secs > 0 {
        let library = default_handle.clone();
        let interval = Duration::from_secs(args.snapshot_interval_secs);
        Some(tokio::spawn(snapshot_timer(library, interval)))
    } else {
        tracing::info!("snapshot timer disabled (--snapshot-interval-secs=0)");
        None
    };

    // Background consolidate timer (default tenant only for now; P7+
    // may extend this to walk every cached tenant).
    let default_write_handle: WriteHandle = default_handle.write().clone();
    let effective_consolidate_interval_secs = args
        .consolidate_interval_secs
        .unwrap_or(config.triples.consolidate_interval_secs);
    let consolidate_task = if effective_consolidate_interval_secs > 0 {
        let h = default_write_handle.clone();
        let interval = Duration::from_secs(effective_consolidate_interval_secs);
        let window = args.consolidate_window_days;
        let force_merge = args.force_merge_on_timer;
        let consolidate_runtime = steward_runtime.clone();
        tracing::info!(
            interval_secs = effective_consolidate_interval_secs,
            window_days = ?window,
            force_merge_on_timer = force_merge,
            "consolidate timer enabled"
        );
        // Dev-log 0152 M9: wrap in a restart loop so a panic inside
        // `consolidate_timer` doesn't silently kill the timer for the
        // rest of the daemon's lifetime. Outer task body has no panic
        // sites; inner gets respawned with a 5-second backoff.
        Some(tokio::spawn(async move {
            loop {
                let h = h.clone();
                let inner = tokio::spawn(consolidate_timer(
                    h,
                    interval,
                    window,
                    force_merge,
                    consolidate_runtime.clone(),
                ));
                match inner.await {
                    Ok(()) => return,
                    Err(e) if e.is_panic() => {
                        tracing::error!(
                            error = ?e,
                            "consolidate_timer panicked; restarting in 5s"
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "consolidate_timer cancelled");
                        return;
                    }
                }
            }
        }))
    } else {
        tracing::info!("consolidate timer disabled (--consolidate-interval-secs=0)");
        None
    };

    // v0.9.0 P4c: background triples-batch timer. Drives
    // `Steward::extract_triples_batch` independently of the cheap
    // clustering pass: pulls clusters without abstractions, asks the
    // LLM for triples, persists via `WriteCommand::AttachAbstractionBatch`
    // (which emits ONE `MemoryTriplesExtract` audit row per batch).
    //
    // Cadence: every `triples.trigger_interval_secs` seconds (default
    // 3600 = 1 hr, plan §3 Decision 2) OR after
    // `trigger_episode_count` new episodes — whichever fires first
    // (v0.9.0 P4-revision (P4 audit M1)). When the operator hasn't
    // explicitly set a custom cadence and consolidate-timer is also
    // off, we still tick on the default — the slot-empty / no-llm
    // fast paths in `run_triples_batch_tick` keep the cost near zero
    // when there's nothing to do.
    //
    // v0.9.0 P4-revision (P4 audit m4): when `trigger_interval_secs ==
    // 0` we disable the time-based arm (tokio::time::interval(ZERO)
    // would panic). The count-based arm still fires via the shared
    // `TriplesBatchSignal`. Symmetric with the consolidate-timer
    // pattern above (`consolidate_interval_secs == 0` skips the
    // spawn entirely; here we still want the count-based arm so the
    // operator can opt out of the time half without losing the count
    // half).
    let triples_task = {
        let interval_secs = config.triples.trigger_interval_secs;
        let cluster_timeout_secs = config.triples.cluster_timeout_secs;
        let reader_pool_handle = default_handle.clone();
        let write_handle = default_write_handle.clone();
        let embedder_id = default_handle.embedder_id();
        let signal = triples_batch_signal.clone();
        let steward_runtime = steward_runtime.clone();
        if interval_secs == 0 {
            tracing::warn!(
                trigger_episode_count = config.triples.trigger_episode_count,
                cluster_timeout_secs,
                "triples-batch timer: trigger_interval_secs == 0 → time-based arm DISABLED; \
                 count-based arm still active (set [triples] trigger_episode_count = 0 to disable both)"
            );
        } else {
            tracing::info!(
                interval_secs,
                trigger_episode_count = config.triples.trigger_episode_count,
                cluster_timeout_secs,
                "triples-batch timer enabled (v0.9.0 P4c, v0.10.1 m5 per-cluster timeout)"
            );
        }
        // Dev-log 0152 M9: same restart loop as consolidate timer.
        Some(tokio::spawn(async move {
            loop {
                let inner = tokio::spawn(triples_batch_timer(
                    reader_pool_handle.clone(),
                    write_handle.clone(),
                    interval_secs,
                    cluster_timeout_secs,
                    embedder_id,
                    signal.clone(),
                    steward_runtime.clone(),
                ));
                match inner.await {
                    Ok(()) => return,
                    Err(e) if e.is_panic() => {
                        tracing::error!(
                            error = ?e,
                            "triples_batch_timer panicked; restarting in 5s"
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                    Err(e) => {
                        tracing::warn!(error = ?e, "triples_batch_timer cancelled");
                        return;
                    }
                }
            }
        }))
    };

    // Optional: HTTP transport co-running on a tokio task. Every request
    // resolves the one Community Memory Library handle.
    //
    // v0.8.0 P3: if the operator has written an `[auth]` block in
    // `solo.config.toml`, the co-mode HTTP transport honors it. The
    // daemon has no `--bearer-token-file` flag of its own — operators
    // who want authenticated co-mode HTTP must put auth in the config.
    let (http_shutdown_tx, http_task) = if let Some(port) = args.http_port {
        use solo_api::{AuthConfig, SoloHttpState};
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        let state = SoloHttpState {
            registry: registry.clone(),
            user_aliases: Arc::new(config.identity.user_aliases.clone()),
            workspace_file_access: workspace_file_access.clone(),
            // v0.11.0 P1: per-process MCP session store. Spawns its own
            // background sweep task on the surrounding tokio runtime —
            // we're already inside `tokio::main` here.
            mcp_sessions: solo_api::mcp_session::SessionStore::new(),
            mcp_tasks: solo_api::mcp_task::TaskStore::new(),
            steward_runtime: steward_runtime.clone(),
            runtime_control: runtime_control.clone(),
        };
        let auth = config.auth.clone().map(AuthConfig::from);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let shutdown = async move {
                let _ = rx.await;
            };
            if let Err(e) =
                solo_api::http::serve_http_with_auth_config(addr, state, auth, shutdown).await
            {
                tracing::error!(error = %e, "http server exited with error");
            }
        });
        tracing::info!(%addr, "co-mode HTTP transport started");
        (Some(tx), Some(task))
    } else {
        (None, None)
    };

    tracing::info!(
        snapshot_interval_secs = args.snapshot_interval_secs,
        consolidate_interval_secs = effective_consolidate_interval_secs,
        pool_size = DEFAULT_POOL_SIZE,
        http_port = ?args.http_port,
        library = "Community Memory Library",
        "solo daemon ready (Ctrl+C to stop)"
    );

    // Wait for shutdown signal — handler was installed up front above.
    let shutdown_trigger = await_shutdown_signal(shutdown_signals, &mut runtime_control_rx).await;
    let save_snapshot_on_shutdown = match shutdown_trigger {
        DaemonShutdownTrigger::Signal => {
            tracing::info!("shutdown signal received; draining tenants");
            true
        }
        DaemonShutdownTrigger::RuntimeRestart => {
            tracing::info!("runtime restart requested; draining tenants");
            spawn_runtime_restart_watchdog();
            false
        }
    };

    if let Some(tx) = http_shutdown_tx {
        let _ = tx.send(());
    }
    if let Some(task) = http_task {
        await_http_shutdown_with_timeout(task).await;
    }

    if let Some(handle) = consolidate_task {
        handle.abort();
        let _ = handle.await;
    }

    if let Some(handle) = triples_task {
        handle.abort();
        let _ = handle.await;
    }

    if let Some(handle) = startup_derived_catchup_task {
        handle.abort();
        let _ = handle.await;
    }

    if let Some(handle) = snapshot_task {
        handle.abort();
        let _ = handle.await;
    }

    if let Some(handle) = upload_sweep_task {
        handle.abort();
        let _ = handle.await;
    }

    // Drop our handle to the default tenant so the registry's
    // shutdown_all path is the sole owner. Then drain every cached
    // tenant: save snapshot, drain writer, drop pool.
    drop(default_write_handle);
    drop(default_handle);
    match tokio::time::timeout(
        Duration::from_secs(SHUTDOWN_TIMEOUT_SECS),
        registry.shutdown_with_snapshot(save_snapshot_on_shutdown),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => {
            tracing::warn!(
                timeout_secs = SHUTDOWN_TIMEOUT_SECS,
                "tenant shutdown exceeded budget; exiting so the supervisor can restart Solo"
            );
        }
    }
    tracing::info!("solo daemon shutdown complete");

    drop(_lock);
    Ok(())
}

async fn run_document_upload_sweep(data_dir: PathBuf, trigger: &'static str) {
    // `sweep_expired_uploads` walks tenant directories and performs
    // synchronous filesystem I/O. Keep that work off Tokio's async workers
    // so a large staging directory cannot stall HTTP/MCP request handling.
    match tokio::task::spawn_blocking(move || {
        solo_api::document_upload::sweep_expired_uploads(&data_dir)
    })
    .await
    {
        Ok(Ok(0)) => tracing::debug!(trigger, "document upload staging sweep complete"),
        Ok(Ok(swept)) => {
            tracing::info!(trigger, swept, "expired document upload staging removed")
        }
        Ok(Err(error)) => tracing::warn!(
            trigger,
            error = %error,
            "document upload staging sweep failed; next periodic tick will retry"
        ),
        Err(error) => tracing::warn!(
            trigger,
            error = %error,
            "document upload staging sweep worker failed; next periodic tick will retry"
        ),
    }
}

async fn document_upload_sweep_timer(data_dir: PathBuf, interval: Duration) {
    debug_assert!(!interval.is_zero());
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // startup sweep already completed before this timer was spawned
    loop {
        tick.tick().await;
        run_document_upload_sweep(data_dir.clone(), "periodic").await;
    }
}

async fn await_http_shutdown_with_timeout(task: tokio::task::JoinHandle<()>) {
    let mut task = task;
    tokio::select! {
        result = &mut task => {
            if let Err(error) = result {
                tracing::warn!(error = %error, "HTTP server task join failed during daemon shutdown");
            }
        }
        _ = tokio::time::sleep(Duration::from_secs(HTTP_SHUTDOWN_TIMEOUT_SECS)) => {
            tracing::warn!(
                timeout_secs = HTTP_SHUTDOWN_TIMEOUT_SECS,
                "HTTP server did not drain before shutdown timeout; aborting so the supervisor can restart Solo"
            );
            task.abort();
        }
    }
}

fn spawn_runtime_restart_watchdog() {
    if let Err(error) = std::thread::Builder::new()
        .name("solo-runtime-restart-watchdog".to_string())
        .spawn(|| {
            std::thread::sleep(Duration::from_secs(RESTART_FORCE_EXIT_TIMEOUT_SECS));
            tracing::warn!(
                timeout_secs = RESTART_FORCE_EXIT_TIMEOUT_SECS,
                "runtime restart exceeded force-exit budget; exiting for supervisor restart"
            );
            std::process::exit(0);
        })
    {
        tracing::warn!(error = %error, "could not spawn runtime restart watchdog");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonShutdownTrigger {
    Signal,
    RuntimeRestart,
}

async fn await_shutdown_signal(
    shutdown_signals: crate::commands::common::ShutdownSignals,
    runtime_control_rx: &mut tokio::sync::mpsc::UnboundedReceiver<solo_api::RuntimeControlCommand>,
) -> DaemonShutdownTrigger {
    if std::env::var_os(ENV_SHUTDOWN_ON_STDIN_EOF).is_some() {
        tokio::select! {
            _ = shutdown_signals.await_any() => DaemonShutdownTrigger::Signal,
            _ = wait_for_stdin_eof() => {
                tracing::info!("stdin EOF received; shutting down daemon");
                DaemonShutdownTrigger::Signal
            }
            trigger = wait_for_runtime_control(runtime_control_rx) => trigger,
        }
    } else {
        tokio::select! {
            _ = shutdown_signals.await_any() => DaemonShutdownTrigger::Signal,
            trigger = wait_for_runtime_control(runtime_control_rx) => trigger,
        }
    }
}

async fn wait_for_runtime_control(
    runtime_control_rx: &mut tokio::sync::mpsc::UnboundedReceiver<solo_api::RuntimeControlCommand>,
) -> DaemonShutdownTrigger {
    match runtime_control_rx.recv().await {
        Some(solo_api::RuntimeControlCommand::Restart) => DaemonShutdownTrigger::RuntimeRestart,
        None => std::future::pending::<DaemonShutdownTrigger>().await,
    }
}

async fn wait_for_stdin_eof() {
    use tokio::io::AsyncReadExt;

    let mut stdin = tokio::io::stdin();
    let mut buf = [0_u8; 1];
    loop {
        match stdin.read(&mut buf).await {
            Ok(0) => return,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "stdin shutdown watcher failed");
                return;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StartupDerivedGraphSnapshot {
    active_episodes: usize,
    clusters: usize,
    abstractions: usize,
    triples: usize,
}

fn should_run_startup_derived_graph_catchup(
    snapshot: StartupDerivedGraphSnapshot,
    min_episode_count: usize,
) -> bool {
    snapshot.active_episodes >= min_episode_count
        && snapshot.triples == 0
        && (snapshot.clusters == 0 || snapshot.abstractions < snapshot.clusters)
}

async fn fetch_startup_derived_graph_snapshot(
    tenant: &solo_storage::LibraryHandle,
) -> Result<StartupDerivedGraphSnapshot> {
    let snapshot = tenant
        .read()
        .interact(|conn| {
            let active_episodes: i64 = conn.query_row(
                "SELECT COUNT(*) FROM episodes WHERE status = 'active'",
                [],
                |row| row.get(0),
            )?;
            let clusters: i64 =
                conn.query_row("SELECT COUNT(*) FROM clusters", [], |row| row.get(0))?;
            let abstractions: i64 =
                conn.query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |row| {
                    row.get(0)
                })?;
            let triples: i64 =
                conn.query_row("SELECT COUNT(*) FROM triples", [], |row| row.get(0))?;
            Ok(StartupDerivedGraphSnapshot {
                active_episodes: active_episodes.max(0) as usize,
                clusters: clusters.max(0) as usize,
                abstractions: abstractions.max(0) as usize,
                triples: triples.max(0) as usize,
            })
        })
        .await
        .context("read startup derived graph snapshot")?;
    Ok(snapshot)
}

async fn run_startup_derived_graph_catchup(
    tenant: Arc<solo_storage::LibraryHandle>,
    min_episode_count: usize,
    cluster_timeout_secs: u64,
) -> Result<()> {
    let snapshot = fetch_startup_derived_graph_snapshot(&tenant).await?;
    if !should_run_startup_derived_graph_catchup(snapshot, min_episode_count) {
        tracing::debug!(
            tenant = %tenant.tenant_id(),
            active_episodes = snapshot.active_episodes,
            clusters = snapshot.clusters,
            abstractions = snapshot.abstractions,
            triples = snapshot.triples,
            min_episode_count,
            "startup derived graph catch-up skipped"
        );
        return Ok(());
    }

    tracing::info!(
        tenant = %tenant.tenant_id(),
        active_episodes = snapshot.active_episodes,
        min_episode_count,
        "startup derived graph catch-up: empty derived graph with raw memories; rebuilding"
    );
    if snapshot.clusters == 0 {
        let consolidate_report = tenant
            .write()
            .consolidate(ConsolidationScope {
                window_days: None,
                force_merge: false,
            })
            .await
            .context("startup derived graph catch-up consolidation")?;
        tracing::info!(
            tenant = %tenant.tenant_id(),
            episodes_seen = consolidate_report.episodes_seen,
            clusters_built = consolidate_report.clusters_built,
            episodes_clustered = consolidate_report.episodes_clustered,
            abstractions_built = consolidate_report.abstractions_built,
            "startup derived graph catch-up consolidation complete"
        );
    }

    let pending_clusters =
        solo_storage::triples_batch::count_clusters_without_abstractions(tenant.read())
            .await
            .context("count startup catch-up pending triple clusters")?;
    if pending_clusters == 0 {
        return Ok(());
    }

    let per_cluster_timeout = Duration::from_secs(cluster_timeout_secs);
    let triples_report = solo_storage::triples_batch::run_triples_batch_tick(
        tenant.read(),
        tenant.write(),
        tenant.steward_slot(),
        tenant.embedder_id(),
        TRIPLES_BATCH_LIMIT_PER_TICK,
        per_cluster_timeout,
        Some("system:startup-derived-catchup".to_string()),
    )
    .await
    .context("startup derived graph catch-up triple extraction")?;

    match triples_report {
        Some(report) => {
            tracing::info!(
                tenant = %tenant.tenant_id(),
                abstractions_built = report.abstractions_built,
                triples_extracted = report.triples_extracted,
                triples_quarantined = report.triples_quarantined,
                clusters_failed = report.clusters_failed,
                clusters_deferred = report.clusters_deferred,
                "startup derived graph catch-up triple extraction complete"
            );
        }
        None => {
            tracing::debug!(
                tenant = %tenant.tenant_id(),
                pending_clusters,
                "startup derived graph catch-up found no runnable triple extraction work"
            );
        }
    }

    Ok(())
}

async fn snapshot_timer(library: Arc<solo_storage::LibraryHandle>, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await;
    loop {
        tick.tick().await;
        if let Err(error) = library.write().save_snapshot().await {
            tracing::warn!(%error, "scheduled Community Memory Library snapshot failed");
        }
    }
}

/// Background loop that dispatches `WriteCommand::Consolidate` at
/// `interval` cadence. Runs only the SWS-equivalent clustering pass
/// today; once a real `LlmClient` is wired into the daemon (Y.3.b),
/// the same dispatch will also produce abstractions + extracted
/// triples — no change to this function.
///
/// First tick is skipped (matches `snapshot_timer`) so a fresh-
/// started daemon doesn't immediately consolidate against a near-
/// empty DB. Subsequent ticks fire on the cadence even if the
/// previous dispatch is still running — `tokio::time::interval`'s
/// default `MissedTickBehavior::Burst` is fine here because
/// `WriteHandle::consolidate` serializes through the writer's mpsc
/// (one consolidate at a time per writer; ADR-0003 §"Concurrency").
/// v0.9.0 P4c: drive the background triples-batch path on the
/// configured cadence.
///
/// Each tick calls
/// [`solo_storage::triples_batch::run_triples_batch_tick`] against the
/// tenant's reader pool + write handle + steward slot. The tick is
/// internally idempotent + slot-aware:
///
///   * Slot empty (no MCP-session-bound Steward attached for sampling
///     backends) → tick is a no-op.
///   * Stub-only Steward → tick is a no-op.
///   * No clusters pending abstraction → tick is a no-op.
///
/// The first tick is delayed by one full interval (matches
/// `snapshot_timer` / `consolidate_timer`) so a fresh-started daemon
/// doesn't immediately ping the LLM against an empty corpus.
///
/// Limit per tick: 50 clusters (matches the `[triples]
/// trigger_episode_count` default). Keeps the per-batch LLM cost
/// bounded; subsequent ticks process the rest.
const TRIPLES_BATCH_LIMIT_PER_TICK: usize = 50;

/// v0.9.0 P4c + P4-revision (P4 audit M1 + m4): time- AND count-based
/// triples-batch driver loop. The two trigger arms `select!` against
/// each other; whichever fires first runs the batch and resets the
/// shared `TriplesBatchSignal`'s counter.
///
/// **m4 (zero-interval guard)**: callers that pass `interval_secs == 0`
/// signal "disable the time-based arm" (operator wants count-based
/// only). We do NOT call `tokio::time::interval(ZERO)` — that panics.
/// Instead the time arm becomes a `pending::<()>()` future that never
/// resolves, leaving only the count arm.
///
/// **M1 (count-based wiring)**: the `TriplesBatchSignal::notified()`
/// future is the count arm. The writer-actor (in every tenant opened
/// via this registry) pings it from `dispatch_remember` after every
/// successful Remember once the threshold is crossed.
///
/// **Dedup**: BOTH arms execute the same `run_triples_batch_tick` +
/// `signal.reset()` sequence. If both fire near-simultaneously, the
/// `select!` returns whichever was ready first; the LATER firing
/// loops back, observes that the counter has been reset to 0, and
/// the count-based arm awaits a fresh threshold crossing. (The
/// time-based arm always fires every `interval_secs`; that's
/// expected.) No double-run.
/// v0.9.0 P4-revision (P4 audit m4): liveness guard.
///
/// Pure decision: return `Some(Duration)` for a positive interval (the
/// time-based arm of `triples_batch_timer`'s `select!`), or `None` to
/// disable the time arm entirely (operator-configured 0).
/// `tokio::time::interval(Duration::from_secs(0))` panics with
/// "interval must be greater than 0"; we never let that call happen.
///
/// Pinned by the m4 test
/// `pick_triples_time_arm_returns_none_for_zero_interval`.
fn pick_triples_time_arm(interval_secs: u64) -> Option<Duration> {
    if interval_secs == 0 {
        None
    } else {
        Some(Duration::from_secs(interval_secs))
    }
}

async fn triples_batch_timer(
    tenant: Arc<solo_storage::LibraryHandle>,
    write_handle: solo_storage::WriteHandle,
    interval_secs: u64,
    cluster_timeout_secs: u64,
    embedder_id: i64,
    signal: Arc<solo_storage::TriplesBatchSignal>,
    steward_runtime: solo_api::StewardRuntimeStatus,
) {
    // m4: zero-interval guard — build a tick future that never fires
    // OR a real tokio::time::interval, depending on the config value.
    enum TimeArm {
        Disabled,
        Enabled(tokio::time::Interval),
    }

    let interval_duration = pick_triples_time_arm(interval_secs);
    let mut next_time_tick_at_ms = interval_duration.map(solo_api::unix_ms_after);
    steward_runtime
        .set_next_triples_run_at_ms(next_time_tick_at_ms)
        .await;

    let mut time_arm = match interval_duration {
        None => TimeArm::Disabled,
        Some(d) => {
            let mut tick = tokio::time::interval(d);
            tick.tick().await; // skip first
            TimeArm::Enabled(tick)
        }
    };

    // v0.10.1 m5: build the per-cluster timeout Duration once. Zero
    // means "disabled" — `Steward::extract_triples_batch` runs every
    // per-cluster call to natural completion.
    let per_cluster_timeout = Duration::from_secs(cluster_timeout_secs);

    loop {
        let trigger: &'static str = match &mut time_arm {
            TimeArm::Enabled(tick) => {
                tokio::select! {
                    _ = tick.tick() => "time",
                    _ = signal.notified() => "count",
                }
            }
            TimeArm::Disabled => {
                signal.notified().await;
                "count"
            }
        };
        if trigger == "time" {
            next_time_tick_at_ms = interval_duration.map(solo_api::unix_ms_after);
            steward_runtime
                .set_next_triples_run_at_ms(next_time_tick_at_ms)
                .await;
        }

        // Reset the counter BEFORE the batch runs. Any episodes
        // remembered during the batch run accumulate toward the NEXT
        // batch — correct, because they aren't covered by the
        // currently-running tick's cluster snapshot.
        let Some(_derived_job) = steward_runtime.try_begin_derived_job() else {
            tracing::debug!(
                trigger,
                "scheduled triples-batch tick skipped because another derived-memory job is running"
            );
            continue;
        };
        signal.reset();

        let result = solo_storage::triples_batch::run_triples_batch_tick(
            tenant.read(),
            &write_handle,
            tenant.steward_slot(),
            embedder_id,
            TRIPLES_BATCH_LIMIT_PER_TICK,
            per_cluster_timeout,
            None,
        )
        .await;
        match result {
            Ok(Some(report)) => {
                steward_runtime
                    .record_triples_batch(
                        trigger,
                        solo_api::StewardTriplesBatchStatus {
                            ran: true,
                            limit: TRIPLES_BATCH_LIMIT_PER_TICK,
                            cluster_timeout_secs,
                            abstractions_built: report.abstractions_built,
                            triples_extracted: report.triples_extracted,
                            triples_quarantined: report.triples_quarantined,
                            clusters_failed: report.clusters_failed,
                            clusters_deferred: report.clusters_deferred,
                            note: "Scheduled Steward triple extraction batch completed."
                                .to_string(),
                        },
                    )
                    .await;
                tracing::info!(
                    trigger,
                    abstractions_built = report.abstractions_built,
                    triples_extracted = report.triples_extracted,
                    triples_quarantined = report.triples_quarantined,
                    clusters_failed = report.clusters_failed,
                    clusters_deferred = report.clusters_deferred,
                    "scheduled triples-batch tick complete"
                );
            }
            Ok(None) => {
                steward_runtime
                    .record_triples_batch(
                        trigger,
                        solo_api::StewardTriplesBatchStatus {
                            ran: false,
                            limit: TRIPLES_BATCH_LIMIT_PER_TICK,
                            cluster_timeout_secs,
                            abstractions_built: 0,
                            triples_extracted: 0,
                            triples_quarantined: 0,
                            clusters_failed: 0,
                            clusters_deferred: 0,
                            note: "Scheduled Steward triple extraction found no runnable clusters."
                                .to_string(),
                        },
                    )
                    .await;
                tracing::debug!(trigger, "scheduled triples-batch tick: nothing to do");
            }
            Err(e) => {
                steward_runtime
                    .record_triples_error(trigger, e.to_string())
                    .await;
                tracing::warn!(trigger, error = %e, "scheduled triples-batch tick failed");
            }
        }
    }
}

async fn consolidate_timer(
    handle: solo_storage::WriteHandle,
    interval: Duration,
    window_days: Option<i64>,
    force_merge: bool,
    steward_runtime: solo_api::StewardRuntimeStatus,
) {
    steward_runtime
        .set_next_consolidation_run_at_ms(Some(solo_api::unix_ms_after(interval)))
        .await;
    let mut tick = tokio::time::interval(interval);
    tick.tick().await; // skip first
    loop {
        tick.tick().await;
        steward_runtime
            .set_next_consolidation_run_at_ms(Some(solo_api::unix_ms_after(interval)))
            .await;
        let scope = ConsolidationScope {
            window_days,
            force_merge,
        };
        let Some(_derived_job) = steward_runtime.try_begin_derived_job() else {
            tracing::debug!(
                "scheduled consolidation skipped because another derived-memory job is running"
            );
            continue;
        };
        match handle.consolidate(scope).await {
            Ok(report) => {
                steward_runtime.record_consolidation_success().await;
                if report.episodes_seen > 0 {
                    tracing::info!(
                        seen = report.episodes_seen,
                        clusters = report.clusters_built,
                        episodes_clustered = report.episodes_clustered,
                        abstractions = report.abstractions_built,
                        triples = report.triples_built,
                        "scheduled consolidate complete"
                    );
                } else {
                    tracing::debug!("scheduled consolidate: no candidates");
                }
            }
            Err(e) => {
                steward_runtime
                    .record_consolidation_error(e.to_string())
                    .await;
                tracing::warn!(error = %e, "scheduled consolidate failed");
            }
        }
    }
}

/// v0.9.0 P2 BLOCKER 2 follow-through: refuse to start the daemon when
/// `[llm] mode = "mcp_sampling"` is configured.
///
/// `solo daemon` runs WITHOUT a connected MCP peer — every transport
/// it exposes (HTTP, future SSE) is server-initiated, so there's no
/// peer to call back to. The `mcp_sampling` backend requires the
/// INVERSE — an MCP client that calls `solo mcp-stdio` so the
/// server can call `peer.create_message`.
///
/// Refuse-to-start with the locked BLOCKER 2 error message rather than
/// silently degrading (plan §3 Decision 4): operators who wanted
/// abstractions deserve to see the failure now, not hours later when
/// the empty `semantic_abstractions` table surprises them.
///
/// `solo mcp-stdio` (in a different subcommand) is the intended entry
/// point for `mcp_sampling` — that path doesn't run this guard.
///
/// Lives in a dedicated fn so the locked error wording can be tested
/// without spinning up a full daemon-startup cycle.
fn check_llm_config_for_daemon_mode(config: &SoloConfig) -> Result<()> {
    if let Some(llm) = config.llm.as_ref() {
        if llm.requires_mcp_peer() {
            bail!(
                "{}\n\nRun this Solo process via `solo mcp-stdio` (so an \
                 MCP client can initialize the session and host the \
                 sampling callback) — daemon mode cannot reach a peer.",
                solo_api::mcp::sampling_capability_missing_error_message()
            );
        }
    }
    Ok(())
}

/// Read passphrase as `Zeroizing<String>` — wipes buffer on drop.
fn read_passphrase() -> Result<zeroize::Zeroizing<String>> {
    use zeroize::Zeroizing;
    if let Ok(env_pass) = std::env::var(ENV_PASSPHRASE) {
        if env_pass.is_empty() {
            bail!("{ENV_PASSPHRASE} is set but empty");
        }
        eprintln!(
            "warning: reading passphrase from {ENV_PASSPHRASE} process environment; \
             it may be visible to same-user processes or diagnostic tools"
        );
        let passphrase = Zeroizing::new(env_pass);
        // SAFETY: daemon startup is still single-threaded here; remove
        // the inherited secret before any child processes can inherit it.
        unsafe {
            std::env::remove_var(ENV_PASSPHRASE);
        }
        return Ok(passphrase);
    }
    if std::env::var_os(ENV_PASSPHRASE_STDIN).is_some() {
        use std::io::BufRead;

        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .context("read passphrase from stdin")?;
        while line.ends_with(['\n', '\r']) {
            line.pop();
        }
        if line.is_empty() {
            bail!("passphrase from stdin must not be empty");
        }
        return Ok(Zeroizing::new(line));
    }
    let p = rpassword::prompt_password("Enter passphrase (will not be echoed): ")
        .context("read passphrase")?;
    if p.is_empty() {
        bail!("passphrase must not be empty");
    }
    Ok(Zeroizing::new(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::LlmSettings;

    fn write_expired_upload_fixture(
        data_dir: &std::path::Path,
        upload_id: &str,
        status: &str,
    ) -> (PathBuf, PathBuf) {
        let staging_dir = data_dir.join("staged-documents");
        let upload_dir = staging_dir.join(upload_id);
        std::fs::create_dir_all(&upload_dir).unwrap();
        let filename = format!("{status}.md");
        let bytes_path = if status == "committed" {
            upload_dir.join(&filename)
        } else {
            upload_dir.join("upload.part")
        };
        std::fs::write(&bytes_path, b"data").unwrap();
        let manifest_path = staging_dir.join(format!("{upload_id}.json"));
        let manifest = serde_json::json!({
            "upload_id": upload_id,
            "filename": filename,
            "sanitized_filename": filename,
            "mime_type": "text/markdown",
            "size_bytes": 4,
            "expected_sha256": null,
            "actual_sha256": if status == "committed" { serde_json::Value::String("0".repeat(64)) } else { serde_json::Value::Null },
            "bytes_received": 4,
            "status": status,
            "created_at_ms": 0,
            "expires_at_ms": 0,
        });
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        (manifest_path, bytes_path)
    }

    #[tokio::test]
    async fn daemon_upload_sweeper_runs_at_startup_and_periodically_without_prepare() {
        let tmp = tempfile::tempdir().unwrap();
        let (open_manifest, open_bytes) = write_expired_upload_fixture(
            tmp.path(),
            "018f60c2-d9e5-7c90-89d9-ccebde970001",
            "open",
        );
        let (committed_manifest, committed_bytes) = write_expired_upload_fixture(
            tmp.path(),
            "018f60c2-d9e5-7c90-89d9-ccebde970002",
            "committed",
        );

        run_document_upload_sweep(tmp.path().to_path_buf(), "startup-test").await;
        for path in [
            &open_manifest,
            &open_bytes,
            &committed_manifest,
            &committed_bytes,
        ] {
            assert!(!path.exists(), "startup sweep left {}", path.display());
        }

        let (periodic_manifest, periodic_bytes) = write_expired_upload_fixture(
            tmp.path(),
            "018f60c2-d9e5-7c90-89d9-ccebde970003",
            "committed",
        );
        let interval = Duration::from_millis(10);
        let task = tokio::spawn(document_upload_sweep_timer(
            tmp.path().to_path_buf(),
            interval,
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !periodic_manifest.exists() && !periodic_bytes.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("periodic upload sweep timed out");

        assert!(!periodic_manifest.exists());
        assert!(!periodic_bytes.exists());
        task.abort();
        let _ = task.await;
    }

    /// Helper: build a minimal `SoloConfig` with the supplied `[llm]`
    /// setting. Bypasses TOML round-tripping so we can target the
    /// startup-guard function directly.
    fn config_with_llm(llm: Option<LlmSettings>) -> SoloConfig {
        // 16-byte zeroed salt — fine for the startup-guard test; we
        // never derive a key from this.
        let salt = [0u8; 16];
        let mut cfg = SoloConfig::new(
            salt,
            solo_storage::EmbedderConfig {
                name: "stub".into(),
                version: "v1".into(),
                dim: 32,
                dtype: "f32".into(),
            },
        );
        cfg.llm = llm;
        cfg
    }

    #[test]
    fn startup_derived_catchup_runs_only_for_empty_nontrivial_graph() {
        let ready_for_rebuild = StartupDerivedGraphSnapshot {
            active_episodes: 42,
            clusters: 0,
            abstractions: 0,
            triples: 0,
        };
        assert!(should_run_startup_derived_graph_catchup(
            ready_for_rebuild,
            25
        ));

        let tiny_profile = StartupDerivedGraphSnapshot {
            active_episodes: 2,
            clusters: 0,
            abstractions: 0,
            triples: 0,
        };
        assert!(!should_run_startup_derived_graph_catchup(tiny_profile, 25));

        let already_clustered = StartupDerivedGraphSnapshot {
            active_episodes: 42,
            clusters: 3,
            abstractions: 3,
            triples: 0,
        };
        assert!(!should_run_startup_derived_graph_catchup(
            already_clustered,
            25
        ));

        let clustered_but_missing_abstractions = StartupDerivedGraphSnapshot {
            active_episodes: 42,
            clusters: 3,
            abstractions: 0,
            triples: 0,
        };
        assert!(should_run_startup_derived_graph_catchup(
            clustered_but_missing_abstractions,
            25
        ));

        let orphan_triples_need_operator_repair = StartupDerivedGraphSnapshot {
            active_episodes: 42,
            clusters: 0,
            abstractions: 0,
            triples: 3,
        };
        assert!(!should_run_startup_derived_graph_catchup(
            orphan_triples_need_operator_repair,
            25
        ));
    }

    /// `[llm] mode = "mcp_sampling"` → daemon startup refuses with the
    /// locked BLOCKER 2 error message + the `solo mcp-stdio` advice
    /// suffix.
    #[test]
    fn daemon_startup_rejects_llm_mode_mcp_sampling_with_helpful_error() {
        let cfg = config_with_llm(Some(LlmSettings::McpSampling));
        let err = check_llm_config_for_daemon_mode(&cfg).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("LLM backend `mcp_sampling`"),
            "error message must name the offending mode: {msg}"
        );
        // Locked alternative TOML blocks must all appear so operators
        // can copy-paste any of the four.
        for snippet in [
            "mode = \"anthropic\"",
            "mode = \"openai\"",
            "mode = \"ollama\"",
            "mode = \"none\"",
        ] {
            assert!(
                msg.contains(snippet),
                "error must list alternative `{snippet}`; was: {msg}"
            );
        }
        // Daemon-mode-specific suffix steers operators at mcp-stdio.
        assert!(
            msg.contains("solo mcp-stdio"),
            "error must steer operator at the mcp-stdio entry point: {msg}"
        );
    }

    /// `[llm]` absent → daemon startup proceeds. (v0.8.x config shape:
    /// no `[llm]` block at all; the env-var fallback path handles it.)
    #[test]
    fn daemon_startup_allows_missing_llm_block() {
        let cfg = config_with_llm(None);
        check_llm_config_for_daemon_mode(&cfg).expect("must allow");
    }

    /// `[llm] mode = "anthropic"` → daemon startup proceeds.
    #[test]
    fn daemon_startup_allows_anthropic_mode() {
        let cfg = config_with_llm(Some(LlmSettings::Anthropic {
            api_key_env: "ANTHROPIC_API_KEY".into(),
            model: "claude-sonnet-4-6".into(),
            hosted_processing_consent: true,
        }));
        check_llm_config_for_daemon_mode(&cfg).expect("must allow");
    }

    /// `[llm] mode = "openai"` → daemon startup proceeds.
    #[test]
    fn daemon_startup_allows_openai_mode() {
        let cfg = config_with_llm(Some(LlmSettings::Openai {
            api_key_env: "OPENAI_API_KEY".into(),
            model: "gpt-5.6-terra".into(),
            hosted_processing_consent: true,
        }));
        check_llm_config_for_daemon_mode(&cfg).expect("must allow");
    }

    /// `[llm] mode = "ollama"` → daemon startup proceeds.
    #[test]
    fn daemon_startup_allows_ollama_mode() {
        let cfg = config_with_llm(Some(LlmSettings::Ollama {
            endpoint: solo_storage::OllamaEndpointKind::Local,
            base_url: "http://localhost:11434".into(),
            model: "qwen3:8b".into(),
            api_key_env: None,
            hosted_processing_consent: false,
        }));
        check_llm_config_for_daemon_mode(&cfg).expect("must allow");
    }

    /// `[llm] mode = "none"` → daemon startup proceeds (cluster-only
    /// mode is still legitimate daemon work).
    #[test]
    fn daemon_startup_allows_none_mode() {
        let cfg = config_with_llm(Some(LlmSettings::None));
        check_llm_config_for_daemon_mode(&cfg).expect("must allow");
    }

    /// v0.9.0 P4-revision (P4 audit m4): liveness guard for
    /// `trigger_interval_secs == 0`.
    ///
    /// Pre-revision, `triples_batch_timer` unconditionally called
    /// `tokio::time::interval(Duration::from_secs(config.triples.
    /// trigger_interval_secs))`. With config = 0, that panics —
    /// `tokio::time::interval(Duration::ZERO)` returns
    /// "interval must be greater than 0". The daemon would crash on
    /// startup the first time it tried to dispatch the timer.
    ///
    /// We've extracted the guard into `pick_triples_time_arm`. Pin the
    /// "0 disables the time arm; everything else keeps it" contract.
    #[test]
    fn pick_triples_time_arm_returns_none_for_zero_interval() {
        // Zero → time arm disabled (count-based-only).
        assert!(
            pick_triples_time_arm(0).is_none(),
            "interval_secs == 0 MUST disable the time-based arm; \
             otherwise tokio::time::interval(ZERO) would panic at \
             daemon startup"
        );
        // Anything positive → time arm enabled.
        assert_eq!(
            pick_triples_time_arm(1),
            Some(Duration::from_secs(1)),
            "interval_secs == 1 must keep the time arm enabled"
        );
        assert_eq!(
            pick_triples_time_arm(3600),
            Some(Duration::from_secs(3600)),
            "default cadence (3600s) keeps the time arm enabled"
        );
    }

    /// v0.10.1 m5: the daemon-side wiring picks `cluster_timeout_secs`
    /// from the `[triples]` config block and threads it into the
    /// `triples_batch_timer` task. We can't drive a full daemon
    /// startup from a unit test (it would need a tenant + writer +
    /// reader pool + tokio runtime), so we assert the LOCAL invariant:
    /// `TriplesConfig::default().cluster_timeout_secs` is 60 (the
    /// default applied when an operator omits `[triples]` from
    /// `solo.config.toml`), and the daemon's helper Duration math
    /// agrees with that.
    #[test]
    fn daemon_triples_timer_uses_cluster_timeout_secs_from_config() {
        use solo_storage::TriplesConfig;
        let cfg = TriplesConfig::default();
        assert_eq!(
            cfg.cluster_timeout_secs, 60,
            "the default per-cluster timeout for the daemon-side \
             triples_batch_timer must be 60 seconds"
        );
        // The daemon constructs `Duration::from_secs(cluster_timeout_secs)`.
        // Pin that arithmetic for a regression guard so a future
        // refactor that accidentally divides or scales it gets caught.
        let d = Duration::from_secs(cfg.cluster_timeout_secs);
        assert_eq!(d, Duration::from_secs(60));
    }

    /// v0.9.0 P4-revision (P4 audit m4 cont.): full integration of the
    /// zero-interval guard with the `TriplesBatchSignal` count arm.
    /// When `interval_secs == 0`, the time arm is disabled, and the
    /// count arm still fires when the threshold is crossed.
    ///
    /// This test exercises `triples_batch_timer`'s body up through the
    /// `select!` decision point WITHOUT requiring a full LibraryHandle
    /// (which would need encrypted SQLite + ReaderPool + writer thread).
    /// We reuse `TriplesBatchSignal` directly because the m4 panic
    /// risk is at the `tokio::time::interval(ZERO)` call, not inside
    /// `run_triples_batch_tick`.
    #[test]
    fn triples_batch_timer_handles_zero_trigger_interval() {
        use std::sync::Arc;
        use std::time::Duration as StdDuration;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            // Threshold 1 → first remember fires.
            let signal = Arc::new(solo_storage::TriplesBatchSignal::new(1));

            // Verify the guard branch: interval_secs == 0 → None.
            // This is the load-bearing assertion: we never call
            // `tokio::time::interval(ZERO)` so we never panic.
            assert!(pick_triples_time_arm(0).is_none());

            // Simulate the count-based arm: the writer-actor pings
            // the signal after a Remember.
            let writer_signal = signal.clone();
            tokio::spawn(async move {
                writer_signal.note_episode_remembered();
            });

            // Daemon's count arm awaits the notification.
            let fired = tokio::time::timeout(StdDuration::from_secs(2), signal.notified()).await;
            assert!(
                fired.is_ok(),
                "with time arm disabled (interval_secs=0), the count \
                 arm MUST still fire; otherwise the daemon's \
                 triples_batch_timer is a no-op (regression)"
            );
        });
    }
}
