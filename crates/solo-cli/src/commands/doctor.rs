// SPDX-License-Identifier: Apache-2.0

//! `solo doctor` — diagnostics. Prints version, data-dir state, file
//! presence, sizes, mtimes, and embedder identity from `solo.config.toml`.
//!
//! Default mode is **passphrase-free**: it reads only what's available
//! without unlocking the database (config TOML, file metadata, lockfile
//! liveness). Pass `--with-stats` to also open the SQLCipher database
//! (prompts for passphrase) and report row counts + HNSW length +
//! drift.

use anyhow::{Context, Result};
use clap::Args;
use solo_storage::default_data_dir;
use solo_storage::embedder::OllamaEmbedder;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

// Env var matching `solo_storage::embedder::ENV_OLLAMA_BASE_URL`. The
// storage crate doesn't re-export the constant; mirror it here so the
// doctor's display reflects the same precedence the runtime uses.
// If a future change moves the env-var name, both call sites need to
// flip together — `build_embedder_from_env` enforces the runtime side,
// `report_embedder_section` mirrors it for display.
const ENV_OLLAMA_BASE_URL: &str = "SOLO_OLLAMA_BASE_URL";

/// Default Ollama base URL when `SOLO_OLLAMA_BASE_URL` is unset/empty.
/// Mirrors `solo_storage::embedder::ollama`'s default.
const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";

/// Placeholder dim handed to `OllamaEmbedder` when constructing one just
/// to call `probe_dim`. The probe path bypasses the strict dim check —
/// see `OllamaEmbedder::probe_dim`'s docstring — so this value doesn't
/// affect correctness.
const PROBE_PLACEHOLDER_DIM: usize = 1;
const MAX_LIVE_STATUS_BYTES: usize = 256 * 1024;

#[derive(Debug, Args)]
pub struct DoctorArgs {
    #[arg(long, env = "SOLO_DATA_DIR")]
    pub data_dir: Option<PathBuf>,

    /// Open the SQLCipher database (prompts for passphrase) and report
    /// live stats: episode count by tier+status, HNSW length, drift,
    /// pending_index queue depth.
    #[arg(long)]
    pub with_stats: bool,

    /// Prove write -> embed -> index -> semantic recall using an isolated
    /// temporary encrypted database. The real memory library is never opened
    /// or modified.
    #[arg(long)]
    pub round_trip: bool,

    /// Live daemon base URL used when --with-stats detects that the daemon
    /// owns solo.lock.
    #[arg(long, default_value = "http://127.0.0.1:17821")]
    pub daemon_url: String,

    /// Print a per-provider MCP tool-name compatibility report (does
    /// not open the database, does not require a passphrase). Exits 1
    /// if any tool name would be rejected by Anthropic / OpenAI /
    /// Gemini function-calling regexes; otherwise exits 0. Useful in
    /// CI to catch a future tool rename that would silently break
    /// some clients.
    #[arg(long)]
    pub check_mcp_compat: bool,
}

pub async fn run(args: DoctorArgs) -> Result<()> {
    // --check-mcp-compat is a self-contained diagnostic: no data dir,
    // no database, no version banner clutter. Process it before the
    // rest of doctor's normal flow.
    if args.check_mcp_compat {
        let all_pass = print_mcp_compat_report();
        if !all_pass {
            std::process::exit(1);
        }
        return Ok(());
    }

    println!(
        "solo {}",
        solo_core::build_info::version_with_build_metadata()
    );
    let build = solo_core::build_info::get();
    if build.version_with_build != build.version {
        println!("build         : {}", build.version_with_build);
    }
    if let Some(ref_name) = build.build_ref {
        println!("build ref     : {ref_name}");
    }
    if let Some(sha) = build.git_sha_short.as_deref() {
        println!("build commit  : {sha}");
    }
    if let Some(dirty) = build.git_dirty {
        println!("build state   : {dirty}");
    }
    if let Some(number) = build.build_number {
        println!("build number  : {number}");
    }
    println!();

    let data_dir = match args.data_dir {
        Some(p) => p,
        None => default_data_dir()
            .context("could not resolve default data dir; pass --data-dir explicitly")?,
    };
    println!("data dir       : {}", data_dir.display());

    if !data_dir.is_dir() {
        println!("status         : not initialized (run `solo init`)");
        if args.round_trip {
            run_isolated_round_trip().await?;
        }
        return Ok(());
    }

    report_files(&data_dir);
    report_lockfile(&data_dir);
    report_config(&data_dir, args.with_stats).await;

    if args.with_stats {
        println!();
        if live_lock_pid(&data_dir).is_some() {
            report_live_daemon_stats(&data_dir, &args.daemon_url).await?;
        } else {
            report_stats(&data_dir).await?;
        }
    } else {
        println!();
        println!("(pass --with-stats to open the database for live counts)");
    }

    if args.round_trip {
        run_isolated_round_trip().await?;
    }

    Ok(())
}

fn live_lock_pid(data_dir: &Path) -> Option<u32> {
    let body = fs::read_to_string(data_dir.join("solo.lock")).ok()?;
    let pid = body.trim().parse::<u32>().ok()?;
    is_pid_alive(pid).then_some(pid)
}

