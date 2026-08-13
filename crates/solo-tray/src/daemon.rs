// SPDX-License-Identifier: Apache-2.0

//! Child-process supervisor for `solo daemon`.
//!
//! Owns the lifecycle: spawn → capture stderr → signal-on-quit →
//! wait-for-exit → optionally respawn. The supervisor task runs in the
//! tokio runtime; the UI thread interacts via the shared
//! `Arc<Mutex<DaemonHandle>>`.

use crate::logs::RingBuffer;
use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use zeroize::Zeroizing;

/// Legacy env var the tray still accepts on startup. The tray no
/// longer forwards this secret to the daemon child.
pub const ENV_PASSPHRASE: &str = "SOLO_PASSPHRASE";

/// Non-secret flag telling `solo daemon` to read one passphrase line
/// from stdin before it starts watching stdin EOF for shutdown.
const ENV_PASSPHRASE_STDIN: &str = "SOLO_PASSPHRASE_STDIN";

/// Lifecycle command from the UI thread → supervisor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Command_ {
    /// Default; supervisor keeps the daemon alive
    #[default]
    Run,
    /// Stop the running daemon and respawn it
    Restart,
    /// Stop and exit the supervisor (clean shutdown for tray quit)
    Quit,
}

/// Supervisor-visible daemon process state. This is intentionally
/// separate from HTTP health: a child can be running while `/v1/status`
/// is still warming up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SupervisorState {
    /// The tray has not received a passphrase yet.
    #[default]
    Locked,
    /// A supervisor task exists and is launching the child.
    Starting,
    /// A daemon child process is alive.
    Running,
    /// A user-requested restart is stopping the current child.
    Restarting,
    /// The daemon exited after it had been running; the supervisor may
    /// restart it after a cooldown.
    Crashed(String),
    /// Startup failed before the daemon became stable. User action is
    /// required, usually a corrected passphrase or environment fix.
    StartupFailed(String),
    /// The supervisor has stopped cleanly.
    Stopped,
}

#[derive(Debug)]
pub struct DaemonHandle {
    /// PID of the current child process, if any. Surfaced to the UI so
    /// the operator can correlate with `Activity Monitor` / `top` /
    /// `Task Manager`.
    pub pid: Option<u32>,
    /// Latest lifecycle command. Read by the supervisor loop on every
    /// iteration.
    pub command: Command_,
    /// Whether the supervisor is currently running the daemon
    /// (vs. between restarts or after a final quit).
    pub running: bool,
    /// Set once the supervisor task has observed `Quit` and returned.
    pub supervisor_exited: bool,
    /// Process-supervisor state, used by the UI for recovery and by
    /// status polling to distinguish "locked" from "down".
    pub state: SupervisorState,
}

impl Default for DaemonHandle {
    fn default() -> Self {
        Self {
            pid: None,
            command: Command_::Run,
            running: false,
            // No supervisor exists yet. This makes Quit before unlock
            // exit immediately instead of waiting for a task that has
            // not been spawned.
            supervisor_exited: true,
            state: SupervisorState::Locked,
        }
    }
}

impl DaemonHandle {
    pub fn request_restart(&mut self) {
        if self.supervisor_exited
            || matches!(
                self.state,
                SupervisorState::Locked
                    | SupervisorState::StartupFailed(_)
                    | SupervisorState::Stopped
            )
        {
            tracing::info!(
                state = ?self.state,
                "restart ignored; daemon supervisor is not running"
            );
            return;
        }
        self.command = Command_::Restart;
    }

    pub fn request_quit(&mut self) {
        self.command = Command_::Quit;
        if self.supervisor_exited {
            self.running = false;
            self.pid = None;
            self.state = SupervisorState::Stopped;
        }
    }

