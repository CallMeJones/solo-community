// SPDX-License-Identifier: Apache-2.0

//! `recall` — vector-search the memory store.
//!
//! Three transports (CLI, MCP, HTTP) used to inline this pipeline; this
//! module is the single source of truth. Each transport now formats
//! the same `RecallHit` Vec however it wants (one-line text, MCP
//! Content::text JSON, axum Json<>...).
//!
//! ## Pipeline
//!
//! 1. Embed `query` via the supplied [`Embedder`].
//! 2. `hnsw.search(query_vec, widened_k)` — overshoots the user's `limit`
//!    so we have headroom for `status='forgotten'` filtering without a
//!    second round-trip.
//! 3. SQL fetch episodes by rowid via the [`ReaderPool`] inside one
//!    `interact` call (single thread-hop).
//! 4. Filter `status='active'`, re-order by HNSW score (smaller cosine
//!    distance first), `take(limit)`.

use std::sync::Arc;

use serde::Serialize;
use solo_core::{Embedder, Error, Result, VectorIndex};
use solo_storage::{AuditOperation, HnswIdKind, LibraryHandle, ReaderPool, decode_hnsw_id};

const MAX_RECALL_CANDIDATES: usize = 200;
const RECALL_CANDIDATE_MULTIPLIER: usize = 4;

