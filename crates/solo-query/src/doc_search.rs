// SPDX-License-Identifier: Apache-2.0

//! `doc_search` — hybrid search restricted to document chunks.
//!
//! v0.7.0 RAG retrieval (Priority 4 of `docs/dev-log/0083-v0.7.0-
//! implementation-plan.md`). Mirrors [`crate::recall::run_recall`] but:
//!
//!   * Resolves vector rowids against `document_chunks` (not `episodes`).
//!   * Adds FTS5/BM25 lexical candidates from `document_chunks_fts`.
//!   * JOINs to `documents` for parent-doc context (title / source /
//!     mime_type) on every hit.
//!   * Filters forgotten documents at the SQL level — chunks of a
//!     `status='forgotten'` document never appear in the result, even
//!     if their HNSW vectors are still in the shared index.
//!
//! ## Shared HNSW namespace
//!
//! Per ADR-0003 (extended in v0.7.0 P1, encoding added in P7b) the HNSW
//! index is keyed by an encoded i64 that packs the SQLite rowid with a
//! kind discriminator in the high bit (see `solo_storage::hnsw_id`).
//! `hnsw.search` returns both episode-encoded and chunk-encoded ids; this
//! module decodes each result and keeps only the chunk-kind hits, then
//! uses the **decoded** rowid (high bit stripped) for the SQL JOIN
//! against `document_chunks.rowid`. We widen the HNSW query past `limit`
//! so the kind filter + forgotten-doc filter don't starve us.
//!
//! ## Pipeline
//!
//!   1. Validate `query` (non-empty after trim).
//!   2. Embed via the supplied [`Embedder`].
//!   3. `hnsw.search(query_vec, candidate_limit)` — vector candidates.
//!   4. `document_chunks_fts MATCH` — lexical candidates.
//!   5. SQL fetch chunks-by-rowid JOIN documents, filter
//!      `documents.status='active'`.
//!   6. Fuse vector + lexical ranks with reciprocal-rank fusion, then
//!      truncate to `limit`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use solo_core::{Embedder, Error, Result, VectorIndex};
use solo_storage::{AuditOperation, HnswIdKind, LibraryHandle, ReaderPool, decode_hnsw_id};

/// One hit from [`run_doc_search`] — a single chunk plus the parent
/// document context an agent needs to ground the answer.
///
/// `chunk_id` / `doc_id` are strings (UUID display form) rather than
/// the typed `ChunkId` / `DocumentId` so the wire shape stays
/// straightforward for the MCP + HTTP transports (matches the
/// `RecallHit` / `FactHit` convention in the rest of `solo-query`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocSearchHit {
    pub chunk_id: String,
    pub doc_id: String,
    pub doc_title: Option<String>,
    pub doc_source: Option<String>,
    pub doc_mime_type: Option<String>,
    pub chunk_index: u32,
    pub content: String,
    /// Cosine distance from the HNSW search step. Smaller = closer.
    /// Carries the value the HNSW returned verbatim. Lexical-only hits use
    /// `1.0` so older clients still receive a numeric distance.
    pub cos_distance: f32,
    /// FTS5 BM25 score when the chunk also matched lexically. Smaller is
    /// better in SQLite's BM25 convention.
    pub bm25_score: Option<f32>,
    /// Final rank score after reciprocal-rank fusion. Larger is better.
    pub fused_score: f32,
    /// 1-based vector rank among raw HNSW chunk candidates, if present.
    pub vector_rank: Option<usize>,
    /// 1-based lexical rank among FTS candidates, if present.
    pub lexical_rank: Option<usize>,
    pub start_offset: u32,
    pub end_offset: u32,
}

const MAX_DOC_SEARCH_CANDIDATES: usize = 200;
const DOC_SEARCH_CANDIDATE_MULTIPLIER: usize = 4;