    pub fn prepare_start(&mut self) -> bool {
        if !self.supervisor_exited
            && !matches!(
                self.state,
                SupervisorState::Locked
                    | SupervisorState::StartupFailed(_)
                    | SupervisorState::Stopped
            )
        {
            return false;
        }
        self.command = Command_::Run;
        self.running = false;
        self.pid = None;
        self.supervisor_exited = false;
        self.state = SupervisorState::Starting;
        true
    }
}

/// The supervisor loop. Spawns `solo daemon`, captures stderr into the
/// log buffer, watches for lifecycle commands from the UI thread.
///
/// Exits when `DaemonHandle::command == Quit` and the child has
/// terminated. The eframe main loop polls the handle's `running` flag
/// to decide whether to display "stopped" vs "running" in the tray
/// icon.
pub async fn supervise(
    handle: Arc<Mutex<DaemonHandle>>,
    log_buffer: Arc<Mutex<RingBuffer>>,
    passphrase: Zeroizing<String>,
) -> Result<()> {
    loop {
        {
            let mut h = handle.lock().await;
            if h.command == Command_::Quit {
                h.running = false;
                h.pid = None;
                h.supervisor_exited = true;
                h.state = SupervisorState::Stopped;
                return Ok(());
            }
            if h.command == Command_::Restart {
                h.command = Command_::Run;
            }
            h.running = false;
            h.pid = None;
            h.supervisor_exited = false;
            h.state = SupervisorState::Starting;
        }

        // Spawn the daemon. The passphrase is written once to the
        // child's private stdin pipe; ambient env like `SOLO_DATA_DIR`
        // still flows through.
        let (mut child, _port) = match spawn_daemon(passphrase.as_str()).await {
            Ok(spawned) => spawned,
            Err(e) => {
                let msg = format!("{e:#}");
                let mut h = handle.lock().await;
                h.running = false;
                h.pid = None;
                h.supervisor_exited = true;
                h.state = SupervisorState::StartupFailed(msg);
                return Err(e).context("spawn solo daemon child");
            }
        };
        let pid = child.id();
        let spawned_at = std::time::Instant::now();

        {
            let mut h = handle.lock().await;
            h.pid = pid;
            h.running = true;
            h.supervisor_exited = false;
            h.state = SupervisorState::Running;
        }
        // Spawn the stderr capture task. tokio::process::Child takes
        // the stderr via .stderr(); we read line-by-line.
        let stderr = child
            .stderr
            .take()
            .context("child has no captured stderr handle")?;
        let log_buf = log_buffer.clone();
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                let mut buf = log_buf.lock().await;
                buf.push_line(line);
            }
        });

        // Watch for quit/restart commands while the child runs. Poll
        // every 250ms — fast enough to feel responsive on click,
        // cheap enough not to burn CPU.
        // Do not use `child.wait()` here. Tokio closes child stdin
        // when `wait()` is polled, and this tray uses stdin EOF as
        // its private graceful-shutdown signal.
        let mut quit_after_exit = false;
        let mut restart_after_exit = false;
        let mut unexpected_exit: Option<String> = None;
        let mut unexpected_exit_success = false;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;

            let cmd = handle.lock().await.command;
            match cmd {
                Command_::Run => {}
                Command_::Restart => {
                    tracing::info!("restart requested; stopping daemon");
                    {
                        let mut h = handle.lock().await;
                        h.command = Command_::Run;
                        h.state = SupervisorState::Restarting;
                    }
                    restart_after_exit = true;
                    stop_child(&mut child).await;
                    break;
                }
                Command_::Quit => {
                    tracing::info!("quit requested; stopping daemon");
                    quit_after_exit = true;
                    stop_child(&mut child).await;
                    break;
                }
            }

            match child.try_wait() {
                Ok(Some(status)) => {
                    let msg = format!("daemon child exited unexpectedly: {status:?}");
                    tracing::warn!(?status, "daemon child exited unexpectedly");
                    unexpected_exit_success = status.success();
                    unexpected_exit = Some(msg);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    let msg = format!("try_wait() on daemon child failed: {e}");
                    tracing::error!(error = %e, "try_wait() on daemon child failed");
                    unexpected_exit = Some(msg);
                    break;
                }
            }
        }

        // Wait for stderr drain to finish so we don't drop captured
        // logs from the last second.
        let _ = stderr_task.await;

        {
            let mut h = handle.lock().await;
            h.running = false;
            h.pid = None;
        }

        if quit_after_exit {
            let mut h = handle.lock().await;
            h.supervisor_exited = true;
            h.state = SupervisorState::Stopped;
            return Ok(());
        }

        if restart_after_exit {
            continue;
        }

        if let Some(msg) = unexpected_exit {
            let startup_failure = spawned_at.elapsed() < std::time::Duration::from_secs(10);
            let mut h = handle.lock().await;
            h.running = false;
            h.pid = None;
            if startup_failure && !unexpected_exit_success {
                h.supervisor_exited = true;
                h.state = SupervisorState::StartupFailed(msg);
                return Ok(());
            }
            if unexpected_exit_success {
                h.state = SupervisorState::Restarting;
            } else {
                h.state = SupervisorState::Crashed(msg);
            }
        }

        // Crash-respawn cooldown so a broken daemon doesn't burn CPU.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

