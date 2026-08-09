// SPDX-License-Identifier: Apache-2.0

//! Audit log primitives (v0.8.0 P4).
//!
//! ## Two emission paths
//!
//! **Synchronous (mutating writer-actor handlers):** an audit row is
//! inserted as part of the same SQLite transaction that performs the
//! write. If the audit insert fails, the entire write rolls back —
//! strict ACID. Implemented via [`insert_audit_row_in_tx`].
//!
//! **Asynchronous (query path):** the query path runs on a `ReaderPool`
//! that doesn't own a writeable connection, and we don't want to pay an
//! extra round-trip on the hot read path. Instead, queries emit through
//! an [`AuditWriter`] backed by a bounded `tokio::sync::mpsc` channel.
//! A background task drains the channel, batches up to
//! [`AUDIT_BATCH_FLUSH_MAX_EVENTS`] events (or up to
//! [`AUDIT_BATCH_FLUSH_MAX_MILLIS`] milliseconds, whichever fires first),
//! and COMMITs each batch in one transaction.
//!
//! Backpressure: the mpsc capacity is [`AUDIT_QUEUE_CAPACITY`]. When the
//! channel is full, `AuditWriter::emit_async` drops the event with a
//! `tracing::warn!` line — the query path is never blocked. This is the
//! explicit trade-off documented in 0090 §"Concurrency contract": the
//! query path's latency budget dominates compliance completeness for the
//! "queries log my reads" case. Mutating writes have no such trade-off —
//! they go through the synchronous path with full ACID guarantees.

use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use solo_core::{Error, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::init::open_sqlcipher;
use crate::key_material::KeyMaterial;

/// Bounded mpsc capacity for the async audit pipeline. 1024 lets a burst
/// of 1000 concurrent recalls land without dropping; sustained load past
/// this drops events with a `tracing::warn!` line. Tuned with the
/// `recall(1000)` stress test in mind.
pub const AUDIT_QUEUE_CAPACITY: usize = 1024;

/// Max events per flushed batch. Trading off larger batches (fewer
/// transactions, lower overhead) vs. tighter recency for the audit log.
/// 64 chosen so a sustained recall storm flushes ~16 batches/sec at
/// burst — well within SQLite's commit-rate envelope.
pub const AUDIT_BATCH_FLUSH_MAX_EVENTS: usize = 64;

/// Max millisecond delay before forcing a flush even with a partial
/// batch. Sets the bound on "how recent is the audit log". 50ms means
/// the audit log lags real time by at most 50ms in the absence of a
/// crash; on crash, any events still in the mpsc/batch are lost (this
/// is the explicit async trade-off — mutating writes use the
/// synchronous transactional path).
pub const AUDIT_BATCH_FLUSH_MAX_MILLIS: u64 = 50;

/// All audit operations Solo records. 1:1 with the 13 MCP tools + admin
/// tenant ops + redaction + GDPR. Display string matches the column
/// value persisted in `audit_events.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditOperation {
    // Per-tenant writes (synchronous emit inside writer-actor tx)
    MemoryRemember,
    /// v0.9.2: batched-remember from agentic clients.
    /// One audit row per batch (not per item) — `details_json`
    /// carries `item_count` so the audit trail still represents
    /// "what the client asked Solo to do" 1:1. Synchronous emit
    /// inside the BEGIN IMMEDIATE tx that covers all batched
    /// `episodes` INSERTs (lesson #30: ACID for batch + audit).
    MemoryRememberBatch,
    MemoryUpdate,
    MemoryForget,
    MemoryReview,
    MemoryConsolidate,
    /// Operator repair for stale/low-signal derived graph artifacts.
    /// Synchronous emit inside the writer-actor transaction when the
    /// repair mutates storage; dry-runs emit an ok audit row outside the
    /// tx with `dry_run=true` in details.
    MemoryDerivedRepair,
    /// Memory Quality inbox decision that may promote or rewrite derived
    /// graph facts. Synchronous emit inside the writer-actor transaction.
    MemoryQualityReview,
    MemoryReembed,
    MemoryIngestDocument,
    MemoryForgetDocument,
    MemoryNormalizeSubjects,
    MemoryBackup,
    MemorySaveSnapshot,
    /// v0.9.0 P1: per-batch audit row emitted by the
    /// (planned-for-P4) Steward background batch when it extracts
    /// triples from accumulated episodes. `details_json` carries
    /// `episode_count`, `triples_extracted`, and `duration_ms`.
    /// Synchronous emit inside the Steward batch transaction so the
    /// row + the INSERTed triples land atomically (lesson #30: ACID
    /// for batch + audit). Variant lands in P1 so P4's writer-actor
    /// reshape doesn't have to bump the enum in the same commit.
    MemoryTriplesExtract,
    /// v0.9.0 P2: per-call audit row emitted by `SamplingLlmClient`
    /// when an MCP-sampling LLM call completes (success or failure).
    /// Lives in the **per-tenant** `audit_events` table because the
    /// prompt content is tenant-scoped data going to a third-party
    /// LLM client.
    ///
    /// `details_json` carries metadata ONLY — model hint, message
    /// count, max_tokens, duration_ms, total prompt characters,
    /// approximate input/output token counts when available. **The
    /// raw prompt content MUST NOT appear in this row** (privacy
    /// invariant; the prompt is user data and the user did NOT
    /// consent to it being logged here). Tests pin this with
    /// `sampling_audit_row_omits_raw_prompt_text`.
    ///
    /// Synchronous emit inside the writer-actor tx (lesson #30: ACID
    /// for the sampling call's only persisted trace). On insert
    /// failure, the caller of `SamplingLlmClient::complete()` MUST
    /// see the failure — the audit row IS the only record of the
    /// call.
    LlmSamplingCall,

    // Per-tenant queries (async emit via AuditWriter)
    MemoryRecall,
    MemoryContext,
    MemoryInspect,
    MemoryThemes,
    MemoryFactsAbout,
    MemoryEntities,
    MemoryQualityAudit,
    MemoryContradictions,
    MemoryContradictionResolve,
    MemoryInspectCluster,
    MemorySearchDocs,
    MemoryInspectDocument,
    MemoryListDocuments,
    MemoryInspectAsset,
    MemoryListAssets,
    MemoryListDocumentAssets,
    MemoryListMemoryAttachments,
    MemoryPrepareAssetDownload,
    MemoryDownloadAsset,
    MemoryInbox,
    MemoryStoreAsset,
    MemoryRecordAssetExtraction,
    MemoryLinkDocumentAsset,
    MemoryAttach,
    MemoryForgetAsset,

    // Per-tenant redaction (v0.8.0 P5; synchronous emit inside the same
    // writer-actor tx as the ingest/remember it summarises).
    RedactionApplied,

    // Admin (write to audit_events_admin in tenants_index.db; wired in P7)
    TenantCreate,
    TenantDelete,
    LibraryBackup,
    LibraryRestore,

    /// v0.8.1 P3: admin-tier op recording a quota change (set / change /
    /// clear). Written to `audit_events_admin` because it's a registry-
    /// scope change, not per-tenant data.
    TenantSetQuota,

    // GDPR (v0.8.0 P6; written to audit_events_admin because the subject
    // can no longer query their own per-tenant DB audit_events post-
    // deletion).
    GdprForgetUser,
}

