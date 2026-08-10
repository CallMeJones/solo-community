use std::collections::{HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FORBIDDEN_RELEASE_TERMS: &[&str] = &[
    "/v1/tenants",
    "/v1/settings/relay",
    "x-solo-tenant",
    "--tenant",
    "backup-tenant",
    "restore-tenant",
    "relay_public",
    concat!("jar", "vis"),
];

fn assert_release_text_is_community_only(surface: &str, text: &str) {
    let lowercase = text.to_ascii_lowercase();
    for forbidden in FORBIDDEN_RELEASE_TERMS {
        assert!(
            !lowercase.contains(forbidden),
            "Community {surface} contains forbidden paid/legacy term {forbidden:?}"
        );
    }
}

fn command_help(path: &[String]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_solo"));
    command.args(path).arg("--help");
    let output = command.output().expect("run solo help");
    assert!(
        output.status.success(),
        "solo {} --help failed: {}",
        path.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("CLI help must be UTF-8")
}

fn direct_subcommands(help: &str) -> Vec<String> {
    let mut in_commands = false;
    let mut commands = Vec::new();
    for line in help.lines() {
        if line.trim() == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() {
            if !commands.is_empty() {
                break;
            }
            continue;
        }
        if !line.starts_with("  ") {
            break;
        }
        if let Some(name) = line.split_whitespace().next()
            && name != "help"
        {
            commands.push(name.to_string());
        }
    }
    commands
}

#[test]
fn every_cli_help_surface_is_community_only() {
    let mut queue = VecDeque::from([Vec::<String>::new()]);
    let mut visited = HashSet::<Vec<String>>::new();

    while let Some(path) = queue.pop_front() {
        if !visited.insert(path.clone()) {
            continue;
        }
        let help = command_help(&path);
        let label = if path.is_empty() {
            "root".to_string()
        } else {
            path.join(" ")
        };
        assert_release_text_is_community_only(&format!("CLI help ({label})"), &help);

        for child in direct_subcommands(&help) {
            let mut child_path = path.clone();
            child_path.push(child);
            queue.push_back(child_path);
        }
    }
}

#[test]
fn openapi_has_no_paid_or_database_routing_surface() {
    let spec = solo_api::openapi_spec();
    let text = serde_json::to_string(&spec).expect("serialize OpenAPI");
    assert_release_text_is_community_only("OpenAPI", &text);
    assert!(spec["paths"].get("/v1/tenants").is_none());
    assert!(spec["paths"].get("/v1/settings/relay").is_none());
    assert_eq!(
        spec["components"]["schemas"]["GraphNode"]["required"],
        serde_json::json!(["id", "kind", "label"])
    );
    assert!(
        spec["components"]["schemas"]["GraphNode"]["properties"]
            .get("tenant_id")
            .is_none()
    );
}

fn collect_files(root: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).unwrap_or_else(|error| {
        panic!(
            "read Community surface directory {}: {error}",
            root.display()
        )
    }) {
        let path = entry.expect("read Community surface entry").path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

fn assert_paths_are_community_only(repo_root: &Path, paths: &[&str]) {
    let mut files = Vec::new();
    for relative in paths {
        let path = repo_root.join(relative);
        if path.is_dir() {
            collect_files(&path, &mut files);
        } else {
            assert!(
                path.is_file(),
                "Community surface must exist: {}",
                path.display()
            );
            files.push(path);
        }
    }

    for path in files {
        let bytes = fs::read(&path).expect("read Community surface");
        let text = String::from_utf8_lossy(&bytes);
        assert_release_text_is_community_only(&path.display().to_string(), &text);
    }
}

#[test]
fn current_documentation_sdks_examples_and_smokes_are_community_only() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    assert_paths_are_community_only(
        &repo_root,
        &[
            "README.md",
            "apps/web/README.md",
            "apps/web/package.json",
            "apps/web/src",
            "apps/web/tests",
            "apps/web/scripts",
            "apps/web/e2e",
            "docs/book/src",
            "docs/editions.md",
            "examples",
            "sdks/README.md",
            "sdks/typescript/README.md",
            "sdks/typescript/package.json",
            "sdks/typescript/solo-client.js",
            "sdks/typescript/solo-client.d.ts",
            "sdks/python/README.md",
            "sdks/python/solo_client.py",
            "scripts/windows_mcp_client_smoke.ps1",
            "scripts/repro_document_upload_contract.ps1",
        ],
    );
}

#[test]
fn community_web_has_one_monorepo_owner() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    assert!(
        repo_root.join("apps/web/package-lock.json").is_file(),
        "Community Web source must live under apps/web"
    );

    let provenance: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(repo_root.join("crates/solo-api/assets/solo-web.provenance.json"))
            .expect("read embedded Web provenance"),
    )
    .expect("parse embedded Web provenance");
    assert_eq!(provenance["schema_version"], 3);
    assert_eq!(
        provenance["source_repository"],
        "CallMeJones/solo-community"
    );
    assert_eq!(provenance["source_path"], "apps/web");

    for relative in [
        "scripts/sync_solo_web_assets.ps1",
        "scripts/verify_embedded_web.mjs",
        ".github/workflows/ci.yml",
        ".github/workflows/pilot-release.yml",
        ".github/workflows/linux-test-release.yml",
        ".github/workflows/publish.yml",
    ] {
        let text = fs::read_to_string(repo_root.join(relative)).expect("read Web owner file");
        assert!(
            !text.contains("CallMeJones/solo-web-community")
                && !text.contains("solo_web_commit")
                && !text.contains("upstream/solo-web"),
            "Community Web still has a second repository owner in {relative}"
        );
    }
}