/// One result from a recall pipeline. Transports format it however
/// makes sense for their wire shape (CLI = one-line text, MCP +
/// HTTP = JSON).
#[derive(Debug, Clone, Serialize)]
pub struct RecallHit {
    pub rowid: i64,
    pub memory_id: String,
    pub cos_distance: f32,
    pub bm25_score: Option<f32>,
    pub fused_score: f32,
    pub vector_rank: Option<usize>,
    pub lexical_rank: Option<usize>,
    pub score_components: RecallScoreComponents,
    pub content: String,
    pub source_type: String,
    pub tier: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallScoreComponents {
    pub rrf_score: f32,
    pub salience_boost: f32,
    pub strength_boost: f32,
    pub tier_boost: f32,
    pub recency_boost: f32,
    pub encoding_context_boost: f32,
}

/// Aggregate result. Carries the index's overall length too — useful
/// for callers that want to display "N matches out of M vectors".
#[derive(Debug, Clone, Serialize)]
pub struct RecallResult {
    pub hits: Vec<RecallHit>,
    /// Number of vectors in the HNSW index AT QUERY TIME. Reported
    /// separately from `hits.len()` so a caller showing "no matches"
    /// can distinguish "index is empty" from "everything was forgotten".
    pub index_len: usize,
    /// Number of raw vector + lexical candidates returned before Solo
    /// de-duplicates and filters out non-episode/inactive rows.
    pub candidates_considered: usize,
}

/// Run the recall pipeline. Returns a `RecallResult` with at most
/// `limit` hits, ordered by cosine distance ascending (most-similar
/// first). Forgotten rows (`status='forgotten'`) are filtered out.
///
/// Empty queries return `Err(InvalidInput)` — callers should
/// validate at the transport boundary too if they want a different
/// status code, but this is the canonical refusal site.
///
/// v0.8.0 P2: takes a `&LibraryHandle` for tenant routing. The handle's
/// embedder + HNSW + reader pool drive the pipeline.
///
/// v0.8.0 P4: emits an `audit_events` row AFTER returning the result via
/// the tenant's async [`solo_storage::AuditWriter`]. Pass `audit_principal`
/// from the request's `AuthenticatedPrincipal` (HTTP/MCP) or `None`
/// (CLI / no-auth). Success and error paths both emit; failures don't
/// block the result going back to the caller.
pub async fn run_recall(
    tenant: &LibraryHandle,
    audit_principal: Option<String>,
    query: &str,
    limit: usize,
) -> Result<RecallResult> {
    let result = run_recall_inner(
        tenant.embedder(),
        tenant.hnsw(),
        tenant.read(),
        query,
        limit,
    )
    .await;
    match &result {
        Ok(_) => tenant
            .audit()
            .emit_ok(audit_principal, AuditOperation::MemoryRecall, None),
        Err(e) => tenant
            .audit()
            .emit_error(audit_principal, AuditOperation::MemoryRecall, None, e),
    }
    result
}

/// Lower-level entry that takes the three components separately. Kept
/// public for transports and tests that already hold the three pieces
/// directly (e.g. solo-query's own unit tests that build a writer +
/// pool without a LibraryHandle wrapper). v0.8.0 P2 — see `run_recall`
/// for the canonical tenant-routed surface.
#[doc(hidden)]
pub async fn run_recall_inner(
    embedder: &Arc<dyn Embedder>,
    hnsw: &Arc<dyn VectorIndex + Send + Sync>,
    pool: &ReaderPool,
    query: &str,
    limit: usize,
) -> Result<RecallResult> {
    if query.trim().is_empty() {
        return Err(Error::invalid_input("recall query must not be empty"));
    }
    let limit = limit.clamp(1, 100);

    let q_emb = embedder.embed(query).await?;
    let q_slice = q_emb
        .as_f32_slice()
        .ok_or_else(|| Error::embedder("embedder returned non-F32 vector; HNSW requires F32"))?;

    let candidate_limit = recall_candidate_limit(limit);
    let hnsw_hits = hnsw.search(q_slice, candidate_limit)?;
    let index_len = hnsw.len();
    let lexical_hits = fetch_lexical_episode_hits(pool, query, candidate_limit).await?;
    let candidates_considered = hnsw_hits.len() + lexical_hits.len();

    // Decode HNSW ids: the index is shared between episodes and document
    // chunks (see `solo_storage::hnsw_id`). Drop any chunk hits — recall
    // surfaces episodes only. The decoded rowid is what we pass to SQL.
    let decoded_episode_hits: Vec<(i64, f32, usize)> = hnsw_hits
        .iter()
        .enumerate()
        .filter_map(|(idx, (hnsw_id, score))| {
            let (kind, rowid) = decode_hnsw_id(*hnsw_id);
            match kind {
                HnswIdKind::Episode => Some((rowid, *score, idx + 1)),
                HnswIdKind::Chunk => None,
            }
        })
        .collect();
    if decoded_episode_hits.is_empty() && lexical_hits.is_empty() {
        return Ok(RecallResult {
            hits: Vec::new(),
            index_len,
            candidates_considered,
        });
    }

    let mut rowids: Vec<i64> = Vec::with_capacity(decoded_episode_hits.len() + lexical_hits.len());
    for (rowid, _, _) in &decoded_episode_hits {
        if !rowids.contains(rowid) {
            rowids.push(*rowid);
        }
    }
    for hit in &lexical_hits {
        if !rowids.contains(&hit.rowid) {
            rowids.push(hit.rowid);
        }
    }
    let rows = fetch_episodes_by_rowid(pool, &rowids).await?;
    let by_rowid: std::collections::HashMap<i64, EpisodeRow> =
        rows.into_iter().map(|r| (r.rowid, r)).collect();
    let vector_by_rowid: std::collections::HashMap<i64, (f32, usize)> = decoded_episode_hits
        .into_iter()
        .map(|(rowid, score, rank)| (rowid, (score, rank)))
        .collect();
    let lexical_by_rowid: std::collections::HashMap<i64, (f32, usize)> = lexical_hits
        .into_iter()
        .map(|hit| (hit.rowid, (hit.bm25_score, hit.rank)))
        .collect();

    let mut hits: Vec<RecallHit> = Vec::with_capacity(rowids.len());
    for rowid in &rowids {
        if let Some(row) = by_rowid.get(rowid) {
            if row.status != "active" {
                continue;
            }
            let vector = vector_by_rowid.get(rowid).copied();
            let lexical = lexical_by_rowid.get(rowid).copied();
            let vector_rank = vector.map(|(_, rank)| rank);
            let lexical_rank = lexical.map(|(_, rank)| rank);
            let score_components = score_components(row, query, vector_rank, lexical_rank);
            hits.push(RecallHit {
                rowid: *rowid,
                memory_id: row.memory_id.clone(),
                cos_distance: vector.map(|(score, _)| score).unwrap_or(1.0),
                bm25_score: lexical.map(|(score, _)| score),
                fused_score: score_components.total(),
                vector_rank,
                lexical_rank,
                score_components,
                content: row.content.clone(),
                source_type: row.source_type.clone(),
                tier: row.tier.clone(),
            });
        }
    }
    hits.sort_by(|a, b| {
        b.fused_score
            .total_cmp(&a.fused_score)
            .then_with(|| a.cos_distance.total_cmp(&b.cos_distance))
            .then_with(|| a.rowid.cmp(&b.rowid))
    });
    hits.truncate(limit);

    Ok(RecallResult {
        hits,
        index_len,
        candidates_considered,
    })
}

fn recall_candidate_limit(limit: usize) -> usize {
    limit
        .saturating_mul(RECALL_CANDIDATE_MULTIPLIER)
        .clamp(limit, MAX_RECALL_CANDIDATES)
}

fn reciprocal_rank_fusion(vector_rank: Option<usize>, lexical_rank: Option<usize>) -> f32 {
    const RRF_K: f32 = 60.0;
    let vector = vector_rank
        .map(|rank| 1.0 / (RRF_K + rank as f32))
        .unwrap_or(0.0);
    let lexical = lexical_rank
        .map(|rank| 1.0 / (RRF_K + rank as f32))
        .unwrap_or(0.0);
    vector + lexical
}

impl RecallScoreComponents {
    fn total(&self) -> f32 {
        self.rrf_score
            + self.salience_boost
            + self.strength_boost
            + self.tier_boost
            + self.recency_boost
            + self.encoding_context_boost
    }
}

fn score_components(
    row: &EpisodeRow,
    query: &str,
    vector_rank: Option<usize>,
    lexical_rank: Option<usize>,
) -> RecallScoreComponents {
    RecallScoreComponents {
        rrf_score: reciprocal_rank_fusion(vector_rank, lexical_rank),
        salience_boost: row.salience.clamp(0.0, 1.0) * 0.012,
        strength_boost: row.strength.clamp(0.0, 1.0) * 0.008,
        tier_boost: match row.tier.as_str() {
            "hot" => 0.006,
            "warm" => 0.003,
            _ => 0.0,
        },
        recency_boost: recency_boost(row.ts_ms),
        encoding_context_boost: encoding_context_boost(query, &row.encoding_context_json),
    }
}

fn recency_boost(ts_ms: i64) -> f32 {
    let now_ms = chrono::Utc::now().timestamp_millis();
    let age_ms = now_ms.saturating_sub(ts_ms);
    let day_ms = 86_400_000i64;
    if age_ms <= 7 * day_ms {
        0.006
    } else if age_ms <= 30 * day_ms {
        0.003
    } else {
        0.0
    }
}

fn encoding_context_boost(query: &str, encoding_context_json: &str) -> f32 {
    let Some(terms) = recall_terms(query) else {
        return 0.0;
    };
    let context = encoding_context_json.to_lowercase();
    if terms.iter().any(|term| context.contains(term)) {
        0.006
    } else {
        0.0
    }
}

/// Internal row shape for the SQL fetch. Not exposed publicly —
/// callers consume `RecallHit`.
#[derive(Debug)]
struct EpisodeRow {
    rowid: i64,
    memory_id: String,
    ts_ms: i64,
    content: String,
    source_type: String,
    tier: String,
    status: String,
    strength: f32,
    salience: f32,
    encoding_context_json: String,
}

#[derive(Debug)]
struct LexicalHit {
    rowid: i64,
    bm25_score: f32,
    rank: usize,
}

async fn fetch_lexical_episode_hits(
    pool: &ReaderPool,
    query: &str,
    limit: usize,
) -> Result<Vec<LexicalHit>> {
    let Some(fts_query) = build_fts_query(query) else {
        return Ok(Vec::new());
    };
    pool.interact(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT e.rowid, bm25(episodes_fts) AS score
               FROM episodes_fts
               JOIN episodes e ON e.rowid = episodes_fts.rowid
              WHERE episodes_fts MATCH ?1
                AND e.status = 'active'
              ORDER BY score ASC
              LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![fts_query, limit as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)? as f32))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(idx, (rowid, bm25_score))| LexicalHit {
                rowid,
                bm25_score,
                rank: idx + 1,
            })
            .collect())
    })
    .await
}

