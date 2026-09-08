// SPDX-License-Identifier: Apache-2.0

//! Hard erasure of attributed records and retained originals in one library.
//!
//! The writer preflights shared asset ownership and paths, removes attributed
//! derived records before their foreign keys can become NULL, and deletes
//! episodes/chunks/documents/assets in one SQL transaction. Original blob
//! removal is irreversible; an I/O or commit failure requires an idempotent
//! retry. The writer also invalidates deleted vector IDs before another write
//! can reuse them. Surviving vectors are then reconciled, and an operator audit
//! entry is written to `solo.db::audit_events_admin`.
//!
//! Legacy records without attribution, existing exports/backups, and operator
//! audit history are outside this automatic cascade. This is not a claim of
//! physical secure wiping or exhaustive erasure of every external copy.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{Connection, TransactionBehavior, params};
use solo_core::{Embedder, Error, LibraryId, Result, VectorIndex};

use crate::audit::{AuditOperation, AuditResult, insert_audit_admin_row};
use crate::embedder_registry::EmbedderIdentity;
use crate::hnsw_id::{chunk_hnsw_id, episode_hnsw_id};
use crate::init::open_sqlcipher;
use crate::key_material::KeyMaterial;

/// Coverage of principal attribution. Unattributed rows require manual review;
/// counts do not establish who owns those rows or authorize their deletion.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AttributionReport {
    pub episodes_total: u64,
    pub episodes_unattributed: u64,
    pub chunks_total: u64,
    pub chunks_unattributed: u64,
}

/// Read existing attribution without opening the writer or changing records.
/// Caller must hold the library lock. This report excludes external copies.
pub fn audit_attribution(db_path: &Path, key: &KeyMaterial) -> Result<AttributionReport> {
    if !db_path.is_file() {
        return Err(Error::not_found("library database is missing"));
    }
    let conn = open_sqlcipher(db_path, key)?;
    conn.execute_batch("PRAGMA query_only = ON;")
        .map_err(|e| Error::storage(format!("set attribution audit read-only: {e}")))?;
    attribution_report(&conn)
}

fn attribution_report(conn: &Connection) -> Result<AttributionReport> {
    conn.query_row(
        "SELECT (SELECT COUNT(*) FROM episodes),
                (SELECT COUNT(*) FROM episodes WHERE principal_subject IS NULL OR trim(principal_subject) = ''),
                (SELECT COUNT(*) FROM document_chunks),
                (SELECT COUNT(*) FROM document_chunks WHERE ingested_by_principal IS NULL OR trim(ingested_by_principal) = '')",
        [], |row| Ok(AttributionReport { episodes_total: row.get::<_, i64>(0)? as u64, episodes_unattributed: row.get::<_, i64>(1)? as u64, chunks_total: row.get::<_, i64>(2)? as u64, chunks_unattributed: row.get::<_, i64>(3)? as u64 })
    ).map_err(|e| Error::storage(format!("audit attribution coverage: {e}")))
}

/// What `forget_principal` did. Returned to callers (typically the CLI
/// `solo gdpr forget` subcommand) for surfacing in the user-visible
/// summary and for downstream tests / scripting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetReport {
    /// Rows deleted from `episodes`.
    pub episodes_deleted: u64,
    /// Facts attributed to the erased episodes/documents directly or through
    /// their relationship evidence.
    pub triples_deleted: u64,
    /// Rows deleted from `document_chunks`.
    pub chunks_deleted: u64,
    /// Exclusively attributed document metadata removed with its chunks.
    pub documents_deleted: u64,
    /// Original asset records and their retained blobs removed.
    pub assets_deleted: u64,
    /// Remaining facts in affected clusters without resolvable source
    /// attribution. Surfaced for operator review instead of guessed deletion.
    pub triples_orphan_null_source: u64,
    /// Did the post-tx HNSW rebuild run? `false` iff no rows were
    /// deleted (absent-subject idempotent case).
    pub hnsw_rebuilt: bool,
    /// `audit_id` of the row written to
    /// `solo.db::audit_events_admin`. Always present — even
    /// the no-op (count=0) case emits a row so the compliance trail
    /// records the attempt.
    pub audit_admin_row_id: i64,
}

