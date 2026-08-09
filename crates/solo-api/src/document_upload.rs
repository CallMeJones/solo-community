// SPDX-License-Identifier: Apache-2.0

//! Resumable document upload staging for Solo.
//!
//! MCP tools use this module as the control plane. The raw bytes move
//! through HTTP `PATCH /uploads/{upload_id}` so large files do not get
//! stuffed into JSON-RPC tool arguments.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solo_core::{DocumentId, Error, Result};
use solo_storage::{
    AssetExtractionReport, DocumentAssetLinkReport, IngestReport, LibraryHandle, StoredAssetReport,
};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

pub const MAX_UPLOAD_BYTES: u64 = 100 * 1024 * 1024;
pub const RECOMMENDED_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub const MCP_BASE64_CHUNK_BYTES: usize = 512 * 1024;
pub const UPLOAD_TTL_SECS: i64 = 60 * 60;
pub const TERMINAL_RECEIPT_TTL_SECS: i64 = 24 * 60 * 60;
pub const UPLOAD_PROTOCOL: &str = "solo-resumable-v1";
pub const STAGED_URI_PREFIX: &str = "solo-staged://upload/";
pub const HTTP_UPLOAD_ROUTE_PREFIX: &str = "/uploads";
pub const UPLOAD_OFFSET_HEADER: &str = "upload-offset";
pub const UPLOAD_LENGTH_HEADER: &str = "upload-length";
pub const UPLOAD_STATUS_HEADER: &str = "x-solo-upload-status";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadPrepareRequest {
    pub filename: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    pub size_bytes: u64,
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadPrepareResponse {
    pub upload_id: String,
    pub upload_url: String,
    pub upload_path: String,
    pub route_kind: String,
    pub upload_method: String,
    pub upload_content_type: String,
    pub upload_offset_header: String,
    pub upload_length_header: String,
    pub upload_status_header: String,
    pub upload_headers: BTreeMap<String, String>,
    pub required_headers: BTreeMap<String, String>,
    pub upload_auth: UploadAuthContract,
    pub protocol: String,
    pub max_file_bytes: u64,
    pub max_chunk_bytes: usize,
    pub recommended_chunk_bytes: usize,
    pub mcp_fallback: UploadMcpFallbackContract,
    pub expires_at_ms: i64,
    pub commit_tool: String,
    pub ingest_tool: String,
    pub default_store_original_file: bool,
    pub next_actions: Vec<UploadNextAction>,
    pub next_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadAuthContract {
    pub mode: String,
    pub required: String,
    pub header: String,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadMcpFallbackContract {
    pub tool: String,
    pub max_chunk_bytes: usize,
    pub max_file_bytes: usize,
    pub encoding: String,
    pub preferred: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadNextAction {
    pub action: String,
    pub transport: String,
    pub method: Option<String>,
    pub url_field: Option<String>,
    pub headers_field: Option<String>,
    pub tool: Option<String>,
    pub when: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadStatusResponse {
    pub upload_id: String,
    pub status: UploadStatus,
    pub bytes_received: u64,
    pub size_bytes: u64,
    pub next_offset: u64,
    pub expires_at_ms: i64,
    pub operation_in_progress: bool,
    pub active_operation: Option<String>,
    pub staged_uri: Option<String>,
    pub commit_result: Option<UploadCommitResponse>,
    pub ingest_result: Option<StagedIngestResponse>,
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadCommitRequest {
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadCommitResponse {
    pub upload_id: String,
    pub staged_uri: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadAbortResponse {
    pub upload_id: String,
    pub status: UploadStatus,
    pub cleanup_performed: bool,
    pub already_aborted: bool,
    pub removed_partial_file: bool,
    pub removed_staged_file: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedIngestRequest {
    pub staged_uri: String,
    #[serde(default)]
    pub retain_source_file: bool,
    #[serde(default)]
    pub store_original_file: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedIngestResponse {
    pub staged_uri: String,
    pub document_id: Option<String>,
    pub chunks_persisted: u32,
    pub bytes_ingested: u64,
    pub deduped: bool,
    pub stored_original_file: bool,
    pub asset: Option<StoredAssetReport>,
    pub document_asset_link: Option<DocumentAssetLinkReport>,
    pub extraction_status: String,
    pub extraction_error: Option<String>,
    pub extraction: Option<AssetExtractionReport>,
    pub deleted_staged_file: bool,
    pub retained_source_file: bool,
    pub report: Option<IngestReport>,
    pub idempotent_replay: bool,
    pub ingest_completed_at_ms: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct RetainOriginalFileRequest {
    pub path: PathBuf,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    pub source: Option<String>,
    pub relation_type: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadStatus {
    Open,
    Busy,
    Committed,
    Ingested,
    Expired,
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadManifest {
    upload_id: String,
    filename: String,
    sanitized_filename: String,
    mime_type: String,
    size_bytes: u64,
    expected_sha256: Option<String>,
    actual_sha256: Option<String>,
    bytes_received: u64,
    status: UploadStatus,
    created_at_ms: i64,
    expires_at_ms: i64,
    #[serde(default)]
    ingest_result: Option<StagedIngestResponse>,
}

struct UploadPaths {
    dir: PathBuf,
    manifest: PathBuf,
    part: PathBuf,
    final_path: PathBuf,
    lock: PathBuf,
}

struct UploadLock {
    file: Option<File>,
    path: PathBuf,
}

impl Drop for UploadLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let mut active = active_upload_operations()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            active.remove(&self.path);
            let _ = FileExt::unlock(&file);
            drop(file);
        }
    }
}

fn active_upload_operations() -> &'static Mutex<HashMap<PathBuf, String>> {
    static ACTIVE: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn prepare_upload(
    data_dir: &Path,
    request: UploadPrepareRequest,
    allowed_extensions: &[String],
    default_store_original_file: bool,
) -> Result<UploadPrepareResponse> {
    validate_prepare_request(&request, allowed_extensions, default_store_original_file)?;
    if let Err(e) = sweep_expired_uploads(data_dir) {
        tracing::warn!(
            error = %e,
            "failed to sweep expired staged document uploads"
        );
    }
    let upload_id = Uuid::now_v7().to_string();
    let sanitized_filename = sanitize_filename(&request.filename);
    let created_at_ms = now_ms();
    let expires_at_ms = created_at_ms.saturating_add(UPLOAD_TTL_SECS.saturating_mul(1000));
    let mime_type = request
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream")
        .to_string();
    let manifest = UploadManifest {
        upload_id: upload_id.clone(),
        filename: request.filename,
        sanitized_filename,
        mime_type,
        size_bytes: request.size_bytes,
        expected_sha256: normalize_sha256(request.sha256.as_deref())?,
        actual_sha256: None,
        bytes_received: 0,
        status: UploadStatus::Open,
        created_at_ms,
        expires_at_ms,
        ingest_result: None,
    };
    let paths = upload_paths(data_dir, &upload_id, &manifest.sanitized_filename);
    std::fs::create_dir_all(&paths.dir).map_err(|e| {
        Error::storage(format!(
            "create upload staging dir {}: {e}",
            paths.dir.display()
        ))
    })?;
    write_manifest(&paths.manifest, &manifest)?;
    let upload_path = format!("{HTTP_UPLOAD_ROUTE_PREFIX}/{upload_id}");
    let mut required_headers = BTreeMap::new();
    required_headers.insert(UPLOAD_OFFSET_HEADER.to_string(), "0".to_string());
    required_headers.insert(
        "content-type".to_string(),
        "application/octet-stream".to_string(),
    );
    required_headers.insert(
        UPLOAD_LENGTH_HEADER.to_string(),
        request.size_bytes.to_string(),
    );
    let upload_headers = required_headers.clone();

    Ok(UploadPrepareResponse {
        upload_id: upload_id.clone(),
        upload_url: upload_path.clone(),
        upload_path,
        route_kind: "direct_local".to_string(),
        upload_method: "PATCH".to_string(),
        upload_content_type: "application/octet-stream".to_string(),
        upload_offset_header: UPLOAD_OFFSET_HEADER.to_string(),
        upload_length_header: UPLOAD_LENGTH_HEADER.to_string(),
        upload_status_header: UPLOAD_STATUS_HEADER.to_string(),
        upload_headers,
        required_headers,
        upload_auth: UploadAuthContract {
            mode: "same_as_solo_http".to_string(),
            required: "when the Solo HTTP API is configured with auth".to_string(),
            header: "authorization".to_string(),
            note: "Direct Solo HTTP uploads use the same Authorization bearer as the rest of the Solo API.".to_string(),
        },
        protocol: UPLOAD_PROTOCOL.to_string(),
        max_file_bytes: MAX_UPLOAD_BYTES,
        max_chunk_bytes: RECOMMENDED_CHUNK_BYTES,
        recommended_chunk_bytes: RECOMMENDED_CHUNK_BYTES,
        mcp_fallback: UploadMcpFallbackContract {
            tool: "document_upload_chunk_base64".to_string(),
            max_chunk_bytes: MCP_BASE64_CHUNK_BYTES,
            max_file_bytes: MCP_BASE64_CHUNK_BYTES,
            encoding: "base64".to_string(),
            preferred: false,
            note: "Use only when the client cannot send raw HTTP PATCH bytes. Raw HTTP is preferred for document uploads.".to_string(),
        },
        expires_at_ms,
        commit_tool: "document_upload_commit".to_string(),
        ingest_tool: "memory_ingest_staged_document".to_string(),
        default_store_original_file,
        next_actions: upload_next_actions(),
        next_steps: vec![
            "Send raw file bytes to upload_url with upload_method and required_headers; do not base64-encode the file in MCP tool arguments unless raw HTTP is unavailable and the file fits the mcp_fallback limit.".to_string(),
            "If interrupted, call document_upload_status. Poll while status is busy; resume only after status is open, using next_offset.".to_string(),
            "After bytes_received equals size_bytes, call document_upload_commit with upload_id and optional sha256. Commit is idempotent, so retry it after a lost response.".to_string(),
            "Call memory_ingest_staged_document with the returned staged_uri. Ingest is idempotent: retry after a lost response, or recover its 24-hour terminal receipt from document_upload_status when status is ingested.".to_string(),
        ],
    })
}

fn upload_next_actions() -> Vec<UploadNextAction> {
    vec![
        UploadNextAction {
            action: "upload_bytes".to_string(),
            transport: "raw_http".to_string(),
            method: Some("PATCH".to_string()),
            url_field: Some("upload_url".to_string()),
            headers_field: Some("required_headers".to_string()),
            tool: None,
            when: Some("preferred".to_string()),
        },
        UploadNextAction {
            action: "upload_bytes_base64".to_string(),
            transport: "mcp_tool".to_string(),
            method: None,
            url_field: None,
            headers_field: None,
            tool: Some("document_upload_chunk_base64".to_string()),
            when: Some("only_if_raw_http_unavailable_and_file_fits_mcp_fallback".to_string()),
        },
        UploadNextAction {
            action: "resume_status".to_string(),
            transport: "mcp_tool".to_string(),
            method: None,
            url_field: None,
            headers_field: None,
            tool: Some("document_upload_status".to_string()),
            when: Some("if_interrupted".to_string()),
        },
        UploadNextAction {
            action: "commit".to_string(),
            transport: "mcp_tool".to_string(),
            method: None,
            url_field: None,
            headers_field: None,
            tool: Some("document_upload_commit".to_string()),
            when: Some("after_bytes_received_equals_size_bytes".to_string()),
        },
        UploadNextAction {
            action: "ingest".to_string(),
            transport: "mcp_tool".to_string(),
            method: None,
            url_field: None,
            headers_field: None,
            tool: Some("memory_ingest_staged_document".to_string()),
            when: Some("after_commit_returns_staged_uri".to_string()),
        },
    ]
}

pub async fn append_upload_chunk(
    data_dir: &Path,
    upload_id: &str,
    offset: u64,
    upload_length: Option<u64>,
    bytes: &[u8],
) -> Result<UploadStatusResponse> {
    validate_upload_id(upload_id)?;
    if bytes.is_empty() {
        return Err(Error::invalid_input("upload chunk must not be empty"));
    }
    if bytes.len() > RECOMMENDED_CHUNK_BYTES {
        return Err(Error::invalid_input(format!(
            "upload chunk must be <= {RECOMMENDED_CHUNK_BYTES} bytes"
        )));
    }

    let _lock = acquire_upload_lock(&upload_lock_path(data_dir, upload_id), "append")?;
    let mut manifest = load_manifest_for_upload(data_dir, upload_id)?;
    ensure_open(&manifest)?;
    if let Some(upload_length) = upload_length
        && upload_length != manifest.size_bytes
    {
        return Err(Error::invalid_input(format!(
            "{UPLOAD_LENGTH_HEADER} ({upload_length}) does not match prepared size_bytes ({})",
            manifest.size_bytes
        )));
    }
    if offset != manifest.bytes_received {
        return Err(Error::conflict(format!(
            "wrong upload offset: expected {}, got {offset}",
            manifest.bytes_received
        )));
    }
    let previous_offset = manifest.bytes_received;
    let incoming = u64::try_from(bytes.len())
        .map_err(|_| Error::invalid_input("upload chunk is too large"))?;
    let next = manifest
        .bytes_received
        .checked_add(incoming)
        .ok_or_else(|| Error::invalid_input("upload size overflow"))?;
    if next > manifest.size_bytes {
        return Err(Error::invalid_input(format!(
            "upload would exceed prepared size_bytes ({})",
            manifest.size_bytes
        )));
    }
    if next > MAX_UPLOAD_BYTES {
        return Err(Error::invalid_input(format!(
            "upload would exceed max_file_bytes ({MAX_UPLOAD_BYTES})"
        )));
    }

    let paths = upload_paths(data_dir, upload_id, &manifest.sanitized_filename);
    let current_len = match tokio::fs::metadata(&paths.part).await {
        Ok(meta) => meta.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            return Err(Error::storage(format!(
                "metadata upload part {}: {e}",
                paths.part.display()
            )));
        }
    };
    if current_len != manifest.bytes_received {
        return Err(Error::conflict(format!(
            "upload part length ({current_len}) does not match manifest offset ({})",
            manifest.bytes_received
        )));
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.part)
        .await
        .map_err(|e| Error::storage(format!("open upload part {}: {e}", paths.part.display())))?;
    file.write_all(bytes)
        .await
        .map_err(|e| Error::storage(format!("write upload part {}: {e}", paths.part.display())))?;
    file.flush()
        .await
        .map_err(|e| Error::storage(format!("flush upload part {}: {e}", paths.part.display())))?;
    drop(file);
    manifest.bytes_received = next;
    if let Err(e) = write_manifest(&paths.manifest, &manifest) {
        rollback_upload_part(&paths.part, previous_offset).await;
        return Err(e);
    }
    Ok(status_from_manifest(&manifest))
}

pub fn upload_status(data_dir: &Path, upload_id: &str) -> Result<UploadStatusResponse> {
    validate_upload_id(upload_id)?;
    let lock_path = upload_lock_path(data_dir, upload_id);
    match try_acquire_upload_lock(&lock_path, "status")? {
        Some(_lock) => {
            let manifest = load_manifest_for_upload(data_dir, upload_id)?;
            Ok(status_from_manifest(&manifest))
        }
        None => {
            // Never report a stale `open` snapshot while commit/append/ingest
            // owns the lifecycle lock. Clients can poll `busy` until the
            // operation publishes a stable state.
            let manifest = load_manifest_for_upload(data_dir, upload_id)?;
            let mut status = status_from_manifest(&manifest);
            status.status = UploadStatus::Busy;
            status.operation_in_progress = true;
            status.active_operation = Some(read_upload_lock_operation(&lock_path));
            status.terminal = false;
            Ok(status)
        }
    }
}

pub fn abort_upload(data_dir: &Path, upload_id: &str) -> Result<UploadAbortResponse> {
    validate_upload_id(upload_id)?;
    let _lock = acquire_upload_lock(&upload_lock_path(data_dir, upload_id), "abort")?;
    let mut manifest = load_manifest_for_upload(data_dir, upload_id)?;
    if manifest.status == UploadStatus::Ingested {
        return Err(Error::conflict(
            "upload is already ingested; inspect its terminal status receipt",
        ));
    }
    if manifest.status == UploadStatus::Aborted {
        return Ok(UploadAbortResponse {
            upload_id: upload_id.to_string(),
            status: UploadStatus::Aborted,
            cleanup_performed: false,
            already_aborted: true,
            removed_partial_file: false,
            removed_staged_file: false,
        });
    }
    let paths = upload_paths(data_dir, upload_id, &manifest.sanitized_filename);
    let removed_partial_file = paths.part.is_file();
    let removed_staged_file = paths.final_path.is_file();
    remove_if_exists(&paths.part)?;
    remove_if_exists(&paths.final_path)?;
    remove_dir_if_empty(&paths.dir)?;
    manifest.status = UploadStatus::Aborted;
    manifest.expires_at_ms = terminal_receipt_expires_at_ms();
    manifest.ingest_result = None;
    write_manifest(&paths.manifest, &manifest)?;
    Ok(UploadAbortResponse {
        upload_id: upload_id.to_string(),
        status: UploadStatus::Aborted,
        cleanup_performed: removed_partial_file || removed_staged_file,
        already_aborted: false,
        removed_partial_file,
        removed_staged_file,
    })
}

pub async fn commit_upload(
    data_dir: &Path,
    upload_id: &str,
    request: UploadCommitRequest,
) -> Result<UploadCommitResponse> {
    validate_upload_id(upload_id)?;
    let _lock = acquire_upload_lock(&upload_lock_path(data_dir, upload_id), "commit")?;
    let mut manifest = load_manifest_for_upload(data_dir, upload_id)?;
    if upload_is_expired_at(&manifest, now_ms()) {
        return Err(Error::invalid_input("staged upload has expired"));
    }
    match manifest.status {
        UploadStatus::Committed | UploadStatus::Ingested => {
            return commit_response_from_manifest(&manifest);
        }
        UploadStatus::Aborted => return Err(Error::conflict("upload is aborted")),
        UploadStatus::Open => {}
        UploadStatus::Busy | UploadStatus::Expired => {
            return Err(Error::conflict(format!(
                "upload cannot be committed from status {:?}",
                manifest.status
            )));
        }
    }
    ensure_open(&manifest)?;
    if manifest.bytes_received != manifest.size_bytes {
        return Err(Error::conflict(format!(
            "upload incomplete: received {} of {} bytes",
            manifest.bytes_received, manifest.size_bytes
        )));
    }
    let paths = upload_paths(data_dir, upload_id, &manifest.sanitized_filename);
    let part_len = tokio::fs::metadata(&paths.part)
        .await
        .map_err(|e| {
            Error::storage(format!(
                "metadata upload part {}: {e}",
                paths.part.display()
            ))
        })?
        .len();
    if part_len != manifest.size_bytes {
        return Err(Error::conflict(format!(
            "upload part length ({part_len}) does not match prepared size_bytes ({})",
            manifest.size_bytes
        )));
    }
    let part = paths.part.clone();
    let actual_sha256 = tokio::task::spawn_blocking(move || sha256_file(&part))
        .await
        .map_err(|e| Error::storage(format!("hash upload task failed: {e}")))??;
    if let Some(expected) =
        normalize_sha256(request.sha256.as_deref())?.or_else(|| manifest.expected_sha256.clone())
        && expected != actual_sha256
    {
        return Err(Error::invalid_input(format!(
            "sha256 mismatch: expected {expected}, got {actual_sha256}"
        )));
    }
    if let Some(parent) = paths.final_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::storage(format!("create final upload dir {}: {e}", parent.display()))
        })?;
    }
    if paths.final_path.exists() {
        remove_if_exists(&paths.final_path)?;
    }
    std::fs::rename(&paths.part, &paths.final_path).map_err(|e| {
        Error::storage(format!(
            "commit upload {} -> {}: {e}",
            paths.part.display(),
            paths.final_path.display()
        ))
    })?;
    manifest.actual_sha256 = Some(actual_sha256.clone());
    manifest.status = UploadStatus::Committed;
    if let Err(e) = write_manifest(&paths.manifest, &manifest) {
        if let Err(rollback) = std::fs::rename(&paths.final_path, &paths.part) {
            tracing::warn!(
                error = %rollback,
                final_path = %paths.final_path.display(),
                part_path = %paths.part.display(),
                "failed to roll back staged upload after manifest commit failure"
            );
        }
        return Err(e);
    }
    commit_response_from_manifest(&manifest)
}

pub async fn ingest_staged_document(
    data_dir: &Path,
    tenant: &LibraryHandle,
    audit_principal: Option<String>,
    request: StagedIngestRequest,
) -> Result<StagedIngestResponse> {
    let upload_id = parse_staged_uri(&request.staged_uri)?;
    let _lock = acquire_upload_lock(&upload_lock_path(data_dir, upload_id), "ingest")?;
    let mut manifest = load_manifest_for_upload(data_dir, upload_id)?;
    if manifest.status == UploadStatus::Ingested {
        let paths = upload_paths(data_dir, upload_id, &manifest.sanitized_filename);
        let mut receipt = manifest.ingest_result.clone().ok_or_else(|| {
            Error::storage(format!(
                "ingested upload {upload_id} is missing its terminal receipt"
            ))
        })?;
        if !receipt.retained_source_file && !receipt.deleted_staged_file {
            receipt.deleted_staged_file =
                cleanup_staged_upload_after_ingest(&paths, receipt.retained_source_file);
            manifest.ingest_result = Some(receipt.clone());
            write_manifest(&paths.manifest, &manifest)?;
        }
        receipt.idempotent_replay = true;
        return Ok(receipt);
    }
    if status_from_manifest(&manifest).status == UploadStatus::Expired {
        return Err(Error::invalid_input("staged upload has expired"));
    }
    if manifest.status != UploadStatus::Committed {
        return Err(Error::conflict(format!(
            "staged upload is not committed (status: {:?})",
            manifest.status
        )));
    }
    let paths = upload_paths(data_dir, upload_id, &manifest.sanitized_filename);
    if !paths.final_path.is_file() {
        return Err(Error::not_found(format!(
            "staged upload file is missing for {upload_id}"
        )));
    }
    let chunk_config = chunk_config_from_document_config(&tenant.config().documents)?;
    let store_original_file = request
        .store_original_file
        .unwrap_or(tenant.config().documents.store_original_files_by_default);
    let document_ingest_allowed = has_allowed_extension(
        &manifest.sanitized_filename,
        &tenant.config().documents.allowed_extensions,
    );

    let asset = if store_original_file {
        Some(
            store_original_file_for_upload(
                tenant,
                audit_principal.clone(),
                &paths.final_path,
                &manifest,
                &request.staged_uri,
            )
            .await?,
        )
    } else {
        None
    };

    if let Some(asset) = asset.as_ref()
        && !document_ingest_allowed
    {
        let extraction_status = "stored_unparsed".to_string();
        let extraction_error = unsupported_extension_error(&manifest.filename);
        let extraction = tenant
            .write()
            .record_asset_extraction_as(
                audit_principal.clone(),
                asset.asset_id,
                solo_storage::document::FALLBACK_BINARY_EXTRACTOR.to_string(),
                solo_storage::document::TEXT_EXTRACTOR_VERSION.to_string(),
                extraction_status.clone(),
                0,
                Some(extraction_error.clone()),
            )
            .await?;
        let response = StagedIngestResponse {
            staged_uri: request.staged_uri,
            document_id: None,
            chunks_persisted: 0,
            bytes_ingested: 0,
            deduped: false,
            stored_original_file: true,
            asset: Some(asset.clone()),
            document_asset_link: None,
            extraction_status,
            extraction_error: Some(extraction_error),
            extraction: Some(extraction),
            deleted_staged_file: false,
            retained_source_file: request.retain_source_file,
            report: None,
            idempotent_replay: false,
            ingest_completed_at_ms: 0,
        };
        return finalize_ingest_response(&paths, &mut manifest, response);
    }

    let report = match tenant
        .write()
        .ingest_document_as(
            audit_principal.clone(),
            paths.final_path.clone(),
            chunk_config,
        )
        .await
    {
        Ok(report) => report,
        Err(err) => {
            if let Some(asset) = asset.as_ref() {
                let extraction_error = err.to_string();
                let extraction_status =
                    extraction_status_for_ingest_error(&extraction_error).to_string();
                let (extractor_name, extractor_version) =
                    extraction_identity_for_path(&paths.final_path);
                let extraction = tenant
                    .write()
                    .record_asset_extraction_as(
                        audit_principal.clone(),
                        asset.asset_id,
                        extractor_name,
                        extractor_version,
                        extraction_status.clone(),
                        0,
                        Some(extraction_error.clone()),
                    )
                    .await?;
                let response = StagedIngestResponse {
                    staged_uri: request.staged_uri,
                    document_id: None,
                    chunks_persisted: 0,
                    bytes_ingested: 0,
                    deduped: false,
                    stored_original_file: true,
                    asset: Some(asset.clone()),
                    document_asset_link: None,
                    extraction_status,
                    extraction_error: Some(extraction_error),
                    extraction: Some(extraction),
                    deleted_staged_file: false,
                    retained_source_file: request.retain_source_file,
                    report: None,
                    idempotent_replay: false,
                    ingest_completed_at_ms: 0,
                };
                return finalize_ingest_response(&paths, &mut manifest, response);
            }
            return Err(err);
        }
    };

    let (asset, document_asset_link, extraction) = if let Some(asset) = asset {
        let link = tenant
            .write()
            .link_document_asset_as(
                audit_principal.clone(),
                report.doc_id,
                asset.asset_id,
                "source_upload".to_string(),
                Some("original staged upload".to_string()),
            )
            .await?;
        let extraction = Some(
            tenant
                .write()
                .record_asset_extraction_as(
                    audit_principal,
                    asset.asset_id,
                    report.extractor_name.clone(),
                    report.extractor_version.clone(),
                    "extracted".to_string(),
                    report.text_chars,
                    None,
                )
                .await?,
        );
        (Some(asset), Some(link), extraction)
    } else {
        (None, None, None)
    };

    let response = StagedIngestResponse {
        staged_uri: request.staged_uri,
        document_id: Some(report.doc_id.to_string()),
        chunks_persisted: report.chunks_persisted,
        bytes_ingested: report.bytes_ingested,
        deduped: report.deduped,
        stored_original_file: asset.is_some(),
        asset,
        document_asset_link,
        extraction_status: "extracted".to_string(),
        extraction_error: None,
        extraction,
        deleted_staged_file: false,
        retained_source_file: request.retain_source_file,
        report: Some(report),
        idempotent_replay: false,
        ingest_completed_at_ms: 0,
    };
    finalize_ingest_response(&paths, &mut manifest, response)
}

async fn store_original_file_for_upload(
    tenant: &LibraryHandle,
    audit_principal: Option<String>,
    path: &Path,
    manifest: &UploadManifest,
    staged_uri: &str,
) -> Result<StoredAssetReport> {
    tenant
        .write()
        .store_asset_from_path_as(
            audit_principal,
            path.to_path_buf(),
            Some(manifest.filename.clone()),
            Some(manifest.mime_type.clone()),
            Some(manifest.size_bytes),
            manifest.actual_sha256.clone(),
            Some(staged_uri.to_string()),
        )
        .await
}

fn extraction_status_for_ingest_error(error: &str) -> &'static str {
    if error.contains("file is empty")
        || error.contains(solo_storage::document::NO_EXTRACTABLE_TEXT_ERROR_MARKER)
    {
        "stored_unparsed"
    } else {
        "failed"
    }
}

fn extraction_identity_for_path(path: &Path) -> (String, String) {
    let extractor_name = document_mime_type_for_path(path)
        .as_deref()
        .map(solo_storage::document::extractor_name_for_mime)
        .unwrap_or(solo_storage::document::FALLBACK_BINARY_EXTRACTOR)
        .to_string();
    (
        extractor_name,
        solo_storage::document::TEXT_EXTRACTOR_VERSION.to_string(),
    )
}

fn unsupported_extension_error(filename: &str) -> String {
    let ext = Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_else(|| "(no extension)".to_string());
    format!("unsupported extension: {ext}")
}

fn cleanup_staged_upload_after_ingest(paths: &UploadPaths, retain_source_file: bool) -> bool {
    if retain_source_file {
        return false;
    }
    match remove_if_exists(&paths.final_path) {
        Ok(()) => {
            let _ = remove_dir_if_empty(&paths.dir);
            true
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                staged_path = %paths.final_path.display(),
                "failed to delete staged source after ingest; leaving manifest for retry/abort"
            );
            false
        }
    }
}

fn finalize_ingest_response(
    paths: &UploadPaths,
    manifest: &mut UploadManifest,
    mut response: StagedIngestResponse,
) -> Result<StagedIngestResponse> {
    let completed_at_ms = now_ms();
    response.idempotent_replay = false;
    response.ingest_completed_at_ms = completed_at_ms;

    // Publish the durable receipt before deleting raw staging. If cleanup or
    // the final receipt update is interrupted, a retry can still return the
    // terminal result without replaying database mutations.
    manifest.status = UploadStatus::Ingested;
    manifest.expires_at_ms = terminal_receipt_expires_at_ms();
    manifest.ingest_result = Some(response.clone());
    write_manifest(&paths.manifest, manifest)?;

    response.deleted_staged_file =
        cleanup_staged_upload_after_ingest(paths, response.retained_source_file);
    manifest.ingest_result = Some(response.clone());
    write_manifest(&paths.manifest, manifest)?;
    Ok(response)
}

fn commit_response_from_manifest(manifest: &UploadManifest) -> Result<UploadCommitResponse> {
    let sha256 = manifest.actual_sha256.clone().ok_or_else(|| {
        Error::storage(format!(
            "committed upload {} is missing its SHA-256",
            manifest.upload_id
        ))
    })?;
    Ok(UploadCommitResponse {
        upload_id: manifest.upload_id.clone(),
        staged_uri: staged_uri(&manifest.upload_id),
        filename: manifest.filename.clone(),
        mime_type: manifest.mime_type.clone(),
        size_bytes: manifest.size_bytes,
        sha256,
    })
}

pub(crate) async fn retain_original_file_for_document(
    tenant: &LibraryHandle,
    audit_principal: Option<String>,
    doc_id: DocumentId,
    request: RetainOriginalFileRequest,
) -> Result<(StoredAssetReport, DocumentAssetLinkReport)> {
    let asset = tenant
        .write()
        .store_asset_from_path_as(
            audit_principal.clone(),
            request.path,
            request.filename,
            request.mime_type,
            request.size_bytes,
            request.sha256,
            request.source,
        )
        .await?;
    let link = tenant
        .write()
        .link_document_asset_as(
            audit_principal,
            doc_id,
            asset.asset_id,
            request.relation_type,
            request.note,
        )
        .await?;
    Ok((asset, link))
}

pub(crate) fn document_mime_type_for_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?;
    solo_storage::document::mime_type_for_extension(ext).map(str::to_string)
}

pub fn staged_path_for_tests(data_dir: &Path, staged_uri: &str) -> Result<PathBuf> {
    let upload_id = parse_staged_uri(staged_uri)?;
    let manifest = load_manifest_for_upload(data_dir, upload_id)?;
    Ok(upload_paths(data_dir, upload_id, &manifest.sanitized_filename).final_path)
}

#[cfg(test)]
pub(crate) fn upload_part_path_for_tests(data_dir: &Path, upload_id: &str) -> Result<PathBuf> {
    validate_upload_id(upload_id)?;
    Ok(upload_paths(data_dir, upload_id, "upload").part)
}

fn validate_prepare_request(
    request: &UploadPrepareRequest,
    allowed_extensions: &[String],
    allow_asset_only_uploads: bool,
) -> Result<()> {
    if request.filename.trim().is_empty() {
        return Err(Error::invalid_input("filename must not be empty"));
    }
    if request.size_bytes == 0 {
        return Err(Error::invalid_input("size_bytes must be > 0"));
    }
    if request.size_bytes > MAX_UPLOAD_BYTES {
        return Err(Error::invalid_input(format!(
            "size_bytes must be <= {MAX_UPLOAD_BYTES}"
        )));
    }
    let filename = sanitize_filename(&request.filename);
    if !allow_asset_only_uploads && !has_allowed_extension(&filename, allowed_extensions) {
        return Err(Error::invalid_input(format!(
            "filename extension is not allowed: {}",
            request.filename
        )));
    }
    let _ = normalize_sha256(request.sha256.as_deref())?;
    Ok(())
}

fn load_manifest_for_upload(data_dir: &Path, upload_id: &str) -> Result<UploadManifest> {
    validate_upload_id(upload_id)?;
    let paths = upload_paths(data_dir, upload_id, "upload");
    let mut failures = Vec::new();
    let mut found_candidate = false;
    for candidate in manifest_recovery_paths(&paths.manifest) {
        let raw = match std::fs::read_to_string(&candidate) {
            Ok(raw) => {
                found_candidate = true;
                raw
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                found_candidate = true;
                failures.push(format!("read {}: {e}", candidate.display()));
                continue;
            }
        };
        let manifest: UploadManifest = match serde_json::from_str(&raw) {
            Ok(manifest) => manifest,
            Err(e) => {
                failures.push(format!("parse {}: {e}", candidate.display()));
                continue;
            }
        };
        if manifest.upload_id != upload_id {
            return Err(Error::storage("upload manifest id mismatch"));
        }
        return Ok(manifest);
    }
    if !found_candidate {
        Err(Error::not_found(format!("upload {upload_id} not found")))
    } else {
        Err(Error::storage(format!(
            "no valid upload manifest found for {upload_id}: {}",
            failures.join("; ")
        )))
    }
}

fn manifest_recovery_paths(path: &Path) -> [PathBuf; 3] {
    // A valid tmp is the newest state: write_manifest persists it before
    // rotating the primary to backup. Prefer it after a crash, then primary,
    // then the last known-good backup. Invalid candidates fall through.
    [
        path.with_extension("json.tmp"),
        path.to_path_buf(),
        path.with_extension("json.bak"),
    ]
}

fn write_manifest(path: &Path, manifest: &UploadManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::storage(format!("create upload dir {}: {e}", parent.display())))?;
    }
    let bytes = serde_json::to_vec_pretty(manifest)?;
    let tmp = path.with_extension("json.tmp");
    let backup = path.with_extension("json.bak");
    remove_if_exists(&tmp)?;
    std::fs::write(&tmp, bytes)
        .map_err(|e| Error::storage(format!("write upload manifest {}: {e}", tmp.display())))?;

    let had_existing = path.exists();
    if had_existing {
        remove_if_exists(&backup)?;
        if let Err(e) = std::fs::rename(path, &backup) {
            let _ = remove_if_exists(&tmp);
            return Err(Error::storage(format!(
                "replace upload manifest backup {} -> {}: {e}",
                path.display(),
                backup.display()
            )));
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        if had_existing
            && !path.exists()
            && let Err(restore) = std::fs::rename(&backup, path)
        {
            tracing::warn!(
                error = %restore,
                backup = %backup.display(),
                path = %path.display(),
                "failed to restore upload manifest backup after replace failure"
            );
        }
        let _ = remove_if_exists(&tmp);
        return Err(Error::storage(format!(
            "replace upload manifest {} -> {}: {e}",
            tmp.display(),
            path.display()
        )));
    }
    if had_existing {
        if let Err(e) = remove_if_exists(&backup) {
            tracing::warn!(
                error = %e,
                backup = %backup.display(),
                "failed to remove upload manifest backup after successful replace"
            );
        }
    }
    Ok(())
}

fn upload_paths(data_dir: &Path, upload_id: &str, sanitized_filename: &str) -> UploadPaths {
    let dir = data_dir.join("staged-documents");
    let upload_dir = dir.join(upload_id);
    UploadPaths {
        manifest: dir.join(format!("{upload_id}.json")),
        part: upload_dir.join("upload.part"),
        final_path: upload_dir.join(sanitized_filename),
        lock: upload_lock_path(data_dir, upload_id),
        dir: upload_dir,
    }
}

fn upload_lock_path(data_dir: &Path, upload_id: &str) -> PathBuf {
    data_dir
        .join("staged-documents")
        .join(format!("{upload_id}.lock"))
}

/// Sweep expired upload staging for the Community Memory Library. This does
/// not open the database and is safe to run at daemon startup and on a
/// background cadence.
pub fn sweep_expired_uploads(data_dir: &Path) -> Result<usize> {
    let staging_dir = data_dir.join("staged-documents");
    let entries = match std::fs::read_dir(&staging_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => {
            return Err(Error::storage(format!(
                "read upload staging dir {}: {e}",
                staging_dir.display()
            )));
        }
    };

    let mut swept = 0usize;
    let mut seen_upload_ids = HashSet::new();
    let now = now_ms();
    for entry in entries {
        let entry = entry.map_err(|e| Error::storage(format!("read upload staging entry: {e}")))?;
        let path = entry.path();
        let Some(upload_id) = upload_id_from_manifest_entry(&path) else {
            continue;
        };
        if !seen_upload_ids.insert(upload_id.to_string()) {
            continue;
        }
        let Ok(manifest) = load_manifest_for_upload(data_dir, upload_id) else {
            continue;
        };
        if !upload_is_expired_at(&manifest, now) {
            continue;
        }
        let paths = upload_paths(data_dir, upload_id, &manifest.sanitized_filename);
        let Some(_lock) = try_acquire_upload_lock(&paths.lock, "sweep")? else {
            // An append, commit, ingest, or abort owns this upload. Leave it
            // for the next lazy sweep instead of deleting bytes underneath
            // an active operation.
            continue;
        };

        // Re-read under the lock. Another operation may have completed in
        // the small window between the first manifest read and lock acquire;
        // never delete based on that stale snapshot.
        let Ok(locked_manifest) = load_manifest_for_upload(data_dir, upload_id) else {
            continue;
        };
        if !upload_is_expired_at(&locked_manifest, now_ms()) {
            continue;
        }
        let locked_paths = upload_paths(data_dir, upload_id, &locked_manifest.sanitized_filename);
        remove_upload_paths(&locked_paths)?;
        swept += 1;
    }
    Ok(swept)
}

fn remove_upload_paths(paths: &UploadPaths) -> Result<()> {
    remove_if_exists(&paths.part)?;
    remove_if_exists(&paths.final_path)?;
    for manifest_path in manifest_recovery_paths(&paths.manifest) {
        remove_if_exists(&manifest_path)?;
    }
    remove_dir_if_empty(&paths.dir)?;
    Ok(())
}

fn upload_id_from_manifest_entry(path: &Path) -> Option<&str> {
    let filename = path.file_name()?.to_str()?;
    [".json", ".json.tmp", ".json.bak"]
        .into_iter()
        .find_map(|suffix| filename.strip_suffix(suffix))
}

async fn rollback_upload_part(path: &Path, len: u64) {
    match tokio::fs::OpenOptions::new().write(true).open(path).await {
        Ok(file) => {
            if let Err(e) = file.set_len(len).await {
                tracing::warn!(
                    error = %e,
                    path = %path.display(),
                    len,
                    "failed to roll back upload part after manifest write failure"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                len,
                "failed to reopen upload part for rollback after manifest write failure"
            );
        }
    }
}

fn acquire_upload_lock(path: &Path, operation: &'static str) -> Result<UploadLock> {
    try_acquire_upload_lock(path, operation)?.ok_or_else(|| Error::conflict("upload is busy"))
}

fn try_acquire_upload_lock(path: &Path, operation: &'static str) -> Result<Option<UploadLock>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            Error::storage(format!("create upload lock dir {}: {e}", parent.display()))
        })?;
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Do not truncate until after the OS lock is held; truncating here
        // would destroy a live owner's operation metadata.
        .truncate(false)
        .open(path)
        .map_err(|e| Error::storage(format!("open upload lock {}: {e}", path.display())))?;
    let mut active = active_upload_operations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match FileExt::try_lock_exclusive(&file) {
        Ok(()) => {}
        Err(e) if upload_lock_is_contended(&e) => return Ok(None),
        Err(e) => {
            return Err(Error::storage(format!(
                "acquire upload lock {}: {e}",
                path.display()
            )));
        }
    }
    active.insert(path.to_path_buf(), operation.to_string());
    drop(active);

    // An unlocked file can be left behind by a crashed process. The OS lock
    // is released automatically on process exit, so acquiring it is the safe
    // stale-lock reclamation boundary. Keep the file handle alive for the
    // whole operation; lock paths are intentionally never unlinked.
    let owner = serde_json::json!({
        "pid": std::process::id(),
        "acquired_at_ms": now_ms(),
        "operation": operation,
    });
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .and_then(|()| file.write_all(owner.to_string().as_bytes()))
        .and_then(|()| file.sync_all())
        .map_err(|e| {
            active_upload_operations()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(path);
            let _ = FileExt::unlock(&file);
            Error::storage(format!("write upload lock {}: {e}", path.display()))
        })?;
    Ok(Some(UploadLock {
        file: Some(file),
        path: path.to_path_buf(),
    }))
}

fn read_upload_lock_operation(path: &Path) -> String {
    if let Some(operation) = active_upload_operations()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(path)
        .cloned()
    {
        return match operation.as_str() {
            "append" | "commit" | "ingest" | "abort" | "sweep" => operation,
            _ => "unknown".to_string(),
        };
    }
    let operation = std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|owner| owner.get("operation")?.as_str().map(str::to_string));
    match operation.as_deref() {
        Some(value @ ("append" | "commit" | "ingest" | "abort" | "sweep")) => value.to_string(),
        _ => "unknown".to_string(),
    }
}

