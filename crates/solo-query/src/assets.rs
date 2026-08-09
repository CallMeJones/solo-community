// SPDX-License-Identifier: Apache-2.0

//! Read-side queries for persisted original-file assets and their links.
//!
//! Assets are metadata rows for content-addressed original files stored
//! outside the SQL document tables.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use solo_core::{AssetId, DocumentId, Error, MemoryId, Result};
use solo_storage::{AuditOperation, AuditWriter, ReaderPool, expected_stored_size};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetRecord {
    pub asset_id: String,
    pub sha256: String,
    pub mime_type: String,
    pub filename: Option<String>,
    pub size_bytes: u64,
    pub storage_path: String,
    pub source: Option<String>,
    pub status: String,
    pub created_by_principal: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(skip)]
    pub encryption_alg: String,
    #[serde(skip)]
    pub encryption_nonce: Option<Vec<u8>>,
    #[serde(skip)]
    pub encrypted_size_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentAssetLinkSummary {
    pub link_id: String,
    pub doc_id: String,
    pub asset_id: String,
    pub relation_type: String,
    pub note: Option<String>,
    pub created_at_ms: i64,
    pub doc_title: Option<String>,
    pub doc_source: Option<String>,
    pub doc_status: Option<String>,
    pub asset_filename: Option<String>,
    pub asset_mime_type: Option<String>,
    pub asset_size_bytes: Option<u64>,
    pub asset_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryAttachmentSummary {
    pub attachment_id: String,
    pub memory_id: String,
    pub doc_id: Option<String>,
    pub asset_id: Option<String>,
    pub relation_type: String,
    pub note: Option<String>,
    pub created_at_ms: i64,
    pub memory_status: Option<String>,
    pub memory_preview: Option<String>,
    pub doc_title: Option<String>,
    pub doc_source: Option<String>,
    pub asset_filename: Option<String>,
    pub asset_mime_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetExtractionSummary {
    pub extraction_id: String,
    pub asset_id: String,
    pub extractor_name: String,
    pub extractor_version: String,
    pub status: String,
    pub text_chars: u64,
    pub error: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AssetInspectResult {
    pub asset: AssetRecord,
    pub extractions: Vec<AssetExtractionSummary>,
    pub document_links: Vec<DocumentAssetLinkSummary>,
    pub memory_attachments: Vec<MemoryAttachmentSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DocumentAssetsResult {
    pub doc_id: String,
    pub assets: Vec<DocumentAssetLinkSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryAttachmentsResult {
    pub memory_id: String,
    pub attachments: Vec<MemoryAttachmentSummary>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssetDownloadTarget {
    pub asset: AssetRecord,
    pub path: PathBuf,
}

pub async fn list_assets(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    limit: usize,
    offset: usize,
    include_deleted: bool,
) -> Result<Vec<AssetRecord>> {
    let result = list_assets_inner(pool, limit, offset, include_deleted).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryListAssets, None),
        Err(e) => audit.emit_error(audit_principal, AuditOperation::MemoryListAssets, None, e),
    }
    result
}

#[doc(hidden)]
pub async fn list_assets_inner(
    pool: &ReaderPool,
    limit: usize,
    offset: usize,
    include_deleted: bool,
) -> Result<Vec<AssetRecord>> {
    let limit = limit.clamp(1, 100) as i64;
    let offset = offset as i64;
    pool.interact(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT asset_id, sha256, mime_type, filename, size_bytes,
                    storage_path, source, status, created_by_principal,
                    created_at_ms, updated_at_ms, encryption_alg,
                    encryption_nonce, encrypted_size_bytes
               FROM assets
              WHERE (?1 OR status = 'active')
              ORDER BY created_at_ms DESC, asset_id ASC
              LIMIT ?2 OFFSET ?3",
        )?;
        stmt.query_map(
            rusqlite::params![include_deleted, limit, offset],
            asset_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
    })
    .await
}

pub async fn inspect_asset(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    asset_id: &AssetId,
) -> Result<Option<AssetInspectResult>> {
    let target = Some(asset_id.to_string());
    let result = inspect_asset_inner(pool, asset_id).await;
    match &result {
        Ok(_) => audit.emit_ok(audit_principal, AuditOperation::MemoryInspectAsset, target),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryInspectAsset,
            target,
            e,
        ),
    }
    result
}

pub async fn prepare_asset_download(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    snapshot_dir: &Path,
    asset_id: &AssetId,
) -> Result<AssetDownloadTarget> {
    let target = Some(asset_id.to_string());
    let result = asset_download_target_inner(pool, snapshot_dir, asset_id).await;
    match &result {
        Ok(_) => audit.emit_ok(
            audit_principal,
            AuditOperation::MemoryPrepareAssetDownload,
            target,
        ),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryPrepareAssetDownload,
            target,
            e,
        ),
    }
    result
}

/// Resolve an active asset blob for the raw download HTTP handler.
///
/// This helper audits lookup/path validation failures only. The HTTP
/// transport owns the successful `memory.download_asset` audit row so it can
/// emit success after the blob bytes have actually been read and validated.
pub async fn download_asset(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    snapshot_dir: &Path,
    asset_id: &AssetId,
) -> Result<AssetDownloadTarget> {
    let target = Some(asset_id.to_string());
    let result = asset_download_target_inner(pool, snapshot_dir, asset_id).await;
    if let Err(e) = &result {
        audit.emit_error(
            audit_principal,
            AuditOperation::MemoryDownloadAsset,
            target,
            e,
        );
    }
    result
}

#[doc(hidden)]
pub async fn asset_download_target_inner(
    pool: &ReaderPool,
    snapshot_dir: &Path,
    asset_id: &AssetId,
) -> Result<AssetDownloadTarget> {
    let asset_id = asset_id.to_string();
    let asset = {
        let query_asset_id = asset_id.clone();
        pool.interact(move |conn| {
            conn.query_row(
                "SELECT asset_id, sha256, mime_type, filename, size_bytes,
                        storage_path, source, status, created_by_principal,
                        created_at_ms, updated_at_ms, encryption_alg,
                        encryption_nonce, encrypted_size_bytes
                   FROM assets
                  WHERE asset_id = ?1",
                rusqlite::params![&query_asset_id],
                asset_from_row,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await?
        .ok_or_else(|| Error::not_found(format!("asset {asset_id} not found")))?
    };
    if asset.status != "active" {
        return Err(Error::not_found(format!("asset {asset_id} is not active")));
    }
    let path = safe_asset_storage_path(snapshot_dir, &asset.storage_path, &asset.sha256)?;
    let metadata = std::fs::metadata(&path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::not_found(format!("asset blob is missing for {asset_id}"))
        } else {
            Error::storage(format!("stat asset blob {}: {e}", path.display()))
        }
    })?;
    if !metadata.is_file() {
        return Err(Error::not_found(format!(
            "asset blob is not a file for {asset_id}"
        )));
    }
    let expected_size = expected_stored_size(
        &asset.encryption_alg,
        asset.size_bytes,
        asset.encrypted_size_bytes,
    )?;
    if metadata.len() != expected_size {
        return Err(Error::storage(format!(
            "asset blob size mismatch for {asset_id}: expected {}, got {}",
            expected_size,
            metadata.len()
        )));
    }
    Ok(AssetDownloadTarget { asset, path })
}

#[doc(hidden)]
pub async fn inspect_asset_inner(
    pool: &ReaderPool,
    asset_id: &AssetId,
) -> Result<Option<AssetInspectResult>> {
    let asset_id = asset_id.to_string();
    pool.interact(move |conn| {
        let asset = conn
            .query_row(
                "SELECT asset_id, sha256, mime_type, filename, size_bytes,
                        storage_path, source, status, created_by_principal,
                        created_at_ms, updated_at_ms, encryption_alg,
                        encryption_nonce, encrypted_size_bytes
                   FROM assets
                  WHERE asset_id = ?1",
                rusqlite::params![&asset_id],
                asset_from_row,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })?;
        let Some(asset) = asset else {
            return Ok(None);
        };
        let extractions = asset_extractions_for_asset(conn, &asset.asset_id)?;
        let document_links = document_asset_links_for_asset(conn, &asset.asset_id)?;
        let memory_attachments = memory_attachments_for_asset(conn, &asset.asset_id)?;
        Ok(Some(AssetInspectResult {
            asset,
            extractions,
            document_links,
            memory_attachments,
        }))
    })
    .await
}

pub async fn list_document_assets(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    doc_id: &DocumentId,
) -> Result<Option<DocumentAssetsResult>> {
    let target = Some(doc_id.to_string());
    let result = list_document_assets_inner(pool, doc_id).await;
    match &result {
        Ok(_) => audit.emit_ok(
            audit_principal,
            AuditOperation::MemoryListDocumentAssets,
            target,
        ),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryListDocumentAssets,
            target,
            e,
        ),
    }
    result
}

