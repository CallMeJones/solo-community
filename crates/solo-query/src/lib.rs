// SPDX-License-Identifier: Apache-2.0

//! Solo query planner. Today's pipelines:
//!
//!   - [`recall`] — HNSW search + SQL fetch + status filter for vector
//!     similarity over episodes. Original surface, used by all three
//!     transports (CLI, MCP, HTTP).
//!   - [`context`] — combined agent-oriented bundle over recall,
//!     themes, facts, and contradictions.
//!   - [`inspect`] — single-row episode lookup by `MemoryId`.
//!   - [`derived`] — read pipelines for the Steward's outputs
//!     (clusters/abstractions/triples/contradictions). Added v0.4.0
//!     to give MCP + HTTP transports first-class queries against the
//!     "memory ABOUT memory" tables that previously had no public
//!     reader.
//!   - [`doc_search`] — HNSW search restricted to document chunks
//!     (v0.7.0 RAG). Mirrors `recall` but resolves rowids against
//!     `document_chunks` JOINed to `documents`, filters forgotten
//!     docs at the SQL level.
//!   - [`doc_inspect`] — single-doc inspection + paginated list.
//!     Mirrors `derived::inspect_cluster`'s pattern but for documents
//!     and their chunks.
//!
//! Future: BM25 / FTS5 fusion, encoding-context rerank, three-tier
//! retrieval per `solo-v0-architecture.md §3.5`.

pub mod assets;
pub mod context;
pub mod derived;
pub mod doc_inspect;
pub mod doc_search;
pub mod inbox;
pub mod inspect;
pub mod recall;
pub mod update;

pub use assets::{
    AssetDownloadTarget, AssetExtractionSummary, AssetInspectResult, AssetRecord,
    DocumentAssetLinkSummary, DocumentAssetsResult, MemoryAttachmentSummary,
    MemoryAttachmentsResult, download_asset, inspect_asset, list_assets, list_document_assets,
    list_memory_attachments, prepare_asset_download,
};
pub use context::{MemoryContextResult, memory_context};
pub use derived::{
    Abstraction, ClusterRecord, ClusterRow, ContradictionHit, ContradictionResolution,
    EPISODE_TRUNCATE_CHARS, EntityHit, EpisodeSummary, FactHit, MemoryQualityAliasGroup,
    MemoryQualityAuditConfig, MemoryQualityAuditReport, MemoryQualityHealth, MemoryQualityIssue,
    MemoryQualityTotals, ThemeHit, TripleSummary, contradictions, entities, facts_about,
    inspect_cluster, memory_quality_audit, resolve_contradiction, themes,
};
pub use doc_inspect::{
    CHUNK_PREVIEW_CHARS, ChunkSummary, DocumentInspectResult, DocumentRecord, DocumentSummary,
    inspect_document, list_documents,
};
pub use doc_search::{DocSearchHit, run_doc_search};
pub use inbox::{INBOX_MAX_LIMIT, MemoryInboxItem, memory_inbox};
pub use inspect::{EpisodeRecord, inspect_one};
pub use recall::{RecallHit, RecallResult, run_recall};
pub use update::{MemoryUpdateResult, memory_update};