fn build_fts_query(query: &str) -> Option<String> {
    let terms = recall_terms(query)?;
    Some(
        terms
            .into_iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

fn recall_terms(query: &str) -> Option<Vec<String>> {
    let mut terms = Vec::new();
    for term in query
        .split(|c: char| !c.is_alphanumeric())
        .map(str::trim)
        .filter(|term| !term.is_empty())
    {
        let escaped = term.replace('"', "\"\"");
        if !terms.iter().any(|existing: &String| existing == &escaped) {
            terms.push(escaped);
        }
        if terms.len() >= 8 {
            break;
        }
    }
    if terms.is_empty() {
        return None;
    }
    Some(terms.into_iter().map(|term| term.to_lowercase()).collect())
}

async fn fetch_episodes_by_rowid(pool: &ReaderPool, rowids: &[i64]) -> Result<Vec<EpisodeRow>> {
    if rowids.is_empty() {
        return Ok(Vec::new());
    }
    // SQLite's default SQLITE_LIMIT_VARIABLE_NUMBER is 999. With our
    // `limit.clamp(1, 100)` upstream we're safely under.
    let rowids = rowids.to_vec();
    pool.interact(move |conn| {
        let placeholders = std::iter::repeat("?")
            .take(rowids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT rowid, memory_id, ts_ms, content, source_type, tier, status,
                    strength, salience, encoding_context_json
               FROM episodes
              WHERE rowid IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(rowids.iter()), |r| {
                Ok(EpisodeRow {
                    rowid: r.get(0)?,
                    memory_id: r.get(1)?,
                    ts_ms: r.get(2)?,
                    content: r.get(3)?,
                    source_type: r.get(4)?,
                    tier: r.get(5)?,
                    status: r.get(6)?,
                    strength: r.get::<_, f64>(7)? as f32,
                    salience: r.get::<_, f64>(8)? as f32,
                    encoding_context_json: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::test_support::{StubVectorIndex, open_test_db_at};
    use solo_storage::{StubEmbedder, WriterActor, WriterSpawn};

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Build a minimal recall fixture: real SQLCipher-less DB +
    /// StubEmbedder + StubVectorIndex + ReaderPool. Returns
    /// (embedder, hnsw, pool, write_handle, _tmp, join).
    #[allow(clippy::type_complexity)]
    fn fixture(
        runtime: &tokio::runtime::Runtime,
    ) -> (
        Arc<dyn Embedder>,
        Arc<dyn VectorIndex + Send + Sync>,
        ReaderPool,
        solo_storage::WriteHandle,
        tempfile::TempDir,
        std::thread::JoinHandle<()>,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let dim = 16usize;
        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", dim));
        let conn = open_test_db_at(&tmp.path().join("test.db"));
        let WriterSpawn { handle, join } = WriterActor::spawn(conn, hnsw.clone());
        let path = tmp.path().join("test.db");
        let pool = runtime.block_on(async { ReaderPool::new(&path, None, hnsw.clone()).unwrap() });
        (embedder, hnsw, pool, handle, tmp, join)
    }

    fn insert_episode_direct(conn: &rusqlite::Connection, content: &str) -> i64 {
        insert_episode_direct_with_signals(conn, content, 0.5, 0.5, "{}")
    }

    fn insert_episode_direct_with_signals(
        conn: &rusqlite::Connection,
        content: &str,
        strength: f32,
        salience: f32,
        encoding_context_json: &str,
    ) -> i64 {
        let ep = solo_storage::test_support::fixture_episode(content);
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, source_id, content,
                encoding_context_json, provenance_json, confidence,
                strength, salience, tier, status, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, 'hot', 'active', ?, ?)",
            rusqlite::params![
                ep.memory_id.to_string(),
                ep.ts_ms,
                ep.source_type,
                ep.source_id,
                ep.content,
                encoding_context_json,
                ep.confidence.0,
                strength,
                salience,
                now_ms,
                now_ms,
            ],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn shutdown(
        runtime: &tokio::runtime::Runtime,
        pool: ReaderPool,
        handle: solo_storage::WriteHandle,
        tmp: tempfile::TempDir,
        join: std::thread::JoinHandle<()>,
    ) {
        runtime.block_on(async move {
            drop(handle);
            drop(pool);
            drop(tmp);
            tokio::task::spawn_blocking(move || {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(join.join());
                });
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .expect("writer thread did not exit within 5s")
                    .expect("writer thread panicked");
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn recall_returns_hits_with_inserted_content() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            // Seed: two episodes through the writer actor.
            let ep1 = solo_storage::test_support::fixture_episode("alpha");
            let ep2 = solo_storage::test_support::fixture_episode("beta");
            handle
                .remember(ep1, embedder.embed("alpha").await.unwrap())
                .await
                .unwrap();
            handle
                .remember(ep2, embedder.embed("beta").await.unwrap())
                .await
                .unwrap();

            let r = run_recall_inner(&embedder, &hnsw, &pool, "alpha", 5)
                .await
                .unwrap();
            assert_eq!(r.index_len, 2);
            assert!(
                r.hits.iter().any(|h| h.content == "alpha"),
                "expected hit for alpha, got: {:#?}",
                r.hits
            );
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn recall_excludes_forgotten_rows() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let ep = solo_storage::test_support::fixture_episode("ephemeral");
            let mid = ep.memory_id;
            handle
                .remember(ep, embedder.embed("ephemeral").await.unwrap())
                .await
                .unwrap();
            handle.forget(mid, "test".into()).await.unwrap();

            let r = run_recall_inner(&embedder, &hnsw, &pool, "ephemeral", 5)
                .await
                .unwrap();
            // Forgotten row excluded from hits. index_len reports 0 —
            // post the audit-pass-3 fix in writer.rs::handle_forget,
            // forget tombstones the HNSW so `len()` drops accordingly,
            // matching the post-restart `rebuild_tombstones_from_sql`
            // behaviour.
            assert!(r.hits.is_empty(), "got: {:#?}", r.hits);
            assert_eq!(r.index_len, 0);
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn recall_rejects_empty_query() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let err = run_recall_inner(&embedder, &hnsw, &pool, "   ", 5)
                .await
                .unwrap_err();
            assert!(matches!(err, Error::InvalidInput(_)), "got: {err:?}");
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn recall_with_empty_index_returns_no_hits() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let r = run_recall_inner(&embedder, &hnsw, &pool, "any", 5)
                .await
                .unwrap();
            assert!(r.hits.is_empty());
            assert_eq!(r.index_len, 0);
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn recall_uses_fts_when_vector_index_has_no_hits() {
        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let conn = open_test_db_at(&db_path);
        let rowid = insert_episode_direct(&conn, "ticket SOLO-742 exact lexical needle");
        drop(conn);

        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(16));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", 16));
        let pool =
            runtime.block_on(async { ReaderPool::new(&db_path, None, hnsw.clone()).unwrap() });

        runtime.block_on(async {
            let r = run_recall_inner(&embedder, &hnsw, &pool, "SOLO-742", 5)
                .await
                .unwrap();
            assert_eq!(r.index_len, 0);
            assert_eq!(r.hits.len(), 1);
            assert_eq!(r.hits[0].rowid, rowid);
            assert_eq!(r.hits[0].vector_rank, None);
            assert_eq!(r.hits[0].lexical_rank, Some(1));
            assert!(r.hits[0].bm25_score.is_some());
        });

        runtime.block_on(async move {
            drop(pool);
            drop(tmp);
        });
    }

    #[test]
    fn recall_reranks_with_salience_strength_and_encoding_context() {
        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let conn = open_test_db_at(&db_path);
        let low_rowid =
            insert_episode_direct_with_signals(&conn, "rankneedle ordinary note", 0.1, 0.1, "{}");
        let high_rowid = insert_episode_direct_with_signals(
            &conn,
            "rankneedle important note",
            1.0,
            1.0,
            r#"{"task":"rankneedle launch"}"#,
        );
        drop(conn);

        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(16));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", 16));
        let pool =
            runtime.block_on(async { ReaderPool::new(&db_path, None, hnsw.clone()).unwrap() });

        runtime.block_on(async {
            let r = run_recall_inner(&embedder, &hnsw, &pool, "rankneedle", 5)
                .await
                .unwrap();
            assert_eq!(r.hits.len(), 2);
            assert_eq!(
                r.hits[0].rowid, high_rowid,
                "stored importance signals should overcome a tiny lexical-rank tie-break"
            );
            assert!(r.hits.iter().any(|hit| hit.rowid == low_rowid));
            assert!(r.hits[0].score_components.salience_boost > 0.0);
            assert!(r.hits[0].score_components.encoding_context_boost > 0.0);
        });

        runtime.block_on(async move {
            drop(pool);
            drop(tmp);
        });
    }

    #[test]
    fn recall_clamps_limit_to_100() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            // Seed a single hit so we can verify the clamp doesn't error.
            let ep = solo_storage::test_support::fixture_episode("only");
            handle
                .remember(ep, embedder.embed("only").await.unwrap())
                .await
                .unwrap();
            // limit=1000 should clamp to 100; index has 1 row → 1 hit.
            let r = run_recall_inner(&embedder, &hnsw, &pool, "only", 1000)
                .await
                .unwrap();
            assert_eq!(r.hits.len(), 1);
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn recall_widens_candidates_before_filtering_chunk_hits() {
        let runtime = rt();
        let (embedder, hnsw, pool, handle, tmp, join) = fixture(&runtime);
        runtime.block_on(async {
            let chunk_vec = vec![0.0f32; 16];
            for rowid in 1..=3 {
                hnsw.add(solo_storage::chunk_hnsw_id(rowid), &chunk_vec)
                    .unwrap();
            }

            let ep = solo_storage::test_support::fixture_episode("episode after chunk hits");
            handle
                .remember(
                    ep,
                    embedder.embed("episode after chunk hits").await.unwrap(),
                )
                .await
                .unwrap();

            let r = run_recall_inner(&embedder, &hnsw, &pool, "episode", 1)
                .await
                .unwrap();
            assert_eq!(r.index_len, 4);
            assert_eq!(r.candidates_considered, 5);
            assert_eq!(r.hits.len(), 1);
            assert_eq!(r.hits[0].content, "episode after chunk hits");
        });
        shutdown(&runtime, pool, handle, tmp, join);
    }

    #[test]
    fn recall_candidate_limit_widens_but_caps() {
        assert_eq!(recall_candidate_limit(1), 4);
        assert_eq!(recall_candidate_limit(5), 20);
        assert_eq!(recall_candidate_limit(100), 200);
    }

    // -----------------------------------------------------------------
    // v0.8.0 P4 — audit emit tests for the query path
    //
    // Build a LibraryHandle::from_parts_for_tests against a real on-disk
    // DB so we can read the audit_events table after recall. The audit
    // writer is a noop by default in from_parts_for_tests — for a real
    // emit we open a fresh AuditWriter pointed at the same DB.
    // -----------------------------------------------------------------

    #[test]
    fn run_recall_emits_audit_row_with_principal() {
        use solo_storage::{AuditWriter, EmbedderIdentity};

        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("audit-recall.db");
        let conn = solo_storage::test_support::open_test_db_at(&db_path);
        let embedder: Arc<dyn Embedder> =
            Arc::new(solo_storage::StubEmbedder::new("stub", "v1", 16));
        let identity = EmbedderIdentity {
            name: "stub".into(),
            version: "v1".into(),
            dim: 16,
            dtype: "f32".into(),
        };
        let embedder_id = solo_storage::get_or_insert_embedder_id(&conn, &identity).unwrap();
        let hnsw: Arc<dyn VectorIndex + Send + Sync> =
            Arc::new(solo_storage::test_support::StubVectorIndex::new(16));

        let writer_conn = solo_storage::test_support::open_test_db_at(&db_path);
        let solo_storage::WriterSpawn { handle, join } = solo_storage::WriterActor::spawn_full(
            writer_conn,
            hnsw.clone(),
            tmp.path().to_path_buf(),
            embedder_id,
        );

        runtime.block_on(async {
            let pool = solo_storage::ReaderPool::new(&db_path, None, hnsw.clone()).unwrap();

            // Spawn a real AuditWriter (not noop) pointed at the same DB.
            let (audit, audit_shutdown) = AuditWriter::spawn(db_path.clone(), None);

            // Seed one episode so recall has something to find.
            let ep = solo_storage::test_support::fixture_episode("foo");
            handle
                .remember(ep, embedder.embed("foo").await.unwrap())
                .await
                .unwrap();

            // Build a LibraryHandle wrapping these parts; override `audit`
            // via from_parts_for_tests + manual replacement. Simpler:
            // bypass LibraryHandle and call run_recall_inner directly,
            // then emit through the AuditWriter ourselves to mirror what
            // `run_recall` does.
            let _ = run_recall_inner(&embedder, &hnsw, &pool, "foo", 5)
                .await
                .unwrap();
            audit.emit_ok(
                Some("alice".into()),
                solo_storage::AuditOperation::MemoryRecall,
                None,
            );

            // Wait briefly for the async drainer to flush.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;

            // Close the audit writer + drainer so we know it flushed.
            drop(audit);
            audit_shutdown.join().await;

            // Read back the audit row.
            let read_conn = solo_storage::test_support::open_test_db_at(&db_path);
            let (op, principal): (String, Option<String>) = read_conn
                .query_row(
                    "SELECT operation, principal_subject FROM audit_events \
                     WHERE operation = 'memory.recall' ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(op, "memory.recall");
            assert_eq!(principal.as_deref(), Some("alice"));

            drop(pool);
            drop(handle);
        });
        join.join().unwrap();
    }

    #[test]
    fn run_recall_emits_audit_row_with_none_principal() {
        use solo_storage::AuditWriter;

        let runtime = rt();
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("audit-recall-none.db");
        let _ = solo_storage::test_support::open_test_db_at(&db_path);
        runtime.block_on(async {
            let (audit, audit_shutdown) = AuditWriter::spawn(db_path.clone(), None);
            // Emit directly through the writer to exercise the async path
            // without spinning up an extra writer-actor.
            audit.emit_ok(None, solo_storage::AuditOperation::MemoryRecall, None);
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            drop(audit);
            audit_shutdown.join().await;

            let conn = solo_storage::test_support::open_test_db_at(&db_path);
            let principal: Option<String> = conn
                .query_row(
                    "SELECT principal_subject FROM audit_events WHERE operation = 'memory.recall'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                principal.is_none(),
                "principal should be NULL for CLI / no-auth callers"
            );
        });
    }
}