fn upload_lock_is_contended(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        // Windows reports LockViolation/SharingViolation as raw errors rather
        // than normalizing them to WouldBlock.
        || matches!(error.raw_os_error(), Some(32 | 33))
}

#[cfg(test)]
pub(crate) fn acquire_upload_lock_for_tests(
    data_dir: &Path,
    upload_id: &str,
    operation: &'static str,
) -> Result<impl Drop> {
    validate_upload_id(upload_id)?;
    acquire_upload_lock(&upload_lock_path(data_dir, upload_id), operation)
}

#[cfg(test)]
pub(crate) fn upload_lock_path_for_tests(data_dir: &Path, upload_id: &str) -> Result<PathBuf> {
    validate_upload_id(upload_id)?;
    Ok(upload_lock_path(data_dir, upload_id))
}

fn ensure_open(manifest: &UploadManifest) -> Result<()> {
    if status_from_manifest(manifest).status == UploadStatus::Expired {
        return Err(Error::invalid_input("upload has expired"));
    }
    if manifest.status != UploadStatus::Open {
        return Err(Error::conflict(format!(
            "upload is not open (status: {:?})",
            manifest.status
        )));
    }
    Ok(())
}

fn status_from_manifest(manifest: &UploadManifest) -> UploadStatusResponse {
    let status = if upload_is_expired_at(manifest, now_ms()) {
        UploadStatus::Expired
    } else {
        manifest.status
    };
    let commit_result = if matches!(status, UploadStatus::Committed | UploadStatus::Ingested) {
        commit_response_from_manifest(manifest).ok()
    } else {
        None
    };
    UploadStatusResponse {
        upload_id: manifest.upload_id.clone(),
        status,
        bytes_received: manifest.bytes_received,
        size_bytes: manifest.size_bytes,
        next_offset: manifest.bytes_received,
        expires_at_ms: manifest.expires_at_ms,
        operation_in_progress: false,
        active_operation: None,
        staged_uri: commit_result
            .as_ref()
            .map(|result| result.staged_uri.clone()),
        commit_result,
        ingest_result: manifest.ingest_result.clone(),
        terminal: matches!(
            status,
            UploadStatus::Ingested | UploadStatus::Expired | UploadStatus::Aborted
        ),
    }
}

