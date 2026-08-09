// SPDX-License-Identifier: Apache-2.0

//! Shared types used across the workspace. See ADR-0002 for design notes.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Tenant ID
// ---------------------------------------------------------------------------

/// Stable identifier for a tenant.
///
/// Validated to be lowercase alphanumeric + `-`/`_`, max 64 chars, non-empty.
/// The reserved id `"default"` is the auto-created tenant on a fresh
/// `solo init`. See `docs/dev-log/0090-v0.8.0-implementation-plan.md` §2 P1.
///
/// The validation rules are intentionally narrow: the id appears in
/// `<data_dir>/tenants/<id>.db` filenames, and we want zero ambiguity
/// across filesystems (case-insensitive HFS+/NTFS, case-sensitive ext4),
/// across URL escaping, and across shell quoting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LibraryId(String);

/// Maximum length of a `LibraryId`, in bytes. 64 is large enough for any
/// reasonable human-readable scope name (`acme-corp-prod`, `org_12345`)
/// while keeping filenames + log lines compact.
pub const TENANT_ID_MAX_LEN: usize = 64;

/// Reserved id auto-created on `solo init`. Used as the implicit fallback
/// as an internal compatibility id for the one Community Memory Library
/// (v0.7.1 single-tenant compatibility).
pub const DEFAULT_TENANT_ID: &str = "default";

impl LibraryId {
    /// Construct + validate.
    ///
    /// Validation rules:
    ///   * non-empty
    ///   * length ≤ 64 bytes
    ///   * each char is lowercase a-z, 0-9, `-`, or `_`
    ///
    /// Uppercase letters, spaces, paths, dots, slashes, and any other
    /// punctuation are rejected.
    pub fn new(id: impl Into<String>) -> std::result::Result<Self, TenantIdError> {
        let s: String = id.into();
        if s.is_empty() {
            return Err(TenantIdError::Empty);
        }
        if s.len() > TENANT_ID_MAX_LEN {
            return Err(TenantIdError::TooLong { len: s.len() });
        }
        for ch in s.chars() {
            let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_';
            if !ok {
                return Err(TenantIdError::InvalidChar { ch });
            }
        }
        Ok(Self(s))
    }

    /// The reserved `"default"` tenant.
    pub fn default_tenant() -> Self {
        // Construction is infallible — `"default"` matches the rules.
        Self(DEFAULT_TENANT_ID.to_string())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume into the underlying String.
    pub fn into_inner(self) -> String {
        self.0
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TenantIdError {
    #[error("tenant id cannot be empty")]
    Empty,
    #[error("tenant id too long ({len} bytes, max {})", TENANT_ID_MAX_LEN)]
    TooLong { len: usize },
    #[error("tenant id has invalid char {ch:?} (allowed: lowercase a-z, 0-9, '-', '_')")]
    InvalidChar { ch: char },
}

impl std::fmt::Display for LibraryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::str::FromStr for LibraryId {
    type Err = TenantIdError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::new(s.to_string())
    }
}

impl TryFrom<String> for LibraryId {
    type Error = TenantIdError;
    fn try_from(s: String) -> std::result::Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<LibraryId> for String {
    fn from(t: LibraryId) -> Self {
        t.0
    }
}

// ---------------------------------------------------------------------------
// Memory ID
// ---------------------------------------------------------------------------

/// A globally unique memory identifier.
///
/// Stored as UUID v7 (time-ordered) so lexicographic sorting matches
/// chronological order — useful for keyset pagination and cache locality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MemoryId(pub Uuid);

impl MemoryId {
    /// Create a new time-ordered MemoryId.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for MemoryId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(s).map(MemoryId)
    }
}

// ---------------------------------------------------------------------------
// Embedding
// ---------------------------------------------------------------------------

/// The dtype of an embedding's components. See `solo-v0-architecture.md §3.2`
/// for the tiering policy (FP32/FP16 hot, INT8 warm, RaBitQ binary cold).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EmbeddingDtype {
    F32,
    F16,
    I8,
    /// 1-bit-per-dim packed binary (RaBitQ cold tier).
    Binary,
}

impl EmbeddingDtype {
    /// Bytes per element. For Binary, returns 0 — use `bytes_for_dim` instead.
    pub fn bytes_per_element(&self) -> usize {
        match self {
            Self::F32 => 4,
            Self::F16 => 2,
            Self::I8 => 1,
            Self::Binary => 0,
        }
    }

