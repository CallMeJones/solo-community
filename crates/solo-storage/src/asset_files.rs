//! Content-addressed asset paths and file staging. Transaction ownership stays in the writer.

use solo_core::{Error, Result};
use std::path::{Component, Path, PathBuf};

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::storage(format!("open asset source {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| Error::storage(format!("read asset source {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub(crate) fn normalize_sha256_hex(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid_input(
            "sha256 must be 64 lowercase or uppercase hex characters",
        ));
    }
    Ok(value)
}

pub(crate) fn asset_blob_paths(snapshot_dir: &Path, sha256: &str) -> Result<(String, PathBuf)> {
    let sha256 = normalize_sha256_hex(sha256)?;
    let prefix = &sha256[..2];
    let storage_path = format!("assets/blobs/{prefix}/{sha256}");
    let final_path = snapshot_dir.join(&storage_path);
    Ok((storage_path, final_path))
}

pub(crate) fn stage_asset_blob_bytes(bytes: &[u8], final_path: &Path) -> Result<PathBuf> {
    let parent = final_path
        .parent()
        .ok_or_else(|| Error::storage("asset final path has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| Error::storage(format!("create asset blob dir {}: {e}", parent.display())))?;
    let tmp = final_path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7().as_simple()));
    std::fs::write(&tmp, bytes)
        .map_err(|e| Error::storage(format!("write asset blob {}: {e}", tmp.display())))?;
    Ok(tmp)
}

pub(crate) fn promote_staged_asset_blob(
    staged_path: &Path,
    final_path: &Path,
    replace_existing: bool,
) -> Result<()> {
    if final_path.is_file() {
        if !replace_existing {
            return Err(Error::storage(format!(
                "asset blob appeared before metadata commit: {}",
                final_path.display()
            )));
        }
        std::fs::remove_file(final_path).map_err(|e| {
            Error::storage(format!(
                "remove stale asset blob {} before promote: {e}",
                final_path.display()
            ))
        })?;
    }
    std::fs::rename(staged_path, final_path).map_err(|e| {
        Error::storage(format!(
            "promote asset blob {} -> {}: {e}",
            staged_path.display(),
            final_path.display()
        ))
    })
}

pub(crate) fn cleanup_failed_staged_asset_blob(
    staged_path: &Path,
    final_path: &Path,
    promoted: bool,
) {
    let _ = std::fs::remove_file(staged_path);
    if promoted {
        let _ = std::fs::remove_file(final_path);
    }
}

pub(crate) fn safe_asset_storage_path(snapshot_dir: &Path, storage_path: &str) -> Result<PathBuf> {
    let rel = Path::new(storage_path);
    if rel.is_absolute() {
        return Err(Error::storage(format!(
            "asset storage_path must be relative: {storage_path:?}"
        )));
    }
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_str().ok_or_else(|| {
                    Error::storage(format!(
                        "asset storage_path component must be UTF-8: {storage_path:?}"
                    ))
                })?;
                parts.push(part);
            }
            _ => {
                return Err(Error::storage(format!(
                    "asset storage_path must contain only normal relative components: {storage_path:?}"
                )));
            }
        }
    }
    if parts.len() != 4 || parts[0] != "assets" || parts[1] != "blobs" {
        return Err(Error::storage(format!(
            "asset storage_path must use assets/blobs/<prefix>/<sha256>: {storage_path:?}"
        )));
    }
    let sha256 = normalize_sha256_hex(parts[3])?;
    if parts[2].len() != 2 || parts[2] != &sha256[..2] {
        return Err(Error::storage(format!(
            "asset storage_path prefix must match sha256: {storage_path:?}"
        )));
    }
    Ok(snapshot_dir.join(rel))
}

pub(crate) fn remove_empty_asset_blob_parent(blob_path: &Path) {
    let Some(parent) = blob_path.parent() else {
        return;
    };
    let _ = std::fs::remove_dir(parent);
}
