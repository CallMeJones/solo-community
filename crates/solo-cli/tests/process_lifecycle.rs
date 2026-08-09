// SPDX-License-Identifier: Apache-2.0

//! Process-lifecycle integration tests. These spawn the real `solo`
//! binary (via Cargo's `CARGO_BIN_EXE_solo` env var) to cover scenarios
//! the in-process `properties.rs` tests can't reach: SIGKILL during a
//! running daemon, lockfile recovery after an ungraceful exit, etc.
//!
//! Per ADR-0003 §"Final consolidated action items" — items #9 (kill -9
//! between SQL commit and HNSW write) and #15 (shutdown timeout) need
//! a real subprocess we can signal. This file is the harness.
//!
//! ## Cost
//!
//! These tests are intentionally slower than the unit-test suite: each
//! `solo init`/`remember`/`recall` invocation runs the full Argon2id
//! key derivation (~1-3 sec each) plus SQLCipher open. A full test
//! file run is ~15-30 sec. They live in `tests/` (separate test
//! binary) so the standard `cargo test --lib` loop stays fast.
//!
//! ## Dev-log entry
//!
//! Introduced in dev log 0015 (next-phase candidate "Process-spawning
//! property tests" from 0014).

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PASSPHRASE: &str = "test-passphrase-for-process-lifecycle-tests";

fn solo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_solo"))
}

/// Build a `Command` for `solo` with the test's data dir + passphrase
/// already set. Caller adds the subcommand + args.
fn solo_cmd(data_dir: &Path) -> Command {
    let mut c = Command::new(solo_bin());
    c.env("SOLO_DATA_DIR", data_dir);
    c.env("SOLO_PASSPHRASE", PASSPHRASE);
    // Certify the fully offline path on both Windows and Linux.
    c.env("SOLO_EMBEDDER", "stub");
    // Mute the stderr passphrase warning from `read_passphrase` so the
    // test output stays focused. Tracing logs still go to stderr from
    // the daemon, which is fine — we only assert on stdout.
    c
}

