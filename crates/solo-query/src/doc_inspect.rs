// SPDX-License-Identifier: Apache-2.0

//! `doc_inspect` — read-side queries for documents and their chunks.
//!
//! v0.7.0 RAG inspection (Priority 4 of `docs/dev-log/0083-v0.7.0-
//! implementation-plan.md`). Mirrors the spirit of
//! [`crate::derived::inspect_cluster`] from v0.5.0 P3 — drill into one
//! aggregate (a document, not a cluster), surface its metadata + a list
//! of summaries (chunks, not episodes), with content previewed at 200
//! Unicode scalars to keep payloads tight.
//!
//! Two public functions:
//!
//!   * [`inspect_document`] — single-doc detail; returns `None` for
//!     unknown ids (rather than `Err::NotFound`) to mirror
//!     [`crate::derived::inspect_cluster`]'s `Option` pattern at the
//!     boundary. (Note: `inspect_cluster` itself returns `Err::NotFound`
//!     externally; we keep `Option` here because the wrappers in
//!     P5 / P6 will translate to the right transport-level shape and
//!     the `Option` is cheaper to pass through.)
//!   * [`list_documents`] — paginated index. Defaults to active-only;
//!     `include_forgotten=true` widens to both statuses.
//!
//! Both are pure read paths via [`ReaderPool`].

use serde::{Deserialize, Serialize};
use solo_core::{DocumentStatus, Result};
use solo_storage::{AuditOperation, AuditWriter, ReaderPool};

use crate::assets::{DocumentAssetLinkSummary, document_asset_links_for_document};

/// Default per-chunk content cap in Unicode scalars when previewing
/// chunks in [`inspect_document`]. Total preview length (truncated
/// content + trailing ellipsis) is exactly [`CHUNK_PREVIEW_CHARS`]
/// chars. Mirrors the spirit of `derived::EPISODE_TRUNCATE_CHARS`
/// (200) — keeps the wire payload of a 50-chunk doc-inspect under
/// ~10KB even before metadata.
pub const CHUNK_PREVIEW_CHARS: usize = 200;

/// Full document metadata as returned by [`inspect_document`]. Flat
/// shape mirrors the `documents` columns so transports re-encode
/// directly without going through `solo_core::Document` (whose typed
/// `DocumentId` / `DocumentStatus` would round-trip through the wire
/// fine, but mixing typed + string-form ids in one payload is
/// inconsistent with the rest of `solo-query`'s read surfaces).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentRecord {
    pub doc_id: String,
    pub source: Option<String>,
    pub title: Option<String>,
    pub mime_type: Option<String>,
    pub ingested_at_ms: i64,
    pub modified_at_ms: Option<i64>,
    pub status: DocumentStatus,
    pub chunk_count: u32,
    pub content_hash: Option<String>,
    pub byte_size: Option<u64>,
    pub extraction_status: Option<String>,
    pub extraction_error: Option<String>,
}

/// One chunk row, with content previewed at [`CHUNK_PREVIEW_CHARS`].
/// Designed for the "drill into a doc" use case — gives an agent
/// enough to decide whether to fetch the full content of a chunk
/// without sending a multi-KB payload up front.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChunkSummary {
    pub chunk_id: String,
    pub chunk_index: u32,
    /// First [`CHUNK_PREVIEW_CHARS`] characters of the chunk's content,
    /// with a trailing `…` if truncated. UTF-8-safe (operates on
    /// Unicode scalars; never splits a codepoint).
    pub content_preview: String,
    pub token_count: u32,
}

/// Result of an [`inspect_document`] call: the doc's metadata + every
/// chunk's summary, ordered by `chunk_index` ascending.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentInspectResult {
    pub document: DocumentRecord,
    pub chunks: Vec<ChunkSummary>,
    pub linked_assets: Vec<DocumentAssetLinkSummary>,
}

/// One row from [`list_documents`]. Lighter than [`DocumentRecord`] —
/// just what an agent or UI needs to render a one-line entry per doc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentSummary {
    pub doc_id: String,
    pub title: Option<String>,
    pub source: Option<String>,
    pub mime_type: Option<String>,
    pub ingested_at_ms: i64,
    pub chunk_count: u32,
    pub status: DocumentStatus,
    pub extraction_status: Option<String>,
    pub extraction_error: Option<String>,
}