    /// Total bytes required to store `dim` elements at this dtype.
    pub fn bytes_for_dim(&self, dim: usize) -> usize {
        match self {
            Self::Binary => dim.div_ceil(8),
            other => other.bytes_per_element() * dim,
        }
    }
}

/// A vector embedding. Carries its own dtype + dim so the storage layer
/// doesn't need out-of-band metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Embedding {
    pub dtype: EmbeddingDtype,
    pub dim: usize,
    /// Raw bytes. Length must equal `dtype.bytes_for_dim(dim)`.
    pub data: Vec<u8>,
}

impl Embedding {
    /// Validate the (dtype, dim, data) length invariant.
    pub fn validate(&self) -> Result<()> {
        let expected = self.dtype.bytes_for_dim(self.dim);
        if self.data.len() != expected {
            return Err(Error::EmbedderProtocol(
                "embedding length does not match dtype * dim",
            ));
        }
        Ok(())
    }

    /// Reinterpret the raw data as `&[f32]` when dtype is F32. Returns None
    /// otherwise.
    pub fn as_f32_slice(&self) -> Option<&[f32]> {
        if self.dtype != EmbeddingDtype::F32 {
            return None;
        }
        bytemuck::try_cast_slice(&self.data).ok()
    }
}

// ---------------------------------------------------------------------------
// Provenance / confidence / tier
// ---------------------------------------------------------------------------

/// Confidence in [0.0, 1.0]. Newtype to prevent silent mixing with other floats.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Confidence(pub f32);

impl Confidence {
    pub fn new(v: f32) -> Result<Self> {
        if (0.0..=1.0).contains(&v) {
            Ok(Self(v))
        } else {
            Err(Error::InvalidInput(format!("confidence out of range: {v}")))
        }
    }
}

/// Storage tier per `solo-v0-architecture.md §3.2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Hot,
    Warm,
    Cold,
}

/// Provenance chain for a derived memory: which sources it was derived from
/// and what derivation produced it.
///
/// Per `human-brain-memory.md §6.13`, every derived memory MUST carry an
/// explicit provenance — without it, a reconstructive retrieval system
/// confabulates confidently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    /// IDs of source memories that this memory was derived from.
    pub derived_from: Vec<MemoryId>,
    /// "summary" | "inference" | "extraction" | "consolidation" | "user_edit"
    pub derivation: String,
    /// Identifier of the agent that produced the derivation. May be a steward
    /// LLM name (e.g., "qwen3-coder-30b-local"), a tool, or "user".
    pub by: String,
    /// Epoch ms when the derivation happened.
    pub at_ms: i64,
}

/// Encoding context per `human-brain-memory.md §6.9` (Tulving's encoding
/// specificity). Stored alongside the memory to enable encoding-context
/// re-ranking at recall time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EncodingContext {
    pub session_id: Option<String>,
    pub task: Option<String>,
    pub recent_summary: Option<String>,
    pub affect: Option<String>,
    /// Free-form additional context fields.
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Memory primitives
// ---------------------------------------------------------------------------

/// An episodic memory — a time-keyed event with full encoding context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub memory_id: MemoryId,
    pub ts_ms: i64,
    pub source_type: String, // user_message | tool_output | observation | ...
    pub source_id: Option<String>,
    pub content: String,
    pub encoding_context: EncodingContext,
    pub provenance: Option<Provenance>,
    pub confidence: Confidence,
    pub strength: f32,
    pub salience: f32,
    pub tier: Tier,
}

/// A semantic-memory triple (subject, predicate, object) with bi-temporal
/// validity windows. See `solo-v0-architecture.md §3.1`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Triple {
    pub triple_id: MemoryId,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: TripleObjectKind,
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,
    pub confidence: Confidence,
    pub provenance: Provenance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TripleObjectKind {
    Entity,
    Literal,
}

// ---------------------------------------------------------------------------
// Documents / chunks (v0.7.0 — RAG memory)
// ---------------------------------------------------------------------------

/// A globally unique document identifier.
///
/// Stored as UUID v7 (time-ordered), same convention as `MemoryId`. See
/// `docs/dev-log/0083-v0.7.0-implementation-plan.md` §2 P1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DocumentId(pub Uuid);

