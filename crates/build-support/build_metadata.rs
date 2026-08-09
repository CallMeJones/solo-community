// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildMetadata {
    pub git_sha: Option<String>,
    pub git_dirty: String,
    pub build_number: Option<String>,
    pub build_attempt: Option<String>,
    pub build_ref: Option<String>,
    pub build_timestamp: String,
}

pub fn repo_root_from_manifest_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    manifest_dir.join("../..")
}

pub fn emit_rerun_instructions(repo_root: &Path) {
    println!("cargo:rerun-if-env-changed=GITHUB_REF");
    println!("cargo:rerun-if-env-changed=GITHUB_REF_NAME");
    println!("cargo:rerun-if-env-changed=GITHUB_RUN_ATTEMPT");
    println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=SOLO_BUILD_NUMBER");
    println!("cargo:rerun-if-env-changed=SOLO_BUILD_DIRTY");
    println!("cargo:rerun-if-env-changed=SOLO_BUILD_TIMESTAMP");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let dot_git = repo_root.join(".git");
    if dot_git.is_file() {
        println!("cargo:rerun-if-changed={}", dot_git.display());
    }
    let Some(git_dir) = resolve_git_dir(repo_root) else {
        return;
    };
    let common_git_dir = resolve_common_git_dir(&git_dir);
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("config.worktree").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        common_git_dir.join("config").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        common_git_dir.join("packed-refs").display()
    );

    let head = match std::fs::read_to_string(git_dir.join("HEAD")) {
        Ok(head) => head,
        Err(_) => return,
    };
    let Some(ref_name) = head.trim().strip_prefix("ref: ") else {
        return;
    };
    let worktree_ref = git_dir.join(ref_name);
    let ref_path = if worktree_ref.exists() {
        worktree_ref
    } else {
        common_git_dir.join(ref_name)
    };
    println!("cargo:rerun-if-changed={}", ref_path.display());
}

fn resolve_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    if dot_git.is_dir() {
        return Some(dot_git);
    }

    let pointer = std::fs::read_to_string(dot_git).ok()?;
    let path = PathBuf::from(pointer.trim().strip_prefix("gitdir:")?.trim());
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}

fn resolve_common_git_dir(git_dir: &Path) -> PathBuf {
    let Some(path) = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|value| PathBuf::from(value.trim()))
        .filter(|value| !value.as_os_str().is_empty())
    else {
        return git_dir.to_path_buf();
    };
    if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    }
}

pub fn collect(repo_root: &Path) -> BuildMetadata {
    let git_sha = non_empty_env("GITHUB_SHA").or_else(|| git(repo_root, &["rev-parse", "HEAD"]));
    let git_dirty = match non_empty_env("SOLO_BUILD_DIRTY") {
        Some(value) => value,
        None => match git_output(repo_root, &["status", "--porcelain"]) {
            Some(status) if !status.trim().is_empty() => "dirty".to_string(),
            Some(_) => "clean".to_string(),
            None => "unknown".to_string(),
        },
    };
    let build_number =
        non_empty_env("GITHUB_RUN_NUMBER").or_else(|| non_empty_env("SOLO_BUILD_NUMBER"));
    let build_attempt = non_empty_env("GITHUB_RUN_ATTEMPT");
    let build_ref = non_empty_env("GITHUB_REF_NAME")
        .or_else(|| {
            non_empty_env("GITHUB_REF").map(|value| {
                value
                    .trim_start_matches("refs/heads/")
                    .trim_start_matches("refs/tags/")
                    .to_string()
            })
        })
        .or_else(|| git(repo_root, &["rev-parse", "--abbrev-ref", "HEAD"]));
    let build_timestamp = non_empty_env("SOLO_BUILD_TIMESTAMP")
        .or_else(|| non_empty_env("SOURCE_DATE_EPOCH"))
        .unwrap_or_else(current_unix_timestamp);

    BuildMetadata {
        git_sha,
        git_dirty,
        build_number,
        build_attempt,
        build_ref,
        build_timestamp,
    }
}

pub fn version_with_build_metadata(metadata: &BuildMetadata) -> String {
    match build_metadata(metadata) {
        Some(build_metadata) => format!("{}+{}", release_version(), build_metadata),
        None => release_version(),
    }
}

pub fn git_sha_short(metadata: &BuildMetadata) -> Option<String> {
    metadata
        .git_sha
        .as_ref()
        .map(|sha| sha.chars().take(12).collect())
}

pub fn windows_resource_comment(
    product: &str,
    version_with_build: &str,
    metadata: &BuildMetadata,
) -> String {
    let mut comment = format!("{product} build {version_with_build}");
    if let Some(sha) = &metadata.git_sha {
        comment.push_str(&format!("; commit {sha}"));
    }
    comment.push_str(&format!("; state {}", metadata.git_dirty));
    if let Some(build_ref) = &metadata.build_ref {
        comment.push_str(&format!("; ref {build_ref}"));
    }
    if let Some(build_number) = &metadata.build_number {
        comment.push_str(&format!("; ci {build_number}"));
    }
    comment
}

fn build_metadata(metadata: &BuildMetadata) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(sha) = git_sha_short(metadata) {
        parts.push(sanitize_metadata_component(&sha));
    }
    if metadata.git_dirty == "dirty" {
        parts.push("dirty".to_string());
    }
    if let Some(number) = &metadata.build_number {
        parts.push(format!("ci{}", sanitize_metadata_component(number)));
    }
    if let Some(attempt) = &metadata.build_attempt {
        parts.push(format!("a{}", sanitize_metadata_component(attempt)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

fn release_version() -> String {
    env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string())
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn git(repo_root: &Path, args: &[&str]) -> Option<String> {
    git_output(repo_root, args).filter(|value| !value.is_empty())
}

fn git_output(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    Some(value.trim().to_string())
}

fn sanitize_metadata_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

fn current_unix_timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_gitdir_pointer_used_by_submodules() {
        let unique = format!(
            "solo-build-metadata-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let expected = root.join("git-data").join("worktree");
        std::fs::create_dir_all(&expected).unwrap();
        std::fs::write(root.join(".git"), "gitdir: git-data/worktree\n").unwrap();

        assert_eq!(resolve_git_dir(&root), Some(expected));

        std::fs::remove_dir_all(root).unwrap();
    }
}
