// SPDX-License-Identifier: Apache-2.0

//! Startup recovery for the HNSW index. Two pieces:
//!
//!   1. [`replay_pending_index`] — drains the `pending_index` outbox into the
//!      live HNSW. Per ADR-0003 §"`pending_index` idempotency on replay" and
//!      §P8-D, we just call `hnsw.add(rowid, embedding)` for each row;
//!      `hnsw_rs` doesn't expose membership-by-id but tolerates duplicate
//!      inserts. The DELETE on `pending_index` happens after the add
//!      succeeds, in the same order as the steady-state writer path.
//!   2. [`detect_drift`] — compares HNSW vector count against
//!      `SELECT COUNT(*) FROM episodes WHERE tier='hot'`. A mismatch beyond
//!      whatever is in `pending_index` indicates the snapshot is stale or
//!      out-of-sync with SQL; the daemon can then choose to rebuild.
//!
//! Both functions are sync and take `&Connection` — they are designed to be
//! called from the startup chain in `main()` (commit 1.5) on the temporary
//! init connection, before the WriterActor's permanent connection is
//! spawned. ADR-0003 §"Migration vs. writer-thread connection lifecycle"
//! explains the rationale.

use rusqlite::{Connection, params};
use solo_core::{Error, Result, VectorIndex};

use crate::hnsw_id::{chunk_hnsw_id, episode_hnsw_id};

#[derive(Debug, Clone, Default)]
pub struct ReplayReport {
    pub rows_seen: usize,
    pub rows_replayed: usize,
    pub rows_failed: usize,
}

#[derive(Debug, Clone, Default)]
pub struct DriftReport {
    pub hot_episodes: usize,
    /// Document chunks whose parent document is `status='active'`. Added
    /// in v0.7.1 — chunks share the HNSW namespace with hot episodes
    /// (kind-encoded via `hnsw_id::chunk_hnsw_id`, see ADR-0003), so
    /// drift detection must account for both kinds. Pre-v0.7.0 data
    /// dirs without a `documents` table read this as zero.
    pub active_chunks: usize,
    pub index_len: usize,
    /// Difference: `(hot_episodes + active_chunks) - index_len`. Positive
    /// = SQL has more rows than the index (snapshot stale or replay
    /// incomplete). Negative = index has more vectors than SQL has live
    /// rows (orphaned vectors; likely benign but worth surfacing).
    pub diff: i64,
}

impl DriftReport {
    pub fn is_clean(&self) -> bool {
        self.diff == 0
    }

    /// Total live rows in SQL that should be present in the HNSW: hot
    /// episodes plus active document chunks. v0.7.1 — exposed for
    /// callers that want to log the expected count alongside the
    /// observed `index_len`.
    pub fn expected_index_len(&self) -> usize {
        self.hot_episodes + self.active_chunks
    }
}

