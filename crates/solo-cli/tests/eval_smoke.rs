// SPDX-License-Identifier: Apache-2.0

//! Smoke tests for the offline eval harness CLI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn solo_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_solo"))
}

fn run_cmd(args: &[&str]) -> Output {
    let mut cmd = Command::new(solo_bin());
    for arg in args {
        cmd.arg(arg);
    }
    cmd.output().expect("spawn solo")
}

fn run_cmd_with_path(prefix: &[&str], path: &Path, suffix: &[&str]) -> Output {
    let mut cmd = Command::new(solo_bin());
    for arg in prefix {
        cmd.arg(arg);
    }
    cmd.arg(path);
    for arg in suffix {
        cmd.arg(arg);
    }
    cmd.output().expect("spawn solo")
}

fn run_cmd_with_report_dir(prefix: &[&str], report_dir: &Path, suffix: &[&str]) -> Output {
    let mut cmd = Command::new(solo_bin());
    for arg in prefix {
        cmd.arg(arg);
    }
    cmd.arg("--report-dir");
    cmd.arg(report_dir);
    for arg in suffix {
        cmd.arg(arg);
    }
    cmd.output().expect("spawn solo")
}

#[test]
fn eval_list_shows_bundled_fixtures() {
    let out = run_cmd(&["eval", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "solo eval list failed: stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("memory-baseline"),
        "list should include memory-baseline: {stdout}"
    );
    assert!(
        stdout.contains("memory-corrections"),
        "list should include memory-corrections: {stdout}"
    );
}

#[test]
fn production_retrieval_corpus_is_versioned_and_covers_hard_cases() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/corpora/retrieval-v1.json");
    let raw = std::fs::read_to_string(&path).expect("read retrieval corpus");
    let corpus: serde_json::Value = serde_json::from_str(&raw).expect("valid corpus JSON");
    assert_eq!(corpus["version"], 1);
    assert_eq!(corpus["model_baseline"], "bundled:all-MiniLM-L6-v2@v2");
    let cases = corpus["cases"].as_array().expect("cases array");
    assert!(
        cases.len() >= 8,
        "retrieval corpus should span multiple failure modes"
    );
    let categories = cases
        .iter()
        .filter_map(|case| case["category"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    for expected in [
        "semantic_paraphrase",
        "lexical_rescue",
        "entity_ambiguity",
        "correction",
        "negation",
    ] {
        assert!(categories.contains(expected), "missing category {expected}");
    }
}

#[test]
fn eval_run_json_scores_bundled_fixture() {
    let out = run_cmd(&["eval", "run", "memory-baseline", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "solo eval run failed: stdout={stdout}\nstderr={stderr}"
    );

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON output");
    assert_eq!(json["fixture"], "memory-baseline");
    assert_eq!(json["passed"], true);
    assert_eq!(json["case_count"], 3);
    assert_eq!(json["cases"][0]["passed"], true);
    assert_eq!(
        json["cases"][2]["forbidden_memory_ids"][0],
        "jordan-city-trip"
    );
    assert_eq!(json["cases"][2]["forbidden_ranked"], serde_json::json!([]));
    assert!(
        json["cases"][0]["top_results"].as_array().unwrap().len() <= 3,
        "top_results should respect fixture top_k: {stdout}"
    );
}

#[test]
fn eval_run_all_json_scores_bundled_fixtures() {
    let out = run_cmd(&["eval", "run", "--all", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "solo eval run --all failed: stdout={stdout}\nstderr={stderr}"
    );

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON output");
    assert_eq!(json["suite"], "bundled");
    assert_eq!(json["passed"], true);
    assert_eq!(json["fixture_count"], 2);
    assert_eq!(json["case_count"], 5);
    assert_eq!(json["fixtures"][0]["selector"], "memory-baseline");
    assert_eq!(json["fixtures"][1]["selector"], "memory-corrections");
    assert_eq!(
        json["fixtures"][1]["cases"][0]["forbidden_memory_ids"][0],
        "invoice-storage-old"
    );
}

#[test]
fn eval_run_save_writes_report_and_report_reads_it() {
    let tmp = tempfile::TempDir::new().expect("tempdir");

    let out = run_cmd_with_report_dir(
        &["eval", "run", "memory-baseline", "--json", "--save"],
        tmp.path(),
        &[],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "solo eval run --save failed: stdout={stdout}\nstderr={stderr}"
    );
    let saved: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON output");
    let run_id = saved["run_id"].as_str().expect("run_id");
    let report_path = saved["report_path"].as_str().expect("report_path");
    assert!(Path::new(report_path).is_file(), "report file should exist");
    assert_eq!(saved["fixture"], "memory-baseline");
    assert_eq!(saved["report_kind"], "fixture");

    let loaded = run_cmd_with_report_dir(&["eval", "report", run_id, "--json"], tmp.path(), &[]);
    let loaded_stdout = String::from_utf8_lossy(&loaded.stdout);
    let loaded_stderr = String::from_utf8_lossy(&loaded.stderr);
    assert!(
        loaded.status.success(),
        "solo eval report failed: stdout={loaded_stdout}\nstderr={loaded_stderr}"
    );
    let loaded_json: serde_json::Value =
        serde_json::from_slice(&loaded.stdout).expect("valid JSON output");
    assert_eq!(loaded_json["run_id"], run_id);
    assert_eq!(loaded_json["fixture"], "memory-baseline");

    let loaded_by_path = run_cmd_with_path(&["eval", "report"], Path::new(report_path), &[]);
    let human = String::from_utf8_lossy(&loaded_by_path.stdout);
    let human_stderr = String::from_utf8_lossy(&loaded_by_path.stderr);
    assert!(
        loaded_by_path.status.success(),
        "solo eval report path failed: stdout={human}\nstderr={human_stderr}"
    );
    assert!(
        human.contains(run_id),
        "human report should include run id: {human}"
    );
    assert!(
        human.contains("memory-baseline"),
        "human report should include fixture name: {human}"
    );
}

#[test]
fn eval_run_all_can_save_suite_report() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let out = run_cmd_with_report_dir(
        &["eval", "run", "--all", "--json", "--save"],
        tmp.path(),
        &[],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "solo eval run --all --save failed: stdout={stdout}\nstderr={stderr}"
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON output");
    assert_eq!(json["suite"], "bundled");
    assert_eq!(json["report_kind"], "suite");
    assert!(json["report_path"].as_str().unwrap().ends_with(".json"));
}

#[test]
fn eval_run_accepts_fixture_file_path() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let fixture_path = tmp.path().join("custom-eval.json");
    std::fs::write(
        &fixture_path,
        r#"{
          "name": "custom-eval",
          "description": "custom path fixture",
          "cases": [{
            "id": "custom-case",
            "query": "what channel should launch reviews use",
            "expected_memory_ids": ["launch-channel"],
            "memories": [
              {
                "id": "launch-channel",
                "text": "Launch reviews should use the stable channel.",
                "tier": "decision",
                "importance": 0.9,
                "status": "active"
              },
              {
                "id": "random-note",
                "text": "The planning note mentions a lunch channel in chat.",
                "tier": "episodic",
                "importance": 0.2,
                "status": "active"
              }
            ]
          }]
        }"#,
    )
    .expect("write fixture");

    let out = run_cmd_with_path(&["eval", "run"], &fixture_path, &["--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        out.status.success(),
        "solo eval run path failed: stdout={stdout}\nstderr={stderr}"
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON output");
    assert_eq!(json["fixture"], "custom-eval");
    assert_eq!(json["passed"], true);
}

#[test]
fn eval_run_requires_fixture_or_all() {
    let out = run_cmd(&["eval", "run"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "empty eval run should fail");
    assert!(
        stderr.contains("provide a fixture name/path or pass --all"),
        "stderr should name the required selector: {stderr}"
    );
}

#[test]
fn eval_run_unknown_fixture_fails_helpfully() {
    let out = run_cmd(&["eval", "run", "not-a-fixture"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "unknown fixture should fail");
    assert!(
        stderr.contains("unknown eval fixture"),
        "stderr should name the problem: {stderr}"
    );
}

#[test]
fn eval_report_unknown_run_id_fails_helpfully() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let out = run_cmd_with_report_dir(&["eval", "report", "missing-run"], tmp.path(), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "unknown report should fail");
    assert!(
        stderr.contains("eval report `missing-run` not found"),
        "stderr should name the missing report: {stderr}"
    );
}
