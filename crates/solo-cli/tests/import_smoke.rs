// SPDX-License-Identifier: Apache-2.0

//! Smoke tests for `solo import ... --dry-run` safety contracts.

use std::path::Path;
use std::process::Command;

const PASSPHRASE: &str = "test-passphrase-for-import-smoke";

fn solo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_solo"))
}

fn solo_cmd(data_dir: &Path) -> Command {
    let mut cmd = solo_bin();
    cmd.env("SOLO_DATA_DIR", data_dir);
    cmd.env("SOLO_PASSPHRASE", PASSPHRASE);
    cmd
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

fn solo_init(data_dir: &Path) {
    let out = solo_cmd(data_dir)
        .arg("init")
        .output()
        .expect("spawn solo init");
    assert!(
        out.status.success(),
        "solo init failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    force_stub_embedder(data_dir);
}

fn run_import(data_dir: &Path, args: &[&str]) -> String {
    let out = solo_cmd(data_dir)
        .args(args)
        .output()
        .expect("run solo import command");
    assert!(
        out.status.success(),
        "solo import command failed: args={args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn assert_import_summary(stdout: &str, expected: &str) {
    assert!(
        stdout.contains(expected),
        "stdout did not contain {expected:?}:\n{stdout}"
    );
}

#[test]
fn markdown_import_dry_run_does_not_open_or_create_data_dir() {
    let source = tempfile::tempdir().expect("source tempdir");
    std::fs::write(
        source.path().join("notes.md"),
        "# Notes\n\nImporter dry-run candidate.\n",
    )
    .expect("write markdown");
    let missing_data_dir = source.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("import")
        .arg("markdown")
        .arg(source.path())
        .arg("--dry-run")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo import markdown --dry-run");

    assert!(
        out.status.success(),
        "dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("import markdown --dry-run"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("candidate files: 1"), "stdout={stdout}");
    assert!(
        !missing_data_dir.exists(),
        "dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn markdown_import_dry_run_json_reports_counts_without_opening_data_dir() {
    let source = tempfile::tempdir().expect("source tempdir");
    std::fs::write(
        source.path().join("notes.md"),
        "# Notes\n\nImporter JSON.\n",
    )
    .expect("write markdown");
    let missing_data_dir = source.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("import")
        .arg("markdown")
        .arg(source.path())
        .arg("--dry-run")
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo import markdown --dry-run --json");

    assert!(
        out.status.success(),
        "json dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid markdown import JSON");
    assert_eq!(json["command"], "import markdown");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["candidate_files"], 1);
    assert_eq!(json["skipped"]["unsupported_extension"], 0);
    assert!(
        !missing_data_dir.exists(),
        "json dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn chatgpt_import_dry_run_json_reports_records_without_opening_data_dir() {
    let export = tempfile::tempdir().expect("export tempdir");
    let conversations = serde_json::json!([
        {
            "id": "conv-1",
            "title": "Solo import",
            "messages": [
                { "role": "user", "content": "What should ship?" },
                { "role": "assistant", "content": "Structured dry-run JSON." }
            ]
        }
    ]);
    std::fs::write(
        export.path().join("conversations.json"),
        serde_json::to_string(&conversations).expect("serialize fixture"),
    )
    .expect("write ChatGPT export");
    let missing_data_dir = export.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("import")
        .arg("chatgpt")
        .arg(export.path())
        .arg("--dry-run")
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo import chatgpt --dry-run --json");

    assert!(
        out.status.success(),
        "schema json dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid ChatGPT import JSON");
    assert_eq!(json["command"], "import chatgpt");
    assert_eq!(json["source"], "ChatGPT");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["records_scanned"], 1);
    assert_eq!(json["candidate_records"], 1);
    assert_eq!(json["materialized_format"], "markdown");
    assert!(
        !missing_data_dir.exists(),
        "schema json dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn text_import_dry_run_json_reports_counts_without_opening_data_dir() {
    let source = tempfile::tempdir().expect("source tempdir");
    std::fs::write(source.path().join("notes.txt"), "Plain text fixture.\n").expect("write text");
    std::fs::write(source.path().join("skip.bin"), "not importable").expect("write binary");
    let missing_data_dir = source.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("import")
        .arg("text")
        .arg(source.path())
        .arg("--dry-run")
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo import text --dry-run --json");

    assert!(
        out.status.success(),
        "text json dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid text import JSON");
    assert_eq!(json["command"], "import text");
    assert_eq!(json["source"], "text");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["candidate_files"], 1);
    assert_eq!(json["skipped"]["unsupported_extension"], 1);
    assert!(
        !missing_data_dir.exists(),
        "text json dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn json_import_dry_run_json_reports_counts_without_opening_data_dir() {
    let source = tempfile::tempdir().expect("source tempdir");
    std::fs::write(
        source.path().join("memory.json"),
        serde_json::json!({ "note": "JSON fixture" }).to_string(),
    )
    .expect("write json");
    std::fs::write(source.path().join("events.ndjson"), "{\"event\":\"one\"}\n")
        .expect("write ndjson");
    std::fs::write(source.path().join("skip.md"), "# Not JSON\n").expect("write markdown");
    let missing_data_dir = source.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("import")
        .arg("json")
        .arg(source.path())
        .arg("--dry-run")
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo import json --dry-run --json");

    assert!(
        out.status.success(),
        "json import dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid json import JSON");
    assert_eq!(json["command"], "import json");
    assert_eq!(json["source"], "json");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["candidate_files"], 2);
    assert_eq!(json["skipped"]["unsupported_extension"], 1);
    assert!(
        json["enabled_extensions"]
            .as_array()
            .expect("enabled extensions")
            .iter()
            .any(|value| value.as_str() == Some("ndjson"))
    );
    assert!(
        !missing_data_dir.exists(),
        "json import dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn claude_import_dry_run_json_reports_records_without_opening_data_dir() {
    let export = tempfile::tempdir().expect("export tempdir");
    let conversations = serde_json::json!({
        "conversations": [
            {
                "uuid": "claude-1",
                "name": "Solo Claude export",
                "created_at": "2026-05-29T00:00:00Z",
                "chat_messages": [
                    { "sender": "human", "text": "What should the importer prove?" },
                    { "sender": "assistant", "text": "Schema-aware Claude records." }
                ]
            }
        ]
    });
    std::fs::write(
        export.path().join("conversations.json"),
        serde_json::to_string(&conversations).expect("serialize fixture"),
    )
    .expect("write Claude export");
    let missing_data_dir = export.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("import")
        .arg("claude")
        .arg(export.path())
        .arg("--dry-run")
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo import claude --dry-run --json");

    assert!(
        out.status.success(),
        "Claude schema dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid Claude import JSON");
    assert_eq!(json["command"], "import claude");
    assert_eq!(json["source"], "Claude");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["records_scanned"], 1);
    assert_eq!(json["candidate_records"], 1);
    assert_eq!(json["materialized_format"], "markdown");
    assert!(
        !missing_data_dir.exists(),
        "Claude schema dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn bookmarks_import_dry_run_json_reports_records_without_opening_data_dir() {
    let export = tempfile::tempdir().expect("export tempdir");
    let bookmarks = export.path().join("bookmarks.html");
    std::fs::write(
        &bookmarks,
        r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
  <DT><A HREF="https://example.com/solo" ADD_DATE="1715625610">Solo docs</A>
</DL><p>
"#,
    )
    .expect("write bookmarks export");
    let missing_data_dir = export.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("import")
        .arg("bookmarks")
        .arg(&bookmarks)
        .arg("--dry-run")
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo import bookmarks --dry-run --json");

    assert!(
        out.status.success(),
        "bookmarks schema dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid bookmarks import JSON");
    assert_eq!(json["command"], "import bookmarks");
    assert_eq!(json["source"], "Bookmarks");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["records_scanned"], 1);
    assert_eq!(json["candidate_records"], 1);
    assert_eq!(json["materialized_format"], "markdown");
    assert!(
        !missing_data_dir.exists(),
        "bookmarks schema dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn import_sources_are_idempotent_when_run_twice() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data_dir = tmp.path().join("solo-data");
    let fixtures = tmp.path().join("fixtures");
    std::fs::create_dir_all(&fixtures).expect("create fixtures dir");
    solo_init(&data_dir);

    let markdown_dir = fixtures.join("markdown");
    std::fs::create_dir_all(&markdown_dir).expect("create markdown dir");
    std::fs::write(
        markdown_dir.join("notes.md"),
        "# Import Markdown\n\nThis markdown file should dedupe.\n",
    )
    .expect("write markdown");

    let text_dir = fixtures.join("text");
    std::fs::create_dir_all(&text_dir).expect("create text dir");
    std::fs::write(
        text_dir.join("notes.txt"),
        "This plain text file should dedupe.\n",
    )
    .expect("write text");

    let json_dir = fixtures.join("json");
    std::fs::create_dir_all(&json_dir).expect("create json dir");
    std::fs::write(
        json_dir.join("memory.json"),
        serde_json::json!({ "kind": "fixture", "should": "dedupe" }).to_string(),
    )
    .expect("write json");

    let chatgpt_dir = fixtures.join("chatgpt");
    std::fs::create_dir_all(&chatgpt_dir).expect("create chatgpt dir");
    let chatgpt = serde_json::json!([
        {
            "id": "chatgpt-dedupe",
            "title": "ChatGPT dedupe",
            "messages": [
                { "role": "user", "content": "Can this import twice?" },
                { "role": "assistant", "content": "The second run should dedupe." }
            ]
        }
    ]);
    std::fs::write(
        chatgpt_dir.join("conversations.json"),
        serde_json::to_string(&chatgpt).expect("serialize chatgpt fixture"),
    )
    .expect("write chatgpt export");

    let claude_dir = fixtures.join("claude");
    std::fs::create_dir_all(&claude_dir).expect("create claude dir");
    let claude = serde_json::json!({
        "conversations": [
            {
                "uuid": "claude-dedupe",
                "name": "Claude dedupe",
                "chat_messages": [
                    { "sender": "human", "text": "Can Claude import twice?" },
                    { "sender": "assistant", "text": "The second run should dedupe." }
                ]
            }
        ]
    });
    std::fs::write(
        claude_dir.join("conversations.json"),
        serde_json::to_string(&claude).expect("serialize claude fixture"),
    )
    .expect("write claude export");

    let bookmarks = fixtures.join("bookmarks.html");
    std::fs::write(
        &bookmarks,
        r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<DL><p>
  <DT><A HREF="https://example.com/solo-import-idempotent">Solo import idempotent</A>
</DL><p>
"#,
    )
    .expect("write bookmarks export");

    let cases = [
        ("markdown", markdown_dir),
        ("text", text_dir),
        ("json", json_dir),
        ("chatgpt", chatgpt_dir),
        ("claude", claude_dir),
        ("bookmarks", bookmarks),
    ];

    for (source, path) in cases {
        let path = path.display().to_string();
        let first = run_import(&data_dir, &["import", source, &path]);
        assert_import_summary(&first, "Summary: imported 1 new, 0 deduped, 0 failed");

        let second = run_import(&data_dir, &["import", source, &path]);
        assert_import_summary(&second, "Summary: imported 0 new, 1 deduped, 0 failed");
    }
}