/// Replay all `pending_index` rows into `hnsw` and drain on success.
///
/// Idempotent: safe to re-run after a crashed replay. `hnsw.add` is
/// duplicate-tolerant (ADR-0003 §P8-D); a rowid that's already in the index
/// from the loaded snapshot will produce a redundant graph node, which is
/// negligible at the typical replay scale (1-100 rows post-snapshot lag).
///
/// The drain runs after the HNSW add, matching the steady-state ordering in
/// `WriterActor::dispatch_remember` / `dispatch_ingest_document`. If the
/// DELETE fails, the row stays in `pending_index` and gets retried on the
/// next startup — same end state.
///
/// v0.7.0 extends replay to handle two kinds of outbox rows:
///
///   * `kind='episode'` → joined against `episodes.memory_id`. Rowid is
///     encoded via [`episode_hnsw_id`] (high bit clear) before
///     `hnsw.add` so episode and chunk namespaces don't collide in the
///     shared index.
///   * `kind='chunk'`   → joined against `document_chunks.chunk_id`.
///     Rowid is encoded via [`chunk_hnsw_id`] (high bit set) before
///     `hnsw.add`.
///
/// Two SELECTs are issued (one per kind) rather than a single UNION because
/// the JOIN target differs and the `rusqlite` query_map ergonomics are
/// cleaner when each row shape is statically known. Both selects share the
/// same per-row drain loop below.
pub fn replay_pending_index(conn: &mut Connection, hnsw: &dyn VectorIndex) -> Result<ReplayReport> {
    let mut report = ReplayReport {
        rows_seen: 0,
        rows_replayed: 0,
        rows_failed: 0,
    };

    // Episode rows. Dev-log 0154: filter out forgotten episodes — if an
    // episode was soft-deleted between the original `hnsw.add` failure
    // and this replay, re-adding its vector would resurrect a dead row
    // in HNSW. The orphan pending_index rows are GC'd separately below
    // so they don't replay every startup.
    let episode_rows: Vec<(String, i64, Vec<u8>, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT p.memory_id, e.rowid, p.embedding, p.embedding_dim
                 FROM pending_index p
                 JOIN episodes e ON e.memory_id = p.memory_id
                 WHERE p.kind = 'episode'
                   AND e.status = 'active'
                 ORDER BY p.enqueued_at",
            )
            .map_err(|e| Error::storage(format!("prepare pending_index episode select: {e}")))?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| Error::storage(format!("query_map pending_index episode: {e}")))?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r.map_err(|e| Error::storage(format!("episode row decode: {e}")))?);
        }
        out
    };

    // Chunk rows. Dev-log 0154: filter out chunks of forgotten
    // documents — same resurrection-prevention as the episode pass.
    let chunk_rows: Vec<(String, i64, Vec<u8>, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT p.chunk_id, dc.rowid, p.embedding, p.embedding_dim
                 FROM pending_index p
                 JOIN document_chunks dc ON dc.chunk_id = p.chunk_id
                 JOIN documents d ON d.doc_id = dc.doc_id
                 WHERE p.kind = 'chunk'
                   AND d.status = 'active'
                 ORDER BY p.enqueued_at",
            )
            .map_err(|e| Error::storage(format!("prepare pending_index chunk select: {e}")))?;
        let mapped = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| Error::storage(format!("query_map pending_index chunk: {e}")))?;
        let mut out = Vec::new();
        for r in mapped {
            out.push(r.map_err(|e| Error::storage(format!("chunk row decode: {e}")))?);
        }
        out
    };

    // Episode pass.
    for (memory_id, rowid, blob, dim) in episode_rows {
        report.rows_seen += 1;
        let dim_u = dim as usize;
        if blob.len() != dim_u * 4 {
            tracing::warn!(
                %memory_id,
                blob_len = blob.len(),
                expected = dim_u * 4,
                "pending_index episode row size mismatch (not F32×dim); skipping"
            );
            report.rows_failed += 1;
            continue;
        }
        let slice: &[f32] = match bytemuck::try_cast_slice::<u8, f32>(&blob) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    %memory_id,
                    error = %e,
                    "pending_index episode blob alignment cast failed; skipping"
                );
                report.rows_failed += 1;
                continue;
            }
        };
        if let Err(e) = hnsw.add(episode_hnsw_id(rowid), slice) {
            tracing::warn!(%memory_id, error = %e, "hnsw.add during episode replay failed");
            report.rows_failed += 1;
            continue;
        }
        match conn.execute(
            "DELETE FROM pending_index WHERE kind = 'episode' AND memory_id = ?",
            params![memory_id],
        ) {
            Ok(_) => report.rows_replayed += 1,
            Err(e) => {
                tracing::warn!(%memory_id, error = %e, "episode drain after replay failed");
                report.rows_failed += 1;
            }
        }
    }

    // Chunk pass.
    for (chunk_id, rowid, blob, dim) in chunk_rows {
        report.rows_seen += 1;
        let dim_u = dim as usize;
        if blob.len() != dim_u * 4 {
            tracing::warn!(
                %chunk_id,
                blob_len = blob.len(),
                expected = dim_u * 4,
                "pending_index chunk row size mismatch (not F32×dim); skipping"
            );
            report.rows_failed += 1;
            continue;
        }
        let slice: &[f32] = match bytemuck::try_cast_slice::<u8, f32>(&blob) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    %chunk_id,
                    error = %e,
                    "pending_index chunk blob alignment cast failed; skipping"
                );
                report.rows_failed += 1;
                continue;
            }
        };
        if let Err(e) = hnsw.add(chunk_hnsw_id(rowid), slice) {
            tracing::warn!(%chunk_id, error = %e, "hnsw.add during chunk replay failed");
            report.rows_failed += 1;
            continue;
        }
        match conn.execute(
            "DELETE FROM pending_index WHERE kind = 'chunk' AND chunk_id = ?",
            params![chunk_id],
        ) {
            Ok(_) => report.rows_replayed += 1,
            Err(e) => {
                tracing::warn!(%chunk_id, error = %e, "chunk drain after replay failed");
                report.rows_failed += 1;
            }
        }
    }

    // Dev-log 0154: orphan GC. Any pending_index rows whose target was
    // forgotten while we were down (or whose target rowid no longer
    // exists at all) would otherwise replay every startup forever.
    // Sweep them here so the outbox stays bounded.
    let orphan_episodes = conn
        .execute(
            "DELETE FROM pending_index
              WHERE kind = 'episode'
                AND memory_id NOT IN (
                    SELECT memory_id FROM episodes WHERE status = 'active'
                )",
            [],
        )
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "orphan episode GC failed (will retry next startup)");
            0
        });
    let orphan_chunks = conn
        .execute(
            "DELETE FROM pending_index
              WHERE kind = 'chunk'
                AND chunk_id NOT IN (
                    SELECT dc.chunk_id FROM document_chunks dc
                    JOIN documents d ON d.doc_id = dc.doc_id
                    WHERE d.status = 'active'
                )",
            [],
        )
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "orphan chunk GC failed (will retry next startup)");
            0
        });
    if orphan_episodes + orphan_chunks > 0 {
        tracing::info!(
            orphan_episodes,
            orphan_chunks,
            "pending_index orphan rows GC'd (target was forgotten or missing)"
        );
    }

    tracing::info!(
        seen = report.rows_seen,
        replayed = report.rows_replayed,
        failed = report.rows_failed,
        "pending_index replay complete"
    );
    Ok(report)
}

