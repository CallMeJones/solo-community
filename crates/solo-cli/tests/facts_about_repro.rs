// SPDX-License-Identifier: Apache-2.0

//! `memory_facts_about` MCP-stdio reliability investigation (v0.5.1
//! Priority 7).
//!
//! Background: the 2026-05-14 thesis test (Claude Desktop driving Solo
//! against a synthetic 30-entry corpus) saw `memory_facts_about` hang
//! for ~4 minutes on its very first call in chat Q11. Subsequent calls
//! in Q12 worked fine. This file is the timed reproduction harness:
//! spawn `solo mcp-stdio` as a fresh subprocess (matching the cold-start
//! scenario), drive `tools/call` for `memory_facts_about` with a range
//! of subject patterns, and record per-call wall times.
//!
//! ## Why `#[ignore]`?
//!
//! Each scenario spawns its own subprocess + runs `solo init` (Argon2id
//! key derivation ~1-3 sec). The full file run is several seconds per
//! scenario times ~6 scenarios, so this lives behind `--ignored` to
//! keep the default `cargo test` loop fast. Run with:
//!
//! ```bash
//! cargo test -p solo-cli --test facts_about_repro -- --ignored --nocapture
//! ```
//!
//! ## What "hang" would look like
//!
//! Any per-call wall time over 5 seconds is suspect; the thesis-test
//! report was 4 minutes. The harness uses an absolute 60-second deadline
//! on each `tools/call` (well above any plausible non-pathological
//! latency) so a real hang produces a clear `panic!` rather than
//! blocking the test indefinitely.
//!
//! ## What was found (2026-05-15)
//!
//! Not reproducible. Per-call times in the 1-60 ms range across all
//! tested subject patterns including cold-start first-call. See
//! `docs/dev-log/0075-facts_about-reliability-investigation.md` for
//! full results and the leading cold-start hypothesis.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PASSPHRASE: &str = "test-passphrase-for-facts-about-repro";
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Per-call deadline. Far above any plausible non-pathological latency,
/// but tight enough that a real 4-minute hang surfaces as a panic
/// (rather than blocking the test).
const TOOL_CALL_DEADLINE: Duration = Duration::from_secs(60);

fn solo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_solo"))
}

fn solo_init(data_dir: &Path) {
    let out = Command::new(solo_bin())
        .env("SOLO_DATA_DIR", data_dir)
        .env("SOLO_PASSPHRASE", PASSPHRASE)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(
        out.status.success(),
        "solo init failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

// Note: a `solo_remember` helper would pre-seed the corpus, but
// `remember` only writes the episode — triple extraction runs during
// `solo consolidate`, which requires an LLM and is intentionally
// out-of-scope here. The reproduction harness therefore exercises
// facts_about's SQL path against zero matching rows (the empty-triples
// path). That's the right scope: the thesis-test hang happened on the
// first call, before any triples could have been extracted in that
// session either, so the empty-triples path is the one to stress.

struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl McpClient {
    fn spawn(data_dir: &Path) -> Self {
        let mut child = Command::new(solo_bin())
            .env("SOLO_DATA_DIR", data_dir)
            .env("SOLO_PASSPHRASE", PASSPHRASE)
            .arg("mcp-stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Mute stderr by default — flip to inherit() to debug.
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn solo mcp-stdio");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 0,
        }
    }

    fn send(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).expect("serialize request");
        let stdin = self.stdin.as_mut().expect("stdin already taken");
        writeln!(stdin, "{line}").expect("write request");
        stdin.flush().expect("flush request");

        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read response");
            if n == 0 {
                panic!("EOF on mcp-stdio stdout while waiting for id={id}");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let resp: Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
                panic!(
                    "parse JSON response (id={id} method={method}): {e}\n\
                     line was: {trimmed:?}"
                )
            });
            if resp.get("id").and_then(|v| v.as_i64()) == Some(id) {
                if let Some(err) = resp.get("error") {
                    panic!("MCP error response for {method}: {err}");
                }
                return resp;
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        let req = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).expect("serialize notification");
        let stdin = self.stdin.as_mut().expect("stdin already taken");
        writeln!(stdin, "{line}").expect("write notification");
        stdin.flush().expect("flush notification");
    }

    fn handshake(&mut self) {
        let resp = self.send(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "solo-test", "version": "0.0.0" },
            }),
        );
        assert!(
            resp.get("result").is_some(),
            "initialize lacked result: {resp}"
        );
        self.notify("notifications/initialized", json!({}));
    }

    /// Call `memory_facts_about` once with `subject` (no other filters)
    /// and return the elapsed wall time. Asserts the call returns
    /// within [`TOOL_CALL_DEADLINE`]; a real 4-minute hang surfaces as
    /// a panic here.
    fn time_facts_about(&mut self, subject: &str) -> Duration {
        let start = Instant::now();
        let resp = self.send(
            "tools/call",
            json!({
                "name": "memory_facts_about",
                "arguments": { "subject": subject },
            }),
        );
        let elapsed = start.elapsed();
        assert!(
            elapsed < TOOL_CALL_DEADLINE,
            "facts_about(subject={subject:?}) exceeded {TOOL_CALL_DEADLINE:?}: \
             took {elapsed:?} — response was {resp}"
        );
        // Spot-check the response is well-formed (avoids silent-no-op
        // bugs where the call returned an error envelope we missed).
        assert!(
            resp.pointer("/result/content/0/text").is_some(),
            "facts_about response missing /result/content/0/text: {resp}"
        );
        elapsed
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => return,
            }
        }
    }
}