#[doc(hidden)]
pub async fn list_document_assets_inner(
    pool: &ReaderPool,
    doc_id: &DocumentId,
) -> Result<Option<DocumentAssetsResult>> {
    let doc_id = doc_id.to_string();
    pool.interact(move |conn| {
        if !row_exists(conn, "documents", "doc_id", &doc_id)? {
            return Ok(None);
        }
        let assets = document_asset_links_for_document(conn, &doc_id)?;
        Ok(Some(DocumentAssetsResult { doc_id, assets }))
    })
    .await
}

pub async fn list_memory_attachments(
    pool: &ReaderPool,
    audit: &AuditWriter,
    audit_principal: Option<String>,
    memory_id: MemoryId,
) -> Result<Option<MemoryAttachmentsResult>> {
    let target = Some(memory_id.to_string());
    let result = list_memory_attachments_inner(pool, memory_id).await;
    match &result {
        Ok(_) => audit.emit_ok(
            audit_principal,
            AuditOperation::MemoryListMemoryAttachments,
            target,
        ),
        Err(e) => audit.emit_error(
            audit_principal,
            AuditOperation::MemoryListMemoryAttachments,
            target,
            e,
        ),
    }
    result
}

#[doc(hidden)]
pub async fn list_memory_attachments_inner(
    pool: &ReaderPool,
    memory_id: MemoryId,
) -> Result<Option<MemoryAttachmentsResult>> {
    let memory_id = memory_id.to_string();
    pool.interact(move |conn| {
        if !row_exists(conn, "episodes", "memory_id", &memory_id)? {
            return Ok(None);
        }
        let attachments = memory_attachments_for_memory(conn, &memory_id)?;
        Ok(Some(MemoryAttachmentsResult {
            memory_id,
            attachments,
        }))
    })
    .await
}

