// SPDX-License-Identifier: Apache-2.0

//! HNSW snapshot save/load. ADR-0003 §P8-C: hnsw_rs writes a pair of files
//! (`*.hnsw.data` + `*.hnsw.graph`); we drive an atomic two-step save with
//! `fsync` and a previous-version backup.
//!
//! ## File layout
//!
//! Snapshots live in a directory the caller owns (typically the data dir
//! holding `solo.db`). Three basenames coexist; for each, hnsw_rs produces
//! `{basename}.hnsw.data` + `{basename}.hnsw.graph`.
//!
//!   - `hnsw_episodes`      — the live snapshot (loaded on startup)
//!   - `hnsw_episodes_bak`  — the previous successful snapshot
//!   - `hnsw_episodes_tmp`  — in-flight write (renamed to live on success)
//!
//! Note: ADR-0003 §P8-C wrote suffixes as `.hnsw.data.bak`, but hnsw_rs's
//! `HnswIo` requires the `.hnsw.data`/`.hnsw.graph` suffix to load. Using a
//! parallel basename (`_bak`) lets us reload the backup with the same loader
//! that handles the live snapshot — no temp-rename dance on the recovery
//! path. Behaviour is identical (a separate filename for the previous
//! version).
//!
//! ## Save sequence (per ADR-0003 §P8-C, with prior-copy backup)
//!
//! 1. Clean any stale `_tmp` pair left by a previously interrupted save.
//! 2. `Hnsw::file_dump(dir, "hnsw_episodes_tmp")` — writes the two tmp files.
//! 3. `fsync` both tmp files; `fsync` the parent directory.
//! 4. If a live pair exists, copy it to `_bak` (overwriting prior `_bak`).
//!    Done as a copy rather than rename so the `_bak` pair is always
//!    self-consistent — even a crash between the two `_tmp → live` renames
//!    leaves us with a complete previous-version `_bak` to fall back to.
//! 5. Rename `_tmp.hnsw.data` → live; `_tmp.hnsw.graph` → live.
//! 6. `fsync` the parent directory.
//!
//! Crash analysis:
//!   - Crash before step 5 → live is unchanged; tmp files are leftover and
//!     get cleaned up next save.
//!   - Crash between the two renames in step 5 → live `.data` is new, live
//!     `.graph` is old. Startup falls back to `_bak` (consistent old pair).
//!   - Crash after step 5 → both live files are new and consistent.
//!
//! ## Startup decision tree (ADR-0003 §"Startup file-existence decision tree")
//!
//! Caller order:
//!   1. `load(dir)` — try the live pair.
//!   2. If that fails, `load_bak(dir)` — try the backup pair.
//!   3. If both fail, the caller falls back to a fresh empty index OR a
//!      rebuild from SQL (deferred to commit 1.5 daemon `main`).
//!
//! ## Windows note
//!
//! `fsync_dir` is a no-op on Windows — opening a directory for `FlushFileBuffers`
//! requires `FILE_FLAG_BACKUP_SEMANTICS` and provides marginal value over
//! NTFS's metadata journaling. File-level `fsync` (sync_all) is portable and
//! we rely on it for snapshot durability.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hnsw_rs::api::AnnT;
use solo_core::{Error, Result};

use crate::vector_index::{HnswIndex, load_inner_from_basename};

pub const LIVE_BASENAME: &str = "hnsw_episodes";
pub const BAK_BASENAME: &str = "hnsw_episodes_bak";
pub const TMP_BASENAME: &str = "hnsw_episodes_tmp";

const DATA_SUFFIX: &str = ".hnsw.data";
const GRAPH_SUFFIX: &str = ".hnsw.graph";

fn data_path(dir: &Path, basename: &str) -> PathBuf {
    let mut p = PathBuf::from(dir);
    p.push(format!("{basename}{DATA_SUFFIX}"));
    p
}

fn graph_path(dir: &Path, basename: &str) -> PathBuf {
    let mut p = PathBuf::from(dir);
    p.push(format!("{basename}{GRAPH_SUFFIX}"));
    p
}

/// True if both `.hnsw.data` and `.hnsw.graph` exist for the given basename.
pub fn pair_exists(dir: &Path, basename: &str) -> bool {
    data_path(dir, basename).is_file() && graph_path(dir, basename).is_file()
}