impl AuditOperation {
    /// Canonical string form persisted in the `operation` column.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MemoryRemember => "memory.remember",
            Self::MemoryRememberBatch => "memory.remember_batch",
            Self::MemoryUpdate => "memory.update",
            Self::MemoryForget => "memory.forget",
            Self::MemoryReview => "memory.review",
            Self::MemoryConsolidate => "memory.consolidate",
            Self::MemoryDerivedRepair => "memory.derived_repair",
            Self::MemoryQualityReview => "memory.quality_review",
            Self::MemoryReembed => "memory.reembed",
            Self::MemoryIngestDocument => "memory.ingest_document",
            Self::MemoryForgetDocument => "memory.forget_document",
            Self::MemoryNormalizeSubjects => "memory.normalize_subjects",
            Self::MemoryBackup => "memory.backup",
            Self::MemorySaveSnapshot => "memory.save_snapshot",
            Self::MemoryTriplesExtract => "memory.triples_extract",
            Self::LlmSamplingCall => "llm.sampling_call",
            Self::MemoryRecall => "memory.recall",
            Self::MemoryContext => "memory.context",
            Self::MemoryInspect => "memory.inspect",
            Self::MemoryThemes => "memory.themes",
            Self::MemoryFactsAbout => "memory.facts_about",
            Self::MemoryEntities => "memory.entities",
            Self::MemoryQualityAudit => "memory.quality_audit",
            Self::MemoryContradictions => "memory.contradictions",
            Self::MemoryContradictionResolve => "memory.contradiction_resolve",
            Self::MemoryInspectCluster => "memory.inspect_cluster",
            Self::MemorySearchDocs => "memory.search_docs",
            Self::MemoryInspectDocument => "memory.inspect_document",
            Self::MemoryListDocuments => "memory.list_documents",
            Self::MemoryInspectAsset => "memory.inspect_asset",
            Self::MemoryListAssets => "memory.list_assets",
            Self::MemoryListDocumentAssets => "memory.list_document_assets",
            Self::MemoryListMemoryAttachments => "memory.list_memory_attachments",
            Self::MemoryPrepareAssetDownload => "memory.prepare_asset_download",
            Self::MemoryDownloadAsset => "memory.download_asset",
            Self::MemoryInbox => "memory.inbox",
            Self::MemoryStoreAsset => "memory.store_asset",
            Self::MemoryRecordAssetExtraction => "memory.record_asset_extraction",
            Self::MemoryLinkDocumentAsset => "memory.link_document_asset",
            Self::MemoryAttach => "memory.attach",
            Self::MemoryForgetAsset => "memory.forget_asset",
            Self::RedactionApplied => "redaction.applied",
            Self::TenantCreate => "tenant.create",
            Self::TenantDelete => "tenant.delete",
            Self::LibraryBackup => "library.backup",
            Self::LibraryRestore => "library.restore",
            Self::TenantSetQuota => "tenant.set_quota",
            Self::GdprForgetUser => "gdpr.forget_user",
        }
    }
}