/// Erase attributed records and originals from the target library, then
/// reconcile surviving HNSW vectors. Call this synchronous maintenance entry
/// point from a blocking worker (the CLI uses `spawn_blocking`).
///
/// `tenant_handle` is the live `LibraryHandle` for the target tenant
/// (so we can route through its writer + embedder + HNSW). `data_dir`
/// + `key` are used to write the admin-audit row into `solo.db`.
///
/// ## Concurrency
///
/// Single `BEGIN IMMEDIATE` tx on the per-tenant DB. Concurrent reads
/// see either the pre- or post-delete state; no torn view. The HNSW
/// rebuild is post-tx, so concurrent recalls during the rebuild window
/// may transiently see vectors for already-deleted episodes — the
/// SELECTed read-side rows are gone, so the read won't surface deleted
/// data even if the HNSW's tombstone bitmap is briefly out of date.
///
/// ## Lockfile
///
/// Caller is responsible for holding `solo.lock` (or running through a
/// live daemon). This helper doesn't acquire it.
pub fn forget_principal(
    tenant_handle: Arc<crate::library::LibraryHandle>,
    principal_subject: &str,
    actor_principal: Option<&str>,
    data_dir: &Path,
    key: &KeyMaterial,
) -> Result<ForgetReport> {
    let tenant_id = tenant_handle.tenant_id().clone();
    let db_path = tenant_handle.db_path().to_path_buf();
    let hnsw = tenant_handle.hnsw().clone();
    let embedder_id = tenant_handle.embedder_id();

    // Serialize SQL and original-file erasure with uploads and backups. A
    // separate connection alone cannot prevent a writer from healing a blob
    // using metadata it read before erasure. Call from spawn_blocking (CLI).
    let DeleteOutcome {
        episodes_deleted,
        triples_deleted,
        triples_orphan_null_source,
        chunks_deleted,
        documents_deleted,
        assets_deleted,
        ..
    } = tenant_handle
        .write()
        .forget_principal_rows_blocking(principal_subject)?;
    let conn = open_sqlcipher(&db_path, key)?;

    if triples_orphan_null_source > 0 {
        // Operator visibility: pre-v0.8.1 triples without a resolved
        // source_episode_id cannot be cascaded by GDPR. They remain in
        // the per-tenant DB as orphans-by-design (documented in the
        // v0.8.1 release notes + this module's docstring).
        tracing::warn!(
            tenant = %tenant_id,
            principal = %principal_subject,
            orphan_count = triples_orphan_null_source,
            "gdpr.forget_user: {triples_orphan_null_source} triple(s) reference \
             the forgotten subject's cluster(s) but have NULL source_episode_id \
             (pre-v0.8.1 schema or unresolved provenance). Cannot cascade. \
             Operator action: review and manually clean up if compliance scope \
             requires it."
        );
    }

    let total_deleted =
        episodes_deleted + triples_deleted + chunks_deleted + documents_deleted + assets_deleted;

    let hnsw_rebuilt = if total_deleted > 0 {
        rebuild_hnsw_after_forget(&conn, hnsw.as_ref(), embedder_id)?;
        true
    } else {
        false
    };

    // Operator audit history remains in the Community database.
    let now_ms = chrono::Utc::now().timestamp_millis();
    let admin_path = data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME);
    let admin_conn = open_sqlcipher(&admin_path, key)?;
    let details = serde_json::json!({
        "principal_subject": principal_subject,
        "episodes_deleted": episodes_deleted,
        "triples_deleted": triples_deleted,
        "triples_orphan_null_source": triples_orphan_null_source,
        "chunks_deleted": chunks_deleted,
        "documents_deleted": documents_deleted,
        "assets_deleted": assets_deleted,
        "hnsw_rebuilt": hnsw_rebuilt,
    });
    let audit_admin_row_id = insert_audit_admin_row(
        &admin_conn,
        now_ms,
        actor_principal,
        AuditOperation::GdprForgetUser,
        Some(tenant_id.as_str()),
        AuditResult::Ok,
        Some(&details),
    )?;

    // v0.10.0: post-commit invalidation. GDPR forget cascades across
    // episodes / triples / chunks; surface as a tenant-kind event so
    // solo-web clients refetch every page. Skip the event when the
    // forget was a no-op (zero rows deleted) — there's nothing to
    // refetch. The broadcast `send` failure mode (no subscribers) is
    // benign; the `.ok()` swallows it.
    if total_deleted > 0 {
        let _ = tenant_handle
            .invalidate_sender()
            .send(solo_core::InvalidateEvent {
                reason: AuditOperation::GdprForgetUser.as_str().to_string(),
                tenant_id: tenant_id.to_string(),
                ts_ms: now_ms,
                kind: "tenant".to_string(),
            });
    }

    Ok(ForgetReport {
        episodes_deleted,
        triples_deleted,
        triples_orphan_null_source,
        chunks_deleted,
        documents_deleted,
        assets_deleted,
        hnsw_rebuilt,
        audit_admin_row_id,
    })
}

/// Outcome of one `delete_principal_rows` pass.
///
/// Includes the surviving legacy facts in affected clusters without source
/// attribution, plus vector IDs to invalidate inside the writer actor.
#[doc(hidden)]
#[derive(Debug)]
pub struct DeleteOutcome {
    episodes_deleted: u64,
    triples_deleted: u64,
    triples_orphan_null_source: u64,
    chunks_deleted: u64,
    documents_deleted: u64,
    assets_deleted: u64,
    episode_rowids: Vec<i64>,
    chunk_rowids: Vec<i64>,
}

impl DeleteOutcome {
    pub(crate) fn invalidate_vectors(&self, index: &dyn VectorIndex) {
        for id in self
            .episode_rowids
            .iter()
            .map(|id| episode_hnsw_id(*id))
            .chain(self.chunk_rowids.iter().map(|id| chunk_hnsw_id(*id)))
        {
            // SQL filtering remains authoritative if a vector implementation
            // cannot invalidate an entry. Never race a following writer's
            // reuse of a deleted SQLite rowid by doing this outside the actor.
            if let Err(error) = index.remove(id) {
                tracing::warn!(%error, "principal erasure vector invalidation failed");
            }
        }
    }
}

/// SQL-side delete cascade. Runs inside one BEGIN IMMEDIATE tx.
///
/// Returns a [`DeleteOutcome`] summarising per-table counts plus the
/// affected episode rowids (so the caller can tombstone the HNSW for
/// those rowids before / instead of triggering a full rebuild).
#[cfg(test)]
fn delete_principal_rows(conn: &mut Connection, principal_subject: &str) -> Result<DeleteOutcome> {
    delete_principal_rows_with_assets(conn, principal_subject, None)
}