const ENV_SHUTDOWN_ON_STDIN_EOF: &str = "SOLO_DAEMON_SHUTDOWN_ON_STDIN_EOF";

/// Spawn `solo daemon --http-port <port>` as a child process with
/// stderr captured.
///
/// We MUST pass `--http-port` explicitly because `solo daemon` does
/// NOT start the HTTP server by default (see dev-log 0160 for the
/// design history). Without it, the tray's `/v1/status` poller would
/// never see a healthy response and the icon would sit amber forever.
async fn spawn_daemon(passphrase: &str) -> Result<(Child, u16)> {
    // Resolve `solo` binary path. Try the same dir as `solo-tray`
    // first (sibling install pattern from the Windows installer);
    // fall back to PATH lookup.
    let solo_bin = which_solo();

    let settings = crate::settings::Settings::load(&crate::settings::settings_path());
    let port = settings.http_port;

    ensure_ollama_startup_dependency().await;

    let mut cmd = Command::new(&solo_bin);
    cmd.args(daemon_args(port))
        .env_remove(ENV_PASSPHRASE)
        .env(ENV_PASSPHRASE_STDIN, "1")
        .env(ENV_SHUTDOWN_ON_STDIN_EOF, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    // Windows: stop the child from popping its own console window.
    // The tray itself called `FreeConsole` after reading the
    // passphrase; if we don't pass `CREATE_NO_WINDOW` here, Windows
    // allocates a brand-new console for the console-subsystem daemon
    // child on every spawn — a flicker on the desktop with each
    // restart. The daemon doesn't read or write its console (stdin =
    // Null, stdout = Null, stderr = piped → log buffer), so a hidden
    // console is fine. We still give the daemon a private stdin pipe
    // so dropping it can request graceful shutdown after the tray has
    // detached from the user-facing console.
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn solo daemon at {}", solo_bin.display()))?;

    match child.stdin.as_mut() {
        Some(stdin) => {
            if let Err(e) = write_passphrase_to_stdin(stdin, passphrase).await {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(e);
            }
        }
        None => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("daemon child stdin was not piped");
        }
    }

    // Bind the child's lifetime to the tray's via a Windows Job
    // Object with `KILL_ON_JOB_CLOSE`. If the tray dies any way
    // (clean Quit, panic, Task Manager, parent shell close), the
    // OS closes the job handle and force-kills every member —
    // including the daemon. Best-effort: if the assignment fails
    // (e.g., the child has another job assignment we can't override),
    // we log and continue; the supervisor's own Quit path still
    // signals graceful shutdown on clean exits.
    #[cfg(windows)]
    {
        if let Some(pid) = child.id() {
            if let Err(e) = assign_to_kill_on_close_job(pid) {
                tracing::warn!(
                    pid,
                    error = %e,
                    "could not bind daemon child to tray job object; \
                     daemon may survive tray crash"
                );
            } else {
                tracing::debug!(pid, "daemon child assigned to tray job object");
            }
        }
    }

    Ok((child, port))
}

