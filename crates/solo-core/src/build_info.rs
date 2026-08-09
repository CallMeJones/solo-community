// SPDX-License-Identifier: Apache-2.0

//! Compile-time build identity shared by Solo binaries and transports.
//!
//! `CARGO_PKG_VERSION` remains the release semver. The build metadata here
//! adds the source revision and CI run identity so two binaries built from
//! different commits no longer both present as plain `0.x.y`.

use serde::Serialize;
use std::sync::OnceLock;

static VERSION_WITH_BUILD: OnceLock<String> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildInfo {
    pub version: &'static str,
    pub version_with_build: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_sha_short: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_dirty: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_number: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_attempt: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_ref: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_timestamp: Option<&'static str>,
}

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn git_sha() -> Option<&'static str> {
    non_empty(option_env!("SOLO_BUILD_GIT_SHA"))
}

pub fn git_dirty() -> Option<&'static str> {
    non_empty(option_env!("SOLO_BUILD_GIT_DIRTY"))
}

pub fn build_number() -> Option<&'static str> {
    non_empty(option_env!("SOLO_BUILD_NUMBER"))
}

pub fn build_attempt() -> Option<&'static str> {
    non_empty(option_env!("SOLO_BUILD_ATTEMPT"))
}

pub fn build_ref() -> Option<&'static str> {
    non_empty(option_env!("SOLO_BUILD_REF"))
}

pub fn build_timestamp() -> Option<&'static str> {
    non_empty(option_env!("SOLO_BUILD_TIMESTAMP"))
}

pub fn git_sha_short() -> Option<String> {
    git_sha().map(|sha| sha.chars().take(12).collect())
}

pub fn build_metadata() -> Option<String> {
    let mut parts = Vec::new();
    if let Some(sha) = git_sha_short() {
        parts.push(sanitize_metadata_component(&sha));
    }
    if matches!(git_dirty(), Some("dirty")) {
        parts.push("dirty".to_string());
    }
    if let Some(number) = build_number() {
        parts.push(format!("ci{}", sanitize_metadata_component(number)));
    }
    if let Some(attempt) = build_attempt() {
        parts.push(format!("a{}", sanitize_metadata_component(attempt)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("."))
    }
}

pub fn version_with_build_metadata() -> String {
    match build_metadata() {
        Some(metadata) => format!("{}+{}", version(), metadata),
        None => version().to_string(),
    }
}

pub fn version_with_build_metadata_static() -> &'static str {
    VERSION_WITH_BUILD
        .get_or_init(version_with_build_metadata)
        .as_str()
}

pub fn get() -> BuildInfo {
    BuildInfo {
        version: version(),
        version_with_build: version_with_build_metadata(),
        git_sha: git_sha(),
        git_sha_short: git_sha_short(),
        git_dirty: git_dirty(),
        build_number: build_number(),
        build_attempt: build_attempt(),
        build_ref: build_ref(),
        build_timestamp: build_timestamp(),
    }
}

fn non_empty(value: Option<&'static str>) -> Option<&'static str> {
    value.filter(|s| !s.trim().is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_with_build_keeps_release_semver_prefix() {
        let display = version_with_build_metadata();
        assert!(
            display == version() || display.starts_with(&format!("{}+", version())),
            "build display must preserve the release version prefix: {display}"
        );
    }

    #[test]
    fn metadata_component_is_semver_build_safe() {
        assert_eq!(sanitize_metadata_component("run/123.4"), "run-123-4");
        assert_eq!(sanitize_metadata_component("..."), "unknown");
    }
}