async fn report_live_daemon_stats(data_dir: &Path, daemon_url: &str) -> Result<()> {
    let config = solo_storage::SoloConfig::read(&data_dir.join("solo.config.toml"))
        .context("read config for live daemon diagnostics")?;
    let base = reqwest::Url::parse(&format!("{}/", daemon_url.trim_end_matches('/')))
        .context("parse live daemon URL")?;
    anyhow::ensure!(
        matches!(base.scheme(), "http" | "https"),
        "doctor live-daemon diagnostics require an http:// or https:// URL"
    );
    anyhow::ensure!(
        base.username().is_empty()
            && base.password().is_none()
            && base.query().is_none()
            && base.fragment().is_none(),
        "doctor live-daemon diagnostics reject URLs containing credentials, a query, or a fragment"
    );
    let host = base
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    let loopback_ip = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    anyhow::ensure!(
        host == "localhost" || loopback_ip,
        "doctor live-daemon diagnostics only send the configured bearer token to a loopback URL"
    );
    let url = base.join("v1/status").context("build live status URL")?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        // Never forward the daemon bearer token through an HTTP redirect.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build doctor live-daemon client")?;
    let mut request = client.get(url.clone());
    if let Some(solo_storage::AuthSettings::Bearer { token }) = config.auth.as_ref() {
        request = request.bearer_auth(token);
    }
    let mut response = request
        .send()
        .await
        .with_context(|| format!("query live Solo daemon at {url}"))?;
    if !response.status().is_success() {
        anyhow::bail!(
            "live Solo daemon returned HTTP {} for {}; use the daemon's authenticated URL or run doctor without --with-stats",
            response.status(),
            url
        );
    }
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= MAX_LIVE_STATUS_BYTES as u64),
        "live Solo daemon status exceeded {MAX_LIVE_STATUS_BYTES} bytes"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .context("read live Solo daemon status")?
    {
        anyhow::ensure!(
            bytes.len().saturating_add(chunk.len()) <= MAX_LIVE_STATUS_BYTES,
            "live Solo daemon status exceeded {MAX_LIVE_STATUS_BYTES} bytes"
        );
        bytes.extend_from_slice(&chunk);
    }
    let status: serde_json::Value =
        serde_json::from_slice(&bytes).context("decode live Solo daemon status")?;
    let coverage = &status["steward"]["coverage"];
    println!("live daemon     : {url} (database lock is healthy)");
    println!(
        "library         : {}",
        status["library"]["name"]
            .as_str()
            .unwrap_or("Community Memory Library")
    );
    println!(
        "active episodes : {}",
        coverage["active_episodes"].as_u64().unwrap_or(0)
    );
    println!(
        "clusters        : {}",
        coverage["clusters"].as_u64().unwrap_or(0)
    );
    println!(
        "abstractions    : {}",
        coverage["abstractions"].as_u64().unwrap_or(0)
    );
    println!(
        "pending clusters: {}",
        coverage["pending_clusters"].as_u64().unwrap_or(0)
    );
    println!(
        "triples         : {}",
        coverage["triples"].as_u64().unwrap_or(0)
    );
    println!(
        "entities        : {}",
        coverage["entities"].as_u64().unwrap_or(0)
    );
    println!(
        "relationships   : {}",
        coverage["relationships"].as_u64().unwrap_or(0)
    );
    if let Some(name) = status["embedder"]["name"].as_str() {
        println!("embedder        : {name}");
    }
    Ok(())
}

#[cfg(feature = "bundled-embedder")]
async fn run_isolated_round_trip() -> Result<()> {
    use solo_core::{Confidence, Embedder, EncodingContext, Episode, MemoryId, Tier};
    use solo_storage::{
        BundledEmbedder, HnswParams, InitParams, KeyMaterial, MemoryLibrary, MemoryLibraryParams,
    };
    use std::sync::Arc;
    use zeroize::Zeroizing;

    println!();
    println!("isolated round-trip:");
    let tmp = tempfile::tempdir().context("create isolated doctor data dir")?;
    let passphrase = Zeroizing::new("solo-doctor-temporary-round-trip".to_string());
    let embedder = Arc::new(BundledEmbedder::new());
    let embedder_config = solo_storage::EmbedderConfig {
        name: solo_storage::BUNDLED_EMBEDDER_NAME.to_string(),
        version: solo_storage::BUNDLED_EMBEDDER_VERSION.to_string(),
        dim: solo_storage::BUNDLED_EMBEDDER_DIM as u32,
        dtype: "f32".to_string(),
    };
    let initialized = solo_storage::init(InitParams {
        data_dir: tmp.path().to_path_buf(),
        passphrase: passphrase.clone(),
        force: false,
        embedder: embedder_config,
    })
    .context("initialize isolated doctor database")?;
    let config = solo_storage::SoloConfig::read(&initialized.config_path)?;
    let key = KeyMaterial::derive(&passphrase, &config.salt_bytes()?)?;
    let library = MemoryLibrary::open(MemoryLibraryParams {
        data_dir: tmp.path().to_path_buf(),
        key,
        embedder: embedder.clone(),
        hnsw_params: HnswParams::default(),
        steward: None,
        runtime_handle: Some(tokio::runtime::Handle::current()),
        steward_factory: None,
        triples_batch_signal: None,
    })?;
    let handle = library.handle().await?;
    let content =
        "Solo's diagnostic validator stores this memory only in an isolated temporary library.";
    let embedding = embedder
        .embed(content)
        .await
        .context("embed temporary memory")?;
    let memory_id = handle
        .write()
        .remember(
            Episode {
                memory_id: MemoryId::new(),
                ts_ms: chrono::Utc::now().timestamp_millis(),
                source_type: "doctor_round_trip".to_string(),
                source_id: None,
                content: content.to_string(),
                encoding_context: EncodingContext::default(),
                provenance: None,
                confidence: Confidence::new(1.0).expect("valid confidence"),
                strength: 1.0,
                salience: 1.0,
                tier: Tier::Hot,
            },
            embedding,
        )
        .await
        .context("write temporary memory")?;
    let recalled = solo_query::run_recall(
        &handle,
        None,
        "Where did the validator put its disposable test record?",
        3,
    )
    .await
    .context("recall temporary memory")?;
    anyhow::ensure!(
        recalled
            .hits
            .iter()
            .any(|hit| hit.memory_id == memory_id.to_string()),
        "temporary memory was not recalled"
    );
    println!("  PASS write -> bundled MiniLM -> HNSW -> semantic recall");
    println!("  data policy    : temporary database deleted after this check");
    println!("  real library   : not opened or modified");
    drop(handle);
    library.shutdown_with_snapshot(false).await;
    Ok(())
}