fn daemon_args(port: u16) -> Vec<std::ffi::OsString> {
    vec![
        "daemon".into(),
        "--http-port".into(),
        port.to_string().into(),
    ]
}

async fn write_passphrase_to_stdin(
    stdin: &mut tokio::process::ChildStdin,
    passphrase: &str,
) -> Result<()> {
    stdin
        .write_all(passphrase.as_bytes())
        .await
        .context("write daemon passphrase to stdin")?;
    stdin
        .write_all(b"\n")
        .await
        .context("write daemon passphrase newline to stdin")?;
    stdin.flush().await.context("flush daemon passphrase stdin")
}

const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434";
const ENV_OLLAMA_BASE_URL: &str = "SOLO_OLLAMA_BASE_URL";
const ENV_OLLAMA_BIN: &str = "SOLO_OLLAMA_BIN";
const ENV_OLLAMA_SERVER_KEEP_ALIVE: &str = "OLLAMA_KEEP_ALIVE";
const DEFAULT_OLLAMA_SERVER_KEEP_ALIVE: &str = "30s";
const OLLAMA_STARTUP_PROBE_ATTEMPTS: usize = 20;
const OLLAMA_STARTUP_PROBE_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
const OLLAMA_STARTUP_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
struct OllamaStartupDependency {
    base_url: String,
    reason: String,
}

async fn ensure_ollama_startup_dependency() {
    let settings_path = crate::settings::settings_path();
    let Some(dependency) = ollama_startup_dependency(&settings_path) else {
        return;
    };
    if !is_loopback_http_base_url(&dependency.base_url) {
        tracing::debug!(
            base_url = %dependency.base_url,
            reason = %dependency.reason,
            "Ollama dependency uses a non-loopback endpoint; not auto-starting local Ollama"
        );
        return;
    }
    if probe_ollama(&dependency.base_url).await {
        tracing::debug!(
            base_url = %dependency.base_url,
            reason = %dependency.reason,
            "Ollama dependency is already reachable"
        );
        return;
    }

    let program = resolve_ollama_program();
    match spawn_ollama_serve(&program).await {
        Ok(pid) => tracing::info!(
            pid,
            base_url = %dependency.base_url,
            reason = %dependency.reason,
            "started local Ollama before Solo daemon startup"
        ),
        Err(error) => {
            tracing::warn!(
                error = %error,
                program = %program.to_string_lossy(),
                base_url = %dependency.base_url,
                reason = %dependency.reason,
                "could not start local Ollama before Solo daemon startup"
            );
            return;
        }
    }

    for _ in 0..OLLAMA_STARTUP_PROBE_ATTEMPTS {
        tokio::time::sleep(OLLAMA_STARTUP_PROBE_DELAY).await;
        if probe_ollama(&dependency.base_url).await {
            tracing::info!(
                base_url = %dependency.base_url,
                "Ollama is reachable; continuing Solo daemon startup"
            );
            return;
        }
    }
    tracing::warn!(
        base_url = %dependency.base_url,
        "Ollama was started but did not become reachable before Solo daemon startup"
    );
}

fn ollama_startup_dependency(settings_path: &Path) -> Option<OllamaStartupDependency> {
    let config_path = settings_path.parent()?.join("solo.config.toml");
    let raw = std::fs::read_to_string(&config_path).ok()?;
    let config = raw.parse::<toml::Value>().ok()?;
    ollama_startup_dependency_from_config(&config)
}