/// Remove every snapshot pair (`live`, `_bak`, and any leftover `_tmp`)
/// from `dir`. Used by `solo reembed` to force a rebuild-from-SQL on
/// next startup: after reembed regenerates the `embeddings` rows, the
/// in-memory HNSW (still holding stale vectors from the previous
/// embedder) becomes worthless. Wiping the on-disk snapshot pairs means
/// the next `solo daemon` / one-shot start finds no usable snapshot,
/// falls through `load_hnsw_with_fallback`'s third branch, and rebuilds
/// from `embeddings` (commit X.3).
///
/// `NotFound` errors are treated as success per file. Failure for any
/// other reason (e.g. permission, in-use) returns an `Error` so callers
/// can surface it; the caller should typically refuse to write a fresh
/// snapshot until the user resolves the underlying issue.
pub fn delete_all_pairs(dir: &Path) -> Result<()> {
    for basename in [LIVE_BASENAME, BAK_BASENAME, TMP_BASENAME] {
        remove_pair(dir, basename).map_err(|e| {
            Error::vector_index(format!("delete snapshot pair {basename:?} in {dir:?}: {e}"))
        })?;
    }
    Ok(())
}

fn remove_pair(dir: &Path, basename: &str) -> io::Result<()> {
    for p in [data_path(dir, basename), graph_path(dir, basename)] {
        match fs::remove_file(&p) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn copy_pair(dir: &Path, src_basename: &str, dst_basename: &str) -> io::Result<()> {
    fs::copy(data_path(dir, src_basename), data_path(dir, dst_basename))?;
    fs::copy(graph_path(dir, src_basename), graph_path(dir, dst_basename))?;
    Ok(())
}

fn rename_pair(dir: &Path, src_basename: &str, dst_basename: &str) -> io::Result<()> {
    fs::rename(data_path(dir, src_basename), data_path(dir, dst_basename))?;
    fs::rename(graph_path(dir, src_basename), graph_path(dir, dst_basename))?;
    Ok(())
}

fn fsync_file(path: &Path) -> io::Result<()> {
    // On Windows, `sync_all` calls `FlushFileBuffers` which requires
    // GENERIC_WRITE access — a read-only handle returns ERROR_ACCESS_DENIED.
    // Opening with `write(true)` (no truncate, no create) gets us a writable
    // handle on an existing file across both Unix and Windows.
    let f = fs::OpenOptions::new().write(true).open(path)?;
    f.sync_all()
}

fn fsync_pair(dir: &Path, basename: &str) -> io::Result<()> {
    fsync_file(&data_path(dir, basename))?;
    fsync_file(&graph_path(dir, basename))?;
    Ok(())
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> io::Result<()> {
    let f = fs::OpenOptions::new().read(true).open(dir)?;
    f.sync_all()
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> io::Result<()> {
    // No-op on Windows — see module docs.
    Ok(())
}

/// Atomically save `idx` to `dir` per ADR-0003 §P8-C.
///
/// **Empty-index special case.** `hnsw_rs::Hnsw::file_dump` fails when no
/// vectors have ever been inserted (the `data_dimension` field is set
/// lazily on first insert; dump errors before it's known). We treat an
/// empty index as "nothing to persist" and skip cleanly. The next
/// successful save (after the first insert lands) writes a real
/// snapshot. Callers see Ok, no `_tmp` files are touched.
///
/// Important: we use `raw_len()` (graph-internal count) here, not
/// `len()` (which is `raw_len - tombstones`). `len() == 0` could mean
/// "every vector has been forgotten" — the graph still holds N
/// entries that need to round-trip across reload, and the SQL-driven
/// tombstone rebuild on startup re-applies the tombstones. Using
/// `len()` here would silently lose those vectors.
pub fn save(idx: &HnswIndex, dir: &Path) -> Result<()> {
    if idx.raw_len() == 0 {
        tracing::debug!(?dir, "snapshot::save: index is empty; skipping");
        return Ok(());
    }

    fs::create_dir_all(dir)
        .map_err(|e| Error::vector_index(format!("create snapshot dir {dir:?}: {e}")))?;

    // 1. Clean stale `_tmp` from a prior interrupted save.
    remove_pair(dir, TMP_BASENAME)
        .map_err(|e| Error::vector_index(format!("clean stale tmp pair: {e}")))?;

    // 2. Hnsw::file_dump writes both `.hnsw.data` and `.hnsw.graph`.
    idx.inner()
        .file_dump(dir, TMP_BASENAME)
        .map_err(|e| Error::vector_index(format!("Hnsw::file_dump: {e}")))?;

    // 3. fsync the new tmp files + the directory entry.
    fsync_pair(dir, TMP_BASENAME)
        .map_err(|e| Error::vector_index(format!("fsync tmp pair: {e}")))?;
    fsync_dir(dir).map_err(|e| Error::vector_index(format!("fsync dir post-tmp: {e}")))?;

    // 4. Promote the previous live pair to `_bak` via copy (keeps `_bak`
    //    self-consistent across the partial-rename window in step 5).
    if pair_exists(dir, LIVE_BASENAME) {
        remove_pair(dir, BAK_BASENAME)
            .map_err(|e| Error::vector_index(format!("clean prior bak: {e}")))?;
        copy_pair(dir, LIVE_BASENAME, BAK_BASENAME)
            .map_err(|e| Error::vector_index(format!("copy live→bak: {e}")))?;
        fsync_pair(dir, BAK_BASENAME)
            .map_err(|e| Error::vector_index(format!("fsync bak pair: {e}")))?;
        fsync_dir(dir).map_err(|e| Error::vector_index(format!("fsync dir post-bak: {e}")))?;
    }

    // 5. Atomic-rename tmp pair into place. On Windows, std::fs::rename
    //    overwrites an existing destination via MoveFileEx with
    //    MOVEFILE_REPLACE_EXISTING, so this works even on first save.
    rename_pair(dir, TMP_BASENAME, LIVE_BASENAME)
        .map_err(|e| Error::vector_index(format!("rename tmp→live: {e}")))?;

    // 6. Final dir fsync.
    fsync_dir(dir).map_err(|e| Error::vector_index(format!("fsync dir post-promote: {e}")))?;

    tracing::debug!(?dir, "HNSW snapshot saved");
    Ok(())
}

/// Load the live snapshot. Returns `Err` if either file is missing or the
/// hnsw_rs loader rejects the pair.
pub fn load(dir: &Path) -> Result<HnswIndex> {
    if !pair_exists(dir, LIVE_BASENAME) {
        return Err(Error::vector_index(format!(
            "live HNSW snapshot pair missing in {dir:?}"
        )));
    }
    load_inner_from_basename(dir, LIVE_BASENAME)
}

/// Load the backup snapshot. Returns `Err` if either file is missing.
pub fn load_bak(dir: &Path) -> Result<HnswIndex> {
    if !pair_exists(dir, BAK_BASENAME) {
        return Err(Error::vector_index(format!(
            "backup HNSW snapshot pair missing in {dir:?}"
        )));
    }
    load_inner_from_basename(dir, BAK_BASENAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector_index::{HnswIndex, HnswParams};
    use solo_core::VectorIndex;

    fn unit_vec(seed: u32, dim: usize) -> Vec<f32> {
        let mut v = vec![0.0f32; dim];
        let s = (seed as f32) * 0.123;
        for (i, x) in v.iter_mut().enumerate() {
            *x = (s + i as f32 * 0.317).sin();
        }
        let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in &mut v {
            *x /= n;
        }
        v
    }

    fn populate(idx: &HnswIndex, n: usize, dim: usize) {
        for i in 0..n {
            idx.add(i as i64 + 1, &unit_vec(i as u32 + 1, dim)).unwrap();
        }
    }

    #[test]
    fn save_then_load_roundtrip_preserves_search_recall() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = 16;
        let idx = HnswIndex::new(dim, HnswParams::default());
        populate(&idx, 20, dim);

        save(&idx, tmp.path()).unwrap();
        assert!(pair_exists(tmp.path(), LIVE_BASENAME));

        let restored = load(tmp.path()).unwrap();
        assert_eq!(restored.dim(), dim);
        assert_eq!(restored.len(), 20);

        // Self-search after reload finds rowid 5 first.
        let q = unit_vec(5, dim);
        let hits = restored.search(&q, 3).unwrap();
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, 5);
    }

    #[test]
    fn second_save_promotes_previous_to_bak() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = 8;

        let idx1 = HnswIndex::new(dim, HnswParams::default());
        populate(&idx1, 5, dim);
        save(&idx1, tmp.path()).unwrap();
        assert!(pair_exists(tmp.path(), LIVE_BASENAME));
        assert!(!pair_exists(tmp.path(), BAK_BASENAME));

        let idx2 = HnswIndex::new(dim, HnswParams::default());
        populate(&idx2, 9, dim);
        save(&idx2, tmp.path()).unwrap();
        assert!(pair_exists(tmp.path(), LIVE_BASENAME));
        assert!(pair_exists(tmp.path(), BAK_BASENAME));

        let live = load(tmp.path()).unwrap();
        assert_eq!(live.len(), 9);
        let bak = load_bak(tmp.path()).unwrap();
        assert_eq!(bak.len(), 5);
    }

    #[test]
    fn corrupt_live_falls_back_to_bak() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = 8;

        let idx1 = HnswIndex::new(dim, HnswParams::default());
        populate(&idx1, 7, dim);
        save(&idx1, tmp.path()).unwrap();

        let idx2 = HnswIndex::new(dim, HnswParams::default());
        populate(&idx2, 11, dim);
        save(&idx2, tmp.path()).unwrap();

        // Corrupt the live graph file (truncate to garbage).
        let live_graph = graph_path(tmp.path(), LIVE_BASENAME);
        std::fs::write(&live_graph, b"GARBAGE").unwrap();

        // Primary load should fail.
        assert!(load(tmp.path()).is_err());

        // Backup must succeed and reflect idx1.
        let bak = load_bak(tmp.path()).unwrap();
        assert_eq!(bak.len(), 7);
    }

    #[test]
    fn missing_snapshot_returns_error_not_panic() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = load(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("missing"));
        let err = load_bak(tmp.path()).unwrap_err();
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn save_on_empty_index_is_noop() {
        // hnsw_rs::file_dump errors on an empty index; snapshot::save
        // detects this and skips cleanly without writing anything.
        let tmp = tempfile::TempDir::new().unwrap();
        let idx = HnswIndex::new(8, HnswParams::default());
        assert_eq!(idx.len(), 0);
        save(&idx, tmp.path()).expect("empty save must succeed");
        // Nothing on disk.
        assert!(!pair_exists(tmp.path(), LIVE_BASENAME));
        assert!(!pair_exists(tmp.path(), TMP_BASENAME));
    }

    /// Regression: `len()` returns `raw_len - tombstones`. If a user
    /// forgets every vector, `len()` is 0 — but the graph still holds
    /// the actual data. snapshot::save must use `raw_len()` (not
    /// `len()`) to decide "is there anything to persist?"; otherwise
    /// the snapshot is silently skipped and the data is lost on restart.
    #[test]
    fn save_persists_when_all_visible_entries_are_tombstoned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = 8usize;
        let idx = HnswIndex::new(dim, HnswParams::default());
        for i in 1..=3 {
            idx.add(i as i64, &unit_vec(i as u32, dim)).unwrap();
        }
        // Tombstone everything → len() == 0, but raw_len() == 3.
        for i in 1..=3 {
            idx.remove(i as i64).unwrap();
        }
        assert_eq!(idx.len(), 0);

        save(&idx, tmp.path()).expect("save must succeed");
        // The snapshot must exist — the graph still has 3 entries.
        assert!(
            pair_exists(tmp.path(), LIVE_BASENAME),
            "snapshot must be written even when all entries are tombstoned"
        );

        let restored = load(tmp.path()).unwrap();
        // raw_len round-trips; tombstones are NOT persisted (rebuilt
        // from SQL on startup, not from the snapshot).
        assert_eq!(restored.len(), 3, "graph entries restored intact");
    }

    #[test]
    fn stale_tmp_files_get_cleaned_on_save() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Plant stale tmp files.
        std::fs::write(data_path(tmp.path(), TMP_BASENAME), b"stale").unwrap();
        std::fs::write(graph_path(tmp.path(), TMP_BASENAME), b"stale").unwrap();

        let idx = HnswIndex::new(8, HnswParams::default());
        populate(&idx, 3, 8);
        save(&idx, tmp.path()).unwrap();

        // After save, tmp pair must be gone (renamed to live).
        assert!(!pair_exists(tmp.path(), TMP_BASENAME));
        assert!(pair_exists(tmp.path(), LIVE_BASENAME));
    }
}