impl DocumentId {
    /// Create a new time-ordered DocumentId.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for DocumentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for DocumentId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(s).map(DocumentId)
    }
}

/// A globally unique chunk identifier.
///
/// Stored as UUID v7 (time-ordered). One chunk belongs to exactly one
/// document; multiple chunks per document distinguished by `chunk_index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChunkId(pub Uuid);

impl ChunkId {
    /// Create a new time-ordered ChunkId.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for ChunkId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ChunkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ChunkId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(s).map(ChunkId)
    }
}

/// A persisted original file/blob identifier.
///
/// Assets store raw bytes (for example an uploaded source file) separately
/// from normalized document text/chunks. Stored as UUID v7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssetId(pub Uuid);

impl AssetId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for AssetId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for AssetId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(s).map(AssetId)
    }
}

/// A link between a memory and a document or asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AttachmentId(pub Uuid);

impl AttachmentId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for AttachmentId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for AttachmentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for AttachmentId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Uuid::parse_str(s).map(AttachmentId)
    }
}

/// Soft-delete status for an ingested document. Mirrors the `status` column
/// on `documents` and the active/forgotten dichotomy used by episodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocumentStatus {
    Active,
    Forgotten,
}

/// An ingested document — metadata only. Chunks are stored separately in
/// `DocumentChunk` rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub doc_id: DocumentId,
    pub source: Option<String>,
    pub title: Option<String>,
    pub mime_type: Option<String>,
    pub ingested_at_ms: i64,
    pub modified_at_ms: Option<i64>,
    pub status: DocumentStatus,
    pub chunk_count: u32,
    pub content_hash: Option<String>,
    pub byte_size: Option<u64>,
}

/// A single chunk of a document. Chunks are the unit of embedding + search.
///
/// `start_offset` / `end_offset` are byte offsets into the source document's
/// normalized text — useful for reconstructing surrounding context at recall
/// time without re-parsing the original file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentChunk {
    pub chunk_id: ChunkId,
    pub doc_id: DocumentId,
    pub chunk_index: u32,
    pub content: String,
    pub token_count: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub created_at_ms: i64,
}

// ---------------------------------------------------------------------------
// Steward outputs (consumed by solo-steward in week 2-3)
// ---------------------------------------------------------------------------

/// A cluster of episodes the steward considers semantically related.
/// Output of the SWS-equivalent dedup pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub cluster_id: MemoryId,
    pub episode_ids: Vec<MemoryId>,
    pub centroid: Option<Embedding>,
    /// Average pairwise cosine similarity within the cluster.
    pub coherence: f32,
}

/// A semantic abstraction over a cluster, generated by the steward LLM
/// during the REM-equivalent integration pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticAbstraction {
    pub abstraction_id: MemoryId,
    pub cluster_id: MemoryId,
    pub content: String,
    pub triples: Vec<Triple>,
    pub provenance: Provenance,
    pub confidence: Confidence,
}

/// A detected contradiction between two triples — typically a (s,p,o) pair
/// with overlapping validity windows where the predicate is single-valued.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contradiction {
    pub a: MemoryId,
    pub b: MemoryId,
    pub kind: ContradictionKind,
    pub explanation: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContradictionKind {
    OverlappingSingleValuedPredicate,
    DirectNegation,
    NumericInconsistency,
    Other,
}

// ---------------------------------------------------------------------------
// Invalidate events (v0.10.0 — solo-web live updates)
// ---------------------------------------------------------------------------