/// Run a hybrid vector + lexical search restricted to document chunks.
///
/// `limit` is clamped to `[1, 100]` (same convention as recall + the
/// derived pipelines). The returned `Vec` carries up to `limit` hits
/// ordered by fused score. Forgotten documents are filtered at the SQL
/// level — their chunks are never surfaced.
///
/// Empty `query` returns `Err(InvalidInput)`. This is the canonical
/// refusal site; transports may add their own validation upstream for
/// nicer error codes.
///
/// v0.8.0 P2: takes a `&LibraryHandle` for tenant routing.
/// v0.8.0 P4: emits an audit row after returning. See [`run_recall`] for
/// the audit-emit conventions.
///
/// [`run_recall`]: crate::recall::run_recall
pub async fn run_doc_search(
    tenant: &LibraryHandle,
    audit_principal: Option<String>,
    query: &str,
    limit: usize,
) -> Result<Vec<DocSearchHit>> {
    let result = run_doc_search_inner(
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
            .emit_ok(audit_principal, AuditOperation::MemorySearchDocs, None),
        Err(e) => {
            tenant
                .audit()
                .emit_error(audit_principal, AuditOperation::MemorySearchDocs, None, e)
        }
    }
    result
}

/// Lower-level entry — see `run_doc_search` for the canonical
/// tenant-routed surface. v0.8.0 P2.
#[doc(hidden)]
pub async fn run_doc_search_inner(
    embedder: &Arc<dyn Embedder>,
    hnsw: &Arc<dyn VectorIndex + Send + Sync>,
    pool: &ReaderPool,
    query: &str,
    limit: usize,
) -> Result<Vec<DocSearchHit>> {
    if query.trim().is_empty() {
        return Err(Error::invalid_input("doc_search query must not be empty"));
    }
    let limit = limit.clamp(1, 100);

    // Embed the query.
    let q_emb = embedder.embed(query).await?;
    let q_slice = q_emb
        .as_f32_slice()
        .ok_or_else(|| Error::embedder("embedder returned non-F32 vector; HNSW requires F32"))?;

    let candidate_limit = doc_search_candidate_limit(limit);
    let hnsw_hits = hnsw.search(q_slice, candidate_limit)?;
    let lexical_hits = fetch_lexical_chunk_hits(pool, query, candidate_limit).await?;

    // Decode HNSW ids: the index is shared between episodes and document
    // chunks (see `solo_storage::hnsw_id`). Keep only the chunk-kind hits,
    // decoding each to the underlying `document_chunks.rowid` for the SQL
    // JOIN. Episode-kind hits are dropped silently.
    let decoded_chunk_hits: Vec<(i64, f32, usize)> = hnsw_hits
        .iter()
        .enumerate()
        .filter_map(|(idx, (hnsw_id, score))| {
            let (kind, rowid) = decode_hnsw_id(*hnsw_id);
            match kind {
                HnswIdKind::Chunk => Some((rowid, *score, idx + 1)),
                HnswIdKind::Episode => None,
            }
        })
        .collect();
    if decoded_chunk_hits.is_empty() && lexical_hits.is_empty() {
        return Ok(Vec::new());
    }

    // SQL fetch — JOINs to `document_chunks` + `documents`, filters
    // forgotten docs. The rowid here is the decoded value (high bit
    // stripped) so it matches `document_chunks.rowid` directly.
    let mut rowids: Vec<i64> = Vec::with_capacity(decoded_chunk_hits.len() + lexical_hits.len());
    for (rowid, _, _) in &decoded_chunk_hits {
        if !rowids.contains(rowid) {
            rowids.push(*rowid);
        }
    }
    for hit in &lexical_hits {
        if !rowids.contains(&hit.rowid) {
            rowids.push(hit.rowid);
        }
    }
    let rows = fetch_chunks_by_rowid(pool, &rowids).await?;

    let by_rowid: std::collections::HashMap<i64, ChunkRow> =
        rows.into_iter().map(|r| (r.rowid, r)).collect();
    let vector_by_rowid: std::collections::HashMap<i64, (f32, usize)> = decoded_chunk_hits
        .into_iter()
        .map(|(rowid, score, rank)| (rowid, (score, rank)))
        .collect();
    let lexical_by_rowid: std::collections::HashMap<i64, (f32, usize)> = lexical_hits
        .into_iter()
        .map(|hit| (hit.rowid, (hit.bm25_score, hit.rank)))
        .collect();

    let mut hits: Vec<DocSearchHit> = Vec::with_capacity(rowids.len());
    for rowid in &rowids {
        if let Some(row) = by_rowid.get(rowid) {
            let vector = vector_by_rowid.get(rowid).copied();
            let lexical = lexical_by_rowid.get(rowid).copied();
            let vector_rank = vector.map(|(_, rank)| rank);
            let lexical_rank = lexical.map(|(_, rank)| rank);
            hits.push(DocSearchHit {
                chunk_id: row.chunk_id.clone(),
                doc_id: row.doc_id.clone(),
                doc_title: row.doc_title.clone(),
                doc_source: row.doc_source.clone(),
                doc_mime_type: row.doc_mime_type.clone(),
                chunk_index: row.chunk_index as u32,
                content: row.content.clone(),
                cos_distance: vector.map(|(score, _)| score).unwrap_or(1.0),
                bm25_score: lexical.map(|(score, _)| score),
                fused_score: reciprocal_rank_fusion(vector_rank, lexical_rank),
                vector_rank,
                lexical_rank,
                start_offset: row.start_offset as u32,
                end_offset: row.end_offset as u32,
            });
        }
    }
    hits.sort_by(|a, b| {
        b.fused_score
            .total_cmp(&a.fused_score)
            .then_with(|| a.cos_distance.total_cmp(&b.cos_distance))
            .then_with(|| a.chunk_index.cmp(&b.chunk_index))
            .then_with(|| a.chunk_id.cmp(&b.chunk_id))
    });
    hits.truncate(limit);

    Ok(hits)
}

