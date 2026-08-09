// SPDX-License-Identifier: Apache-2.0

//! Shared HNSW-tombstone-rebuild helpers.
//!
//! Walks the SQL truth (`episodes.status='forgotten'` /
//! `documents.status='forgotten'`) and registers a tombstone in the
//! in-memory HNSW index for each forgotten target. Runs at startup,
//! after the HNSW snapshot loads, so the tombstone set matches the
//! SQL `status='active'` filter on the recall path.
//!
//! Both helpers are idempotent — safe to re-run after a crashed
//! startup. `VectorIndex::remove` is itself idempotent.
//!
//! Dev-log 0154: extracted from the previously-duplicated copies in
//! `startup.rs` (default data dir) and `tenants/handle.rs` (per-tenant
//! variant). Both paths now call into here so they can never drift.
//! Dev-log 0152 H4 was the original entry that added the chunk-side
//! pass; this module is the natural home for both halves.

use rusqlite::Connection;
use solo_core::{Error, Result, VectorIndex};

use crate::hnsw_id::{chunk_hnsw_id, episode_hnsw_id};

/// Walk `episodes WHERE status='forgotten'` and call `hnsw.remove`
/// for each. Returns the count of tombstones registered.
pub fn rebuild_episode_tombstones_from_sql(
    conn: &Connection,
    hnsw: &dyn VectorIndex,
) -> Result<usize> {
    let mut stmt = conn
        .prepare("SELECT rowid FROM episodes WHERE status = 'forgotten'")
        .map_err(|e| Error::storage(format!("prepare forgotten episode select: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| Error::storage(format!("query_map forgotten episodes: {e}")))?;
    let mut count = 0usize;
    for r in rows {
        let rowid = r.map_err(|e| Error::storage(format!("forgotten episode row decode: {e}")))?;
        hnsw.remove(episode_hnsw_id(rowid))?;
        count += 1;
    }
    Ok(count)
}

/// Chunk-side sibling — walks `document_chunks` joined to `documents`
/// where `documents.status='forgotten'` and tombstones each chunk's
/// HNSW rowid. Without this, `detect_drift` reported non-zero drift on
/// every restart for any tenant that had forgotten a document
/// (dev-log 0152 H4).
pub fn rebuild_chunk_tombstones_from_sql(
    conn: &Connection,
    hnsw: &dyn VectorIndex,
) -> Result<usize> {
    let mut stmt = conn
        .prepare(
            "SELECT dc.rowid \
               FROM document_chunks dc \
               JOIN documents d ON d.doc_id = dc.doc_id \
              WHERE d.status = 'forgotten'",
        )
        .map_err(|e| Error::storage(format!("prepare forgotten chunks select: {e}")))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| Error::storage(format!("query_map forgotten chunks: {e}")))?;
    let mut count = 0usize;
    for r in rows {
        let rowid = r.map_err(|e| Error::storage(format!("forgotten chunk row decode: {e}")))?;
        hnsw.remove(chunk_hnsw_id(rowid))?;
        count += 1;
    }
    Ok(count)
}
