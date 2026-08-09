// SPDX-License-Identifier: Apache-2.0

//! End-to-end smoke tests for the v0.7.0 P7a `solo ingest` and
//! `solo documents {list, inspect, forget}` subcommands.
//!
//! Spawns the real `solo` binary against a tempdir data dir so the
//! lockfile + Argon2id + SQLCipher + writer-actor + read-pool stack
//! all participate. The stub embedder is fine — these tests assert
//! command-line surface (exit codes, stdout shape), not embedding
//! quality.
//!
//! ## Cost
//!
//! Each test runs a fresh `solo init` (Argon2id, ~1-3 sec) plus one
//! or more one-shot CLI invocations (each is another Argon2id derive).
//! Total file run: ~10-20 sec on Windows. They live in `tests/` to keep
//! the `--lib` loop fast — same pattern as `process_lifecycle.rs`.
//!
//! ## Why subprocess-based
//!
//! `solo ingest` and `solo documents forget` write through the writer
//! actor, which spawns a dedicated thread (per ADR-0003). In-process
//! tests would have to mock that out; spawning the real binary against
//! a tempdir is simpler and covers the actual exit-code / lockfile /
//! shutdown path that matters at the user surface.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PASSPHRASE: &str = "test-passphrase-for-documents-smoke";

fn solo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_solo"))
}

/// Build a `Command` for `solo` with the test's data dir + passphrase
/// already set. Caller adds the subcommand + args.
fn solo_cmd(data_dir: &Path) -> Command {
    let mut c = Command::new(solo_bin());
    c.env("SOLO_DATA_DIR", data_dir);
    c.env("SOLO_PASSPHRASE", PASSPHRASE);
    c
}

fn force_stub_embedder(data_dir: &Path) {
    let path = data_dir.join("solo.config.toml");
    let existing = std::fs::read_to_string(&path).expect("read solo.config.toml");
    let salt_hex = existing
        .lines()
        .find_map(|line| line.trim().strip_prefix("salt_hex = "))
        .expect("extract salt_hex from solo.config.toml");
    std::fs::write(
        &path,
        format!(
            "schema_version = 1\n\
             salt_hex = {salt_hex}\n\n\
             [embedder]\n\
             name = \"stub\"\n\
             version = \"v1\"\n\
             dim = 32\n\
             dtype = \"f32\"\n\n\
             [llm]\n\
             mode = \"none\"\n"
        ),
    )
    .expect("write stub solo.config.toml");
}

