// SPDX-License-Identifier: Apache-2.0
//! Exercise the real installer and hook process without touching user settings.

use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};
use wiremock::matchers::{body_partial_json, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn solo(home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_solo"));
    cmd.env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("CODEX_HOME", home.join(".codex"))
        .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
        .env_remove("SOLO_DATA_DIR")
        .env_remove("SOLO_BEARER_TOKEN");
    cmd
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn setup_roundtrip_for_every_supported_agent() {
    for client in ["claude-code", "codex", "gemini", "cursor", "copilot"] {
        let home = tempfile::tempdir().unwrap();
        let dry_run = solo(home.path())
            .args(["setup-client", client, "--dry-run"])
            .output()
            .unwrap();
        success(&dry_run);
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 0);
        let apply = solo(home.path())
            .args(["setup-client", client, "--apply"])
            .output()
            .unwrap();
        success(&apply);
        let repeated = solo(home.path())
            .args(["setup-client", client, "--apply"])
            .output()
            .unwrap();
        success(&repeated);
        let text = String::from_utf8_lossy(&repeated.stdout);
        assert!(text.contains("Unchanged:"), "{client}: {text}");
        assert!(!text.contains("Update:"), "{client}: {text}");
        let verify = solo(home.path())
            .args(["setup-client", "verify", client, "--format", "json"])
            .output()
            .unwrap();
        success(&verify);
        let report: Value = serde_json::from_slice(&verify.stdout).unwrap();
        assert!(
            report.to_string().contains("\"status\":\"installed\""),
            "{client}: {report}"
        );
        let remove = solo(home.path())
            .args(["setup-client", client, "--apply", "--no-hooks"])
            .output()
            .unwrap();
        success(&remove);
        let verify = solo(home.path())
            .args(["setup-client", "verify", client, "--format", "json"])
            .output()
            .unwrap();
        success(&verify);
        let report: Value = serde_json::from_slice(&verify.stdout).unwrap();
        assert!(
            report.to_string().contains("\"status\":\"missing\""),
            "{client}: {report}"
        );
    }
}

fn hook(input: &[u8], url: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_solo"))
        .args([
            "agent-hook",
            "--client",
            "claude-code",
            "--event",
            "prompt",
            "--url",
            url,
            "--timeout-ms",
            "300",
        ])
        .env_remove("SOLO_BEARER_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn failures_are_protocol_clean_and_do_not_block_the_prompt() {
    for input in [b"not json".as_slice(), br#"{"prompt":"private prompt"}"#] {
        let output = hook(input, "http://127.0.0.1:1/mcp");
        success(&output);
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            json!({})
        );
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(!errors.contains("private prompt"));
        assert!(!errors.contains("127.0.0.1"));
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_solo"))
        .args([
            "agent-hook",
            "--client",
            "codex",
            "--event",
            "session-start",
            "--url",
            "http://127.0.0.1:1/mcp",
            "--timeout-ms",
            "100",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Keep stdin open: an async timeout must still let the process terminate.
    let input = child.stdin.take().unwrap();
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            panic!("hook hung waiting for stdin");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(input);
    success(&child.wait_with_output().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_command_delivers_unicode_context_using_mcp() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method":"initialize"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":1,"result":{"protocolVersion":"2025-03-26"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            json!({"method":"notifications/initialized"}),
        ))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(body_partial_json(json!({"method":"tools/call"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id":2,"result":{"structuredContent":{"recall":{"hits":[{"content":"Préférer 日本語 — confirmed preference","memory_id":"evidence-1"}]}}}})))
        .mount(&server).await;
    let home = tempfile::tempdir().unwrap();
    success(
        &solo(home.path())
            .args(["setup-client", "gemini", "--url", &server.uri(), "--apply"])
            .output()
            .unwrap(),
    );
    let config: Value =
        serde_json::from_slice(&std::fs::read(home.path().join(".gemini/settings.json")).unwrap())
            .unwrap();
    let command = config["hooks"]["BeforeAgent"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let mut shell = if cfg!(windows) {
        let mut c = Command::new("cmd.exe");
        c.args(["/D", "/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let mut child = shell
        .env_remove("SOLO_BEARER_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(br#"{"hook_event_name":"BeforeAgent","prompt":"Preferences?","cwd":"/example/project"}"#).unwrap();
    let output = child.wait_with_output().unwrap();
    success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["hookSpecificOutput"]["hookEventName"], "BeforeAgent");
    let context = value["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        context.contains("Préférer 日本語 — confirmed preference"),
        "{context}"
    );
    assert!(context.contains("evidence-1"));
}