impl std::fmt::Display for AuditOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Persisted `result` column value. Mirrors the SQL CHECK domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditResult {
    Ok,
    Error,
    Forbidden,
}

impl AuditResult {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Forbidden => "forbidden",
        }
    }
}

/// One audit event before persistence. Built by the caller; the audit
/// pipeline owns persistence + retention.
#[derive(Debug, Clone)]
pub struct AuditEvent {
    /// Epoch ms. Filled by callers via `chrono::Utc::now().timestamp_millis()`.
    pub ts_ms: i64,
    /// Authenticated principal's subject (JWT `sub` or `"bearer"`). `None`
    /// for CLI / no-auth / system-initiated paths.
    pub principal_subject: Option<String>,
    pub operation: AuditOperation,
    /// Subject of the operation: a memory_id, doc_id, cluster_id, query
    /// hash, etc. `None` when the operation isn't bound to a single id
    /// (e.g. `memory.themes`).
    pub target_id: Option<String>,
    pub result: AuditResult,
    /// Optional structured detail as a `serde_json::Value`. Serialized
    /// to TEXT for storage; `None` persists as NULL.
    pub details: Option<serde_json::Value>,
}

impl AuditEvent {
    /// Construct an `ok` event with current-time timestamp. Convenience
    /// for the synchronous mutating-handler emit path.
    pub fn ok_now(
        principal_subject: Option<String>,
        operation: AuditOperation,
        target_id: Option<String>,
    ) -> Self {
        Self {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            principal_subject,
            operation,
            target_id,
            result: AuditResult::Ok,
            details: None,
        }
    }

    /// Construct an `error` event with current-time timestamp + a JSON
    /// details blob carrying the failure summary.
    pub fn error_now(
        principal_subject: Option<String>,
        operation: AuditOperation,
        target_id: Option<String>,
        error_message: impl Into<String>,
    ) -> Self {
        Self {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            principal_subject,
            operation,
            target_id,
            result: AuditResult::Error,
            details: Some(serde_json::json!({ "error": error_message.into() })),
        }
    }
}

/// Persist one audit event inside an already-open SQLCipher transaction.
/// Synchronous emit path — used by the writer-actor for mutating ops, so
/// the audit row is atomic with the audited SQL write.
///
/// If this returns an error the caller MUST rollback the surrounding
/// transaction — strict ACID for the mutating write.
pub fn insert_audit_row_in_tx(tx: &Transaction<'_>, event: &AuditEvent) -> Result<()> {
    let details_json: Option<String> = match event.details.as_ref() {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| Error::storage(format!("serialize audit details: {e}")))?,
        ),
        None => None,
    };
    tx.execute(
        "INSERT INTO audit_events (
            ts_ms, principal_subject, operation, target_id, result, details_json
         ) VALUES (?, ?, ?, ?, ?, ?)",
        params![
            event.ts_ms,
            event.principal_subject.as_deref(),
            event.operation.as_str(),
            event.target_id.as_deref(),
            event.result.as_str(),
            details_json,
        ],
    )
    .map_err(|e| Error::storage(format!("INSERT audit_events: {e}")))?;
    Ok(())
}

/// Persist one admin-tier audit event into the
/// `audit_events_admin` table in `tenants_index.db`. Schema differs
/// from per-tenant `audit_events`: the row is keyed by
/// `target_tenant_id` (the tenant the admin operation affects) rather
/// than `target_id` (which is per-tenant data scope).
///
/// Returns the last_insert_rowid for the inserted row — callers (GDPR
/// + backup/restore) bubble that to their reports for traceability.
///
/// Synchronous — wrap in a tx if the caller wants atomicity with
/// surrounding work. The standalone helper just executes the INSERT on
/// the supplied connection.
pub fn insert_audit_admin_row(
    conn: &Connection,
    ts_ms: i64,
    principal_subject: Option<&str>,
    operation: AuditOperation,
    target_tenant_id: Option<&str>,
    result: AuditResult,
    details: Option<&serde_json::Value>,
) -> Result<i64> {
    let details_json: Option<String> = match details {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| Error::storage(format!("serialize admin audit details: {e}")))?,
        ),
        None => None,
    };
    conn.execute(
        "INSERT INTO audit_events_admin (
            ts_ms, principal_subject, operation, target_tenant_id, result, details_json
         ) VALUES (?, ?, ?, ?, ?, ?)",
        params![
            ts_ms,
            principal_subject,
            operation.as_str(),
            target_tenant_id,
            result.as_str(),
            details_json,
        ],
    )
    .map_err(|e| Error::storage(format!("INSERT audit_events_admin: {e}")))?;
    Ok(conn.last_insert_rowid())
}