/// Wait up to `timeout` for the lockfile to exist. The daemon writes
/// it via `Lockfile::acquire` very early in startup; once it appears,
/// we know the daemon has gotten past key derivation + DB open.
fn wait_for_lockfile(data_dir: &Path, timeout: Duration) -> bool {
    let lock_path = data_dir.join("solo.lock");
    let start = Instant::now();
    while start.elapsed() < timeout {
        if lock_path.is_file() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// Pick a free localhost port by binding 0 (OS-assigned), then drop the
/// listener. There's a small TOCTOU window before the daemon binds, but
/// for an integration test on a quiet machine it's negligible. We use
/// 127.0.0.1 specifically so the result is meaningful when the daemon
/// is later told to bind loopback.
fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    let port = listener.local_addr().expect("local_addr").port();
    drop(listener);
    port
}

/// Poll `GET /health` until 200 or `timeout` elapses. The lockfile
/// existing only proves the writer is up; the HTTP transport is
/// spawned later as a separate tokio task, so HTTP-driven tests need
/// this stronger readiness signal.
fn wait_for_http_ready(port: u16, timeout: Duration) -> bool {
    let url = format!("http://127.0.0.1:{port}/health");
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(resp) = ureq::get(&url).timeout(Duration::from_millis(500)).call() {
            if resp.status() == 200 {
                return true;
            }
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    false
}

/// POST `{ "content": <content> }` to `/memory`, return the response
/// body parsed as `serde_json::Value`. Panics on transport / non-2xx
/// errors so the test fails fast with a clear message.
fn http_remember(port: u16, content: &str) -> serde_json::Value {
    let url = format!("http://127.0.0.1:{port}/memory");
    ureq::post(&url)
        .timeout(Duration::from_secs(10))
        .send_json(serde_json::json!({ "content": content }))
        .unwrap_or_else(|e| panic!("POST /memory for {content:?} failed: {e}"))
        .into_json::<serde_json::Value>()
        .expect("parse JSON response")
}

/// Smoke test: the integration-test exe pattern works on this platform
/// at all. If `cargo test --test process_lifecycle` can't even spawn
/// `solo --version` and read its stdout, the more complex tests below
/// will be impossible to interpret. (On Windows, integration tests
/// occasionally trip the UAC ERROR_ELEVATION_REQUIRED heuristic for
/// certain exe names — the file is named `process_lifecycle.rs`, not
/// `install_*` or `setup_*`, to dodge that.)
#[test]
fn solo_version_runs() {
    let out = Command::new(solo_bin())
        .arg("--version")
        .output()
        .expect("spawn solo --version");
    assert!(out.status.success(), "solo --version failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("solo"),
        "stdout doesn't look like a version string: {stdout:?}"
    );
}

/// SIGKILL recovery: an ungracefully-terminated daemon must leave the
/// data dir in a state where the next `solo` invocation can acquire
/// the lockfile and read previously-written memories.
///
/// Sequence:
///   1. `solo init` (creates DB + config).
///   2. `solo remember "alpha"` (one-shot — committed before daemon).
///   3. `solo daemon` (long-running) — wait until it acquires the lock.
///   4. Process::kill the daemon ungracefully (SIGKILL on Unix,
///      TerminateProcess on Windows). The daemon never gets to release
///      the lockfile or run its `wal_checkpoint(TRUNCATE)`.
///   5. `solo recall "alpha"` — must succeed and find the memory.
///      This exercises Lockfile's PID-alive recovery (the dead daemon's
///      PID is in the lockfile; recall verifies the PID is gone and
///      takes over the lock) AND verifies SQLCipher's WAL replay leaves
///      the DB consistent.
///
/// Pre-existing one-shot writes (step 2) are the safety net we assert
/// on. We deliberately do NOT write through the daemon (avoids needing
/// HTTP in the test); instead we kill the daemon while it's *idle* —
/// still meaningful because it tests the lockfile-recovery + WAL-replay
/// path, which is the same path that would also recover from a
/// mid-write crash.
#[test]
fn kill9_during_idle_daemon_is_recoverable() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // 1. solo init
    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    // 2. solo remember "alpha" (one-shot, lock released on exit)
    let out = solo_cmd(data_dir)
        .args(["remember", "alpha-zebra-quokka"])
        .output()
        .expect("spawn solo remember");
    assert!(
        out.status.success(),
        "solo remember failed: {:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    // 3. spawn daemon, wait for lockfile
    let mut daemon = solo_cmd(data_dir)
        .arg("daemon")
        // Snapshot timer at default; HTTP off (default).
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn solo daemon");
    assert!(
        wait_for_lockfile(data_dir, Duration::from_secs(15)),
        "daemon did not acquire lockfile within 15s"
    );

    // 4. SIGKILL / TerminateProcess
    daemon.kill().expect("kill daemon");
    let _ = daemon.wait();

    // 5. solo recall — must find the memory we wrote in step 2
    let out = solo_cmd(data_dir)
        .args(["recall", "alpha-zebra-quokka"])
        .output()
        .expect("spawn solo recall");
    assert!(
        out.status.success(),
        "solo recall failed after kill: status={:?} stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("alpha-zebra-quokka"),
        "recall didn't find the pre-kill memory; stdout: {stdout}"
    );
}

/// Mid-write SIGKILL recovery: writes that the daemon has acknowledged
/// over HTTP (i.e., received a 2xx response) must be durable across an
/// ungraceful kill.
///
/// This is the stronger version of `kill9_during_idle_daemon_is_recoverable`
/// — it kills the daemon while it's been actively committing user-
/// visible writes, not while it's idle. Exercises the writer's "reply
/// after SQL commit + HNSW add, before pending_index drain" ordering
/// (ADR-0003 §P8-E) plus the lockfile + WAL recovery paths.
///
/// Sequence:
///   1. `solo init` (one-shot).
///   2. Spawn `solo daemon --http-port=N` on an OS-assigned port.
///   3. Wait for `GET /health` to return 200 (HTTP transport up).
///   4. POST 5 distinct contents to `/memory`; collect 200 responses.
///   5. `Process::kill` ungracefully (SIGKILL on Unix, TerminateProcess
///      on Windows).
///   6. For each of the 5 contents, run `solo recall <content>` and
///      assert it appears in the output.
#[test]
fn mid_write_kill9_recovery() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // 1. init
    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    // 2. spawn daemon with HTTP
    let port = pick_free_port();
    let mut daemon = solo_cmd(data_dir)
        .args(["daemon", "--http-port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn solo daemon");

    // 3. wait for HTTP readiness
    if !wait_for_http_ready(port, Duration::from_secs(20)) {
        let _ = daemon.kill();
        let _ = daemon.wait();
        panic!("daemon HTTP did not come up on 127.0.0.1:{port} within 20s");
    }

    // 4. POST 5 contents
    let contents: Vec<String> = (0..5)
        .map(|i| format!("midwrite-content-{i}-{:x}", rand_suffix()))
        .collect();
    let mut acked = Vec::new();
    for c in &contents {
        let resp = http_remember(port, c);
        // Response shape: { "memory_id": "..." } per http_remember handler.
        // We don't assert the shape — just that we got a JSON body back
        // (i.e., the daemon ack'd before we kill it).
        let _ = resp;
        acked.push(c.clone());
    }
    assert_eq!(acked.len(), 5, "expected all 5 writes to be ack'd");

    // 5. kill
    daemon.kill().expect("kill daemon");
    let _ = daemon.wait();

    // 6. recall each via one-shot
    for c in &contents {
        let out = solo_cmd(data_dir)
            .args(["recall", c])
            .output()
            .expect("spawn solo recall");
        assert!(
            out.status.success(),
            "solo recall {c} failed: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(c),
            "recall didn't surface ack'd memory {c}; stdout: {stdout}"
        );
    }
}

/// Cheap process-local random suffix so concurrent test runs don't
/// collide on remembered content (which would make `recall` ambiguous).
fn rand_suffix() -> u32 {
    use std::time::SystemTime;
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    nanos ^ (std::process::id())
}

/// Y.6.1 — `solo consolidate` one-shot runs end-to-end. Setup with
/// 3 identical-content remembers (stub embedder produces unit-norm
/// hash vectors → identical content → identical vectors → cluster
/// above threshold), then dispatch the CLI command and assert the
/// report's stdout shape includes the expected fields.
#[test]
fn solo_consolidate_one_shot_reports_clusters_built() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    // Three remembers with identical content → identical vectors →
    // one cluster after consolidate.
    let suffix = rand_suffix();
    let theme = format!("y6-cli-theme-{suffix:x}");
    for _ in 0..3 {
        let out = solo_cmd(data_dir)
            .args(["remember", &theme])
            .output()
            .expect("spawn solo remember");
        assert!(
            out.status.success(),
            "solo remember failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = solo_cmd(data_dir)
        .arg("consolidate")
        .output()
        .expect("spawn solo consolidate");
    assert!(
        out.status.success(),
        "solo consolidate failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("episodes_seen=3"),
        "stdout missing episodes_seen=3: {stdout}"
    );
    assert!(
        stdout.contains("clusters_built=1"),
        "stdout missing clusters_built=1: {stdout}"
    );

    // Idempotent: second run finds nothing new.
    let out = solo_cmd(data_dir)
        .arg("consolidate")
        .output()
        .expect("spawn solo consolidate (re-run)");
    assert!(out.status.success());
    let stdout2 = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout2.contains("episodes_seen=0"),
        "second run should see no new candidates: {stdout2}"
    );
}

/// Y.5 — daemon's consolidate timer doesn't crash the daemon under
/// realistic load and shuts down cleanly. Spawns the daemon with a
/// 1-second consolidate cadence, lets it tick a few times against
/// active writes, then kills it and verifies a follow-up recall
/// still succeeds (DB is consistent + lockfile is reclaimable).
///
/// We don't assert on cluster row counts here — the
/// `properties::consolidate_*` tests already cover that
/// deterministically in-process. The value of this test is the
/// integration: spawn → timer ticks interleave with writes →
/// graceful kill → next run starts cleanly.
#[test]
fn consolidate_timer_runs_under_writes_and_shuts_down_cleanly() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    let port = pick_free_port();
    let mut daemon = solo_cmd(data_dir)
        .args([
            "daemon",
            "--http-port",
            &port.to_string(),
            "--consolidate-interval-secs",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn solo daemon");

    if !wait_for_http_ready(port, Duration::from_secs(20)) {
        let _ = daemon.kill();
        let _ = daemon.wait();
        panic!("daemon HTTP did not come up within 20s");
    }

    // Two write bursts > 1s apart so the consolidate timer ticks at
    // least once between them. Identical content per burst → stub
    // embedder produces identical unit-norm vectors → clusters
    // form above the 0.85 threshold.
    let suffix = rand_suffix();
    let theme = format!("y5-timer-theme-{suffix:x}");
    for _ in 0..3 {
        let _ = http_remember(port, &theme);
    }
    std::thread::sleep(Duration::from_millis(1500));
    for _ in 0..3 {
        let _ = http_remember(port, &theme);
    }
    std::thread::sleep(Duration::from_millis(1500));

    daemon.kill().expect("kill daemon");
    let _ = daemon.wait();

    let out = solo_cmd(data_dir)
        .args(["recall", &theme])
        .output()
        .expect("spawn solo recall");
    assert!(
        out.status.success(),
        "recall after consolidate-timer daemon kill failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&theme),
        "recall didn't surface the themed content; stdout: {stdout}"
    );
}

/// Graceful shutdown: SIGTERM must result in a clean exit within a
/// bounded time. ADR-0003 §O7 sets `SHUTDOWN_TIMEOUT_SECS = 30` as the
/// daemon's internal budget; this test asserts a tighter 15s ceiling
/// for an idle daemon, which exercises the entire signal-handling +
/// HTTP-drain + writer-flush + snapshot-save chain.
///
/// Unix-only because Windows lacks a clean equivalent of SIGTERM that
/// targets a non-console child process. On Windows the same chain is
/// covered indirectly by `kill9_during_idle_daemon_is_recoverable` +
/// the daemon's panic-hook / OS-reaping path.
#[cfg(unix)]
#[test]
fn graceful_shutdown_within_budget() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    // init + remember "graceful-anchor"
    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");
    let out = solo_cmd(data_dir)
        .args(["remember", "graceful-anchor"])
        .output()
        .expect("spawn solo remember");
    assert!(out.status.success(), "solo remember failed: {out:?}");

    // spawn daemon, wait for lockfile (no HTTP needed here)
    let mut daemon = solo_cmd(data_dir)
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn solo daemon");
    assert!(
        wait_for_lockfile(data_dir, Duration::from_secs(15)),
        "daemon did not acquire lockfile within 15s"
    );

    // SIGTERM
    let pid = Pid::from_raw(daemon.id() as i32);
    kill(pid, Signal::SIGTERM).expect("send SIGTERM");

    // Poll for the child to exit, with a tight 15s budget.
    let budget = Duration::from_secs(15);
    let started = Instant::now();
    let exit_status = loop {
        match daemon.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if started.elapsed() >= budget => {
                // Force-kill so we don't leak the process, then fail.
                let _ = daemon.kill();
                let _ = daemon.wait();
                panic!(
                    "daemon did not exit within {}s of SIGTERM",
                    budget.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(
        exit_status.success(),
        "daemon exited non-zero after SIGTERM: {exit_status:?}"
    );

    // Sanity: the post-shutdown DB still works.
    let out = solo_cmd(data_dir)
        .args(["recall", "graceful-anchor"])
        .output()
        .expect("spawn solo recall");
    assert!(
        out.status.success(),
        "solo recall failed after graceful shutdown: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("graceful-anchor"),
        "recall didn't surface anchor memory; stdout: {stdout}"
    );
}

/// Tray-owned shutdown path: when the daemon is launched with the private
/// tray env var, closing stdin must behave like a graceful shutdown signal.
#[test]
fn stdin_eof_shutdown_within_budget() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    let mut daemon = solo_cmd(data_dir)
        .arg("daemon")
        .env("SOLO_DAEMON_SHUTDOWN_ON_STDIN_EOF", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn solo daemon");
    assert!(
        wait_for_lockfile(data_dir, Duration::from_secs(15)),
        "daemon did not acquire lockfile within 15s"
    );

    drop(daemon.stdin.take());

    let budget = Duration::from_secs(15);
    let started = Instant::now();
    let exit_status = loop {
        match daemon.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if started.elapsed() >= budget => {
                let _ = daemon.kill();
                let _ = daemon.wait();
                panic!(
                    "daemon did not exit within {}s of stdin EOF",
                    budget.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(
        exit_status.success(),
        "daemon exited non-zero after stdin EOF: {exit_status:?}"
    );
}

/// Tray-owned startup path: the tray writes the passphrase as the
/// first stdin line, keeps the pipe open while the daemon runs, then
/// closes it to request graceful shutdown. This avoids exposing the
/// passphrase in the daemon child environment.
#[test]
fn stdin_passphrase_then_eof_shutdown_within_budget() {
    use std::io::Write;

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    let mut daemon = solo_cmd(data_dir)
        .arg("daemon")
        .env_remove("SOLO_PASSPHRASE")
        .env("SOLO_PASSPHRASE_STDIN", "1")
        .env("SOLO_DAEMON_SHUTDOWN_ON_STDIN_EOF", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn solo daemon");

    {
        let stdin = daemon.stdin.as_mut().expect("daemon stdin piped");
        writeln!(stdin, "{PASSPHRASE}").expect("write stdin passphrase");
        stdin.flush().expect("flush stdin passphrase");
    }

    assert!(
        wait_for_lockfile(data_dir, Duration::from_secs(15)),
        "daemon did not acquire lockfile within 15s"
    );

    drop(daemon.stdin.take());

    let budget = Duration::from_secs(15);
    let started = Instant::now();
    let exit_status = loop {
        match daemon.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if started.elapsed() >= budget => {
                let _ = daemon.kill();
                let _ = daemon.wait();
                panic!(
                    "daemon did not exit within {}s of stdin EOF",
                    budget.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    assert!(
        exit_status.success(),
        "daemon exited non-zero after stdin EOF: {exit_status:?}"
    );
}

/// Regression for the v0.3.4 pre-release smoke finding: `solo backup
/// --to <data-dir>/solo.db --force` MUST refuse without destroying the
/// source database. v0.3.3's CLI ran `remove_file(dest)` before the
/// inner same-file check, silently wiping the source data dir on a
/// `--force` call where dest == source. The fix hoists the check
/// before remove_file in the CLI's pre-flight.
///
/// Asserts:
///   1. The command exits non-zero with a "same file" error.
///   2. The source `solo.db` is unchanged (size + first-byte sanity).
///   3. A subsequent `solo recall` still finds the sentinel memory
///      stored before the failed backup.
#[test]
fn backup_force_with_same_file_dest_does_not_destroy_source() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    let sentinel = format!("backup-same-file-sentinel-{:x}", rand_suffix());
    let out = solo_cmd(data_dir)
        .args(["remember", &sentinel])
        .output()
        .expect("spawn solo remember");
    assert!(
        out.status.success(),
        "solo remember failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Community has exactly one physical Memory Library database at the
    // data-directory root.
    let db_path = data_dir.join("solo.db");
    let pre_size = std::fs::metadata(&db_path)
        .expect("source solo.db exists pre-attack")
        .len();
    let pre_first_kb = {
        let mut buf = [0u8; 1024];
        let mut f = std::fs::File::open(&db_path).expect("open source pre-attack");
        use std::io::Read;
        let n = f.read(&mut buf).unwrap_or(0);
        buf[..n].to_vec()
    };

    // The destructive call: --to points at the live source, --force
    // would (pre-fix) trigger remove_file before the same-file check.
    let out = solo_cmd(data_dir)
        .args(["backup", "--to"])
        .arg(&db_path)
        .arg("--force")
        .output()
        .expect("spawn solo backup --force same-file");
    assert!(
        !out.status.success(),
        "solo backup against same-file MUST refuse but exited 0: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("same file") && stderr.contains("refusing"),
        "expected same-file refusal in stderr; got: {stderr}"
    );

    // Source must be byte-identical to before. Pre-fix, this would
    // be either a deleted file or a 4096-byte fresh SQLite header.
    let post_size = std::fs::metadata(&db_path)
        .expect("source solo.db still exists post-refusal")
        .len();
    assert_eq!(
        pre_size, post_size,
        "source solo.db size changed: was {pre_size}, now {post_size}"
    );
    let post_first_kb = {
        let mut buf = [0u8; 1024];
        let mut f = std::fs::File::open(&db_path).expect("open source post-refusal");
        use std::io::Read;
        let n = f.read(&mut buf).unwrap_or(0);
        buf[..n].to_vec()
    };
    assert_eq!(
        pre_first_kb, post_first_kb,
        "source solo.db first-1KB bytes changed (encrypted-page-level corruption?)"
    );

    // Sanity: the sentinel memory still exists.
    let out = solo_cmd(data_dir)
        .args(["recall", &sentinel])
        .output()
        .expect("spawn solo recall");
    assert!(
        out.status.success(),
        "solo recall failed after refused backup: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&sentinel),
        "sentinel '{sentinel}' missing from recall stdout: {stdout}"
    );
}

#[test]
fn community_backup_restore_round_trip_replaces_the_one_library() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    let backup_path = data_dir.join("community-backup.db");
    let before = format!("restore-before-{:x}", rand_suffix());
    let after = format!("restore-after-{:x}", rand_suffix());

    let init = solo_cmd(data_dir).arg("init").output().expect("spawn init");
    assert!(init.status.success(), "solo init failed: {init:?}");
    let remember_before = solo_cmd(data_dir)
        .args(["remember", &before])
        .output()
        .expect("remember pre-backup sentinel");
    assert!(
        remember_before.status.success(),
        "remember failed: {remember_before:?}"
    );

    let backup = solo_cmd(data_dir)
        .args(["backup", "--to"])
        .arg(&backup_path)
        .output()
        .expect("create backup");
    assert!(
        backup.status.success(),
        "backup failed: stderr={}",
        String::from_utf8_lossy(&backup.stderr)
    );
    assert!(backup_path.is_file(), "backup file was not created");

    let remember_after = solo_cmd(data_dir)
        .args(["remember", &after])
        .output()
        .expect("remember post-backup sentinel");
    assert!(
        remember_after.status.success(),
        "remember failed: {remember_after:?}"
    );

    let restore = solo_cmd(data_dir)
        .args(["restore", "--from"])
        .arg(&backup_path)
        .arg("--confirm")
        .output()
        .expect("restore backup");
    assert!(
        restore.status.success(),
        "restore failed: stderr={}",
        String::from_utf8_lossy(&restore.stderr)
    );

    let recalled_before = solo_cmd(data_dir)
        .args(["recall", &before])
        .output()
        .expect("recall pre-backup sentinel");
    assert!(recalled_before.status.success());
    assert!(String::from_utf8_lossy(&recalled_before.stdout).contains(&before));

    let recalled_after = solo_cmd(data_dir)
        .args(["recall", &after])
        .output()
        .expect("recall post-backup sentinel");
    assert!(recalled_after.status.success());
    assert!(
        !String::from_utf8_lossy(&recalled_after.stdout).contains(&after),
        "post-backup sentinel survived restore"
    );
}

/// v0.10.2 — a single `solo daemon --http-port N` process exposes BOTH
/// the REST graph surface (`/v1/graph/nodes`) AND the MCP JSON-RPC
/// surface (`/mcp`) from the same writer. This pins the cross-surface
/// invariant the v0.10.2 spec was written to unlock: solo-web's graph
/// view and other MCP clients can run against the same data
/// dir concurrently without the single-writer-per-data-dir lock dance.
///
/// Sequence:
///   1. `solo init`.
///   2. Spawn `solo daemon --http-port N`.
///   3. POST a `tools/call` to `/mcp` calling `memory_remember`.
///   4. GET `/v1/graph/nodes?kind=episode` and confirm the just-
///      remembered episode shows up.
///   5. POST a `tools/list` to `/mcp` and confirm the canonical tool set
///      tools come back.
#[test]
fn daemon_serves_mcp_http_and_graph_nodes_from_same_writer() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();

    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(out.status.success(), "solo init failed: {out:?}");

    let port = pick_free_port();
    let mut daemon = solo_cmd(data_dir)
        .args(["daemon", "--http-port", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn solo daemon");

    if !wait_for_http_ready(port, Duration::from_secs(20)) {
        let _ = daemon.kill();
        let _ = daemon.wait();
        panic!("daemon HTTP did not come up within 20s");
    }

    // 1. POST tools/list — must return the 39 canonical tools.
    let url = format!("http://127.0.0.1:{port}/mcp");
    let body = ureq::post(&url)
        .timeout(Duration::from_secs(10))
        .send_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/list",
        }))
        .unwrap_or_else(|e| panic!("POST /mcp tools/list failed: {e}"))
        .into_json::<serde_json::Value>()
        .expect("parse tools/list response");
    let tools = body
        .pointer("/result/tools")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("missing /result/tools: {body}"));
    assert_eq!(
        tools.len(),
        39,
        "expected 39 canonical MCP tools over HTTP; got: {body}"
    );

    // 2. POST tools/call → memory_remember
    let needle = format!("daemon-mcp-http-needle-{:x}", rand_suffix());
    let body = ureq::post(&url)
        .timeout(Duration::from_secs(10))
        .send_json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "memory_remember",
                "arguments": { "content": needle },
            },
        }))
        .unwrap_or_else(|e| panic!("POST /mcp memory_remember failed: {e}"))
        .into_json::<serde_json::Value>()
        .expect("parse memory_remember response");
    let result_text = body
        .pointer("/result/content/0/text")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("missing remember text: {body}"));
    assert!(
        result_text.starts_with("remembered "),
        "expected `remembered <id>`, got: {result_text}"
    );

    // 3. GET /v1/graph/nodes — the MCP write must be visible to the
    //    REST surface (same writer, same Community Memory Library).
    let nodes_url = format!("http://127.0.0.1:{port}/v1/graph/nodes?kind=episode&limit=50");
    let nodes_body: serde_json::Value = ureq::get(&nodes_url)
        .timeout(Duration::from_secs(10))
        .call()
        .unwrap_or_else(|e| panic!("GET /v1/graph/nodes failed: {e}"))
        .into_json()
        .expect("parse graph/nodes response");
    let nodes = nodes_body
        .get("nodes")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("missing nodes: {nodes_body}"));
    let needle_found = nodes.iter().any(|n| {
        let label_hit = n
            .get("label")
            .and_then(|c| c.as_str())
            .is_some_and(|s| s.contains(&needle));
        let preview_hit = n
            .get("preview")
            .and_then(|c| c.as_str())
            .is_some_and(|s| s.contains(&needle));
        label_hit || preview_hit
    });
    assert!(
        needle_found,
        "needle `{needle}` written via /mcp not visible via /v1/graph/nodes: {nodes_body}"
    );

    // Cleanup.
    daemon.kill().expect("kill daemon");
    let _ = daemon.wait();
}