/// One graph-data-changed signal emitted by the writer-actor AFTER a
/// successful commit and fanned out to every `GET /v1/graph/stream`
/// subscriber for the same tenant.
///
/// Per `docs/dev-log/0105-solo-web-scoping.md` §3 Decision C, the wire
/// format carries an INVALIDATION (a "your tenant's data changed;
/// refetch the affected page") rather than the row payload itself.
/// This keeps the SSE channel privacy-conscious (no user content
/// crosses the wire), keeps the wire shape stable, and avoids leaking
/// the writer-actor's per-row schema into the public API.
///
/// **Invariant** (lesson #30): an `InvalidateEvent` is sent ONLY after
/// the writer-actor's commit succeeds. Rolled-back writes MUST NOT
/// produce an event. The `solo-storage` writer-actor pairs the
/// broadcast `send` with the same `tx.commit().is_ok()` branch that
/// emits the audit row.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InvalidateEvent {
    /// The writer-actor mutation that produced this invalidation, as
    /// the canonical `AuditOperation::as_str()` form (`memory.remember`,
    /// `memory.forget`, `memory.consolidate`, etc.). Free-form string
    /// here (no enum) so adding a new mutation kind in the storage
    /// crate doesn't force a `solo-core` rebuild on every downstream.
    pub reason: String,
    /// The tenant whose data changed. Always == the subscribing
    /// tenant for events that survive the per-tenant filter; included
    /// in the wire format as belt-and-suspenders defense (a buggy
    /// filter would surface as the wrong tenant id on the wire).
    pub tenant_id: String,
    /// Wall-clock millis when the commit landed. Used by clients for
    /// "ignore stale events" logic on reconnect.
    pub ts_ms: i64,
    /// Which node kind in the solo-web graph this affects:
    /// `episode` / `document` / `chunk` / `cluster` / `triple` /
    /// `tenant` (the last for GDPR cascades). Drives the client's
    /// per-page refetch dispatch.
    pub kind: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_dtype_byte_sizes() {
        assert_eq!(EmbeddingDtype::F32.bytes_for_dim(1024), 4096);
        assert_eq!(EmbeddingDtype::F16.bytes_for_dim(1024), 2048);
        assert_eq!(EmbeddingDtype::I8.bytes_for_dim(1024), 1024);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(1024), 128);
        // Off-by-one cases on Binary
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(7), 1);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(8), 1);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(9), 2);
    }

    #[test]
    fn embedding_validate_ok() {
        let e = Embedding {
            dtype: EmbeddingDtype::F32,
            dim: 4,
            data: vec![0u8; 16],
        };
        assert!(e.validate().is_ok());
    }

    #[test]
    fn embedding_validate_length_mismatch() {
        let e = Embedding {
            dtype: EmbeddingDtype::F32,
            dim: 4,
            data: vec![0u8; 12], // should be 16
        };
        assert!(e.validate().is_err());
    }

    #[test]
    fn confidence_bounds() {
        assert!(Confidence::new(0.0).is_ok());
        assert!(Confidence::new(0.5).is_ok());
        assert!(Confidence::new(1.0).is_ok());
        assert!(Confidence::new(-0.1).is_err());
        assert!(Confidence::new(1.1).is_err());
        assert!(Confidence::new(f32::NAN).is_err());
    }

    #[test]
    fn memory_id_is_unique_and_ordered() {
        let a = MemoryId::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MemoryId::new();
        assert_ne!(a, b);
        // UUID v7 is time-ordered; later IDs sort greater.
        assert!(a.0 < b.0);
    }

    #[test]
    fn memory_id_from_str_roundtrips_canonical() {
        use std::str::FromStr;
        let mid = MemoryId::new();
        let s = mid.to_string();
        let parsed = MemoryId::from_str(&s).unwrap();
        assert_eq!(parsed, mid);
    }

    #[test]
    fn memory_id_from_str_rejects_bogus() {
        use std::str::FromStr;
        // Empty
        assert!(MemoryId::from_str("").is_err());
        // Wrong length
        assert!(MemoryId::from_str("not-a-uuid").is_err());
        // Right length but non-hex chars
        assert!(MemoryId::from_str("ZZZZZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZZZZZZZZZ").is_err());
        // Whitespace doesn't get trimmed implicitly
        assert!(MemoryId::from_str(" 019dfd11-45b3-71c2-b067-96266bd387e9 ").is_err());
    }

    #[test]
    fn embedding_validate_with_zero_dim_and_zero_data_is_ok() {
        let e = Embedding {
            dtype: EmbeddingDtype::F32,
            dim: 0,
            data: vec![],
        };
        // Degenerate but technically consistent.
        assert!(e.validate().is_ok());
        // bytemuck::try_cast_slice rejects unaligned empty Vec<u8> backings
        // because the dangling pointer is u8-aligned (1), not f32-aligned (4).
        // Returning None here is fine; downstream code that legitimately
        // needs to handle a zero-dim embedding should check `dim == 0` first.
        let _ = e.as_f32_slice();
    }

    #[test]
    fn embedding_dtype_bytes_for_dim_handles_binary_packing() {
        // Binary dtype packs 1 bit per element. Verify the dim/8 ceil arithmetic.
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(0), 0);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(1), 1);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(7), 1);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(8), 1);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(9), 2);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(15), 2);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(16), 2);
        assert_eq!(EmbeddingDtype::Binary.bytes_for_dim(17), 3);
    }

    #[test]
    fn confidence_rejects_infinity() {
        assert!(Confidence::new(f32::INFINITY).is_err());
        assert!(Confidence::new(f32::NEG_INFINITY).is_err());
    }

    #[test]
    fn as_f32_slice_returns_none_for_non_f32_dtype() {
        for dtype in [
            EmbeddingDtype::F16,
            EmbeddingDtype::I8,
            EmbeddingDtype::Binary,
        ] {
            let dim = if dtype == EmbeddingDtype::Binary {
                8
            } else {
                4
            };
            let e = Embedding {
                dtype,
                dim,
                data: vec![0u8; dtype.bytes_for_dim(dim)],
            };
            assert!(
                e.as_f32_slice().is_none(),
                "dtype {dtype:?} must not cast to f32"
            );
        }
    }

    // -------- Document / chunk types (v0.7.0 P1) --------

    #[test]
    fn document_id_new_is_uuid_v7() {
        let id = DocumentId::new();
        assert_eq!(id.0.get_version_num(), 7, "DocumentId must be UUID v7");
    }

    #[test]
    fn chunk_id_new_is_uuid_v7() {
        let id = ChunkId::new();
        assert_eq!(id.0.get_version_num(), 7, "ChunkId must be UUID v7");
    }

    #[test]
    fn document_id_from_str_roundtrips() {
        use std::str::FromStr;
        let id = DocumentId::new();
        let parsed = DocumentId::from_str(&id.to_string()).unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn document_status_serde_roundtrip() {
        for status in [DocumentStatus::Active, DocumentStatus::Forgotten] {
            let s = serde_json::to_string(&status).unwrap();
            let back: DocumentStatus = serde_json::from_str(&s).unwrap();
            assert_eq!(status, back);
        }
        // Verify the lowercase serialization shape (matches the SQL CHECK domain).
        assert_eq!(
            serde_json::to_string(&DocumentStatus::Active).unwrap(),
            "\"active\""
        );
        assert_eq!(
            serde_json::to_string(&DocumentStatus::Forgotten).unwrap(),
            "\"forgotten\""
        );
    }

    #[test]
    fn document_struct_serde_roundtrip() {
        let doc = Document {
            doc_id: DocumentId::new(),
            source: Some("/notes/intro.md".into()),
            title: Some("Intro".into()),
            mime_type: Some("text/markdown".into()),
            ingested_at_ms: 1_700_000_000_000,
            modified_at_ms: Some(1_699_999_999_000),
            status: DocumentStatus::Active,
            chunk_count: 4,
            content_hash: Some("a".repeat(64)),
            byte_size: Some(1234),
        };
        let s = serde_json::to_string(&doc).unwrap();
        let back: Document = serde_json::from_str(&s).unwrap();
        assert_eq!(back.doc_id, doc.doc_id);
        assert_eq!(back.source, doc.source);
        assert_eq!(back.title, doc.title);
        assert_eq!(back.status, doc.status);
        assert_eq!(back.chunk_count, doc.chunk_count);
        assert_eq!(back.content_hash, doc.content_hash);
        assert_eq!(back.byte_size, doc.byte_size);
    }

    #[test]
    fn document_chunk_serde_roundtrip() {
        let chunk = DocumentChunk {
            chunk_id: ChunkId::new(),
            doc_id: DocumentId::new(),
            chunk_index: 2,
            content: "hello world".into(),
            token_count: 2,
            start_offset: 0,
            end_offset: 11,
            created_at_ms: 1_700_000_000_000,
        };
        let s = serde_json::to_string(&chunk).unwrap();
        let back: DocumentChunk = serde_json::from_str(&s).unwrap();
        assert_eq!(back.chunk_id, chunk.chunk_id);
        assert_eq!(back.doc_id, chunk.doc_id);
        assert_eq!(back.chunk_index, chunk.chunk_index);
        assert_eq!(back.content, chunk.content);
        assert_eq!(back.token_count, chunk.token_count);
        assert_eq!(back.start_offset, chunk.start_offset);
        assert_eq!(back.end_offset, chunk.end_offset);
        assert_eq!(back.created_at_ms, chunk.created_at_ms);
    }

    #[test]
    fn document_id_default_matches_new_shape() {
        // `Default::default()` should produce a valid UUID v7 (not nil).
        let id = DocumentId::default();
        assert_ne!(id.0, Uuid::nil());
        assert_eq!(id.0.get_version_num(), 7);
    }

    #[test]
    fn chunk_id_default_matches_new_shape() {
        let id = ChunkId::default();
        assert_ne!(id.0, Uuid::nil());
        assert_eq!(id.0.get_version_num(), 7);
    }

    // -------- LibraryId (v0.8.0 P1) --------

    #[test]
    fn tenant_id_accepts_default() {
        let t = LibraryId::new("default").unwrap();
        assert_eq!(t.as_str(), "default");
        assert_eq!(LibraryId::default_tenant(), t);
    }

    #[test]
    fn tenant_id_accepts_lowercase_alphanumeric() {
        for s in ["a", "z", "0", "9", "tenant01", "abc123xyz"] {
            assert!(LibraryId::new(s).is_ok(), "rejected legal id: {s}");
        }
    }

    #[test]
    fn tenant_id_accepts_dashes_and_underscores() {
        for s in [
            "tenant-01",
            "acme_co",
            "a-b-c",
            "_underscore",
            "-dash",
            "trailing-",
            "trailing_",
            "x-y_z-1_2",
        ] {
            assert!(LibraryId::new(s).is_ok(), "rejected legal id: {s}");
        }
    }

    #[test]
    fn tenant_id_rejects_uppercase() {
        for s in ["Default", "ACME", "Tenant1", "aBc"] {
            let err = LibraryId::new(s).unwrap_err();
            assert!(
                matches!(err, TenantIdError::InvalidChar { .. }),
                "expected InvalidChar for {s}, got {err:?}"
            );
        }
    }

    #[test]
    fn tenant_id_rejects_spaces() {
        for s in ["tenant 1", " leading", "trailing ", "a b c"] {
            let err = LibraryId::new(s).unwrap_err();
            assert!(
                matches!(err, TenantIdError::InvalidChar { ch: ' ' }),
                "expected InvalidChar for {s}, got {err:?}"
            );
        }
    }

    #[test]
    fn tenant_id_rejects_paths_with_slash() {
        for s in [
            "../escape",
            "tenant/sub",
            "a\\b",
            ".hidden",
            "a.b",
            "a:b",
            "a;b",
            "a@b",
        ] {
            let err = LibraryId::new(s).unwrap_err();
            assert!(
                matches!(err, TenantIdError::InvalidChar { .. }),
                "expected InvalidChar for {s}, got {err:?}"
            );
        }
    }

    #[test]
    fn tenant_id_rejects_empty() {
        let err = LibraryId::new("").unwrap_err();
        assert_eq!(err, TenantIdError::Empty);
    }

    #[test]
    fn tenant_id_rejects_too_long() {
        let s = "a".repeat(TENANT_ID_MAX_LEN + 1);
        let err = LibraryId::new(s.clone()).unwrap_err();
        assert!(
            matches!(err, TenantIdError::TooLong { len } if len == TENANT_ID_MAX_LEN + 1),
            "got {err:?}"
        );

        // Exactly at the max is OK.
        let edge = "a".repeat(TENANT_ID_MAX_LEN);
        assert!(LibraryId::new(edge).is_ok());
    }

    #[test]
    fn tenant_id_fromstr_display_roundtrip() {
        use std::str::FromStr;
        for s in ["default", "tenant-01", "acme_co", "x", "_underscore"] {
            let t = LibraryId::from_str(s).unwrap();
            assert_eq!(t.to_string(), s);
            // Round-trip via display.
            let t2 = LibraryId::from_str(&t.to_string()).unwrap();
            assert_eq!(t, t2);
        }
    }

    #[test]
    fn tenant_id_serde_roundtrips_legal_id() {
        let t = LibraryId::new("tenant-01").unwrap();
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(s, "\"tenant-01\"");
        let back: LibraryId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn tenant_id_serde_rejects_illegal_id() {
        // Uppercase is illegal under our rules — TryFrom<String> must reject.
        let err = serde_json::from_str::<LibraryId>("\"BadCase\"");
        assert!(err.is_err(), "serde must reject illegal tenant id");
    }
}