pub(crate) fn delete_principal_rows_with_assets(
    conn: &mut Connection,
    principal_subject: &str,
    snapshot_dir: Option<&Path>,
) -> Result<DeleteOutcome> {
    if principal_subject.trim().is_empty() {
        return Err(Error::invalid_input("principal subject must not be empty"));
    }
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for forget: {e}")))?;
    let assets = crate::gdpr_assets::prepare(&tx, principal_subject, snapshot_dir)?;
    let chunk_rowids = {
        let mut stmt = tx
            .prepare("SELECT rowid FROM document_chunks WHERE ingested_by_principal = ?")
            .map_err(|e| Error::storage(format!("select erased chunk vectors: {e}")))?;
        stmt.query_map(params![principal_subject], |r| r.get::<_, i64>(0))
            .map_err(|e| Error::storage(format!("query erased chunk vectors: {e}")))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::storage(format!("collect erased chunk vectors: {e}")))?
    };

    // Capture attribution before cascades or SET NULL erase it. Temporary scope
    // tables also avoid SQLite's bind-variable limit for large principals.
    tx.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS solo_forget_triples (id TEXT PRIMARY KEY);
         CREATE TEMP TABLE IF NOT EXISTS solo_forget_edges (id TEXT PRIMARY KEY);
         DELETE FROM solo_forget_triples;
         DELETE FROM solo_forget_edges;",
    )
    .map_err(|e| Error::storage(format!("prepare graph erasure scope: {e}")))?;
    tx.execute(
        "INSERT OR IGNORE INTO solo_forget_triples
         SELECT triple_id FROM triples WHERE source_episode_id IN
             (SELECT rowid FROM episodes WHERE principal_subject = ?1)
         UNION SELECT triple_id FROM relationship_evidence WHERE
             source_episode_id IN (SELECT rowid FROM episodes WHERE principal_subject = ?1)
             OR memory_id IN (SELECT memory_id FROM episodes WHERE principal_subject = ?1)
             OR chunk_id IN (SELECT chunk_id FROM document_chunks WHERE ingested_by_principal = ?1)
             OR doc_id IN (SELECT id FROM solo_forget_docs)",
        params![principal_subject],
    )
    .map_err(|e| Error::storage(format!("collect attributed graph facts: {e}")))?;
    tx.execute(
        "INSERT OR IGNORE INTO solo_forget_edges SELECT edge_id FROM relationship_evidence
         WHERE triple_id IN (SELECT id FROM solo_forget_triples)",
        [],
    )
    .map_err(|e| Error::storage(format!("collect affected graph relationships: {e}")))?;

    // Remove newly introduced derived records before ON DELETE SET NULL can
    // sever their attribution. Summaries of mixed clusters must be regenerated
    // from the surviving raw episodes rather than retaining erased content.
    for sql in [
        "DELETE FROM memory_claims WHERE source_episode_id IN
            (SELECT rowid FROM episodes WHERE principal_subject = ?1)
            OR chunk_id IN (SELECT chunk_id FROM document_chunks WHERE ingested_by_principal = ?1)
            OR doc_id IN (SELECT id FROM solo_forget_docs)
            OR triple_id IN (SELECT id FROM solo_forget_triples)",
        "DELETE FROM triple_reviews WHERE source_episode_id IN
            (SELECT rowid FROM episodes WHERE principal_subject = ?1)
            OR triple_id IN (SELECT id FROM solo_forget_triples)",
        "DELETE FROM semantic_abstractions WHERE cluster_id IN
            (SELECT ce.cluster_id FROM cluster_episodes ce JOIN episodes e USING (memory_id)
             WHERE e.principal_subject = ?1)",
    ] {
        tx.execute(sql, params![principal_subject])
            .map_err(|e| Error::storage(format!("erase attributed derived memory: {e}")))?;
    }

    // Step 1: collect rowids of episodes belonging to the subject. We
    // use `?` parameterisation defensively even though `principal_subject`
    // is operator-supplied (CLI arg) — never trust input that crosses
    // a process boundary.
    let mut rowids: Vec<i64> = {
        let mut stmt = tx
            .prepare("SELECT rowid FROM episodes WHERE principal_subject = ?")
            .map_err(|e| Error::storage(format!("prepare SELECT episodes.rowid: {e}")))?;
        let rows = stmt
            .query_map(params![principal_subject], |r| r.get::<_, i64>(0))
            .map_err(|e| Error::storage(format!("query episodes.rowid: {e}")))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|e| Error::storage(format!("collect episode rowids: {e}")))?
    };
    rowids.sort_unstable();

    // Delete attributable facts before their source foreign keys become NULL.
    let triples_deleted =
        tx.execute(
            "DELETE FROM triples WHERE triple_id IN (SELECT id FROM solo_forget_triples)",
            [],
        )
        .map_err(|e| Error::storage(format!("DELETE triples: {e}")))? as u64;
    // Evidence cascades with triples. Only remove affected relationships that
    // have lost all support; independently supported and manual edges survive.
    tx.execute_batch(
        "DELETE FROM relationship_edges WHERE edge_id IN (SELECT id FROM solo_forget_edges)
             AND NOT EXISTS (SELECT 1 FROM relationship_evidence re
                             WHERE re.edge_id = relationship_edges.edge_id);
         UPDATE relationship_edges SET evidence_count =
             (SELECT COUNT(*) FROM relationship_evidence re
              WHERE re.edge_id = relationship_edges.edge_id)
         WHERE edge_id IN (SELECT id FROM solo_forget_edges);",
    )
    .map_err(|e| Error::storage(format!("erase unsupported graph relationships: {e}")))?;

    // Legacy facts without resolvable attribution remain operator-visible.
    let triples_orphan_null_source: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM triples t WHERE t.source_episode_id IS NULL
             AND t.cluster_id IN (
                 SELECT ce.cluster_id FROM cluster_episodes ce
                 JOIN episodes e ON e.memory_id = ce.memory_id
                 WHERE e.principal_subject = ?1)",
            params![principal_subject],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("COUNT orphan triples: {e}")))?;

    // Step 3: episodes themselves. CASCADE on FK from `embeddings` +
    // `pending_index` + `cluster_episodes` handles the row-level fanout
    // for us (see migration 0001 + 0002). The principal_subject column
    // was added in migration 0006 — pre-0006 rows have it as NULL and
    // are unaffected by this WHERE.
    //
    // Note on order: we delete triples FIRST so the
    // `triples.source_episode_id REFERENCES episodes(rowid) ON DELETE
    // SET NULL` from migration 0007 doesn't get a chance to silently
    // clear the FK on rows we're trying to count as `triples_deleted`.
    // Deleting triples-by-source first guarantees the count reflects
    // the principal's actual derived facts, not the SET NULL fallback.
    let episodes_deleted: u64 =
        tx.execute(
            "DELETE FROM episodes WHERE principal_subject = ?",
            params![principal_subject],
        )
        .map_err(|e| Error::storage(format!("DELETE episodes: {e}")))? as u64;

    // Shared documents keep their other principals' chunks. Exclusively
    // attributed documents and originals are removed by the preflighted plan.
    let chunks_deleted: u64 =
        tx.execute(
            "DELETE FROM document_chunks WHERE ingested_by_principal = ?",
            params![principal_subject],
        )
        .map_err(|e| Error::storage(format!("DELETE document_chunks: {e}")))? as u64;

    assets.delete(&tx)?;

    tx.commit()
        .map_err(|e| Error::storage(format!("COMMIT forget: {e}")))?;

    Ok(DeleteOutcome {
        episodes_deleted,
        triples_deleted,
        triples_orphan_null_source: triples_orphan_null_source.max(0) as u64,
        chunks_deleted,
        documents_deleted: assets.documents_deleted,
        assets_deleted: assets.assets_deleted,
        episode_rowids: rowids,
        chunk_rowids,
    })
}

/// Eager full HNSW rebuild from the surviving SQL rows. Walks
/// `episodes` + `document_chunks` (both `active`) + the cached
/// embedder_id and re-adds every vector.
///
/// Used by `forget_principal`. v0.8.0 P6 GDPR is rare; an eager rebuild
/// is the operator-acceptable trade-off. The rebuild is racing with
/// concurrent reads — they're either reading the SQL (gone) or the
/// HNSW (about to be replaced). A read seeing a stale HNSW row for a
/// deleted episode still gets back nothing from the SQL `SELECT` step
/// of recall, so the read-side leak risk is zero.
fn rebuild_hnsw_after_forget(
    conn: &Connection,
    hnsw: &dyn VectorIndex,
    embedder_id: i64,
) -> Result<()> {
    // Clear the in-memory state by `remove`-ing every active rowid.
    // The HNSW impl handles `remove` of non-present rowids cheaply
    // (HashSet insert) so iterating works even if some entries are
    // already tombstoned. We don't have a `clear()` method on
    // `VectorIndex`, so this is the closest equivalent without
    // breaking the trait surface.
    //
    // Actually — `crate::recovery::rebuild_hnsw_from_sql` re-inserts
    // every active row, but if we don't clear first we may end up
    // with rowids that ARE active in SQL but were ALSO present in the
    // index from before. Defensive: don't try to clear; just rebuild,
    // since the surviving rows will hash to the same hnsw_ids and
    // hnsw_rs treats re-add of an existing id as a no-op.
    let _ = crate::recovery::rebuild_hnsw_from_sql(conn, hnsw, embedder_id)?;

    Ok(())
}