fn asset_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetRecord> {
    let size_bytes: i64 = row.get(4)?;
    let encrypted_size_bytes: Option<i64> = row.get(13)?;
    Ok(AssetRecord {
        asset_id: row.get(0)?,
        sha256: row.get(1)?,
        mime_type: row.get(2)?,
        filename: row.get(3)?,
        size_bytes: size_bytes.max(0) as u64,
        storage_path: row.get(5)?,
        source: row.get(6)?,
        status: row.get(7)?,
        created_by_principal: row.get(8)?,
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
        encryption_alg: row.get(11)?,
        encryption_nonce: row.get(12)?,
        encrypted_size_bytes: encrypted_size_bytes.map(|v| v.max(0) as u64),
    })
}

fn document_asset_link_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<DocumentAssetLinkSummary> {
    let asset_size_bytes: Option<i64> = row.get(11)?;
    Ok(DocumentAssetLinkSummary {
        link_id: row.get(0)?,
        doc_id: row.get(1)?,
        asset_id: row.get(2)?,
        relation_type: row.get(3)?,
        note: row.get(4)?,
        created_at_ms: row.get(5)?,
        doc_title: row.get(6)?,
        doc_source: row.get(7)?,
        doc_status: row.get(8)?,
        asset_filename: row.get(9)?,
        asset_mime_type: row.get(10)?,
        asset_size_bytes: asset_size_bytes.map(|v| v.max(0) as u64),
        asset_sha256: row.get(12)?,
    })
}