/// What `rebuild_hnsw_from_sql` reports back to the caller.
#[derive(Debug, Clone, Default)]
pub struct RebuildReport {
    /// Rows the SELECTs returned.
    pub rows_seen: usize,
    /// Rows successfully added to the HNSW.
    pub rows_added: usize,
    /// Rows skipped due to per-row decode failure (size mismatch, alignment,
    /// non-f32 dtype, hnsw.add error). Each skipped row is logged at WARN
    /// with the rowid and reason; the rebuild does NOT abort.
    pub rows_skipped: usize,
    /// Active episodes with a current-embedder row seen during rebuild.
    pub episodes_seen: usize,
    /// Active episodes successfully added.
    pub episodes_added: usize,
    /// Active document chunks with a current-embedder row seen during rebuild.
    pub chunks_seen: usize,
    /// Active document chunks successfully added.
    pub chunks_added: usize,
}

/// Rebuild the HNSW from the `embeddings` table for `current_embedder_id`.
///
/// Used by the startup chain when neither the live nor the `_bak`
/// snapshot pair could be loaded — typically after `solo reembed`
/// deletes the pairs to force this path. Without this rebuild, recall
/// would silently return zero hits until the user re-remembered enough
/// content to repopulate the index naturally.
///
/// Iterates `episodes JOIN embeddings WHERE em.embedder_id =
/// current_embedder_id AND e.status = 'active' ORDER BY e.rowid` and
/// calls `hnsw.add(rowid, vector)` for each row.
///
/// **Currently f32-only.** The `dim * 4` size check assumes 4-byte
/// elements; non-f32 rows would mismatch and get skipped. In practice
/// this is fine because `dispatch_remember` enforces F32 at insert
/// time (`as_f32_slice` check), so only F32 rows can exist today. If
/// a future writer accepts other dtypes, this branch needs to widen.
///
/// **Failure handling matches `replay_pending_index`**: a corrupt row
/// (size mismatch, alignment, non-f32 dtype, hnsw.add error) is logged
/// at WARN and skipped, NOT propagated. This keeps the daemon (and
/// `solo doctor`) bootable so the user can investigate via logs and
/// re-run `solo reembed` to overwrite the bad row. Fail-fast would
/// leave the database unbootable from inside the product.
///
/// Cost: dominated by hnsw_rs's per-insert work (~1 ms for the default
/// HNSW params at 1024-dim). 10K episodes ≈ 10 sec; surfaced via a
/// tracing::info from the caller.
pub fn rebuild_hnsw_from_sql(
    conn: &Connection,
    hnsw: &dyn VectorIndex,
    current_embedder_id: i64,
) -> Result<RebuildReport> {
    let mut report = RebuildReport::default();

    {
        let mut stmt = conn
            .prepare(
                "SELECT e.rowid, em.vector, em.dim
                 FROM episodes e
                 JOIN embeddings em ON em.memory_id = e.memory_id
                 WHERE em.embedder_id = ?1
                   AND e.status = 'active'
                 ORDER BY e.rowid",
            )
            .map_err(|e| Error::storage(format!("prepare rebuild_hnsw_from_sql episodes: {e}")))?;

        let rows = stmt
            .query_map(params![current_embedder_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| {
                Error::storage(format!("query_map rebuild_hnsw_from_sql episodes: {e}"))
            })?;

        for row in rows {
            report.rows_seen += 1;
            report.episodes_seen += 1;
            let (rowid, blob, dim) = match row {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "rebuild_hnsw_from_sql: episode row decode failed; skipping");
                    report.rows_skipped += 1;
                    continue;
                }
            };
            if add_rebuild_row(hnsw, episode_hnsw_id(rowid), rowid, &blob, dim, "episode") {
                report.rows_added += 1;
                report.episodes_added += 1;
            } else {
                report.rows_skipped += 1;
            }
        }
    }

    if documents_tables_present(conn)? {
        let mut stmt = conn
            .prepare(
                "SELECT c.rowid, ce.vector, ce.dim
                 FROM document_chunks c
                 JOIN documents d ON d.doc_id = c.doc_id
                 JOIN chunk_embeddings ce ON ce.chunk_id = c.chunk_id
                 WHERE ce.embedder_id = ?1
                   AND d.status = 'active'
                 ORDER BY c.rowid",
            )
            .map_err(|e| Error::storage(format!("prepare rebuild_hnsw_from_sql chunks: {e}")))?;

        let rows = stmt
            .query_map(params![current_embedder_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| Error::storage(format!("query_map rebuild_hnsw_from_sql chunks: {e}")))?;

        for row in rows {
            report.rows_seen += 1;
            report.chunks_seen += 1;
            let (rowid, blob, dim) = match row {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "rebuild_hnsw_from_sql: chunk row decode failed; skipping");
                    report.rows_skipped += 1;
                    continue;
                }
            };
            if add_rebuild_row(hnsw, chunk_hnsw_id(rowid), rowid, &blob, dim, "chunk") {
                report.rows_added += 1;
                report.chunks_added += 1;
            } else {
                report.rows_skipped += 1;
            }
        }
    }
    Ok(report)
}