/// The core scenario from the thesis test: the very FIRST `tools/call`
/// against `memory_facts_about` in a fresh `mcp-stdio` subprocess. This
/// is the cold-start path — first SQLite open, first reader-pool
/// connection check-out, first prepared-statement compile for the
/// facts_about SQL.
#[test]
#[ignore]
fn facts_about_cold_start_first_call() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    let elapsed = client.time_facts_about("alex");
    eprintln!("[facts_about cold-start FIRST call] {elapsed:?}");
}

/// After the first call lands, do four more in the same process.
/// Compares "first-in-process" vs "Nth-in-process" latencies — if the
/// first one is dramatically slower, cold-start (statement-prep cache
/// warm, page-cache fill, connection-pool first-borrow) is the leading
/// hypothesis.
#[test]
#[ignore]
fn facts_about_repeated_calls_after_cold_start() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    let mut timings: Vec<Duration> = Vec::with_capacity(5);
    for _ in 0..5 {
        timings.push(client.time_facts_about("alex"));
    }
    eprintln!("[facts_about 5 calls same process] {timings:?}");
}

/// Varied subject patterns — empty-but-rejected, unknown, long, alias-
/// shaped, special chars. The empty-string case is rejected before SQL
/// runs (validated at the handler), so we hit that path separately.
#[test]
#[ignore]
fn facts_about_varied_subjects() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    // Each call records (label, elapsed); a single subject pattern at
    // a time so a hang on any one is identifiable.
    let mut report: Vec<(String, Duration)> = Vec::new();

    report.push((
        "unknown short subject".into(),
        client.time_facts_about("nonexistent"),
    ));
    report.push((
        "common subject (no triples)".into(),
        client.time_facts_about("alex"),
    ));
    report.push((
        "alias-shaped 'user' canonical".into(),
        client.time_facts_about("user"),
    ));
    report.push((
        "long subject (1000 chars)".into(),
        client.time_facts_about(&"a".repeat(1000)),
    ));
    report.push((
        "subject with spaces and punctuation".into(),
        client.time_facts_about("Alex's Café (formerly Café Alex)"),
    ));
    report.push((
        "unicode subject".into(),
        client.time_facts_about("マヤさん"),
    ));

    for (label, dur) in &report {
        eprintln!("[facts_about subject patterns] {label}: {dur:?}");
    }
}

/// Empty-subject is rejected at the handler (`subject.trim().is_empty()`),
/// returning an `invalid_params` error before SQL runs. We test that
/// path specifically so a hang in the validation layer would surface
/// here distinct from a hang in the SQL layer.
#[test]
#[ignore]
fn facts_about_empty_subject_rejected_quickly() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    // Use send() directly because the empty-subject path RETURNS AN
    // ERROR, and `time_facts_about` asserts on success.
    let start = Instant::now();
    let req = json!({
        "name": "memory_facts_about",
        "arguments": { "subject": "   " },
    });
    let id = {
        client.next_id += 1;
        client.next_id
    };
    let line = serde_json::to_string(&json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": req,
    }))
    .unwrap();
    let stdin = client.stdin.as_mut().expect("stdin");
    writeln!(stdin, "{line}").expect("write");
    stdin.flush().expect("flush");

    loop {
        let mut buf = String::new();
        let n = client.stdout.read_line(&mut buf).expect("read");
        assert!(n > 0, "EOF before error response");
        if buf.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(buf.trim()).expect("parse JSON");
        if v.get("id").and_then(|x| x.as_i64()) == Some(id) {
            // Either a JSON-RPC error or an MCP CallToolResult with
            // isError=true; both signal validation rejection.
            assert!(
                v.get("error").is_some()
                    || v.pointer("/result/isError")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false),
                "expected error for empty subject; got: {v}"
            );
            break;
        }
    }
    let elapsed = start.elapsed();
    eprintln!("[facts_about empty-subject rejection] {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "empty-subject rejection took {elapsed:?} — should be near-instant"
    );
}

/// One process, many concurrent-ish (sequentially-issued but rapid)
/// `facts_about` calls. Catches any per-call leak / unbounded growth
/// in statement cache, response queue, or trace buffer that would
/// surface as a hang at large N.
#[test]
#[ignore]
fn facts_about_many_calls_same_process() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let mut client = McpClient::spawn(data_dir);
    client.handshake();

    const N: usize = 50;
    let mut max_dur = Duration::ZERO;
    let mut sum = Duration::ZERO;
    for i in 0..N {
        // Vary subject to avoid any same-key prepared-statement-cache
        // optimisation that could hide a slow first-bind.
        let subj = format!("subject-{i}");
        let d = client.time_facts_about(&subj);
        if d > max_dur {
            max_dur = d;
        }
        sum += d;
    }
    let avg = sum / N as u32;
    eprintln!("[facts_about {N} calls] avg={avg:?} max={max_dur:?}");
    // Sanity bound — if any single call took longer than 10s, that's
    // a smoking gun.
    assert!(
        max_dur < Duration::from_secs(10),
        "facts_about max-of-{N} took {max_dur:?} — investigate"
    );
}