fn ollama_startup_dependency_from_config(config: &toml::Value) -> Option<OllamaStartupDependency> {
    let embedder_name = config
        .get("embedder")
        .and_then(toml::Value::as_table)
        .and_then(|section| section.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let embedder_uses_ollama = embedder_name.starts_with("ollama:");

    let llm = config.get("llm").and_then(toml::Value::as_table);
    let llm_mode = llm
        .and_then(|section| section.get("mode"))
        .and_then(toml::Value::as_str)
        .unwrap_or_default();
    let llm_uses_ollama = llm_mode == "ollama";

    if !embedder_uses_ollama && !llm_uses_ollama {
        return None;
    }

    let base_url = std::env::var(ENV_OLLAMA_BASE_URL)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            llm.and_then(|section| section.get("base_url"))
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string());

    let reason = match (embedder_uses_ollama, llm_uses_ollama) {
        (true, true) => "embedder and Steward LLM use Ollama",
        (true, false) => "embedder uses Ollama",
        (false, true) => "Steward LLM uses Ollama",
        (false, false) => unreachable!(),
    }
    .to_string();

    Some(OllamaStartupDependency {
        base_url: normalize_base_url(&base_url),
        reason,
    })
}

async fn probe_ollama(base_url: &str) -> bool {
    let url = ollama_api_url(base_url, "tags");
    let client = reqwest::Client::new();
    match client
        .get(url)
        .timeout(OLLAMA_STARTUP_PROBE_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response.status().is_success(),
        Err(_) => false,
    }
}

async fn spawn_ollama_serve(program: &OsString) -> Result<u32> {
    let mut cmd = Command::new(program);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if std::env::var_os(ENV_OLLAMA_SERVER_KEEP_ALIVE).is_none() {
        cmd.env(
            ENV_OLLAMA_SERVER_KEEP_ALIVE,
            DEFAULT_OLLAMA_SERVER_KEEP_ALIVE,
        );
    }

    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    let child = cmd.spawn().context("spawn ollama serve")?;
    Ok(child.id().unwrap_or_default())
}