/// Persist one audit event on a non-transactional connection. Used by
/// the async batch drainer (which opens its own SQLCipher connection
/// per tenant; the writer-actor's connection is unavailable from the
/// query side).
#[allow(dead_code)]
fn insert_audit_row_one_off(conn: &Connection, event: &AuditEvent) -> Result<()> {
    let details_json: Option<String> = match event.details.as_ref() {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| Error::storage(format!("serialize audit details: {e}")))?,
        ),
        None => None,
    };
    conn.execute(
        "INSERT INTO audit_events (
            ts_ms, principal_subject, operation, target_id, result, details_json
         ) VALUES (?, ?, ?, ?, ?, ?)",
        params![
            event.ts_ms,
            event.principal_subject.as_deref(),
            event.operation.as_str(),
            event.target_id.as_deref(),
            event.result.as_str(),
            details_json,
        ],
    )
    .map_err(|e| Error::storage(format!("INSERT audit_events (async): {e}")))?;
    Ok(())
}

/// Cheaply-cloneable handle for async audit emit.
///
/// Cloned freely across query handlers (one clone per outstanding query
/// is fine — the channel sender is itself cheap). Dropping the LAST
/// clone closes the channel and lets the background drainer flush + exit
/// cleanly.
#[derive(Clone)]
pub struct AuditWriter {
    tx: mpsc::Sender<AuditEvent>,
}

/// Handle to the spawned audit drainer task. Held by `LibraryHandle` so a
/// graceful shutdown can wait for the drainer to finish flushing the
/// pending batch.
pub struct AuditWriterShutdown {
    drainer: tokio::task::JoinHandle<()>,
}

impl AuditWriterShutdown {
    /// Await the drainer's exit. Call AFTER dropping every `AuditWriter`
    /// clone so the mpsc channel closes and the drainer drains-and-exits.
    pub async fn join(self) {
        if let Err(e) = self.drainer.await {
            tracing::warn!(error = %e, "audit drainer task join error");
        }
    }
}

impl AuditWriter {
    /// Spawn a new audit drainer for one tenant's DB. Returns a cheap-
    /// to-clone writer plus a shutdown handle the caller stores alongside.
    ///
    /// `db_path` + `key` are used by the drainer to open its own
    /// SQLCipher connection (separate from the writer-actor's so the
    /// async emit path doesn't contend on the writer's mutex). The
    /// connection is opened lazily on first event so cold tenants don't
    /// pay the cost.
    pub fn spawn(db_path: PathBuf, key: Option<KeyMaterial>) -> (Self, AuditWriterShutdown) {
        let (tx, rx) = mpsc::channel::<AuditEvent>(AUDIT_QUEUE_CAPACITY);
        let drainer = tokio::spawn(audit_drainer_loop(rx, db_path, key));
        (Self { tx }, AuditWriterShutdown { drainer })
    }

    /// Spawn a no-op writer that drops every event. Used by tests that
    /// don't care about audit emission, and by code paths that want to
    /// thread an `AuditWriter` argument without actually wiring storage
    /// (e.g. the legacy single-tenant test harnesses).
    pub fn noop() -> Self {
        // 1-capacity channel whose receiver is dropped immediately — every
        // send fails, which `emit_async` swallows.
        let (tx, _rx) = mpsc::channel::<AuditEvent>(1);
        // Drop the receiver in a background task to keep the channel open
        // for `try_send` to succeed in capacity-1 mode (the receiver is
        // immediately dropped here so try_send fails — the warn-on-drop
        // path covers this). Either way the event is intentionally lost.
        Self { tx }
    }

    /// Try to enqueue an event for async persistence. Non-blocking: if
    /// the channel is full, the event is dropped + a single
    /// `tracing::warn!` line records the loss. The query path is never
    /// blocked.
    ///
    /// Returns `true` if enqueued, `false` if dropped.
    pub fn emit_async(&self, event: AuditEvent) -> bool {
        match self.tx.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(ev)) => {
                tracing::warn!(
                    operation = %ev.operation,
                    "audit: mpsc full, dropping event (queue capacity {AUDIT_QUEUE_CAPACITY})"
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(ev)) => {
                tracing::debug!(
                    operation = %ev.operation,
                    "audit: writer closed, dropping event (shutdown in progress?)"
                );
                false
            }
        }
    }

    /// Convenience: build + emit an `ok` event for `operation`.
    pub fn emit_ok(
        &self,
        principal_subject: Option<String>,
        operation: AuditOperation,
        target_id: Option<String>,
    ) {
        let _ = self.emit_async(AuditEvent::ok_now(principal_subject, operation, target_id));
    }

    /// Convenience: build + emit an `error` event for `operation` with
    /// the failure message in details.
    pub fn emit_error(
        &self,
        principal_subject: Option<String>,
        operation: AuditOperation,
        target_id: Option<String>,
        err: impl std::fmt::Display,
    ) {
        let _ = self.emit_async(AuditEvent::error_now(
            principal_subject,
            operation,
            target_id,
            err.to_string(),
        ));
    }
}