fn add_rebuild_row(
    hnsw: &dyn VectorIndex,
    hnsw_id: i64,
    rowid: i64,
    blob: &[u8],
    dim: i64,
    kind: &str,
) -> bool {
    let dim = dim as usize;
    if blob.len() != dim * 4 {
        tracing::warn!(
            rowid,
            kind,
            blob_len = blob.len(),
            expected = dim * 4,
            "rebuild_hnsw_from_sql: f32-vector size mismatch; skipping (run `solo reembed` to overwrite)"
        );
        return false;
    }
    let slice: &[f32] = match bytemuck::try_cast_slice(blob) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                rowid,
                kind,
                error = %e,
                "rebuild_hnsw_from_sql: blob alignment cast failed; skipping"
            );
            return false;
        }
    };
    if let Err(e) = hnsw.add(hnsw_id, slice) {
        tracing::warn!(rowid, kind, error = %e, "rebuild_hnsw_from_sql: hnsw.add failed; skipping");
        return false;
    }
    true
}

/// Compare HNSW vector count against the live SQL rows that share the
/// shared episode/chunk HNSW namespace.
///
/// In a healthy steady state `index_len == hot_episodes + active_chunks`.
/// Mismatches signal either a stale snapshot (live count < SQL count) or
/// stale tombstones / orphans (live count > SQL count). The daemon
/// decides what to do (typically: rebuild from SQL if `diff` exceeds a
/// threshold).
///
/// v0.7.1 — pre-v0.7.1 versions compared `index_len` against `hot_episodes`
/// only, which produced a false-positive drift warning on every startup
/// where any document had been ingested (chunks landed in the HNSW with
/// the kind-discriminated encoding from `hnsw_id::chunk_hnsw_id` but were
/// invisible to the drift comparison). We now COUNT(*) the chunks whose
/// parent document is `status='active'` and add them to the expected
/// total. The `documents` / `document_chunks` tables are absent in
/// pre-0003 schemas, so the count query is gated on a sqlite_master probe
/// and returns 0 when the tables are missing — keeps the function safe
/// to call against legacy data dirs that haven't migrated yet.
pub fn detect_drift(conn: &Connection, hnsw: &dyn VectorIndex) -> Result<DriftReport> {
    let hot_episodes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM episodes WHERE tier = 'hot' AND status = 'active'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("count hot episodes: {e}")))?;

    let active_chunks: i64 = if documents_tables_present(conn)? {
        conn.query_row(
            "SELECT COUNT(*)
             FROM document_chunks dc
             JOIN documents d ON d.doc_id = dc.doc_id
             WHERE d.status = 'active'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("count active chunks: {e}")))?
    } else {
        0
    };

    let index_len = hnsw.len();
    let expected = hot_episodes + active_chunks;
    let diff = expected - (index_len as i64);

    Ok(DriftReport {
        hot_episodes: hot_episodes as usize,
        active_chunks: active_chunks as usize,
        index_len,
        diff,
    })
}