fn resolve_ollama_program() -> OsString {
    if let Some(path) = std::env::var_os(ENV_OLLAMA_BIN).filter(|value| !value.is_empty()) {
        return path;
    }

    #[cfg(windows)]
    {
        for root in [
            std::env::var_os("LOCALAPPDATA"),
            std::env::var_os("ProgramFiles"),
            std::env::var_os("ProgramFiles(x86)"),
        ]
        .into_iter()
        .flatten()
        {
            let root = PathBuf::from(root);
            let candidate = root.join("Programs").join("Ollama").join("ollama.exe");
            if candidate.is_file() {
                return candidate.into_os_string();
            }
            let candidate = root.join("Ollama").join("ollama.exe");
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
    }

    OsString::from(if cfg!(windows) {
        "ollama.exe"
    } else {
        "ollama"
    })
}

fn ollama_api_url(base_url: &str, path: &str) -> String {
    format!(
        "{}/api/{}",
        normalize_base_url(base_url),
        path.trim_start_matches('/')
    )
}

fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

fn is_loopback_http_base_url(base_url: &str) -> bool {
    let lower = normalize_base_url(base_url).to_ascii_lowercase();
    lower == "http://localhost"
        || lower.starts_with("http://localhost:")
        || lower == "http://127.0.0.1"
        || lower.starts_with("http://127.0.0.1:")
        || lower == "http://[::1]"
        || lower.starts_with("http://[::1]:")
}

/// Windows-only: bind `child_pid` to a process-wide Job Object so it
/// dies with the tray. The job object is created lazily on the first
/// call (one job per tray process) and its handle is held in a
/// `OnceLock` for the rest of the process lifetime — the handle's
/// drop on process exit is what fires the `KILL_ON_JOB_CLOSE` flag.
///
/// We hold a `HANDLE` (an opaque `isize` on Windows) inside a static
/// `OnceLock` so the kernel-side reference count for the job stays
/// nonzero. When the tray process terminates (regardless of cause),
/// the OS closes all handles for the process, including this one;
/// the kernel observes the last handle drop on a job with
/// `KILL_ON_JOB_CLOSE` set and terminates every job member.
///
/// Linux/macOS parallel is `prctl(PR_SET_PDEATHSIG)` on Linux and a
/// kqueue-based child-watch on BSD/macOS, neither of which is wired
/// yet. The supervisor's graceful Quit path still kills the daemon
/// on clean tray shutdowns there; only crashes / SIGKILLs of the
/// tray leave an orphan daemon. Tracked separately.
#[cfg(windows)]
fn assign_to_kill_on_close_job(child_pid: u32) -> std::io::Result<()> {
    use std::sync::OnceLock;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// Hold the job handle as `usize` so it can sit inside a
    /// `OnceLock` (raw pointers like `HANDLE = *mut c_void` aren't
    /// `Send + Sync`). 0 == NULL == "creation failed". We never
    /// close the handle ourselves — the kernel closes it when the
    /// tray process exits, which is exactly when we WANT
    /// `KILL_ON_JOB_CLOSE` to fire.
    static JOB_HANDLE: OnceLock<usize> = OnceLock::new();

    let job_raw = *JOB_HANDLE.get_or_init(|| {
        // SAFETY: passing null for lpJobAttributes and lpName creates
        // an anonymous job with default security. Returns NULL on
        // failure — we surface that by storing 0.
        let h: HANDLE = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if h.is_null() {
            return 0;
        }
        // SAFETY: zeroed JOBOBJECT_EXTENDED_LIMIT_INFORMATION is a
        // valid initial state (all limits disabled); we then opt
        // into the single flag we want.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `&info` is a valid pointer to the struct of the
        // declared size; class JobObjectExtendedLimitInformation
        // pairs with that struct.
        let ok = unsafe {
            SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            // Do not keep a job handle that lacks the kill-on-close
            // limit; callers would incorrectly believe crash cleanup
            // is active. Store 0 so assignment fails visibly.
            unsafe { CloseHandle(h) };
            return 0;
        }
        h as usize
    });

    if job_raw == 0 {
        return Err(std::io::Error::other("CreateJobObjectW returned NULL"));
    }
    let job: HANDLE = job_raw as HANDLE;

    // We need a process handle (not just a PID) for
    // AssignProcessToJobObject. The minimum access rights documented
    // for the call are PROCESS_SET_QUOTA + PROCESS_TERMINATE.
    // SAFETY: OpenProcess is safe to call with a PID; returns NULL on
    // failure (we check below). The returned handle is duplicated for
    // our use and must be closed (we do, at the end of this fn).
    let child_handle: HANDLE =
        unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, child_pid) };
    if child_handle.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: both `job` and `child_handle` are valid handles
    // obtained above.
    let ok = unsafe { AssignProcessToJobObject(job, child_handle) };

    // Always close the child handle — we don't need it after assignment.
    // The assignment itself is persisted in the kernel's job-membership
    // table, not in our handle.
    // SAFETY: child_handle was obtained from OpenProcess just above.
    unsafe { CloseHandle(child_handle) };

    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Best-effort search for the `solo` executable.
fn which_solo() -> std::path::PathBuf {
    // Sibling-of-current-exe lookup first (covers the Windows
    // installer pattern where solo-tray.exe and solo.exe live in the
    // same directory).
    if let Ok(self_path) = std::env::current_exe()
        && let Some(dir) = self_path.parent()
    {
        let sibling = dir.join(if cfg!(windows) { "solo.exe" } else { "solo" });
        if sibling.is_file() {
            return sibling;
        }
    }
    // Fallback: PATH lookup via the shell's resolution.
    std::path::PathBuf::from(if cfg!(windows) { "solo.exe" } else { "solo" })
}