/// Background loop: read up to `AUDIT_BATCH_FLUSH_MAX_EVENTS` events or
/// wait `AUDIT_BATCH_FLUSH_MAX_MILLIS`, whichever first; flush the batch
/// to SQLite in one transaction; repeat.
///
/// Exits when the mpsc channel is closed (every `AuditWriter` clone has
/// been dropped). Flushes any partial batch before exit.
async fn audit_drainer_loop(
    mut rx: mpsc::Receiver<AuditEvent>,
    db_path: PathBuf,
    key: Option<KeyMaterial>,
) {
    // Lazy connection — opened on first event so cold tenants don't pay
    // the cost. `None` until we have something to write.
    let mut conn: Option<Connection> = None;
    let flush_interval = Duration::from_millis(AUDIT_BATCH_FLUSH_MAX_MILLIS);

    loop {
        // Block until we have at least one event (or shutdown).
        let first = match rx.recv().await {
            Some(e) => e,
            None => {
                // Channel closed; no more events will arrive. Done.
                tracing::debug!("audit drainer: mpsc closed, exiting");
                return;
            }
        };
        let mut batch: Vec<AuditEvent> = Vec::with_capacity(AUDIT_BATCH_FLUSH_MAX_EVENTS);
        batch.push(first);

        // Greedily pull more events without waiting (drain whatever else
        // is already buffered), up to the batch cap.
        while batch.len() < AUDIT_BATCH_FLUSH_MAX_EVENTS {
            match rx.try_recv() {
                Ok(e) => batch.push(e),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }

        // If we still have budget for more events AND the batch isn't
        // full, wait up to flush_interval for stragglers. Done with
        // `tokio::time::timeout` so the wait is bounded.
        if batch.len() < AUDIT_BATCH_FLUSH_MAX_EVENTS {
            let deadline = tokio::time::Instant::now() + flush_interval;
            while batch.len() < AUDIT_BATCH_FLUSH_MAX_EVENTS {
                let now = tokio::time::Instant::now();
                if now >= deadline {
                    break;
                }
                let remaining = deadline - now;
                match tokio::time::timeout(remaining, rx.recv()).await {
                    Ok(Some(e)) => batch.push(e),
                    Ok(None) => break, // channel closed
                    Err(_) => break,   // deadline reached
                }
            }
        }

        // Ensure the connection is open (lazy first-event open).
        if conn.is_none() {
            match open_drainer_conn(&db_path, key.as_ref()) {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        path = %db_path.display(),
                        dropped = batch.len(),
                        "audit drainer: failed to open connection, dropping batch"
                    );
                    // Don't retry on this iteration — we'd loop forever.
                    // The next batch may succeed (e.g. transient I/O).
                    continue;
                }
            }
        }
        let c = conn.as_mut().expect("conn just set");

        if let Err(e) = flush_batch(c, &batch) {
            tracing::error!(
                error = %e,
                dropped = batch.len(),
                "audit drainer: flush failed, dropping batch"
            );
        }
    }
}

fn open_drainer_conn(db_path: &std::path::Path, key: Option<&KeyMaterial>) -> Result<Connection> {
    if let Some(k) = key {
        open_sqlcipher(db_path, k)
    } else {
        // Test/legacy path: open plain SQLite. Same shape as
        // `ReaderPool::new(..., None, ...)`.
        let conn = Connection::open(db_path).map_err(|e| {
            Error::storage(format!("audit drainer open {}: {e}", db_path.display()))
        })?;
        conn.execute_batch(
            "PRAGMA journal_mode = wal;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(|e| Error::storage(format!("audit drainer pragmas: {e}")))?;
        Ok(conn)
    }
}

fn flush_batch(conn: &mut Connection, batch: &[AuditEvent]) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| Error::storage(format!("audit drainer BEGIN IMMEDIATE: {e}")))?;
    for event in batch {
        insert_audit_row_in_tx(&tx, event)?;
    }
    tx.commit()
        .map_err(|e| Error::storage(format!("audit drainer COMMIT: {e}")))?;
    Ok(())
}