/// Run `solo init` synchronously; panic with stderr on non-zero exit.
fn solo_init(data_dir: &Path) {
    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(
        out.status.success(),
        "solo init failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    force_stub_embedder(data_dir);
}

/// Run a subcommand and return the captured output. Caller decides
/// whether to assert on the exit status — these tests intentionally
/// check both success and failure cases.
fn run_cmd(data_dir: &Path, args: &[&str]) -> Output {
    let mut cmd = solo_cmd(data_dir);
    for a in args {
        cmd.arg(a);
    }
    cmd.output().expect("spawn solo subcommand")
}

fn write_fixture(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(&path, body).expect("write fixture");
    path
}

/// Body large enough that the chunker produces at least one chunk
/// reliably. The chunker's lower bound is target_tokens / 4 ~= 125
/// chars, so ~400 chars is safely above the threshold for default
/// config.
fn doc_body(needle: &str) -> String {
    let lead = "Lorem ipsum dolor sit amet, consectetur adipiscing elit. \
                Sed do eiusmod tempor incididunt ut labore et dolore magna \
                aliqua. Ut enim ad minim veniam, quis nostrud exercitation \
                ullamco laboris nisi ut aliquip ex ea commodo consequat.";
    format!("{lead}\n\nNeedle: {needle}\n\n{lead}\n")
}

#[test]
fn ingest_single_file_succeeds() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let fixture = write_fixture(data_dir, "doc.md", &doc_body("alpha"));
    let out = run_cmd(data_dir, &["ingest", &fixture.to_string_lossy()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "solo ingest failed: stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("✓ ingested"),
        "stdout missing ingest line: {stdout}"
    );
    assert!(
        stdout.contains("chunks"),
        "stdout missing chunks count: {stdout}"
    );
}

#[test]
fn ingest_dir_walks_recursively_and_skips_unsupported() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    // Layout:
    //   data_dir/in/top.md       (allowed)
    //   data_dir/in/sub/deep.txt (allowed)
    //   data_dir/in/skip.bin     (NOT in default allowed_extensions)
    let in_dir = data_dir.join("in");
    write_fixture(&in_dir, "top.md", &doc_body("topneedle"));
    write_fixture(&in_dir, "sub/deep.txt", &doc_body("deepneedle"));
    write_fixture(&in_dir, "skip.bin", "binary-ish content here");

    let out = run_cmd(data_dir, &["ingest", "--dir", &in_dir.to_string_lossy()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "solo ingest --dir failed: stdout={stdout}\nstderr={stderr}"
    );
    // Two ingest lines, one summary.
    let ingest_lines = stdout.matches("✓ ingested").count();
    assert_eq!(
        ingest_lines, 2,
        "expected 2 ingest lines (md+txt); .bin must be skipped. stdout={stdout}"
    );
    assert!(
        stdout.contains("Summary: ingested 2 new"),
        "stdout missing summary: {stdout}"
    );
    assert!(
        !stdout.contains("skip.bin"),
        "stdout must not mention skip.bin: {stdout}"
    );
}

#[test]
fn ingest_dir_with_no_matching_files_prints_helpful_message() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    // Empty allowed-extensions dir.
    let empty = data_dir.join("empty_dir");
    std::fs::create_dir(&empty).expect("mkdir");
    write_fixture(&empty, "ignored.bin", "x");

    let out = run_cmd(data_dir, &["ingest", "--dir", &empty.to_string_lossy()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "should exit 0 even with no matches: {stdout}"
    );
    assert!(
        stdout.contains("no files under"),
        "expected helpful empty-matches message: {stdout}"
    );
}

#[test]
fn ingest_lockfile_contention_errors_clearly() {
    use std::fs::OpenOptions;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    // Manually grab solo.lock. We're not the real daemon — just a
    // file-system holder — but the lockfile protocol is file-existence
    // + PID-alive check, so any process holding the file with our PID
    // counts. We write our own PID so the recovery path doesn't
    // declare us dead.
    let lock_path = data_dir.join("solo.lock");
    let mut lock_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock_path)
        .expect("create solo.lock manually");
    use std::io::Write;
    write!(lock_file, "{}\n", std::process::id()).expect("write pid");
    lock_file.sync_all().expect("sync");
    drop(lock_file);

    let fixture = write_fixture(data_dir, "doc.md", &doc_body("contended"));
    let out = run_cmd(data_dir, &["ingest", &fixture.to_string_lossy()]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "expected non-zero exit when lockfile is held: stderr={stderr}"
    );
    // Either the lockfile-acquire path or the recovery path errors;
    // both surface a message containing "lock" so the user knows what
    // happened.
    assert!(
        stderr.to_ascii_lowercase().contains("lock")
            || stderr.to_ascii_lowercase().contains("running"),
        "stderr must mention the lock contention: {stderr}"
    );

    // Clean up so tempdir drop succeeds.
    std::fs::remove_file(&lock_path).ok();
}

#[test]
fn documents_list_returns_active_docs() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let f1 = write_fixture(data_dir, "a.md", &doc_body("doc-a-needle"));
    let f2 = write_fixture(data_dir, "b.md", &doc_body("doc-b-needle"));
    let _ = run_cmd(data_dir, &["ingest", &f1.to_string_lossy()]);
    let _ = run_cmd(data_dir, &["ingest", &f2.to_string_lossy()]);

    let out = run_cmd(data_dir, &["documents", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "documents list failed: {stdout}");
    // Header + 2 data rows. The header line has "id", "title", "chunks".
    assert!(stdout.contains("chunks"), "stdout missing header: {stdout}");
    // Each ingested file's name (no title was set; title may be the
    // first heading parsed by the chunker, but the fixture doesn't
    // have one, so titles are likely None → "(no title)").
    // Two data rows = two newlines after the dashes header. Just count
    // "active" status occurrences.
    let active_rows = stdout.matches("active").count();
    assert!(
        active_rows >= 2,
        "expected at least 2 active rows in list output: {stdout}"
    );
}