fn doc_search_candidate_limit(limit: usize) -> usize {
    limit
        .saturating_mul(DOC_SEARCH_CANDIDATE_MULTIPLIER)
        .clamp(limit, MAX_DOC_SEARCH_CANDIDATES)
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

/// Internal row shape for the SQL JOIN. Not exposed publicly — callers
/// receive `DocSearchHit`.
#[derive(Debug)]
struct ChunkRow {
    rowid: i64,
    chunk_id: String,
    doc_id: String,
    doc_title: Option<String>,
    doc_source: Option<String>,
    doc_mime_type: Option<String>,
    chunk_index: i64,
    content: String,
    start_offset: i64,
    end_offset: i64,
}

/// Look up chunk rows by their HNSW rowids, JOINed against `documents`
/// for parent-doc context, with `documents.status='active'` filter.
///
/// Returns rows in whatever order SQLite picked — the caller re-orders
/// to HNSW score outside this function.
async fn fetch_chunks_by_rowid(pool: &ReaderPool, rowids: &[i64]) -> Result<Vec<ChunkRow>> {
    if rowids.is_empty() {
        return Ok(Vec::new());
    }
    // SQLITE_LIMIT_VARIABLE_NUMBER is 999 by default. With `limit.clamp(1, 100)`
    // and `HNSW_WIDEN_FACTOR = 4` (capped at HNSW_WIDEN_MAX = 400),
    // we're safely under.
    let rowids = rowids.to_vec();
    pool.interact(move |conn| {
        let placeholders = std::iter::repeat("?")
            .take(rowids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT dc.rowid, dc.chunk_id, dc.doc_id,
                    d.title, d.source, d.mime_type,
                    dc.chunk_index, dc.content,
                    dc.start_offset, dc.end_offset
               FROM document_chunks dc
               JOIN documents d ON d.doc_id = dc.doc_id
              WHERE dc.rowid IN ({placeholders})
                AND d.status = 'active'"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(rowids.iter()), |r| {
                Ok(ChunkRow {
                    rowid: r.get(0)?,
                    chunk_id: r.get(1)?,
                    doc_id: r.get(2)?,
                    doc_title: r.get(3)?,
                    doc_source: r.get(4)?,
                    doc_mime_type: r.get(5)?,
                    chunk_index: r.get(6)?,
                    content: r.get(7)?,
                    start_offset: r.get(8)?,
                    end_offset: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await
}

#[derive(Debug)]
struct LexicalChunkHit {
    rowid: i64,
    bm25_score: f32,
    rank: usize,
}

async fn fetch_lexical_chunk_hits(
    pool: &ReaderPool,
    query: &str,
    limit: usize,
) -> Result<Vec<LexicalChunkHit>> {
    let Some(fts_query) = build_fts_query(query) else {
        return Ok(Vec::new());
    };
    pool.interact(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT dc.rowid, bm25(document_chunks_fts) AS score
               FROM document_chunks_fts
               JOIN document_chunks dc ON dc.rowid = document_chunks_fts.rowid
               JOIN documents d ON d.doc_id = dc.doc_id
              WHERE document_chunks_fts MATCH ?1
                AND d.status = 'active'
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
            .map(|(idx, (rowid, bm25_score))| LexicalChunkHit {
                rowid,
                bm25_score,
                rank: idx + 1,
            })
            .collect())
    })
    .await
}

fn build_fts_query(query: &str) -> Option<String> {
    let terms = doc_search_terms(query)?;
    Some(
        terms
            .into_iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

fn doc_search_terms(query: &str) -> Option<Vec<String>> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use solo_core::{ChunkId, DocumentId, Embedder, VectorIndex};
    use solo_storage::test_support::{StubVectorIndex, open_test_db_at};
    use solo_storage::{ReaderPool, StubEmbedder, chunk_hnsw_id, episode_hnsw_id};
    use std::sync::Arc;

    /// Build a fresh fixture: tempdir + on-disk SQLite with migrations
    /// applied + ReaderPool + StubVectorIndex (shared via Arc) + a
    /// StubEmbedder. Tests seed `documents` / `document_chunks` rows
    /// directly via raw SQL; for HNSW state they call `hnsw.add` on
    /// the returned Arc directly. This avoids spinning up a full
    /// WriterActor for what is a pure read-path module.
    #[allow(clippy::type_complexity)]
    fn fixture() -> (
        Arc<dyn Embedder>,
        Arc<dyn VectorIndex + Send + Sync>,
        ReaderPool,
        tempfile::TempDir,
        rusqlite::Connection,
    ) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let dim = 16usize;
        let hnsw: Arc<dyn VectorIndex + Send + Sync> = Arc::new(StubVectorIndex::new(dim));
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", dim));
        let db_path = tmp.path().join("test.db");
        let conn = open_test_db_at(&db_path);
        // ReaderPool::new is sync — no nested runtime needed. Each
        // #[tokio::test] supplies its own runtime; the pool's internal
        // connection-pool spins up its threads independently.
        let pool = ReaderPool::new(&db_path, None, hnsw.clone()).expect("pool");
        (embedder, hnsw, pool, tmp, conn)
    }

    /// Seed one `documents` row.
    fn seed_document(
        conn: &rusqlite::Connection,
        doc_id: &DocumentId,
        source: &str,
        title: &str,
        mime: &str,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO documents
                 (doc_id, source, title, mime_type, ingested_at_ms,
                  status, chunk_count, content_hash, byte_size)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'active', 0, ?6, 0)",
            rusqlite::params![
                doc_id.to_string(),
                source,
                title,
                mime,
                now,
                // unique content_hash per doc to avoid UNIQUE collisions
                doc_id.to_string(),
            ],
        )
        .expect("seed document");
    }

    /// Seed one `document_chunks` row. Returns the assigned rowid.
    fn seed_chunk(
        conn: &rusqlite::Connection,
        doc_id: &DocumentId,
        chunk_id: &ChunkId,
        chunk_index: u32,
        content: &str,
    ) -> i64 {
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO document_chunks
                 (chunk_id, doc_id, chunk_index, content,
                  token_count, start_offset, end_offset, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                chunk_id.to_string(),
                doc_id.to_string(),
                chunk_index as i64,
                content,
                (content.split_whitespace().count() as i64).max(1),
                0i64,
                content.len() as i64,
                now,
            ],
        )
        .expect("seed chunk");
        conn.last_insert_rowid()
    }

    /// Mark a document forgotten. Mirrors the writer's
    /// `handle_forget_document` SQL.
    fn mark_forgotten(conn: &rusqlite::Connection, doc_id: &DocumentId) {
        conn.execute(
            "UPDATE documents SET status = 'forgotten' WHERE doc_id = ?1",
            rusqlite::params![doc_id.to_string()],
        )
        .expect("mark forgotten");
    }

    // ---- run_doc_search ----

    #[tokio::test]
    async fn run_doc_search_returns_matching_chunks() {
        let (embedder, hnsw, pool, _tmp, conn) = fixture();
        let doc_id = DocumentId::new();
        seed_document(&conn, &doc_id, "/tmp/intro.md", "Intro", "text/markdown");
        let chunks = [
            (ChunkId::new(), 0, "alpha first paragraph"),
            (ChunkId::new(), 1, "alpha second paragraph"),
            (ChunkId::new(), 2, "alpha third paragraph"),
        ];
        let mut rowids = Vec::new();
        for (cid, idx, content) in &chunks {
            let rowid = seed_chunk(&conn, &doc_id, cid, *idx, content);
            rowids.push((cid.clone(), rowid, *content));
        }
        // Add each chunk's embedding to the HNSW. Chunks use the
        // chunk-kind encoded id (high bit set) to share namespace with
        // episodes safely — see `solo_storage::hnsw_id`.
        for (_, rowid, content) in &rowids {
            let emb = embedder.embed(content).await.unwrap();
            hnsw.add(chunk_hnsw_id(*rowid), emb.as_f32_slice().unwrap())
                .unwrap();
        }

        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "alpha first", 5)
            .await
            .expect("search ok");
        assert_eq!(hits.len(), 3, "all three chunks should match");
        assert!(
            hits.iter().all(|h| h.doc_id == doc_id.to_string()),
            "every hit must carry the parent doc_id"
        );
        // Parent-doc context is carried.
        assert!(
            hits.iter().all(|h| h.doc_title.as_deref() == Some("Intro")),
            "doc_title joined onto every hit"
        );
        assert!(
            hits.iter()
                .all(|h| h.doc_source.as_deref() == Some("/tmp/intro.md")),
            "doc_source joined onto every hit"
        );
    }

    #[tokio::test]
    async fn run_doc_search_filters_forgotten_docs() {
        let (embedder, hnsw, pool, _tmp, conn) = fixture();
        let doc_id = DocumentId::new();
        seed_document(&conn, &doc_id, "/tmp/ghost.md", "Ghost", "text/markdown");
        let cid = ChunkId::new();
        let rowid = seed_chunk(&conn, &doc_id, &cid, 0, "ephemeral content");
        let emb = embedder.embed("ephemeral content").await.unwrap();
        hnsw.add(chunk_hnsw_id(rowid), emb.as_f32_slice().unwrap())
            .unwrap();

        // First confirm it's visible.
        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "ephemeral content", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        // Forget the doc and re-query — the chunk is no longer surfaced.
        mark_forgotten(&conn, &doc_id);
        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "ephemeral content", 5)
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "forgotten-doc chunks must be filtered at the SQL level: {hits:?}"
        );
    }

    #[tokio::test]
    async fn run_doc_search_with_mixed_episode_and_chunk_index_returns_chunks_only() {
        // The shared HNSW namespace holds both episode-encoded ids
        // (high bit clear) and chunk-encoded ids (high bit set). This
        // test exercises the decoder: an episode-kind id in HNSW must
        // not surface in doc_search results, even when its decoded
        // rowid happens to coincide with a chunk rowid (which it can
        // since both tables AUTOINCREMENT independently per ADR-0003 §
        // shared-HNSW-namespace).
        let (embedder, hnsw, pool, _tmp, conn) = fixture();
        let doc_id = DocumentId::new();
        seed_document(&conn, &doc_id, "/tmp/mixed.md", "Mixed", "text/markdown");
        let cid = ChunkId::new();
        let chunk_rowid = seed_chunk(&conn, &doc_id, &cid, 0, "real chunk content");

        // Insert a real episode row so the side-table is realistic.
        let now = chrono::Utc::now().timestamp_millis();
        let episode_id = solo_core::MemoryId::new();
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, tier, status,
                confidence, strength, salience,
                created_at_ms, updated_at_ms
             ) VALUES (?, ?, 'user_message', ?, '{}', 'hot', 'active',
                       1.0, 1.0, 1.0, ?, ?)",
            rusqlite::params![
                episode_id.to_string(),
                now,
                "competing episode content",
                now,
                now,
            ],
        )
        .unwrap();

        // Add the real chunk's rowid, encoded as a chunk id.
        let chunk_emb = embedder.embed("real chunk content").await.unwrap();
        hnsw.add(
            chunk_hnsw_id(chunk_rowid),
            chunk_emb.as_f32_slice().unwrap(),
        )
        .unwrap();
        // Add an episode-encoded HNSW entry. Important: this episode's
        // rowid (`chunk_rowid`) is the SAME numeric value as the chunk
        // above — proving the encoding keeps them in distinct
        // namespaces. The decoder picks Episode kind for this id and
        // doc_search drops it.
        let ep_emb = embedder.embed("competing episode content").await.unwrap();
        hnsw.add(episode_hnsw_id(chunk_rowid), ep_emb.as_f32_slice().unwrap())
            .unwrap();

        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "anything", 10)
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            1,
            "only the chunk hit survives the kind filter; got: {hits:?}"
        );
        assert_eq!(hits[0].chunk_id, cid.to_string());
    }

    #[tokio::test]
    async fn run_doc_search_respects_limit() {
        let (embedder, hnsw, pool, _tmp, conn) = fixture();
        let doc_id = DocumentId::new();
        seed_document(&conn, &doc_id, "/tmp/big.md", "Big", "text/markdown");
        for i in 0..10 {
            let cid = ChunkId::new();
            let content = format!("chunk number {i}");
            let rowid = seed_chunk(&conn, &doc_id, &cid, i as u32, &content);
            let emb = embedder.embed(&content).await.unwrap();
            hnsw.add(chunk_hnsw_id(rowid), emb.as_f32_slice().unwrap())
                .unwrap();
        }

        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "chunk", 3)
            .await
            .unwrap();
        assert_eq!(hits.len(), 3, "limit must be honored");
    }

    #[tokio::test]
    async fn run_doc_search_empty_query_rejected() {
        let (embedder, hnsw, pool, _tmp, _conn) = fixture();
        let err = run_doc_search_inner(&embedder, &hnsw, &pool, "   ", 5)
            .await
            .expect_err("empty query must be rejected");
        assert!(matches!(err, Error::InvalidInput(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn run_doc_search_returns_lexical_only_hits() {
        let (embedder, hnsw, pool, _tmp, conn) = fixture();
        let doc_id = DocumentId::new();
        seed_document(
            &conn,
            &doc_id,
            "/tmp/incidents.md",
            "Incidents",
            "text/markdown",
        );
        let cid = ChunkId::new();
        seed_chunk(
            &conn,
            &doc_id,
            &cid,
            0,
            "Runbook entry for rarekeyword42 and support escalation",
        );

        // No vector entry is added. Exact/keyword-only matches still need
        // to work for local-file RAG, especially IDs, function names, and
        // sparse operational terms.
        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "rarekeyword42", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk_id, cid.to_string());
        assert_eq!(hits[0].vector_rank, None);
        assert_eq!(hits[0].lexical_rank, Some(1));
        assert!(
            hits[0].bm25_score.is_some(),
            "lexical-only hit must expose BM25 score"
        );
        assert_eq!(hits[0].cos_distance, 1.0);
    }

    #[tokio::test]
    async fn run_doc_search_fuses_vector_and_lexical_ranks() {
        let (embedder, hnsw, pool, _tmp, conn) = fixture();
        let doc_id = DocumentId::new();
        seed_document(&conn, &doc_id, "/tmp/fusion.md", "Fusion", "text/markdown");

        let vector_only = ChunkId::new();
        let exact = ChunkId::new();
        let vector_only_rowid = seed_chunk(
            &conn,
            &doc_id,
            &vector_only,
            0,
            "semantically plausible but not the exact term",
        );
        let exact_rowid = seed_chunk(
            &conn,
            &doc_id,
            &exact,
            1,
            "contains rarefusionterm and should win",
        );

        let vector_only_emb = embedder
            .embed("semantically plausible but not the exact term")
            .await
            .unwrap();
        let exact_emb = embedder
            .embed("contains rarefusionterm and should win")
            .await
            .unwrap();
        hnsw.add(
            chunk_hnsw_id(vector_only_rowid),
            vector_only_emb.as_f32_slice().unwrap(),
        )
        .unwrap();
        hnsw.add(
            chunk_hnsw_id(exact_rowid),
            exact_emb.as_f32_slice().unwrap(),
        )
        .unwrap();

        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "rarefusionterm", 5)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0].chunk_id,
            exact.to_string(),
            "lexical rank should let the exact chunk outrank a vector-only first candidate"
        );
        assert_eq!(hits[0].vector_rank, Some(2));
        assert_eq!(hits[0].lexical_rank, Some(1));
        assert_eq!(hits[1].chunk_id, vector_only.to_string());
        assert_eq!(hits[1].vector_rank, Some(1));
        assert_eq!(hits[1].lexical_rank, None);
        assert!(hits[0].fused_score > hits[1].fused_score);
    }

    #[tokio::test]
    async fn run_doc_search_preserves_hnsw_order() {
        // The StubVectorIndex.search returns entries in INSERTION order
        // (and reports cos_distance=0.0 for all). The default SQL `IN
        // (...)` result order, by contrast, is rowid-ascending. To prove
        // our code preserves HNSW order we seed chunks so SQL's natural
        // order ≠ HNSW insertion order, then assert the returned hit
        // sequence matches the HNSW order.
        let (embedder, hnsw, pool, _tmp, conn) = fixture();
        let doc_id = DocumentId::new();
        seed_document(&conn, &doc_id, "/tmp/order.md", "Order", "text/markdown");
        // Seed three chunks with increasing rowids (1, 2, 3 by insertion
        // order into document_chunks).
        let c1 = ChunkId::new();
        let c2 = ChunkId::new();
        let c3 = ChunkId::new();
        let r1 = seed_chunk(&conn, &doc_id, &c1, 0, "first chunk text");
        let r2 = seed_chunk(&conn, &doc_id, &c2, 1, "second chunk text");
        let r3 = seed_chunk(&conn, &doc_id, &c3, 2, "third chunk text");
        // Add to the HNSW in REVERSE order — so hnsw.search returns
        // r3, r2, r1 (StubVectorIndex preserves insertion order in `entries`).
        let e3 = embedder.embed("third chunk text").await.unwrap();
        let e2 = embedder.embed("second chunk text").await.unwrap();
        let e1 = embedder.embed("first chunk text").await.unwrap();
        hnsw.add(chunk_hnsw_id(r3), e3.as_f32_slice().unwrap())
            .unwrap();
        hnsw.add(chunk_hnsw_id(r2), e2.as_f32_slice().unwrap())
            .unwrap();
        hnsw.add(chunk_hnsw_id(r1), e1.as_f32_slice().unwrap())
            .unwrap();

        let hits = run_doc_search_inner(&embedder, &hnsw, &pool, "semantic query", 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 3);
        // HNSW order is r3, r2, r1 — preserved by the post-fetch
        // re-ordering loop. SQL would have returned them r1, r2, r3
        // (rowid ascending).
        assert_eq!(
            hits[0].chunk_id,
            c3.to_string(),
            "hnsw-first chunk must be hits[0]"
        );
        assert_eq!(hits[1].chunk_id, c2.to_string());
        assert_eq!(hits[2].chunk_id, c1.to_string());
    }
}