/// Delete every audit/retrieval-log row older than `cutoff_ms`.
/// Returns the number of rows deleted.
///
/// Idempotent: re-running on the same cutoff is a no-op once the rows
/// are gone. Single-transaction so a crash mid-purge leaves the table
/// either fully purged or fully intact.
pub fn purge_older_than(conn: &mut Connection, cutoff_ms: i64) -> Result<usize> {
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for purge: {e}")))?;
    let audit_rows = tx
        .execute(
            "DELETE FROM audit_events WHERE ts_ms < ?",
            params![cutoff_ms],
        )
        .map_err(|e| Error::storage(format!("DELETE audit_events: {e}")))?;
    let retrieval_rows = tx
        .execute(
            "DELETE FROM memory_retrieval_log WHERE created_at_ms < ?",
            params![cutoff_ms],
        )
        .map_err(|e| Error::storage(format!("DELETE memory_retrieval_log: {e}")))?;
    tx.commit()
        .map_err(|e| Error::storage(format!("COMMIT purge: {e}")))?;
    Ok(audit_rows + retrieval_rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_in_memory_with_audit() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        crate::migration::run_migrations(&mut conn).expect("migrations");
        conn
    }

    #[test]
    fn audit_operation_display_matches_canonical_string() {
        assert_eq!(
            AuditOperation::MemoryRemember.to_string(),
            "memory.remember"
        );
        assert_eq!(AuditOperation::MemoryRecall.to_string(), "memory.recall");
        assert_eq!(AuditOperation::TenantCreate.to_string(), "tenant.create");
    }

    #[test]
    fn audit_result_check_constraint_rejects_unknown_value() {
        let conn = open_in_memory_with_audit();
        // Direct INSERT bypasses our enum so we can test the SQL CHECK.
        let res = conn.execute(
            "INSERT INTO audit_events (ts_ms, operation, result) VALUES (?, ?, ?)",
            params![0i64, "memory.remember", "bogus"],
        );
        assert!(res.is_err(), "result='bogus' must violate CHECK");
    }

    #[test]
    fn insert_then_select_round_trip() {
        let mut conn = open_in_memory_with_audit();
        let tx = conn.transaction().unwrap();
        let event = AuditEvent::ok_now(
            Some("alice".into()),
            AuditOperation::MemoryRemember,
            Some("00000000-0000-0000-0000-000000000001".into()),
        );
        insert_audit_row_in_tx(&tx, &event).unwrap();
        tx.commit().unwrap();

        let (op, principal, target, result): (String, Option<String>, Option<String>, String) =
            conn.query_row(
                "SELECT operation, principal_subject, target_id, result FROM audit_events",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(op, "memory.remember");
        assert_eq!(principal.as_deref(), Some("alice"));
        assert_eq!(
            target.as_deref(),
            Some("00000000-0000-0000-0000-000000000001")
        );
        assert_eq!(result, "ok");
    }

    #[test]
    fn purge_older_than_drops_old_rows() {
        let mut conn = open_in_memory_with_audit();
        // Three rows at ts=100/200/300; purge anything ts < 250.
        for ts in [100i64, 200, 300] {
            conn.execute(
                "INSERT INTO audit_events (ts_ms, operation, result) VALUES (?, ?, ?)",
                params![ts, "memory.remember", "ok"],
            )
            .unwrap();
        }
        let purged = purge_older_than(&mut conn, 250).unwrap();
        assert_eq!(purged, 2, "purge should drop ts=100 and ts=200");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM audit_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn purge_older_than_drops_old_retrieval_log_rows() {
        let mut conn = open_in_memory_with_audit();
        for ts in [100i64, 200, 300] {
            conn.execute(
                "INSERT INTO memory_retrieval_log
                    (retrieval_id, query, recalled_ids_json, reason_codes_json, created_at_ms)
                 VALUES (?, 'query', '[]', '[]', ?)",
                params![format!("ret-{ts}"), ts],
            )
            .unwrap();
        }

        let purged = purge_older_than(&mut conn, 250).unwrap();

        assert_eq!(purged, 2, "purge should drop retrieval ts=100 and ts=200");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_retrieval_log", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn purge_older_than_idempotent() {
        let mut conn = open_in_memory_with_audit();
        conn.execute(
            "INSERT INTO audit_events (ts_ms, operation, result) VALUES (100, 'memory.remember', 'ok')",
            [],
        )
        .unwrap();
        assert_eq!(purge_older_than(&mut conn, 200).unwrap(), 1);
        assert_eq!(purge_older_than(&mut conn, 200).unwrap(), 0);
    }

    #[test]
    fn noop_writer_drops_events_without_blocking() {
        let writer = AuditWriter::noop();
        // try_send on a closed-receiver channel fails immediately, but
        // emit_async swallows that — should never panic / block.
        for _ in 0..100 {
            writer.emit_ok(None, AuditOperation::MemoryRecall, None);
        }
    }

    #[test]
    fn migration_0005_applied_once_on_repeated_open() {
        // Build a fresh in-memory DB, run migrations TWICE; the audit
        // schema_migrations row count for version 5 should be exactly 1.
        let mut conn = Connection::open_in_memory().expect("in-memory");
        crate::migration::run_migrations(&mut conn).unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "migration 0005 must apply at most once");
    }

    #[test]
    fn audit_table_present_and_indices_exist() {
        let conn = open_in_memory_with_audit();
        let table: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='audit_events'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(table, 1);
        for idx in ["idx_audit_ts", "idx_audit_principal"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?",
                    params![idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(exists, 1, "missing index: {idx}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn batched_load_does_not_drop_under_burst() {
        // Spawn the drainer; emit 1000 events; verify all 1000 land.
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("burst.db");
        // Bootstrap the schema on a separate handle (the drainer opens
        // its own lazily, but the audit_events table must exist before
        // the first batch lands).
        let mut conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = wal;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        drop(conn);

        let (audit, shutdown) = AuditWriter::spawn(db_path.clone(), None);
        // Burst: capacity is 1024; 1000 should fit without dropping. We
        // emit synchronously from a single task — the drainer pulls in
        // parallel — so we exercise the queue's drain rate.
        for i in 0..1000 {
            audit.emit_ok(
                Some(format!("user-{i}")),
                AuditOperation::MemoryRecall,
                None,
            );
        }
        // Give the drainer time to flush.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(audit);
        shutdown.join().await;

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE operation = 'memory.recall'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1000, "all 1000 audit rows must land");
    }

    #[test]
    fn ok_now_and_error_now_construct_expected_shapes() {
        let ok = AuditEvent::ok_now(
            Some("u".into()),
            AuditOperation::MemoryRecall,
            Some("tid".into()),
        );
        assert_eq!(ok.result, AuditResult::Ok);
        assert!(ok.details.is_none());

        let err = AuditEvent::error_now(None, AuditOperation::MemoryRecall, None, "boom");
        assert_eq!(err.result, AuditResult::Error);
        let details = err.details.expect("error event carries details");
        assert_eq!(details["error"], "boom");
    }

    /// v0.9.0 P1: the new `MemoryTriplesExtract` variant exists and
    /// renders to its canonical persisted spelling. P4 will emit rows
    /// with this operation when the Steward background batch finishes;
    /// pinning the string here keeps the wire format stable across the
    /// P1 → P4 handoff.
    #[test]
    fn memory_triples_extract_renders_canonical_string() {
        assert_eq!(
            AuditOperation::MemoryTriplesExtract.as_str(),
            "memory.triples_extract"
        );
        // `Display` mirrors `as_str` for every variant — same contract
        // the existing variants honour.
        assert_eq!(
            format!("{}", AuditOperation::MemoryTriplesExtract),
            "memory.triples_extract"
        );
    }

    /// v0.9.0 P2: the new `LlmSamplingCall` variant exists and renders
    /// to its canonical persisted spelling. `SamplingLlmClient`
    /// (lives in `solo-api`) emits rows with this operation on every
    /// `peer.create_message` call; pinning the string here keeps the
    /// wire format stable across the P2 implementation.
    #[test]
    fn llm_sampling_call_renders_canonical_string() {
        assert_eq!(
            AuditOperation::LlmSamplingCall.as_str(),
            "llm.sampling_call"
        );
        // `Display` mirrors `as_str` — same contract.
        assert_eq!(
            format!("{}", AuditOperation::LlmSamplingCall),
            "llm.sampling_call"
        );
    }

    // ---- v0.10.1 F7 audit-minor closure: burst ordering pin
    //      (deferred from v0.9.0 P2 §F7). ----
    //
    // `AuditEvent::ok_now` / `error_now` stamp `ts_ms` from
    // `chrono::Utc::now().timestamp_millis()`. Under tight bursts
    // (many emits in <1ms), multiple rows can share the same `ts_ms`.
    // The existing `reconfigurable_fake_distinguishes_audit_rows`
    // test (lives in `solo-api`) reads back with
    // `ORDER BY ts_ms ASC, rowid ASC` and relies on the secondary
    // `rowid` sort, but the F7 audit minor flagged that no test
    // explicitly exercised the burst-same-ts ordering path. These
    // tests close that gap.
    //
    //   1. `audit_rows_under_burst_are_ordered_by_ts_then_rowid` —
    //      drives N emits as fast as possible through the async
    //      drainer; reads back ordered by `(ts_ms, rowid)`; asserts
    //      insertion order is preserved even when multiple rows
    //      share `ts_ms`. The N=50 figure is enough to virtually
    //      guarantee at least two rows share a millisecond on
    //      modern hardware.
    //   2. `audit_row_ts_ms_is_monotonic_within_writer_actor` —
    //      asserts the weaker invariant: ts_ms never decreases. If
    //      the system clock jumps backward mid-burst, this would
    //      fail; we treat the failure as "the test environment's
    //      clock is broken", not a Solo bug. The chrono Utc::now
    //      call is wall-clock; we don't try to enforce monotonicity
    //      ourselves (lesson #30 — only pin what we own).
    //
    // Grep terms: F7, audit_rows_under_burst, audit_row_ts_ms_is_monotonic.

    /// F7 pin: under a 50-row burst, reading back with
    /// `ORDER BY ts_ms ASC, audit_id ASC` recovers insertion order
    /// even when multiple rows share `ts_ms`. The `audit_id` is the
    /// table's PRIMARY KEY AUTOINCREMENT, which monotonically
    /// increases for every successful INSERT (SQLite guarantees
    /// strictly increasing with AUTOINCREMENT), so the secondary
    /// sort is sufficient.
    ///
    /// Tagged the burst entries by `target_id` (a sequence
    /// "0".."49") so we can assert post-read order matches the
    /// emit order exactly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn audit_rows_under_burst_are_ordered_by_ts_then_rowid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("burst-order.db");
        // Bootstrap the schema; the drainer opens its own
        // connection later but the `audit_events` table must exist
        // first.
        let mut conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = wal;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        drop(conn);

        let (audit, shutdown) = AuditWriter::spawn(db_path.clone(), None);
        // Emit 50 rows as fast as a single task can push them. We
        // tag each row by `target_id = "0".."49"` so the post-read
        // order assertion is unambiguous.
        const BURST: i64 = 50;
        for i in 0..BURST {
            audit.emit_ok(None, AuditOperation::MemoryRecall, Some(i.to_string()));
        }
        // Give the drainer time to flush + shut it down.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(audit);
        shutdown.join().await;

        // Read back ordered by (ts_ms, audit_id). Both columns are
        // load-bearing: under burst, multiple rows share ts_ms; the
        // audit_id AUTOINCREMENT is the tiebreaker that recovers
        // insertion order.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT target_id, ts_ms, audit_id FROM audit_events
                 WHERE operation = 'memory.recall'
                 ORDER BY ts_ms ASC, audit_id ASC",
            )
            .unwrap();
        let rows: Vec<(Option<String>, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows.len(),
            BURST as usize,
            "all {BURST} burst rows must land"
        );
        // Insertion order recovered: target_id values must be "0",
        // "1", ..., "49" in order.
        let target_ids: Vec<String> = rows
            .iter()
            .map(|(t, _, _)| t.clone().expect("target_id present"))
            .collect();
        let expected: Vec<String> = (0..BURST).map(|i| i.to_string()).collect();
        assert_eq!(
            target_ids, expected,
            "burst-emit order must be recoverable from (ts_ms ASC, audit_id ASC)"
        );

        // Verify the burst actually exercised the same-ms case —
        // i.e., at least two rows DO share a ts_ms. If every row
        // had a distinct ms, the test wouldn't have exercised the
        // F7 concern. We compute the unique ts_ms count and assert
        // it's < BURST. (On hardware too slow to ever co-locate
        // two emits in a ms — single-digit MHz — this assertion
        // could fail; the floor for modern CPUs is ~1µs per emit,
        // far below 1ms, so this is safe.)
        let unique_ts: std::collections::HashSet<i64> = rows.iter().map(|(_, ts, _)| *ts).collect();
        assert!(
            unique_ts.len() < rows.len(),
            "burst should produce same-ms collisions (got {} unique ts for {} rows)",
            unique_ts.len(),
            rows.len()
        );

        // And the secondary (audit_id) sort: audit_id is strictly
        // ascending within the (ts_ms ASC, audit_id ASC) order we
        // queried by — pin that the sort key gives a total order.
        let audit_ids: Vec<i64> = rows.iter().map(|(_, _, id)| *id).collect();
        let mut sorted = audit_ids.clone();
        sorted.sort();
        assert_eq!(
            audit_ids, sorted,
            "audit_id sequence under the (ts_ms, audit_id) sort must already be ascending"
        );
    }

    /// F7 pin (weak invariant): ts_ms never decreases within a
    /// single-task burst. `chrono::Utc::now()` is wall-clock, so the
    /// test will fail only if the OS clock jumps backward — which we
    /// treat as a broken environment, not a Solo bug. The test
    /// guards against an accidental refactor that stamps from
    /// `Instant::now` deltas without `Utc` reconciliation (which
    /// could go backward across DST or NTP jumps).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn audit_row_ts_ms_is_monotonic_within_writer_actor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let db_path = tmp.path().join("monotonic.db");
        let mut conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA journal_mode = wal;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
        .unwrap();
        crate::migration::run_migrations(&mut conn).unwrap();
        drop(conn);

        let (audit, shutdown) = AuditWriter::spawn(db_path.clone(), None);
        for _ in 0..100 {
            audit.emit_ok(None, AuditOperation::MemoryRecall, None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(audit);
        shutdown.join().await;

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT ts_ms FROM audit_events
                 WHERE operation = 'memory.recall'
                 ORDER BY audit_id ASC",
            )
            .unwrap();
        let ts_seq: Vec<i64> = stmt
            .query_map([], |r| r.get::<_, i64>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(ts_seq.len(), 100);
        for w in ts_seq.windows(2) {
            assert!(
                w[1] >= w[0],
                "ts_ms must not decrease within an emit burst, got {} then {}",
                w[0],
                w[1]
            );
        }
    }
}
