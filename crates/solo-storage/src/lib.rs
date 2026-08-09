// SPDX-License-Identifier: Apache-2.0

//! Solo storage: SQLite + SQLCipher persistence layer.
//!
//! ## Concurrency invariants (per ADR-0003)
//!
//!   * **Writes go through `WriteHandle`; reads go through `ReaderPool`.**
//!     Direct connection access is an anti-pattern outside the actor + pool.
//!   * The writer connection opens once and is owned by the writer thread for
//!     the daemon's lifetime.
//!   * The read pool's `post_create` hook binds the raw SQLCipher key on each
//!     new connection.
//!   * `pending_index` ordering is **always** SQL COMMIT → HNSW.add → drain
//!     row. Never reverse.
//!   * `Arc<dyn VectorIndex + Send + Sync>` is shared between the writer and
//!     the read pool; concurrency is provided by the impl (e.g., hnsw_rs's
//!     internal `parking_lot::RwLock`), not by application-level locks.
//!
//! ## Module layout
//!
//! Commit 1.1 — `solo init` building blocks:
//!
//!   - `path_validation` — refuse cloud-sync data dirs.
//!   - `key_material`    — Argon2id passphrase → 32-byte SQLCipher key.
//!   - `config`          — `solo.config.toml` (salt + embedder identity).
//!   - `migration`       — runner + the v0 schema (migrations/0001_initial.sql).
//!   - `lockfile`        — RAII `solo.lock` to serialize concurrent runs.
//!   - `init`            — orchestrator: `solo_storage::init(params)`.
//!
//! Commit 1.2 — single-writer actor + read pool:
//!
//!   - `writer`          — `WriterActor`, `WriteHandle`, `WriteCommand`.
//!   - `reader`          — `ReaderPool` (deadpool-sqlite + post_create raw-key).
//!
//! Commit 1.3 — HNSW backing for `solo_core::VectorIndex` + snapshot I/O:
//!
//!   - `vector_index`    — `HnswIndex` (`hnsw_rs` wrapper), `HnswFactory`.
//!   - `snapshot`        — atomic two-file save (live/`_bak`/`_tmp` basenames)
//!                         + `load`/`load_bak` per ADR-0003 §"Startup
//!                         file-existence decision tree".
//!   - `recovery`        — `replay_pending_index`, `detect_drift`. Used by
//!                         the daemon-main startup chain (commit 1.5).
//!
//! Embedder impls:
//!
//!   - `embedder::stub` — `StubEmbedder`, deterministic hash-based F32
//!                        embedder for tests + offline development.
//!   - `embedder::ollama` — `OllamaEmbedder`, real semantic embeddings
//!                          via a local Ollama daemon (`/api/embeddings`).
//!                          The recommended production backend since
//!                          v0.5.1; default for new deployments.
//!
//! (v0.5.x also shipped a BGE-M3 / candle-transformers backend; it was
//! deprecated in v0.5.0 and removed in v0.6.0. The replacement is
//! `OllamaEmbedder`.)
//!
//! Commit 1.5+ (daemon main + signal handlers) lands in subsequent files;
//! the surfaces here are stable for that wiring.

#![allow(dead_code)]

pub mod asset_blob;
pub mod audit;
pub mod backup;
pub mod config;
pub mod document;
pub mod embedder;
pub mod embedder_registry;
pub mod gdpr;
pub mod hnsw_id;
pub mod hnsw_rebuild;
pub mod init;
pub mod key_material;
pub mod library;
mod library_backup;
pub mod llm;
pub mod lockfile;
pub mod memory_library;
pub mod merge_candidates;
pub mod migration;
pub mod path_validation;
pub mod reader;
pub mod recovery;
pub mod redaction;
pub mod schema_import;
pub mod snapshot;
pub mod startup;
pub mod steward_factory;
pub mod triple_quality;
pub mod triples_batch;
pub mod vector_index;
pub mod writer;

#[cfg(test)]
mod properties;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