#[cfg(not(feature = "bundled-embedder"))]
async fn run_isolated_round_trip() -> Result<()> {
    anyhow::bail!(
        "this Solo build does not include the bundled embedder required by doctor --round-trip"
    )
}

// ---------------------------------------------------------------------------
// MCP cross-provider compatibility report (`--check-mcp-compat`)
// ---------------------------------------------------------------------------

/// Print the per-provider PASS/FAIL table for every MCP tool name
/// Solo registers. Returns `true` iff every cell is PASS.
///
/// Format: ASCII only (no box-drawing characters) for Windows
/// terminal portability. Provider regexes are kept in lock-step with
/// the `tool_names_match_cross_provider_regex` test in
/// `solo-api::mcp` — if you change one, change both.
fn print_mcp_compat_report() -> bool {
    println!("MCP cross-provider compatibility report");
    println!();
    println!("Tool                       Anthropic   OpenAI   Gemini");
    println!("-------------------------  ---------   ------   ------");

    let mut all_pass = true;
    for name in solo_api::tool_names() {
        let a = if name_passes_anthropic(name) {
            "PASS"
        } else {
            "FAIL"
        };
        let o = if name_passes_openai(name) {
            "PASS"
        } else {
            "FAIL"
        };
        let g = if name_passes_gemini(name) {
            "PASS"
        } else {
            "FAIL"
        };
        if a == "FAIL" || o == "FAIL" || g == "FAIL" {
            all_pass = false;
        }
        // Tool column width 25 (longest current name is
        // `memory_inspect_cluster` = 22 chars).
        println!("{name:<25}  {a:<9}   {o:<6}   {g:<6}");
    }

    println!();
    println!("Tested MCP clients: Claude Desktop, Cursor, Claude Code");
    // `rmcp` 0.1.x implements MCP spec version 2024-11-05 (the
    // version the existing `mcp_smoke.rs` tests handshake with). Keep
    // these strings in sync with the `PROTOCOL_VERSION` constant in
    // `crates/solo-cli/tests/mcp_smoke.rs` if rmcp ever bumps.
    println!("MCP spec version : 2024-11-05 (via rmcp 0.1)");
    if !all_pass {
        println!();
        println!("RESULT: one or more tools FAIL — fix tool names before release");
    }
    all_pass
}

/// Anthropic API name regex: `^[a-zA-Z0-9_-]{1,64}$`. Mirrors
/// `solo-api::mcp::dispatch_tests::passes_anthropic`.
fn name_passes_anthropic(name: &str) -> bool {
    let len = name.len();
    if !(1..=64).contains(&len) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// OpenAI function-calling name regex: `^[a-zA-Z_][a-zA-Z0-9_-]*$`,
/// length ≤ 64.
fn name_passes_openai(name: &str) -> bool {
    let len = name.len();
    if !(1..=64).contains(&len) {
        return false;
    }
    let mut chars = name.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Gemini function-calling name regex (conservative reading):
/// `^[a-zA-Z_][a-zA-Z0-9_]*$`, length ≤ 63. Strictest of the three.
fn name_passes_gemini(name: &str) -> bool {
    let len = name.len();
    if !(1..=63).contains(&len) {
        return false;
    }
    let mut chars = name.chars();
    let first = match chars.next() {
        Some(c) => c,
        None => return false,
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn report_files(data_dir: &Path) {
    let names: &[(&str, &str)] = &[
        ("solo.db", "Community Memory Library"),
        ("solo.db-wal", "Memory Library WAL"),
        ("solo.db-shm", "Memory Library shared-memory"),
        ("solo.config.toml", "config"),
        ("hnsw_episodes.hnsw.data", "hnsw live data"),
        ("hnsw_episodes.hnsw.graph", "hnsw live graph"),
        ("hnsw_episodes_bak.hnsw.data", "hnsw backup data"),
        ("hnsw_episodes_bak.hnsw.graph", "hnsw backup graph"),
    ];
    println!();
    println!("files:");
    for (name, label) in names {
        let p = data_dir.join(name);
        match fs::metadata(&p) {
            Ok(meta) => {
                let size = meta.len();
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| {
                        chrono::DateTime::<chrono::Utc>::from_timestamp(
                            d.as_secs() as i64,
                            d.subsec_nanos(),
                        )
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_else(|| "?".into())
                    })
                    .unwrap_or_else(|| "?".into());
                println!(
                    "  ✓ {:<32} {:>14}  {:<25}  {label}",
                    name,
                    fmt_size(size),
                    mtime
                );
            }
            Err(_) => {
                println!(
                    "  ✗ {name:<32}                                              {label} (missing)"
                );
            }
        }
    }

    if let Some(summary) = previous_layout_summary(data_dir) {
        println!();
        println!("{summary}");
    }
}

fn fmt_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn previous_layout_summary(data_dir: &Path) -> Option<String> {
    let previous_dir = data_dir.join("tenants");
    let previous_default = previous_dir.join("default.db");
    let has_previous_default = previous_default.is_file();
    let has_previous_index = ["", "-wal", "-shm"]
        .iter()
        .any(|suffix| data_dir.join(format!("tenants_index.db{suffix}")).exists());
    let mut extra_databases = Vec::new();

    if previous_dir.is_dir() {
        match fs::read_dir(&previous_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file()
                        && path.extension().is_some_and(|ext| ext == "db")
                        && path.file_name().is_some_and(|name| name != "default.db")
                    {
                        extra_databases.push(path);
                    }
                }
            }
            Err(error) => {
                return Some(format!(
                    "previous layout : present but unreadable at {}: {error}",
                    previous_dir.display()
                ));
            }
        }
    }

    if !has_previous_default && !has_previous_index && !previous_dir.exists() {
        return None;
    }

    if !extra_databases.is_empty() {
        let paths = extra_databases
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Some(format!(
            "previous layout : extra database files found ({paths}); Community startup will stop so these can be exported first"
        ));
    }

    if has_previous_default {
        return Some(format!(
            "previous layout : default library found at {}; Community startup will promote it to solo.db",
            previous_default.display()
        ));
    }

    Some(format!(
        "previous layout : remnants found but default library is missing at {}; Community startup will stop until this is restored or cleaned up",
        previous_default.display()
    ))
}

#[cfg(test)]
mod previous_layout_summary_tests {
    use super::*;

    #[test]
    fn absent_when_only_community_layout_exists() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("solo.db"), b"db").unwrap();

        assert!(previous_layout_summary(temp.path()).is_none());
    }

    #[test]
    fn reports_promotable_previous_default_library() {
        let temp = tempfile::tempdir().unwrap();
        let previous_dir = temp.path().join("tenants");
        fs::create_dir_all(&previous_dir).unwrap();
        fs::write(previous_dir.join("default.db"), b"db").unwrap();
        fs::write(temp.path().join("tenants_index.db"), b"index").unwrap();

        let summary = previous_layout_summary(temp.path()).unwrap();

        assert!(summary.contains("default library found"), "{summary}");
        assert!(summary.contains("promote it to solo.db"), "{summary}");
    }

    #[test]
    fn reports_extra_previous_database_as_startup_stop() {
        let temp = tempfile::tempdir().unwrap();
        let previous_dir = temp.path().join("tenants");
        fs::create_dir_all(&previous_dir).unwrap();
        fs::write(previous_dir.join("default.db"), b"default").unwrap();
        fs::write(previous_dir.join("work.db"), b"work").unwrap();

        let summary = previous_layout_summary(temp.path()).unwrap();

        assert!(summary.contains("extra database files found"), "{summary}");
        assert!(summary.contains("startup will stop"), "{summary}");
    }
}