fn memory_attachment_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<MemoryAttachmentSummary> {
    let content: Option<String> = row.get(8)?;
    Ok(MemoryAttachmentSummary {
        attachment_id: row.get(0)?,
        memory_id: row.get(1)?,
        doc_id: row.get(2)?,
        asset_id: row.get(3)?,
        relation_type: row.get(4)?,
        note: row.get(5)?,
        created_at_ms: row.get(6)?,
        memory_status: row.get(7)?,
        memory_preview: content.map(|value| truncate_chars(&value, 200)),
        doc_title: row.get(9)?,
        doc_source: row.get(10)?,
        asset_filename: row.get(11)?,
        asset_mime_type: row.get(12)?,
    })
}

fn asset_extraction_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssetExtractionSummary> {
    let text_chars: i64 = row.get(5)?;
    Ok(AssetExtractionSummary {
        extraction_id: row.get(0)?,
        asset_id: row.get(1)?,
        extractor_name: row.get(2)?,
        extractor_version: row.get(3)?,
        status: row.get(4)?,
        text_chars: text_chars.max(0) as u64,
        error: row.get(6)?,
        created_at_ms: row.get(7)?,
    })
}

fn row_exists(
    conn: &rusqlite::Connection,
    table: &str,
    id_column: &str,
    id: &str,
) -> rusqlite::Result<bool> {
    let sql = format!("SELECT COUNT(*) > 0 FROM {table} WHERE {id_column} = ?1");
    conn.query_row(&sql, rusqlite::params![id], |row| row.get(0))
}