fn upload_is_expired_at(manifest: &UploadManifest, now: i64) -> bool {
    matches!(
        manifest.status,
        UploadStatus::Open
            | UploadStatus::Committed
            | UploadStatus::Ingested
            | UploadStatus::Aborted
    ) && now > manifest.expires_at_ms
}

fn terminal_receipt_expires_at_ms() -> i64 {
    now_ms().saturating_add(TERMINAL_RECEIPT_TTL_SECS.saturating_mul(1000))
}

fn chunk_config_from_document_config(
    config: &solo_storage::DocumentConfig,
) -> Result<solo_storage::ChunkConfig> {
    if config.chunk_token_target == 0 {
        return Err(Error::invalid_input(
            "documents.chunk_token_target must be > 0",
        ));
    }
    if config.chunk_overlap_tokens >= config.chunk_token_target {
        return Err(Error::invalid_input(format!(
            "documents.chunk_overlap_tokens ({}) must be strictly less than documents.chunk_token_target ({})",
            config.chunk_overlap_tokens, config.chunk_token_target
        )));
    }
    Ok(solo_storage::ChunkConfig {
        target_tokens: config.chunk_token_target,
        overlap_tokens: config.chunk_overlap_tokens,
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::storage(format!("open staged upload {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| Error::storage(format!("read staged upload {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn normalize_sha256(value: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid_input(
            "sha256 must be 64 lowercase or uppercase hex characters",
        ));
    }
    Ok(Some(value))
}

fn parse_staged_uri(uri: &str) -> Result<&str> {
    let upload_id = uri
        .strip_prefix(STAGED_URI_PREFIX)
        .ok_or_else(|| Error::invalid_input("staged_uri must start with solo-staged://upload/"))?;
    validate_upload_id(upload_id)?;
    Ok(upload_id)
}

fn staged_uri(upload_id: &str) -> String {
    format!("{STAGED_URI_PREFIX}{upload_id}")
}

fn validate_upload_id(upload_id: &str) -> Result<()> {
    Uuid::parse_str(upload_id)
        .map(|_| ())
        .map_err(|e| Error::invalid_input(format!("invalid upload_id: {e}")))
}

fn sanitize_filename(filename: &str) -> String {
    let leaf = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(filename);
    let sanitized = sanitize_path_segment(leaf);
    if sanitized.is_empty() {
        "upload".to_string()
    } else {
        sanitized.chars().take(160).collect()
    }
}

fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('.')
        .to_string()
}

fn has_allowed_extension(filename: &str, allowed_extensions: &[String]) -> bool {
    Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            let ext = ext.trim_start_matches('.');
            allowed_extensions
                .iter()
                .any(|allowed| allowed.trim_start_matches('.').eq_ignore_ascii_case(ext))
        })
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::storage(format!("remove {}: {e}", path.display()))),
    }
}