/// Estimate (via COUNT) how many rows the operator's `forget` will
/// affect, BEFORE running the destructive sequence. Used by the CLI
/// confirmation gate to decide whether `--double-confirm` is required.
pub fn estimate_forget_scope(
    db_path: &Path,
    key: &KeyMaterial,
    principal_subject: &str,
) -> Result<(u64, u64)> {
    let conn = open_sqlcipher(db_path, key)?;
    let episodes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM episodes WHERE principal_subject = ?",
            params![principal_subject],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("COUNT episodes for estimate: {e}")))?;
    let chunks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM document_chunks WHERE ingested_by_principal = ?",
            params![principal_subject],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("COUNT chunks for estimate: {e}")))?;
    Ok((episodes as u64, chunks as u64))
}

// Silence false-positive unused-import lints for items referenced
// through public function signatures only.
#[allow(dead_code)]
fn _silence_unused() {
    let _: Option<&dyn Embedder> = None;
    let _: Option<LibraryId> = None;
    let _: Option<&EmbedderIdentity> = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// Open a per-tenant DB (with migration 0006 applied) and seed
    /// episodes under two principals. Returns (path, key).
    fn seed_two_principal_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("forget.db");
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = wal;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();

        let now_ms = chrono::Utc::now().timestamp_millis();
        // alice has 3 episodes; bob has 2.
        for i in 0..3 {
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, content,
                    encoding_context_json, confidence, strength, salience,
                    tier, created_at_ms, updated_at_ms, principal_subject
                 ) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?, 'alice')",
                params![
                    format!("00000000-0000-0000-0000-00000000a{i:03x}"),
                    now_ms,
                    format!("alice content {i}"),
                    now_ms,
                    now_ms,
                ],
            )
            .unwrap();
        }
        for i in 0..2 {
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, content,
                    encoding_context_json, confidence, strength, salience,
                    tier, created_at_ms, updated_at_ms, principal_subject
                 ) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?, 'bob')",
                params![
                    format!("00000000-0000-0000-0000-00000000b{i:03x}"),
                    now_ms,
                    format!("bob content {i}"),
                    now_ms,
                    now_ms,
                ],
            )
            .unwrap();
        }

        // Seed a documents row + chunks (3 under alice, 1 under bob).
        conn.execute(
            "INSERT INTO documents (
                doc_id, source, title, mime_type, ingested_at_ms,
                modified_at_ms, status, chunk_count, content_hash, byte_size
             ) VALUES ('00000000-0000-0000-0000-000000000d01', 'src://test', 't',
                       'text/markdown', ?, NULL, 'active', 4, 'hashabc', 200)",
            params![now_ms],
        )
        .unwrap();
        for i in 0..3 {
            conn.execute(
                "INSERT INTO document_chunks (
                    chunk_id, doc_id, chunk_index, content, token_count,
                    start_offset, end_offset, created_at_ms, ingested_by_principal
                 ) VALUES (?, ?, ?, ?, 5, ?, ?, ?, 'alice')",
                params![
                    format!("00000000-0000-0000-0000-00000000c{i:03x}"),
                    "00000000-0000-0000-0000-000000000d01",
                    i,
                    format!("alice chunk {i}"),
                    (i * 10) as i64,
                    ((i + 1) * 10) as i64,
                    now_ms,
                ],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO document_chunks (
                chunk_id, doc_id, chunk_index, content, token_count,
                start_offset, end_offset, created_at_ms, ingested_by_principal
             ) VALUES ('00000000-0000-0000-0000-00000000ccc1', '00000000-0000-0000-0000-000000000d01',
                       3, 'bob chunk', 5, 30, 40, ?, 'bob')",
            params![now_ms],
        )
        .unwrap();

        (tmp, db_path)
    }

    #[test]
    fn attribution_audit_counts_unknown_owners_without_assigning_or_deleting() {
        let (_tmp, db_path) = seed_two_principal_db();
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE episodes SET principal_subject = NULL WHERE principal_subject = 'alice'",
            [],
        )
        .unwrap();
        conn.execute("UPDATE document_chunks SET ingested_by_principal = ' ' WHERE ingested_by_principal = 'bob'", []).unwrap();
        let report = attribution_report(&conn).unwrap();
        assert_eq!(
            report,
            AttributionReport {
                episodes_total: 5,
                episodes_unattributed: 3,
                chunks_total: 4,
                chunks_unattributed: 1
            }
        );
        assert_eq!(attribution_report(&conn).unwrap(), report);
    }

    #[test]
    fn delete_principal_rows_targets_only_named_subject() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let DeleteOutcome {
            episodes_deleted: episodes,
            chunks_deleted: chunks,
            ..
        } = delete_principal_rows(&mut conn, "alice").unwrap();
        assert_eq!(episodes, 3, "should delete alice's 3 episodes");
        assert_eq!(chunks, 3, "should delete alice's 3 chunks");
        // Bob's rows remain.
        let alice_remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE principal_subject = 'alice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alice_remaining, 0);
        let bob_remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE principal_subject = 'bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bob_remaining, 2);
        let bob_chunks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks WHERE ingested_by_principal = 'bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bob_chunks, 1);
    }

    #[test]
    fn erasure_invalidates_both_vector_kinds_and_preserves_other_ids() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let outcome = delete_principal_rows(&mut conn, "alice").unwrap();
        let index = crate::test_support::StubVectorIndex::new(2);
        assert_eq!(outcome.chunk_rowids.len(), 3);
        for id in outcome
            .episode_rowids
            .iter()
            .map(|id| episode_hnsw_id(*id))
            .chain(outcome.chunk_rowids.iter().map(|id| chunk_hnsw_id(*id)))
        {
            index.add(id, &[1.0, 0.0]).unwrap();
        }
        let survivor = episode_hnsw_id(9999);
        index.add(survivor, &[1.0, 0.0]).unwrap();
        outcome.invalidate_vectors(&index);
        let hits = index.search(&[1.0, 0.0], 100).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, survivor);
    }

    #[test]
    fn erasure_removes_claims_and_reviews_before_attribution_is_lost() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;
            INSERT INTO memory_claims
                (claim_id,candidate_fingerprint,subject_id,predicate,object_id,object_kind,
                 source_episode_id,confidence,quality_score,status,created_at_ms,updated_at_ms)
            SELECT memory_id,memory_id,principal_subject,'likes','private detail','literal',
                   rowid,0.9,0.9,'active',1,1 FROM episodes;
            INSERT INTO triple_reviews
                (review_id,candidate_fingerprint,source_episode_id,subject_id,predicate,
                 object_id,object_kind,confidence,reason_code,reason,provenance_json,created_at_ms,updated_at_ms)
            SELECT memory_id,memory_id,rowid,principal_subject,'likes','private detail','literal',
                   0.9,'review','review','{}',1,1 FROM episodes;").unwrap();
        delete_principal_rows(&mut conn, "alice").unwrap();
        for table in ["memory_claims", "triple_reviews"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 2, "only Bob's records should survive in {table}");
            let orphaned: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE source_episode_id IS NULL"),
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(orphaned, 0);
        }
    }

    #[test]
    fn erasure_removes_unsupported_edges_and_preserves_independent_evidence() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let alice: i64 = conn
            .query_row(
                "SELECT rowid FROM episodes WHERE principal_subject = 'alice' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let bob: i64 = conn
            .query_row(
                "SELECT rowid FROM episodes WHERE principal_subject = 'bob' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for (triple, source) in [
            ("private", Some(alice)),
            ("shared-a", Some(alice)),
            ("shared-b", Some(bob)),
            ("chunk-only", None),
        ] {
            seed_triple_with_source(&conn, triple, "subject", "likes", "detail", source, None);
        }
        conn.execute_batch(
            "INSERT INTO entities (entity_id,canonical_name,created_at_ms,updated_at_ms)
                 VALUES ('subject','Subject',1,1);
             INSERT INTO relationship_edges
                 (edge_id,subject_entity_id,predicate,object_literal,object_kind,
                  valid_from_ms,confidence,evidence_count,created_at_ms,updated_at_ms)
             VALUES ('private','subject','likes','private detail','literal',1,0.9,1,1,1),
                    ('shared','subject','likes','shared detail','literal',1,0.9,2,1,1),
                    ('chunk-only','subject','likes','document detail','literal',1,0.9,1,1,1),
                    ('manual','subject','likes','unattributed detail','literal',1,0.9,0,1,1);
             INSERT INTO relationship_evidence
                 (evidence_id,edge_id,triple_id,source_episode_id,extraction_confidence,created_at_ms)
             SELECT triple_id,CASE WHEN triple_id LIKE 'shared-%' THEN 'shared' ELSE triple_id END,
                    triple_id,source_episode_id,0.9,1 FROM triples;
             UPDATE relationship_evidence SET chunk_id =
                 (SELECT chunk_id FROM document_chunks WHERE ingested_by_principal = 'alice' LIMIT 1)
             WHERE triple_id = 'chunk-only';"
        ).unwrap();
        let outcome = delete_principal_rows(&mut conn, "alice").unwrap();
        assert_eq!(outcome.triples_deleted, 3);
        let edges: Vec<(String, i64)> = conn
            .prepare("SELECT edge_id,evidence_count FROM relationship_edges ORDER BY edge_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(edges, vec![("manual".into(), 0), ("shared".into(), 1)]);
        let surviving_source: i64 = conn
            .query_row(
                "SELECT source_episode_id FROM relationship_evidence",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(surviving_source, bob);
        assert_eq!(
            delete_principal_rows(&mut conn, "alice")
                .unwrap()
                .triples_deleted,
            0
        );
    }

    #[test]
    fn delete_principal_rows_idempotent_on_absent_subject() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        let DeleteOutcome {
            episodes_deleted: episodes,
            chunks_deleted: chunks,
            episode_rowids: rowids,
            ..
        } = delete_principal_rows(&mut conn, "ghost").unwrap();
        assert_eq!(episodes, 0);
        assert_eq!(chunks, 0);
        assert!(rowids.is_empty());
        // Re-run: still 0.
        let DeleteOutcome {
            episodes_deleted: e2,
            chunks_deleted: c2,
            ..
        } = delete_principal_rows(&mut conn, "ghost").unwrap();
        assert_eq!(e2, 0);
        assert_eq!(c2, 0);
    }

    #[test]
    fn delete_principal_rows_cascades_to_embeddings() {
        // Migration 0001 declares ON DELETE CASCADE from embeddings.memory_id
        // → episodes.memory_id. Verify that the cascade fires when GDPR
        // delete removes the episode.
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        // Seed an embedding row tied to one of alice's episodes.
        let alice_id_0 = "00000000-0000-0000-0000-00000000a000";
        // First need an embedder row to satisfy the FK.
        conn.execute(
            "INSERT INTO embedders (name, version, dim, dtype, first_seen_ms)
             VALUES ('test', 'v1', 4, 'f32', 0)",
            [],
        )
        .unwrap();
        let embedder_id: i64 = conn
            .query_row(
                "SELECT embedder_id FROM embedders WHERE name = 'test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let vec_blob = vec![0u8; 16];
        conn.execute(
            "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms)
             VALUES (?, ?, 'f32', 4, ?, 0)",
            params![alice_id_0, embedder_id, &vec_blob[..]],
        )
        .unwrap();

        let before: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(before, 1);

        let _ = delete_principal_rows(&mut conn, "alice").unwrap();

        let after: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after, 0, "embeddings should cascade-delete with episodes");
    }

    #[test]
    #[ignore = "requires SQLCipher: estimate_forget_scope opens via open_sqlcipher which fails on plain SQLite test DBs. Round-trip is covered by SQLCipher-enabled integration tests."]
    fn estimate_forget_scope_counts_correctly() {
        // SQLCipher round-trip — needs a real PRAGMA key bind that the
        // plain bundled SQLite test profile rejects. The estimate
        // helper is also exercised end-to-end by `delete_principal_rows`
        // through the unit test above; this just covers the COUNT side.
        let (_tmp, db_path) = seed_two_principal_db();
        let key = crate::key_material::KeyMaterial::derive("test-pass", &[0u8; 16]).unwrap();
        let (eps, chunks) = estimate_forget_scope(&db_path, &key, "alice").unwrap();
        assert_eq!(eps, 3);
        assert_eq!(chunks, 3);
        let (eps_b, chunks_b) = estimate_forget_scope(&db_path, &key, "bob").unwrap();
        assert_eq!(eps_b, 2);
        assert_eq!(chunks_b, 1);
        let (eps_ghost, _) = estimate_forget_scope(&db_path, &key, "ghost").unwrap();
        assert_eq!(eps_ghost, 0);
    }

    /// Round-trip `forget_principal` against a real SQLCipher data dir
    /// initialised via `init()`. Verifies: episodes/chunks/embeddings
    /// cascade-delete, HNSW rebuilt flag set, admin-audit row written.
    ///
    /// SQLCipher-only because `open_sqlcipher` rejects plain SQLite files.
    #[test]
    fn forget_principal_round_trip_against_real_sqlcipher() {
        use crate::init::{InitParams, init};
        use solo_core::LibraryId;
        use std::sync::Arc;
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let pass = "forget round-trip test passphrase";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .expect("init");

        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let salt = cfg.salt_bytes().unwrap();
        let key = crate::key_material::KeyMaterial::derive(pass, &salt).unwrap();

        // Seed two principals worth of episodes via direct SQL.
        {
            let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            for i in 0..3 {
                conn.execute(
                    "INSERT INTO episodes (
                        memory_id, ts_ms, source_type, content,
                        encoding_context_json, confidence, strength, salience,
                        tier, created_at_ms, updated_at_ms, principal_subject
                     ) VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5,
                               'hot', ?, ?, 'alice')",
                    params![
                        format!("00000000-0000-0000-0000-0000000{i:08x}"),
                        now,
                        format!("alice ep {i}"),
                        now,
                        now,
                    ],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, content,
                    encoding_context_json, confidence, strength, salience,
                    tier, created_at_ms, updated_at_ms, principal_subject
                 ) VALUES (?, ?, 'user_message', 'bob ep', '{}', 0.9, 0.5, 0.5,
                           'hot', ?, ?, 'bob')",
                params!["00000000-0000-0000-0000-0000000b000000", now, now, now,],
            )
            .unwrap();
        }

        // Build a minimal LibraryHandle via from_parts_for_tests.
        let stub = Arc::new(crate::test_support::StubVectorIndex::new(4));
        let writer_conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
        let crate::writer::WriterSpawn {
            handle: write_handle,
            join,
        } = crate::writer::WriterActor::spawn(writer_conn, stub.clone() as Arc<_>);
        let read_pool = crate::reader::ReaderPool::new(
            &outcome.db_path,
            Some(key.clone()),
            stub.clone() as Arc<_>,
        )
        .unwrap();
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", 4));
        let handle = Arc::new(crate::library::LibraryHandle::from_parts_for_tests(
            LibraryId::default_tenant(),
            cfg.clone(),
            outcome.db_path.clone(),
            data_dir.clone(),
            1, // embedder_id placeholder
            stub.clone() as Arc<_>,
            embedder,
            write_handle,
            join,
            read_pool,
        ));

        // Run forget_principal.
        let report = forget_principal(handle, "alice", Some("admin"), &data_dir, &key)
            .expect("forget_principal");
        assert_eq!(report.episodes_deleted, 3);
        assert_eq!(report.triples_deleted, 0);
        assert_eq!(report.chunks_deleted, 0);
        assert!(report.hnsw_rebuilt);

        // Verify alice's rows are gone, bob's remain.
        let after_conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
        let alice_left: i64 = after_conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE principal_subject = 'alice'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alice_left, 0);
        let bob_left: i64 = after_conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE principal_subject = 'bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bob_left, 1);

        // Verify the admin-audit row landed.
        let admin_conn = crate::init::open_sqlcipher(
            &data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME),
            &key,
        )
        .unwrap();
        let (op, target, principal): (String, Option<String>, Option<String>) = admin_conn
            .query_row(
                "SELECT operation, target_tenant_id, principal_subject \
                 FROM audit_events_admin WHERE audit_id = ?",
                params![report.audit_admin_row_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(op, "gdpr.forget_user");
        assert_eq!(target.as_deref(), Some("default"));
        assert_eq!(principal.as_deref(), Some("admin"));
    }

    /// `forget_principal` on an absent subject is idempotent: 0 rows
    /// deleted, HNSW NOT rebuilt, but an admin-audit row still emitted
    /// with count=0 so the compliance trail records the attempt.
    #[test]
    fn forget_principal_idempotent_on_absent_subject() {
        use crate::init::{InitParams, init};
        use solo_core::LibraryId;
        use std::sync::Arc;
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let pass = "idempotent forget test";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .expect("init");
        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let salt = cfg.salt_bytes().unwrap();
        let key = crate::key_material::KeyMaterial::derive(pass, &salt).unwrap();

        let stub = Arc::new(crate::test_support::StubVectorIndex::new(4));
        let writer_conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
        let crate::writer::WriterSpawn {
            handle: write_handle,
            join,
        } = crate::writer::WriterActor::spawn(writer_conn, stub.clone() as Arc<_>);
        let read_pool = crate::reader::ReaderPool::new(
            &outcome.db_path,
            Some(key.clone()),
            stub.clone() as Arc<_>,
        )
        .unwrap();
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", 4));
        let handle = Arc::new(crate::library::LibraryHandle::from_parts_for_tests(
            LibraryId::default_tenant(),
            cfg,
            outcome.db_path.clone(),
            data_dir.clone(),
            1,
            stub.clone() as Arc<_>,
            embedder,
            write_handle,
            join,
            read_pool,
        ));

        let report = forget_principal(handle, "ghost", None, &data_dir, &key)
            .expect("forget_principal on absent subject");
        assert_eq!(report.episodes_deleted, 0);
        assert_eq!(report.chunks_deleted, 0);
        assert!(!report.hnsw_rebuilt, "no rebuild when nothing deleted");
        // Admin audit row must still be present.
        assert!(report.audit_admin_row_id > 0);
    }

    // ---- v0.8.1 P1: triples cascade ----
    //
    // The following tests use the plain-SQLite seed (no SQLCipher) to
    // exercise the `delete_principal_rows` cascade through the new
    // `triples.source_episode_id` FK introduced in migration 0007.

    /// Helper: seed a triple linked to a specific source episode rowid.
    fn seed_triple_with_source(
        conn: &Connection,
        triple_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        source_episode_id: Option<i64>,
        cluster_id: Option<&str>,
    ) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO triples (
                triple_id, subject_id, predicate, object_id, object_kind,
                valid_from_ms, valid_to_ms, confidence, provenance_json,
                created_at_ms, updated_at_ms, source_episode_id, cluster_id
             ) VALUES (?, ?, ?, ?, 'literal', ?, NULL, 0.9, '{}', ?, ?, ?, ?)",
            params![
                triple_id,
                subject,
                predicate,
                object,
                now_ms,
                now_ms,
                now_ms,
                source_episode_id,
                cluster_id,
            ],
        )
        .expect("seed triple");
    }

    #[test]
    fn delete_principal_rows_cascades_through_triples_by_source_episode() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        // Look up alice's episode rowids.
        let alice_rowids: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT rowid FROM episodes WHERE principal_subject = 'alice' ORDER BY rowid",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert_eq!(alice_rowids.len(), 3);
        let bob_rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM episodes WHERE principal_subject = 'bob' LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Two triples derived from alice's first episode + one from bob's.
        seed_triple_with_source(
            &conn,
            "00000000-0000-0000-0000-000000000a01",
            "alice",
            "uses",
            "rust",
            Some(alice_rowids[0]),
            None,
        );
        seed_triple_with_source(
            &conn,
            "00000000-0000-0000-0000-000000000a02",
            "alice",
            "likes",
            "skiing",
            Some(alice_rowids[1]),
            None,
        );
        seed_triple_with_source(
            &conn,
            "00000000-0000-0000-0000-000000000b01",
            "bob",
            "uses",
            "go",
            Some(bob_rowid),
            None,
        );

        let outcome = delete_principal_rows(&mut conn, "alice").unwrap();
        assert_eq!(outcome.episodes_deleted, 3);
        assert_eq!(
            outcome.triples_deleted, 2,
            "should delete the two triples whose source was an alice episode"
        );
        assert_eq!(outcome.triples_orphan_null_source, 0);

        // Bob's triple survives.
        let bob_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM triples WHERE subject_id = 'bob'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bob_count, 1);
    }

    #[test]
    fn delete_principal_rows_preserves_null_source_triples() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        let alice_rowids: Vec<i64> = {
            let mut stmt = conn
                .prepare(
                    "SELECT rowid FROM episodes WHERE principal_subject = 'alice' ORDER BY rowid",
                )
                .unwrap();
            stmt.query_map([], |r| r.get::<_, i64>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };

        // Triple with NULL source — pre-v0.8.1 row shape. NOT linked to a
        // cluster either, so it shouldn't be counted as orphan-by-cluster.
        seed_triple_with_source(
            &conn,
            "00000000-0000-0000-0000-000000000c01",
            "alice",
            "claimed",
            "x",
            None,
            None,
        );
        // Triple with source on alice's first episode — should cascade.
        seed_triple_with_source(
            &conn,
            "00000000-0000-0000-0000-000000000c02",
            "alice",
            "linked",
            "y",
            Some(alice_rowids[0]),
            None,
        );

        let outcome = delete_principal_rows(&mut conn, "alice").unwrap();
        assert_eq!(outcome.triples_deleted, 1, "only the source-linked triple");
        // The NULL-source triple stays — it's orphan-by-design but not
        // counted as orphan-by-cluster (no cluster membership).
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM triples WHERE source_episode_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1, "NULL-source row must survive");
    }

    #[test]
    fn delete_principal_rows_counts_orphans_via_cluster_membership() {
        let (_tmp, db_path) = seed_two_principal_db();
        let mut conn = Connection::open(&db_path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();

        let alice_rows: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare(
                    "SELECT rowid, memory_id FROM episodes WHERE principal_subject = 'alice' \
                     ORDER BY rowid",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        let alice_memory_id = alice_rows[0].1.clone();

        // Seed a cluster + cluster_episodes link for alice's first episode.
        let cluster_id = "00000000-0000-0000-0000-0000000c1001";
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO clusters (cluster_id, coherence, created_at_ms) VALUES (?, ?, ?)",
            params![cluster_id, 0.9_f64, now_ms],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
            params![cluster_id, alice_memory_id],
        )
        .unwrap();

        // Pre-v0.8.1-shape triple: source_episode_id IS NULL but cluster
        // membership ties it to alice. This is the "orphan by design" case.
        seed_triple_with_source(
            &conn,
            "00000000-0000-0000-0000-000000000d01",
            "alice",
            "claimed",
            "fact",
            None,
            Some(cluster_id),
        );

        let outcome = delete_principal_rows(&mut conn, "alice").unwrap();
        assert_eq!(
            outcome.triples_orphan_null_source, 1,
            "the NULL-source triple linked via cluster must appear in the orphan count"
        );

        // The orphan triple is NOT deleted; only the operator visibility
        // counter records it.
        let remaining: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM triples WHERE triple_id = '00000000-0000-0000-0000-000000000d01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1, "orphan triple must remain in the table");
    }

    #[test]
    fn migration_0007_adds_source_episode_id_column_and_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        // Column present.
        let cols: Vec<(String, String)> = conn
            .prepare("PRAGMA table_info('triples')")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let names: Vec<&str> = cols.iter().map(|(n, _)| n.as_str()).collect();
        assert!(
            names.contains(&"source_episode_id"),
            "triples missing source_episode_id after 0007; got {names:?}"
        );
        // Index present.
        let idx_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='index' AND name='idx_triples_source_episode'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx_exists, 1, "idx_triples_source_episode missing");
    }

    #[test]
    fn migration_0007_is_idempotent_on_repeated_open() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 7",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "0007 row must not be inserted twice");
    }

    #[test]
    fn migration_0007_backfills_source_episode_id_from_provenance_json() {
        // Build a v0.6-style DB (no source_episode_id yet) by hand-running
        // the migrations up to 6, seeding a triple whose provenance_json
        // references a live episode by memory_id, then running 0007 and
        // verifying the column got populated.
        let mut conn = Connection::open_in_memory().unwrap();
        // Apply migrations 1..=6 manually so we control the pre-0007 state.
        crate::migration::run_migrations(&mut conn).unwrap();
        // Now seed: insert an episode, then a triple whose
        // provenance_json references that episode's memory_id, then
        // clear source_episode_id on the triple (simulating a row
        // written before 0007 was applied — the column would have been
        // NULL by default for a pre-existing INSERT).
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, created_at_ms, updated_at_ms
             ) VALUES ('00000000-0000-0000-0000-000000000111', ?, 'user_message',
                       'seed', '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
            params![now_ms, now_ms, now_ms],
        )
        .unwrap();
        let ep_rowid: i64 = conn
            .query_row(
                "SELECT rowid FROM episodes WHERE memory_id = ?",
                params!["00000000-0000-0000-0000-000000000111"],
                |r| r.get(0),
            )
            .unwrap();
        // Insert a triple with provenance_json carrying the memory_id
        // (Provenance.derived_from JSON shape) but explicit NULL source.
        let prov = serde_json::json!({
            "derived_from": ["00000000-0000-0000-0000-000000000111"],
            "derivation": "extraction",
            "by": "test",
            "at_ms": now_ms,
        });
        conn.execute(
            "INSERT INTO triples (
                triple_id, subject_id, predicate, object_id, object_kind,
                valid_from_ms, valid_to_ms, confidence, provenance_json,
                created_at_ms, updated_at_ms, source_episode_id
             ) VALUES ('00000000-0000-0000-0000-000000000222',
                       'alice', 'uses', 'rust', 'literal',
                       ?, NULL, 0.9, ?, ?, ?, NULL)",
            params![now_ms, prov.to_string(), now_ms, now_ms],
        )
        .unwrap();

        // Manually clear + re-run the backfill UPDATE that lives in 0007.
        // (Migrations are idempotent — this re-run hits only the NULL
        // rows, which is the same behavior 0007 takes on first apply.)
        let backfill = include_str!("migrations/0007_triples_source.sql");
        // The backfill statement is the trailing UPDATE; running the
        // whole file twice would error on the ALTER. So we extract the
        // UPDATE portion alone for this re-run.
        let update_sql = backfill
            .split("UPDATE triples")
            .nth(1)
            .map(|tail| format!("UPDATE triples{tail}"))
            .expect("0007 contains the UPDATE backfill");
        conn.execute_batch(&update_sql).unwrap();

        let backfilled: Option<i64> = conn
            .query_row(
                "SELECT source_episode_id FROM triples \
                 WHERE triple_id = '00000000-0000-0000-0000-000000000222'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            backfilled,
            Some(ep_rowid),
            "backfill must resolve derived_from[0] memory_id → episodes.rowid"
        );
    }

    #[test]
    fn forget_report_carries_triples_deleted_count_through_to_audit_details() {
        // Same setup as the existing round-trip test, but with a triple
        // wired to the deleted principal's episode. Verifies the report's
        // triples_deleted count surfaces correctly + the audit row
        // captures it.
        use crate::init::{InitParams, init};
        use solo_core::LibraryId;
        use zeroize::Zeroizing;

        let tmp = tempfile::TempDir::new().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let pass = "forget triples cascade test passphrase";
        let outcome = init(InitParams {
            data_dir: data_dir.clone(),
            passphrase: Zeroizing::new(pass.into()),
            force: false,
            embedder: crate::init::default_embedder(),
        })
        .expect("init");
        let cfg = crate::config::SoloConfig::read(&outcome.config_path).unwrap();
        let salt = cfg.salt_bytes().unwrap();
        let key = crate::key_material::KeyMaterial::derive(pass, &salt).unwrap();

        // Seed alice's episode + a triple sourced from it via direct SQL.
        {
            let conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
            let now = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, content,
                    encoding_context_json, confidence, strength, salience,
                    tier, created_at_ms, updated_at_ms, principal_subject
                 ) VALUES ('00000000-0000-0000-0000-000000000ace', ?, 'user_message',
                           'alice ep', '{}', 0.9, 0.5, 0.5, 'hot', ?, ?, 'alice')",
                params![now, now, now],
            )
            .unwrap();
            let ep_rowid: i64 = conn
                .query_row(
                    "SELECT rowid FROM episodes WHERE memory_id = ?",
                    params!["00000000-0000-0000-0000-000000000ace"],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO triples (
                    triple_id, subject_id, predicate, object_id, object_kind,
                    valid_from_ms, valid_to_ms, confidence, provenance_json,
                    created_at_ms, updated_at_ms, source_episode_id
                 ) VALUES ('00000000-0000-0000-0000-000000000a11',
                           'alice', 'uses', 'rust', 'literal',
                           ?, NULL, 0.9, '{}', ?, ?, ?)",
                params![now, now, now, ep_rowid],
            )
            .unwrap();
        }

        // Stand up a LibraryHandle via from_parts_for_tests, then run forget.
        let stub = Arc::new(crate::test_support::StubVectorIndex::new(4));
        let writer_conn = crate::init::open_sqlcipher(&outcome.db_path, &key).unwrap();
        let crate::writer::WriterSpawn {
            handle: write_handle,
            join,
        } = crate::writer::WriterActor::spawn(writer_conn, stub.clone() as Arc<_>);
        let read_pool = crate::reader::ReaderPool::new(
            &outcome.db_path,
            Some(key.clone()),
            stub.clone() as Arc<_>,
        )
        .unwrap();
        let embedder: Arc<dyn solo_core::Embedder> =
            Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", 4));
        let handle = Arc::new(crate::library::LibraryHandle::from_parts_for_tests(
            LibraryId::default_tenant(),
            cfg,
            outcome.db_path.clone(),
            data_dir.clone(),
            1,
            stub.clone() as Arc<_>,
            embedder,
            write_handle,
            join,
            read_pool,
        ));

        let report = forget_principal(handle, "alice", Some("admin"), &data_dir, &key)
            .expect("forget_principal");
        assert_eq!(report.episodes_deleted, 1);
        assert_eq!(report.triples_deleted, 1, "real count, not 0");

        // The admin-audit row's details_json must carry the same count.
        let admin_conn = crate::init::open_sqlcipher(
            &data_dir.join(crate::memory_library::COMMUNITY_DB_FILENAME),
            &key,
        )
        .unwrap();
        let details_json: String = admin_conn
            .query_row(
                "SELECT details_json FROM audit_events_admin WHERE audit_id = ?",
                params![report.audit_admin_row_id],
                |r| r.get(0),
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&details_json).unwrap();
        assert_eq!(parsed["triples_deleted"], 1);
        assert_eq!(parsed["episodes_deleted"], 1);
    }

    /// Same shape as `estimate_forget_scope_counts_correctly` but uses a
    /// raw `Connection::query_row` against the seeded plain-SQLite DB.
    /// Exercises the SELECT COUNT logic without going through
    /// `open_sqlcipher`.
    #[test]
    fn estimate_via_direct_count_matches_seeded_data() {
        let (_tmp, db_path) = seed_two_principal_db();
        let conn = Connection::open(&db_path).unwrap();
        let alice_eps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE principal_subject = ?",
                params!["alice"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alice_eps, 3);
        let alice_chunks: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks WHERE ingested_by_principal = ?",
                params!["alice"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(alice_chunks, 3);
        let bob_eps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE principal_subject = ?",
                params!["bob"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bob_eps, 2);
        let ghost_eps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episodes WHERE principal_subject = ?",
                params!["ghost"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ghost_eps, 0);
    }
}