fn report_lockfile(data_dir: &Path) {
    let p = data_dir.join("solo.lock");
    println!();
    if p.is_file() {
        let body = fs::read_to_string(&p).unwrap_or_default();
        let pid_str = body.trim();
        let pid = pid_str.parse::<u32>().ok();
        let alive = pid.map(is_pid_alive).unwrap_or(false);
        if alive {
            println!("lockfile       : held by pid {pid_str} (alive)");
        } else if pid.is_some() {
            println!(
                "lockfile       : stale (pid {pid_str} not alive — would be recovered on next acquire)"
            );
        } else {
            println!("lockfile       : present but body unparseable: {pid_str:?}");
        }
    } else {
        println!("lockfile       : free");
    }
}

fn is_pid_alive(pid: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};
    let sys =
        System::new_with_specifics(RefreshKind::new().with_processes(ProcessRefreshKind::new()));
    sys.process(Pid::from_u32(pid)).is_some()
}

/// Read `solo.config.toml` and emit the `config:` + `embedder:`
/// subsections.
///
/// The `embedder:` subsection classifies the persisted `embedder.name`
/// into a Backend label (Ollama / Stub / Unknown) and surfaces
/// backend-specific fields:
///
///   - Ollama: Backend, Base URL, Model, Dim, optional Sample latency
///   - Stub + Unknown: Backend + Dim only
///
/// (BGE-M3 was supported in v0.5.x; removed in v0.6.0. A persisted
/// `BAAI/bge-m3` identity now classifies as `Unknown` with the raw
/// name surfaced for the operator.)
///
/// `Sample latency` only runs when `with_stats == true` — it issues one
/// HTTP round-trip to Ollama via `OllamaEmbedder::probe_dim`. A failed
/// probe degrades to `(unreachable: <err>)` rather than propagating;
/// `solo doctor` is health diagnostics, not a startup check.
async fn report_config(data_dir: &Path, with_stats: bool) {
    let cfg_path = data_dir.join("solo.config.toml");
    println!();
    if !cfg_path.is_file() {
        println!("config         : not found (run `solo init`)");
        return;
    }
    let cfg = match solo_storage::SoloConfig::read(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            println!("config         : present but unreadable: {e}");
            return;
        }
    };

    println!("config:");
    println!("  schema_version : {}", cfg.schema_version);
    println!(
        "  salt_hex       : {}…  (16-byte Argon2 salt)",
        &cfg.salt_hex[..8.min(cfg.salt_hex.len())]
    );
    println!("  embedder.name  : {}", cfg.embedder.name);
    println!("  embedder.version: {}", cfg.embedder.version);
    println!("  embedder.dim   : {}", cfg.embedder.dim);
    println!("  embedder.dtype : {}", cfg.embedder.dtype);

    // Embedder subsection — backend classification + optional probe.
    // Probe runs only when --with-stats is set so the default `solo
    // doctor` invocation stays passphrase-free AND network-free.
    let latency = if with_stats {
        probe_ollama_latency(&cfg.embedder.name).await
    } else {
        None
    };
    print!("{}", format_embedder_section(&cfg.embedder, latency));
}