// Re-exports for the most common surface:
pub use asset_blob::{
    ASSET_BLOB_ENCRYPTION_ALG, ASSET_BLOB_PLAINTEXT_ALG, decode_asset_blob, expected_stored_size,
};
pub use audit::{
    AuditEvent, AuditOperation, AuditResult, AuditWriter, AuditWriterShutdown,
    insert_audit_admin_row, insert_audit_row_in_tx, purge_older_than,
};
pub use backup::{
    AssetBackupReport, BACKUP_STACK_SIZE_BYTES, DEFAULT_BACKUP_PAGES_PER_STEP, backup_database,
    backup_from_connection, backup_temp_path, package_asset_blobs_into_backup,
    paths_refer_to_same_file, replace_with_completed_backup, restore_asset_blobs_from_backup,
    strip_asset_blobs_from_backup_db,
};
pub use config::{
    AuditSettings, AuthSettings, CustomRedactionPattern, DocumentConfig, EmbedderConfig,
    IdentityConfig, LlmSettings, RedactionConfig, SamplingConfig, SamplingConfigDiagnostic,
    SoloConfig, StewardSettings, TriplesConfig, WorkspaceFileAccessConfig,
};
pub use document::{ChunkConfig, ChunkSpec, ParseError, ParsedDocument, chunk_text, parse_file};
pub use embedder::{
    OllamaEmbedder, StubEmbedder, build_embedder_from_env, probe_embedder_config_from_env,
};
pub use gdpr::{ForgetReport, estimate_forget_scope, forget_principal};
pub use library_backup::{BackupReport, RestoreReport, backup_library, restore_library};
pub use redaction::{RedactionMatch, RedactionRegistry, RedactionResult};
pub use schema_import::{
    ImportRecord, SchemaImportScan, SchemaImportSource, estimate_schema_chunks,
    materialize_schema_record, materialized_schema_record_path, parse_schema_import,
};
pub use steward_factory::{McpSamplingStewardFactory, StaticStewardFactory, StewardFactory};
// v0.9.0 P3: BundledEmbedder + its identity constants are re-exported
// from the crate root only when the `bundled-embedder` Cargo feature
// is on. Downstream code that needs to interrogate the feature gate
// at runtime should use `cfg!(feature = "bundled-embedder")`.
#[cfg(feature = "bundled-embedder")]
pub use embedder::{
    BUNDLED_EMBEDDER_DIM, BUNDLED_EMBEDDER_NAME, BUNDLED_EMBEDDER_VERSION, BundledEmbedder,
};
pub use embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
pub use hnsw_id::{HNSW_CHUNK_BIT, HnswIdKind, chunk_hnsw_id, decode_hnsw_id, episode_hnsw_id};
pub use init::{InitOutcome, InitParams, default_data_dir, default_embedder, init, open_sqlcipher};
pub use key_material::KeyMaterial;
pub use library::{LibraryHandle, LibraryOpenParams};
pub use lockfile::Lockfile;
pub use memory_library::{
    COMMUNITY_DB_FILENAME, MemoryLibrary, MemoryLibraryParams, migrate_legacy_default_library,
};
pub use merge_candidates::{MergeCandidateStats, count_existing_merge_candidates};
pub use migration::{current_version, run_migrations};
pub use path_validation::validate_data_dir;
pub use reader::{DEFAULT_POOL_SIZE, ReaderPool};
pub use recovery::{
    DriftReport, RebuildReport, ReplayReport, detect_drift, rebuild_hnsw_from_sql,
    replay_pending_index,
};
pub use snapshot::{BAK_BASENAME, LIVE_BASENAME, TMP_BASENAME};
pub use startup::{StartupOutcome, StartupParams, run as startup_run};
pub use triples_batch::TriplesBatchSignal;
pub use vector_index::{HnswFactory, HnswIndex, HnswParams};
pub use writer::{
    AssetExtractionReport, AttachAbstractionBatchReport, ConsolidationReport, ConsolidationScope,
    DEFAULT_CHANNEL_CAPACITY, DEFAULT_INGEST_MAX_BYTES, DerivedRepairCandidate, DerivedRepairMode,
    DerivedRepairReport, DerivedRepairScope, DocumentAssetLinkReport, EntityReviewOpReport,
    EntitySplitRequest, ForgetAssetReport, ForgetDocumentReport, IngestReport,
    MAX_REMEMBER_BATCH_SIZE, MemoryAttachmentReport, MemoryQualityReviewRewrite,
    MemoryQualityReviewStatus, MemoryQualityReviewUpdate, MemoryQualityReviewUpdateReport,
    MemoryRetrievalLogEntry, MemoryRetrievalLogReport, MemoryReviewReport, MemoryReviewState,
    NormalizeReport, ReembedReport, ReembedScope, ResolveContradictionReport, StoredAssetReport,
    WriteCommand, WriteHandle, WriterActor, WriterSpawn, resolve_ingest_max_bytes,
};