fn remove_dir_if_empty(path: &Path) -> Result<()> {
    match std::fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(e) => Err(Error::storage(format!(
            "remove dir {}: {e}",
            path.display()
        ))),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload_manifest_fixture(
        upload_id: &str,
        filename: &str,
        status: UploadStatus,
        expires_at_ms: i64,
    ) -> UploadManifest {
        UploadManifest {
            upload_id: upload_id.to_string(),
            filename: filename.to_string(),
            sanitized_filename: filename.to_string(),
            mime_type: "text/markdown".to_string(),
            size_bytes: 4,
            expected_sha256: None,
            actual_sha256: (status == UploadStatus::Committed).then(|| "0".repeat(64)),
            bytes_received: 4,
            status,
            created_at_ms: expires_at_ms.saturating_sub(10_000),
            expires_at_ms,
            ingest_result: None,
        }
    }

    fn write_upload_fixture(data_dir: &Path, manifest: &UploadManifest) -> UploadPaths {
        let paths = upload_paths(data_dir, &manifest.upload_id, &manifest.sanitized_filename);
        std::fs::create_dir_all(&paths.dir).expect("upload fixture dir");
        let bytes_path = if manifest.status == UploadStatus::Committed {
            &paths.final_path
        } else {
            &paths.part
        };
        std::fs::write(bytes_path, b"data").expect("upload fixture bytes");
        write_manifest(&paths.manifest, manifest).expect("upload fixture manifest");
        paths
    }

    #[test]
    fn upload_cap_matches_default_ingest_guardrail() {
        assert_eq!(MAX_UPLOAD_BYTES, solo_storage::DEFAULT_INGEST_MAX_BYTES);
    }

    #[test]
    fn staged_ingest_request_distinguishes_omitted_from_explicit_false() {
        let omitted: StagedIngestRequest =
            serde_json::from_value(serde_json::json!({ "staged_uri": "solo-staged://upload/a" }))
                .expect("omitted request");
        assert_eq!(omitted.store_original_file, None);

        let explicit_false: StagedIngestRequest = serde_json::from_value(serde_json::json!({
            "staged_uri": "solo-staged://upload/a",
            "store_original_file": false
        }))
        .expect("explicit false request");
        assert_eq!(explicit_false.store_original_file, Some(false));
    }

    #[test]
    fn prepare_upload_accepts_asset_only_extension_when_originals_default_on() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let response = prepare_upload(
            tmp.path(),
            UploadPrepareRequest {
                filename: "artifact.bin".to_string(),
                mime_type: Some("application/octet-stream".to_string()),
                size_bytes: 12,
                sha256: None,
            },
            &["md".to_string()],
            true,
        )
        .expect("asset-only upload should be accepted when originals are retained by default");

        assert_eq!(response.upload_method, "PATCH");
        assert!(response.default_store_original_file);
    }

    #[test]
    fn prepare_upload_accepts_registered_document_extensions_from_default_policy() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let allowed_extensions = solo_storage::DocumentConfig::default().allowed_extensions;
        for (filename, mime_type) in [
            (
                "workbook.xlsx",
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ),
            (
                "brief.docx",
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ),
            (
                "deck.pptx",
                "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ),
            ("diagram.png", "image/png"),
            ("photo.jpg", "image/jpeg"),
            ("photo.jpeg", "image/jpeg"),
            ("graphic.webp", "image/webp"),
            ("scan.tif", "image/tiff"),
            ("scan.tiff", "image/tiff"),
            ("scene.blend", "application/x-blender"),
            ("archive.zip", "application/zip"),
            ("scene.gltf", "model/gltf+json"),
            ("scene.glb", "model/gltf-binary"),
            ("mesh.obj", "model/obj"),
            ("mesh.stl", "model/stl"),
        ] {
            let response = prepare_upload(
                tmp.path(),
                UploadPrepareRequest {
                    filename: filename.to_string(),
                    mime_type: Some(mime_type.to_string()),
                    size_bytes: 12,
                    sha256: None,
                },
                &allowed_extensions,
                false,
            )
            .unwrap_or_else(|err| {
                panic!("{filename} upload should be accepted by the default document policy: {err}")
            });

            assert_eq!(response.upload_method, "PATCH");
            assert!(!response.default_store_original_file);
        }
    }

    #[test]
    fn document_mime_type_for_path_uses_parser_registry() {
        assert_eq!(
            document_mime_type_for_path(Path::new("sheet.xlsx")).as_deref(),
            Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("table.csv")).as_deref(),
            Some("text/csv")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("table.tsv")).as_deref(),
            Some("text/tab-separated-values")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("brief.docx")).as_deref(),
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("deck.pptx")).as_deref(),
            Some("application/vnd.openxmlformats-officedocument.presentationml.presentation")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("diagram.png")).as_deref(),
            Some("image/png")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("photo.jpg")).as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("photo.jpeg")).as_deref(),
            Some("image/jpeg")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("graphic.webp")).as_deref(),
            Some("image/webp")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("scan.tif")).as_deref(),
            Some("image/tiff")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("scan.tiff")).as_deref(),
            Some("image/tiff")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("scene.blend")).as_deref(),
            Some("application/x-blender")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("archive.zip")).as_deref(),
            Some("application/zip")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("scene.gltf")).as_deref(),
            Some("model/gltf+json")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("scene.glb")).as_deref(),
            Some("model/gltf-binary")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("mesh.obj")).as_deref(),
            Some("model/obj")
        );
        assert_eq!(
            document_mime_type_for_path(Path::new("mesh.stl")).as_deref(),
            Some("model/stl")
        );
    }

    #[test]
    fn prepare_upload_rejects_asset_only_extension_when_originals_default_off() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let err = prepare_upload(
            tmp.path(),
            UploadPrepareRequest {
                filename: "artifact.bin".to_string(),
                mime_type: Some("application/octet-stream".to_string()),
                size_bytes: 12,
                sha256: None,
            },
            &["md".to_string()],
            false,
        )
        .expect_err("asset-only upload should still respect explicit no-retention default");

        assert!(
            err.to_string()
                .contains("filename extension is not allowed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn write_manifest_replaces_existing_manifest() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let path = tmp.path().join("upload.json");
        let mut manifest = UploadManifest {
            upload_id: Uuid::now_v7().to_string(),
            filename: "notes.md".to_string(),
            sanitized_filename: "notes.md".to_string(),
            mime_type: "text/markdown".to_string(),
            size_bytes: 10,
            expected_sha256: None,
            actual_sha256: None,
            bytes_received: 0,
            status: UploadStatus::Open,
            created_at_ms: 1,
            expires_at_ms: 2,
            ingest_result: None,
        };
        write_manifest(&path, &manifest).expect("first write");
        manifest.bytes_received = 10;
        write_manifest(&path, &manifest).expect("replace write");

        let raw = std::fs::read_to_string(&path).expect("read manifest");
        let loaded: UploadManifest = serde_json::from_str(&raw).expect("manifest json");
        assert_eq!(loaded.bytes_received, 10);
        assert!(!path.with_extension("json.bak").exists());
    }

    #[test]
    fn manifest_recovery_prefers_newer_valid_tmp_over_primary() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let upload_id = Uuid::now_v7().to_string();
        let mut primary = upload_manifest_fixture(
            &upload_id,
            "recover.md",
            UploadStatus::Open,
            now_ms() + 60_000,
        );
        primary.bytes_received = 0;
        let paths = write_upload_fixture(tmp.path(), &primary);
        let mut newer = primary.clone();
        newer.bytes_received = newer.size_bytes;
        newer.status = UploadStatus::Committed;
        newer.actual_sha256 = Some("0".repeat(64));
        std::fs::write(
            paths.manifest.with_extension("json.tmp"),
            serde_json::to_vec(&newer).expect("newer manifest json"),
        )
        .expect("newer recovery tmp");

        let recovered =
            load_manifest_for_upload(tmp.path(), &upload_id).expect("recover newer tmp manifest");
        assert_eq!(recovered.status, UploadStatus::Committed);
        assert_eq!(recovered.bytes_received, recovered.size_bytes);
    }

    #[test]
    fn sweep_expired_uploads_removes_open_and_committed_staging() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let open_id = Uuid::now_v7().to_string();
        let committed_id = Uuid::now_v7().to_string();
        let current_id = Uuid::now_v7().to_string();
        let now = now_ms();

        let open = upload_manifest_fixture(&open_id, "open.md", UploadStatus::Open, now - 1_000);
        let committed = upload_manifest_fixture(
            &committed_id,
            "committed.md",
            UploadStatus::Committed,
            now - 1_000,
        );
        let current = upload_manifest_fixture(
            &current_id,
            "current.md",
            UploadStatus::Committed,
            now + 60_000,
        );
        let open_paths = write_upload_fixture(tmp.path(), &open);
        let committed_paths = write_upload_fixture(tmp.path(), &committed);
        let current_paths = write_upload_fixture(tmp.path(), &current);

        assert_eq!(
            status_from_manifest(&committed).status,
            UploadStatus::Expired
        );
        assert_eq!(sweep_expired_uploads(tmp.path()).unwrap(), 2);
        assert!(!open_paths.manifest.exists());
        assert!(!open_paths.part.exists());
        assert!(!committed_paths.manifest.exists());
        assert!(!committed_paths.final_path.exists());
        assert!(current_paths.manifest.exists());
        assert!(current_paths.final_path.exists());
    }

    #[test]
    fn sweep_recovers_expired_upload_from_backup_when_primary_missing_and_tmp_corrupt() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let upload_id = Uuid::now_v7().to_string();
        let upload = upload_manifest_fixture(
            &upload_id,
            "crash-recovery.md",
            UploadStatus::Committed,
            now_ms() - 1_000,
        );
        let paths = write_upload_fixture(tmp.path(), &upload);
        let recovery_tmp = paths.manifest.with_extension("json.tmp");
        let recovery_backup = paths.manifest.with_extension("json.bak");

        // Simulate a process dying after rotating the primary and while a
        // replacement tmp is only partially written.
        std::fs::rename(&paths.manifest, &recovery_backup).expect("rotate primary to backup");
        std::fs::write(&recovery_tmp, b"{\"upload_id\":").expect("partial recovery tmp");

        assert_eq!(sweep_expired_uploads(tmp.path()).unwrap(), 1);
        assert!(!paths.manifest.exists());
        assert!(!recovery_tmp.exists());
        assert!(!recovery_backup.exists());
        assert!(!paths.final_path.exists());
    }

    #[test]
    fn prepare_upload_sweeps_expired_committed_staging_after_restart() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let stale_id = Uuid::now_v7().to_string();
        let stale = upload_manifest_fixture(
            &stale_id,
            "stale.md",
            UploadStatus::Committed,
            now_ms() - 1_000,
        );
        let stale_paths = write_upload_fixture(tmp.path(), &stale);

        // Prepare keeps a best-effort library sweep in addition to the
        // daemon's independent startup/background sweeper.
        let fresh = prepare_upload(
            tmp.path(),
            UploadPrepareRequest {
                filename: "fresh.md".to_string(),
                mime_type: Some("text/markdown".to_string()),
                size_bytes: 4,
                sha256: None,
            },
            &["md".to_string()],
            false,
        )
        .expect("prepare after restart");

        assert!(!stale_paths.manifest.exists());
        assert!(!stale_paths.final_path.exists());
        assert!(upload_status(tmp.path(), &fresh.upload_id).is_ok());
    }

    #[test]
    fn sweep_expired_uploads_skips_active_upload_lock() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let upload_id = Uuid::now_v7().to_string();
        let upload = upload_manifest_fixture(
            &upload_id,
            "active.md",
            UploadStatus::Committed,
            now_ms() - 1_000,
        );
        let paths = write_upload_fixture(tmp.path(), &upload);
        let lock = acquire_upload_lock(&paths.lock, "test").expect("active upload lock");

        assert_eq!(sweep_expired_uploads(tmp.path()).unwrap(), 0);
        assert!(paths.manifest.exists());
        assert!(paths.final_path.exists());

        drop(lock);
        assert_eq!(sweep_expired_uploads(tmp.path()).unwrap(), 1);
        assert!(!paths.manifest.exists());
        assert!(!paths.final_path.exists());
    }

    #[test]
    fn crash_stale_lock_file_is_reclaimed_without_unlinking() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let upload_id = Uuid::now_v7().to_string();
        let lock_path = upload_lock_path(tmp.path(), &upload_id);
        std::fs::create_dir_all(lock_path.parent().expect("lock parent")).unwrap();
        std::fs::write(&lock_path, b"crashed-owner").unwrap();

        let lock = acquire_upload_lock(&lock_path, "test").expect("reclaim crash-stale lock");
        assert!(lock_path.exists(), "live guard must retain the lock path");
        assert!(
            try_acquire_upload_lock(&lock_path, "test")
                .unwrap()
                .is_none(),
            "a second operation must observe the live OS lock"
        );
        drop(lock);

        assert!(
            lock_path.exists(),
            "lock paths are intentionally persistent"
        );
        acquire_upload_lock(&lock_path, "test").expect("released lock must be reusable");
    }

    #[tokio::test]
    async fn append_commit_abort_and_sweep_serialize_on_one_upload_lock() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let upload_id = Uuid::now_v7().to_string();
        let upload = upload_manifest_fixture(
            &upload_id,
            "barrier.md",
            UploadStatus::Open,
            now_ms() - 1_000,
        );
        let paths = write_upload_fixture(tmp.path(), &upload);
        let lock = acquire_upload_lock(&paths.lock, "commit").expect("barrier lock");

        let busy = upload_status(tmp.path(), &upload_id).expect("busy status");
        assert_eq!(busy.status, UploadStatus::Busy);
        assert!(busy.operation_in_progress);
        assert_eq!(busy.active_operation.as_deref(), Some("commit"));

        let append = append_upload_chunk(tmp.path(), &upload_id, 4, None, b"x")
            .await
            .expect_err("append must not cross active lifecycle lock");
        assert!(append.to_string().contains("upload is busy"));

        let commit = commit_upload(tmp.path(), &upload_id, UploadCommitRequest { sha256: None })
            .await
            .expect_err("commit must not cross active lifecycle lock");
        assert!(commit.to_string().contains("upload is busy"));

        let abort = abort_upload(tmp.path(), &upload_id)
            .expect_err("abort must not cross active lifecycle lock");
        assert!(abort.to_string().contains("upload is busy"));

        assert_eq!(
            sweep_expired_uploads(tmp.path()).unwrap(),
            0,
            "sweep must skip an active lifecycle lock"
        );
        assert!(paths.manifest.exists());
        assert!(paths.part.exists());
        assert!(paths.lock.exists());

        drop(lock);
        let aborted = abort_upload(tmp.path(), &upload_id).expect("abort after barrier release");
        assert_eq!(aborted.status, UploadStatus::Aborted);
        assert!(aborted.cleanup_performed);
        assert!(aborted.removed_partial_file);
        assert!(!aborted.removed_staged_file);
        assert!(!aborted.already_aborted);
        assert!(paths.manifest.exists(), "abort receipt must remain durable");
        assert!(!paths.part.exists());
        let status = upload_status(tmp.path(), &upload_id).expect("aborted status");
        assert_eq!(status.status, UploadStatus::Aborted);
        assert!(status.terminal);

        let replayed = abort_upload(tmp.path(), &upload_id)
            .expect("repeated abort returns its terminal receipt");
        assert!(replayed.already_aborted);
        assert!(!replayed.cleanup_performed);
        assert!(
            paths.lock.exists(),
            "cleanup must never unlink the reusable lock path"
        );
    }
}