/// Probe `sqlite_master` for the v0.7.0 `documents` table. Pre-migration-0003
/// data dirs don't have it; the drift detector treats them as
/// `active_chunks = 0` rather than erroring on a missing table.
fn documents_tables_present(conn: &Connection) -> Result<bool> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'documents'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("probe sqlite_master for documents: {e}")))?;
    Ok(exists > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{StubVectorIndex, fixture_episode, open_test_db};
    use rusqlite::params;
    use solo_core::{Tier, VectorIndex};

    fn insert_episode(conn: &Connection, content: &str) -> (String, i64) {
        let ep = fixture_episode(content);
        let memory_id = ep.memory_id.to_string();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tier = match ep.tier {
            Tier::Hot => "hot",
            Tier::Warm => "warm",
            Tier::Cold => "cold",
        };
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, source_id, content,
                encoding_context_json, provenance_json, confidence,
                strength, salience, tier, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                memory_id,
                ep.ts_ms,
                ep.source_type,
                ep.source_id,
                ep.content,
                "{}",
                Option::<String>::None,
                ep.confidence.0,
                ep.strength,
                ep.salience,
                tier,
                now_ms,
                now_ms,
            ],
        )
        .unwrap();
        let rowid = conn.last_insert_rowid();
        (memory_id, rowid)
    }

    fn enqueue_pending(conn: &Connection, memory_id: &str, dim: usize) {
        let zeros = vec![0u8; dim * 4];
        conn.execute(
            "INSERT INTO pending_index (memory_id, embedding, embedding_dim, enqueued_at)
             VALUES (?, ?, ?, ?)",
            params![memory_id, &zeros[..], dim as i64, 0i64],
        )
        .unwrap();
    }

    fn insert_current_embedder(conn: &Connection, dim: usize) -> i64 {
        conn.execute(
            "INSERT INTO embedders (name, version, dim, dtype, first_seen_ms)
             VALUES ('stub', 'v1', ?, 'f32', 0)",
            params![dim as i64],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn f32_blob(values: &[f32]) -> Vec<u8> {
        bytemuck::cast_slice(values).to_vec()
    }

    fn insert_episode_embedding(
        conn: &Connection,
        memory_id: &str,
        embedder_id: i64,
        values: &[f32],
    ) {
        let blob = f32_blob(values);
        conn.execute(
            "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms)
             VALUES (?, ?, 'f32', ?, ?, 0)",
            params![memory_id, embedder_id, values.len() as i64, blob],
        )
        .unwrap();
    }

    fn insert_chunk_embedding(conn: &Connection, chunk_id: &str, embedder_id: i64, values: &[f32]) {
        let blob = f32_blob(values);
        conn.execute(
            "INSERT INTO chunk_embeddings (chunk_id, embedder_id, dtype, dim, vector, created_at_ms)
             VALUES (?, ?, 'f32', ?, ?, 0)",
            params![chunk_id, embedder_id, values.len() as i64, blob],
        )
        .unwrap();
    }

    #[test]
    fn replay_drains_all_rows_and_calls_add() {
        let (mut conn, _tmp) = open_test_db();
        let (mid_a, rowid_a) = insert_episode(&conn, "a");
        let (mid_b, rowid_b) = insert_episode(&conn, "b");
        enqueue_pending(&conn, &mid_a, 4);
        enqueue_pending(&conn, &mid_b, 4);

        let stub = StubVectorIndex::new(4);
        let report = replay_pending_index(&mut conn, &stub).unwrap();
        assert_eq!(report.rows_seen, 2);
        assert_eq!(report.rows_replayed, 2);
        assert_eq!(report.rows_failed, 0);
        assert_eq!(stub.add_count(), 2);

        // pending_index is fully drained.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);

        // The added rowids match the joined episode rowids.
        let entries = stub.entries();
        let added_rowids: std::collections::HashSet<i64> =
            entries.iter().map(|(r, _)| *r).collect();
        let expected: std::collections::HashSet<i64> = [rowid_a, rowid_b].into_iter().collect();
        assert_eq!(added_rowids, expected);
    }

    #[test]
    fn replay_is_idempotent_when_run_twice() {
        let (mut conn, _tmp) = open_test_db();
        let (mid, _rowid) = insert_episode(&conn, "x");
        enqueue_pending(&conn, &mid, 4);

        let stub = StubVectorIndex::new(4);
        let r1 = replay_pending_index(&mut conn, &stub).unwrap();
        assert_eq!(r1.rows_replayed, 1);

        // Re-run with empty pending_index — must be a no-op.
        let r2 = replay_pending_index(&mut conn, &stub).unwrap();
        assert_eq!(r2.rows_seen, 0);
        assert_eq!(r2.rows_replayed, 0);
        assert_eq!(stub.add_count(), 1, "no extra add on second run");
    }

    #[test]
    fn replay_skips_size_mismatch_rows() {
        let (mut conn, _tmp) = open_test_db();
        let (mid_good, _) = insert_episode(&conn, "good");
        let (mid_bad, _) = insert_episode(&conn, "bad");
        enqueue_pending(&conn, &mid_good, 4);
        // Bad row: dim says 4 but blob is only 8 bytes (= 2 floats).
        conn.execute(
            "INSERT INTO pending_index (memory_id, embedding, embedding_dim, enqueued_at)
             VALUES (?, ?, ?, ?)",
            params![mid_bad, &vec![0u8; 8][..], 4i64, 0i64],
        )
        .unwrap();

        let stub = StubVectorIndex::new(4);
        let report = replay_pending_index(&mut conn, &stub).unwrap();
        assert_eq!(report.rows_seen, 2);
        assert_eq!(report.rows_replayed, 1);
        assert_eq!(report.rows_failed, 1);
        // Bad row stays in pending_index for ops to investigate.
        let stuck: String = conn
            .query_row("SELECT memory_id FROM pending_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stuck, mid_bad);
    }

    #[test]
    fn drift_clean_when_index_matches_episodes() {
        let (conn, _tmp) = open_test_db();
        let _ = insert_episode(&conn, "a");
        let _ = insert_episode(&conn, "b");

        let stub = StubVectorIndex::new(4);
        stub.add(1, &[0.0; 4]).unwrap();
        stub.add(2, &[0.0; 4]).unwrap();

        let drift = detect_drift(&conn, &stub).unwrap();
        assert_eq!(drift.hot_episodes, 2);
        assert_eq!(drift.index_len, 2);
        assert!(drift.is_clean());
    }

    #[test]
    fn drift_positive_when_index_lags_sql() {
        let (conn, _tmp) = open_test_db();
        let _ = insert_episode(&conn, "a");
        let _ = insert_episode(&conn, "b");
        let _ = insert_episode(&conn, "c");

        let stub = StubVectorIndex::new(4);
        stub.add(1, &[0.0; 4]).unwrap();

        let drift = detect_drift(&conn, &stub).unwrap();
        assert_eq!(drift.hot_episodes, 3);
        assert_eq!(drift.active_chunks, 0);
        assert_eq!(drift.index_len, 1);
        assert_eq!(drift.diff, 2);
        assert!(!drift.is_clean());
    }

    // ----- v0.7.1: chunk-aware drift detector -----

    /// Helper: insert one document row plus N chunks. `status` is the
    /// `documents.status` value (`active` for live, `forgotten` for soft-
    /// deleted). Each chunk row gets a unique rowid via AUTOINCREMENT —
    /// the rowids are returned so the caller can encode them for HNSW
    /// adds.
    fn insert_document_with_chunks(conn: &Connection, status: &str, n: usize) -> Vec<i64> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let doc_id = format!("doc-{now_ms}-{n}-{status}");
        conn.execute(
            "INSERT INTO documents (
                doc_id, source, title, mime_type, ingested_at_ms,
                modified_at_ms, status, chunk_count, content_hash, byte_size
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                doc_id,
                "fixture",
                "T",
                "text/markdown",
                now_ms,
                Option::<i64>::None,
                status,
                n as i64,
                format!("hash-{doc_id}"),
                42i64,
            ],
        )
        .unwrap();
        let mut rowids = Vec::with_capacity(n);
        for i in 0..n {
            let chunk_id = format!("{doc_id}-c{i}");
            conn.execute(
                "INSERT INTO document_chunks (
                    chunk_id, doc_id, chunk_index, content, token_count,
                    start_offset, end_offset, created_at_ms
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    chunk_id, doc_id, i as i64, "content", 5i64, 0i64, 10i64, now_ms
                ],
            )
            .unwrap();
            rowids.push(conn.last_insert_rowid());
        }
        rowids
    }

    fn chunk_id_for_rowid(conn: &Connection, rowid: i64) -> String {
        conn.query_row(
            "SELECT chunk_id FROM document_chunks WHERE rowid = ?",
            params![rowid],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn rebuild_hnsw_from_sql_restores_active_episodes_and_chunks() {
        let (conn, _tmp) = open_test_db();
        let embedder_id = insert_current_embedder(&conn, 4);
        let (memory_id, episode_rowid) = insert_episode(&conn, "episode rebuild target");
        insert_episode_embedding(&conn, &memory_id, embedder_id, &[1.0, 0.0, 0.0, 0.0]);

        let active_chunk_rowids = insert_document_with_chunks(&conn, "active", 2);
        for (idx, rowid) in active_chunk_rowids.iter().enumerate() {
            let chunk_id = chunk_id_for_rowid(&conn, *rowid);
            insert_chunk_embedding(
                &conn,
                &chunk_id,
                embedder_id,
                &[0.0, idx as f32 + 1.0, 0.0, 0.0],
            );
        }

        let forgotten_chunk_rowids = insert_document_with_chunks(&conn, "forgotten", 1);
        let forgotten_chunk_id = chunk_id_for_rowid(&conn, forgotten_chunk_rowids[0]);
        insert_chunk_embedding(
            &conn,
            &forgotten_chunk_id,
            embedder_id,
            &[0.0, 0.0, 1.0, 0.0],
        );

        let stub = StubVectorIndex::new(4);
        let report = rebuild_hnsw_from_sql(&conn, &stub, embedder_id).unwrap();

        assert_eq!(report.episodes_seen, 1);
        assert_eq!(report.episodes_added, 1);
        assert_eq!(report.chunks_seen, 2);
        assert_eq!(report.chunks_added, 2);
        assert_eq!(report.rows_seen, 3);
        assert_eq!(report.rows_added, 3);
        assert_eq!(report.rows_skipped, 0);

        let added_ids: std::collections::HashSet<i64> =
            stub.entries().iter().map(|(id, _)| *id).collect();
        assert!(added_ids.contains(&crate::hnsw_id::episode_hnsw_id(episode_rowid)));
        for rowid in active_chunk_rowids {
            assert!(added_ids.contains(&crate::hnsw_id::chunk_hnsw_id(rowid)));
        }
        assert!(!added_ids.contains(&crate::hnsw_id::chunk_hnsw_id(forgotten_chunk_rowids[0])));
    }

    #[test]
    fn drift_counts_active_chunks_alongside_hot_episodes() {
        // Reproduces v0.7.0 Spawn-B smoke false alarm: ingest a doc, the
        // HNSW correctly carries the chunk vector, and drift should be
        // clean — not negative.
        let (conn, _tmp) = open_test_db();
        let _ = insert_episode(&conn, "ep-a");
        let chunk_rowids = insert_document_with_chunks(&conn, "active", 2);

        let stub = StubVectorIndex::new(4);
        // Episode lands at the encoded (identity) episode id.
        stub.add(crate::hnsw_id::episode_hnsw_id(1), &[0.0; 4])
            .unwrap();
        // Chunks land at the encoded (high-bit) chunk ids.
        for rid in &chunk_rowids {
            stub.add(crate::hnsw_id::chunk_hnsw_id(*rid), &[0.0; 4])
                .unwrap();
        }

        let drift = detect_drift(&conn, &stub).unwrap();
        assert_eq!(drift.hot_episodes, 1);
        assert_eq!(drift.active_chunks, 2);
        assert_eq!(drift.expected_index_len(), 3);
        assert_eq!(drift.index_len, 3);
        assert_eq!(drift.diff, 0);
        assert!(
            drift.is_clean(),
            "drift must be clean when HNSW carries every hot episode + every \
             active chunk; got: {drift:?}"
        );
    }

    #[test]
    fn drift_excludes_forgotten_documents_chunks() {
        // Chunks under `documents.status='forgotten'` are tombstoned in
        // the HNSW (writer::handle_forget_document) — they must NOT be
        // counted in `active_chunks`.
        let (conn, _tmp) = open_test_db();
        let active_rowids = insert_document_with_chunks(&conn, "active", 1);
        let _forgotten_rowids = insert_document_with_chunks(&conn, "forgotten", 3);

        let stub = StubVectorIndex::new(4);
        for rid in &active_rowids {
            stub.add(crate::hnsw_id::chunk_hnsw_id(*rid), &[0.0; 4])
                .unwrap();
        }
        // Forgotten chunks intentionally NOT added — simulates the
        // tombstone path leaving them out of `index.len()`.

        let drift = detect_drift(&conn, &stub).unwrap();
        assert_eq!(drift.hot_episodes, 0);
        assert_eq!(drift.active_chunks, 1, "forgotten docs' chunks excluded");
        assert_eq!(drift.index_len, 1);
        assert!(drift.is_clean());
    }

    #[test]
    fn drift_still_fires_when_real_drift_exists_after_ingest() {
        // Sanity: kind-awareness must not mask genuine drift. SQL has 2
        // active chunks but HNSW has only 1.
        let (conn, _tmp) = open_test_db();
        let chunk_rowids = insert_document_with_chunks(&conn, "active", 2);

        let stub = StubVectorIndex::new(4);
        stub.add(crate::hnsw_id::chunk_hnsw_id(chunk_rowids[0]), &[0.0; 4])
            .unwrap();

        let drift = detect_drift(&conn, &stub).unwrap();
        assert_eq!(drift.hot_episodes, 0);
        assert_eq!(drift.active_chunks, 2);
        assert_eq!(drift.expected_index_len(), 2);
        assert_eq!(drift.index_len, 1);
        assert_eq!(drift.diff, 1);
        assert!(!drift.is_clean(), "true drift must still surface");
    }
}