#[test]
fn documents_list_pagination_via_limit_and_offset() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    for i in 0..3 {
        let path = write_fixture(data_dir, &format!("d{i}.md"), &doc_body(&format!("n{i}")));
        let _ = run_cmd(data_dir, &["ingest", &path.to_string_lossy()]);
    }

    // limit=1 — exactly one data row in stdout. Header lines are fixed,
    // so total line count is header(2) + 1 = 3 non-blank lines (plus a
    // possible trailing newline).
    let out = run_cmd(data_dir, &["documents", "list", "--limit", "1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "list --limit 1 failed: {stdout}");
    let data_rows = stdout.matches("active").count();
    assert_eq!(data_rows, 1, "expected 1 active row at limit=1: {stdout}");

    // offset=10 — past the end, should print "(no documents in this page)".
    let out = run_cmd(data_dir, &["documents", "list", "--offset", "10"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "list --offset 10 failed: {stdout}");
    assert!(
        stdout.contains("no documents"),
        "expected empty-page message at offset=10: {stdout}"
    );
}

#[test]
fn documents_inspect_returns_doc_and_chunks() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let f = write_fixture(data_dir, "doc.md", &doc_body("inspecting"));
    let ingest_out = run_cmd(data_dir, &["ingest", &f.to_string_lossy()]);
    let ingest_stdout = String::from_utf8_lossy(&ingest_out.stdout);
    // Pull the short doc_id (first 8 chars after "→ ").
    let short_id = ingest_stdout
        .split("→ ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or_else(|| panic!("could not extract short doc_id from: {ingest_stdout}"));
    assert_eq!(
        short_id.len(),
        8,
        "expected 8-char short id, got {short_id:?}"
    );

    let out = run_cmd(data_dir, &["documents", "inspect", short_id]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "inspect failed: {stdout}");
    assert!(
        stdout.contains("doc_id"),
        "inspect output missing doc_id: {stdout}"
    );
    assert!(
        stdout.contains("chunk_count"),
        "inspect missing chunk_count: {stdout}"
    );
    assert!(
        stdout.contains("chunk "),
        "inspect missing chunk listing: {stdout}"
    );
}

#[test]
fn documents_inspect_unknown_doc_id_errors_exit_1() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let out = run_cmd(data_dir, &["documents", "inspect", "deadbeefdeadbeef"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "inspect unknown id must fail: stderr={stderr}"
    );
    assert!(
        stderr.to_ascii_lowercase().contains("no document matches")
            || stderr.to_ascii_lowercase().contains("not found"),
        "stderr must explain not-found: {stderr}"
    );
}

#[test]
fn documents_forget_soft_deletes_and_include_forgotten_shows() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    let f = write_fixture(data_dir, "doc.md", &doc_body("forgetting"));
    let ingest_out = run_cmd(data_dir, &["ingest", &f.to_string_lossy()]);
    let ingest_stdout = String::from_utf8_lossy(&ingest_out.stdout);
    let short_id = ingest_stdout
        .split("→ ")
        .nth(1)
        .and_then(|s| s.split_whitespace().next())
        .expect("extract short id from ingest output");

    // Forget it.
    let out = run_cmd(
        data_dir,
        &["documents", "forget", short_id, "--reason", "test"],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "forget failed: {stdout}");
    assert!(stdout.contains("✓ forgotten"), "forget output: {stdout}");

    // Default list (active-only) should now show 0 active docs.
    let out = run_cmd(data_dir, &["documents", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    let active_rows = stdout.matches("active").count();
    assert_eq!(
        active_rows, 0,
        "active list should be empty after forget: {stdout}"
    );

    // --include-forgotten should show 1 forgotten row.
    let out = run_cmd(data_dir, &["documents", "list", "--include-forgotten"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(
        stdout.contains("forgotten"),
        "include-forgotten should show forgotten status: {stdout}"
    );
}

#[test]
fn documents_forget_unknown_doc_id_errors_exit_1() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let data_dir = tmp.path();
    solo_init(data_dir);

    // No documents ingested → any prefix is "not found".
    let out = run_cmd(data_dir, &["documents", "forget", "feedfacefeedface"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "forget unknown id must fail: stderr={stderr}"
    );
    assert!(
        stderr.to_ascii_lowercase().contains("no document matches"),
        "stderr must explain not-found: {stderr}"
    );
}