/// UTF-8-safe truncation. Mirrors `derived::truncate_chars`. Takes up
/// to `max - 1` Unicode scalars then pushes a single `'…'`. Returns
/// the input verbatim when it's already within budget.
fn truncate_chars(s: &str, max: usize) -> String {
    debug_assert!(max >= 1, "max must leave room for at least the ellipsis");
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Parse the `documents.status` column back into the typed enum. Anything
/// unexpected lands as `Active` — the schema's CHECK constraint already
/// guarantees only `'active'` or `'forgotten'` reach this code.
fn parse_status(s: &str) -> DocumentStatus {
    match s {
        "forgotten" => DocumentStatus::Forgotten,
        _ => DocumentStatus::Active,
    }
}

/// Fetch a document by id, returning its metadata + every chunk's
/// summary (200-char preview, `chunk_index` ascending).
///
/// Returns `Ok(None)` when no `documents.doc_id` row matches. Unlike
/// [`crate::derived::inspect_cluster`] (which returns `Err::NotFound`),
/// we leave the not-found-vs-error distinction to the transport — the
/// MCP / HTTP wrappers in P5 / P6 can map `None` to a 404 / "not
/// found" error envelope while internal Solo callers can branch
/// directly on the `Option`.
///
/// Forgotten documents are NOT filtered out here — the caller may
/// want forensic access to a document they've forgotten (the
/// transport surfaces will gate this if needed). The `chunks` list
/// still surfaces, since `document_chunks` survive the soft-delete
/// per ADR-0003.
/// v0.8.0 P4 audit-aware wrapper around [`inspect_document_inner`].
pub async fn inspect_document(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    doc_id: &solo_core::DocumentId,
) -> Result<Option<DocumentInspectResult>> {
    let target = Some(doc_id.to_string());
    let result = inspect_document_inner(pool, doc_id).await;
    match &result {
        Ok(_) => audit.emit_ok(
            audit_principal,
            AuditOperation::MemoryInspectDocument,
            target,
        ),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryInspectDocument,
            target,
            e,
        ),
    }
    result
}