/// Backend classification derived from `embedder.name`.
///
/// The classifier inspects only the persisted `name` field — it does
/// no I/O and is the unit-of-testing for backend display.
///
/// `BAAI/bge-m3` (the v0.5.x BGE-M3 identity) now classifies as
/// `Unknown` — BGE-M3 was removed in v0.6.0 and operators upgrading
/// with a stale config will see the raw name surfaced so they know to
/// run `solo reembed` against an Ollama backend.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EmbedderBackend {
    Ollama {
        model: String,
    },
    Stub,
    /// Captures the raw `name` so the report can show what the operator
    /// actually has on disk when it doesn't match any known backend.
    Unknown(String),
}

/// Classify a persisted `embedder.name` into a [`EmbedderBackend`].
///
/// Rules:
///
/// - Starts with `ollama:` → `Ollama { model: <suffix> }`
/// - Equals `stub` → `Stub`
/// - Anything else → `Unknown(<raw>)` (including a stale `BAAI/bge-m3`
///   identity from a v0.5.x install upgraded to v0.6.0+)
///
/// `starts_with("ollama:")` is unicode-safe: `&str::starts_with(&str)`
/// matches by byte sequence, and `"ollama:"` is pure ASCII so the
/// byte-prefix check coincides with a code-point-prefix check.
fn classify_backend(name: &str) -> EmbedderBackend {
    if let Some(model) = name.strip_prefix("ollama:") {
        EmbedderBackend::Ollama {
            model: model.to_string(),
        }
    } else if name == "stub" {
        EmbedderBackend::Stub
    } else {
        EmbedderBackend::Unknown(name.to_string())
    }
}

/// Resolve the Ollama base URL from env using the same precedence as
/// `solo_storage::embedder::build_ollama_from_env`: `SOLO_OLLAMA_BASE_URL`
/// (treating empty string as unset), defaulting to
/// `http://localhost:11434`.
fn resolve_ollama_base_url() -> String {
    std::env::var(ENV_OLLAMA_BASE_URL)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string())
}

/// Pure formatter for the `embedder:` subsection. Returns the formatted
/// block (trailing newline included) so the caller can `print!` it.
///
/// Returning a `String` rather than printing directly makes the section
/// unit-testable without stdout capture — see the `format_embedder_section_tests`
/// module for the per-backend layout assertions.
///
/// `latency` semantics:
///
/// - `None` → no `Sample latency:` line emitted (non-Ollama backends, or
///   `--with-stats` not set for Ollama).
/// - `Some(Ok(ms))` → `Sample latency: <ms>ms` line.
/// - `Some(Err(msg))` → `Sample latency: (unreachable: <msg>)` line.
fn format_embedder_section(
    cfg: &solo_storage::EmbedderConfig,
    latency: Option<std::result::Result<u64, String>>,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out);
    let _ = writeln!(out, "embedder:");
    let backend = classify_backend(&cfg.name);
    match &backend {
        EmbedderBackend::Ollama { model } => {
            let _ = writeln!(out, "  Backend        : Ollama");
            let _ = writeln!(out, "  Base URL       : {}", resolve_ollama_base_url());
            let _ = writeln!(out, "  Model          : {model}");
            let _ = writeln!(out, "  Dim            : {}", cfg.dim);
            match latency {
                Some(Ok(ms)) => {
                    let _ = writeln!(out, "  Sample latency : {ms}ms");
                }
                Some(Err(msg)) => {
                    let _ = writeln!(out, "  Sample latency : (unreachable: {msg})");
                }
                None => {
                    let _ = writeln!(out, "  Sample latency : (pass --with-stats to probe)");
                }
            }
        }
        EmbedderBackend::Stub => {
            let _ = writeln!(out, "  Backend        : Stub");
            let _ = writeln!(out, "  Dim            : {}", cfg.dim);
        }
        EmbedderBackend::Unknown(raw) => {
            let _ = writeln!(out, "  Backend        : Unknown (raw name: {raw})");
            let _ = writeln!(out, "  Dim            : {}", cfg.dim);
        }
    }
    out
}

/// Issue one `OllamaEmbedder::probe_dim` call and time the round trip.
///
/// Returns `None` if the persisted `name` isn't an Ollama identity
/// (so the doctor knows to skip emitting a latency line). Returns
/// `Some(Ok(ms))` on success and `Some(Err(human-readable))` on
/// failure — the doctor displays the error inline; it does NOT
/// propagate. `OllamaEmbedder::new`'s reqwest builder failure is
/// folded into the same `Err` channel.
///
/// The probe uses `RetryConfig::none()`-equivalent behaviour by
/// reusing `OllamaEmbedder`'s default retry policy. A future
/// `--no-retry` flag could short-circuit this; for now the 60-second
/// default timeout caps doctor's worst-case delay even when Ollama is
/// slow.
async fn probe_ollama_latency(name: &str) -> Option<std::result::Result<u64, String>> {
    let backend = classify_backend(name);
    let model = match backend {
        EmbedderBackend::Ollama { model } => model,
        _ => return None,
    };
    let base_url = resolve_ollama_base_url();
    let embedder = match OllamaEmbedder::new(&base_url, &model, PROBE_PLACEHOLDER_DIM) {
        Ok(e) => e,
        Err(e) => return Some(Err(format!("{e}"))),
    };
    let start = Instant::now();
    match embedder.probe_dim().await {
        Ok(_) => {
            let elapsed_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
            Some(Ok(elapsed_ms))
        }
        Err(e) => Some(Err(format!("{e}"))),
    }
}