/// Gracefully stop the daemon. First closes the tray-owned stdin pipe
/// so the daemon's EOF watcher can shut down cleanly; Unix also gets
/// SIGTERM as a fallback. After 10s, escalates to a hard kill.
async fn stop_child(child: &mut Child) {
    let pid = match child.id() {
        Some(p) => p,
        None => return, // already exited
    };

    // The tray-spawned daemon watches this private pipe for EOF. Drop
    // it before platform signals so Windows can shut down gracefully
    // even after the tray has called FreeConsole.
    let _ = child.stdin.take();

    send_graceful_signal(pid);

    // Wait up to 10s for clean exit.
    let wait = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait()).await;
    match wait {
        Ok(Ok(status)) => {
            tracing::info!(pid, ?status, "daemon exited cleanly");
            return;
        }
        Ok(Err(e)) => {
            tracing::warn!(pid, error = %e, "wait() failed; escalating to kill");
        }
        Err(_) => {
            tracing::warn!(pid, "daemon did not exit within 10s; escalating to kill");
        }
    }

    // Escalation: SIGKILL (or its Windows equivalent — tokio::Child::kill
    // already TerminateProcess's on Windows).
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(unix)]
fn send_graceful_signal(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    if let Err(e) = kill(Pid::from_raw(pid as i32), Signal::SIGTERM) {
        tracing::warn!(pid, error = %e, "SIGTERM failed; waiting for stdin EOF shutdown");
    }
}

#[cfg(windows)]
fn send_graceful_signal(_pid: u32) {
    // No console signal on Windows: the daemon child is intentionally
    // spawned with CREATE_NO_WINDOW, so stdin EOF is the graceful path.
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg_strings(args: Vec<std::ffi::OsString>) -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn daemon_args_bind_only_the_community_library() {
        assert_eq!(
            arg_strings(daemon_args(17821)),
            vec!["daemon", "--http-port", "17821"]
        );
    }

    #[test]
    fn ollama_startup_dependency_detects_embedder_config() {
        let config = r#"
            [embedder]
            name = "ollama:nomic-embed-text"

            [llm]
            mode = "none"
        "#
        .parse::<toml::Value>()
        .unwrap();

        let dependency = ollama_startup_dependency_from_config(&config).unwrap();
        assert_eq!(dependency.base_url, DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(dependency.reason, "embedder uses Ollama");
    }

    #[test]
    fn ollama_startup_dependency_uses_llm_base_url() {
        let config = r#"
            [embedder]
            name = "bundled:all-minilm"

            [llm]
            mode = "ollama"
            base_url = "http://127.0.0.1:11435/"
        "#
        .parse::<toml::Value>()
        .unwrap();

        let dependency = ollama_startup_dependency_from_config(&config).unwrap();
        assert_eq!(dependency.base_url, "http://127.0.0.1:11435");
        assert_eq!(dependency.reason, "Steward LLM uses Ollama");
    }

    #[test]
    fn ollama_startup_dependency_ignores_non_ollama_config() {
        let config = r#"
            [embedder]
            name = "bundled:all-minilm"

            [llm]
            mode = "anthropic"
        "#
        .parse::<toml::Value>()
        .unwrap();

        assert!(ollama_startup_dependency_from_config(&config).is_none());
    }

    #[test]
    fn loopback_base_url_detection_is_conservative() {
        assert!(is_loopback_http_base_url("http://localhost:11434"));
        assert!(is_loopback_http_base_url("http://127.0.0.1:11434/"));
        assert!(is_loopback_http_base_url("http://[::1]:11434"));
        assert!(!is_loopback_http_base_url("https://localhost:11434"));
        assert!(!is_loopback_http_base_url("http://192.168.1.50:11434"));
        assert!(!is_loopback_http_base_url("http://ollama.lan:11434"));
    }

    #[test]
    fn ollama_api_url_normalizes_slashes() {
        assert_eq!(
            ollama_api_url("http://localhost:11434/", "/tags"),
            "http://localhost:11434/api/tags"
        );
    }
}