#[test]
fn embedded_web_release_is_community_only() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("solo-api")
        .join("assets")
        .join("solo-web");
    let mut files = Vec::new();
    collect_files(&root, &mut files);
    assert!(!files.is_empty(), "embedded Solo Web assets must exist");

    for path in files {
        let bytes = fs::read(&path).expect("read embedded Web asset");
        let text = String::from_utf8_lossy(&bytes);
        assert_release_text_is_community_only(&format!("Web asset {}", path.display()), &text);
    }
}

#[test]
fn community_storage_layout_has_one_database() {
    let temp = tempfile::tempdir().expect("temporary data directory");
    let data_dir: OsString = temp.path().as_os_str().to_owned();
    let output = Command::new(env!("CARGO_BIN_EXE_solo"))
        .args(["init", "--data-dir"])
        .arg(data_dir)
        .env("SOLO_PASSPHRASE", "community-boundary-test")
        .output()
        .expect("run solo init");
    assert!(
        output.status.success(),
        "solo init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(temp.path().join("solo.db").is_file());
    assert!(!temp.path().join("tenants").exists());
    assert!(!temp.path().join("tenants_index.db").exists());
}

#[test]
fn ubuntu_package_declares_the_tray_xkb_runtime() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let package_script = fs::read_to_string(repo_root.join("scripts/package_ubuntu_deb.sh"))
        .expect("read Ubuntu package script");
    assert!(
        package_script.contains("libxkbcommon-x11-0"),
        "Ubuntu package must install the runtime library loaded by the tray"
    );

    let release_workflow =
        fs::read_to_string(repo_root.join(".github/workflows/linux-test-release.yml"))
            .expect("read Linux release workflow");
    assert!(
        release_workflow.contains("/usr/bin/solo-tray"),
        "Linux release workflow must launch the installed tray binary"
    );
    assert!(
        release_workflow.contains("linux_tray_gui_smoke.sh"),
        "Linux release workflow must exercise the installed tray under a virtual desktop"
    );
}

#[test]
fn embedding_model_fetch_retries_transient_failures_on_both_platforms() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let shell = fs::read_to_string(repo_root.join("scripts/fetch_embedding_model.sh"))
        .expect("read POSIX model fetch helper");
    assert!(shell.contains("--retry 8"));
    assert!(shell.contains("--retry-all-errors"));
    assert!(shell.contains("--retry-max-time 600"));

    let powershell = fs::read_to_string(repo_root.join("scripts/fetch_embedding_model.ps1"))
        .expect("read PowerShell model fetch helper");
    assert!(powershell.contains("$maxAttempts = 8"));
    assert!(powershell.contains("Start-Sleep -Seconds $delaySeconds"));
}