async fn report_stats(data_dir: &Path) -> Result<()> {
    use crate::commands::common::prepare_oneshot;

    println!("opening database (prompting for passphrase)...");
    let ctx = prepare_oneshot(Some(data_dir.to_path_buf())).await?;

    let counts: Vec<(String, String, i64)> = ctx
        .read_pool()
        .interact(|conn| {
            let mut stmt =
                conn.prepare("SELECT tier, status, COUNT(*) FROM episodes GROUP BY tier, status")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .context("read episodes count by tier+status")?;
    let pending: i64 = ctx
        .read_pool()
        .interact(|conn| conn.query_row("SELECT COUNT(*) FROM pending_index", [], |r| r.get(0)))
        .await
        .context("read pending_index count")?;

    println!();
    println!("library         : Community Memory Library");
    println!("episode counts:");
    if counts.is_empty() {
        println!("  (no episodes yet)");
    } else {
        for (tier, status, n) in counts {
            println!("  tier={tier:<6} status={status:<10} {n}");
        }
    }
    println!();
    let hnsw = ctx.library_handle.hnsw();
    println!(
        "hnsw            : {} vectors (dim {})",
        hnsw.len(),
        hnsw.dim()
    );
    println!("pending_index   : {pending} rows");
    let drift = ctx.library_handle.drift();
    if !drift.is_clean() {
        println!(
            "drift           : SQL hot={} vs hnsw_len={} (diff={})",
            drift.hot_episodes, drift.index_len, drift.diff
        );
    } else {
        println!("drift           : clean");
    }
    if ctx.library_handle.used_bak_snapshot() {
        println!("snapshot        : LOADED FROM .bak (live snapshot was unusable)");
    } else if ctx.library_handle.started_fresh() {
        let rebuild = ctx.library_handle.rebuild();
        if rebuild.rows_added > 0 || rebuild.rows_skipped > 0 {
            if rebuild.rows_skipped > 0 {
                println!(
                    "snapshot        : none (rebuilt {} from `embeddings`, {} rows skipped — see logs)",
                    rebuild.rows_added, rebuild.rows_skipped
                );
            } else {
                println!(
                    "snapshot        : none (rebuilt {} vectors from `embeddings`)",
                    rebuild.rows_added
                );
            }
        } else {
            println!("snapshot        : none (started with empty index)");
        }
    } else {
        println!("snapshot        : live");
    }

    // Merge-candidate signal: count of pre-existing cluster pairs that
    // the existing-vs-existing merge pass would coalesce on the next
    // `consolidate --force-merge` (or daemon `--force-merge-on-timer`)
    // cycle. Surfaces drift accumulation: the daemon's regular timer
    // hard-codes `force_merge: false` and the empty-CANDIDATES early
    // return prevents merges from firing on idle cycles, so the count
    // can grow silently between manual force-merge runs.
    //
    // The thresholds MUST resolve from the same TOML+env pathway the
    // daemon and `solo consolidate` use
    // (`StewardConfig::from_settings_then_env`), otherwise the doctor's
    // count would diverge from what `--force-merge` would actually
    // apply — see `solo_storage::merge_candidates`'s "Sync requirement"
    // docstring.
    //
    // v0.11.1: read `[steward]` from the open tenant's `SoloConfig`
    // (already parsed by `prepare_oneshot`) and layer env vars on top,
    // matching the daemon/CLI resolution order.
    let cfg = ctx.config();
    let steward_cfg = solo_steward::StewardConfig::from_settings_then_env(
        cfg.steward.cluster_min_size,
        cfg.steward.cluster_cosine_threshold,
    )
    .context("parse [steward] config + SOLO_CLUSTER_* env vars for doctor")?;
    let expected_dim = ctx.library_handle.hnsw().dim();
    let merge_stats = ctx
        .read_pool()
        .interact(move |conn| {
            Ok(solo_storage::count_existing_merge_candidates(
                conn,
                expected_dim,
                &steward_cfg,
            ))
        })
        .await
        .context("interact: merge-candidate count")?
        .context("compute merge-candidate count")?;
    if merge_stats.clusters_examined < 2 {
        println!(
            "merge candidates: 0 ({} cluster{} — need ≥2 to evaluate)",
            merge_stats.clusters_examined,
            if merge_stats.clusters_examined == 1 {
                ""
            } else {
                "s"
            },
        );
    } else if merge_stats.merge_ops == 0 {
        println!(
            "merge candidates: 0 ({} clusters examined, all distinct above threshold)",
            merge_stats.clusters_examined,
        );
    } else {
        println!(
            "merge candidates: {} ops, {} clusters would absorb (of {} examined)",
            merge_stats.merge_ops, merge_stats.clusters_would_absorb, merge_stats.clusters_examined,
        );
        println!("                  → run `solo consolidate --force-merge` to apply");
    }

    ctx.shutdown().await.context("doctor shutdown")?;
    Ok(())
}

#[cfg(test)]
mod check_mcp_compat_tests {
    //! Per-provider regex helpers behave consistently with the
    //! cross-provider test in `solo-api`. If a future tool-name
    //! change breaks one side without breaking the other, these
    //! tests catch the divergence locally before doctor prints a
    //! misleading PASS.
    use super::*;

    #[test]
    fn all_current_tools_pass_every_provider() {
        for name in solo_api::tool_names() {
            assert!(
                name_passes_anthropic(name),
                "tool {name} fails Anthropic in doctor's check"
            );
            assert!(
                name_passes_openai(name),
                "tool {name} fails OpenAI in doctor's check"
            );
            assert!(
                name_passes_gemini(name),
                "tool {name} fails Gemini in doctor's check"
            );
        }
    }

    #[test]
    fn anthropic_accepts_hyphen_openai_too_gemini_rejects() {
        // Gemini's conservative reading (no hyphen) is the
        // discriminator. Use a hypothetical "memory-recall" name to
        // assert the table would surface a FAIL on Gemini but PASS
        // on the other two. Guards against future renames that look
        // fine on Anthropic + OpenAI but quietly break Gemini.
        let bad = "memory-recall";
        assert!(name_passes_anthropic(bad));
        assert!(name_passes_openai(bad));
        assert!(
            !name_passes_gemini(bad),
            "Gemini's conservative regex must reject hyphens"
        );
    }

    #[test]
    fn dot_name_fails_all_providers() {
        // v0.4.1's regression: `memory.X` names. None of the three
        // providers should accept this shape.
        let bad = "memory.recall";
        assert!(!name_passes_anthropic(bad));
        assert!(!name_passes_openai(bad));
        assert!(!name_passes_gemini(bad));
    }

    #[test]
    fn empty_and_overlong_names_fail() {
        assert!(!name_passes_anthropic(""));
        assert!(!name_passes_openai(""));
        assert!(!name_passes_gemini(""));
        let too_long: String = "a".repeat(65);
        assert!(!name_passes_anthropic(&too_long));
        assert!(!name_passes_openai(&too_long));
        // 65 > 63 too, Gemini also rejects.
        assert!(!name_passes_gemini(&too_long));
    }
}

#[cfg(test)]
mod format_embedder_section_tests {
    //! Pure-string formatter tests for the v0.5.1 6E `embedder:` block.
    //!
    //! These don't touch the filesystem or network — they exercise
    //! `classify_backend` + `format_embedder_section` directly. Latency
    //! integration is covered by `probe_embedder_latency_tests` below.
    use super::*;
    use solo_storage::EmbedderConfig;

    fn cfg(name: &str, dim: u32) -> EmbedderConfig {
        EmbedderConfig {
            name: name.into(),
            version: "v1".into(),
            dim,
            dtype: "f32".into(),
        }
    }

    #[test]
    fn classify_recognises_ollama_prefix() {
        assert_eq!(
            classify_backend("ollama:nomic-embed-text"),
            EmbedderBackend::Ollama {
                model: "nomic-embed-text".into()
            }
        );
        // Multi-segment model names (e.g. registry/model:tag style) are
        // preserved verbatim past the `ollama:` prefix.
        assert_eq!(
            classify_backend("ollama:registry/mxbai-embed-large"),
            EmbedderBackend::Ollama {
                model: "registry/mxbai-embed-large".into()
            }
        );
    }

    #[test]
    fn classify_recognises_stub_exactly() {
        assert_eq!(classify_backend("stub"), EmbedderBackend::Stub);
        // Case-sensitive — guards against a future regression where
        // someone normalises the case and accidentally bypasses the
        // exact match.
        assert_eq!(
            classify_backend("STUB"),
            EmbedderBackend::Unknown("STUB".into())
        );
    }

    #[test]
    fn classify_falls_back_to_unknown_for_legacy_bge_m3_identity() {
        // BGE-M3 was removed in v0.6.0; a stale `BAAI/bge-m3` identity
        // on disk now classifies as Unknown so the operator sees the
        // raw name in `solo doctor` and knows the config is from a
        // pre-v0.6.0 install.
        assert_eq!(
            classify_backend("BAAI/bge-m3"),
            EmbedderBackend::Unknown("BAAI/bge-m3".into())
        );
    }

    #[test]
    fn classify_falls_back_to_unknown_with_raw_name() {
        assert_eq!(
            classify_backend("something-future"),
            EmbedderBackend::Unknown("something-future".into())
        );
    }

    #[test]
    fn shows_ollama_backend_with_latency_when_probe_succeeds() {
        let out = format_embedder_section(&cfg("ollama:nomic-embed-text", 768), Some(Ok(42)));
        assert!(out.contains("Backend        : Ollama"), "got:\n{out}");
        assert!(out.contains("Base URL       : http://"), "got:\n{out}");
        assert!(
            out.contains("Model          : nomic-embed-text"),
            "got:\n{out}"
        );
        assert!(out.contains("Dim            : 768"), "got:\n{out}");
        assert!(out.contains("Sample latency : 42ms"), "got:\n{out}");
    }

    #[test]
    fn shows_ollama_backend_with_unreachable_message_on_probe_err() {
        let out = format_embedder_section(
            &cfg("ollama:nomic-embed-text", 768),
            Some(Err("connection refused".to_string())),
        );
        assert!(out.contains("Backend        : Ollama"));
        assert!(
            out.contains("Sample latency : (unreachable: connection refused)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn shows_ollama_backend_with_hint_when_with_stats_off() {
        let out = format_embedder_section(&cfg("ollama:nomic-embed-text", 768), None);
        assert!(out.contains("Backend        : Ollama"));
        // No live numeric latency, just the hint pointing at the flag.
        assert!(
            out.contains("Sample latency : (pass --with-stats to probe)"),
            "got:\n{out}"
        );
        assert!(!out.contains(": 0ms"), "should not emit a fake latency");
    }

    #[test]
    fn shows_legacy_bge_m3_identity_as_unknown_with_raw_name() {
        // After v0.6.0 P9 hard-removes BGE-M3, the legacy `BAAI/bge-m3`
        // identity classifies as Unknown — operator sees the raw name
        // surfaced and a hint that this is a stale config from before
        // the migration.
        let out = format_embedder_section(&cfg("BAAI/bge-m3", 1024), Some(Ok(99)));
        assert!(
            out.contains("Backend        : Unknown (raw name: BAAI/bge-m3)"),
            "got:\n{out}"
        );
        assert!(out.contains("Dim            : 1024"), "got:\n{out}");
        // Latency line must NOT appear even if a caller (incorrectly)
        // passes a probe result — backend gate is in the formatter.
        assert!(
            !out.contains("Sample latency"),
            "Unknown backend must not show latency: got:\n{out}"
        );
        assert!(!out.contains("Base URL"));
        assert!(!out.contains("Model "));
    }

    #[test]
    fn shows_stub_backend_no_latency() {
        let out = format_embedder_section(&cfg("stub", 32), Some(Ok(99)));
        assert!(out.contains("Backend        : Stub"), "got:\n{out}");
        assert!(out.contains("Dim            : 32"), "got:\n{out}");
        assert!(
            !out.contains("Sample latency"),
            "Stub must not show latency: got:\n{out}"
        );
    }

    #[test]
    fn shows_unknown_backend_with_raw_name_no_latency() {
        let out = format_embedder_section(&cfg("future-model-xyz", 512), None);
        assert!(
            out.contains("Backend        : Unknown (raw name: future-model-xyz)"),
            "got:\n{out}"
        );
        assert!(out.contains("Dim            : 512"));
        assert!(!out.contains("Sample latency"));
    }
}

#[cfg(test)]
mod probe_embedder_latency_tests {
    //! Live-probe path tests for the v0.5.1 6E latency probe.
    //!
    //! Env vars are process-global mutable state. The cargo test runner
    //! parallelises tests within a binary, so we serialise these cases
    //! through a module-level `Mutex`. Mirrors the pattern in
    //! `solo_storage::embedder::probe_config_from_env_tests`.
    use super::*;
    use std::sync::Mutex;
    use wiremock::matchers::{method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: caller holds ENV_LOCK.
            unsafe { std::env::remove_var(ENV_OLLAMA_BASE_URL) };
        }
    }

    fn fresh_env() -> EnvGuard {
        // SAFETY: caller holds ENV_LOCK.
        unsafe { std::env::remove_var(ENV_OLLAMA_BASE_URL) };
        EnvGuard
    }

    fn fixture(dim: usize, seed: u32) -> Vec<f32> {
        (0..dim)
            .map(|i| ((seed.wrapping_add(i as u32)) as f32) * 1e-3)
            .collect()
    }

    #[tokio::test]
    async fn returns_none_for_non_ollama_name() {
        // No env mutation needed — the probe short-circuits on the
        // backend classification before reading env.
        assert!(probe_ollama_latency("stub").await.is_none());
        assert!(probe_ollama_latency("BAAI/bge-m3").await.is_none());
        assert!(probe_ollama_latency("future-model").await.is_none());
    }

    #[tokio::test]
    async fn returns_some_ok_when_mock_ollama_responds() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wm_path("/api/embeddings"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "embedding": fixture(768, 1)
            })))
            .expect(1)
            .mount(&server)
            .await;

        // SAFETY: ENV_LOCK held.
        unsafe { std::env::set_var(ENV_OLLAMA_BASE_URL, server.uri()) };

        let result = probe_ollama_latency("ollama:nomic-embed-text")
            .await
            .expect("Ollama-named identity must probe");
        let ms = result.expect("mock responds 200");
        // Sanity check: wiremock loopback is single-digit ms typically;
        // even an under-load CI host should clear comfortably in < 30s.
        assert!(ms < 30_000, "latency should be sub-30s: got {ms}");
    }

    #[tokio::test]
    async fn returns_some_err_when_ollama_unreachable_softly() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();

        // Point at a port nothing listens on. Loopback refusal is fast
        // on every supported platform — this test is shape-checking the
        // soft-failure path, not measuring real timeout.
        // SAFETY: ENV_LOCK held.
        unsafe { std::env::set_var(ENV_OLLAMA_BASE_URL, "http://127.0.0.1:1") };

        let result = probe_ollama_latency("ollama:nomic-embed-text")
            .await
            .expect("Ollama-named identity must attempt probe");
        let err_msg = result.expect_err("port 1 must refuse");
        // Surface should mention `ollama` so the operator's eye lands
        // on the right runbook entry; exact wording flows from
        // `OllamaEmbedder::embed_one`'s reqwest error formatting.
        assert!(
            err_msg.to_ascii_lowercase().contains("ollama"),
            "soft-failure error should be ollama-flavoured: got {err_msg}"
        );
    }

    #[tokio::test]
    async fn report_config_handles_ollama_unreachable_softly_end_to_end() {
        // Verify the soft-failure path doesn't panic by walking the
        // full report_config code path against a tempdir-backed
        // solo.config.toml. `report_config` returns `()` even on
        // probe failure — assert that and that the printed output
        // would contain the unreachable marker by re-using the
        // formatter directly with the same probe result.
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = fresh_env();

        // SAFETY: ENV_LOCK held.
        unsafe { std::env::set_var(ENV_OLLAMA_BASE_URL, "http://127.0.0.1:1") };

        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("solo.config.toml");
        let cfg = solo_storage::SoloConfig::new(
            [0u8; 16],
            solo_storage::EmbedderConfig {
                name: "ollama:nomic-embed-text".into(),
                version: "v1".into(),
                dim: 768,
                dtype: "f32".into(),
            },
        );
        cfg.write(&cfg_path).expect("write config");

        // Direct call into report_config with --with-stats=true so the
        // probe attempts the unreachable endpoint. The probe will
        // return Err; report_config must NOT propagate.
        report_config(tmp.path(), true).await;

        // Re-derive the formatter output with the same backend +
        // soft-failure shape so we can assert the format would have
        // contained `(unreachable: …)` — without parsing stdout.
        let formatted = format_embedder_section(
            &cfg.embedder,
            Some(Err("connection refused (synthetic for assertion)".into())),
        );
        assert!(formatted.contains("Backend        : Ollama"));
        assert!(formatted.contains("Sample latency : (unreachable:"));
    }
}
