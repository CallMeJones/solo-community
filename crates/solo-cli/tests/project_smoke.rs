// SPDX-License-Identifier: Apache-2.0

//! Smoke tests for `solo project ...` command-line safety contracts.

use std::path::Path;
use std::process::Command;

const PASSPHRASE: &str = "test-passphrase-for-project-smoke";

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

fn canonical_display(path: &Path) -> String {
    let path = std::fs::canonicalize(path).expect("canonicalize path");
    strip_windows_verbatim_prefix(&path.display().to_string())
}

fn strip_windows_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    path.strip_prefix(r"\\?\").unwrap_or(path).to_string()
}

#[test]
fn project_ingest_dry_run_does_not_open_or_create_data_dir() {
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::write(
        project.path().join("README.md"),
        "# Test Project\n\nProject docs candidate.\n",
    )
    .expect("write readme");
    let missing_data_dir = project.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("project")
        .arg("ingest")
        .arg(project.path())
        .arg("--dry-run")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo project ingest --dry-run");

    assert!(
        out.status.success(),
        "dry-run should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("project ingest --dry-run"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("candidate: README.md"), "stdout={stdout}");
    assert!(
        !missing_data_dir.exists(),
        "dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn project_ingest_json_reports_shape_without_opening_data_dir() {
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::create_dir_all(project.path().join(".solo")).expect("create project config dir");
    std::fs::write(
        project.path().join(".solo").join("project.toml"),
        r#"
schema_version = 1

[project]
name = "Project Docs Fixture"
id = "project-docs-fixture"
root = "."
tags = []
ignore_dirs = ["target", "node_modules"]
"#,
    )
    .expect("write project config");
    std::fs::write(project.path().join("README.md"), "# Test Project\n").expect("write readme");
    std::fs::create_dir_all(project.path().join("docs")).expect("create docs dir");
    std::fs::write(project.path().join("docs").join("plan.md"), "# Plan\n")
        .expect("write docs plan");
    let missing_data_dir = project.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("project")
        .arg("ingest")
        .arg(project.path())
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo project ingest --json");

    assert!(
        out.status.success(),
        "--json should be a dry run that does not require an initialized database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid project ingest dry-run JSON");
    assert_eq!(json["command"], "project ingest");
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["root"], canonical_display(project.path()));
    assert_eq!(json["project_name"], "Project Docs Fixture");
    assert_eq!(json["project_id"], "project-docs-fixture");
    assert_eq!(json["project"]["name"], "Project Docs Fixture");
    assert_eq!(json["project"]["id"], "project-docs-fixture");
    assert_eq!(json["project"]["root"], canonical_display(project.path()));
    assert_eq!(json["files_scanned"], 2);
    assert_eq!(json["candidates_found"], 2);
    assert_eq!(json["counts"]["files_scanned"], 2);
    assert_eq!(json["counts"]["candidate_files"], 2);
    assert_eq!(json["counts"]["skipped_files"], 0);
    assert_eq!(json["counts"]["skipped_ignored_dirs"], 1);
    assert_eq!(json["counts"]["truncated"], false);
    assert_eq!(
        json["candidate_paths"],
        serde_json::json!(["README.md", "docs/plan.md"])
    );
    assert_eq!(
        json["candidates"],
        serde_json::json!([
            {
                "path": canonical_display(&project.path().join("README.md")),
                "relative_path": "README.md"
            },
            {
                "path": canonical_display(&project.path().join("docs").join("plan.md")),
                "relative_path": "docs/plan.md"
            }
        ])
    );
    assert!(
        !missing_data_dir.exists(),
        "--json dry-run should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn project_policy_json_reports_repo_scoped_agent_instructions_without_opening_data_dir() {
    let project = tempfile::tempdir().expect("project tempdir");
    std::fs::create_dir_all(project.path().join(".solo")).expect("create project config dir");
    std::fs::write(
        project.path().join(".solo").join("project.toml"),
        r#"
schema_version = 1

[project]
name = "Policy Fixture"
id = "policy-fixture"
root = "."
tags = ["memory", "desktop"]
ignore_dirs = ["target", "node_modules"]
"#,
    )
    .expect("write project config");
    let missing_data_dir = project.path().join("missing-solo-data");

    let out = solo_bin()
        .arg("project")
        .arg("policy")
        .arg(project.path())
        .arg("--client")
        .arg("codex")
        .arg("--json")
        .env("SOLO_DATA_DIR", &missing_data_dir)
        .env_remove("SOLO_PASSPHRASE")
        .output()
        .expect("run solo project policy --json");

    assert!(
        out.status.success(),
        "project policy should not require an initialized encrypted database: stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("valid project policy JSON");
    assert_eq!(json["command"], "project policy");
    assert_eq!(json["client"], "codex");
    assert_eq!(json["project"]["name"], "Policy Fixture");
    assert_eq!(json["project"]["id"], "policy-fixture");
    assert_eq!(json["project"]["root"], canonical_display(project.path()));
    assert_eq!(
        json["project"]["tags"],
        serde_json::json!(["memory", "desktop"])
    );
    let policy = json["policy"].as_str().expect("policy text");
    assert!(policy.contains("Solo Project Memory Policy - Codex"));
    assert!(policy.contains("Project id: policy-fixture"));
    assert!(policy.contains("Include the project name and project id in memory queries."));
    assert!(
        !missing_data_dir.exists(),
        "project policy should return before creating or opening SOLO_DATA_DIR"
    );
}

#[test]
fn project_facts_and_decisions_json_are_agent_consumable() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let project = tmp.path().join("repo");
    let data_dir = tmp.path().join("solo-data");
    std::fs::create_dir_all(&project).expect("create project root");
    solo_init(&data_dir);

    let init = solo_bin()
        .arg("project")
        .arg("init")
        .arg(&project)
        .arg("--name")
        .arg("Project JSON Fixture")
        .arg("--id")
        .arg("project-json-fixture")
        .output()
        .expect("run solo project init");
    assert!(
        init.status.success(),
        "project init failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&init.stdout),
        String::from_utf8_lossy(&init.stderr)
    );

    let facts = solo_cmd(&data_dir)
        .arg("project")
        .arg("facts")
        .arg(&project)
        .arg("--json")
        .output()
        .expect("run solo project facts --json");
    assert!(
        facts.status.success(),
        "project facts --json failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&facts.stdout),
        String::from_utf8_lossy(&facts.stderr)
    );
    let facts_json: serde_json::Value =
        serde_json::from_slice(&facts.stdout).expect("valid project facts JSON");
    assert_eq!(facts_json["command"], "project facts");
    assert_eq!(facts_json["project"]["id"], "project-json-fixture");
    assert_eq!(facts_json["subject"], "Project JSON Fixture");
    assert_eq!(facts_json["facts"], serde_json::json!([]));

    let add = solo_cmd(&data_dir)
        .arg("project")
        .arg("decisions")
        .arg(&project)
        .arg("--add")
        .arg("Use ADR files for architecture decisions.")
        .arg("--json")
        .output()
        .expect("run solo project decisions --add --json");
    assert!(
        add.status.success(),
        "project decisions --add --json failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    let add_json: serde_json::Value =
        serde_json::from_slice(&add.stdout).expect("valid project decision add JSON");
    assert_eq!(add_json["command"], "project decisions");
    assert_eq!(add_json["action"], "add");
    assert_eq!(add_json["project"]["id"], "project-json-fixture");
    assert_eq!(add_json["source_type"], "project_decision");
    assert!(add_json["memory_id"].as_str().unwrap().len() > 20);
    assert!(
        add_json["source_id"]
            .as_str()
            .unwrap()
            .starts_with("project:project-json-fixture:decision:")
    );

    let query = solo_cmd(&data_dir)
        .arg("project")
        .arg("decisions")
        .arg(&project)
        .arg("--query")
        .arg("architecture decisions")
        .arg("--json")
        .output()
        .expect("run solo project decisions --query --json");
    assert!(
        query.status.success(),
        "project decisions --query --json failed: stdout={}\nstderr={}",
        String::from_utf8_lossy(&query.stdout),
        String::from_utf8_lossy(&query.stderr)
    );
    let query_json: serde_json::Value =
        serde_json::from_slice(&query.stdout).expect("valid project decision query JSON");
    assert_eq!(query_json["command"], "project decisions");
    assert_eq!(query_json["action"], "query");
    assert_eq!(query_json["project"]["id"], "project-json-fixture");
    assert_eq!(query_json["hits"][0]["source_type"], "project_decision");
    assert!(
        query_json["hits"][0]["content"]
            .as_str()
            .unwrap()
            .contains("Use ADR files for architecture decisions.")
    );
}