#[doc(hidden)]
pub async fn inspect_document_inner(
    pool: &ReaderPool,
    doc_id: &solo_core::DocumentId,
) -> Result<Option<DocumentInspectResult>> {
    let id_str = doc_id.to_string();
    pool.interact(move |conn| {
        // Step 1: fetch the document row. None = doc_id not in table.
        let doc_opt: Option<DocumentRecord> = conn
            .query_row(
                "SELECT d.doc_id, d.source, d.title, d.mime_type, d.ingested_at_ms,
                        d.modified_at_ms, d.status, d.chunk_count, d.content_hash,
                        d.byte_size,
                        (
                            SELECT ae.status
                              FROM document_assets da
                              JOIN asset_extractions ae ON ae.asset_id = da.asset_id
                             WHERE da.doc_id = d.doc_id
                               AND da.relation_type IN ('source_upload', 'source_import')
                             ORDER BY ae.created_at_ms DESC
                             LIMIT 1
                        ) AS extraction_status,
                        (
                            SELECT ae.error
                              FROM document_assets da
                              JOIN asset_extractions ae ON ae.asset_id = da.asset_id
                             WHERE da.doc_id = d.doc_id
                               AND da.relation_type IN ('source_upload', 'source_import')
                             ORDER BY ae.created_at_ms DESC
                             LIMIT 1
                        ) AS extraction_error
                   FROM documents d
                  WHERE d.doc_id = ?1",
                rusqlite::params![&id_str],
                |r| {
                    let status_str: String = r.get(6)?;
                    let byte_size: Option<i64> = r.get(9)?;
                    Ok(DocumentRecord {
                        doc_id: r.get(0)?,
                        source: r.get(1)?,
                        title: r.get(2)?,
                        mime_type: r.get(3)?,
                        ingested_at_ms: r.get(4)?,
                        modified_at_ms: r.get(5)?,
                        status: parse_status(&status_str),
                        chunk_count: {
                            let c: i64 = r.get(7)?;
                            c as u32
                        },
                        content_hash: r.get(8)?,
                        byte_size: byte_size.map(|v| v as u64),
                        extraction_status: r.get(10)?,
                        extraction_error: r.get(11)?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let document = match doc_opt {
            Some(d) => d,
            None => return Ok(None),
        };

        // Step 2: fetch every chunk for this doc, ascending by index.
        let mut stmt = conn.prepare(
            "SELECT chunk_id, chunk_index, content, token_count
               FROM document_chunks
              WHERE doc_id = ?1
              ORDER BY chunk_index ASC",
        )?;
        let chunks: Vec<ChunkSummary> = stmt
            .query_map(rusqlite::params![&document.doc_id], |r| {
                let content: String = r.get(2)?;
                let token_count: i64 = r.get(3)?;
                let chunk_index: i64 = r.get(1)?;
                Ok(ChunkSummary {
                    chunk_id: r.get(0)?,
                    chunk_index: chunk_index as u32,
                    content_preview: truncate_chars(&content, CHUNK_PREVIEW_CHARS),
                    token_count: token_count as u32,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let linked_assets = document_asset_links_for_document(conn, &document.doc_id)?;

        Ok(Some(DocumentInspectResult {
            document,
            chunks,
            linked_assets,
        }))
    })
    .await
}

/// Paginated list of documents, newest first.
///
/// `include_forgotten = false` (default for agent UX): only
/// `status='active'` rows. `include_forgotten = true`: both statuses.
///
/// `limit` is clamped to `[1, 100]` to match the rest of `solo-query`.
/// `offset` is taken verbatim (capped only by SQLite's i64 column).
///
/// Ordering: `ingested_at_ms` descending, with `doc_id` as a tiebreaker
/// (UUID v7 is time-ordered so this stays stable within a single-ms
/// burst of ingests).
/// v0.8.0 P4 audit-aware wrapper around [`list_documents_inner`].
pub async fn list_documents(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    limit: usize,
    offset: usize,
    include_forgotten: bool,
) -> Result<Vec<DocumentSummary>> {
    let result = list_documents_inner(pool, limit, offset, include_forgotten).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryListDocuments, None),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryListDocuments,
            None,
            e,
        ),
    }
    result
}

#[doc(hidden)]
pub async fn list_documents_inner(
    pool: &ReaderPool,
    limit: usize,
    offset: usize,
    include_forgotten: bool,
) -> Result<Vec<DocumentSummary>> {
    let limit = limit.clamp(1, 100) as i64;
    let offset = offset as i64;
    pool.interact(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT d.doc_id, d.title, d.source, d.mime_type, d.ingested_at_ms,
                    d.chunk_count, d.status,
                    (
                        SELECT ae.status
                          FROM document_assets da
                          JOIN asset_extractions ae ON ae.asset_id = da.asset_id
                         WHERE da.doc_id = d.doc_id
                           AND da.relation_type IN ('source_upload', 'source_import')
                         ORDER BY ae.created_at_ms DESC
                         LIMIT 1
                    ) AS extraction_status,
                    (
                        SELECT ae.error
                          FROM document_assets da
                          JOIN asset_extractions ae ON ae.asset_id = da.asset_id
                         WHERE da.doc_id = d.doc_id
                           AND da.relation_type IN ('source_upload', 'source_import')
                         ORDER BY ae.created_at_ms DESC
                         LIMIT 1
                    ) AS extraction_error
               FROM documents d
              WHERE (?1 OR d.status = 'active')
              ORDER BY d.ingested_at_ms DESC, d.doc_id ASC
              LIMIT ?2 OFFSET ?3",
        )?;
        let rows = stmt
            .query_map(rusqlite::params![include_forgotten, limit, offset], |r| {
                let status_str: String = r.get(6)?;
                let chunk_count: i64 = r.get(5)?;
                Ok(DocumentSummary {
                    doc_id: r.get(0)?,
                    title: r.get(1)?,
                    source: r.get(2)?,
                    mime_type: r.get(3)?,
                    ingested_at_ms: r.get(4)?,
                    chunk_count: chunk_count as u32,
                    status: parse_status(&status_str),
                    extraction_status: r.get(7)?,
                    extraction_error: r.get(8)?,
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
    use solo_core::{AssetId, ChunkId, DocumentId};
    use solo_storage::ReaderPool;
    use solo_storage::test_support::{StubVectorIndex, open_test_db_at};

    /// Open a fresh fixture: a ReaderPool against a tempdir DB seeded
    /// by `seed`, plus the kept-alive tempdir handle and a side
    /// connection callers can use to inspect or mutate the DB after
    /// setup.
    fn pool_with_seed(seed: impl FnOnce(&rusqlite::Connection)) -> (ReaderPool, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let conn = open_test_db_at(&db_path);
        seed(&conn);
        drop(conn);
        let hnsw: std::sync::Arc<dyn solo_core::VectorIndex + Send + Sync> =
            std::sync::Arc::new(StubVectorIndex::new(16));
        let pool = ReaderPool::new(&db_path, None, hnsw).expect("pool");
        (pool, tmp)
    }

    /// Seed one document row. `source` / `title` / `mime` are required
    /// so tests can assert their round-trip.
    fn seed_document(
        conn: &rusqlite::Connection,
        doc_id: &DocumentId,
        source: &str,
        title: &str,
        mime: &str,
        ingested_at_ms: i64,
        status: &str,
        chunk_count: u32,
    ) {
        conn.execute(
            "INSERT INTO documents
                 (doc_id, source, title, mime_type, ingested_at_ms,
                  modified_at_ms, status, chunk_count, content_hash,
                  byte_size)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                doc_id.to_string(),
                source,
                title,
                mime,
                ingested_at_ms,
                Option::<i64>::None,
                status,
                chunk_count as i64,
                doc_id.to_string(), // unique content_hash
                Option::<i64>::None,
            ],
        )
        .expect("seed document");
    }

    /// Seed one chunk. Tests that care about chunk content / order
    /// supply real values; tests that don't can pass any.
    fn seed_chunk(
        conn: &rusqlite::Connection,
        doc_id: &DocumentId,
        chunk_id: &ChunkId,
        chunk_index: u32,
        content: &str,
    ) {
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
                0i64,
            ],
        )
        .expect("seed chunk");
    }

    fn seed_asset(conn: &rusqlite::Connection, asset_id: &AssetId, filename: &str) {
        let hash = "a".repeat(64);
        conn.execute(
            "INSERT INTO assets (
                asset_id, sha256, mime_type, filename, size_bytes,
                storage_path, source, status, created_by_principal,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'text/markdown', ?3, 11, ?4,
                'solo-staged://upload/test', 'active', 'tester', 1_000, 1_000)",
            rusqlite::params![
                asset_id.to_string(),
                &hash,
                filename,
                format!("assets/blobs/aa/{hash}"),
            ],
        )
        .expect("seed asset");
    }

    fn seed_document_asset_link(
        conn: &rusqlite::Connection,
        doc_id: &DocumentId,
        asset_id: &AssetId,
        relation_type: &str,
    ) {
        conn.execute(
            "INSERT INTO document_assets (
                link_id, doc_id, asset_id, relation_type, note, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 'original', 1_001)",
            rusqlite::params![
                AssetId::new().to_string(),
                doc_id.to_string(),
                asset_id.to_string(),
                relation_type,
            ],
        )
        .expect("seed document asset link");
    }

    fn seed_asset_extraction(
        conn: &rusqlite::Connection,
        asset_id: &AssetId,
        status: &str,
        error: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO asset_extractions (
                extraction_id, asset_id, extractor_name, extractor_version,
                status, text_chars, error, created_at_ms
             ) VALUES (?1, ?2, 'markdown_text', 'v1', ?3, 42, ?4, 1_002)",
            rusqlite::params![
                AssetId::new().to_string(),
                asset_id.to_string(),
                status,
                error,
            ],
        )
        .expect("seed asset extraction");
    }

    // ---- inspect_document ----

    #[tokio::test]
    async fn inspect_document_returns_doc_and_chunks() {
        let doc_id = DocumentId::new();
        let c1 = ChunkId::new();
        let c2 = ChunkId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/a.md",
                "Doc A",
                "text/markdown",
                1_000,
                "active",
                2,
            );
            seed_chunk(conn, &doc_id, &c1, 0, "first paragraph body");
            seed_chunk(conn, &doc_id, &c2, 1, "second paragraph body");
        });

        let result = inspect_document_inner(&pool, &doc_id)
            .await
            .expect("ok")
            .expect("found");
        assert_eq!(result.document.doc_id, doc_id.to_string());
        assert_eq!(result.document.title.as_deref(), Some("Doc A"));
        assert_eq!(result.document.source.as_deref(), Some("/tmp/a.md"));
        assert_eq!(result.document.mime_type.as_deref(), Some("text/markdown"));
        assert_eq!(result.document.status, DocumentStatus::Active);
        assert_eq!(result.document.chunk_count, 2);
        assert_eq!(result.chunks.len(), 2);
        assert!(result.linked_assets.is_empty());
    }

    #[tokio::test]
    async fn inspect_document_surfaces_linked_assets() {
        let doc_id = DocumentId::new();
        let chunk_id = ChunkId::new();
        let asset_id = AssetId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/source.md",
                "Source",
                "text/markdown",
                1_000,
                "active",
                1,
            );
            seed_chunk(conn, &doc_id, &chunk_id, 0, "source body");
            seed_asset(conn, &asset_id, "source.md");
            seed_document_asset_link(conn, &doc_id, &asset_id, "source_upload");
        });

        let result = inspect_document_inner(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.linked_assets.len(), 1);
        assert_eq!(result.linked_assets[0].asset_id, asset_id.to_string());
        assert_eq!(result.linked_assets[0].relation_type, "source_upload");
        assert_eq!(
            result.linked_assets[0].asset_filename.as_deref(),
            Some("source.md")
        );
    }

    #[tokio::test]
    async fn inspect_document_surfaces_source_extraction_status() {
        let doc_id = DocumentId::new();
        let chunk_id = ChunkId::new();
        let asset_id = AssetId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/source.md",
                "Source",
                "text/markdown",
                1_000,
                "active",
                1,
            );
            seed_chunk(conn, &doc_id, &chunk_id, 0, "source body");
            seed_asset(conn, &asset_id, "source.md");
            seed_document_asset_link(conn, &doc_id, &asset_id, "source_upload");
            seed_asset_extraction(conn, &asset_id, "extracted", None);
        });

        let result = inspect_document_inner(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result.document.extraction_status.as_deref(),
            Some("extracted")
        );
        assert_eq!(result.document.extraction_error, None);
    }

    #[tokio::test]
    async fn inspect_document_chunks_ordered_by_chunk_index() {
        let doc_id = DocumentId::new();
        let c_idx2 = ChunkId::new();
        let c_idx0 = ChunkId::new();
        let c_idx1 = ChunkId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/ordered.md",
                "Order",
                "text/markdown",
                1_000,
                "active",
                3,
            );
            // Insert in a non-monotone order so the SQL ORDER BY is
            // load-bearing — if we forgot the ORDER BY this test
            // would surface the bug.
            seed_chunk(conn, &doc_id, &c_idx2, 2, "two");
            seed_chunk(conn, &doc_id, &c_idx0, 0, "zero");
            seed_chunk(conn, &doc_id, &c_idx1, 1, "one");
        });

        let result = inspect_document_inner(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.chunks.len(), 3);
        assert_eq!(result.chunks[0].chunk_index, 0);
        assert_eq!(result.chunks[1].chunk_index, 1);
        assert_eq!(result.chunks[2].chunk_index, 2);
    }

    #[tokio::test]
    async fn inspect_document_unknown_id_returns_none() {
        let real_id = DocumentId::new();
        let unknown_id = DocumentId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            // Seed a different doc so we prove the None is for the
            // requested id specifically, not "no docs at all".
            seed_document(
                conn,
                &real_id,
                "/tmp/x.md",
                "X",
                "text/markdown",
                1_000,
                "active",
                0,
            );
        });
        let result = inspect_document_inner(&pool, &unknown_id).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn inspect_document_content_preview_is_200_char_truncated() {
        let doc_id = DocumentId::new();
        let cid = ChunkId::new();
        let long = "a".repeat(500);
        let long_for_seed = long.clone();
        let (pool, _tmp) = pool_with_seed(move |conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/long.md",
                "Long",
                "text/markdown",
                1_000,
                "active",
                1,
            );
            seed_chunk(conn, &doc_id, &cid, 0, &long_for_seed);
        });

        let result = inspect_document_inner(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.chunks.len(), 1);
        let preview = &result.chunks[0].content_preview;
        let n_chars = preview.chars().count();
        assert_eq!(
            n_chars, CHUNK_PREVIEW_CHARS,
            "preview must be exactly {CHUNK_PREVIEW_CHARS} chars, got {n_chars}"
        );
        assert!(
            preview.ends_with('…'),
            "preview must end with U+2026 horizontal ellipsis"
        );
        // First 199 chars are still 'a' (no early splitting).
        let prefix: String = preview.chars().take(CHUNK_PREVIEW_CHARS - 1).collect();
        assert_eq!(prefix, "a".repeat(CHUNK_PREVIEW_CHARS - 1));
    }

    #[tokio::test]
    async fn inspect_document_content_preview_utf8_safe() {
        // 500 CJK codepoints (each 3 bytes UTF-8). A byte-level
        // truncation would either split a codepoint (UTF-8 error)
        // or arrive at a different char count. We assert chars()
        // arithmetic.
        let doc_id = DocumentId::new();
        let cid = ChunkId::new();
        let cjk: String = std::iter::repeat('日').take(500).collect();
        let cjk_for_seed = cjk.clone();
        let (pool, _tmp) = pool_with_seed(move |conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/cjk.md",
                "CJK",
                "text/markdown",
                1_000,
                "active",
                1,
            );
            seed_chunk(conn, &doc_id, &cid, 0, &cjk_for_seed);
        });

        let result = inspect_document_inner(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        let preview = &result.chunks[0].content_preview;
        assert_eq!(preview.chars().count(), CHUNK_PREVIEW_CHARS);
        assert!(preview.ends_with('…'));
        for c in preview.chars().take(CHUNK_PREVIEW_CHARS - 1) {
            assert_eq!(
                c, '日',
                "preview body must be all CJK chars, not a split codepoint"
            );
        }
    }

    #[tokio::test]
    async fn inspect_document_short_content_not_truncated() {
        // Off-by-one guard around the truncation boundary.
        let doc_id = DocumentId::new();
        let cid = ChunkId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/tiny.md",
                "Tiny",
                "text/markdown",
                1_000,
                "active",
                1,
            );
            seed_chunk(conn, &doc_id, &cid, 0, "tiny");
        });
        let result = inspect_document_inner(&pool, &doc_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.chunks[0].content_preview, "tiny");
        assert!(!result.chunks[0].content_preview.ends_with('…'));
    }

    // ---- list_documents ----

    #[tokio::test]
    async fn list_documents_returns_active_docs() {
        let d1 = DocumentId::new();
        let d2 = DocumentId::new();
        let d_forgotten = DocumentId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &d1,
                "/tmp/1.md",
                "One",
                "text/markdown",
                1_000,
                "active",
                0,
            );
            seed_document(
                conn,
                &d2,
                "/tmp/2.md",
                "Two",
                "text/markdown",
                2_000,
                "active",
                0,
            );
            seed_document(
                conn,
                &d_forgotten,
                "/tmp/g.md",
                "Ghost",
                "text/markdown",
                3_000,
                "forgotten",
                0,
            );
        });

        let docs = list_documents_inner(&pool, 10, 0, false).await.unwrap();
        assert_eq!(
            docs.len(),
            2,
            "forgotten doc must be filtered when include_forgotten=false"
        );
        // Newest first.
        assert_eq!(docs[0].doc_id, d2.to_string());
        assert_eq!(docs[1].doc_id, d1.to_string());
        assert!(docs.iter().all(|d| d.status == DocumentStatus::Active));
    }

    #[tokio::test]
    async fn list_documents_surfaces_source_extraction_status() {
        let doc_id = DocumentId::new();
        let asset_id = AssetId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &doc_id,
                "/tmp/failed.md",
                "Failed",
                "text/markdown",
                1_000,
                "active",
                0,
            );
            seed_asset(conn, &asset_id, "failed.md");
            seed_document_asset_link(conn, &doc_id, &asset_id, "source_import");
            seed_asset_extraction(conn, &asset_id, "failed", Some("extractor crashed"));
        });

        let docs = list_documents_inner(&pool, 10, 0, false).await.unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].doc_id, doc_id.to_string());
        assert_eq!(docs[0].extraction_status.as_deref(), Some("failed"));
        assert_eq!(
            docs[0].extraction_error.as_deref(),
            Some("extractor crashed")
        );
    }

    #[tokio::test]
    async fn list_documents_pagination_offset_works() {
        let ids: Vec<DocumentId> = (0..5).map(|_| DocumentId::new()).collect();
        let ids_for_seed = ids.clone();
        let (pool, _tmp) = pool_with_seed(move |conn| {
            for (i, id) in ids_for_seed.iter().enumerate() {
                seed_document(
                    conn,
                    id,
                    &format!("/tmp/{i}.md"),
                    &format!("Doc {i}"),
                    "text/markdown",
                    (i as i64) * 1_000,
                    "active",
                    0,
                );
            }
        });

        // Page 1 (limit=2, offset=0): 2 newest docs.
        let p1 = list_documents_inner(&pool, 2, 0, false).await.unwrap();
        assert_eq!(p1.len(), 2);
        assert_eq!(p1[0].title.as_deref(), Some("Doc 4"));
        assert_eq!(p1[1].title.as_deref(), Some("Doc 3"));

        // Page 2 (limit=2, offset=2).
        let p2 = list_documents_inner(&pool, 2, 2, false).await.unwrap();
        assert_eq!(p2.len(), 2);
        assert_eq!(p2[0].title.as_deref(), Some("Doc 2"));
        assert_eq!(p2[1].title.as_deref(), Some("Doc 1"));

        // Page 3 (limit=2, offset=4): just the last doc.
        let p3 = list_documents_inner(&pool, 2, 4, false).await.unwrap();
        assert_eq!(p3.len(), 1);
        assert_eq!(p3[0].title.as_deref(), Some("Doc 0"));

        // Page 4 (limit=2, offset=10): empty.
        let p4 = list_documents_inner(&pool, 2, 10, false).await.unwrap();
        assert!(p4.is_empty());
    }

    #[tokio::test]
    async fn list_documents_include_forgotten_flag_works() {
        let d_active = DocumentId::new();
        let d_forgotten = DocumentId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_document(
                conn,
                &d_active,
                "/tmp/a.md",
                "Active",
                "text/markdown",
                1_000,
                "active",
                0,
            );
            seed_document(
                conn,
                &d_forgotten,
                "/tmp/g.md",
                "Ghost",
                "text/markdown",
                2_000,
                "forgotten",
                0,
            );
        });

        // Active-only (default).
        let active_only = list_documents_inner(&pool, 10, 0, false).await.unwrap();
        assert_eq!(active_only.len(), 1);
        assert_eq!(active_only[0].doc_id, d_active.to_string());
        assert_eq!(active_only[0].status, DocumentStatus::Active);

        // Include forgotten.
        let all = list_documents_inner(&pool, 10, 0, true).await.unwrap();
        assert_eq!(all.len(), 2);
        // Newest first → forgotten (2000) before active (1000).
        assert_eq!(all[0].doc_id, d_forgotten.to_string());
        assert_eq!(all[0].status, DocumentStatus::Forgotten);
        assert_eq!(all[1].doc_id, d_active.to_string());
        assert_eq!(all[1].status, DocumentStatus::Active);
    }
}
