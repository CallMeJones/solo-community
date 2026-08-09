// SPDX-License-Identifier: Apache-2.0

//! Daemon-side guard for filesystem-reading memory operations.
//!
//! The default policy is unrestricted for backward compatibility. Operators
//! can opt into a root allow-list through `solo.config.toml` or the
//! `SOLO_WORKSPACE_FILE_ROOTS` path-list env var. When restricted, HTTP and
//! MCP document-ingest paths must stay under one of the canonical roots before
//! Solo opens the file.

use std::path::{Path, PathBuf};

use solo_core::{Error, Result};

pub const ENV_WORKSPACE_FILE_ROOTS: &str = "SOLO_WORKSPACE_FILE_ROOTS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceFileAccessPolicy {
    mode: WorkspaceFileAccessMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkspaceFileAccessMode {
    Unrestricted,
    Restricted { allowed_roots: Vec<PathBuf> },
}

impl Default for WorkspaceFileAccessPolicy {
    fn default() -> Self {
        Self::unrestricted()
    }
}

impl WorkspaceFileAccessPolicy {
    pub fn unrestricted() -> Self {
        Self {
            mode: WorkspaceFileAccessMode::Unrestricted,
        }
    }

    pub fn from_config_and_env(config_roots: Option<&[String]>) -> Result<Self> {
        match std::env::var_os(ENV_WORKSPACE_FILE_ROOTS) {
            Some(raw) => {
                let roots = std::env::split_paths(&raw).collect::<Vec<_>>();
                Self::restricted_to_roots(roots)
            }
            None => match config_roots {
                Some(roots) => {
                    Self::restricted_to_roots(roots.iter().map(PathBuf::from).collect::<Vec<_>>())
                }
                None => Ok(Self::unrestricted()),
            },
        }
    }

    pub fn restricted_to_roots(roots: Vec<PathBuf>) -> Result<Self> {
        let mut allowed_roots = roots
            .into_iter()
            .filter(|root| !root.as_os_str().is_empty())
            .map(canonical_workspace_root)
            .collect::<Result<Vec<_>>>()?;
        allowed_roots.sort();
        allowed_roots.dedup();
        Ok(Self {
            mode: WorkspaceFileAccessMode::Restricted { allowed_roots },
        })
    }

    pub fn is_restricted(&self) -> bool {
        matches!(self.mode, WorkspaceFileAccessMode::Restricted { .. })
    }

    pub fn allowed_roots(&self) -> &[PathBuf] {
        match &self.mode {
            WorkspaceFileAccessMode::Unrestricted => &[],
            WorkspaceFileAccessMode::Restricted { allowed_roots } => allowed_roots,
        }
    }

    pub fn check_path(&self, path: &Path) -> Result<PathBuf> {
        match &self.mode {
            WorkspaceFileAccessMode::Unrestricted => canonical_requested_path(path),
            WorkspaceFileAccessMode::Restricted { allowed_roots } => {
                if allowed_roots.is_empty() {
                    return Err(Error::Forbidden(
                        "workspace file access is disabled; configure workspace_file_access.allowed_roots to ingest documents"
                            .to_string(),
                    ));
                }
                let canonical = canonical_requested_path(path)?;
                if allowed_roots
                    .iter()
                    .any(|root| canonical == *root || canonical.starts_with(root))
                {
                    Ok(canonical)
                } else {
                    Err(Error::Forbidden(format!(
                        "path {} is outside workspace_file_access.allowed_roots",
                        path.display()
                    )))
                }
            }
        }
    }
}

fn canonical_workspace_root(path: PathBuf) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(&path).map_err(|e| {
        Error::InvalidInput(format!(
            "workspace_file_access.allowed_roots entry {} is not readable: {e}",
            path.display()
        ))
    })?;
    let metadata = std::fs::metadata(&canonical).map_err(|e| {
        Error::InvalidInput(format!(
            "workspace_file_access.allowed_roots entry {} is not readable: {e}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(Error::InvalidInput(format!(
            "workspace_file_access.allowed_roots entry {} is not a directory",
            path.display()
        )));
    }
    Ok(canonical)
}

fn canonical_requested_path(path: &Path) -> Result<PathBuf> {
    std::fs::canonicalize(path)
        .map_err(|e| Error::InvalidInput(format!("path {} is not readable: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_policy_allows_readable_path() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("note.md");
        std::fs::write(&file, "hello").unwrap();
        let canonical = WorkspaceFileAccessPolicy::unrestricted()
            .check_path(&file)
            .expect("unrestricted policy allows readable file");
        assert!(canonical.ends_with("note.md"));
    }

    #[test]
    fn restricted_policy_allows_child_and_rejects_outside_path() {
        let allowed = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let inside_file = allowed.path().join("note.md");
        let outside_file = outside.path().join("note.md");
        std::fs::write(&inside_file, "inside").unwrap();
        std::fs::write(&outside_file, "outside").unwrap();

        let policy =
            WorkspaceFileAccessPolicy::restricted_to_roots(vec![allowed.path().to_path_buf()])
                .expect("policy");
        policy.check_path(&inside_file).expect("inside allowed");
        let err = policy
            .check_path(&outside_file)
            .expect_err("outside rejected");
        assert!(matches!(err, Error::Forbidden(_)));
    }

    #[test]
    fn restricted_empty_policy_denies_file_reads() {
        let temp = tempfile::TempDir::new().unwrap();
        let file = temp.path().join("note.md");
        std::fs::write(&file, "hello").unwrap();
        let policy = WorkspaceFileAccessPolicy::restricted_to_roots(Vec::new()).unwrap();
        let err = policy.check_path(&file).expect_err("empty roots deny");
        assert!(matches!(err, Error::Forbidden(_)));
    }
}