fn asset_extractions_for_asset(
    conn: &rusqlite::Connection,
    asset_id: &str,
) -> rusqlite::Result<Vec<AssetExtractionSummary>> {
    let mut stmt = conn.prepare(
        "SELECT extraction_id, asset_id, extractor_name, extractor_version,
                status, text_chars, error, created_at_ms
           FROM asset_extractions
          WHERE asset_id = ?1
          ORDER BY created_at_ms DESC, extractor_name ASC, extractor_version ASC",
    )?;
    stmt.query_map(rusqlite::params![asset_id], asset_extraction_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
}

fn document_asset_links_for_asset(
    conn: &rusqlite::Connection,
    asset_id: &str,
) -> rusqlite::Result<Vec<DocumentAssetLinkSummary>> {
    let sql = format!(
        "{DOCUMENT_ASSET_LINK_SELECT} WHERE da.asset_id = ?1 ORDER BY da.created_at_ms DESC, da.link_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(rusqlite::params![asset_id], document_asset_link_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
}

pub(crate) fn document_asset_links_for_document(
    conn: &rusqlite::Connection,
    doc_id: &str,
) -> rusqlite::Result<Vec<DocumentAssetLinkSummary>> {
    let sql = format!(
        "{DOCUMENT_ASSET_LINK_SELECT} WHERE da.doc_id = ?1 ORDER BY da.created_at_ms DESC, da.link_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(rusqlite::params![doc_id], document_asset_link_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
}

fn memory_attachments_for_asset(
    conn: &rusqlite::Connection,
    asset_id: &str,
) -> rusqlite::Result<Vec<MemoryAttachmentSummary>> {
    let sql = format!(
        "{MEMORY_ATTACHMENT_SELECT} WHERE ma.asset_id = ?1 ORDER BY ma.created_at_ms DESC, ma.attachment_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(rusqlite::params![asset_id], memory_attachment_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
}

fn memory_attachments_for_memory(
    conn: &rusqlite::Connection,
    memory_id: &str,
) -> rusqlite::Result<Vec<MemoryAttachmentSummary>> {
    let sql = format!(
        "{MEMORY_ATTACHMENT_SELECT} WHERE ma.memory_id = ?1 ORDER BY ma.created_at_ms DESC, ma.attachment_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_map(rusqlite::params![memory_id], memory_attachment_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()
}

const DOCUMENT_ASSET_LINK_SELECT: &str = "
    SELECT da.link_id, da.doc_id, da.asset_id, da.relation_type, da.note,
           da.created_at_ms, d.title, d.source, d.status, a.filename,
           a.mime_type, a.size_bytes, a.sha256
      FROM document_assets da
      JOIN documents d ON d.doc_id = da.doc_id
      JOIN assets a ON a.asset_id = da.asset_id";

const MEMORY_ATTACHMENT_SELECT: &str = "
    SELECT ma.attachment_id, ma.memory_id, ma.doc_id, ma.asset_id,
           ma.relation_type, ma.note, ma.created_at_ms, e.status,
           e.content, d.title, d.source, a.filename, a.mime_type
      FROM memory_attachments ma
      JOIN episodes e ON e.memory_id = ma.memory_id
      LEFT JOIN documents d ON d.doc_id = ma.doc_id
      LEFT JOIN assets a ON a.asset_id = ma.asset_id";

fn truncate_chars(s: &str, max: usize) -> String {
    debug_assert!(max >= 1);
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

fn safe_asset_storage_path(
    snapshot_dir: &Path,
    storage_path: &str,
    expected_sha256: &str,
) -> Result<PathBuf> {
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
    let sha256 = normalize_sha256_hex(expected_sha256)?;
    if parts[2].len() != 2 || parts[2] != &sha256[..2] || parts[3] != sha256 {
        return Err(Error::storage(format!(
            "asset storage_path must match sha256: {storage_path:?}"
        )));
    }
    Ok(snapshot_dir.join(rel))
}

fn normalize_sha256_hex(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::storage(
            "sha256 must be 64 lowercase or uppercase hex characters",
        ));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solo_storage::test_support::{StubVectorIndex, open_test_db_at};
    use solo_storage::{AuditWriter, ReaderPool};
    use std::sync::Arc;

    fn pool_with_seed(seed: impl FnOnce(&rusqlite::Connection)) -> (ReaderPool, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.db");
        let conn = open_test_db_at(&db_path);
        seed(&conn);
        drop(conn);
        let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
            Arc::new(StubVectorIndex::new(16));
        let pool = ReaderPool::new(&db_path, None, hnsw).expect("pool");
        (pool, tmp)
    }

    fn seed_asset(conn: &rusqlite::Connection, asset_id: &AssetId, filename: &str, status: &str) {
        let hash = if filename.contains("deleted") {
            "b".repeat(64)
        } else {
            "a".repeat(64)
        };
        conn.execute(
            "INSERT INTO assets (
                asset_id, sha256, mime_type, filename, size_bytes,
                storage_path, source, status, created_by_principal,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'text/plain', ?3, 11, ?4, 'solo-staged://upload/test', ?5, 'tester', ?6, ?6)",
            rusqlite::params![
                asset_id.to_string(),
                &hash,
                filename,
                format!("assets/blobs/{}/{}", &hash[..2], hash),
                status,
                1_000_i64,
            ],
        )
        .expect("seed asset");
    }

    fn seed_document(conn: &rusqlite::Connection, doc_id: &DocumentId, title: &str) {
        conn.execute(
            "INSERT INTO documents (
                doc_id, source, title, mime_type, ingested_at_ms, status, chunk_count
             ) VALUES (?1, '/tmp/doc.md', ?2, 'text/markdown', 900, 'active', 1)",
            rusqlite::params![doc_id.to_string(), title],
        )
        .expect("seed document");
    }

    fn seed_memory(conn: &rusqlite::Connection, memory_id: MemoryId) {
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, content,
                encoding_context_json, confidence, strength, salience,
                tier, status, created_at_ms, updated_at_ms
             ) VALUES (?1, 800, 'user_message', ?2, '{}', 0.9, 0.5, 0.5, 'hot', 'active', 800, 800)",
            rusqlite::params![
                memory_id.to_string(),
                "This memory has an attached original file.",
            ],
        )
        .expect("seed memory");
    }

    #[tokio::test]
    async fn list_assets_filters_deleted_by_default() {
        let active = AssetId::new();
        let deleted = AssetId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &active, "active.txt", "active");
            seed_asset(conn, &deleted, "deleted.txt", "deleted");
        });

        let active_only = list_assets_inner(&pool, 10, 0, false).await.unwrap();
        assert_eq!(active_only.len(), 1);
        assert_eq!(active_only[0].asset_id, active.to_string());

        let all = list_assets_inner(&pool, 10, 0, true).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn inspect_asset_returns_document_and_memory_links() {
        let asset_id = AssetId::new();
        let doc_id = DocumentId::new();
        let memory_id = MemoryId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &asset_id, "source.md", "active");
            seed_document(conn, &doc_id, "Source Document");
            seed_memory(conn, memory_id);
            conn.execute(
                "INSERT INTO document_assets (
                    link_id, doc_id, asset_id, relation_type, note, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'source_upload', 'original', 1_001)",
                rusqlite::params![
                    AssetId::new().to_string(),
                    doc_id.to_string(),
                    asset_id.to_string()
                ],
            )
            .expect("seed document asset link");
            conn.execute(
                "INSERT INTO memory_attachments (
                    attachment_id, memory_id, asset_id, relation_type, note, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'source_file', 'evidence', 1_002)",
                rusqlite::params![
                    AssetId::new().to_string(),
                    memory_id.to_string(),
                    asset_id.to_string()
                ],
            )
            .expect("seed memory attachment");
        });

        let result = inspect_asset_inner(&pool, &asset_id)
            .await
            .unwrap()
            .expect("asset");
        assert_eq!(result.asset.filename.as_deref(), Some("source.md"));
        assert_eq!(result.document_links.len(), 1);
        assert_eq!(result.document_links[0].doc_id, doc_id.to_string());
        assert_eq!(result.document_links[0].relation_type, "source_upload");
        assert_eq!(result.memory_attachments.len(), 1);
        assert_eq!(
            result.memory_attachments[0].memory_id,
            memory_id.to_string()
        );
    }

    #[tokio::test]
    async fn inspect_asset_surfaces_extraction_status() {
        let asset_id = AssetId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &asset_id, "model.glb", "active");
            conn.execute(
                "INSERT INTO asset_extractions (
                    extraction_id, asset_id, extractor_name, extractor_version,
                    status, text_chars, error, created_at_ms
                 ) VALUES (?1, ?2, 'fallback_binary', 'v1', 'stored_unparsed', 0, ?3, 1_003)",
                rusqlite::params![
                    AssetId::new().to_string(),
                    asset_id.to_string(),
                    "unsupported extension: glb",
                ],
            )
            .expect("seed asset extraction");
        });

        let result = inspect_asset_inner(&pool, &asset_id)
            .await
            .unwrap()
            .expect("asset");
        assert_eq!(result.extractions.len(), 1);
        assert_eq!(result.extractions[0].asset_id, asset_id.to_string());
        assert_eq!(result.extractions[0].extractor_name, "fallback_binary");
        assert_eq!(result.extractions[0].status, "stored_unparsed");
        assert_eq!(result.extractions[0].text_chars, 0);
        assert_eq!(
            result.extractions[0].error.as_deref(),
            Some("unsupported extension: glb")
        );
    }

    #[tokio::test]
    async fn asset_download_target_resolves_active_blob() {
        let asset_id = AssetId::new();
        let (pool, tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &asset_id, "active.txt", "active");
        });
        let hash = "a".repeat(64);
        let blob_path = tmp.path().join(format!("assets/blobs/aa/{hash}"));
        std::fs::create_dir_all(blob_path.parent().expect("blob parent")).unwrap();
        std::fs::write(&blob_path, b"hello world").unwrap();

        let result = asset_download_target_inner(&pool, tmp.path(), &asset_id)
            .await
            .expect("download target");
        assert_eq!(result.asset.asset_id, asset_id.to_string());
        assert_eq!(result.path, blob_path);
    }

    #[tokio::test]
    async fn asset_download_target_accepts_encrypted_blob_size() {
        let asset_id = AssetId::new();
        let (pool, tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &asset_id, "active.txt", "active");
            conn.execute(
                "UPDATE assets
                    SET encryption_alg = 'xchacha20poly1305-blake3-v1',
                        encryption_nonce = ?1,
                        encrypted_size_bytes = 27
                  WHERE asset_id = ?2",
                rusqlite::params![vec![1u8; 24], asset_id.to_string()],
            )
            .expect("mark encrypted asset");
        });
        let hash = "a".repeat(64);
        let blob_path = tmp.path().join(format!("assets/blobs/aa/{hash}"));
        std::fs::create_dir_all(blob_path.parent().expect("blob parent")).unwrap();
        std::fs::write(&blob_path, vec![7u8; 27]).unwrap();

        let result = asset_download_target_inner(&pool, tmp.path(), &asset_id)
            .await
            .expect("download target");
        assert_eq!(result.asset.size_bytes, 11);
        assert_eq!(result.asset.encrypted_size_bytes, Some(27));
        assert_eq!(result.path, blob_path);
    }

    #[tokio::test]
    async fn asset_download_target_rejects_deleted_asset() {
        let asset_id = AssetId::new();
        let (pool, tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &asset_id, "deleted.txt", "deleted");
        });

        let err = asset_download_target_inner(&pool, tmp.path(), &asset_id)
            .await
            .expect_err("deleted asset should not be downloadable");
        assert!(
            err.to_string().contains("not active"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn download_asset_audits_resolution_errors_but_not_success() {
        let asset_id = AssetId::new();
        let missing_id = AssetId::new();
        let (pool, tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &asset_id, "active.txt", "active");
        });
        let hash = "a".repeat(64);
        let blob_path = tmp.path().join(format!("assets/blobs/aa/{hash}"));
        std::fs::create_dir_all(blob_path.parent().expect("blob parent")).unwrap();
        std::fs::write(&blob_path, b"hello world").unwrap();
        let db_path = tmp.path().join("test.db");
        let (audit, shutdown) = AuditWriter::spawn(db_path.clone(), None);

        download_asset(
            &pool,
            &audit,
            Some("tester".to_string()),
            tmp.path(),
            &asset_id,
        )
        .await
        .expect("active asset target");
        download_asset(
            &pool,
            &audit,
            Some("tester".to_string()),
            tmp.path(),
            &missing_id,
        )
        .await
        .expect_err("missing asset should audit an error");
        drop(audit);
        shutdown.join().await;

        let conn = open_test_db_at(&db_path);
        let (result, target_id): (String, Option<String>) = conn
            .query_row(
                "SELECT result, target_id FROM audit_events
                 WHERE operation = 'memory.download_asset'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("one download audit row");
        assert_eq!(result, "error");
        assert_eq!(target_id, Some(missing_id.to_string()));
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_events
                 WHERE operation = 'memory.download_asset'",
                [],
                |row| row.get(0),
            )
            .expect("audit row count");
        assert_eq!(
            count, 1,
            "successful target resolution must not audit download success before bytes are served"
        );
    }

    #[tokio::test]
    async fn list_document_assets_returns_none_for_unknown_document() {
        let (pool, _tmp) = pool_with_seed(|_conn| {});
        let result = list_document_assets_inner(&pool, &DocumentId::new())
            .await
            .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn list_memory_attachments_returns_document_and_asset_targets() {
        let asset_id = AssetId::new();
        let doc_id = DocumentId::new();
        let memory_id = MemoryId::new();
        let (pool, _tmp) = pool_with_seed(|conn| {
            seed_asset(conn, &asset_id, "source.md", "active");
            seed_document(conn, &doc_id, "Source Document");
            seed_memory(conn, memory_id);
            conn.execute(
                "INSERT INTO memory_attachments (
                    attachment_id, memory_id, doc_id, relation_type, note, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'document_evidence', NULL, 1_001)",
                rusqlite::params![
                    AssetId::new().to_string(),
                    memory_id.to_string(),
                    doc_id.to_string()
                ],
            )
            .expect("seed doc attachment");
            conn.execute(
                "INSERT INTO memory_attachments (
                    attachment_id, memory_id, asset_id, relation_type, note, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'source_file', NULL, 1_002)",
                rusqlite::params![
                    AssetId::new().to_string(),
                    memory_id.to_string(),
                    asset_id.to_string()
                ],
            )
            .expect("seed asset attachment");
        });

        let result = list_memory_attachments_inner(&pool, memory_id)
            .await
            .unwrap()
            .expect("memory attachments");
        assert_eq!(result.attachments.len(), 2);
        assert!(
            result
                .attachments
                .iter()
                .any(|item| item.doc_id.as_deref() == Some(&doc_id.to_string()))
        );
        assert!(
            result
                .attachments
                .iter()
                .any(|item| item.asset_id.as_deref() == Some(&asset_id.to_string()))
        );
    }
}
