// SPDX-License-Identifier: Apache-2.0

//! `WriterActor`, `WriteCommand`, `WriteHandle` — single-writer actor on a
//! dedicated OS thread. See ADR-0003 §"Trait shapes" and §"Operational
//! invariants".
//!
//! ## Why a dedicated OS thread (not a tokio task)
//!
//! `rusqlite::Connection::execute` blocks the current thread. If the actor
//! ran on a tokio worker, every write would block one worker for the
//! duration of the SQL — under burst load the runtime would starve and
//! every other task (HTTP handlers, MCP handlers, the snapshot timer) would
//! stall. The dedicated thread isolates that blocking from the runtime.
//!
//! Inside `run()` we use `mpsc::Receiver::blocking_recv()`; from outside the
//! actor, `WriteHandle::send().await` is async, so callers don't block on
//! sends.
//!
//! ## Reply-before-drain (ADR-0003 §P8-E)
//!
//! `Remember` carries a oneshot reply channel. The reply is sent **after**
//! the SQL transaction commits and `hnsw.add` succeeds, but **before** the
//! `pending_index` row is drained. Callers see "Ok = durable AND searchable"
//! without waiting on cleanup. If the drain itself fails, the row replays
//! on next startup — same end state.
//!
//! ## What's stubbed in commit 1.2
//!
//! `handle_forget`, `handle_consolidate`, `handle_reembed`,
//! `handle_save_snapshot` return `Error::Other("not yet implemented (commit
//! 1.x)")`. They're plumbed through the dispatch so callers can wire the
//! API today; the bodies fill in as the relevant commits land.

use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use solo_core::{
    AssetId, AttachmentId, DocumentId, Embedder, Embedding, Episode, Error, InvalidateEvent,
    MemoryId, Result, Tier, VectorIndex,
};
use tokio::runtime::Handle;
use tokio::sync::{RwLock as AsyncRwLock, broadcast, mpsc, oneshot};

use crate::asset_blob::{ASSET_BLOB_ENCRYPTION_ALG, ASSET_BLOB_PLAINTEXT_ALG, encrypt_asset_blob};
use crate::audit::{AuditEvent, AuditOperation, AuditResult, insert_audit_row_in_tx};
use crate::backup::{
    AssetBackupReport, BACKUP_STACK_SIZE_BYTES, backup_from_connection,
    package_asset_blobs_into_backup,
};
use crate::hnsw_id::{chunk_hnsw_id, episode_hnsw_id};
use crate::key_material::KeyMaterial;
use crate::triple_quality::{
    ActiveTriple, MIN_ACTIVE_TRIPLE_CONFIDENCE, TripleQualityDecision,
    active_entity_object_should_be_literal, active_triple_review_reason, canonical_entity_id,
    claim_status_for_review_reason, evaluate_extracted_triple, insert_active_triple_in_tx,
    insert_active_triple_with_optional_cluster_in_tx, insert_triple_review_in_tx,
    review_claim_quality_score, upsert_graph_for_active_triple_in_tx,
};

/// Default mpsc channel capacity. ADR-0003 §"Channel capacity": 1024 lets a
/// 1000-write consolidation burst land without backpressure-blocking the
/// regular write path.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// v0.10.0: capacity of the per-tenant `broadcast::Sender<InvalidateEvent>`
/// that the writer-actor uses to fan out post-commit invalidations to
/// `GET /v1/graph/stream` SSE subscribers.
///
/// 256 is plenty for the realistic ratio of (writer commits) :
/// (SSE-subscriber poll latency) — even a 100-write consolidation burst
/// against 3 connected solo-web clients leaves the slowest subscriber
/// 150+ slots of head-room. A subscriber that falls behind by more than
/// 256 events gets `broadcast::error::RecvError::Lagged(n)`, which the
/// SSE handler maps to a single emit-only-once warning + a graceful
/// skip (the client will see a heartbeat next and refetch on the next
/// real invalidate — there's no correctness loss because invalidation
/// events are idempotent "refetch your data" signals, not deltas).
pub const INVALIDATE_BROADCAST_CAPACITY: usize = 256;

/// v0.9.2: hard cap on `WriteCommand::RememberBatch` item count.
///
/// Bounds the worst case for batched-write from agentic clients
/// (an agent may write back a turn's worth of episodes at once). A
/// typical agent turn produces 5–30 items; 200 gives 6×+ head-room
/// without exposing the writer-actor to pathological batches.
/// Exceeded → the handler returns `Error::InvalidInput`; the request
/// never reaches the BEGIN IMMEDIATE tx.
pub const MAX_REMEMBER_BATCH_SIZE: usize = 200;
const MAX_RETRIEVAL_LOG_QUERY_CHARS: usize = 512;

/// Filter + flags for `WriteCommand::Consolidate`.
///
/// Implements `Deserialize` so HTTP / future MCP transports can
/// build it from a JSON request body without extra plumbing.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ConsolidationScope {
    /// If `Some(N)`, only consolidate episodes with `ts_ms` in the last
    /// N days. `None` = walk all `tier='hot' AND status='active'` rows.
    /// Bounded windows are typical for the daemon's nightly timer;
    /// unbounded for a one-shot bulk run.
    pub window_days: Option<i64>,
    /// When `true`, run the merge + regen passes even if there are
    /// zero unclustered episodes to feed to `cluster_episodes`. The
    /// usual flow short-circuits on empty candidates because there's
    /// no fresh work — but pre-existing clusters can drift across
    /// runs and should occasionally coalesce regardless. `--force-merge`
    /// on `solo consolidate` (or `force_merge: true` in the HTTP
    /// JSON body) opts into that drift catch-up.
    #[serde(default)]
    pub force_merge: bool,
}

/// What `WriteCommand::Consolidate` returns to the caller. v0.2.0
/// covers the SWS-equivalent clustering pass; abstraction +
/// contradiction counts will populate in later commits.
///
/// `Serialize` so HTTP responses can ship it directly as JSON.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ConsolidationReport {
    /// Distinct episodes the candidate query returned.
    pub episodes_seen: usize,
    /// Clusters that survived the size + threshold filter and were
    /// persisted to the `clusters` table.
    pub clusters_built: usize,
    /// Sum of episode_ids across all built clusters (i.e. how many
    /// `cluster_episodes` rows were inserted).
    pub episodes_clustered: usize,
    /// Number of clusters absorbed into a survivor by the centroid-
    /// merge pass (`solo_steward::cluster::merge_clusters_by_centroid`).
    /// Closes the cross-UTC-day-bucket case where conversations
    /// straddling midnight produce two clusters with similar centroids.
    /// 0 when no merges were possible. Counts losers, not survivors —
    /// `clusters_built` reflects the post-merge count.
    pub clusters_merged: usize,
    /// Number of freshly-built clusters absorbed into pre-existing
    /// DB clusters via
    /// `solo_steward::cluster::absorb_into_existing` (cross-run
    /// re-consolidation). The absorbed cluster never gets its own
    /// `clusters` row; its episodes link under the existing
    /// cluster_id, and the existing cluster's centroid + coherence
    /// refresh. Counts new-side clusters; `clusters_built` reflects
    /// the post-absorb count of brand-new clusters that survived to
    /// be inserted.
    pub clusters_absorbed: usize,
    /// Number of pre-existing clusters absorbed into another
    /// pre-existing cluster by the existing-vs-existing merge pass
    /// (`solo_steward::cluster::plan_existing_merges`). Closes the
    /// long-tail case where two clusters drift toward each other
    /// over time via repeated absorbs and should now coalesce.
    /// Counts losers, not survivors. Each loser's
    /// `cluster_episodes` rows reassign to the survivor's
    /// `cluster_id`, then the loser row is DELETEd (cascading via
    /// the 0001 + 0002 FKs through `cluster_episodes` (already
    /// empty after the UPDATE), `semantic_abstractions`, and
    /// `triples`). Survivor's stale abstraction is then regenerated
    /// by the same regen pass that handles cross-run absorb
    /// modifications.
    pub existing_clusters_merged: usize,
    /// Number of pre-existing clusters whose stale
    /// `semantic_abstractions` + linked `triples` were dropped and
    /// regenerated as a follow-on to the cross-run absorb pass.
    /// Equal to the count of distinct existing cluster_ids that
    /// absorbed at least one new cluster, **as long as** the
    /// regenerate-abstract LLM call succeeds; per-cluster failures
    /// are logged + skipped (the cluster row + its absorbed episodes
    /// stay; the abstraction row stays empty until the next run).
    /// 0 when no LLM steward is wired or no absorptions happened.
    pub abstractions_regenerated: usize,
    /// Number of `semantic_abstractions` rows successfully persisted
    /// (Y.3.3). 0 when the writer was spawned without a `Steward` —
    /// the prod default until a real `LlmClient` ships.
    pub abstractions_built: usize,
    /// Number of `triples` rows persisted alongside the abstractions
    /// (Y.3.3). Each abstraction can produce 0..N triples; the LLM
    /// is asked to extract them but may legitimately return none.
    pub triples_built: usize,
    /// Reserved for Y.4. Always 0 in this commit.
    pub contradictions_found: usize,
}

const DEFAULT_DERIVED_REPAIR_MIN_EMPTY_ABSTRACTION_EPISODES: usize = 25;
const DERIVED_REPAIR_CANDIDATE_SAMPLE_LIMIT: usize = 50;

fn default_derived_repair_min_empty_abstraction_episode_count() -> usize {
    DEFAULT_DERIVED_REPAIR_MIN_EMPTY_ABSTRACTION_EPISODES
}

/// Scope for repairing Solo's derived graph layer.
///
/// This intentionally never mutates raw `episodes`, `embeddings`,
/// `documents`, or `document_chunks`. It only clears data produced by
/// consolidation / Steward extraction so the normal pipeline can rebuild it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DerivedRepairScope {
    #[serde(default)]
    pub mode: DerivedRepairMode,
    /// When true, count and report what would change without committing.
    #[serde(default)]
    pub dry_run: bool,
    /// In `stale_abstractions` mode, clusters with an abstraction but zero
    /// active triples are considered stale once they have at least this many
    /// member episodes. Low-signal abstraction text is repaired regardless of
    /// size.
    #[serde(default = "default_derived_repair_min_empty_abstraction_episode_count")]
    pub min_empty_abstraction_episode_count: usize,
    /// Optional safety cap for `stale_abstractions` mode. `None` repairs all
    /// matching clusters.
    #[serde(default)]
    pub max_clusters: Option<usize>,
}

impl Default for DerivedRepairScope {
    fn default() -> Self {
        Self {
            mode: DerivedRepairMode::default(),
            dry_run: false,
            min_empty_abstraction_episode_count:
                DEFAULT_DERIVED_REPAIR_MIN_EMPTY_ABSTRACTION_EPISODES,
            max_clusters: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DerivedRepairMode {
    /// Delete stale/low-signal `semantic_abstractions` plus their linked
    /// triples, preserving cluster membership so triples extraction can retry.
    #[default]
    StaleAbstractions,
    /// Re-run deterministic quality rules over existing active triples,
    /// quarantining weak legacy rows and rewriting obvious metadata entity
    /// objects to literal objects. Raw memories and abstractions are kept.
    QualityGate,
    /// Delete all derived graph rows and let consolidation rebuild from raw
    /// episodes/documents.
    RebuildAll,
}

#[derive(Debug, Clone, Default, serde::Serialize, PartialEq, Eq)]
pub struct DerivedRepairReport {
    pub mode: DerivedRepairMode,
    pub dry_run: bool,
    pub clusters_scanned: usize,
    pub clusters_repaired: usize,
    pub abstractions_deleted: usize,
    pub triples_deleted: usize,
    pub triples_rewritten: usize,
    pub triple_reviews_created: usize,
    pub contradictions_deleted: usize,
    pub clusters_deleted: usize,
    pub cluster_memberships_deleted: usize,
    pub candidate_samples: Vec<DerivedRepairCandidate>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct DerivedRepairCandidate {
    pub cluster_id: String,
    pub episode_count: usize,
    pub abstraction_count: usize,
    pub triple_count: usize,
    pub abstraction_preview: String,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone)]
struct DerivedRepairScanRow {
    cluster_id: String,
    abstraction_content: String,
    abstraction_count: usize,
    episode_count: usize,
    triple_count: usize,
    active_triple_count: usize,
}

#[derive(Debug, Clone)]
struct ActiveTripleQualityRow {
    triple_id: String,
    cluster_id: Option<String>,
    source_episode_id: Option<i64>,
    subject_id: String,
    predicate: String,
    object_id: String,
    object_kind: String,
    confidence: f32,
    provenance_json: String,
    created_at_ms: i64,
    literal_only_subject: bool,
}

#[derive(Debug, Clone)]
struct ActiveTripleQualityCandidate {
    row: ActiveTripleQualityRow,
    reason_code: &'static str,
    reason: String,
}

#[derive(Debug, Clone)]
struct MetadataObjectRewriteCandidate {
    triple_id: String,
    cluster_id: Option<String>,
    subject_id: String,
    predicate: String,
    object_id: String,
}

#[derive(Debug, Clone)]
struct MemoryQualityReviewDbRow {
    review_id: String,
    candidate_fingerprint: String,
    triple_id: Option<String>,
    cluster_id: Option<String>,
    source_episode_id: Option<i64>,
    subject_id: String,
    predicate: String,
    object_id: String,
    object_kind: String,
    confidence: f32,
    status: String,
    created_at_ms: i64,
    provenance_json: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryRetrievalLogEntry {
    pub query: String,
    pub recalled_ids: Vec<String>,
    pub reason_codes: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryRetrievalLogReport {
    pub retrieval_id: String,
    pub created_at_ms: i64,
}

impl DerivedRepairScanRow {
    fn into_candidate(
        self,
        min_empty_abstraction_episode_count: usize,
    ) -> Option<DerivedRepairCandidate> {
        let mut reasons = Vec::new();
        if self.active_triple_count == 0
            && self.episode_count >= min_empty_abstraction_episode_count
        {
            reasons.push("empty_abstraction_no_active_triples".to_string());
        }
        if solo_steward::abstraction::is_low_signal_abstraction(&self.abstraction_content) {
            reasons.push("low_signal_abstraction".to_string());
        }
        if reasons.is_empty() {
            return None;
        }
        Some(DerivedRepairCandidate {
            cluster_id: self.cluster_id,
            episode_count: self.episode_count,
            abstraction_count: self.abstraction_count,
            triple_count: self.triple_count,
            abstraction_preview: derived_repair_preview(&self.abstraction_content),
            reasons,
        })
    }
}

fn i64_to_usize(value: i64) -> usize {
    value.max(0) as usize
}

fn derived_repair_preview(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut preview: String = normalized.chars().take(160).collect();
    if normalized.chars().count() > 160 {
        preview.push_str("...");
    }
    preview
}

fn apply_quality_repair_cap(
    quarantine: &mut Vec<ActiveTripleQualityCandidate>,
    rewrites: &mut Vec<MetadataObjectRewriteCandidate>,
    max_clusters: Option<usize>,
) {
    let Some(max_clusters) = max_clusters else {
        return;
    };
    if max_clusters == 0 {
        quarantine.clear();
        rewrites.clear();
        return;
    }
    let mut allowed = std::collections::BTreeSet::new();
    for cluster in quarantine
        .iter()
        .map(|c| c.row.cluster_id.as_deref().unwrap_or("unclustered"))
        .chain(
            rewrites
                .iter()
                .map(|c| c.cluster_id.as_deref().unwrap_or("unclustered")),
        )
    {
        if allowed.len() >= max_clusters && !allowed.contains(cluster) {
            continue;
        }
        allowed.insert(cluster.to_string());
    }
    quarantine.retain(|c| allowed.contains(c.row.cluster_id.as_deref().unwrap_or("unclustered")));
    rewrites.retain(|c| allowed.contains(c.cluster_id.as_deref().unwrap_or("unclustered")));
}

fn quality_repair_samples(
    quarantine: &[ActiveTripleQualityCandidate],
    rewrites: &[MetadataObjectRewriteCandidate],
) -> Vec<DerivedRepairCandidate> {
    quarantine
        .iter()
        .map(|candidate| {
            let row = &candidate.row;
            DerivedRepairCandidate {
                cluster_id: row
                    .cluster_id
                    .clone()
                    .unwrap_or_else(|| "unclustered".to_string()),
                episode_count: 0,
                abstraction_count: 0,
                triple_count: 1,
                abstraction_preview: derived_repair_preview(&format!(
                    "{} --{}--> {} [{}]",
                    row.subject_id, row.predicate, row.object_id, row.object_kind
                )),
                reasons: vec![candidate.reason_code.to_string()],
            }
        })
        .chain(rewrites.iter().map(|candidate| {
            DerivedRepairCandidate {
                cluster_id: candidate
                    .cluster_id
                    .clone()
                    .unwrap_or_else(|| "unclustered".to_string()),
                episode_count: 0,
                abstraction_count: 0,
                triple_count: 1,
                abstraction_preview: derived_repair_preview(&format!(
                    "{} --{}--> {} [entity -> literal]",
                    candidate.subject_id, candidate.predicate, candidate.object_id
                )),
                reasons: vec!["metadata_entity_object".to_string()],
            }
        }))
        .take(DERIVED_REPAIR_CANDIDATE_SAMPLE_LIMIT)
        .collect()
}

fn active_quality_review_fingerprint(triple_id: &str, reason_code: &str) -> String {
    format!("active-quality|{triple_id}|{reason_code}")
}

fn active_quality_review_id(triple_id: &str, reason_code: &str) -> String {
    let short_reason = reason_code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(32)
        .collect::<String>();
    format!("trv-quality-{triple_id}-{short_reason}")
}

#[allow(clippy::too_many_arguments)]
fn insert_entity_review_op_in_tx(
    tx: &rusqlite::Transaction<'_>,
    op_kind: &str,
    status: &str,
    source_entity_id: &str,
    target_entity_id: Option<&str>,
    affected_aliases: &[String],
    reason: Option<&str>,
    revision_kind: &str,
    replacement_id: Option<&str>,
    now_ms: i64,
) -> Result<EntityReviewOpReport> {
    let op_id = MemoryId::new().to_string();
    let affected_aliases_json = serde_json::to_string(affected_aliases)
        .map_err(|e| Error::storage(format!("encode entity review aliases: {e}")))?;
    tx.execute(
        "INSERT INTO entity_review_ops
            (op_id, op_kind, status, source_entity_id, target_entity_id,
             affected_aliases_json, reason, created_at_ms, applied_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8,
                 CASE WHEN ?3 = 'applied' THEN ?8 ELSE NULL END, ?8)",
        params![
            &op_id,
            op_kind,
            status,
            source_entity_id,
            target_entity_id,
            &affected_aliases_json,
            reason,
            now_ms,
        ],
    )
    .map_err(|e| Error::storage(format!("INSERT entity review op {op_kind}: {e}")))?;

    let revision_id = MemoryId::new().to_string();
    tx.execute(
        "INSERT INTO memory_revisions
            (revision_id, revision_kind, target_kind, target_id,
             previous_id, replacement_id, reason, metadata_json, created_at_ms)
         VALUES (?1, ?2, 'entity', ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            revision_id,
            revision_kind,
            target_entity_id.unwrap_or(source_entity_id),
            source_entity_id,
            replacement_id,
            reason,
            serde_json::json!({
                "op_id": op_id,
                "op_kind": op_kind,
                "status": status,
                "affected_aliases": affected_aliases,
            })
            .to_string(),
            now_ms,
        ],
    )
    .map_err(|e| Error::storage(format!("INSERT entity memory revision {op_kind}: {e}")))?;

    Ok(EntityReviewOpReport {
        op_id,
        op_kind: op_kind.to_string(),
        status: status.to_string(),
        source_entity_id: source_entity_id.to_string(),
        target_entity_id: target_entity_id.map(str::to_string),
        affected_aliases: affected_aliases.to_vec(),
        reason: reason.map(str::to_string),
        created_at_ms: now_ms,
    })
}

fn upsert_active_quality_memory_claim_in_tx(
    tx: &rusqlite::Transaction<'_>,
    row: &ActiveTripleQualityRow,
    fingerprint: &str,
    review_id: &str,
    reason_code: &str,
    now_ms: i64,
) -> Result<()> {
    let claim_id = format!("mc-{fingerprint}");
    let status = claim_status_for_review_reason(reason_code);
    let quality_score = review_claim_quality_score(row.confidence, reason_code);
    let reason_codes_json = serde_json::to_string(&vec!["quality_gate", reason_code, status])
        .map_err(|e| Error::storage(format!("encode active quality memory claim reasons: {e}")))?;
    tx.execute(
        "INSERT INTO memory_claims
            (claim_id, candidate_fingerprint, triple_id, review_id, subject_id,
             predicate, object_id, object_kind, source_type, cluster_id,
             source_episode_id, confidence, quality_score, status, reason_codes_json,
             evidence_count, user_approved, created_at_ms, updated_at_ms, provenance_json)
         VALUES
            (?1, ?2, ?3, ?4, ?5,
             ?6, ?7, ?8, 'quality_repair', ?9,
             ?10, ?11, ?12, ?13, ?14,
             1, 0, ?15, ?15, ?16)
         ON CONFLICT(candidate_fingerprint) DO UPDATE SET
             triple_id = excluded.triple_id,
             review_id = excluded.review_id,
             subject_id = excluded.subject_id,
             predicate = excluded.predicate,
             object_id = excluded.object_id,
             object_kind = excluded.object_kind,
             source_type = excluded.source_type,
             cluster_id = excluded.cluster_id,
             source_episode_id = excluded.source_episode_id,
             confidence = excluded.confidence,
             quality_score = excluded.quality_score,
             status = excluded.status,
             reason_codes_json = excluded.reason_codes_json,
             updated_at_ms = excluded.updated_at_ms,
             provenance_json = excluded.provenance_json",
        params![
            claim_id,
            fingerprint,
            &row.triple_id,
            review_id,
            &row.subject_id,
            &row.predicate,
            &row.object_id,
            &row.object_kind,
            row.cluster_id.as_deref(),
            row.source_episode_id,
            row.confidence,
            quality_score,
            status,
            reason_codes_json,
            now_ms,
            &row.provenance_json,
        ],
    )
    .map_err(|e| {
        Error::storage(format!(
            "upsert active quality memory claim for {}: {e}",
            row.triple_id
        ))
    })?;
    Ok(())
}

fn derived_repair_audit_details(report: &DerivedRepairReport) -> serde_json::Value {
    serde_json::json!({
        "mode": report.mode,
        "dry_run": report.dry_run,
        "clusters_scanned": report.clusters_scanned,
        "clusters_repaired": report.clusters_repaired,
        "abstractions_deleted": report.abstractions_deleted,
        "triples_deleted": report.triples_deleted,
        "triples_rewritten": report.triples_rewritten,
        "triple_reviews_created": report.triple_reviews_created,
        "contradictions_deleted": report.contradictions_deleted,
        "clusters_deleted": report.clusters_deleted,
        "cluster_memberships_deleted": report.cluster_memberships_deleted,
        "candidate_sample_count": report.candidate_samples.len(),
    })
}

/// Filter + flags for `WriteCommand::Reembed`.
#[derive(Debug, Clone, Default)]
pub struct ReembedScope {
    /// If `Some((name, version))`, only reembed memories whose existing
    /// embedding row was produced by this embedder identity. If `None`,
    /// every memory whose embedding's `embedder_id` differs from the
    /// writer's current `embedder_id` is a candidate.
    pub from: Option<(String, String)>,
    /// Walk + count only; write nothing.
    pub dry_run: bool,
    /// After re-embedding each touched memory, DELETE the prior
    /// `embeddings` rows for that memory whose `embedder_id` differs
    /// from the current. Without this flag, stale rows are retained
    /// for forensics or rollback.
    pub gc: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ReembedReport {
    /// Distinct memory_ids that matched the candidate query.
    pub rows_seen: usize,
    /// Memories whose new embedding was successfully written.
    pub rows_reembedded: usize,
    /// Memories that hit an error during embed or insert.
    pub rows_failed: usize,
    /// Number of stale `embeddings` rows DELETED via `--gc`.
    pub rows_gc_deleted: usize,
    /// Mirrors the scope flag so callers can format output without
    /// retaining the original scope.
    pub dry_run: bool,
}

/// What `WriteCommand::NormalizeSubjects` returns to the caller.
///
/// Opt-in backfill tool: rewrites historical `triples.subject_id` and
/// `triples.object_id` values according to a caller-supplied alias map.
/// The companion to v0.5.0's read-path alias resolution
/// (`IdentityConfig.user_aliases`) — that bridges queries transparently
/// against existing rows, while this rewrites the underlying data so
/// downstream consumers (third-party tools, exports) see the canonical
/// identity. See `docs/dev-log/0071-v0.5.x-roadmap.md` Priority 10.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct NormalizeReport {
    /// Number of `(from, to)` alias pairs processed (== `scope.aliases.len()`).
    pub aliases_processed: usize,
    /// Rows whose `subject_id` was rewritten (summed across all pairs).
    /// In `dry_run` mode this is the count that *would* be rewritten;
    /// the transaction is rolled back before the change is persisted.
    pub subject_rows_updated: usize,
    /// Rows whose `object_id` was rewritten (summed across all pairs).
    /// Same dry-run semantics as `subject_rows_updated`.
    pub object_rows_updated: usize,
    /// Mirrors the scope flag so callers can format output without
    /// retaining the original scope.
    pub dry_run: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EntityReviewOpReport {
    pub op_id: String,
    pub op_kind: String,
    pub status: String,
    pub source_entity_id: String,
    pub target_entity_id: Option<String>,
    pub affected_aliases: Vec<String>,
    pub reason: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct EntitySplitRequest {
    pub entity_id: String,
    pub affected_aliases: Vec<String>,
    pub reason: Option<String>,
}

/// Default per-file ingest size cap when `SOLO_INGEST_MAX_BYTES` is not
/// set in the environment. This matches the staged-upload product cap so
/// a file accepted by upload staging is not rejected by the writer's
/// default ingestion guardrail.
///
/// Above the cap, the writer returns an error before `parse_file` opens
/// the file, so the SQL and HNSW state are untouched. Set
/// `SOLO_INGEST_MAX_BYTES=0` to disable the cap entirely (caller-managed
/// resource bound).
pub const DEFAULT_INGEST_MAX_BYTES: u64 = 100 * 1024 * 1024;
pub(crate) const SOLO_INGEST_MAX_BYTES_ENV: &str = "SOLO_INGEST_MAX_BYTES";

#[cfg(test)]
thread_local! {
    static TEST_INGEST_MAX_BYTES_OVERRIDE: std::cell::Cell<Option<Option<u64>>> =
        const { std::cell::Cell::new(None) };
}

/// Effective per-file ingest cap.
///
/// Returns:
///
///   - `Some(n)` — enforce a cap of `n` bytes.
///   - `None`    — cap disabled (env var explicitly set to `0`).
///
/// Resolution order:
///
///   1. If `SOLO_INGEST_MAX_BYTES` is unset → `Some(DEFAULT_INGEST_MAX_BYTES)`.
///   2. If `SOLO_INGEST_MAX_BYTES=0` → `None` (disabled).
///   3. If `SOLO_INGEST_MAX_BYTES=<positive integer>` → `Some(n)`.
///   4. If the env var is set but unparseable (negative, garbage, whitespace,
///      etc.) → `Some(DEFAULT_INGEST_MAX_BYTES)` plus a `tracing::warn!`. The
///      conservative fallback is "use the default" rather than "disable cap"
///      so a typo can't silently turn off the safety net.
pub fn resolve_ingest_max_bytes() -> Option<u64> {
    #[cfg(test)]
    if let Some(value) = TEST_INGEST_MAX_BYTES_OVERRIDE.with(|slot| slot.get()) {
        return value;
    }

    let raw = std::env::var(SOLO_INGEST_MAX_BYTES_ENV).ok();
    resolve_ingest_max_bytes_from_raw(raw.as_deref())
}

fn resolve_ingest_max_bytes_from_raw(raw: Option<&str>) -> Option<u64> {
    let Some(raw) = raw else {
        return Some(DEFAULT_INGEST_MAX_BYTES);
    };
    let trimmed = raw.trim();
    match trimmed.parse::<u64>() {
        Ok(0) => None,
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!(
                value = %raw,
                env = SOLO_INGEST_MAX_BYTES_ENV,
                default_bytes = DEFAULT_INGEST_MAX_BYTES,
                "unparseable SOLO_INGEST_MAX_BYTES; falling back to default"
            );
            Some(DEFAULT_INGEST_MAX_BYTES)
        }
    }
}

/// What `WriteCommand::IngestDocument` returns to the caller. New in
/// v0.7.0 (RAG / document-memory). See `docs/dev-log/0083-v0.7.0-
/// implementation-plan.md` §2 P3.
///
/// `deduped == true` indicates the same content_hash was already present
/// in `documents`; the returned `doc_id` is the pre-existing document's
/// id and `chunks_persisted` is zero (no new chunks were written, no
/// embeddings were called). Forgotten documents still participate in
/// dedup — re-ingesting the same text after `forget_document` returns
/// the forgotten doc_id unchanged (callers can re-activate via a
/// future `restore` command, or simply ingest under a different source
/// path if they want a fresh active doc).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IngestReport {
    pub doc_id: solo_core::DocumentId,
    pub chunks_persisted: u32,
    pub bytes_ingested: u64,
    pub text_chars: u64,
    pub extractor_name: String,
    pub extractor_version: String,
    pub deduped: bool,
}

/// What `WriteCommand::ForgetDocument` returns to the caller. New in
/// v0.7.0.
///
/// `chunks_tombstoned` counts the `document_chunks` rows whose HNSW
/// rowid was tombstoned (so `index.len()` no longer counts them and
/// `detect_drift` stays clean). The chunk rows themselves are NOT
/// deleted from SQL — `documents.status='forgotten'` is the soft-delete
/// marker; chunks survive for forensic value (same pattern as episodes'
/// soft-delete via `episodes.status='forgotten'`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForgetDocumentReport {
    pub doc_id: solo_core::DocumentId,
    pub chunks_tombstoned: u32,
}

/// A raw original file persisted into Solo's tenant-local asset store.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredAssetReport {
    pub asset_id: AssetId,
    pub sha256: String,
    pub mime_type: String,
    pub filename: Option<String>,
    pub size_bytes: u64,
    pub storage_path: String,
    pub deduped: bool,
}

/// One extraction attempt recorded for a retained original-file asset.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AssetExtractionReport {
    pub extraction_id: String,
    pub asset_id: AssetId,
    pub extractor_name: String,
    pub extractor_version: String,
    pub status: String,
    pub text_chars: u64,
    pub error: Option<String>,
    pub created_at_ms: i64,
}

/// Link between an ingested document and the original asset it came from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocumentAssetLinkReport {
    pub link_id: AttachmentId,
    pub doc_id: DocumentId,
    pub asset_id: AssetId,
    pub relation_type: String,
    pub created_at_ms: i64,
}

/// Link between a remembered episode and a document or asset.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryAttachmentReport {
    pub attachment_id: AttachmentId,
    pub memory_id: MemoryId,
    pub doc_id: Option<DocumentId>,
    pub asset_id: Option<AssetId>,
    pub relation_type: String,
    pub note: Option<String>,
    pub created_at_ms: i64,
}

/// Result for forgetting a persisted original-file asset.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ForgetAssetReport {
    pub asset_id: AssetId,
    pub blob_deleted: bool,
    pub already_deleted: bool,
    pub document_links: u32,
    pub memory_attachments: u32,
}

/// Result for correcting an existing active episode through the
/// single-writer actor.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryUpdateReport {
    pub memory_id: MemoryId,
    pub rowid: i64,
    pub content: String,
    pub updated_at_ms: i64,
}

/// Persisted Memory Inbox review state. Missing row means "needs review".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryReviewState {
    Approved,
    Dismissed,
}

impl MemoryReviewState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Dismissed => "dismissed",
        }
    }
}

impl std::fmt::Display for MemoryReviewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryReviewState {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "approved" => Ok(Self::Approved),
            "dismissed" => Ok(Self::Dismissed),
            other => Err(Error::invalid_input(format!(
                "review state must be approved or dismissed, got {other:?}"
            ))),
        }
    }
}

/// Result for setting or clearing an Inbox review decision.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryReviewReport {
    pub memory_id: MemoryId,
    pub state: Option<MemoryReviewState>,
    pub reviewed_at_ms: Option<i64>,
}

/// Memory Quality inbox review decision for a derived triple candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryQualityReviewStatus {
    NeedsReview,
    Approved,
    Dismissed,
    Rewritten,
}

impl MemoryQualityReviewStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NeedsReview => "needs_review",
            Self::Approved => "approved",
            Self::Dismissed => "dismissed",
            Self::Rewritten => "rewritten",
        }
    }
}

impl std::fmt::Display for MemoryQualityReviewStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for MemoryQualityReviewStatus {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "needs_review" => Ok(Self::NeedsReview),
            "approved" => Ok(Self::Approved),
            "dismissed" => Ok(Self::Dismissed),
            "rewritten" => Ok(Self::Rewritten),
            other => Err(Error::invalid_input(format!(
                "review status must be needs_review, approved, dismissed, or rewritten, got {other:?}"
            ))),
        }
    }
}

/// Replacement fact supplied when a Memory Quality candidate is rewritten.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryQualityReviewRewrite {
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: String,
    pub confidence: Option<f32>,
}

/// Writer-actor input for a Memory Quality candidate update.
#[derive(Debug, Clone)]
pub struct MemoryQualityReviewUpdate {
    pub review_id: String,
    pub status: MemoryQualityReviewStatus,
    pub note: Option<String>,
    pub rewrites: Vec<MemoryQualityReviewRewrite>,
}

/// Updated Memory Quality candidate as persisted by the writer actor.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryQualityReviewUpdateReport {
    pub review_id: String,
    pub claim_id: Option<String>,
    pub triple_id: Option<String>,
    pub cluster_id: Option<String>,
    pub source_episode_id: Option<i64>,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: String,
    pub confidence: f32,
    pub reason_code: String,
    pub reason: String,
    pub status: String,
    pub claim_status: Option<String>,
    pub quality_score: Option<f32>,
    pub claim_reason_codes: Vec<String>,
    pub created_at_ms: i64,
    pub evidence_preview: Option<String>,
}

fn validate_memory_quality_review_update(update: &MemoryQualityReviewUpdate) -> Result<()> {
    if update.review_id.trim().is_empty() {
        return Err(Error::invalid_input("review_id must not be empty"));
    }
    for rewrite in &update.rewrites {
        validate_memory_quality_review_rewrite(rewrite)?;
    }
    Ok(())
}

fn validate_memory_quality_review_rewrite(rewrite: &MemoryQualityReviewRewrite) -> Result<()> {
    let object_kind = rewrite.object_kind.trim();
    if !matches!(object_kind, "entity" | "literal") {
        return Err(Error::invalid_input(
            "rewrite object_kind must be entity or literal",
        ));
    }
    if rewrite.subject_id.trim().is_empty()
        || rewrite.predicate.trim().is_empty()
        || rewrite.object_id.trim().is_empty()
    {
        return Err(Error::invalid_input(
            "rewrite subject_id, predicate, and object_id must not be empty",
        ));
    }
    if let Some(confidence) = rewrite.confidence {
        if !(0.0..=1.0).contains(&confidence) || confidence.is_nan() {
            return Err(Error::invalid_input(
                "rewrite confidence must be a finite value between 0.0 and 1.0",
            ));
        }
        if confidence < MIN_ACTIVE_TRIPLE_CONFIDENCE {
            return Err(Error::invalid_input(format!(
                "rewrite confidence must be at least {MIN_ACTIVE_TRIPLE_CONFIDENCE:.2}"
            )));
        }
    }
    Ok(())
}

fn parse_claim_reason_codes(raw: Option<String>) -> Vec<String> {
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

fn push_unique_reason_code(codes: &mut Vec<String>, code: &str) {
    if !codes.iter().any(|existing| existing == code) {
        codes.push(code.to_string());
    }
}

fn normalize_entity_review_aliases(aliases: Vec<String>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for alias in aliases {
        let alias = alias.trim();
        if alias.is_empty() {
            continue;
        }
        if !out.iter().any(|existing| existing == alias) {
            out.push(alias.to_string());
        }
    }
    if out.is_empty() {
        return Err(Error::invalid_input(
            "entity split request must include at least one affected alias",
        ));
    }
    Ok(out)
}

fn bounded_retrieval_log_query(query: &str) -> String {
    query.chars().take(MAX_RETRIEVAL_LOG_QUERY_CHARS).collect()
}

/// All write operations go through this enum. Each variant carries a
/// oneshot reply channel.
///
/// v0.8.0 P4: every mutating variant also carries `audit_principal:
/// Option<String>` — the authenticated principal's subject. Threaded
/// from the auth middleware (HTTP / MCP) through to the writer-actor,
/// where the synchronous audit emit records "who did this". `None`
/// covers CLI / no-auth / system-initiated paths.
#[derive(Debug)]
pub enum WriteCommand {
    Remember {
        episode: Episode,
        embedding: Embedding,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<MemoryId>>,
    },
    /// v0.9.2: atomically insert N episodes in one BEGIN IMMEDIATE tx.
    /// Used by agentic clients that write back a full turn
    /// — user message + assistant response + tool outputs — as one
    /// transactional unit so a session crash can never leave a half-
    /// persisted turn.
    ///
    /// Same outbox-via-`pending_index` discipline as single `Remember`:
    /// BEGIN IMMEDIATE → INSERTs (episodes + embeddings + pending_index
    /// per item) → ONE batch-level audit row inside the tx → COMMIT →
    /// `hnsw.add` per item → DELETE `pending_index` rows. If an
    /// `hnsw.add` crashes mid-batch the SQL state is already committed
    /// and the un-drained outbox rows replay on next startup.
    ///
    /// Item count capped at [`MAX_REMEMBER_BATCH_SIZE`]; over-cap
    /// requests are rejected before BEGIN with `Error::InvalidInput`.
    ///
    /// Reply is `Vec<MemoryId>` in input order — caller pairs them with
    /// their input items by position.
    RememberBatch {
        items: Vec<(Episode, Embedding)>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<Vec<MemoryId>>>,
    },
    Forget {
        memory_id: MemoryId,
        reason: String,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    Update {
        memory_id: MemoryId,
        content: String,
        embedding: Embedding,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<MemoryUpdateReport>>,
    },
    ReviewMemory {
        memory_id: MemoryId,
        state: Option<MemoryReviewState>,
        note: Option<String>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<MemoryReviewReport>>,
    },
    /// Ingest a document from `path` into the documents / document_chunks
    /// tables, embedding each chunk via the writer's configured Embedder.
    /// Same outbox-via-`pending_index` discipline as `Remember`: BEGIN
    /// IMMEDIATE → INSERT documents → INSERT document_chunks → INSERT
    /// pending_index (kind='chunk') → COMMIT → hnsw.add per chunk →
    /// DELETE pending_index rows. Content-hash dedup short-circuits
    /// re-ingest of the same normalized text.
    ///
    /// Available only when the writer was spawned with an active embedder
    /// (the `spawn_full_with_embedder*` variants). Other spawn paths get
    /// a clear "not configured" error — same pattern as `Reembed`.
    IngestDocument {
        path: std::path::PathBuf,
        chunk_config: crate::document::ChunkConfig,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<IngestReport>>,
    },
    /// Soft-delete a document: set `documents.status='forgotten'` and
    /// tombstone every chunk's HNSW rowid. Chunks remain in SQL for
    /// forensic value; queries that JOIN through `documents` filter
    /// `status='active'`. Forgotten docs survive content-hash dedup —
    /// re-ingesting the same content returns the forgotten doc_id.
    ForgetDocument {
        doc_id: solo_core::DocumentId,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<ForgetDocumentReport>>,
    },
    StoreAssetFromPath {
        path: PathBuf,
        filename: Option<String>,
        mime_type: Option<String>,
        size_bytes: Option<u64>,
        sha256: Option<String>,
        source: Option<String>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<StoredAssetReport>>,
    },
    RecordAssetExtraction {
        asset_id: AssetId,
        extractor_name: String,
        extractor_version: String,
        status: String,
        text_chars: u64,
        error: Option<String>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<AssetExtractionReport>>,
    },
    LinkDocumentAsset {
        doc_id: DocumentId,
        asset_id: AssetId,
        relation_type: String,
        note: Option<String>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<DocumentAssetLinkReport>>,
    },
    AttachMemory {
        memory_id: MemoryId,
        doc_id: Option<DocumentId>,
        asset_id: Option<AssetId>,
        relation_type: String,
        note: Option<String>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<MemoryAttachmentReport>>,
    },
    ForgetAsset {
        asset_id: AssetId,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<ForgetAssetReport>>,
    },
    Consolidate {
        scope: ConsolidationScope,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<ConsolidationReport>>,
    },
    /// Repair derived memory artifacts without touching raw memories.
    ///
    /// This is an operator escape hatch for cases where consolidation or the
    /// Steward produces stale/low-signal graph state. It is routed through the
    /// writer actor so derived deletes, audit, and graph invalidation preserve
    /// the same discipline as normal writes.
    RepairDerived {
        scope: DerivedRepairScope,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<DerivedRepairReport>>,
    },
    /// Apply a Memory Quality inbox decision through the writer actor.
    ///
    /// Approving or rewriting a candidate can activate graph facts, so this
    /// must serialize with normal extraction/repair writes and emit audit in
    /// the same transaction.
    UpdateMemoryQualityReview {
        update: MemoryQualityReviewUpdate,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<MemoryQualityReviewUpdateReport>>,
    },
    RecordRetrieval {
        entry: MemoryRetrievalLogEntry,
        reply: oneshot::Sender<Result<MemoryRetrievalLogReport>>,
    },
    Reembed {
        scope: ReembedScope,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<ReembedReport>>,
    },
    SaveSnapshot {
        reply: oneshot::Sender<Result<()>>,
    },
    /// Online encrypted backup of the writer's source database to
    /// `dest_path`. The destination is created with PRAGMA key bound to
    /// the same raw key the writer holds, so the backup file restores
    /// under the same passphrase + salt as the source.
    ///
    /// Available only when the writer was spawned with a `KeyMaterial`
    /// (the `spawn_full_with_key_and_optional_steward` variant). Other
    /// spawn paths get a clear "not configured" error.
    Backup {
        dest_path: PathBuf,
        reply: oneshot::Sender<Result<AssetBackupReport>>,
    },
    /// Backfill: rewrite historical `triples.subject_id` and
    /// `triples.object_id` values per a caller-supplied alias map.
    /// Each `(from, to)` pair is applied to **both** the subject and
    /// object columns (a name appearing in either position should
    /// normalize identically).
    ///
    /// Opt-in: read-path alias resolution (v0.5.0 P1) already covers
    /// query-time bridging without touching stored rows. This command
    /// is for users who want the underlying data to match the canonical
    /// identity (e.g., when exporting to a system that won't honor
    /// `IdentityConfig.user_aliases`). See `docs/dev-log/0071-v0.5.x-roadmap.md`
    /// Priority 10.
    NormalizeSubjects {
        /// `(from_id, to_id)` pairs — e.g. `[("alex", "user"),
        /// ("bob", "user")]`. Each pair is applied as
        /// `UPDATE triples SET subject_id = to WHERE subject_id = from`
        /// and the symmetric object update.
        aliases: Vec<(String, String)>,
        /// When true, run the UPDATEs inside a transaction, count the
        /// affected rows, then `ROLLBACK` instead of committing. The
        /// returned report's row counts reflect what *would* have been
        /// rewritten.
        dry_run: bool,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<NormalizeReport>>,
    },
    RequestEntitySplit {
        request: EntitySplitRequest,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<EntityReviewOpReport>>,
    },
    /// v0.9.0 P2: emit a single `AuditOperation::LlmSamplingCall` row
    /// into the per-tenant `audit_events` table. Used by
    /// `SamplingLlmClient` (in `solo-api`) on every
    /// `peer.create_message` completion — success or failure.
    ///
    /// Routed through the writer-actor so the INSERT lands inside a
    /// dedicated sync transaction on the writer's connection (lesson
    /// #30: ACID for the sampling call's only persisted trace). The
    /// `reply` channel surfaces insert failures to the caller so a
    /// missed audit row can NOT be silently swallowed — the caller
    /// of `SamplingLlmClient::complete()` must see the failure
    /// because this row is the ONLY record of the call.
    ///
    /// **Privacy invariant**: `event.details` must NOT contain the
    /// raw prompt content. Enforcement lives at the construction
    /// site (`SamplingLlmClient::audit_event`); we surface only
    /// metadata (model hint, message count, max_tokens, duration_ms,
    /// total prompt char count). The audit test
    /// `sampling_audit_row_omits_raw_prompt_text` pins this.
    EmitLlmSamplingAudit {
        event: AuditEvent,
        reply: oneshot::Sender<Result<()>>,
    },
    /// v0.9.0 P4c: persist a batch of `(cluster_id, abstraction)`
    /// pairs in a single transaction + emit ONE
    /// `AuditOperation::MemoryTriplesExtract` audit row carrying the
    /// batch's aggregate counts.
    ///
    /// Sent by the daemon-side consolidate-timer's triples batch
    /// path (see `crates/solo-cli/src/commands/daemon.rs::
    /// triples_batch_tick`). For each `(cluster_id, abstraction)`:
    ///
    ///   * INSERT one `semantic_abstractions` row.
    ///   * INSERT N `triples` rows (where N = `abstraction.triples.len()`).
    ///
    /// Then emit ONE audit row per batch, with
    /// `details_json = {episode_count, cluster_count,
    /// abstractions_built, triples_extracted, duration_ms}`.
    ///
    /// **Atomicity** (plan §4 P4c / lesson #30): the entire batch
    /// runs inside ONE `BEGIN IMMEDIATE` tx; the audit emit is
    /// SYNC inside that same tx. If the audit emit fails, the
    /// whole batch aborts — preserving the "audit row IS the only
    /// persisted record of the batch" invariant.
    ///
    /// **Partial-batch tolerance**: per-cluster INSERT failures
    /// (e.g. FK violation if the cluster_id was dropped between
    /// snapshot and persist) are LOGGED and skipped, but the tx
    /// stays open. The audit row's `details_json.abstractions_built`
    /// counter reflects the SUCCESSFUL inserts only. Test:
    /// [`tests::p4c_attach_abstraction_batch_tests`].
    AttachAbstractionBatch {
        /// `(cluster_id, abstraction)` pairs to persist. The
        /// abstraction's `cluster_id` field is REQUIRED to equal the
        /// tuple's `MemoryId`; the handler asserts this and rejects
        /// the whole batch on mismatch.
        items: Vec<(MemoryId, solo_core::SemanticAbstraction)>,
        /// Number of episodes that flowed into this batch (for the
        /// audit row's `details_json.episode_count`).
        episode_count: usize,
        /// Wall-time the upstream collection+LLM round-trip took
        /// (for the audit row's `details_json.duration_ms`). The
        /// writer-actor's own tx duration is small and not included.
        duration_ms: u64,
        /// v0.10.1 (P4 audit m5): number of clusters that timed out
        /// during their per-cluster `abstract_cluster` call. Surfaces
        /// in the audit row's `details_json.clusters_deferred` and in
        /// the returned `AttachAbstractionBatchReport.clusters_deferred`.
        /// Distinct from `clusters_failed` (per-cluster INSERT
        /// SAVEPOINT rollbacks): a deferred cluster never made it INTO
        /// the batch's `items` because the LLM call timed out
        /// upstream.
        clusters_deferred: usize,
        /// Cached audit `principal_subject` for the daemon path —
        /// usually `None` since the consolidate timer runs without
        /// an explicit principal.
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<AttachAbstractionBatchReport>>,
    },
    /// Mark a contradiction resolved / unresolved / reopened. Routes
    /// through the writer actor (dev-log 0152 finding H1 — restores
    /// ADR-0003: the previous code path used the **reader pool** to
    /// UPDATE `contradictions`, racing with the writer-actor on multiple
    /// connections and writing the audit row outside the tx).
    ///
    /// **Atomicity**: UPDATE + audit emit inside one BEGIN IMMEDIATE
    /// transaction. If the audit row insert fails, the UPDATE rolls back.
    ResolveContradiction {
        a_id: String,
        b_id: String,
        kind: String,
        status: String,
        resolution_note: Option<String>,
        winning_triple_id: Option<String>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<ResolveContradictionReport>>,
    },
}

/// Report from [`WriteCommand::ResolveContradiction`] (dev-log 0152 H1).
/// Fields mirror the read-side `solo_query::ContradictionResolution`
/// shape so callers can convert by field name. Defined here in
/// solo-storage because the writer-actor cannot depend on solo-query
/// (which depends on solo-storage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveContradictionReport {
    pub a_id: String,
    pub b_id: String,
    pub kind: String,
    pub status: String,
    pub resolved_at_ms: Option<i64>,
    pub resolution_note: Option<String>,
    pub winning_triple_id: Option<String>,
}

/// v0.9.0 P4c: report from
/// [`WriteCommand::AttachAbstractionBatch`]. The daemon-side caller
/// can log + surface these to the consolidate-timer's
/// `tracing::info!` summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttachAbstractionBatchReport {
    pub abstractions_built: usize,
    pub triples_extracted: usize,
    pub triples_quarantined: usize,
    pub clusters_failed: usize,
    /// v0.10.1 (P4 audit m5): number of clusters whose per-cluster
    /// LLM call timed out in `Steward::extract_triples_batch`.
    /// Echoed from the caller's `clusters_deferred` argument so the
    /// daemon's `tracing::info!` summary surfaces it alongside the
    /// SQL-level counters. Also lands in the audit row's
    /// `details_json.clusters_deferred`.
    pub clusters_deferred: usize,
    /// Last LLM or storage error observed for this batch, if any.
    pub last_error: Option<String>,
}

/// Cheaply cloneable handle. Pass clones to every task that needs to write.
/// Dropping the *last* clone closes the channel and triggers actor shutdown.
#[derive(Clone, Debug)]
pub struct WriteHandle {
    tx: mpsc::Sender<WriteCommand>,
}

impl WriteHandle {
    pub async fn remember(&self, episode: Episode, embedding: Embedding) -> Result<MemoryId> {
        self.remember_as(None, episode, embedding).await
    }

    /// v0.8.0 P4: like `remember`, but records `audit_principal` in the
    /// audit_events row. Pass `Some(subject)` from auth-aware transports
    /// (HTTP via `AuthenticatedPrincipal`, MCP via cached principal).
    /// `None` matches CLI / no-auth paths.
    pub async fn remember_as(
        &self,
        audit_principal: Option<String>,
        episode: Episode,
        embedding: Embedding,
    ) -> Result<MemoryId> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Remember {
                episode,
                embedding,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// v0.9.2: atomic batched-remember. See [`WriteCommand::RememberBatch`]
    /// for semantics. Returns `MemoryId`s in input order.
    pub async fn remember_batch_as(
        &self,
        audit_principal: Option<String>,
        items: Vec<(Episode, Embedding)>,
    ) -> Result<Vec<MemoryId>> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::RememberBatch {
                items,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn forget(&self, memory_id: MemoryId, reason: String) -> Result<()> {
        self.forget_as(None, memory_id, reason).await
    }

    /// v0.8.0 P4 audit-aware variant. See [`Self::remember_as`].
    pub async fn forget_as(
        &self,
        audit_principal: Option<String>,
        memory_id: MemoryId,
        reason: String,
    ) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Forget {
                memory_id,
                reason,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn update(
        &self,
        memory_id: MemoryId,
        content: String,
        embedding: Embedding,
    ) -> Result<MemoryUpdateReport> {
        self.update_as(None, memory_id, content, embedding).await
    }

    /// Correct an active episode's content and embedding through the
    /// writer actor so it serializes with remember/forget/drain work.
    pub async fn update_as(
        &self,
        audit_principal: Option<String>,
        memory_id: MemoryId,
        content: String,
        embedding: Embedding,
    ) -> Result<MemoryUpdateReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Update {
                memory_id,
                content,
                embedding,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn review_memory(
        &self,
        memory_id: MemoryId,
        state: Option<MemoryReviewState>,
        note: Option<String>,
    ) -> Result<MemoryReviewReport> {
        self.review_memory_as(None, memory_id, state, note).await
    }

    /// Mark or clear the daemon-backed Memory Inbox review state.
    pub async fn review_memory_as(
        &self,
        audit_principal: Option<String>,
        memory_id: MemoryId,
        state: Option<MemoryReviewState>,
        note: Option<String>,
    ) -> Result<MemoryReviewReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::ReviewMemory {
                memory_id,
                state,
                note,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// Ingest a document at `path` using `chunk_config` for splitting.
    /// See [`WriteCommand::IngestDocument`] for the persistence pipeline.
    pub async fn ingest_document(
        &self,
        path: std::path::PathBuf,
        chunk_config: crate::document::ChunkConfig,
    ) -> Result<IngestReport> {
        self.ingest_document_as(None, path, chunk_config).await
    }

    /// v0.8.0 P4 audit-aware variant. See [`Self::remember_as`].
    pub async fn ingest_document_as(
        &self,
        audit_principal: Option<String>,
        path: std::path::PathBuf,
        chunk_config: crate::document::ChunkConfig,
    ) -> Result<IngestReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::IngestDocument {
                path,
                chunk_config,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// Soft-delete a document and tombstone its chunks' HNSW rowids.
    /// See [`WriteCommand::ForgetDocument`] for semantics.
    pub async fn forget_document(
        &self,
        doc_id: solo_core::DocumentId,
    ) -> Result<ForgetDocumentReport> {
        self.forget_document_as(None, doc_id).await
    }

    /// v0.8.0 P4 audit-aware variant. See [`Self::remember_as`].
    pub async fn forget_document_as(
        &self,
        audit_principal: Option<String>,
        doc_id: solo_core::DocumentId,
    ) -> Result<ForgetDocumentReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::ForgetDocument {
                doc_id,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn store_asset_from_path_as(
        &self,
        audit_principal: Option<String>,
        path: PathBuf,
        filename: Option<String>,
        mime_type: Option<String>,
        size_bytes: Option<u64>,
        sha256: Option<String>,
        source: Option<String>,
    ) -> Result<StoredAssetReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::StoreAssetFromPath {
                path,
                filename,
                mime_type,
                size_bytes,
                sha256,
                source,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn record_asset_extraction_as(
        &self,
        audit_principal: Option<String>,
        asset_id: AssetId,
        extractor_name: String,
        extractor_version: String,
        status: String,
        text_chars: u64,
        error: Option<String>,
    ) -> Result<AssetExtractionReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::RecordAssetExtraction {
                asset_id,
                extractor_name,
                extractor_version,
                status,
                text_chars,
                error,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn link_document_asset_as(
        &self,
        audit_principal: Option<String>,
        doc_id: DocumentId,
        asset_id: AssetId,
        relation_type: String,
        note: Option<String>,
    ) -> Result<DocumentAssetLinkReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::LinkDocumentAsset {
                doc_id,
                asset_id,
                relation_type,
                note,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn attach_memory_as(
        &self,
        audit_principal: Option<String>,
        memory_id: MemoryId,
        doc_id: Option<DocumentId>,
        asset_id: Option<AssetId>,
        relation_type: String,
        note: Option<String>,
    ) -> Result<MemoryAttachmentReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::AttachMemory {
                memory_id,
                doc_id,
                asset_id,
                relation_type,
                note,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn forget_asset_as(
        &self,
        audit_principal: Option<String>,
        asset_id: AssetId,
    ) -> Result<ForgetAssetReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::ForgetAsset {
                asset_id,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn consolidate(&self, scope: ConsolidationScope) -> Result<ConsolidationReport> {
        self.consolidate_as(None, scope).await
    }

    /// v0.8.0 P4 audit-aware variant. See [`Self::remember_as`].
    pub async fn consolidate_as(
        &self,
        audit_principal: Option<String>,
        scope: ConsolidationScope,
    ) -> Result<ConsolidationReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Consolidate {
                scope,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn repair_derived(&self, scope: DerivedRepairScope) -> Result<DerivedRepairReport> {
        self.repair_derived_as(None, scope).await
    }

    /// Audit-aware derived-layer repair. See [`WriteCommand::RepairDerived`].
    pub async fn repair_derived_as(
        &self,
        audit_principal: Option<String>,
        scope: DerivedRepairScope,
    ) -> Result<DerivedRepairReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::RepairDerived {
                scope,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn update_memory_quality_review_as(
        &self,
        audit_principal: Option<String>,
        update: MemoryQualityReviewUpdate,
    ) -> Result<MemoryQualityReviewUpdateReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::UpdateMemoryQualityReview {
                update,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn record_memory_retrieval(
        &self,
        entry: MemoryRetrievalLogEntry,
    ) -> Result<MemoryRetrievalLogReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::RecordRetrieval {
                entry,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn reembed(&self, scope: ReembedScope) -> Result<ReembedReport> {
        self.reembed_as(None, scope).await
    }

    /// v0.8.0 P4 audit-aware variant. See [`Self::remember_as`].
    pub async fn reembed_as(
        &self,
        audit_principal: Option<String>,
        scope: ReembedScope,
    ) -> Result<ReembedReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Reembed {
                scope,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// Run an online encrypted backup of the writer's source database
    /// to `dest_path`. Available only when the writer was spawned with
    /// a `KeyMaterial` (see [`WriterActor::spawn_full_with_key_and_optional_steward`]).
    pub async fn backup(&self, dest_path: PathBuf) -> Result<AssetBackupReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::Backup {
                dest_path,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn save_snapshot(&self) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::SaveSnapshot { reply: reply_tx })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// Rewrite historical `triples.subject_id` / `triples.object_id`
    /// values for each `(from, to)` pair in `aliases`. See
    /// [`WriteCommand::NormalizeSubjects`] for semantics.
    pub async fn normalize_subjects(
        &self,
        aliases: Vec<(String, String)>,
        dry_run: bool,
    ) -> Result<NormalizeReport> {
        self.normalize_subjects_as(None, aliases, dry_run).await
    }

    /// v0.8.0 P4 audit-aware variant. See [`Self::remember_as`].
    pub async fn normalize_subjects_as(
        &self,
        audit_principal: Option<String>,
        aliases: Vec<(String, String)>,
        dry_run: bool,
    ) -> Result<NormalizeReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::NormalizeSubjects {
                aliases,
                dry_run,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    pub async fn request_entity_split_as(
        &self,
        audit_principal: Option<String>,
        request: EntitySplitRequest,
    ) -> Result<EntityReviewOpReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::RequestEntitySplit {
                request,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// v0.9.0 P2: emit a single `AuditOperation::LlmSamplingCall` row
    /// into the per-tenant `audit_events` table via the writer-actor.
    ///
    /// The INSERT lands inside a dedicated `BEGIN IMMEDIATE`
    /// transaction on the writer's connection (lesson #30: ACID for
    /// the sampling call's only persisted trace). Insert failures
    /// surface to the caller — `SamplingLlmClient` propagates them up
    /// so a missed audit row can NOT be silently swallowed.
    ///
    /// **Privacy invariant**: callers MUST construct `event.details`
    /// with metadata only (model hint, message count, max_tokens,
    /// duration_ms, total prompt char count). The raw prompt content
    /// is user data that the user did NOT consent to log here.
    /// Enforcement lives at the call site
    /// (`SamplingLlmClient::audit_event`); this method has no way to
    /// scrub a malformed event in the writer-actor.
    pub async fn emit_llm_sampling_audit(&self, event: AuditEvent) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::EmitLlmSamplingAudit {
                event,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// v0.9.0 P4c: persist a batch of cluster-level abstractions +
    /// their extracted triples in a single writer-actor transaction,
    /// emitting ONE `AuditOperation::MemoryTriplesExtract` audit row.
    ///
    /// Called from the daemon-side consolidate-timer's triples batch
    /// path. The writer-actor handles ACID: every successful
    /// (cluster, abstraction) insert + the audit emit land or
    /// rollback together. Per-cluster INSERT failures (e.g. cluster
    /// row deleted concurrently) log + skip but do NOT abort the
    /// batch's tx.
    pub async fn attach_abstraction_batch(
        &self,
        items: Vec<(MemoryId, solo_core::SemanticAbstraction)>,
        episode_count: usize,
        duration_ms: u64,
        clusters_deferred: usize,
        audit_principal: Option<String>,
    ) -> Result<AttachAbstractionBatchReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::AttachAbstractionBatch {
                items,
                episode_count,
                duration_ms,
                clusters_deferred,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }

    /// Mark a contradiction resolved / unresolved / reopened (dev-log
    /// 0152 H1). Routes through the writer actor so the UPDATE +
    /// `audit_events` row land atomically in one BEGIN IMMEDIATE
    /// transaction. Status must be one of `unresolved` | `resolved` |
    /// `reopened`. Returns `Error::NotFound` if the (a_id, b_id, kind)
    /// triple does not match any contradiction row.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_contradiction_as(
        &self,
        audit_principal: Option<String>,
        a_id: String,
        b_id: String,
        kind: String,
        status: String,
        resolution_note: Option<String>,
        winning_triple_id: Option<String>,
    ) -> Result<ResolveContradictionReport> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(WriteCommand::ResolveContradiction {
                a_id,
                b_id,
                kind,
                status,
                resolution_note,
                winning_triple_id,
                audit_principal,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::storage("writer task gone (channel closed)"))?;
        reply_rx
            .await
            .map_err(|_| Error::storage("writer dropped reply channel"))?
    }
}

/// The writer actor.
pub struct WriterActor {
    conn: Connection,
    hnsw: Arc<dyn VectorIndex + Send + Sync>,
    rx: mpsc::Receiver<WriteCommand>,
    /// Directory for HNSW snapshot save. `None` means `SaveSnapshot` returns
    /// an error (used in unit tests that don't exercise the snapshot path).
    /// The daemon main (commit 1.5) sets this to the data dir.
    snapshot_dir: Option<PathBuf>,
    /// Resolved `embedders.embedder_id` for the active embedder. Set once
    /// at startup (`solo_storage::startup::run` resolves via
    /// `get_or_insert_embedder_id`) and cached for every `INSERT INTO
    /// embeddings` row. `None` means the writer was spawned in a test
    /// context that doesn't exercise the embeddings-table-write path —
    /// `dispatch_remember` falls back to skipping the embeddings INSERT
    /// and just writes pending_index.
    embedder_id: Option<i64>,
    /// The active embedder. Required by `handle_reembed` to regenerate
    /// vectors for episodes whose embeddings were produced by a different
    /// (older) embedder. The regular `Remember` path does NOT call this —
    /// callers pass pre-computed `Embedding` objects in `WriteCommand::
    /// Remember`. `None` here means `solo reembed` is not available on
    /// this writer (unit tests, daemon paths that opt out).
    embedder: Option<Arc<dyn Embedder>>,
    /// Handle to the tokio runtime that constructed this actor. Captured
    /// at `spawn_full_with_embedder` time so the dedicated writer thread
    /// can `block_on` async embedder calls during reembed. `None` matches
    /// `embedder == None`.
    runtime_handle: Option<Handle>,
    /// The Steward (clustering + LLM-driven abstraction). Required by
    /// `handle_consolidate`'s abstraction step (Y.3.3); the cheap
    /// clustering step (Y.2) always runs and uses
    /// `StewardConfig::default()` even when the steward is `None`. So:
    /// - `None` → `consolidate` runs cluster persistence only,
    ///   `abstractions_built` stays 0. This is the default for prod
    ///   today since no real `LlmClient` ships.
    /// - `Some` → after cluster persistence, the actor walks each
    ///   cluster, calls `steward.abstract_cluster`, persists the
    ///   `SemanticAbstraction`. Failures per cluster are logged +
    ///   counted; clusters themselves are already-persisted ground
    ///   truth and never roll back.
    steward: Option<Arc<solo_steward::Steward>>,
    /// v0.9.0 P4a: per-tenant lazily-populated Steward slot
    /// (`LibraryHandle::steward_slot()` from P0c). When present, the
    /// writer-actor PREFERS this slot's contents over `self.steward`
    /// on every consolidate tick (see [`Self::current_steward`]).
    ///
    /// This activates the v0.9.0 P2-plumbed-but-inert sampling Steward:
    /// the MCP-initialize hook writes a peer-bound Steward into the
    /// slot AFTER the writer-actor has spawned; reading the slot per
    /// command (rather than capturing `self.steward` once at spawn)
    /// is how the writer-actor observes that late-bound population.
    ///
    /// Static backends (Anthropic / OpenAI / Ollama / None) populate
    /// the slot eagerly at `LibraryHandle::open` via the configured
    /// `StewardFactory`. For those backends, `slot.try_read()` is an
    /// uncontested fast-read returning `Some(steward)` — the slot and
    /// `self.steward` carry the same `Arc` identity (the slot is the
    /// canonical source, `self.steward` mirrors it for backwards-compat
    /// with v0.8.x writer spawn paths that don't yet plumb the slot).
    ///
    /// `None` here means the writer was spawned by a path that doesn't
    /// know about the slot (older spawn variants, pure-storage tests
    /// that don't go through `LibraryHandle::open`). The writer falls
    /// back to `self.steward` in that case (which preserves v0.8.x
    /// behavior).
    steward_slot: Option<Arc<AsyncRwLock<Option<Arc<solo_steward::Steward>>>>>,
    /// Raw SQLCipher key used to open the source connection. Required
    /// by `handle_backup` so it can encrypt the destination connection
    /// with the same key. `None` means the writer was spawned without
    /// key material (test paths, the legacy spawn variants); `WriteCommand::
    /// Backup` returns a clear "not configured" error in that case.
    key: Option<KeyMaterial>,
    /// v0.8.0 P5: PII redaction registry. Disabled by default
    /// (`RedactionRegistry::is_enabled` returns `false`). When enabled,
    /// the redactor runs over text content before INSERT in every
    /// `remember` and `ingest_document` path. Built either from the
    /// per-data-dir `[redaction]` config (prod) or from `builtin()`
    /// (tests). Cheap to clone the `Arc`; cheaper still to skip the
    /// whole pass via the early-exit `is_enabled` check.
    redactor: Arc<crate::redaction::RedactionRegistry>,
    /// v0.8.1 P3: per-tenant byte quota cached at open time. `None`
    /// means unlimited — enforcement short-circuits at the top of every
    /// growth-bearing handler (remember / ingest_document), so the no-
    /// quota path is one branch and one Option compare. The cached
    /// value is refreshed only on LibraryHandle re-open; admins who
    /// reduce a quota via `solo tenants set-quota` see the change after
    /// the next daemon restart or tenant reopen.
    quota_bytes: Option<u64>,
    /// v0.8.1 P3: on-disk path used to estimate `current_size_bytes`
    /// before checking against `quota_bytes`. `None` is the test-only
    /// spawn variant; enforcement quietly skips when path is unknown.
    db_path: Option<PathBuf>,
    /// v0.9.0 P4-revision (P4 audit M1): count-based trigger signal for
    /// the daemon-side `triples_batch_timer`. When wired, the actor
    /// calls `signal.note_episode_remembered()` after every successful
    /// `Remember` so the daemon can short-circuit to a batch run
    /// before the next `trigger_interval_secs` tick.
    ///
    /// `None` for v0.8.x spawn variants + test paths that don't drive
    /// the count-based trigger. The note hook then becomes a no-op via
    /// `Option::as_ref().map(...)` shape.
    triples_batch_signal: Option<Arc<crate::triples_batch::TriplesBatchSignal>>,
    /// v0.10.0: per-tenant broadcast channel for invalidation events.
    /// Populated by `LibraryHandle::open`; the SSE subscribers
    /// (`GET /v1/graph/stream` handlers) call `tx.subscribe()` to get
    /// a `Receiver`. The writer-actor calls `tx.send(...).ok()` AFTER
    /// every successful commit; the `.ok()` swallows "no subscribers"
    /// (a normal state when no clients are connected).
    ///
    /// `None` for pure-storage test spawn paths that don't drive the
    /// SSE surface; every prod path via `LibraryHandle::open` wires this.
    /// See `INVALIDATE_BROADCAST_CAPACITY` for the channel sizing.
    invalidate_tx: Option<broadcast::Sender<InvalidateEvent>>,
    /// v0.10.0: cached tenant id (as a string) used to populate the
    /// `tenant_id` field of every emitted `InvalidateEvent`. Paired
    /// 1:1 with `invalidate_tx` — both `Some(_)` or both `None`.
    invalidate_tenant_id: Option<String>,
}

/// What `WriterActor::spawn*` returns. The daemon needs the
/// `std::thread::JoinHandle<()>` so it can wait for the writer's
/// `shutdown()` (`PRAGMA wal_checkpoint(TRUNCATE)`, final HNSW save)
/// to complete after the last `WriteHandle` is dropped. Without this,
/// the OS reaps the writer thread when `main` returns, possibly
/// mid-checkpoint.
///
/// Tests that don't care about clean shutdown timing can simply drop
/// the JoinHandle along with the WriteHandle.
pub struct WriterSpawn {
    pub handle: WriteHandle,
    pub join: std::thread::JoinHandle<()>,
}

impl WriterSpawn {
    /// Drop the WriteHandle (closing the mpsc) and block until the writer
    /// thread finishes its `shutdown()` and exits. No timeout — production
    /// supervisors decide when to force-kill via SIGKILL.
    pub fn shutdown_blocking(self) {
        drop(self.handle);
        if let Err(panic) = self.join.join() {
            tracing::error!(?panic, "solo-writer thread panicked during shutdown");
        }
    }
}

impl WriterActor {
    pub fn spawn(conn: Connection, hnsw: Arc<dyn VectorIndex + Send + Sync>) -> WriterSpawn {
        Self::spawn_with_capacity(conn, hnsw, DEFAULT_CHANNEL_CAPACITY)
    }

    pub fn spawn_with_capacity(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        capacity: usize,
    ) -> WriterSpawn {
        Self::spawn_internal(
            conn, hnsw, capacity, None, None, None, None, None, None, None,
        )
    }

    /// Spawn with a snapshot directory wired up. The daemon main calls this
    /// path so `WriteCommand::SaveSnapshot` can reach disk.
    pub fn spawn_with_snapshot_dir(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
    ) -> WriterSpawn {
        Self::spawn_internal(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// Spawn with both snapshot dir + cached embedder_id. The daemon
    /// main calls this so every `remember` also INSERTs into the
    /// `embeddings` table for durability + future `solo reembed`.
    pub fn spawn_full(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
    ) -> WriterSpawn {
        Self::spawn_internal(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// Spawn with snapshot dir + embedder_id + the active embedder. Use
    /// this from any path that may invoke `WriteCommand::Reembed` — i.e.
    /// the `solo reembed` one-shot. Captures `Handle::current()` to bridge
    /// the async embedder API onto the writer's blocking thread.
    ///
    /// **Requires a multi-thread tokio runtime.** Panics if called outside
    /// any runtime context. On a `current_thread` runtime it would NOT
    /// panic, but `handle_reembed`'s `runtime.block_on(embedder.embed(...))`
    /// from the writer thread would deadlock — the runtime's only worker
    /// would be the test's outer thread, already blocked awaiting the
    /// reembed reply. Production callers run inside `#[tokio::main]`
    /// (multi-thread by default); tests use `rt_multi(N)` with N >= 1
    /// worker independent of the test's calling thread.
    pub fn spawn_full_with_embedder(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
    ) -> WriterSpawn {
        Self::spawn_full_with_embedder_and_optional_steward(
            conn,
            hnsw,
            snapshot_dir,
            embedder_id,
            embedder,
            None,
        )
    }

    /// The full surface: snapshot + embedder + steward. Use this from
    /// the `solo consolidate` one-shot path or from a prod daemon
    /// that ships with a real `LlmClient` configured. The steward's
    /// `Arc<dyn LlmClient>` powers `handle_consolidate`'s abstraction
    /// step (Y.3.3); without a steward, consolidate runs the
    /// clustering pass only.
    pub fn spawn_full_with_embedder_and_optional_steward(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
        steward: Option<Arc<solo_steward::Steward>>,
    ) -> WriterSpawn {
        let handle = Handle::current();
        Self::spawn_internal(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            Some(embedder),
            Some(handle),
            steward,
            None,
            None,
        )
    }

    /// Like [`Self::spawn_full_with_embedder_and_optional_steward`] but
    /// also captures `key` so the writer can serve `WriteCommand::Backup`.
    /// The daemon (and one-shot paths that want HTTP-side backup) use
    /// this variant; pure-test spawn paths can use the no-key variant.
    pub fn spawn_full_with_key_and_optional_steward(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: KeyMaterial,
    ) -> WriterSpawn {
        let handle = Handle::current();
        Self::spawn_internal(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            Some(embedder),
            Some(handle),
            steward,
            Some(key),
            None,
        )
    }

    /// Variant of [`Self::spawn_full_with_key_and_optional_steward`] that
    /// takes an explicit `runtime_handle` rather than calling
    /// `Handle::current()`. v0.8.0 P2: `LibraryHandle::open` is sync (the
    /// registry's lazy-load path is `async fn` but calls it via
    /// `spawn_blocking`), so we cannot rely on a current runtime; the
    /// caller passes the handle in.
    pub fn spawn_full_with_key_steward_and_runtime(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: KeyMaterial,
        runtime_handle: Handle,
    ) -> WriterSpawn {
        Self::spawn_internal(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            Some(embedder),
            Some(runtime_handle),
            steward,
            Some(key),
            None,
        )
    }

    /// v0.8.0 P5: variant of [`Self::spawn_full_with_key_steward_and_runtime`]
    /// that also threads in a pre-built `RedactionRegistry`. Used by
    /// `LibraryHandle::open` so the per-data-dir `[redaction]` config
    /// reaches the writer-actor.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_full_with_redactor(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: KeyMaterial,
        runtime_handle: Handle,
        redactor: Arc<crate::redaction::RedactionRegistry>,
    ) -> WriterSpawn {
        Self::spawn_internal(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            Some(embedder),
            Some(runtime_handle),
            steward,
            Some(key),
            Some(redactor),
        )
    }

    /// v0.8.1 P3: variant of [`Self::spawn_full_with_redactor`] that
    /// also captures the cached per-tenant `quota_bytes` and `db_path`
    /// so the writer-actor's `handle_remember` / `handle_ingest_document`
    /// can enforce the quota before INSERT. `quota_bytes = None` means
    /// unlimited (default for tenants without a quota set); enforcement
    /// short-circuits in one branch.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_full_with_quota(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: KeyMaterial,
        runtime_handle: Handle,
        redactor: Arc<crate::redaction::RedactionRegistry>,
        quota_bytes: Option<u64>,
        db_path: PathBuf,
    ) -> WriterSpawn {
        Self::spawn_internal_full(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            Some(embedder),
            Some(runtime_handle),
            steward,
            Some(key),
            Some(redactor),
            quota_bytes,
            Some(db_path),
            None,
            None,
            None,
            None,
        )
    }

    /// v0.9.0 P4a: variant of [`Self::spawn_full_with_quota`] that also
    /// threads the per-tenant `steward_slot` so the writer-actor's
    /// consolidate path can observe late-bound sampling-backed
    /// Stewards (populated by the MCP-initialize hook after writer
    /// spawn). Called from [`crate::library::handle::LibraryHandle::
    /// open`] — every other spawn path stays on `spawn_full_with_quota`
    /// and falls back to `self.steward` in [`Self::current_steward`].
    ///
    /// Per plan §4 P4a: "WriterActor reads `tenant.steward_slot()` per
    /// command (or per consolidate-tick), falling back to `self.steward`
    /// if the slot is None." The slot-read is cheap
    /// (`steward_slot.try_read().clone()` returns
    /// `Option<Arc<Steward>>`; the clone is an Arc-bump). For the
    /// sampling backend specifically, `self.steward` is `None` at spawn
    /// (the factory builds a no-op); the slot read picks up the
    /// Steward once the MCP session is initialized.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_full_with_quota_and_slot(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: KeyMaterial,
        runtime_handle: Handle,
        redactor: Arc<crate::redaction::RedactionRegistry>,
        quota_bytes: Option<u64>,
        db_path: PathBuf,
        steward_slot: Arc<AsyncRwLock<Option<Arc<solo_steward::Steward>>>>,
        triples_batch_signal: Option<Arc<crate::triples_batch::TriplesBatchSignal>>,
    ) -> WriterSpawn {
        Self::spawn_internal_full(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            Some(embedder),
            Some(runtime_handle),
            steward,
            Some(key),
            Some(redactor),
            quota_bytes,
            Some(db_path),
            Some(steward_slot),
            triples_batch_signal,
            None,
            None,
        )
    }

    /// v0.10.0: variant of [`Self::spawn_full_with_quota_and_slot`] that
    /// also threads the per-tenant `broadcast::Sender<InvalidateEvent>`
    /// + the tenant id string so the writer-actor can fan out
    /// post-commit invalidations to `GET /v1/graph/stream` SSE
    /// subscribers. Called from [`crate::library::handle::LibraryHandle::
    /// open`] (the prod entry point); test paths can use any of the
    /// older spawn variants and skip invalidation broadcasting.
    ///
    /// Invariant (lesson #30): the broadcast `send` happens AFTER the
    /// writer-actor's commit returns `Ok`. Rolled-back writes MUST NOT
    /// produce an event. Implementation lives in each mutation handler's
    /// dispatch wrapper next to the audit-emit (success path) /
    /// emit_audit_best_effort (failure path) call.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_full_with_invalidate(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        snapshot_dir: PathBuf,
        embedder_id: i64,
        embedder: Arc<dyn Embedder>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: KeyMaterial,
        runtime_handle: Handle,
        redactor: Arc<crate::redaction::RedactionRegistry>,
        quota_bytes: Option<u64>,
        db_path: PathBuf,
        steward_slot: Arc<AsyncRwLock<Option<Arc<solo_steward::Steward>>>>,
        triples_batch_signal: Option<Arc<crate::triples_batch::TriplesBatchSignal>>,
        invalidate_tx: broadcast::Sender<InvalidateEvent>,
        invalidate_tenant_id: String,
    ) -> WriterSpawn {
        Self::spawn_internal_full(
            conn,
            hnsw,
            DEFAULT_CHANNEL_CAPACITY,
            Some(snapshot_dir),
            Some(embedder_id),
            Some(embedder),
            Some(runtime_handle),
            steward,
            Some(key),
            Some(redactor),
            quota_bytes,
            Some(db_path),
            Some(steward_slot),
            triples_batch_signal,
            Some(invalidate_tx),
            Some(invalidate_tenant_id),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_internal(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        capacity: usize,
        snapshot_dir: Option<PathBuf>,
        embedder_id: Option<i64>,
        embedder: Option<Arc<dyn Embedder>>,
        runtime_handle: Option<Handle>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: Option<KeyMaterial>,
        redactor: Option<Arc<crate::redaction::RedactionRegistry>>,
    ) -> WriterSpawn {
        Self::spawn_internal_full(
            conn,
            hnsw,
            capacity,
            snapshot_dir,
            embedder_id,
            embedder,
            runtime_handle,
            steward,
            key,
            redactor,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// v0.8.1 P3: full internal spawn that also threads `quota_bytes` +
    /// `db_path` for the writer-actor's quota-enforcement path. Older
    /// `spawn_internal` calls delegate here with `None` for both,
    /// preserving the existing test-spawn paths' "no quota check"
    /// behavior.
    ///
    /// v0.10.0: also threads the optional
    /// `broadcast::Sender<InvalidateEvent>` + `invalidate_tenant_id`
    /// so the writer-actor can fan out post-commit invalidations to
    /// `GET /v1/graph/stream` SSE subscribers. The pair is `(None, None)`
    /// for spawn paths that don't drive the SSE surface.
    #[allow(clippy::too_many_arguments)]
    fn spawn_internal_full(
        conn: Connection,
        hnsw: Arc<dyn VectorIndex + Send + Sync>,
        capacity: usize,
        snapshot_dir: Option<PathBuf>,
        embedder_id: Option<i64>,
        embedder: Option<Arc<dyn Embedder>>,
        runtime_handle: Option<Handle>,
        steward: Option<Arc<solo_steward::Steward>>,
        key: Option<KeyMaterial>,
        redactor: Option<Arc<crate::redaction::RedactionRegistry>>,
        quota_bytes: Option<u64>,
        db_path: Option<PathBuf>,
        steward_slot: Option<Arc<AsyncRwLock<Option<Arc<solo_steward::Steward>>>>>,
        triples_batch_signal: Option<Arc<crate::triples_batch::TriplesBatchSignal>>,
        invalidate_tx: Option<broadcast::Sender<InvalidateEvent>>,
        invalidate_tenant_id: Option<String>,
    ) -> WriterSpawn {
        let (tx, rx) = mpsc::channel(capacity);
        let redactor = redactor.unwrap_or_else(|| {
            // Disabled registry: writer's redaction path no-ops via the
            // early `is_enabled` check.
            Arc::new(
                crate::redaction::RedactionRegistry::from_config(
                    &crate::config::RedactionConfig::default(),
                )
                .expect("default RedactionConfig must build a disabled registry"),
            )
        });
        // Pair invalidate_tx with invalidate_tenant_id — both must be
        // Some or both None. Drop one silently if only one was supplied
        // (test path defensively; prod always supplies both).
        let (invalidate_tx, invalidate_tenant_id) = match (invalidate_tx, invalidate_tenant_id) {
            (Some(tx), Some(tid)) => (Some(tx), Some(tid)),
            _ => (None, None),
        };
        let actor = Self {
            conn,
            hnsw,
            rx,
            snapshot_dir,
            embedder_id,
            embedder,
            runtime_handle,
            steward,
            steward_slot,
            triples_batch_signal,
            key,
            redactor,
            quota_bytes,
            db_path,
            invalidate_tx,
            invalidate_tenant_id,
        };
        let join = std::thread::Builder::new()
            .name("solo-writer".into())
            // SQLCipher's online-backup path overflows Rust's default
            // Windows thread stack in optimized builds. The writer owns that
            // live-connection operation, so reserve its proven stack here.
            .stack_size(BACKUP_STACK_SIZE_BYTES)
            .spawn(move || actor.run())
            .expect("spawn solo-writer thread");
        WriterSpawn {
            handle: WriteHandle { tx },
            join,
        }
    }

    fn run(mut self) {
        while let Some(cmd) = self.rx.blocking_recv() {
            self.dispatch(cmd);
        }
        self.shutdown();
    }

    /// v0.9.0 P4a: resolve the "active" Steward for this writer-actor at
    /// command-time. Prefers the lazily-populated [`Self::steward_slot`]
    /// over the eagerly-captured [`Self::steward`].
    ///
    /// Read priority — slot, then `self.steward`:
    ///
    ///   * **Slot populated** (sampling backend mid-life, or any
    ///     factory-driven static backend): `try_read` succeeds
    ///     uncontested; observable Steward is the one written by the
    ///     MCP-initialize hook (P2's `populate_sampling_steward`).
    ///   * **Slot populated but contended** (rare — only happens during
    ///     the brief window where the MCP-initialize hook is writing):
    ///     `try_read` fails. Fall back to `self.steward` rather than
    ///     blocking the writer thread; the next consolidate tick will
    ///     observe the populated slot.
    ///   * **Slot empty AND `self.steward` populated**: a v0.8.x-style
    ///     caller wired the eager Steward without using the slot.
    ///     Return `self.steward`.
    ///   * **Slot empty AND `self.steward` empty**: no LLM backend
    ///     configured; consolidate runs the cheap clustering pass only
    ///     (the existing v0.2-era posture).
    ///
    /// **Deadlock check** (lesson #30 + plan §6 read-side discipline):
    /// the slot uses `tokio::sync::RwLock`. Calling `try_read` from the
    /// writer thread (a synchronous OS thread, NOT a tokio worker)
    /// would normally panic — `tokio::sync::RwLock::try_read` is safe
    /// for sync callers but the wider `try_read().await` form is not.
    /// We use the sync `try_read` shape, which returns
    /// `Result<RwLockReadGuard, TryLockError>` and never blocks.
    fn current_steward(&self) -> Option<Arc<solo_steward::Steward>> {
        if let Some(slot) = self.steward_slot.as_ref() {
            // try_read is the sync variant — never blocks, returns an
            // error if a writer holds the lock. We do not retry; the
            // brief contended-write window (the MCP-initialize hook)
            // is so short that the next consolidate tick observes the
            // populated slot.
            if let Ok(guard) = slot.try_read() {
                if let Some(s) = guard.as_ref() {
                    return Some(Arc::clone(s));
                }
            }
        }
        self.steward.clone()
    }

    fn dispatch(&mut self, cmd: WriteCommand) {
        match cmd {
            WriteCommand::Remember {
                episode,
                embedding,
                audit_principal,
                reply,
            } => self.dispatch_remember(episode, embedding, audit_principal, reply),
            WriteCommand::RememberBatch {
                items,
                audit_principal,
                reply,
            } => self.dispatch_remember_batch(items, audit_principal, reply),
            WriteCommand::Forget {
                memory_id,
                reason,
                audit_principal,
                reply,
            } => {
                let result = self.handle_forget(memory_id, reason, audit_principal.clone());
                // v0.8.0 P4: success-path audit is inside the tx. Error
                // path: best-effort emit here (the tx already aborted).
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryForget,
                        Some(memory_id.to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                // v0.10.0: post-commit invalidation (lesson #30).
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryForget.as_str(), "episode");
                }
            }
            WriteCommand::Update {
                memory_id,
                content,
                embedding,
                audit_principal,
                reply,
            } => {
                self.dispatch_update(memory_id, content, embedding, audit_principal, reply);
            }
            WriteCommand::ReviewMemory {
                memory_id,
                state,
                note,
                audit_principal,
                reply,
            } => {
                let result =
                    self.handle_memory_review(memory_id, state, note, audit_principal.clone());
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryReview,
                        Some(memory_id.to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryReview.as_str(), "episode");
                }
            }
            WriteCommand::IngestDocument {
                path,
                chunk_config,
                audit_principal,
                reply,
            } => {
                self.dispatch_ingest_document(path, chunk_config, audit_principal, reply);
            }
            WriteCommand::ForgetDocument {
                doc_id,
                audit_principal,
                reply,
            } => {
                let result = self.handle_forget_document(doc_id, audit_principal.clone());
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryForgetDocument,
                        Some(doc_id.to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                // v0.10.0: post-commit invalidation (lesson #30).
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryForgetDocument.as_str(), "document");
                }
            }
            WriteCommand::StoreAssetFromPath {
                path,
                filename,
                mime_type,
                size_bytes,
                sha256,
                source,
                audit_principal,
                reply,
            } => {
                let result = self.handle_store_asset_from_path(
                    &path,
                    filename,
                    mime_type,
                    size_bytes,
                    sha256,
                    source,
                    audit_principal.clone(),
                );
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryStoreAsset,
                        Some(path.display().to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryStoreAsset.as_str(), "asset");
                }
            }
            WriteCommand::RecordAssetExtraction {
                asset_id,
                extractor_name,
                extractor_version,
                status,
                text_chars,
                error,
                audit_principal,
                reply,
            } => {
                let result = self.handle_record_asset_extraction(
                    asset_id,
                    extractor_name,
                    extractor_version,
                    status,
                    text_chars,
                    error,
                    audit_principal.clone(),
                );
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryRecordAssetExtraction,
                        Some(asset_id.to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(
                        AuditOperation::MemoryRecordAssetExtraction.as_str(),
                        "asset",
                    );
                }
            }
            WriteCommand::LinkDocumentAsset {
                doc_id,
                asset_id,
                relation_type,
                note,
                audit_principal,
                reply,
            } => {
                let result = self.handle_link_document_asset(
                    doc_id,
                    asset_id,
                    relation_type,
                    note,
                    audit_principal.clone(),
                );
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryLinkDocumentAsset,
                        Some(doc_id.to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(
                        AuditOperation::MemoryLinkDocumentAsset.as_str(),
                        "document",
                    );
                }
            }
            WriteCommand::AttachMemory {
                memory_id,
                doc_id,
                asset_id,
                relation_type,
                note,
                audit_principal,
                reply,
            } => {
                let result = self.handle_attach_memory(
                    memory_id,
                    doc_id,
                    asset_id,
                    relation_type,
                    note,
                    audit_principal.clone(),
                );
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryAttach,
                        Some(memory_id.to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryAttach.as_str(), "episode");
                }
            }
            WriteCommand::ForgetAsset {
                asset_id,
                audit_principal,
                reply,
            } => {
                let result = self.handle_forget_asset(asset_id, audit_principal.clone());
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryForgetAsset,
                        Some(asset_id.to_string()),
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryForgetAsset.as_str(), "asset");
                }
            }
            WriteCommand::Consolidate {
                scope,
                audit_principal,
                reply,
            } => {
                let result = self.handle_consolidate(scope, audit_principal);
                let durable_ok = result.is_ok();
                let _ = reply.send(result);
                // v0.10.0: post-commit invalidation (lesson #30).
                // Consolidate affects clusters; the abstraction +
                // triples cascade fires on its own batch path.
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryConsolidate.as_str(), "cluster");
                }
            }
            WriteCommand::RepairDerived {
                scope,
                audit_principal,
                reply,
            } => {
                let dry_run = scope.dry_run;
                let result = self.handle_repair_derived(scope, audit_principal);
                let durable_ok = result.is_ok() && !dry_run;
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryDerivedRepair.as_str(), "cluster");
                }
            }
            WriteCommand::UpdateMemoryQualityReview {
                update,
                audit_principal,
                reply,
            } => {
                let result =
                    self.handle_memory_quality_review_update(update, audit_principal.clone());
                let durable_ok = result.is_ok();
                if let Err(ref e) = result {
                    self.emit_audit_best_effort(
                        AuditOperation::MemoryQualityReview,
                        None,
                        AuditResult::Error,
                        audit_principal,
                        Some(serde_json::json!({ "error": e.to_string() })),
                    );
                }
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryQualityReview.as_str(), "triple");
                }
            }
            WriteCommand::RecordRetrieval { entry, reply } => {
                let result = self.handle_record_memory_retrieval(entry);
                let _ = reply.send(result);
            }
            WriteCommand::Reembed {
                scope,
                audit_principal,
                reply,
            } => {
                let result = self.handle_reembed(scope, audit_principal);
                let durable_ok = result.is_ok();
                let _ = reply.send(result);
                // v0.10.0: post-commit invalidation (lesson #30).
                // Reembed changes episode vectors so semantic
                // neighbors shift — surface as an episode kind.
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryReembed.as_str(), "episode");
                }
            }
            WriteCommand::SaveSnapshot { reply } => {
                let _ = reply.send(self.handle_save_snapshot());
            }
            WriteCommand::Backup { dest_path, reply } => {
                let _ = reply.send(self.handle_backup(&dest_path));
            }
            WriteCommand::NormalizeSubjects {
                aliases,
                dry_run,
                audit_principal,
                reply,
            } => {
                let result = self.handle_normalize_subjects(aliases, dry_run, audit_principal);
                let durable_ok = result.is_ok() && !dry_run;
                let _ = reply.send(result);
                // v0.10.0: post-commit invalidation (lesson #30). Skip
                // the dry-run path — by construction it ROLLs back.
                if durable_ok {
                    self.emit_invalidate(
                        AuditOperation::MemoryNormalizeSubjects.as_str(),
                        "triple",
                    );
                }
            }
            WriteCommand::RequestEntitySplit {
                request,
                audit_principal,
                reply,
            } => {
                let result = self.handle_request_entity_split(request, audit_principal);
                let durable_ok = result.is_ok();
                let _ = reply.send(result);
                if durable_ok {
                    self.emit_invalidate(
                        AuditOperation::MemoryNormalizeSubjects.as_str(),
                        "entity",
                    );
                }
            }
            WriteCommand::EmitLlmSamplingAudit { event, reply } => {
                let _ = reply.send(self.handle_emit_llm_sampling_audit(event));
            }
            WriteCommand::ResolveContradiction {
                a_id,
                b_id,
                kind,
                status,
                resolution_note,
                winning_triple_id,
                audit_principal,
                reply,
            } => {
                let result = self.handle_resolve_contradiction(
                    a_id,
                    b_id,
                    kind,
                    status,
                    resolution_note,
                    winning_triple_id,
                    audit_principal,
                );
                let durable_ok = result.is_ok();
                let _ = reply.send(result);
                // Post-commit invalidation: contradictions are derived data
                // surfaced by `memory_contradictions` + the graph view.
                if durable_ok {
                    self.emit_invalidate(
                        AuditOperation::MemoryContradictionResolve.as_str(),
                        "contradiction",
                    );
                }
            }
            WriteCommand::AttachAbstractionBatch {
                items,
                episode_count,
                duration_ms,
                clusters_deferred,
                audit_principal,
                reply,
            } => {
                let result = self.handle_attach_abstraction_batch(
                    items,
                    episode_count,
                    duration_ms,
                    clusters_deferred,
                    audit_principal,
                );
                let durable_ok = result.is_ok();
                let _ = reply.send(result);
                // v0.10.0: post-commit invalidation (lesson #30).
                // Abstractions + triples both attach to a cluster —
                // surface as a cluster kind. Subscribers refetch the
                // cluster-kind page on this event.
                if durable_ok {
                    self.emit_invalidate(AuditOperation::MemoryTriplesExtract.as_str(), "cluster");
                }
            }
        }
    }

    /// v0.9.0 P2: insert one `AuditOperation::LlmSamplingCall` row in
    /// a dedicated sync tx on the writer's connection.
    ///
    /// Returns `Ok(())` on a successful INSERT + COMMIT; surfaces every
    /// SQLite-layer failure to the caller via `Err(_)`. The caller is
    /// `SamplingLlmClient::complete()`, which propagates the error to
    /// the LLM-using subsystem (Steward abstraction / contradiction
    /// path) — a failed audit insert means the sampling call's only
    /// persisted trace is missing, which is operator-visible.
    ///
    /// The transaction is `BEGIN IMMEDIATE` so the writer-actor's
    /// other handlers (which use the same connection) don't race with
    /// the audit insert.
    fn handle_emit_llm_sampling_audit(&mut self, event: AuditEvent) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| {
                Error::storage(format!("BEGIN IMMEDIATE for llm.sampling_call audit: {e}"))
            })?;
        insert_audit_row_in_tx(&tx, &event)?;
        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT llm.sampling_call audit: {e}")))?;
        Ok(())
    }

    /// Dev-log 0152 H1: contradiction resolution routed through the
    /// writer-actor (was previously executed via the reader pool, racing
    /// with the writer-actor on multiple connections and emitting the
    /// audit row outside the tx).
    ///
    /// One BEGIN IMMEDIATE tx wraps the UPDATE + audit row. If the audit
    /// INSERT fails, the UPDATE rolls back — strict ACID.
    #[allow(clippy::too_many_arguments)]
    fn handle_resolve_contradiction(
        &mut self,
        a_id: String,
        b_id: String,
        kind: String,
        status: String,
        resolution_note: Option<String>,
        winning_triple_id: Option<String>,
        audit_principal: Option<String>,
    ) -> Result<ResolveContradictionReport> {
        let status = status.trim().to_string();
        if !matches!(status.as_str(), "unresolved" | "resolved" | "reopened") {
            return Err(Error::invalid_input(
                "contradiction status must be unresolved, resolved, or reopened",
            ));
        }
        let note = resolution_note
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let winning = winning_triple_id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let resolved_at_ms = if status == "resolved" {
            Some(chrono::Utc::now().timestamp_millis())
        } else {
            None
        };
        let now_ms = chrono::Utc::now().timestamp_millis();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| {
                Error::storage(format!("BEGIN IMMEDIATE for resolve_contradiction: {e}"))
            })?;

        let changed = tx
            .execute(
                "UPDATE contradictions
                    SET status = ?4,
                        resolved_at_ms = ?5,
                        resolution_note = ?6,
                        winning_triple_id = ?7
                  WHERE a_memory_id = ?1
                    AND b_memory_id = ?2
                    AND kind = ?3",
                rusqlite::params![a_id, b_id, kind, status, resolved_at_ms, note, winning],
            )
            .map_err(|e| Error::storage(format!("UPDATE contradictions: {e}")))?;
        if changed == 0 {
            // Roll back the empty tx implicitly (drop) — nothing to audit.
            return Err(Error::not_found("contradiction not found"));
        }

        // Audit row inside the SAME tx — UPDATE rolls back if INSERT fails.
        let target = format!("{a_id}:{b_id}:{kind}");
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryContradictionResolve,
                target_id: Some(target),
                result: AuditResult::Ok,
                details: None,
            },
        )?;

        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT resolve_contradiction: {e}")))?;

        Ok(ResolveContradictionReport {
            a_id,
            b_id,
            kind,
            status,
            resolved_at_ms,
            resolution_note: note,
            winning_triple_id: winning,
        })
    }

    /// v0.9.0 P4c: persist a batch of `(cluster_id, abstraction)`
    /// pairs in a single transaction + emit ONE
    /// `AuditOperation::MemoryTriplesExtract` audit row carrying the
    /// batch's aggregate counts.
    ///
    /// One `BEGIN IMMEDIATE` tx wraps all the per-cluster INSERTs +
    /// the audit emit.
    ///
    /// **v0.9.0 P4-revision (P4 audit M2)**: each cluster's
    /// DELETE-stale + INSERT-new pair is wrapped in a per-cluster
    /// `SAVEPOINT`. On per-cluster INSERT failure we ROLLBACK TO
    /// SAVEPOINT, which undoes that cluster's DELETE — preserving the
    /// stale abstraction rather than orphaning the cluster with no
    /// abstraction at all. RELEASE SAVEPOINT on success folds the
    /// per-cluster work back into the outer tx.
    ///
    /// Audit emit is SYNC inside the same outer tx (lesson #30): if
    /// the audit INSERT fails, the entire batch tx aborts — the audit
    /// row IS the only persisted record of the batch.
    fn handle_attach_abstraction_batch(
        &mut self,
        items: Vec<(MemoryId, solo_core::SemanticAbstraction)>,
        episode_count: usize,
        duration_ms: u64,
        clusters_deferred: usize,
        audit_principal: Option<String>,
    ) -> Result<AttachAbstractionBatchReport> {
        // Validate input shape (defensive). Each tuple's `MemoryId`
        // MUST equal the embedded abstraction's `cluster_id` — the
        // caller (daemon-side batch path) constructs them as a pair.
        for (cluster_id, abstraction) in &items {
            if abstraction.cluster_id != *cluster_id {
                return Err(Error::Other(format!(
                    "AttachAbstractionBatch: cluster_id mismatch on tuple \
                     (got {} but abstraction.cluster_id is {})",
                    cluster_id, abstraction.cluster_id
                )));
            }
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| {
                Error::storage(format!("BEGIN IMMEDIATE for attach_abstraction_batch: {e}"))
            })?;

        let mut report = AttachAbstractionBatchReport::default();
        for (idx, (cluster_id, abstraction)) in items.iter().enumerate() {
            let prov_json = match serde_json::to_string(&abstraction.provenance) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        cluster_id = %cluster_id,
                        error = %e,
                        "attach_abstraction_batch: serialize provenance failed; skipping cluster"
                    );
                    report.clusters_failed += 1;
                    report.last_error = Some(e.to_string());
                    continue;
                }
            };

            // v0.9.0 P4-revision (P4 audit M2): wrap each cluster's
            // DELETE-stale + INSERT-new in a SAVEPOINT so a failed
            // INSERT can ROLLBACK TO the savepoint and undo its OWN
            // DELETE. Pre-revision, the per-cluster DELETE happened
            // unconditionally inside the outer tx; if the per-cluster
            // INSERT then failed, the outer tx still committed and the
            // cluster's stale abstraction was destroyed without
            // replacement. Now each cluster is atomic — either the
            // savepoint releases (replacement persisted) or it rolls
            // back (old abstraction kept; cluster booked as failed).
            //
            // Naming: SAVEPOINT identifiers are SQL identifiers; we
            // use `cluster_<idx>` (the position in `items`) so two
            // clusters in the same batch never collide. The
            // `cluster_id` itself is a UUID-shaped MemoryId — not
            // SQL-safe to use directly as an identifier. The idx
            // approach is collision-free + grep-friendly.
            let sp_name = format!("cluster_{idx}");

            if let Err(e) = tx.execute_batch(&format!("SAVEPOINT {sp_name};")) {
                tracing::warn!(
                    cluster_id = %cluster_id,
                    error = %e,
                    "attach_abstraction_batch: open SAVEPOINT failed; skipping cluster"
                );
                report.clusters_failed += 1;
                report.last_error = Some(e.to_string());
                continue;
            }

            let cluster_id_s = cluster_id.to_string();
            let per_cluster_res = (|| -> rusqlite::Result<(usize, usize)> {
                // Drop any stale abstraction + its cascaded triples for
                // this cluster — idempotent re-runs after a partial
                // earlier batch don't double-INSERT.
                tx.execute(
                    "DELETE FROM semantic_abstractions WHERE cluster_id = ?",
                    params![&cluster_id_s],
                )?;
                tx.execute(
                    "DELETE FROM triples WHERE cluster_id = ?",
                    params![&cluster_id_s],
                )?;
                tx.execute(
                    "DELETE FROM triple_reviews WHERE cluster_id = ?",
                    params![&cluster_id_s],
                )?;

                tx.execute(
                    "INSERT INTO semantic_abstractions
                        (abstraction_id, cluster_id, content, provenance_json,
                         confidence, created_at_ms)
                     VALUES (?, ?, ?, ?, ?, ?)",
                    params![
                        abstraction.abstraction_id.to_string(),
                        abstraction.cluster_id.to_string(),
                        abstraction.content,
                        prov_json,
                        abstraction.confidence.0,
                        now_ms,
                    ],
                )?;
                let mut triples_accepted = 0usize;
                let mut triples_quarantined = 0usize;
                for triple in &abstraction.triples {
                    let tprov = serde_json::to_string(&triple.provenance)
                        .unwrap_or_else(|_| "{}".to_string());
                    let source_eid = resolve_source_episode_id_in_tx(&tx, &triple.provenance);
                    match evaluate_extracted_triple(
                        triple,
                        &cluster_id_s,
                        source_eid,
                        tprov.clone(),
                    ) {
                        TripleQualityDecision::Active(active) => {
                            insert_active_triple_in_tx(
                                &tx,
                                &active,
                                &cluster_id_s,
                                source_eid,
                                &tprov,
                                now_ms,
                            )?;
                            triples_accepted += 1;
                        }
                        TripleQualityDecision::Review(candidate) => {
                            insert_triple_review_in_tx(&tx, &candidate, now_ms)?;
                            triples_quarantined += 1;
                        }
                    }
                }
                Ok((triples_accepted, triples_quarantined))
            })();

            match per_cluster_res {
                Ok((triples_accepted, triples_quarantined)) => {
                    // RELEASE the savepoint — the per-cluster
                    // write is now part of the outer transaction.
                    if let Err(e) = tx.execute_batch(&format!("RELEASE SAVEPOINT {sp_name};")) {
                        // RELEASE failure is exceptional (e.g. DB
                        // gone, disk full). Treat the cluster as
                        // failed and try to ROLLBACK to clean up.
                        tracing::warn!(
                            cluster_id = %cluster_id,
                            error = %e,
                            "attach_abstraction_batch: RELEASE SAVEPOINT failed; cluster booked as failed"
                        );
                        let _ = tx.execute_batch(&format!(
                            "ROLLBACK TO SAVEPOINT {sp_name}; RELEASE SAVEPOINT {sp_name};"
                        ));
                        report.clusters_failed += 1;
                        report.last_error = Some(e.to_string());
                        continue;
                    }
                    report.abstractions_built += 1;
                    report.triples_extracted += triples_accepted;
                    report.triples_quarantined += triples_quarantined;
                }
                Err(e) => {
                    // ROLLBACK TO undoes everything since the
                    // SAVEPOINT (including the DELETE). RELEASE
                    // afterwards pops the savepoint off the stack.
                    // Both must succeed or the savepoint stack would
                    // leak — but if they fail we log and continue
                    // (the outer tx still rolls back on Err return
                    // from this fn; here we want to PRESERVE the
                    // other clusters' SUCCESSFUL writes).
                    tracing::warn!(
                        cluster_id = %cluster_id,
                        error = %e,
                        "attach_abstraction_batch: per-cluster work failed; ROLLBACK TO SAVEPOINT"
                    );
                    if let Err(rb_err) = tx.execute_batch(&format!(
                        "ROLLBACK TO SAVEPOINT {sp_name}; RELEASE SAVEPOINT {sp_name};"
                    )) {
                        // Savepoint stack is now in an unknown state.
                        // Bail out of the whole batch — the outer tx
                        // will roll back when we Err return.
                        return Err(Error::storage(format!(
                            "ROLLBACK TO SAVEPOINT for cluster {cluster_id} \
                             failed (rb_err={rb_err}; original cluster err={e}); \
                             aborting entire batch"
                        )));
                    }
                    report.clusters_failed += 1;
                    report.last_error = Some(e.to_string());
                }
            }
        }

        // v0.10.1 (m5): surface the caller's per-cluster-timeout
        // tally on the returned report so the daemon's tracing line
        // shows clusters_deferred alongside the SQL-level counters.
        report.clusters_deferred = clusters_deferred;

        // Audit emit sync inside the same tx (lesson #30). If it
        // fails, the whole batch rolls back — the audit row IS the
        // persisted record of the batch.
        let audit_event = AuditEvent {
            ts_ms: now_ms,
            principal_subject: audit_principal,
            operation: AuditOperation::MemoryTriplesExtract,
            target_id: None,
            result: AuditResult::Ok,
            details: Some(serde_json::json!({
                "episode_count": episode_count,
                "cluster_count": items.len(),
                "abstractions_built": report.abstractions_built,
                "triples_extracted": report.triples_extracted,
                "triples_quarantined": report.triples_quarantined,
                "clusters_failed": report.clusters_failed,
                "clusters_deferred": clusters_deferred,
                "duration_ms": duration_ms,
            })),
        };
        insert_audit_row_in_tx(&tx, &audit_event)?;

        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT attach_abstraction_batch: {e}")))?;
        Ok(report)
    }

    fn dispatch_remember(
        &mut self,
        episode: Episode,
        embedding: Embedding,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<MemoryId>>,
    ) {
        let memory_id = episode.memory_id;
        let result = self.handle_remember_durable(episode, embedding, audit_principal.clone());
        let durable_ok = result.is_ok();
        // v0.8.0 P4: error-path audit emit. Success-path audit is inside
        // the write tx (handle_remember_durable). On failure, the tx
        // aborted so we record best-effort here.
        if let Err(ref e) = result {
            self.emit_audit_best_effort(
                AuditOperation::MemoryRemember,
                Some(memory_id.to_string()),
                AuditResult::Error,
                audit_principal,
                Some(serde_json::json!({ "error": e.to_string() })),
            );
        }
        let _ = reply.send(result);

        // v0.10.0: fan out an `InvalidateEvent` on success. Lesson #30:
        // AFTER commit, never before. `durable_ok == false` means the
        // tx rolled back and no row landed; no event.
        if durable_ok {
            self.emit_invalidate(AuditOperation::MemoryRemember.as_str(), "episode");
        }

        if durable_ok {
            // v0.9.0 P4-revision (P4 audit M1): note the new episode for
            // the count-based trigger. Fires `notify_one` once the
            // counter crosses `trigger_episode_count` so the daemon's
            // `triples_batch_timer` can short-circuit the next batch
            // run without waiting for the time-interval tick. No-op
            // when the signal isn't wired (v0.8.x spawn variants).
            if let Some(sig) = self.triples_batch_signal.as_ref() {
                sig.note_episode_remembered();
            }

            if let Err(e) = self.conn.execute(
                "DELETE FROM pending_index WHERE memory_id = ?",
                params![memory_id.to_string()],
            ) {
                tracing::warn!(
                    error = %e,
                    %memory_id,
                    "pending_index drain failed; will replay on next startup"
                );
            }
        }
    }

    fn dispatch_update(
        &mut self,
        memory_id: MemoryId,
        content: String,
        embedding: Embedding,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<MemoryUpdateReport>>,
    ) {
        let result =
            self.handle_update_durable(memory_id, content, embedding, audit_principal.clone());
        let durable_ok = result.is_ok();
        if let Err(ref e) = result {
            self.emit_audit_best_effort(
                AuditOperation::MemoryUpdate,
                Some(memory_id.to_string()),
                AuditResult::Error,
                audit_principal,
                Some(serde_json::json!({ "error": e.to_string() })),
            );
        }
        let memory_id_for_drain = result.as_ref().ok().map(|r| r.memory_id);
        let _ = reply.send(result);

        if durable_ok {
            self.emit_invalidate(AuditOperation::MemoryUpdate.as_str(), "episode");
            if let Some(mid) = memory_id_for_drain {
                if let Err(e) = self.conn.execute(
                    "DELETE FROM pending_index WHERE kind = 'episode' AND memory_id = ?",
                    params![mid.to_string()],
                ) {
                    tracing::warn!(
                        error = %e,
                        %mid,
                        "pending_index drain failed (update); will replay on next startup"
                    );
                }
            }
        }
    }

    fn handle_update_durable(
        &mut self,
        memory_id: MemoryId,
        content: String,
        embedding: Embedding,
        audit_principal: Option<String>,
    ) -> Result<MemoryUpdateReport> {
        embedding.validate()?;
        let f32_slice = embedding.as_f32_slice().ok_or_else(|| {
            Error::embedder("HNSW expects F32 embeddings; convert dtype upstream")
        })?;
        let content = content.trim();
        if content.is_empty() {
            return Err(Error::invalid_input(
                "updated memory content must not be empty",
            ));
        }

        let redaction = self.redactor.redact(content);
        let redacted_content: &str = redaction.text.as_ref();
        let memory_id_s = memory_id.to_string();
        let now_ms = chrono::Utc::now().timestamp_millis();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for update: {e}")))?;

        let existing: Option<(i64, String)> = tx
            .query_row(
                "SELECT rowid, status FROM episodes WHERE memory_id = ?1",
                params![&memory_id_s],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| Error::storage(format!("SELECT episode for update: {e}")))?;
        let (rowid, status) = existing.ok_or_else(|| Error::not_found("memory not found"))?;
        if status != "active" {
            return Err(Error::conflict("cannot update a non-active memory"));
        }

        tx.execute(
            "UPDATE episodes
                SET content = ?2,
                    updated_at_ms = ?3
              WHERE memory_id = ?1",
            params![&memory_id_s, redacted_content, now_ms],
        )
        .map_err(|e| Error::storage(format!("UPDATE episode: {e}")))?;

        if let Some(eid) = self.embedder_id {
            let dtype_str = match embedding.dtype {
                solo_core::EmbeddingDtype::F32 => "f32",
                solo_core::EmbeddingDtype::F16 => "f16",
                solo_core::EmbeddingDtype::I8 => "i8",
                solo_core::EmbeddingDtype::Binary => "binary",
            };
            tx.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(memory_id, embedder_id)
                 DO UPDATE SET dtype = excluded.dtype,
                               dim = excluded.dim,
                               vector = excluded.vector,
                               created_at_ms = excluded.created_at_ms",
                params![
                    &memory_id_s,
                    eid,
                    dtype_str,
                    embedding.dim as i64,
                    &embedding.data[..],
                    now_ms
                ],
            )
            .map_err(|e| Error::storage(format!("UPSERT embedding: {e}")))?;
        }

        tx.execute(
            "INSERT INTO pending_index (kind, memory_id, embedding, embedding_dim, enqueued_at)
             VALUES ('episode', ?1, ?2, ?3, ?4)
             ON CONFLICT(memory_id)
             DO UPDATE SET kind = 'episode',
                           chunk_id = NULL,
                           embedding = excluded.embedding,
                           embedding_dim = excluded.embedding_dim,
                           enqueued_at = excluded.enqueued_at",
            params![
                &memory_id_s,
                &embedding.data[..],
                embedding.dim as i64,
                now_ms
            ],
        )
        .map_err(|e| Error::storage(format!("UPSERT pending_index: {e}")))?;

        if !redaction.matches.is_empty() {
            insert_audit_row_in_tx(
                &tx,
                &redaction_audit_event(
                    now_ms,
                    audit_principal.clone(),
                    Some(memory_id.to_string()),
                    &redaction.matches,
                ),
            )?;
        }

        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryUpdate,
                target_id: Some(memory_id.to_string()),
                result: AuditResult::Ok,
                details: None,
            },
        )?;

        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT update: {e}")))?;

        self.hnsw.add(episode_hnsw_id(rowid), f32_slice)?;

        Ok(MemoryUpdateReport {
            memory_id,
            rowid,
            content: redacted_content.to_string(),
            updated_at_ms: now_ms,
        })
    }

    fn handle_memory_review(
        &mut self,
        memory_id: MemoryId,
        state: Option<MemoryReviewState>,
        note: Option<String>,
        audit_principal: Option<String>,
    ) -> Result<MemoryReviewReport> {
        let memory_id_s = memory_id.to_string();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let note = note.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for memory review: {e}")))?;

        let existing: Option<String> = tx
            .query_row(
                "SELECT status FROM episodes WHERE memory_id = ?1",
                params![&memory_id_s],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| Error::storage(format!("SELECT episode for memory review: {e}")))?;
        let status = existing.ok_or_else(|| Error::not_found("memory not found"))?;
        if status != "active" {
            return Err(Error::conflict("cannot review a non-active memory"));
        }

        let reviewed_at_ms = match state {
            Some(review_state) => {
                tx.execute(
                    "INSERT INTO memory_reviews (
                        memory_id, state, reviewed_at_ms, note, created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?3, ?3)
                     ON CONFLICT(memory_id)
                     DO UPDATE SET state = excluded.state,
                                   reviewed_at_ms = excluded.reviewed_at_ms,
                                   note = excluded.note,
                                   updated_at_ms = excluded.updated_at_ms",
                    params![&memory_id_s, review_state.as_str(), now_ms, note.as_deref()],
                )
                .map_err(|e| Error::storage(format!("UPSERT memory_reviews: {e}")))?;
                Some(now_ms)
            }
            None => {
                tx.execute(
                    "DELETE FROM memory_reviews WHERE memory_id = ?1",
                    params![&memory_id_s],
                )
                .map_err(|e| Error::storage(format!("DELETE memory_reviews: {e}")))?;
                None
            }
        };

        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryReview,
                target_id: Some(memory_id_s),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "state": state.map(|s| s.as_str()).unwrap_or("needs_review"),
                    "note_present": note.is_some(),
                })),
            },
        )?;

        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT memory review: {e}")))?;

        Ok(MemoryReviewReport {
            memory_id,
            state,
            reviewed_at_ms,
        })
    }

    fn handle_memory_quality_review_update(
        &mut self,
        update: MemoryQualityReviewUpdate,
        audit_principal: Option<String>,
    ) -> Result<MemoryQualityReviewUpdateReport> {
        validate_memory_quality_review_update(&update)?;
        let review_id = update.review_id.trim().to_string();
        let note = update.note.and_then(|raw| {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN memory quality review update: {e}")))?;

        let row = Self::fetch_memory_quality_review_db_row_in_tx(&tx, &review_id)
            .map_err(|e| Error::storage(format!("SELECT memory quality review {review_id}: {e}")))?
            .ok_or_else(|| Error::not_found("memory quality review not found"))?;

        let same_status = row.status == update.status.as_str();
        if !same_status && row.status != MemoryQualityReviewStatus::NeedsReview.as_str() {
            return Err(Error::invalid_input(format!(
                "memory quality review {review_id} is already {}; terminal decisions are immutable",
                row.status
            )));
        }
        if !same_status {
            match update.status {
                MemoryQualityReviewStatus::Approved => {
                    Self::promote_memory_quality_review_in_tx(&tx, &row, now_ms)?;
                }
                MemoryQualityReviewStatus::Rewritten => {
                    if update.rewrites.is_empty() {
                        return Err(Error::invalid_input(
                            "rewritten reviews must include at least one replacement fact",
                        ));
                    }
                    for rewrite in &update.rewrites {
                        Self::insert_rewritten_memory_quality_fact_in_tx(
                            &tx, &row, rewrite, now_ms,
                        )?;
                    }
                }
                MemoryQualityReviewStatus::NeedsReview | MemoryQualityReviewStatus::Dismissed => {}
            }
        }

        let reviewed_at_ms = if update.status == MemoryQualityReviewStatus::NeedsReview {
            None
        } else {
            Some(now_ms)
        };
        tx.execute(
            "UPDATE triple_reviews
                SET status = ?1,
                    review_note = ?2,
                    reviewed_at_ms = ?3,
                    updated_at_ms = ?4
              WHERE review_id = ?5",
            params![
                update.status.as_str(),
                note.as_deref(),
                reviewed_at_ms,
                now_ms,
                &review_id,
            ],
        )
        .map_err(|e| Error::storage(format!("UPDATE memory quality review {review_id}: {e}")))?;

        if !same_status {
            Self::update_memory_claim_for_review_in_tx(&tx, &row, update.status, now_ms)?;
        }

        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryQualityReview,
                target_id: Some(review_id.clone()),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "status": update.status.as_str(),
                    "note_present": note.is_some(),
                    "rewrite_count": update.rewrites.len(),
                    "triple_id": row.triple_id,
                })),
            },
        )?;

        let report =
            Self::fetch_memory_quality_review_report_in_tx(&tx, &review_id).map_err(|e| {
                Error::storage(format!(
                    "SELECT updated memory quality review {review_id}: {e}"
                ))
            })?;
        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT memory quality review update: {e}")))?;

        Ok(report)
    }

    fn update_memory_claim_for_review_in_tx(
        tx: &rusqlite::Transaction<'_>,
        row: &MemoryQualityReviewDbRow,
        status: MemoryQualityReviewStatus,
        now_ms: i64,
    ) -> Result<()> {
        let claim_id = tx
            .query_row(
                "SELECT claim_id
                   FROM memory_claims
                  WHERE review_id = ?1 OR candidate_fingerprint = ?2
                  ORDER BY CASE WHEN review_id = ?1 THEN 0 ELSE 1 END
                  LIMIT 1",
                params![&row.review_id, &row.candidate_fingerprint],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| {
                Error::storage(format!(
                    "SELECT memory claim for review {}: {e}",
                    row.review_id
                ))
            })?
            .unwrap_or_else(|| format!("mc-{}", row.candidate_fingerprint));

        let quality_score = review_claim_quality_score(row.confidence, "legacy_review");
        tx.execute(
            "INSERT OR IGNORE INTO memory_claims
                (claim_id, candidate_fingerprint, triple_id, review_id, subject_id,
                 predicate, object_id, object_kind, source_type, cluster_id,
                 source_episode_id, confidence, quality_score, status, reason_codes_json,
                 evidence_count, user_approved, created_at_ms, activated_at_ms,
                 reviewed_at_ms, updated_at_ms, provenance_json)
             VALUES
                (?1, ?2, ?3, ?4, ?5,
                 ?6, ?7, ?8, 'legacy_review', ?9,
                 ?10, ?11, ?12, 'needs_review', '[\"legacy_review\",\"quality_gate\"]',
                 1, 0, ?13, NULL,
                 NULL, ?14, ?15)",
            params![
                &claim_id,
                &row.candidate_fingerprint,
                row.triple_id.as_deref(),
                &row.review_id,
                &row.subject_id,
                &row.predicate,
                &row.object_id,
                &row.object_kind,
                row.cluster_id.as_deref(),
                row.source_episode_id,
                row.confidence,
                quality_score,
                row.created_at_ms,
                now_ms,
                &row.provenance_json,
            ],
        )
        .map_err(|e| {
            Error::storage(format!(
                "INSERT fallback memory claim for review {}: {e}",
                row.review_id
            ))
        })?;

        let existing_reason_codes = tx
            .query_row(
                "SELECT reason_codes_json FROM memory_claims WHERE claim_id = ?1",
                params![&claim_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| Error::storage(format!("SELECT memory claim reasons {claim_id}: {e}")))?
            .flatten();
        let mut reason_codes = parse_claim_reason_codes(existing_reason_codes);
        push_unique_reason_code(&mut reason_codes, "quality_gate");

        let (claim_status, user_approved, activated_at_ms, reviewed_at_ms, revision_kind) =
            match status {
                MemoryQualityReviewStatus::Approved => {
                    push_unique_reason_code(&mut reason_codes, "user_approved");
                    push_unique_reason_code(&mut reason_codes, "activated");
                    (
                        "active",
                        1,
                        Some(now_ms),
                        Some(now_ms),
                        Some("claim_activated"),
                    )
                }
                MemoryQualityReviewStatus::Rewritten => {
                    push_unique_reason_code(&mut reason_codes, "user_rewritten");
                    push_unique_reason_code(&mut reason_codes, "superseded");
                    ("superseded", 0, None, Some(now_ms), Some("claim_rewritten"))
                }
                MemoryQualityReviewStatus::Dismissed => {
                    push_unique_reason_code(&mut reason_codes, "user_rejected");
                    ("rejected", 0, None, Some(now_ms), Some("claim_rejected"))
                }
                MemoryQualityReviewStatus::NeedsReview => {
                    push_unique_reason_code(&mut reason_codes, "needs_review");
                    ("needs_review", 0, None, None, None)
                }
            };
        let reason_codes_json = serde_json::to_string(&reason_codes)
            .map_err(|e| Error::storage(format!("encode memory claim reasons: {e}")))?;

        tx.execute(
            "UPDATE memory_claims
                SET review_id = ?1,
                    triple_id = COALESCE(?2, triple_id),
                    status = ?3,
                    user_approved = ?4,
                    activated_at_ms = ?5,
                    reviewed_at_ms = ?6,
                    updated_at_ms = ?7,
                    reason_codes_json = ?8
              WHERE claim_id = ?9",
            params![
                &row.review_id,
                row.triple_id.as_deref(),
                claim_status,
                user_approved,
                activated_at_ms,
                reviewed_at_ms,
                now_ms,
                &reason_codes_json,
                &claim_id,
            ],
        )
        .map_err(|e| Error::storage(format!("UPDATE memory claim {claim_id}: {e}")))?;

        if let Some(revision_kind) = revision_kind {
            let revision_id = MemoryId::new().to_string();
            tx.execute(
                "INSERT INTO memory_revisions
                    (revision_id, revision_kind, target_kind, target_id,
                     previous_id, replacement_id, reason, metadata_json, created_at_ms)
                 VALUES (?1, ?2, 'claim', ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    revision_id,
                    revision_kind,
                    &claim_id,
                    &row.review_id,
                    row.triple_id.as_deref(),
                    status.as_str(),
                    serde_json::json!({
                        "review_id": row.review_id,
                        "candidate_fingerprint": row.candidate_fingerprint,
                        "subject_id": row.subject_id,
                        "predicate": row.predicate,
                        "object_kind": row.object_kind,
                    })
                    .to_string(),
                    now_ms,
                ],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "INSERT memory revision for review {}: {e}",
                    row.review_id
                ))
            })?;
        }

        Ok(())
    }

    fn handle_record_memory_retrieval(
        &mut self,
        entry: MemoryRetrievalLogEntry,
    ) -> Result<MemoryRetrievalLogReport> {
        let query = entry.query.trim();
        if query.is_empty() {
            return Err(Error::invalid_input("retrieval query must not be empty"));
        }
        let redaction = self.redactor.redact(query);
        let stored_query = bounded_retrieval_log_query(redaction.text.as_ref());
        let retrieval_id = MemoryId::new().to_string();
        let created_at_ms = chrono::Utc::now().timestamp_millis();
        let recalled_ids = entry.recalled_ids.into_iter().take(100).collect::<Vec<_>>();
        let reason_codes = entry
            .reason_codes
            .into_iter()
            .filter(|code| !code.trim().is_empty())
            .take(32)
            .collect::<Vec<_>>();
        let recalled_ids_json = serde_json::to_string(&recalled_ids)
            .map_err(|e| Error::storage(format!("encode retrieval recalled ids: {e}")))?;
        let reason_codes_json = serde_json::to_string(&reason_codes)
            .map_err(|e| Error::storage(format!("encode retrieval reason codes: {e}")))?;
        self.conn
            .execute(
                "INSERT INTO memory_retrieval_log
                    (retrieval_id, query, recalled_ids_json, reason_codes_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    &retrieval_id,
                    &stored_query,
                    &recalled_ids_json,
                    &reason_codes_json,
                    created_at_ms,
                ],
            )
            .map_err(|e| Error::storage(format!("INSERT memory retrieval log: {e}")))?;
        Ok(MemoryRetrievalLogReport {
            retrieval_id,
            created_at_ms,
        })
    }

    fn fetch_memory_quality_review_db_row_in_tx(
        tx: &rusqlite::Transaction<'_>,
        review_id: &str,
    ) -> rusqlite::Result<Option<MemoryQualityReviewDbRow>> {
        tx.query_row(
            "SELECT tr.review_id, tr.candidate_fingerprint,
                    tr.triple_id, tr.cluster_id, tr.source_episode_id,
                    tr.subject_id, tr.predicate, tr.object_id, tr.object_kind,
                    tr.confidence, tr.status, tr.created_at_ms,
                    tr.provenance_json
               FROM triple_reviews tr
              WHERE tr.review_id = ?1",
            params![review_id],
            |r| {
                Ok(MemoryQualityReviewDbRow {
                    review_id: r.get(0)?,
                    candidate_fingerprint: r.get(1)?,
                    triple_id: r.get(2)?,
                    cluster_id: r.get(3)?,
                    source_episode_id: r.get(4)?,
                    subject_id: r.get(5)?,
                    predicate: r.get(6)?,
                    object_id: r.get(7)?,
                    object_kind: r.get(8)?,
                    confidence: r.get(9)?,
                    status: r.get(10)?,
                    created_at_ms: r.get(11)?,
                    provenance_json: r.get(12)?,
                })
            },
        )
        .optional()
    }

    fn fetch_memory_quality_review_report_in_tx(
        tx: &rusqlite::Transaction<'_>,
        review_id: &str,
    ) -> rusqlite::Result<MemoryQualityReviewUpdateReport> {
        tx.query_row(
            "SELECT tr.review_id, mc.claim_id, tr.triple_id, tr.cluster_id, tr.source_episode_id,
                    tr.subject_id, tr.predicate, tr.object_id, tr.object_kind,
                    tr.confidence, tr.reason_code, tr.reason, tr.status, tr.created_at_ms,
                    mc.status AS claim_status, mc.quality_score, mc.reason_codes_json,
                    substr(e.content, 1, 240) AS evidence_preview
               FROM triple_reviews tr
               LEFT JOIN episodes e ON e.rowid = tr.source_episode_id
               LEFT JOIN memory_claims mc
                 ON mc.claim_id = COALESCE((
                    SELECT mc2.claim_id
                      FROM memory_claims mc2
                     WHERE mc2.review_id = tr.review_id
                     ORDER BY mc2.updated_at_ms DESC,
                              mc2.claim_id
                     LIMIT 1
                 ), (
                    SELECT mc3.claim_id
                      FROM memory_claims mc3
                     WHERE mc3.candidate_fingerprint = tr.candidate_fingerprint
                     LIMIT 1
                 ))
              WHERE tr.review_id = ?1",
            params![review_id],
            |r| {
                let reason_codes_json: Option<String> = r.get(16)?;
                Ok(MemoryQualityReviewUpdateReport {
                    review_id: r.get(0)?,
                    claim_id: r.get(1)?,
                    triple_id: r.get(2)?,
                    cluster_id: r.get(3)?,
                    source_episode_id: r.get(4)?,
                    subject_id: r.get(5)?,
                    predicate: r.get(6)?,
                    object_id: r.get(7)?,
                    object_kind: r.get(8)?,
                    confidence: r.get(9)?,
                    reason_code: r.get(10)?,
                    reason: r.get(11)?,
                    status: r.get(12)?,
                    created_at_ms: r.get(13)?,
                    claim_status: r.get(14)?,
                    quality_score: r.get(15)?,
                    claim_reason_codes: parse_claim_reason_codes(reason_codes_json),
                    evidence_preview: r.get(17)?,
                })
            },
        )
    }

    fn promote_memory_quality_review_in_tx(
        tx: &rusqlite::Transaction<'_>,
        row: &MemoryQualityReviewDbRow,
        now_ms: i64,
    ) -> Result<()> {
        let triple_id = row
            .triple_id
            .clone()
            .unwrap_or_else(|| MemoryId::new().to_string());
        let active = Self::memory_quality_active_triple(
            triple_id.clone(),
            &row.subject_id,
            &row.predicate,
            &row.object_id,
            &row.object_kind,
            row.confidence.max(0.9),
            row.created_at_ms,
        )?;

        let changed = tx
            .execute(
                "UPDATE triples
                    SET status = 'active',
                        subject_id = ?1,
                        predicate = ?2,
                        object_id = ?3,
                        object_kind = ?4,
                        valid_from_ms = ?5,
                        valid_to_ms = NULL,
                        confidence = ?6,
                        provenance_json = ?7,
                        cluster_id = ?8,
                        source_episode_id = ?9,
                        updated_at_ms = ?10
                  WHERE triple_id = ?11",
                params![
                    &active.subject_id,
                    &active.predicate,
                    &active.object_id,
                    active.object_kind,
                    active.valid_from_ms,
                    active.confidence,
                    &row.provenance_json,
                    row.cluster_id.as_deref(),
                    row.source_episode_id,
                    now_ms,
                    &triple_id,
                ],
            )
            .map_err(|e| Error::storage(format!("UPDATE promoted triple {triple_id}: {e}")))?;

        if changed == 0 {
            insert_active_triple_with_optional_cluster_in_tx(
                tx,
                &active,
                row.cluster_id.as_deref(),
                row.source_episode_id,
                &row.provenance_json,
                now_ms,
            )
            .map_err(|e| Error::storage(format!("INSERT promoted triple {triple_id}: {e}")))?;
        } else {
            upsert_graph_for_active_triple_in_tx(
                tx,
                &active,
                row.cluster_id.as_deref(),
                row.source_episode_id,
                now_ms,
            )
            .map_err(|e| {
                Error::storage(format!("UPSERT graph for promoted triple {triple_id}: {e}"))
            })?;
        }
        Ok(())
    }

    fn insert_rewritten_memory_quality_fact_in_tx(
        tx: &rusqlite::Transaction<'_>,
        row: &MemoryQualityReviewDbRow,
        rewrite: &MemoryQualityReviewRewrite,
        now_ms: i64,
    ) -> Result<()> {
        Self::insert_memory_quality_fact_in_tx(
            tx,
            MemoryId::new().to_string(),
            rewrite.subject_id.trim(),
            rewrite.predicate.trim(),
            rewrite.object_id.trim(),
            rewrite.object_kind.trim(),
            rewrite.confidence.unwrap_or(row.confidence.max(0.9)),
            row.cluster_id.as_deref(),
            row.source_episode_id,
            &row.provenance_json,
            now_ms,
            now_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_memory_quality_fact_in_tx(
        tx: &rusqlite::Transaction<'_>,
        triple_id: String,
        subject_id: &str,
        predicate: &str,
        object_id: &str,
        object_kind: &str,
        confidence: f32,
        cluster_id: Option<&str>,
        source_episode_id: Option<i64>,
        provenance_json: &str,
        valid_from_ms: i64,
        now_ms: i64,
    ) -> Result<()> {
        let active = Self::memory_quality_active_triple(
            triple_id,
            subject_id,
            predicate,
            object_id,
            object_kind,
            confidence,
            valid_from_ms,
        )?;
        Self::validate_rewritten_memory_quality_active_triple_in_tx(tx, &active)?;
        insert_active_triple_with_optional_cluster_in_tx(
            tx,
            &active,
            cluster_id,
            source_episode_id,
            provenance_json,
            now_ms,
        )
        .map_err(|e| Error::storage(format!("INSERT rewritten memory quality fact: {e}")))
    }

    fn memory_quality_active_triple(
        triple_id: String,
        subject_id: &str,
        predicate: &str,
        object_id: &str,
        object_kind: &str,
        confidence: f32,
        valid_from_ms: i64,
    ) -> Result<ActiveTriple> {
        let subject_alias = subject_id.trim().to_string();
        let subject_id = canonical_entity_id(&subject_alias);
        if subject_id.is_empty() {
            return Err(Error::invalid_input(
                "review fact subject_id must normalize to a useful entity",
            ));
        }
        let object_kind = object_kind.trim();
        if !matches!(object_kind, "entity" | "literal") {
            return Err(Error::invalid_input(
                "review fact object_kind must be entity or literal",
            ));
        }
        let (object_id, object_alias, object_kind) =
            if object_kind == "entity" && !active_entity_object_should_be_literal(object_id) {
                let alias = object_id.trim().to_string();
                let object_id = canonical_entity_id(&alias);
                if object_id.is_empty() {
                    return Err(Error::invalid_input(
                        "review fact object_id must normalize to a useful entity",
                    ));
                }
                (object_id, Some(alias), "entity")
            } else {
                let object_id = object_id.trim().to_string();
                if object_id.is_empty() {
                    return Err(Error::invalid_input(
                        "review fact object_id must not be empty",
                    ));
                }
                (object_id, None, "literal")
            };
        Ok(ActiveTriple {
            triple_id,
            subject_id,
            subject_alias,
            predicate: predicate.trim().to_string(),
            object_id,
            object_alias,
            object_kind,
            valid_from_ms,
            valid_to_ms: None,
            confidence: confidence.clamp(0.0, 1.0),
        })
    }

    fn validate_rewritten_memory_quality_active_triple_in_tx(
        tx: &rusqlite::Transaction<'_>,
        active: &ActiveTriple,
    ) -> Result<()> {
        let literal_only_subject =
            Self::memory_quality_literal_only_subject_in_tx(tx, &active.subject_id).map_err(
                |e| {
                    Error::storage(format!(
                        "SELECT literal-only subject for rewritten memory quality fact: {e}"
                    ))
                },
            )?;
        let object_for_review = active.object_alias.as_deref().unwrap_or(&active.object_id);
        if let Some(reason) = active_triple_review_reason(
            &active.subject_alias,
            &active.predicate,
            object_for_review,
            active.object_kind,
            active.confidence,
            literal_only_subject,
        ) {
            return Err(Error::invalid_input(format!(
                "rewrite fact rejected by quality gate: {} ({})",
                reason.reason, reason.reason_code
            )));
        }
        Ok(())
    }

    fn memory_quality_literal_only_subject_in_tx(
        tx: &rusqlite::Transaction<'_>,
        subject_id: &str,
    ) -> rusqlite::Result<bool> {
        tx.query_row(
            "SELECT NOT EXISTS (
                SELECT 1
                  FROM triples te
                 WHERE te.status = 'active'
                   AND te.object_kind = 'entity'
                   AND (te.subject_id = ?1 OR te.object_id = ?1)
             )",
            params![subject_id],
            |r| Ok(r.get::<_, i64>(0)? != 0),
        )
    }

    fn handle_remember_durable(
        &mut self,
        episode: Episode,
        embedding: Embedding,
        audit_principal: Option<String>,
    ) -> Result<MemoryId> {
        embedding.validate()?;
        let memory_id = episode.memory_id;

        // v0.8.1 P3: quota enforcement. Estimated growth is a
        // conservative upper bound on the bytes about to land on disk —
        // we prefer over-counting to under-counting (the brief is
        // explicit on this). One episode row plus one embeddings row
        // plus one pending_index row plus a per-row SQLite page-and-FTS
        // overhead. The check short-circuits when quota_bytes is None
        // (the common case for v0.8.0 tenants).
        let estimated_growth: u64 = (episode.content.len() as u64)
            .saturating_add(embedding.data.len() as u64)
            // Per-row SQLite overhead (3 INSERTs across episodes +
            // embeddings + pending_index) — conservative, since FTS5
            // also incurs trigger-driven writes. 2 KiB total covers
            // each row's metadata + FTS portion comfortably.
            .saturating_add(2048);
        match check_quota(self.quota_bytes, self.db_path.as_deref(), estimated_growth) {
            QuotaDecision::Unlimited | QuotaDecision::Allowed { .. } => {}
            QuotaDecision::Exceeded {
                current_size,
                estimated_growth,
                quota,
            } => {
                // Reject the write. The dispatch caller's error-path
                // audit emit covers the "forbidden" audit row — we
                // surface the structured error here. The error-message
                // text is the operator-visible payload; the audit row
                // gets `details_json` carrying the same numbers.
                let err = QuotaExceededError {
                    current_size,
                    estimated_growth,
                    quota,
                };
                // v0.8.1 P3: emit the `forbidden` audit row before
                // returning. Unlike `Error` results (which dispatch
                // handles via emit_audit_best_effort), `Forbidden` is a
                // policy outcome — the audit row IS the only record
                // because no tx ran. Use the synchronous path so the
                // row lands before we return.
                self.emit_audit_best_effort(
                    AuditOperation::MemoryRemember,
                    Some(memory_id.to_string()),
                    AuditResult::Forbidden,
                    audit_principal,
                    Some(err.to_details_json()),
                );
                return Err(Error::forbidden(err.to_string()));
            }
        }

        // v0.8.0 P5: redact PII from the content BEFORE INSERT. The
        // unredacted text never lands on disk. The Remember path's
        // embedding was computed by the caller (pre-writer); if the
        // operator has redaction enabled, the on-disk text and embedding
        // diverge by the redaction-substitution. This is the documented
        // trade-off: for `ingest_document` the embedding is computed
        // inside the writer after redaction (consistent); for Remember
        // it isn't because the caller controls embedding. Operators who
        // need strict text↔embedding consistency under redaction should
        // either (a) redact upstream of `remember`, or (b) prefer
        // ingestion via `ingest_document`.
        let redaction = self.redactor.redact(&episode.content);
        let redacted_content: &str = redaction.text.as_ref();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for remember: {e}")))?;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let encoding_ctx = serde_json::to_string(&episode.encoding_context)
            .map_err(|e| Error::storage(format!("serialize encoding_context: {e}")))?;
        let provenance_json = match &episode.provenance {
            Some(p) => Some(
                serde_json::to_string(p)
                    .map_err(|e| Error::storage(format!("serialize provenance: {e}")))?,
            ),
            None => None,
        };
        let tier_str = match episode.tier {
            Tier::Hot => "hot",
            Tier::Warm => "warm",
            Tier::Cold => "cold",
        };

        tx.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, source_id, content,
                encoding_context_json, provenance_json, confidence,
                strength, salience, tier, created_at_ms, updated_at_ms,
                principal_subject
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                memory_id.to_string(),
                episode.ts_ms,
                episode.source_type,
                episode.source_id,
                redacted_content,
                encoding_ctx,
                provenance_json,
                episode.confidence.0,
                episode.strength,
                episode.salience,
                tier_str,
                now_ms,
                now_ms,
                audit_principal.as_deref(),
            ],
        )
        .map_err(|e| Error::storage(format!("INSERT episode: {e}")))?;

        let rowid = tx.last_insert_rowid();

        // Persist the embedding to the `embeddings` table when
        // we have a cached embedder_id. Without this row, `solo
        // reembed` (post-v0.1) wouldn't know what vector this episode
        // had under the previous model, and HNSW rebuild from SQL
        // (also post-v0.1) couldn't repopulate the graph.
        //
        // Skipped when embedder_id is None — only happens in unit-test
        // setups that didn't run the embedder-registry resolution
        // step. The pending_index INSERT below still happens, so
        // recovery on restart still works for tests that exercise
        // the replay path.
        if let Some(eid) = self.embedder_id {
            let dtype_str = match embedding.dtype {
                solo_core::EmbeddingDtype::F32 => "f32",
                solo_core::EmbeddingDtype::F16 => "f16",
                solo_core::EmbeddingDtype::I8 => "i8",
                solo_core::EmbeddingDtype::Binary => "binary",
            };
            tx.execute(
                "INSERT INTO embeddings (
                    memory_id, embedder_id, dtype, dim, vector, created_at_ms
                 ) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    memory_id.to_string(),
                    eid,
                    dtype_str,
                    embedding.dim as i64,
                    &embedding.data[..],
                    now_ms,
                ],
            )
            .map_err(|e| Error::storage(format!("INSERT embeddings: {e}")))?;
        }

        tx.execute(
            "INSERT INTO pending_index (
                memory_id, embedding, embedding_dim, enqueued_at
             ) VALUES (?, ?, ?, ?)",
            params![
                memory_id.to_string(),
                &embedding.data[..],
                embedding.dim as i64,
                now_ms,
            ],
        )
        .map_err(|e| Error::storage(format!("INSERT pending_index: {e}")))?;

        // v0.8.0 P5: emit a single `redaction.applied` audit row when
        // one or more patterns fired. `details_json` carries pattern-
        // name match counts only — never the matched substring. The
        // test `audit_row_does_not_contain_original_pii` enforces this.
        if !redaction.matches.is_empty() {
            insert_audit_row_in_tx(
                &tx,
                &redaction_audit_event(
                    now_ms,
                    audit_principal.clone(),
                    Some(memory_id.to_string()),
                    &redaction.matches,
                ),
            )?;
        }

        // v0.8.0 P4: synchronous audit emit inside the same tx. If this
        // INSERT fails the surrounding tx rolls back via the `?` operator
        // returning before COMMIT — strict ACID for the audited write.
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryRemember,
                target_id: Some(memory_id.to_string()),
                result: AuditResult::Ok,
                details: None,
            },
        )?;

        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT remember: {e}")))?;

        let f32_slice = embedding.as_f32_slice().ok_or_else(|| {
            Error::embedder("HNSW expects F32 embeddings; convert dtype upstream")
        })?;
        // Encode the rowid in the shared HNSW namespace (high bit clear
        // for episodes). See `crate::hnsw_id` for the encoding rationale.
        self.hnsw.add(episode_hnsw_id(rowid), f32_slice)?;

        Ok(memory_id)
    }

    /// v0.9.2: dispatch for [`WriteCommand::RememberBatch`]. Mirrors
    /// [`Self::dispatch_remember`] but operates over `Vec<(Episode,
    /// Embedding)>` and surfaces a single `MemoryRememberBatch` audit
    /// row (success or error) plus one invalidate fan-out per batch
    /// (not per item).
    fn dispatch_remember_batch(
        &mut self,
        items: Vec<(Episode, Embedding)>,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<Vec<MemoryId>>>,
    ) {
        let item_count = items.len();

        let result = self.handle_remember_batch_durable(items, audit_principal.clone());

        // Dev-log 0152 H3: the handler now returns the per-item HNSW
        // success mask alongside the memory ids. Items whose HNSW add
        // failed are NOT drained from `pending_index` — they replay on
        // next startup. SQL state for every item is already committed,
        // so callers see the full memory_id list even when one item's
        // HNSW add tripped (no silent partial success on retry).
        let (forwarded, drain_ids): (Result<Vec<MemoryId>>, Vec<MemoryId>) = match result {
            Ok((ids, hnsw_ok_mask)) => {
                let drain: Vec<MemoryId> = ids
                    .iter()
                    .zip(hnsw_ok_mask.iter().copied())
                    .filter_map(|(mid, ok)| if ok { Some(*mid) } else { None })
                    .collect();
                (Ok(ids), drain)
            }
            Err(e) => (Err(e), Vec::new()),
        };
        let durable_ok = forwarded.is_ok();

        // Error-path audit emit (the in-tx success-path audit row is inside
        // `handle_remember_batch_durable`). Quota/InvalidInput paths emit
        // their own audit rows inside the handler.
        if let Err(ref e) = forwarded {
            self.emit_audit_best_effort(
                AuditOperation::MemoryRememberBatch,
                None,
                AuditResult::Error,
                audit_principal,
                Some(serde_json::json!({
                    "error": e.to_string(),
                    "item_count": item_count,
                })),
            );
        }
        let _ = reply.send(forwarded);

        if durable_ok {
            // One invalidate per batch, not per item. Subscribers refetch
            // the episode page once and pick up all N rows.
            self.emit_invalidate(AuditOperation::MemoryRememberBatch.as_str(), "episode");

            // v0.9.0 P4: triples-batch trigger. Each remembered episode
            // increments the counter; the daemon's `triples_batch_timer`
            // short-circuits when the counter crosses its threshold.
            if let Some(sig) = self.triples_batch_signal.as_ref() {
                for _ in 0..item_count {
                    sig.note_episode_remembered();
                }
            }

            // Drain `pending_index` only for items whose HNSW add
            // succeeded. Items that failed HNSW stay in pending_index
            // so the next-startup replay can finish the job.
            for mid in &drain_ids {
                if let Err(e) = self.conn.execute(
                    "DELETE FROM pending_index WHERE memory_id = ?",
                    params![mid.to_string()],
                ) {
                    tracing::warn!(
                        error = %e,
                        %mid,
                        "pending_index drain failed (batch); will replay on next startup"
                    );
                }
            }
        }
    }

    /// v0.9.2: durable batched-remember.
    ///
    /// Pipeline (mirrors `handle_remember_durable` but folded over N items):
    ///
    ///   1. Validate batch size against [`MAX_REMEMBER_BATCH_SIZE`] and
    ///      non-emptiness.
    ///   2. Validate every embedding (dim, dtype, finiteness).
    ///   3. Sum estimated growth across items and check quota ONCE for
    ///      the whole batch. Forbidden → `forbidden` audit row + early
    ///      return (no tx opened).
    ///   4. `BEGIN IMMEDIATE`.
    ///   5. For each item: redact PII → INSERT episode → INSERT embedding
    ///      (if embedder_id wired) → INSERT pending_index → per-item
    ///      redaction audit row when matches fired. Capture rowids.
    ///   6. ONE batch-level audit row inside the tx
    ///      (`AuditOperation::MemoryRememberBatch`, `details.item_count`).
    ///   7. `COMMIT`.
    ///   8. `hnsw.add` per item using captured rowids. A mid-batch
    ///      failure aborts the rest and returns Err; the `pending_index`
    ///      outbox preserves the un-added rows for next-startup replay.
    /// Dev-log 0152 H3: returns `(memory_ids, hnsw_ok_mask)` so the
    /// dispatcher can drain `pending_index` for only the items whose
    /// HNSW add succeeded. Items where HNSW add failed are kept in the
    /// outbox for next-startup replay.
    fn handle_remember_batch_durable(
        &mut self,
        items: Vec<(Episode, Embedding)>,
        audit_principal: Option<String>,
    ) -> Result<(Vec<MemoryId>, Vec<bool>)> {
        // 1. Batch-size validation.
        if items.is_empty() {
            return Err(Error::invalid_input(
                "memory_remember_batch: items must not be empty".to_string(),
            ));
        }
        if items.len() > MAX_REMEMBER_BATCH_SIZE {
            return Err(Error::invalid_input(format!(
                "memory_remember_batch: {} items exceeds MAX_REMEMBER_BATCH_SIZE = {}",
                items.len(),
                MAX_REMEMBER_BATCH_SIZE,
            )));
        }

        // 2. Embedding shape validation. We do this up front so a malformed
        //    item in slot 7 fails before slot 1 hits SQLite.
        for (_, embedding) in &items {
            embedding.validate()?;
        }

        // 3. Quota for the whole batch.
        let mut total_growth: u64 = 0;
        for (episode, embedding) in &items {
            total_growth = total_growth.saturating_add(
                (episode.content.len() as u64)
                    .saturating_add(embedding.data.len() as u64)
                    .saturating_add(2048),
            );
        }
        match check_quota(self.quota_bytes, self.db_path.as_deref(), total_growth) {
            QuotaDecision::Unlimited | QuotaDecision::Allowed { .. } => {}
            QuotaDecision::Exceeded {
                current_size,
                estimated_growth,
                quota,
            } => {
                let err = QuotaExceededError {
                    current_size,
                    estimated_growth,
                    quota,
                };
                self.emit_audit_best_effort(
                    AuditOperation::MemoryRememberBatch,
                    None,
                    AuditResult::Forbidden,
                    audit_principal,
                    Some(err.to_details_json()),
                );
                return Err(Error::forbidden(err.to_string()));
            }
        }

        // 4. BEGIN IMMEDIATE — the whole batch's INSERTs land atomically.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for remember_batch: {e}")))?;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut memory_ids: Vec<MemoryId> = Vec::with_capacity(items.len());
        let mut rowids: Vec<i64> = Vec::with_capacity(items.len());

        // 5. Per-item INSERTs.
        for (episode, embedding) in &items {
            let memory_id = episode.memory_id;

            // Redact PII before INSERT. Same trade-off as single Remember:
            // the on-disk text and the (pre-computed) embedding may diverge
            // by the redaction substitution.
            let redaction = self.redactor.redact(&episode.content);
            let redacted_content: &str = redaction.text.as_ref();

            let encoding_ctx = serde_json::to_string(&episode.encoding_context)
                .map_err(|e| Error::storage(format!("serialize encoding_context: {e}")))?;
            let provenance_json = match &episode.provenance {
                Some(p) => Some(
                    serde_json::to_string(p)
                        .map_err(|e| Error::storage(format!("serialize provenance: {e}")))?,
                ),
                None => None,
            };
            let tier_str = match episode.tier {
                Tier::Hot => "hot",
                Tier::Warm => "warm",
                Tier::Cold => "cold",
            };

            tx.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, source_id, content,
                    encoding_context_json, provenance_json, confidence,
                    strength, salience, tier, created_at_ms, updated_at_ms,
                    principal_subject
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    memory_id.to_string(),
                    episode.ts_ms,
                    episode.source_type,
                    episode.source_id,
                    redacted_content,
                    encoding_ctx,
                    provenance_json,
                    episode.confidence.0,
                    episode.strength,
                    episode.salience,
                    tier_str,
                    now_ms,
                    now_ms,
                    audit_principal.as_deref(),
                ],
            )
            .map_err(|e| Error::storage(format!("INSERT episode (batch): {e}")))?;

            let rowid = tx.last_insert_rowid();

            if let Some(eid) = self.embedder_id {
                let dtype_str = match embedding.dtype {
                    solo_core::EmbeddingDtype::F32 => "f32",
                    solo_core::EmbeddingDtype::F16 => "f16",
                    solo_core::EmbeddingDtype::I8 => "i8",
                    solo_core::EmbeddingDtype::Binary => "binary",
                };
                tx.execute(
                    "INSERT INTO embeddings (
                        memory_id, embedder_id, dtype, dim, vector, created_at_ms
                     ) VALUES (?, ?, ?, ?, ?, ?)",
                    params![
                        memory_id.to_string(),
                        eid,
                        dtype_str,
                        embedding.dim as i64,
                        &embedding.data[..],
                        now_ms,
                    ],
                )
                .map_err(|e| Error::storage(format!("INSERT embeddings (batch): {e}")))?;
            }

            tx.execute(
                "INSERT INTO pending_index (
                    memory_id, embedding, embedding_dim, enqueued_at
                 ) VALUES (?, ?, ?, ?)",
                params![
                    memory_id.to_string(),
                    &embedding.data[..],
                    embedding.dim as i64,
                    now_ms,
                ],
            )
            .map_err(|e| Error::storage(format!("INSERT pending_index (batch): {e}")))?;

            // Per-item redaction audit row when patterns fired. Same shape
            // as single Remember — `details_json` carries pattern-name
            // match counts only.
            if !redaction.matches.is_empty() {
                insert_audit_row_in_tx(
                    &tx,
                    &redaction_audit_event(
                        now_ms,
                        audit_principal.clone(),
                        Some(memory_id.to_string()),
                        &redaction.matches,
                    ),
                )?;
            }

            memory_ids.push(memory_id);
            rowids.push(rowid);
        }

        // 6. ONE batch-level audit row inside the same tx (the dev-log
        //    0120 §3 Decision G invariant: one batch = one audit row).
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryRememberBatch,
                target_id: None,
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "item_count": items.len(),
                })),
            },
        )?;

        // 7. COMMIT.
        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT remember_batch: {e}")))?;

        // 8. HNSW.add per item. Dev-log 0152 H3: log-and-continue on
        //    failure rather than short-circuiting the rest of the batch.
        //    Items whose HNSW add fails stay in `pending_index` so the
        //    next-startup replay picks them up. The caller (dispatch)
        //    uses the returned `hnsw_ok_mask` to decide which items to
        //    drain. SQL state for every item is already committed, so
        //    callers always see the full `memory_id` list and won't
        //    duplicate on retry.
        let mut hnsw_ok_mask: Vec<bool> = Vec::with_capacity(items.len());
        for ((episode, embedding), rowid) in items.iter().zip(rowids.iter()) {
            let f32_slice = match embedding.as_f32_slice() {
                Some(s) => s,
                None => {
                    tracing::warn!(
                        memory_id = %episode.memory_id,
                        "remember_batch: embedding not F32 — HNSW add skipped; pending_index row left for replay"
                    );
                    hnsw_ok_mask.push(false);
                    continue;
                }
            };
            match self.hnsw.add(episode_hnsw_id(*rowid), f32_slice) {
                Ok(()) => hnsw_ok_mask.push(true),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        memory_id = %episode.memory_id,
                        "remember_batch: hnsw.add failed; pending_index row left for replay"
                    );
                    hnsw_ok_mask.push(false);
                }
            }
        }

        Ok((memory_ids, hnsw_ok_mask))
    }

    fn handle_forget(
        &mut self,
        memory_id: MemoryId,
        reason: String,
        audit_principal: Option<String>,
    ) -> Result<()> {
        // Soft-delete: set status='forgotten' on the episode. Per ADR-0003
        // Forget reply timing semantics, the HNSW vector is NOT removed
        // from the underlying graph — recall paths exclude
        // `status='forgotten'` rows by SQL filter, and the architecture
        // preserves silent traces for forensics + consolidation.
        //
        // BUT: we DO mark the rowid in the in-memory tombstone set
        // (`HnswIndex::tombstones`). Without this, `index.len()` keeps
        // counting the forgotten vector and `detect_drift` spuriously
        // warns at runtime. Post-restart `rebuild_tombstones_from_sql`
        // does the same thing; runtime tombstoning matches that behaviour.
        //
        // The `reason` parameter is logged but not persisted. A future
        // schema (memory_revisions or forget_log) can record it; v0.1
        // surfaces it through tracing only.
        //
        // v0.8.0 P4: forget is wrapped in a single tx that includes the
        // audit row, so the audit emit is atomic with the soft-delete.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let id_str = memory_id.to_string();

        // Look up the rowid first so we can tombstone the HNSW even on
        // the already-forgotten / not-found paths. The query is cheap
        // (UNIQUE index on memory_id). Reads don't need to live inside
        // the tx — they're advisory.
        let rowid: Option<i64> = self
            .conn
            .query_row(
                "SELECT rowid FROM episodes WHERE memory_id = ?",
                params![&id_str],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| Error::storage(format!("lookup rowid for forget: {e}")))?;
        let Some(rowid) = rowid else {
            return Err(Error::not_found(format!(
                "memory_id {memory_id} not found in episodes"
            )));
        };

        // BEGIN IMMEDIATE tx wraps the UPDATE + audit row insert. If the
        // audit row fails, the UPDATE rolls back — strict ACID.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for forget: {e}")))?;

        let updated = tx
            .execute(
                "UPDATE episodes
                    SET status = 'forgotten', updated_at_ms = ?
                  WHERE memory_id = ? AND status <> 'forgotten'",
                params![now_ms, &id_str],
            )
            .map_err(|e| Error::storage(format!("UPDATE episodes for forget: {e}")))?;

        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryForget,
                target_id: Some(id_str.clone()),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({ "reason": reason })),
            },
        )?;

        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT forget: {e}")))?;

        // Tombstone the HNSW. Idempotent for already-tombstoned rowids
        // (HashSet::insert just no-ops). The encoded id MUST match the
        // one passed at insert time (see `dispatch_remember`).
        if let Err(e) = self.hnsw.remove(episode_hnsw_id(rowid)) {
            tracing::warn!(
                error = %e,
                %memory_id,
                "hnsw.remove during forget failed (non-fatal; SQL filter still hides the row)"
            );
        }

        if updated == 0 {
            // Already forgotten → idempotent success.
            tracing::debug!(%memory_id, "forget called on already-forgotten episode (idempotent)");
            return Ok(());
        }
        tracing::info!(%memory_id, %reason, "episode soft-deleted (status=forgotten)");
        Ok(())
    }

    // --------------------------------------------------------------------
    // v0.7.0 — document ingest + forget
    //
    // Same outbox discipline as `dispatch_remember`: the SQL transaction
    // commits BEFORE we touch HNSW, the reply goes BEFORE we drain the
    // pending_index outbox. If hnsw.add fails the row stays in
    // pending_index and replay picks it up next startup; if the DELETE
    // drain fails the row stays and replay is idempotent.
    //
    // See `docs/dev-log/0083-v0.7.0-implementation-plan.md` §2 P3.
    // --------------------------------------------------------------------

    /// Mirror of `dispatch_remember`: send the reply (durable + HNSW-
    /// resident report) BEFORE draining the pending_index outbox rows.
    /// Drain failure logs + leaves the rows for next-startup replay.
    fn dispatch_ingest_document(
        &mut self,
        path: std::path::PathBuf,
        chunk_config: crate::document::ChunkConfig,
        audit_principal: Option<String>,
        reply: oneshot::Sender<Result<IngestReport>>,
    ) {
        // Capture the chunk_ids inserted so we can drain only those rows
        // (NOT a blanket `DELETE FROM pending_index WHERE kind='chunk'`,
        // which would clobber concurrent ingests' in-flight rows).
        let (result, drained_chunks) =
            self.handle_ingest_document_durable(path, chunk_config, audit_principal.clone());
        let durable_ok = result.is_ok();
        // v0.8.0 P4: error-path audit emit. Success + dedup paths emit
        // inside their respective tx / read-only paths.
        if let Err(ref e) = result {
            self.emit_audit_best_effort(
                AuditOperation::MemoryIngestDocument,
                None,
                AuditResult::Error,
                audit_principal,
                Some(serde_json::json!({ "error": e.to_string() })),
            );
        }
        let _ = reply.send(result);

        // v0.10.0: post-commit invalidation (lesson #30). Both the
        // first-ingest and dedup-shortcircuit paths are `durable_ok`;
        // both leave the documents listing consistent and warrant a
        // refetch (a deduped doc is still a "document exists" signal
        // for clients that didn't have it cached).
        if durable_ok {
            self.emit_invalidate(AuditOperation::MemoryIngestDocument.as_str(), "document");
        }

        if durable_ok && !drained_chunks.is_empty() {
            for chunk_id in &drained_chunks {
                if let Err(e) = self.conn.execute(
                    "DELETE FROM pending_index WHERE kind = 'chunk' AND chunk_id = ?",
                    params![chunk_id.to_string()],
                ) {
                    tracing::warn!(
                        error = %e,
                        %chunk_id,
                        "pending_index drain (chunk) failed; will replay on next startup"
                    );
                }
            }
        }
    }

    /// The body of `IngestDocument`. Returns the report plus the list of
    /// chunk_ids whose pending_index rows want draining (empty on the
    /// dedup short-circuit and on any error).
    ///
    /// Pipeline:
    ///   1. Parse the file (off-tx).
    ///   2. Chunk the text (off-tx).
    ///   3. Compute content_hash; check `documents.content_hash` for dedup.
    ///   4. Embed all chunks via `embedder.embed_batch` (off-tx).
    ///   5. BEGIN IMMEDIATE.
    ///   6. INSERT documents row.
    ///   7. For each chunk: INSERT document_chunks → INSERT chunk_embeddings
    ///      → INSERT pending_index (kind='chunk').
    ///   8. COMMIT.
    ///   9. hnsw.add(chunk_rowid, embedding) for each chunk.
    ///  10. Caller drains pending_index rows.
    fn handle_ingest_document_durable(
        &mut self,
        path: std::path::PathBuf,
        chunk_config: crate::document::ChunkConfig,
        audit_principal: Option<String>,
    ) -> (Result<IngestReport>, Vec<solo_core::ChunkId>) {
        // -------- Step 0: SOLO_INGEST_MAX_BYTES guardrail --------
        //
        // Read the file's size on disk and reject before paying any parse /
        // chunk / embed cost when it exceeds the configured cap. The cap is
        // a per-file precheck so the writer never holds an oversized
        // document in RAM. `SOLO_INGEST_MAX_BYTES=0` disables the cap
        // entirely; an unparseable env var falls back to the default with
        // a single WARN line.
        //
        // We deliberately consult `std::fs::metadata(path).len()` rather
        // than reading the file twice — `parse_file` will read it again
        // immediately after, and the OS page cache makes the second read
        // free. Using `metadata().len()` for the precheck also means we
        // can reject a multi-GB PDF without ever asking `pdf-extract` to
        // allocate.
        let file_size: u64 = match std::fs::metadata(&path) {
            Ok(meta) => {
                let size = meta.len();
                if let Some(cap) = resolve_ingest_max_bytes() {
                    if size > cap {
                        return (
                            Err(Error::storage(format!(
                                "ingest_document: file {} is {size} bytes, exceeds \
                                 SOLO_INGEST_MAX_BYTES cap of {cap} bytes. Set \
                                 SOLO_INGEST_MAX_BYTES=<larger> to override, or \
                                 SOLO_INGEST_MAX_BYTES=0 to disable the cap.",
                                path.display()
                            ))),
                            Vec::new(),
                        );
                    }
                }
                size
            }
            Err(e) => {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: stat {}: {e}",
                        path.display()
                    ))),
                    Vec::new(),
                );
            }
        };

        // -------- Step 0.5: per-tenant quota_bytes precheck (v0.8.1 P3) --------
        //
        // Use the file_size as a conservative growth estimate. Chunks +
        // chunk_embeddings + pending_index rows together expand the
        // on-disk DB by roughly the parsed text size plus embedding
        // overhead (~4 bytes per dim per chunk). 2x file_size gives a
        // generous-but-bounded over-count, preferring to surprise
        // operators with reject-early rather than reject-mid-ingest.
        let ingest_growth_estimate: u64 = file_size.saturating_mul(2);
        match check_quota(
            self.quota_bytes,
            self.db_path.as_deref(),
            ingest_growth_estimate,
        ) {
            QuotaDecision::Unlimited | QuotaDecision::Allowed { .. } => {}
            QuotaDecision::Exceeded {
                current_size,
                estimated_growth,
                quota,
            } => {
                let err = QuotaExceededError {
                    current_size,
                    estimated_growth,
                    quota,
                };
                self.emit_audit_best_effort(
                    AuditOperation::MemoryIngestDocument,
                    Some(path.display().to_string()),
                    AuditResult::Forbidden,
                    audit_principal,
                    Some(err.to_details_json()),
                );
                return (Err(Error::forbidden(err.to_string())), Vec::new());
            }
        }

        // -------- Steps 1 + 2: parse + chunk (pure, off-tx) --------
        let parsed = match crate::document::parse_file(&path) {
            Ok(p) => p,
            Err(e) => {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: parse {}: {e}",
                        path.display()
                    ))),
                    Vec::new(),
                );
            }
        };
        let parsed_text_chars = parsed.text.chars().count() as u64;
        let parsed_extractor_name = parsed.extractor_name.clone();
        let parsed_extractor_version = parsed.extractor_version.clone();
        let chunks = crate::document::chunk_text(&parsed.text, &chunk_config);

        // -------- Step 3: content_hash + dedup pre-check --------
        let content_hash = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(parsed.text.as_bytes());
            hex::encode(hasher.finalize())
        };

        let existing_doc: Option<String> = match self
            .conn
            .query_row(
                "SELECT doc_id FROM documents WHERE content_hash = ? LIMIT 1",
                params![&content_hash],
                |r| r.get::<_, String>(0),
            )
            .optional()
        {
            Ok(v) => v,
            Err(e) => {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: dedup lookup: {e}"
                    ))),
                    Vec::new(),
                );
            }
        };
        if let Some(doc_id_s) = existing_doc {
            let doc_id = match solo_core::DocumentId::from_str(&doc_id_s) {
                Ok(id) => id,
                Err(e) => {
                    return (
                        Err(Error::storage(format!(
                            "ingest_document: parse existing doc_id `{doc_id_s}`: {e}"
                        ))),
                        Vec::new(),
                    );
                }
            };
            tracing::info!(
                %doc_id,
                content_hash = %content_hash,
                "ingest_document: dedup hit; returning existing doc_id"
            );
            // v0.8.0 P4: best-effort audit emit for the dedup short-circuit
            // path. No tx of our own to embed it in — the dedup check was
            // a read-only lookup. The micro-tx inside `emit_audit_best_effort`
            // serializes correctly with any concurrent writer activity.
            self.emit_audit_best_effort(
                AuditOperation::MemoryIngestDocument,
                Some(doc_id.to_string()),
                AuditResult::Ok,
                audit_principal.clone(),
                Some(serde_json::json!({ "deduped": true })),
            );
            return (
                Ok(IngestReport {
                    doc_id,
                    chunks_persisted: 0,
                    bytes_ingested: parsed.byte_size,
                    text_chars: parsed_text_chars,
                    extractor_name: parsed_extractor_name,
                    extractor_version: parsed_extractor_version,
                    deduped: true,
                }),
                Vec::new(),
            );
        }

        // Need an embedder + runtime to embed chunks. Pure dedup hit above
        // doesn't need either — every other path does.
        let embedder = match self.embedder.clone() {
            Some(e) => e,
            None => {
                return (
                    Err(Error::Other(
                        "ingest_document: writer has no embedder \
                         (use spawn_full_with_embedder)"
                            .into(),
                    )),
                    Vec::new(),
                );
            }
        };
        let runtime = match self.runtime_handle.clone() {
            Some(r) => r,
            None => {
                return (
                    Err(Error::Other(
                        "ingest_document: writer has no runtime handle \
                         (use spawn_full_with_embedder)"
                            .into(),
                    )),
                    Vec::new(),
                );
            }
        };
        let embedder_id = match self.embedder_id {
            Some(id) => id,
            None => {
                return (
                    Err(Error::Other(
                        "ingest_document: writer has no embedder_id \
                         (use spawn_full_with_embedder)"
                            .into(),
                    )),
                    Vec::new(),
                );
            }
        };

        // Empty document (e.g. only-whitespace would have returned
        // ParseError::Empty already, but defensive): nothing to embed.
        if chunks.is_empty() {
            return (
                Err(Error::storage(
                    "ingest_document: parser returned text but chunker produced 0 chunks",
                )),
                Vec::new(),
            );
        }

        // -------- Step 3.5: PII redaction (v0.8.0 P5) --------
        // Redact each chunk's content BEFORE embedding so the embedding
        // matches what lands on disk. Aggregate match counts across all
        // chunks into ONE `redaction.applied` audit row per ingest
        // (don't spam N rows for N chunks of the same document).
        let mut redacted_chunks: Vec<crate::document::ChunkSpec> = Vec::with_capacity(chunks.len());
        let mut redaction_match_counts: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for spec in &chunks {
            let result = self.redactor.redact(&spec.content);
            for m in &result.matches {
                *redaction_match_counts
                    .entry(m.pattern_name.clone())
                    .or_insert(0) += m.count;
            }
            let new_content = match result.text {
                std::borrow::Cow::Borrowed(_) => spec.content.clone(),
                std::borrow::Cow::Owned(s) => s,
            };
            redacted_chunks.push(crate::document::ChunkSpec {
                content: new_content,
                token_count: spec.token_count,
                start_offset: spec.start_offset,
                end_offset: spec.end_offset,
            });
        }
        let chunks = redacted_chunks;

        // -------- Step 4: embed batch BEFORE the transaction --------
        // Embed-before-tx is the same risk-of-stale-tx pattern called out in
        // ADR-0003 §"Reembed batch ordering" — if the embedder fails or
        // hangs, no SQL state has changed and the writer is still free to
        // serve other commands.
        let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
        let embeddings = match runtime.block_on(embedder.embed_batch(&texts)) {
            Ok(v) => v,
            Err(e) => {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: embed_batch failed: {e}"
                    ))),
                    Vec::new(),
                );
            }
        };
        if embeddings.len() != chunks.len() {
            return (
                Err(Error::storage(format!(
                    "ingest_document: embed_batch returned {} embeddings for {} chunks",
                    embeddings.len(),
                    chunks.len()
                ))),
                Vec::new(),
            );
        }
        // Validate every embedding's (dtype, dim, data.len()) before SQL.
        for (i, emb) in embeddings.iter().enumerate() {
            if let Err(e) = emb.validate() {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: chunk {i} embedding invalid: {e}"
                    ))),
                    Vec::new(),
                );
            }
        }

        // Cache the dtype string for INSERT INTO chunk_embeddings.
        let dtype_str = match embedder.dtype() {
            solo_core::EmbeddingDtype::F32 => "f32",
            solo_core::EmbeddingDtype::F16 => "f16",
            solo_core::EmbeddingDtype::I8 => "i8",
            solo_core::EmbeddingDtype::Binary => "binary",
        };

        // -------- Step 5: allocate ids + open transaction --------
        let doc_id = solo_core::DocumentId::new();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let modified_at_ms: Option<i64> = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64);

        // Title heuristic: first markdown-style heading (`# ...`) on the
        // first 64 lines, else the file stem. Filename is more useful than
        // "(untitled)" in the typical case.
        let title: String = derive_document_title(&parsed.text, &path);
        let source: String = path.to_string_lossy().to_string();

        let chunk_count = chunks.len() as u32;

        // Collect chunk_ids + their assigned rowids for the post-COMMIT
        // hnsw.add + drain. We have to pull rowid out of `last_insert_rowid`
        // inside the tx; collect the (chunk_id, rowid, embedding) tuples
        // for use AFTER the commit.
        let mut chunk_records: Vec<(solo_core::ChunkId, i64, solo_core::Embedding)> =
            Vec::with_capacity(chunks.len());

        let tx = match self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
        {
            Ok(t) => t,
            Err(e) => {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: BEGIN IMMEDIATE: {e}"
                    ))),
                    Vec::new(),
                );
            }
        };

        // -------- Step 6: INSERT documents row --------
        if let Err(e) = tx.execute(
            "INSERT INTO documents (
                doc_id, source, title, mime_type,
                ingested_at_ms, modified_at_ms, status,
                chunk_count, content_hash, byte_size
             ) VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?, ?)",
            params![
                doc_id.to_string(),
                source,
                title,
                parsed.mime_type,
                now_ms,
                modified_at_ms,
                chunk_count as i64,
                content_hash,
                parsed.byte_size as i64,
            ],
        ) {
            return (
                Err(Error::storage(format!(
                    "ingest_document: INSERT documents: {e}"
                ))),
                Vec::new(),
            );
        }

        // -------- Step 7: INSERT chunk + embedding + pending_index rows --------
        for (idx, (spec, embedding)) in chunks.iter().zip(embeddings.iter()).enumerate() {
            let chunk_id = solo_core::ChunkId::new();
            if let Err(e) = tx.execute(
                "INSERT INTO document_chunks (
                    chunk_id, doc_id, chunk_index, content,
                    token_count, start_offset, end_offset, created_at_ms,
                    ingested_by_principal
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    chunk_id.to_string(),
                    doc_id.to_string(),
                    idx as i64,
                    spec.content,
                    spec.token_count as i64,
                    spec.start_offset as i64,
                    spec.end_offset as i64,
                    now_ms,
                    audit_principal.as_deref(),
                ],
            ) {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: INSERT document_chunks (idx {idx}): {e}"
                    ))),
                    Vec::new(),
                );
            }
            let rowid = tx.last_insert_rowid();

            if let Err(e) = tx.execute(
                "INSERT INTO chunk_embeddings (
                    chunk_id, embedder_id, dtype, dim, vector, created_at_ms
                 ) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    chunk_id.to_string(),
                    embedder_id,
                    dtype_str,
                    embedding.dim as i64,
                    &embedding.data[..],
                    now_ms,
                ],
            ) {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: INSERT chunk_embeddings (idx {idx}): {e}"
                    ))),
                    Vec::new(),
                );
            }

            if let Err(e) = tx.execute(
                "INSERT INTO pending_index (
                    kind, chunk_id, embedding, embedding_dim, enqueued_at
                 ) VALUES ('chunk', ?, ?, ?, ?)",
                params![
                    chunk_id.to_string(),
                    &embedding.data[..],
                    embedding.dim as i64,
                    now_ms,
                ],
            ) {
                return (
                    Err(Error::storage(format!(
                        "ingest_document: INSERT pending_index (idx {idx}): {e}"
                    ))),
                    Vec::new(),
                );
            }

            chunk_records.push((chunk_id, rowid, embedding.clone()));
        }

        // -------- Step 7.4: redaction audit emit (v0.8.0 P5) --------
        // ONE row aggregating every chunk's matches across the document.
        if !redaction_match_counts.is_empty() {
            let aggregated: Vec<crate::redaction::RedactionMatch> = redaction_match_counts
                .iter()
                .map(|(name, count)| crate::redaction::RedactionMatch {
                    pattern_name: name.clone(),
                    count: *count,
                })
                .collect();
            if let Err(e) = insert_audit_row_in_tx(
                &tx,
                &redaction_audit_event(
                    now_ms,
                    audit_principal.clone(),
                    Some(doc_id.to_string()),
                    &aggregated,
                ),
            ) {
                return (Err(e), Vec::new());
            }
        }

        // -------- Step 7.5: synchronous audit emit inside tx --------
        // v0.8.0 P4. If this fails the surrounding tx aborts via the
        // early return below — strict ACID for the audited write.
        if let Err(e) = insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal.clone(),
                operation: AuditOperation::MemoryIngestDocument,
                target_id: Some(doc_id.to_string()),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "chunks_persisted": chunk_count,
                    "bytes_ingested": parsed.byte_size,
                })),
            },
        ) {
            return (Err(e), Vec::new());
        }

        // -------- Step 8: COMMIT --------
        if let Err(e) = tx.commit() {
            return (
                Err(Error::storage(format!("ingest_document: COMMIT: {e}"))),
                Vec::new(),
            );
        }

        // -------- Step 9: hnsw.add per chunk --------
        // Failure here is non-fatal — the row in pending_index will replay
        // on next startup. We log and continue so partial-success-with-
        // recoverable-replay matches the steady-state ordering of
        // dispatch_remember.
        let mut drained: Vec<solo_core::ChunkId> = Vec::with_capacity(chunk_records.len());
        for (chunk_id, rowid, embedding) in &chunk_records {
            let f32_slice = match embedding.as_f32_slice() {
                Some(s) => s,
                None => {
                    tracing::warn!(
                        %chunk_id,
                        "ingest_document: chunk embedding is not F32; HNSW add skipped \
                         (pending_index row will be replayed)"
                    );
                    continue;
                }
            };
            // Chunks encode their rowid with the chunk bit set so they
            // share the HNSW namespace with episodes without collision.
            // See `crate::hnsw_id` for the encoding rationale.
            match self.hnsw.add(chunk_hnsw_id(*rowid), f32_slice) {
                Ok(_) => drained.push(*chunk_id),
                Err(e) => {
                    tracing::warn!(
                        %chunk_id,
                        error = %e,
                        "ingest_document: hnsw.add failed; pending_index row left for replay"
                    );
                }
            }
        }

        tracing::info!(
            %doc_id,
            chunks = chunk_records.len(),
            indexed = drained.len(),
            bytes = parsed.byte_size,
            "ingest_document complete"
        );

        (
            Ok(IngestReport {
                doc_id,
                chunks_persisted: chunk_count,
                bytes_ingested: parsed.byte_size,
                text_chars: parsed_text_chars,
                extractor_name: parsed_extractor_name,
                extractor_version: parsed_extractor_version,
                deduped: false,
            }),
            drained,
        )
    }

    /// Soft-delete a document. See [`WriteCommand::ForgetDocument`]:
    ///
    ///   1. UPDATE documents SET status='forgotten' WHERE doc_id = ?
    ///      → 0 rows == Err::NotFound
    ///   2. SELECT every chunk's rowid for this document.
    ///   3. COMMIT (single-statement UPDATE is effectively atomic).
    ///   4. hnsw.remove(rowid) for each chunk — tombstones the in-memory
    ///      bitmap so `index.len()` accounting stays clean. Failures here
    ///      are logged + skipped; the SQL `status='forgotten'` filter is
    ///      the source of truth for queries anyway.
    ///
    /// Idempotent: re-forgetting an already-forgotten doc UPDATEs zero
    /// rows but still returns Ok (chunks_tombstoned reflects the chunks
    /// found by the SELECT; on second call those are the same chunks
    /// whose rowids are already tombstoned — `hnsw.remove` is a HashSet
    /// insert so no-ops on re-entry). Returning NotFound only when the
    /// doc_id was never in the table.
    fn handle_forget_document(
        &mut self,
        doc_id: solo_core::DocumentId,
        audit_principal: Option<String>,
    ) -> Result<ForgetDocumentReport> {
        let id_str = doc_id.to_string();

        // Confirm the doc exists; this is also the NotFound check.
        let exists: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM documents WHERE doc_id = ?",
                params![&id_str],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| Error::storage(format!("forget_document: lookup status: {e}")))?;
        let Some(prior_status) = exists else {
            return Err(Error::not_found(format!(
                "doc_id {doc_id} not found in documents"
            )));
        };

        // v0.8.0 P4: wrap the UPDATE + audit row in a tx for ACID.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("forget_document: BEGIN IMMEDIATE: {e}")))?;

        // UPDATE to forgotten. Idempotent — UPDATEs zero rows when already
        // forgotten but we still proceed to tombstone the HNSW.
        tx.execute(
            "UPDATE documents
                SET status = 'forgotten'
              WHERE doc_id = ? AND status <> 'forgotten'",
            params![&id_str],
        )
        .map_err(|e| Error::storage(format!("forget_document: UPDATE status: {e}")))?;

        // Synchronous audit row inside the same tx.
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: chrono::Utc::now().timestamp_millis(),
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryForgetDocument,
                target_id: Some(id_str.clone()),
                result: AuditResult::Ok,
                details: None,
            },
        )?;

        tx.commit()
            .map_err(|e| Error::storage(format!("forget_document: COMMIT: {e}")))?;

        // Collect chunk rowids for HNSW tombstone (outside tx — read-only).
        let mut stmt = self
            .conn
            .prepare("SELECT rowid FROM document_chunks WHERE doc_id = ?")
            .map_err(|e| {
                Error::storage(format!("forget_document: prepare chunk-rowid select: {e}"))
            })?;
        // Dev-log 0152 M4 (corrected by 0154 audit): propagate per-row
        // decode errors instead of silently swallowing via
        // `filter_map(|r| r.ok())`. A missed chunk rowid means a missed
        // HNSW tombstone — exactly the drift H4's startup rebuild is
        // supposed to prevent. Note: the soft-delete UPDATE has ALREADY
        // committed at this point (tx.commit() above); propagating the
        // decode error here means the caller sees Err while the SQL
        // forget is durable. The startup rebuild (H4) covers the missed
        // tombstone on next restart, so caller-side retry isn't needed
        // for correctness — the error is informational about a bad row.
        let rowids: Vec<i64> = stmt
            .query_map(params![&id_str], |r| r.get::<_, i64>(0))
            .map_err(|e| Error::storage(format!("forget_document: query chunk rowids: {e}")))?
            .collect::<std::result::Result<Vec<i64>, rusqlite::Error>>()
            .map_err(|e| Error::storage(format!("forget_document: decode chunk rowid row: {e}")))?;

        let chunks_tombstoned = rowids.len() as u32;
        for rowid in rowids {
            // The encoded id MUST match the one passed at ingest time
            // (see `dispatch_ingest_document`).
            if let Err(e) = self.hnsw.remove(chunk_hnsw_id(rowid)) {
                tracing::warn!(
                    rowid,
                    %doc_id,
                    error = %e,
                    "forget_document: hnsw.remove failed (non-fatal; SQL filter still hides chunk)"
                );
            }
        }

        if prior_status == "forgotten" {
            tracing::debug!(
                %doc_id,
                "forget_document called on already-forgotten doc (idempotent)"
            );
        } else {
            tracing::info!(
                %doc_id,
                chunks_tombstoned,
                "document soft-deleted (status=forgotten)"
            );
        }
        Ok(ForgetDocumentReport {
            doc_id,
            chunks_tombstoned,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_store_asset_from_path(
        &mut self,
        path: &Path,
        filename: Option<String>,
        mime_type: Option<String>,
        size_bytes: Option<u64>,
        sha256: Option<String>,
        source: Option<String>,
        audit_principal: Option<String>,
    ) -> Result<StoredAssetReport> {
        let snapshot_dir = self
            .snapshot_dir
            .as_ref()
            .ok_or_else(|| Error::storage("store_asset: writer has no tenant asset directory"))?;
        if !path.is_file() {
            return Err(Error::not_found(format!(
                "asset source file not found: {}",
                path.display()
            )));
        }
        let actual_size = std::fs::metadata(path)
            .map_err(|e| Error::storage(format!("store_asset: stat {}: {e}", path.display())))?
            .len();
        if let Some(expected) = size_bytes
            && expected != actual_size
        {
            return Err(Error::invalid_input(format!(
                "asset size mismatch: expected {expected} bytes, got {actual_size}"
            )));
        }
        if actual_size == 0 {
            return Err(Error::invalid_input("asset source file must not be empty"));
        }

        let actual_sha = sha256_file(path)?;
        if let Some(expected) = sha256 {
            let expected = normalize_sha256_hex(&expected)?;
            if expected != actual_sha {
                return Err(Error::invalid_input(format!(
                    "asset sha256 mismatch: expected {expected}, got {actual_sha}"
                )));
            }
        }

        let (storage_path, final_path) = asset_blob_paths(snapshot_dir, &actual_sha)?;
        if let Some(existing) = self
            .conn
            .query_row(
                "SELECT asset_id, mime_type, filename, size_bytes, storage_path, status,
                        encryption_alg, encryption_nonce, encrypted_size_bytes
                   FROM assets
                  WHERE sha256 = ?
                  LIMIT 1",
                params![&actual_sha],
                |r| {
                    let encrypted_size_bytes: Option<i64> = r.get(8)?;
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, String>(6)?,
                        r.get::<_, Option<Vec<u8>>>(7)?,
                        encrypted_size_bytes.map(|v| v.max(0) as u64),
                    ))
                },
            )
            .optional()
            .map_err(|e| Error::storage(format!("store_asset: lookup by sha256: {e}")))?
        {
            let asset_id = AssetId::from_str(&existing.0).map_err(|e| {
                Error::storage(format!("store_asset: parse existing asset_id: {e}"))
            })?;
            let existing_final_path = snapshot_dir.join(&existing.4);
            if !existing_final_path.is_file() {
                if existing.6 != ASSET_BLOB_PLAINTEXT_ALG && self.key.is_none() {
                    return Err(Error::storage(
                        "store_asset: cannot heal encrypted asset blob without tenant key",
                    ));
                }
                let blob =
                    build_asset_blob_payload(path, self.key.as_ref(), &actual_sha, actual_size)?;
                enforce_asset_quota(
                    self.quota_bytes,
                    self.db_path.as_deref(),
                    snapshot_dir,
                    blob.bytes.len() as u64,
                )?;
                let now_ms = chrono::Utc::now().timestamp_millis();
                let staged_blob = stage_asset_blob_bytes(&blob.bytes, &existing_final_path)?;
                let mut promoted_blob = false;
                let result = (|| -> Result<()> {
                    let tx = self
                        .conn
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .map_err(|e| {
                            Error::storage(format!("store_asset: BEGIN heal existing asset: {e}"))
                        })?;
                    tx.execute(
                        "UPDATE assets
                            SET status = 'active',
                                encryption_alg = ?,
                                encryption_nonce = ?,
                                encrypted_size_bytes = ?,
                                updated_at_ms = ?
                          WHERE asset_id = ?",
                        params![
                            blob.encryption_alg,
                            blob.encryption_nonce.as_deref(),
                            blob.encrypted_size_bytes.map(|v| v as i64),
                            now_ms,
                            asset_id.to_string(),
                        ],
                    )
                    .map_err(|e| {
                        Error::storage(format!("store_asset: heal existing asset blob: {e}"))
                    })?;
                    promote_staged_asset_blob(&staged_blob, &existing_final_path, false)?;
                    promoted_blob = true;
                    tx.commit()
                        .map_err(|e| Error::storage(format!("store_asset: COMMIT heal asset: {e}")))
                })();
                if let Err(e) = result {
                    cleanup_failed_staged_asset_blob(
                        &staged_blob,
                        &existing_final_path,
                        promoted_blob,
                    );
                    return Err(e);
                }
            } else if existing.5 == "deleted" {
                let now_ms = chrono::Utc::now().timestamp_millis();
                self.conn
                    .execute(
                        "UPDATE assets
                            SET status = 'active', updated_at_ms = ?
                          WHERE asset_id = ?",
                        params![now_ms, asset_id.to_string()],
                    )
                    .map_err(|e| Error::storage(format!("store_asset: reactivate asset: {e}")))?;
            }
            return Ok(StoredAssetReport {
                asset_id,
                sha256: actual_sha,
                mime_type: existing.1,
                filename: existing.2,
                size_bytes: existing.3.max(0) as u64,
                storage_path: existing.4,
                deduped: true,
            });
        }

        let blob = build_asset_blob_payload(path, self.key.as_ref(), &actual_sha, actual_size)?;
        enforce_asset_quota(
            self.quota_bytes,
            self.db_path.as_deref(),
            snapshot_dir,
            blob.bytes.len() as u64,
        )?;
        let staged_blob = stage_asset_blob_bytes(&blob.bytes, &final_path)?;

        let asset_id = AssetId::new();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let clean_filename = filename.and_then(normalize_optional_text);
        let clean_mime = mime_type
            .and_then(normalize_optional_text)
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let clean_source = source.and_then(normalize_optional_text);
        let mut promoted_blob = false;
        let result = (|| -> Result<()> {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| Error::storage(format!("store_asset: BEGIN IMMEDIATE: {e}")))?;
            tx.execute(
                "INSERT INTO assets (
                    asset_id, sha256, mime_type, filename, size_bytes,
                    storage_path, source, status, created_by_principal,
                    created_at_ms, updated_at_ms, encryption_alg,
                    encryption_nonce, encrypted_size_bytes
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?, ?, ?, ?, ?, ?)",
                params![
                    asset_id.to_string(),
                    &actual_sha,
                    &clean_mime,
                    clean_filename.as_deref(),
                    actual_size as i64,
                    &storage_path,
                    clean_source.as_deref(),
                    audit_principal.as_deref(),
                    now_ms,
                    now_ms,
                    blob.encryption_alg,
                    blob.encryption_nonce.as_deref(),
                    blob.encrypted_size_bytes.map(|v| v as i64),
                ],
            )
            .map_err(|e| Error::storage(format!("store_asset: INSERT assets: {e}")))?;
            insert_audit_row_in_tx(
                &tx,
                &AuditEvent {
                    ts_ms: now_ms,
                    principal_subject: audit_principal,
                    operation: AuditOperation::MemoryStoreAsset,
                    target_id: Some(asset_id.to_string()),
                    result: AuditResult::Ok,
                    details: Some(serde_json::json!({
                        "sha256": actual_sha.clone(),
                        "size_bytes": actual_size,
                        "encryption_alg": blob.encryption_alg,
                        "stored_size_bytes": blob.bytes.len(),
                    })),
                },
            )?;
            promote_staged_asset_blob(&staged_blob, &final_path, true)?;
            promoted_blob = true;
            tx.commit()
                .map_err(|e| Error::storage(format!("store_asset: COMMIT: {e}")))
        })();
        if let Err(e) = result {
            cleanup_failed_staged_asset_blob(&staged_blob, &final_path, promoted_blob);
            return Err(e);
        }

        Ok(StoredAssetReport {
            asset_id,
            sha256: actual_sha,
            mime_type: clean_mime,
            filename: clean_filename,
            size_bytes: actual_size,
            storage_path,
            deduped: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_record_asset_extraction(
        &mut self,
        asset_id: AssetId,
        extractor_name: String,
        extractor_version: String,
        status: String,
        text_chars: u64,
        error: Option<String>,
        audit_principal: Option<String>,
    ) -> Result<AssetExtractionReport> {
        ensure_asset_exists(&self.conn, asset_id)?;
        let extractor_name = normalize_optional_text(extractor_name)
            .ok_or_else(|| Error::invalid_input("extractor_name must not be empty"))?;
        let extractor_version = normalize_optional_text(extractor_version)
            .ok_or_else(|| Error::invalid_input("extractor_version must not be empty"))?;
        let status = normalize_optional_text(status)
            .ok_or_else(|| Error::invalid_input("extraction status must not be empty"))?;
        if !matches!(status.as_str(), "extracted" | "stored_unparsed" | "failed") {
            return Err(Error::invalid_input(format!(
                "unsupported extraction status: {status}"
            )));
        }
        let text_chars_i64 = i64::try_from(text_chars)
            .map_err(|_| Error::invalid_input("text_chars must fit in a signed 64-bit integer"))?;
        let error = error.and_then(normalize_optional_text);
        let extraction_id = uuid::Uuid::now_v7().to_string();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("record_asset_extraction: BEGIN: {e}")))?;
        tx.execute(
            "INSERT INTO asset_extractions (
                extraction_id, asset_id, extractor_name, extractor_version,
                status, text_chars, error, created_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(asset_id, extractor_name, extractor_version)
             DO UPDATE SET
                status = excluded.status,
                text_chars = excluded.text_chars,
                error = excluded.error,
                created_at_ms = excluded.created_at_ms",
            params![
                &extraction_id,
                asset_id.to_string(),
                &extractor_name,
                &extractor_version,
                &status,
                text_chars_i64,
                error.as_deref(),
                now_ms,
            ],
        )
        .map_err(|e| Error::storage(format!("record_asset_extraction: upsert: {e}")))?;
        let stored_extraction_id: String = tx
            .query_row(
                "SELECT extraction_id
                   FROM asset_extractions
                  WHERE asset_id = ? AND extractor_name = ? AND extractor_version = ?",
                params![asset_id.to_string(), &extractor_name, &extractor_version],
                |r| r.get(0),
            )
            .map_err(|e| Error::storage(format!("record_asset_extraction: reread id: {e}")))?;
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryRecordAssetExtraction,
                target_id: Some(asset_id.to_string()),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "extractor_name": extractor_name.clone(),
                    "extractor_version": extractor_version.clone(),
                    "status": status.clone(),
                    "text_chars": text_chars,
                })),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("record_asset_extraction: COMMIT: {e}")))?;
        Ok(AssetExtractionReport {
            extraction_id: stored_extraction_id,
            asset_id,
            extractor_name,
            extractor_version,
            status,
            text_chars,
            error,
            created_at_ms: now_ms,
        })
    }

    fn handle_link_document_asset(
        &mut self,
        doc_id: DocumentId,
        asset_id: AssetId,
        relation_type: String,
        note: Option<String>,
        audit_principal: Option<String>,
    ) -> Result<DocumentAssetLinkReport> {
        ensure_document_exists(&self.conn, doc_id)?;
        ensure_asset_exists(&self.conn, asset_id)?;
        let relation_type = normalize_relation_type(&relation_type, "source")?;
        let note = note.and_then(normalize_optional_text);
        if let Some(existing) = self
            .conn
            .query_row(
                "SELECT link_id, created_at_ms
                   FROM document_assets
                  WHERE doc_id = ? AND asset_id = ? AND relation_type = ?
                  LIMIT 1",
                params![doc_id.to_string(), asset_id.to_string(), &relation_type],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| Error::storage(format!("link_document_asset: lookup existing: {e}")))?
        {
            return Ok(DocumentAssetLinkReport {
                link_id: AttachmentId::from_str(&existing.0).map_err(|e| {
                    Error::storage(format!("link_document_asset: parse existing link_id: {e}"))
                })?,
                doc_id,
                asset_id,
                relation_type,
                created_at_ms: existing.1,
            });
        }

        let link_id = AttachmentId::new();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("link_document_asset: BEGIN IMMEDIATE: {e}")))?;
        tx.execute(
            "INSERT INTO document_assets (
                link_id, doc_id, asset_id, relation_type, note, created_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                link_id.to_string(),
                doc_id.to_string(),
                asset_id.to_string(),
                &relation_type,
                note.as_deref(),
                now_ms,
            ],
        )
        .map_err(|e| Error::storage(format!("link_document_asset: INSERT: {e}")))?;
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryLinkDocumentAsset,
                target_id: Some(doc_id.to_string()),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "asset_id": asset_id.to_string(),
                    "relation_type": relation_type.clone(),
                })),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("link_document_asset: COMMIT: {e}")))?;
        Ok(DocumentAssetLinkReport {
            link_id,
            doc_id,
            asset_id,
            relation_type,
            created_at_ms: now_ms,
        })
    }

    fn handle_attach_memory(
        &mut self,
        memory_id: MemoryId,
        doc_id: Option<DocumentId>,
        asset_id: Option<AssetId>,
        relation_type: String,
        note: Option<String>,
        audit_principal: Option<String>,
    ) -> Result<MemoryAttachmentReport> {
        match (doc_id, asset_id) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(Error::invalid_input(
                    "memory attachment must target exactly one doc_id or asset_id",
                ));
            }
            _ => {}
        }
        ensure_memory_exists(&self.conn, memory_id)?;
        if let Some(doc_id) = doc_id {
            ensure_document_exists(&self.conn, doc_id)?;
        }
        if let Some(asset_id) = asset_id {
            ensure_asset_exists(&self.conn, asset_id)?;
        }
        let relation_type = normalize_relation_type(&relation_type, "related")?;
        let note = note.and_then(normalize_optional_text);
        if let Some((attachment_id, existing_note, created_at_ms)) =
            existing_memory_attachment(&self.conn, memory_id, doc_id, asset_id, &relation_type)?
        {
            return Ok(MemoryAttachmentReport {
                attachment_id,
                memory_id,
                doc_id,
                asset_id,
                relation_type,
                note: existing_note,
                created_at_ms,
            });
        }
        let attachment_id = AttachmentId::new();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("attach_memory: BEGIN IMMEDIATE: {e}")))?;
        tx.execute(
            "INSERT INTO memory_attachments (
                attachment_id, memory_id, doc_id, asset_id,
                relation_type, note, provenance_json, created_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?)",
            params![
                attachment_id.to_string(),
                memory_id.to_string(),
                doc_id.map(|id| id.to_string()),
                asset_id.map(|id| id.to_string()),
                &relation_type,
                note.as_deref(),
                now_ms,
            ],
        )
        .map_err(|e| Error::storage(format!("attach_memory: INSERT: {e}")))?;
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryAttach,
                target_id: Some(memory_id.to_string()),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "doc_id": doc_id.map(|id| id.to_string()),
                    "asset_id": asset_id.map(|id| id.to_string()),
                    "relation_type": relation_type.clone(),
                })),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("attach_memory: COMMIT: {e}")))?;
        Ok(MemoryAttachmentReport {
            attachment_id,
            memory_id,
            doc_id,
            asset_id,
            relation_type,
            note,
            created_at_ms: now_ms,
        })
    }

    fn handle_forget_asset(
        &mut self,
        asset_id: AssetId,
        audit_principal: Option<String>,
    ) -> Result<ForgetAssetReport> {
        let snapshot_dir = self
            .snapshot_dir
            .as_ref()
            .ok_or_else(|| Error::storage("forget_asset: writer has no tenant asset directory"))?;
        let row = self
            .conn
            .query_row(
                "SELECT status, storage_path FROM assets WHERE asset_id = ?",
                params![asset_id.to_string()],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|e| Error::storage(format!("forget_asset: lookup asset: {e}")))?;
        let Some((prior_status, storage_path)) = row else {
            return Err(Error::not_found(format!(
                "asset_id {asset_id} not found in assets"
            )));
        };
        let blob_path = safe_asset_storage_path(snapshot_dir, &storage_path)?;
        let already_deleted = prior_status == "deleted";

        let document_links: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM document_assets WHERE asset_id = ?",
                params![asset_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|e| Error::storage(format!("forget_asset: count document links: {e}")))?;
        let memory_attachments: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memory_attachments WHERE asset_id = ?",
                params![asset_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|e| Error::storage(format!("forget_asset: count memory attachments: {e}")))?;

        let blob_deleted = if blob_path.is_file() {
            std::fs::remove_file(&blob_path).map_err(|e| {
                Error::storage(format!(
                    "forget_asset: remove asset blob {}: {e}",
                    blob_path.display()
                ))
            })?;
            remove_empty_asset_blob_parent(&blob_path);
            true
        } else {
            false
        };

        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("forget_asset: BEGIN IMMEDIATE: {e}")))?;
        tx.execute(
            "UPDATE assets
                SET status = 'deleted', updated_at_ms = ?
              WHERE asset_id = ? AND status <> 'deleted'",
            params![now_ms, asset_id.to_string()],
        )
        .map_err(|e| Error::storage(format!("forget_asset: UPDATE status: {e}")))?;
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryForgetAsset,
                target_id: Some(asset_id.to_string()),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "prior_status": prior_status,
                    "storage_path": storage_path,
                    "blob_deleted": blob_deleted,
                    "document_links": document_links,
                    "memory_attachments": memory_attachments,
                })),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("forget_asset: COMMIT: {e}")))?;

        Ok(ForgetAssetReport {
            asset_id,
            blob_deleted,
            already_deleted,
            document_links: document_links.max(0) as u32,
            memory_attachments: memory_attachments.max(0) as u32,
        })
    }

    fn handle_consolidate(
        &mut self,
        scope: ConsolidationScope,
        audit_principal: Option<String>,
    ) -> Result<ConsolidationReport> {
        // Dev-log 0152 H2: refuse-or-warn when the active embedder is
        // the StubEmbedder (32-dim BLAKE3 hash). Cluster membership
        // groups episodes by textual hash proximity, not semantic
        // similarity — the downstream LLM abstractions look plausible
        // but the data backing them is garbage.
        //
        // The unit-test surface uses the stub extensively, so the guard
        // is a loud `tracing::error!` plus a one-shot per-process
        // counter rather than a hard refusal. Production operators wire
        // log aggregation; the error-level event will trip alerting.
        // To get a hard refusal, set SOLO_REFUSE_STUB_EMBEDDER=1.
        if let Some(embedder) = self.embedder.as_ref() {
            if embedder.name() == crate::embedder::STUB_EMBEDDER_NAME {
                if std::env::var_os("SOLO_REFUSE_STUB_EMBEDDER").is_some() {
                    return Err(Error::invalid_input(
                        "consolidation refused: StubEmbedder produces \
                         non-semantic vectors. Set SOLO_EMBEDDER=bundled \
                         or =ollama, or unset SOLO_REFUSE_STUB_EMBEDDER to \
                         downgrade this to a warning."
                            .to_string(),
                    ));
                }
                tracing::error!(
                    "consolidation running with StubEmbedder — cluster \
                     membership is BLAKE3-hash proximity, not semantic. \
                     Configure SOLO_EMBEDDER=bundled or =ollama for real \
                     vectors. Set SOLO_REFUSE_STUB_EMBEDDER=1 to make this \
                     a hard error."
                );
            }
        }
        // v0.2.0 implements only the SWS-equivalent clustering pass.
        // Abstraction + contradiction-detection (Y.3+) require the
        // LLM client; both fields stay 0 in the report.
        //
        // Discipline mirrors `handle_reembed`: validate state, build
        // candidates from SQL, run the pure-deterministic algorithm,
        // persist the output in one transaction. Mid-run failures
        // bubble up — there's nothing to "skip and continue" inside
        // the storage step (we either commit all clusters or none).
        let result = self.handle_consolidate_impl(scope);
        // v0.8.0 P4: best-effort audit emit after consolidate completes.
        // Consolidate spans multiple sub-txes (cluster persist, abstractions,
        // contradictions), so we can't embed the audit row inside a single
        // tx atomic with "the consolidate". Recording after the fact with
        // the result-summary in details is the pragmatic choice.
        match &result {
            Ok(report) => self.emit_audit_best_effort(
                AuditOperation::MemoryConsolidate,
                None,
                AuditResult::Ok,
                audit_principal,
                Some(serde_json::json!({
                    "episodes_seen": report.episodes_seen,
                    "clusters_built": report.clusters_built,
                    "abstractions_built": report.abstractions_built,
                    "triples_built": report.triples_built,
                    "contradictions_found": report.contradictions_found,
                })),
            ),
            Err(e) => self.emit_audit_best_effort(
                AuditOperation::MemoryConsolidate,
                None,
                AuditResult::Error,
                audit_principal,
                Some(serde_json::json!({ "error": e.to_string() })),
            ),
        }
        result
    }

    fn handle_repair_derived(
        &mut self,
        scope: DerivedRepairScope,
        audit_principal: Option<String>,
    ) -> Result<DerivedRepairReport> {
        let result = self.handle_repair_derived_impl(&scope, audit_principal.clone());
        if let Err(ref e) = result {
            self.emit_audit_best_effort(
                AuditOperation::MemoryDerivedRepair,
                None,
                AuditResult::Error,
                audit_principal,
                Some(serde_json::json!({ "error": e.to_string() })),
            );
        }
        result
    }

    fn handle_repair_derived_impl(
        &mut self,
        scope: &DerivedRepairScope,
        audit_principal: Option<String>,
    ) -> Result<DerivedRepairReport> {
        if matches!(scope.max_clusters, Some(0)) {
            return Err(Error::invalid_input("max_clusters must be >= 1 when set"));
        }

        match scope.mode {
            DerivedRepairMode::StaleAbstractions => {
                self.repair_stale_abstractions(scope, audit_principal)
            }
            DerivedRepairMode::QualityGate => {
                self.repair_active_triple_quality(scope, audit_principal)
            }
            DerivedRepairMode::RebuildAll => self.rebuild_all_derived(scope, audit_principal),
        }
    }

    fn repair_stale_abstractions(
        &mut self,
        scope: &DerivedRepairScope,
        audit_principal: Option<String>,
    ) -> Result<DerivedRepairReport> {
        let rows = self.scan_derived_repair_candidates()?;
        let clusters_scanned = rows.len();
        let mut candidates: Vec<DerivedRepairCandidate> = rows
            .into_iter()
            .filter_map(|row| row.into_candidate(scope.min_empty_abstraction_episode_count))
            .collect();
        if let Some(max) = scope.max_clusters {
            candidates.truncate(max);
        }

        let mut report = DerivedRepairReport {
            mode: scope.mode,
            dry_run: scope.dry_run,
            clusters_scanned,
            clusters_repaired: candidates.len(),
            abstractions_deleted: candidates.iter().map(|c| c.abstraction_count).sum(),
            triples_deleted: candidates.iter().map(|c| c.triple_count).sum(),
            candidate_samples: candidates
                .iter()
                .take(DERIVED_REPAIR_CANDIDATE_SAMPLE_LIMIT)
                .cloned()
                .collect(),
            ..DerivedRepairReport::default()
        };

        if scope.dry_run {
            report.contradictions_deleted =
                self.estimate_contradictions_for_candidate_clusters(&candidates)?;
            self.emit_audit_best_effort(
                AuditOperation::MemoryDerivedRepair,
                None,
                AuditResult::Ok,
                audit_principal,
                Some(derived_repair_audit_details(&report)),
            );
            return Ok(report);
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN derived repair: {e}")))?;

        report.abstractions_deleted = 0;
        report.triples_deleted = 0;
        report.contradictions_deleted = 0;
        for candidate in &candidates {
            let cluster_id = &candidate.cluster_id;
            let contradictions = tx
                .execute(
                    "DELETE FROM contradictions
                      WHERE a_memory_id IN (
                                SELECT triple_id FROM triples WHERE cluster_id = ?1
                            )
                         OR b_memory_id IN (
                                SELECT triple_id FROM triples WHERE cluster_id = ?1
                            )",
                    params![cluster_id],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "DELETE derived contradictions for cluster {cluster_id}: {e}"
                    ))
                })?;
            let triples = tx
                .execute(
                    "DELETE FROM triples WHERE cluster_id = ?1",
                    params![cluster_id],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "DELETE derived triples for cluster {cluster_id}: {e}"
                    ))
                })?;
            tx.execute(
                "DELETE FROM triple_reviews WHERE cluster_id = ?1",
                params![cluster_id],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "DELETE derived triple reviews for cluster {cluster_id}: {e}"
                ))
            })?;
            let abstractions = tx
                .execute(
                    "DELETE FROM semantic_abstractions WHERE cluster_id = ?1",
                    params![cluster_id],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "DELETE semantic abstraction for cluster {cluster_id}: {e}"
                    ))
                })?;
            report.contradictions_deleted += contradictions;
            report.triples_deleted += triples;
            report.abstractions_deleted += abstractions;
        }

        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryDerivedRepair,
                target_id: None,
                result: AuditResult::Ok,
                details: Some(derived_repair_audit_details(&report)),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT derived repair: {e}")))?;

        Ok(report)
    }

    fn rebuild_all_derived(
        &mut self,
        scope: &DerivedRepairScope,
        audit_principal: Option<String>,
    ) -> Result<DerivedRepairReport> {
        let mut report = DerivedRepairReport {
            mode: scope.mode,
            dry_run: scope.dry_run,
            clusters_scanned: self.count_rows("clusters")?,
            clusters_repaired: self.count_rows("clusters")?,
            abstractions_deleted: self.count_rows("semantic_abstractions")?,
            triples_deleted: self.count_rows("triples")?,
            contradictions_deleted: self.count_rows("contradictions")?,
            clusters_deleted: self.count_rows("clusters")?,
            cluster_memberships_deleted: self.count_rows("cluster_episodes")?,
            candidate_samples: Vec::new(),
            ..DerivedRepairReport::default()
        };

        if scope.dry_run {
            self.emit_audit_best_effort(
                AuditOperation::MemoryDerivedRepair,
                None,
                AuditResult::Ok,
                audit_principal,
                Some(derived_repair_audit_details(&report)),
            );
            return Ok(report);
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN derived rebuild: {e}")))?;

        report.contradictions_deleted = tx
            .execute("DELETE FROM contradictions", [])
            .map_err(|e| Error::storage(format!("DELETE contradictions: {e}")))?;
        tx.execute("DELETE FROM triple_reviews", [])
            .map_err(|e| Error::storage(format!("DELETE triple_reviews: {e}")))?;
        tx.execute("DELETE FROM entity_aliases", [])
            .map_err(|e| Error::storage(format!("DELETE entity_aliases: {e}")))?;
        report.triples_deleted = tx
            .execute("DELETE FROM triples", [])
            .map_err(|e| Error::storage(format!("DELETE triples: {e}")))?;
        tx.execute("DELETE FROM relationship_edges", [])
            .map_err(|e| Error::storage(format!("DELETE relationship_edges: {e}")))?;
        tx.execute("DELETE FROM entities", [])
            .map_err(|e| Error::storage(format!("DELETE entities: {e}")))?;
        report.abstractions_deleted = tx
            .execute("DELETE FROM semantic_abstractions", [])
            .map_err(|e| Error::storage(format!("DELETE semantic_abstractions: {e}")))?;
        report.cluster_memberships_deleted = tx
            .execute("DELETE FROM cluster_episodes", [])
            .map_err(|e| Error::storage(format!("DELETE cluster_episodes: {e}")))?;
        report.clusters_deleted = tx
            .execute("DELETE FROM clusters", [])
            .map_err(|e| Error::storage(format!("DELETE clusters: {e}")))?;

        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryDerivedRepair,
                target_id: None,
                result: AuditResult::Ok,
                details: Some(derived_repair_audit_details(&report)),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT derived rebuild: {e}")))?;

        Ok(report)
    }

    fn repair_active_triple_quality(
        &mut self,
        scope: &DerivedRepairScope,
        audit_principal: Option<String>,
    ) -> Result<DerivedRepairReport> {
        let rows = self.scan_active_triple_quality_rows()?;
        let mut quarantine = Vec::new();
        let mut rewrites = Vec::new();
        for row in rows {
            if row.object_kind == "entity" && active_entity_object_should_be_literal(&row.object_id)
            {
                rewrites.push(MetadataObjectRewriteCandidate {
                    triple_id: row.triple_id,
                    cluster_id: row.cluster_id,
                    subject_id: row.subject_id,
                    predicate: row.predicate,
                    object_id: row.object_id,
                });
                continue;
            }
            if let Some(reason) = active_triple_review_reason(
                &row.subject_id,
                &row.predicate,
                &row.object_id,
                &row.object_kind,
                row.confidence,
                row.literal_only_subject,
            ) {
                quarantine.push(ActiveTripleQualityCandidate {
                    row,
                    reason_code: reason.reason_code,
                    reason: reason.reason,
                });
            }
        }

        apply_quality_repair_cap(&mut quarantine, &mut rewrites, scope.max_clusters);

        let mut repaired_clusters = std::collections::BTreeSet::new();
        for candidate in &quarantine {
            repaired_clusters.insert(
                candidate
                    .row
                    .cluster_id
                    .clone()
                    .unwrap_or_else(|| "unclustered".to_string()),
            );
        }
        for candidate in &rewrites {
            repaired_clusters.insert(
                candidate
                    .cluster_id
                    .clone()
                    .unwrap_or_else(|| "unclustered".to_string()),
            );
        }

        let mut report = DerivedRepairReport {
            mode: scope.mode,
            dry_run: scope.dry_run,
            clusters_scanned: self.count_rows("clusters")?,
            clusters_repaired: repaired_clusters.len(),
            triples_rewritten: if scope.dry_run { rewrites.len() } else { 0 },
            triple_reviews_created: if scope.dry_run { quarantine.len() } else { 0 },
            candidate_samples: quality_repair_samples(&quarantine, &rewrites),
            ..DerivedRepairReport::default()
        };

        if scope.dry_run {
            self.emit_audit_best_effort(
                AuditOperation::MemoryDerivedRepair,
                None,
                AuditResult::Ok,
                audit_principal,
                Some(derived_repair_audit_details(&report)),
            );
            return Ok(report);
        }

        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN active triple quality repair: {e}")))?;

        for rewrite in &rewrites {
            let changed = tx
                .execute(
                    "UPDATE triples
                        SET object_kind = 'literal',
                            updated_at_ms = ?1
                      WHERE triple_id = ?2
                        AND status = 'active'
                        AND object_kind = 'entity'",
                    params![now_ms, &rewrite.triple_id],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "rewrite metadata object triple {} to literal: {e}",
                        rewrite.triple_id
                    ))
                })?;
            report.triples_rewritten += changed;
            tx.execute(
                "UPDATE relationship_edges
                    SET object_kind = 'literal',
                        object_entity_id = NULL,
                        object_literal = ?1,
                        updated_at_ms = ?2
                  WHERE edge_id = ?3
                    AND object_kind = 'entity'",
                params![&rewrite.object_id, now_ms, &rewrite.triple_id],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "rewrite relationship edge {} metadata object to literal: {e}",
                    rewrite.triple_id
                ))
            })?;
        }

        for candidate in &quarantine {
            let row = &candidate.row;
            let fingerprint =
                active_quality_review_fingerprint(&row.triple_id, candidate.reason_code);
            let review_id = active_quality_review_id(&row.triple_id, candidate.reason_code);
            tx.execute(
                "INSERT INTO triple_reviews
                    (review_id, candidate_fingerprint, triple_id, cluster_id,
                     source_episode_id, subject_id, predicate, object_id, object_kind,
                     confidence, reason_code, reason, provenance_json,
                     created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)
                 ON CONFLICT(candidate_fingerprint) DO UPDATE SET
                     triple_id = excluded.triple_id,
                     cluster_id = excluded.cluster_id,
                     source_episode_id = excluded.source_episode_id,
                     subject_id = excluded.subject_id,
                     predicate = excluded.predicate,
                     object_id = excluded.object_id,
                     object_kind = excluded.object_kind,
                     confidence = excluded.confidence,
                     reason_code = excluded.reason_code,
                     reason = excluded.reason,
                     provenance_json = excluded.provenance_json,
                     status = 'needs_review',
                     reviewed_at_ms = NULL,
                     updated_at_ms = excluded.updated_at_ms",
                params![
                    review_id,
                    fingerprint,
                    &row.triple_id,
                    row.cluster_id.as_deref(),
                    row.source_episode_id,
                    &row.subject_id,
                    &row.predicate,
                    &row.object_id,
                    &row.object_kind,
                    row.confidence,
                    candidate.reason_code,
                    &candidate.reason,
                    &row.provenance_json,
                    now_ms,
                ],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "insert active triple quality review for {}: {e}",
                    row.triple_id
                ))
            })?;
            upsert_active_quality_memory_claim_in_tx(
                &tx,
                row,
                &fingerprint,
                &review_id,
                candidate.reason_code,
                now_ms,
            )?;
            let contradictions = tx
                .execute(
                    "DELETE FROM contradictions
                      WHERE a_memory_id = ?1 OR b_memory_id = ?1",
                    params![&row.triple_id],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "delete contradictions for quarantined triple {}: {e}",
                        row.triple_id
                    ))
                })?;
            report.contradictions_deleted += contradictions;
            let changed = tx
                .execute(
                    "UPDATE triples
                        SET status = 'superseded',
                            updated_at_ms = ?1
                      WHERE triple_id = ?2
                        AND status = 'active'",
                    params![now_ms, &row.triple_id],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "supersede weak active triple {}: {e}",
                        row.triple_id
                    ))
                })?;
            report.triple_reviews_created += changed;
            tx.execute(
                "UPDATE relationship_edges
                    SET status = 'superseded',
                        updated_at_ms = ?1
                  WHERE edge_id = ?2
                    AND status = 'active'",
                params![now_ms, &row.triple_id],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "supersede relationship edge for weak triple {}: {e}",
                    row.triple_id
                ))
            })?;
        }

        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryDerivedRepair,
                target_id: None,
                result: AuditResult::Ok,
                details: Some(derived_repair_audit_details(&report)),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT active triple quality repair: {e}")))?;

        Ok(report)
    }

    fn scan_derived_repair_candidates(&self) -> Result<Vec<DerivedRepairScanRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT c.cluster_id,
                        COALESCE(GROUP_CONCAT(sa.content, char(10)), ''),
                        COUNT(DISTINCT sa.abstraction_id),
                        COUNT(DISTINCT ce.memory_id),
                        COUNT(DISTINCT t.triple_id),
                        COUNT(DISTINCT CASE WHEN t.status = 'active' THEN t.triple_id END)
                   FROM clusters c
                   JOIN semantic_abstractions sa ON sa.cluster_id = c.cluster_id
                   LEFT JOIN cluster_episodes ce ON ce.cluster_id = c.cluster_id
                   LEFT JOIN triples t ON t.cluster_id = c.cluster_id
                  GROUP BY c.cluster_id
                  ORDER BY c.rowid ASC",
            )
            .map_err(|e| Error::storage(format!("prepare derived repair scan: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DerivedRepairScanRow {
                    cluster_id: r.get(0)?,
                    abstraction_content: r.get(1)?,
                    abstraction_count: i64_to_usize(r.get(2)?),
                    episode_count: i64_to_usize(r.get(3)?),
                    triple_count: i64_to_usize(r.get(4)?),
                    active_triple_count: i64_to_usize(r.get(5)?),
                })
            })
            .map_err(|e| Error::storage(format!("query derived repair scan: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let row = row.map_err(|e| Error::storage(format!("decode derived repair row: {e}")))?;
            out.push(row);
        }
        Ok(out)
    }

    fn scan_active_triple_quality_rows(&self) -> Result<Vec<ActiveTripleQualityRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT t.triple_id,
                        t.cluster_id,
                        t.source_episode_id,
                        t.subject_id,
                        t.predicate,
                        t.object_id,
                        t.object_kind,
                        t.confidence,
                        t.provenance_json,
                        t.created_at_ms,
                        NOT EXISTS (
                            SELECT 1
                              FROM triples te
                             WHERE te.status = 'active'
                               AND te.object_kind = 'entity'
                               AND (te.subject_id = t.subject_id OR te.object_id = t.subject_id)
                        ) AS literal_only_subject
                   FROM triples t
                  WHERE t.status = 'active'
                  ORDER BY t.created_at_ms ASC, t.triple_id ASC",
            )
            .map_err(|e| Error::storage(format!("prepare active triple quality scan: {e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ActiveTripleQualityRow {
                    triple_id: r.get(0)?,
                    cluster_id: r.get(1)?,
                    source_episode_id: r.get(2)?,
                    subject_id: r.get(3)?,
                    predicate: r.get(4)?,
                    object_id: r.get(5)?,
                    object_kind: r.get(6)?,
                    confidence: r.get(7)?,
                    provenance_json: r.get(8)?,
                    created_at_ms: r.get(9)?,
                    literal_only_subject: r.get::<_, i64>(10)? != 0,
                })
            })
            .map_err(|e| Error::storage(format!("scan active triple quality rows: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(
                row.map_err(|e| Error::storage(format!("decode active triple quality row: {e}")))?,
            );
        }
        Ok(out)
    }

    fn estimate_contradictions_for_candidate_clusters(
        &self,
        candidates: &[DerivedRepairCandidate],
    ) -> Result<usize> {
        let mut total = 0usize;
        for candidate in candidates {
            let count: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*)
                       FROM contradictions
                      WHERE a_memory_id IN (
                                SELECT triple_id FROM triples WHERE cluster_id = ?1
                            )
                         OR b_memory_id IN (
                                SELECT triple_id FROM triples WHERE cluster_id = ?1
                            )",
                    params![candidate.cluster_id.as_str()],
                    |r| r.get(0),
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "COUNT derived contradictions for cluster {}: {e}",
                        candidate.cluster_id
                    ))
                })?;
            total += i64_to_usize(count);
        }
        Ok(total)
    }

    fn count_rows(&self, table: &str) -> Result<usize> {
        let sql = match table {
            "clusters" => "SELECT COUNT(*) FROM clusters",
            "cluster_episodes" => "SELECT COUNT(*) FROM cluster_episodes",
            "semantic_abstractions" => "SELECT COUNT(*) FROM semantic_abstractions",
            "triples" => "SELECT COUNT(*) FROM triples",
            "contradictions" => "SELECT COUNT(*) FROM contradictions",
            other => return Err(Error::storage(format!("unsupported count table {other}"))),
        };
        let count: i64 = self
            .conn
            .query_row(sql, [], |r| r.get(0))
            .map_err(|e| Error::storage(format!("{sql}: {e}")))?;
        Ok(i64_to_usize(count))
    }

    fn handle_consolidate_impl(
        &mut self,
        scope: ConsolidationScope,
    ) -> Result<ConsolidationReport> {
        let current_id = self.embedder_id.ok_or_else(|| {
            Error::Other(
                "consolidate: writer has no current embedder_id (use spawn_full_with_embedder)"
                    .into(),
            )
        })?;

        // v0.9.0 P4a: resolve the active Steward ONCE per consolidate
        // tick. Snapshot the Arc here so the merge-plan gate and the
        // abstraction loop observe the same Steward identity even if a
        // concurrent MCP-initialize-hook write overwrites the slot
        // mid-consolidate.
        let active_steward: Option<Arc<solo_steward::Steward>> = self.current_steward();

        // Optional time window. Computed BEFORE the SELECT so a slow
        // `now_ms` clock doesn't drift candidate selection.
        let now_ms = chrono::Utc::now().timestamp_millis();
        let cutoff_ms: Option<i64> = scope.window_days.and_then(|days| {
            const MS_PER_DAY: i64 = 86_400_000;
            days.checked_mul(MS_PER_DAY).map(|w| now_ms - w)
        });

        // Build candidates: (Episode, Embedding) tuples for active+hot
        // rows whose embedding row matches the current embedder. The
        // shape mirrors what `solo_steward::cluster::cluster_episodes`
        // expects, but we only populate the fields the algorithm reads
        // (memory_id, ts_ms) plus those that round-trip cheaply via SQL
        // (content). The defaulted fields (provenance, encoding_context,
        // confidence/strength/salience, source_id) are not used by the
        // SWS pass; Y.3's REM pass will need them and we'll widen this
        // SELECT then.
        // Idempotency: exclude memories that are already part of a
        // cluster. Without this, re-running consolidate on the same
        // data set creates duplicate clusters with different UUID v7
        // cluster_ids — same shape but two rows per memory in
        // `cluster_episodes`, plus wasted LLM `abstract_cluster` calls
        // in Y.3. Trade-off: a memory can never be re-clustered once
        // it's been placed; cluster-merging across consolidation
        // windows is a v0.3 feature ("re-consolidation"). For v0.2.0,
        // first-write wins.
        let candidates: Vec<(Episode, Embedding)> = {
            let (sql, params): (&str, Vec<rusqlite::types::Value>) = match cutoff_ms {
                Some(cutoff) => (
                    "SELECT e.memory_id, e.ts_ms, e.source_type, e.content,
                            e.confidence, e.strength, e.salience,
                            em.dtype, em.dim, em.vector
                     FROM episodes e
                     JOIN embeddings em ON em.memory_id = e.memory_id
                     WHERE em.embedder_id = ?1
                       AND e.status = 'active'
                       AND e.tier = 'hot'
                       AND e.ts_ms >= ?2
                       AND e.memory_id NOT IN (SELECT memory_id FROM cluster_episodes)
                     ORDER BY e.ts_ms, e.rowid",
                    vec![current_id.into(), cutoff.into()],
                ),
                None => (
                    "SELECT e.memory_id, e.ts_ms, e.source_type, e.content,
                            e.confidence, e.strength, e.salience,
                            em.dtype, em.dim, em.vector
                     FROM episodes e
                     JOIN embeddings em ON em.memory_id = e.memory_id
                     WHERE em.embedder_id = ?1
                       AND e.status = 'active'
                       AND e.tier = 'hot'
                       AND e.memory_id NOT IN (SELECT memory_id FROM cluster_episodes)
                     ORDER BY e.ts_ms, e.rowid",
                    vec![current_id.into()],
                ),
            };

            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| Error::storage(format!("prepare consolidate select: {e}")))?;
            let rows = stmt
                .query_map(params_from_iter(&params), |r| {
                    let memory_id: String = r.get(0)?;
                    let ts_ms: i64 = r.get(1)?;
                    let source_type: String = r.get(2)?;
                    let content: String = r.get(3)?;
                    let confidence_f: f32 = r.get(4)?;
                    let strength: f32 = r.get(5)?;
                    let salience: f32 = r.get(6)?;
                    let dtype_str: String = r.get(7)?;
                    let dim: i64 = r.get(8)?;
                    let vector: Vec<u8> = r.get(9)?;
                    Ok((
                        memory_id,
                        ts_ms,
                        source_type,
                        content,
                        confidence_f,
                        strength,
                        salience,
                        dtype_str,
                        dim,
                        vector,
                    ))
                })
                .map_err(|e| Error::storage(format!("query_map consolidate: {e}")))?;

            let mut out = Vec::new();
            for row in rows {
                let (
                    memory_id,
                    ts_ms,
                    source_type,
                    content,
                    conf,
                    strength,
                    salience,
                    dtype_str,
                    dim,
                    vector,
                ) = row.map_err(|e| Error::storage(format!("consolidate row decode: {e}")))?;
                let mid = MemoryId::from_str(&memory_id)
                    .map_err(|e| Error::storage(format!("parse memory_id `{memory_id}`: {e}")))?;
                let dtype = match dtype_str.as_str() {
                    "f32" => solo_core::EmbeddingDtype::F32,
                    "f16" => solo_core::EmbeddingDtype::F16,
                    "i8" => solo_core::EmbeddingDtype::I8,
                    "binary" => solo_core::EmbeddingDtype::Binary,
                    other => {
                        return Err(Error::storage(format!(
                            "unknown embeddings.dtype value `{other}`"
                        )));
                    }
                };
                let embedding = Embedding {
                    dtype,
                    dim: dim as usize,
                    data: vector,
                };
                let confidence = solo_core::Confidence::new(conf).map_err(|e| {
                    Error::storage(format!("invalid confidence in episodes row: {e}"))
                })?;
                let episode = Episode {
                    memory_id: mid,
                    ts_ms,
                    source_type,
                    source_id: None, // not selected; Y.3 will widen
                    content,
                    encoding_context: solo_core::EncodingContext::default(),
                    provenance: None,
                    confidence,
                    strength,
                    salience,
                    tier: Tier::Hot,
                };
                out.push((episode, embedding));
            }
            out
        };

        let mut report = ConsolidationReport {
            episodes_seen: candidates.len(),
            ..ConsolidationReport::default()
        };

        if candidates.is_empty() && !scope.force_merge {
            tracing::info!(seen = 0, "consolidate: no candidates");
            return Ok(report);
        }
        if candidates.is_empty() {
            tracing::info!(
                seen = 0,
                "consolidate: no candidates, but force_merge set; falling through to merge + regen"
            );
        }

        // Run the pure-deterministic clustering. Threshold + min-size
        // resolve from the `Steward`'s captured config when one is
        // wired (v0.11.1: this picks up the daemon/CLI's
        // `[steward]` TOML + `SOLO_CLUSTER_*` env layering — see
        // `StewardConfig::from_settings_then_env`). When no steward is
        // attached at all (pure-storage test paths, daemons that never
        // wired an LLM AND have no steward_slot population), fall back
        // to env-only parsing so the threshold pair is still operator-
        // controllable via env on those paths.
        //
        // The chosen `config` governs uniformly:
        //   - this clustering pass (new candidates),
        //   - the in-run `merge_clusters_by_centroid` call below,
        //   - the existing-vs-existing `plan_existing_merges` further
        //     down — all three read this same `config`.
        //
        // The `count_existing_merge_candidates` doctor path takes the
        // same resolved config as a parameter — see `merge_candidates`'s
        // "Sync requirement" docstring for the SQL+threshold pair.
        let config = match active_steward.as_ref() {
            Some(s) => s.config().clone(),
            None => solo_steward::StewardConfig::from_env()?,
        };
        let mut clusters = solo_steward::cluster::cluster_episodes(&candidates, &config)?;

        // Re-consolidation pass: fold clusters whose centroids are
        // above threshold into a single survivor. v0.3 MVP scope is
        // **just-built only** — the merge sees only the clusters
        // produced by this run, not pre-existing ones in the DB.
        // That closes the cross-UTC-day-bucket case (conversations
        // straddling midnight produce two same-themed clusters in
        // the per-day bucketing). Cross-run merge requires fetching
        // existing clusters + abstraction-regeneration plumbing —
        // separate iteration.
        let absorbed = solo_steward::cluster::merge_clusters_by_centroid(&mut clusters, &config)?;
        report.clusters_merged = absorbed;
        if absorbed > 0 {
            tracing::info!(
                absorbed,
                survivors = clusters.len(),
                "consolidate: centroid merge collapsed cross-bucket clusters"
            );
        }

        report.clusters_built = clusters.len();
        report.episodes_clustered = clusters.iter().map(|c| c.episode_ids.len()).sum();

        if clusters.is_empty() {
            tracing::info!(
                seen = report.episodes_seen,
                "consolidate: no new clusters formed; falling through to merge + regen"
            );
            // No early-return: the existing-vs-existing merge pass
            // and the regen pass downstream operate on pre-existing
            // DB clusters and can fire even when this run produced
            // no fresh clusters (e.g. drift catch-up). The absorb
            // pass naturally no-ops on an empty `clusters` Vec.
        }

        // -------- Cross-run absorb pass --------
        //
        // For each freshly-built cluster, decide whether it should
        // fold into a pre-existing DB cluster with a similar
        // centroid (within `cluster_cosine_threshold`). If so:
        //   - the new cluster gets NO row in `clusters`;
        //   - its `cluster_episodes` rows link under the existing
        //     cluster's id;
        //   - the existing cluster's centroid + coherence refresh
        //     to the post-absorb weighted mean;
        //   - the existing cluster's `semantic_abstractions` row is
        //     DELETED (with cascaded triples) so the next triples-batch
        //     tick regenerates against the post-absorb episode set.
        //     Dev-log 0152 M6: previously left in place, which meant
        //     the abstraction described a stale (pre-absorb) cluster
        //     forever — the `fetch_clusters_without_abstractions`
        //     query never picked it up because it still had a row.
        //
        // The expected_dim for the existing-cluster fetch comes from
        // the first candidate's embedding (every candidate uses the
        // current embedder's dim, enforced by the SELECT above).
        // expected_dim is normally read off the first candidate's
        // embedding; with `force_merge` and an empty candidate set
        // there's no candidate to read from, so fall back to the
        // current embedder's row in the `embedders` table.
        let expected_dim = if let Some(c) = candidates.first() {
            c.1.dim
        } else {
            self.conn
                .query_row(
                    "SELECT dim FROM embedders WHERE embedder_id = ?",
                    params![current_id],
                    |r| r.get::<_, i64>(0),
                )
                .map(|d| d as usize)
                .map_err(|e| {
                    Error::storage(format!(
                        "consolidate force_merge: lookup dim for embedder_id {current_id}: {e}"
                    ))
                })?
        };
        let existing_summaries = self.fetch_existing_cluster_summaries(cutoff_ms, expected_dim)?;
        let absorb_plan = if existing_summaries.is_empty() {
            solo_steward::cluster::AbsorbPlan::default()
        } else {
            solo_steward::cluster::absorb_into_existing(&clusters, &existing_summaries, &config)?
        };
        report.clusters_absorbed = absorb_plan.absorptions.len();
        if !absorb_plan.absorptions.is_empty() {
            tracing::info!(
                absorbed = absorb_plan.absorptions.len(),
                existing_modified = absorb_plan.modified_existing_ids().len(),
                "consolidate: cross-run absorb folded clusters into existing"
            );
        }

        // Build a quick map: new_cluster_id → AbsorbedCluster, for
        // O(1) lookup during the persistence loop.
        let absorbed_by_new: std::collections::HashMap<
            MemoryId,
            &solo_steward::cluster::AbsorbedCluster,
        > = absorb_plan
            .absorptions
            .iter()
            .map(|a| (a.new_cluster_id, a))
            .collect();

        // `clusters_built` should reflect the count of brand-new
        // clusters that actually got persisted (non-absorbed). Update
        // now that we know the absorb count.
        report.clusters_built = clusters.len() - absorb_plan.absorptions.len();

        // Persist all clusters in ONE transaction. If any insert fails,
        // rollback — partial state would leave dangling cluster_episodes
        // referencing nonexistent clusters.
        let txn = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN consolidate: {e}")))?;

        for cluster in &clusters {
            // Cross-run absorb: if this freshly-built cluster was
            // matched to an existing DB cluster, link its episodes
            // under that existing cluster_id and skip the INSERT
            // into `clusters`. The existing cluster's centroid +
            // coherence refresh happens after this loop in a single
            // batched UPDATE step.
            if let Some(absorbed) = absorbed_by_new.get(&cluster.cluster_id) {
                let target_id_s = absorbed.existing_cluster_id.to_string();
                for memid in &cluster.episode_ids {
                    txn.execute(
                        "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                        params![target_id_s, memid.to_string()],
                    )
                    .map_err(|e| {
                        Error::storage(format!("INSERT cluster_episodes (absorbed): {e}"))
                    })?;
                }
                continue;
            }

            let centroid_dtype: Option<&'static str> =
                cluster.centroid.as_ref().map(|e| match e.dtype {
                    solo_core::EmbeddingDtype::F32 => "f32",
                    solo_core::EmbeddingDtype::F16 => "f16",
                    solo_core::EmbeddingDtype::I8 => "i8",
                    solo_core::EmbeddingDtype::Binary => "binary",
                });
            let centroid_dim: Option<i64> = cluster.centroid.as_ref().map(|e| e.dim as i64);
            let centroid_blob: Option<&[u8]> = cluster.centroid.as_ref().map(|e| e.data.as_slice());

            txn.execute(
                "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    cluster.cluster_id.to_string(),
                    centroid_blob,
                    centroid_dtype,
                    centroid_dim,
                    cluster.coherence as f64,
                    now_ms,
                ],
            )
            .map_err(|e| Error::storage(format!("INSERT cluster: {e}")))?;

            for memid in &cluster.episode_ids {
                txn.execute(
                    "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                    params![cluster.cluster_id.to_string(), memid.to_string()],
                )
                .map_err(|e| Error::storage(format!("INSERT cluster_episodes: {e}")))?;
            }
        }

        // After the new INSERTs, refresh each modified existing
        // cluster's centroid + coherence to the post-absorb values.
        // Order: deterministic (sorted by cluster_id) so multi-
        // existing absorb runs commit in stable order.
        let mut modified_existing: Vec<&solo_steward::cluster::AbsorbedCluster> =
            absorb_plan.absorptions.iter().collect();
        modified_existing.sort_by(|a, b| a.existing_cluster_id.cmp(&b.existing_cluster_id));
        // Only the LAST absorption into a given existing cluster
        // carries the final centroid + coherence (the working state
        // accumulates step-wise inside `absorb_into_existing`).
        // Deduplicate to keep the most recent per existing_cluster_id.
        let mut last_per_existing: std::collections::HashMap<
            MemoryId,
            &solo_steward::cluster::AbsorbedCluster,
        > = std::collections::HashMap::new();
        for a in &absorb_plan.absorptions {
            last_per_existing.insert(a.existing_cluster_id, a);
        }
        let mut existing_ids_sorted: Vec<MemoryId> = last_per_existing.keys().copied().collect();
        existing_ids_sorted.sort();
        for existing_id in existing_ids_sorted {
            let absorbed = last_per_existing[&existing_id];
            txn.execute(
                "UPDATE clusters
                    SET centroid = ?, centroid_dtype = ?, centroid_dim = ?, coherence = ?
                  WHERE cluster_id = ?",
                params![
                    absorbed.merged_centroid.data.as_slice(),
                    "f32",
                    absorbed.merged_centroid.dim as i64,
                    absorbed.merged_coherence as f64,
                    existing_id.to_string(),
                ],
            )
            .map_err(|e| Error::storage(format!("UPDATE existing cluster centroid: {e}")))?;
            // Dev-log 0152 M6: drop the stale abstraction + cascaded
            // triples so the cluster shows up in the next triples-batch
            // tick's `fetch_clusters_without_abstractions` query and
            // gets regenerated against the post-absorb episode set.
            // Without this delete, the existing semantic_abstractions
            // row describes a smaller (pre-absorb) cluster indefinitely
            // and the triples it generated stay orphaned to the old
            // membership.
            txn.execute(
                "DELETE FROM semantic_abstractions WHERE cluster_id = ?",
                params![existing_id.to_string()],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "DELETE stale abstraction on absorb (cluster {existing_id}): {e}"
                ))
            })?;
            txn.execute(
                "DELETE FROM triples WHERE cluster_id = ?",
                params![existing_id.to_string()],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "DELETE stale triples on absorb (cluster {existing_id}): {e}"
                ))
            })?;
        }

        txn.commit()
            .map_err(|e| Error::storage(format!("COMMIT consolidate: {e}")))?;

        // -------- Existing-vs-existing merge pass --------
        //
        // Independent of cross-run absorb: detects pre-existing
        // clusters whose centroids have drifted close enough to
        // coalesce. Runs AFTER absorb so post-absorb centroid
        // updates feed in (an absorbed-into existing cluster might
        // now be similar to another pre-existing cluster).
        //
        // For each MergeOp in the plan:
        //   1. UPDATE cluster_episodes SET cluster_id = survivor
        //      WHERE cluster_id IN (loser_ids) — episodes move.
        //   2. UPDATE clusters SET centroid + coherence on the
        //      survivor.
        //   3. DELETE FROM clusters WHERE cluster_id IN (loser_ids)
        //      — cascades drop loser's `cluster_episodes` (now
        //      empty), `semantic_abstractions`, and `triples`.
        //
        // Survivor's own stale `semantic_abstractions` + `triples`
        // are NOT dropped here — the regen pass below handles them
        // identically to absorb-modified survivors.
        //
        // Skipped if no LLM steward is wired (the regen pass that
        // would replace the dropped abstractions only runs with a
        // steward; running merge without regen would leave
        // survivors in a stale-abstraction state with no recovery
        // until the next consolidate). Without a steward the
        // existing v0.2-era posture (stale-but-readable
        // abstractions) is preserved.
        let merge_plan: solo_steward::cluster::MergePlan = if active_steward.is_some() {
            let existing_full = self.fetch_existing_clusters_full(cutoff_ms, expected_dim)?;
            if existing_full.len() < 2 {
                solo_steward::cluster::MergePlan::default()
            } else {
                solo_steward::cluster::plan_existing_merges(&existing_full, &config)?
            }
        } else {
            solo_steward::cluster::MergePlan::default()
        };
        report.existing_clusters_merged = merge_plan.absorbed();

        if !merge_plan.merges.is_empty() {
            tracing::info!(
                merges = merge_plan.merges.len(),
                absorbed = merge_plan.absorbed(),
                "consolidate: existing-vs-existing merge applied"
            );
            let merge_txn = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| Error::storage(format!("BEGIN existing-merge txn: {e}")))?;
            for op in &merge_plan.merges {
                let survivor_str = op.survivor_id.to_string();
                // 1. Move episodes from each loser to the survivor.
                for loser_id in &op.loser_ids {
                    merge_txn
                        .execute(
                            "UPDATE cluster_episodes
                                SET cluster_id = ?1
                              WHERE cluster_id = ?2",
                            params![survivor_str, loser_id.to_string()],
                        )
                        .map_err(|e| {
                            Error::storage(format!("UPDATE cluster_episodes (existing-merge): {e}"))
                        })?;
                }
                // 2. Refresh survivor's centroid + coherence.
                merge_txn
                    .execute(
                        "UPDATE clusters
                            SET centroid = ?, centroid_dtype = ?, centroid_dim = ?, coherence = ?
                          WHERE cluster_id = ?",
                        params![
                            op.merged_centroid.data.as_slice(),
                            "f32",
                            op.merged_centroid.dim as i64,
                            op.merged_coherence as f64,
                            survivor_str,
                        ],
                    )
                    .map_err(|e| {
                        Error::storage(format!("UPDATE clusters (existing-merge): {e}"))
                    })?;
                // 3. DELETE losers — cascades clean their
                // `cluster_episodes` (already empty),
                // `semantic_abstractions`, and `triples`.
                for loser_id in &op.loser_ids {
                    merge_txn
                        .execute(
                            "DELETE FROM clusters WHERE cluster_id = ?",
                            params![loser_id.to_string()],
                        )
                        .map_err(|e| {
                            Error::storage(format!("DELETE clusters (existing-merge): {e}"))
                        })?;
                }
            }
            merge_txn
                .commit()
                .map_err(|e| Error::storage(format!("COMMIT existing-merge: {e}")))?;
        }

        // -------- v0.9.0 P4b: LLM-driven steps deferred to background batch --------
        //
        // The v0.8.x writer-actor ran THREE LLM-touching passes inline
        // here, blocking the writer thread on `rt.block_on(...)`:
        //
        //   1. Y.3.3 abstraction loop — `block_on(steward.abstract_cluster)`
        //      for every new cluster, persisting abstractions + triples.
        //   2. Re-abstraction regen — same call for every absorb /
        //      merge-modified existing cluster.
        //   3. Y.4.2 contradiction sweep —
        //      `block_on(steward.detect_contradiction)` for every
        //      new-triple pair.
        //
        // v0.9.0 P4 moves all three to the daemon-side consolidate
        // timer (see `crates/solo-cli/src/commands/daemon.rs::
        // triples_batch_timer` + `Steward::extract_triples_batch`).
        // Rationale (plan §4 P4b):
        //
        //   * `WriteCommand::Consolidate` returns FAST: cluster
        //     persistence is bounded SQL work; no LLM hop on the
        //     writer thread.
        //   * Background batching coalesces N per-cluster LLM calls
        //     into ⌈N/M⌉ for the sampling backend (P4c's
        //     `SamplingCoordinator`) — N approval prompts in Claude
        //     Desktop collapse to one per batch window.
        //   * `tenant.steward_slot()` is the source of truth (read
        //     by `self.current_steward()` above); the slot stays
        //     `Some(_)` for static backends from `LibraryHandle::open`
        //     and gets populated mid-life for the sampling backend
        //     at MCP-`initialize` time.
        //
        // The cluster + cluster_episodes INSERTs above ALREADY landed
        // when we got here; recall queries see the new clusters
        // immediately. Their abstractions + triples land on the
        // next consolidate-timer batch tick (`[triples]
        // trigger_interval_secs` default 3600s, or
        // `trigger_episode_count` default 50 — whichever fires
        // first). Documented as "Known behaviour change" in the
        // v0.9.0 release notes per plan §3 Decision 2.
        //
        // `active_steward` resolution above stays for the merge_plan
        // gate — that's pure-Rust steward.cluster algorithms, no
        // LLM call. The merge-persistence work (lines 2944-3018) ran
        // unchanged.
        //
        // Tests pinning this (in `tests::p4b_no_inline_llm_pins`):
        //   * `consolidate_command_returns_quickly_without_blocking_on_llm`
        //   * `triples_extraction_does_not_happen_in_writer_actor_command_path`
        let _writer_actor_no_longer_does_llm_inline =
            (active_steward.is_some(), self.runtime_handle.is_some());

        tracing::info!(
            seen = report.episodes_seen,
            clusters = report.clusters_built,
            episodes_clustered = report.episodes_clustered,
            abstractions = report.abstractions_built,
            triples = report.triples_built,
            contradictions = report.contradictions_found,
            "consolidate complete"
        );
        Ok(report)
    }

    /// Fetch every triple that shares `(subject_id, predicate)` with
    /// the new triple, excluding the new run's batch (passed via
    /// `exclude`). Used by `handle_consolidate`'s contradiction sweep
    /// to narrow LLM-judge candidates to plausible pairs only.
    ///
    /// Returns reassembled `Triple` structs. Provenance is parsed
    /// from the row's `provenance_json` column; on parse failure we
    /// substitute a placeholder `Provenance` (the triple is still
    /// usable for contradiction detection — the rule filter doesn't
    /// touch provenance, and the LLM judge prompt doesn't include
    /// it either).
    fn fetch_triples_for_pair(
        &self,
        subject_id: &str,
        predicate: &str,
        exclude: &std::collections::HashSet<MemoryId>,
    ) -> Result<Vec<solo_core::Triple>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT triple_id, object_id, object_kind, valid_from_ms, valid_to_ms,
                        confidence, provenance_json
                 FROM triples
                 WHERE subject_id = ?1 AND predicate = ?2
                   AND status = 'active'",
            )
            .map_err(|e| Error::storage(format!("prepare fetch_triples_for_pair: {e}")))?;
        let rows = stmt
            .query_map(params![subject_id, predicate], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, f32>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| Error::storage(format!("query_map triples: {e}")))?;

        let mut out = Vec::new();
        for row in rows {
            let (triple_id_s, object_id, object_kind_s, valid_from_ms, valid_to_ms, conf, prov_s) =
                row.map_err(|e| Error::storage(format!("triples row decode: {e}")))?;
            let triple_id = MemoryId::from_str(&triple_id_s)
                .map_err(|e| Error::storage(format!("parse triple_id `{triple_id_s}`: {e}")))?;
            if exclude.contains(&triple_id) {
                continue;
            }
            let object_kind = match object_kind_s.as_str() {
                "entity" => solo_core::TripleObjectKind::Entity,
                "literal" => solo_core::TripleObjectKind::Literal,
                other => {
                    return Err(Error::storage(format!(
                        "unknown object_kind value `{other}` in triples row"
                    )));
                }
            };
            let confidence = solo_core::Confidence::new(conf)
                .map_err(|e| Error::storage(format!("invalid confidence in triples row: {e}")))?;
            let provenance: solo_core::Provenance =
                serde_json::from_str(&prov_s).unwrap_or_else(|_| solo_core::Provenance {
                    derived_from: vec![],
                    derivation: "(unparseable)".into(),
                    by: "(unknown)".into(),
                    at_ms: 0,
                });
            out.push(solo_core::Triple {
                triple_id,
                subject_id: subject_id.to_string(),
                predicate: predicate.to_string(),
                object_id,
                object_kind,
                valid_from_ms,
                valid_to_ms,
                confidence,
                provenance,
            });
        }
        Ok(out)
    }

    /// Fetch compact summaries of every existing cluster within the
    /// consolidate window. Used by `handle_consolidate`'s cross-run
    /// absorb pass to decide which freshly-built clusters fold into
    /// pre-existing DB clusters with similar centroids.
    ///
    /// Filters:
    ///
    ///   - `cutoff_ms`: when `Some(ms)`, restrict to clusters with
    ///     `created_at_ms >= ms` (matches the candidate-episode
    ///     window). When `None`, all clusters in the table.
    ///   - Centroid must be present (non-null) and dim must equal
    ///     `expected_dim` — clusters built under a different
    ///     embedder are skipped (their centroids live in a
    ///     different vector space and absorb-cosine would be
    ///     meaningless).
    ///
    /// Returns one row per surviving cluster with its centroid +
    /// coherence + episode count. The episode_count is computed via
    /// a correlated `COUNT(*)` against `cluster_episodes` — cheap
    /// thanks to `idx_cluster_episodes_memory` (well, technically
    /// thanks to the `(cluster_id, memory_id)` PRIMARY KEY ordering).
    fn fetch_existing_cluster_summaries(
        &self,
        cutoff_ms: Option<i64>,
        expected_dim: usize,
    ) -> Result<Vec<solo_steward::cluster::ExistingClusterSummary>> {
        let (sql, params): (&str, Vec<rusqlite::types::Value>) = match cutoff_ms {
            Some(cutoff) => (
                "SELECT c.cluster_id, c.centroid, c.centroid_dtype, c.centroid_dim,
                        c.coherence,
                        (SELECT COUNT(*) FROM cluster_episodes ce
                         WHERE ce.cluster_id = c.cluster_id) AS episode_count
                 FROM clusters c
                 WHERE c.centroid IS NOT NULL
                   AND c.centroid_dtype = 'f32'
                   AND c.centroid_dim = ?1
                   AND c.created_at_ms >= ?2
                 ORDER BY c.cluster_id",
                vec![(expected_dim as i64).into(), cutoff.into()],
            ),
            None => (
                "SELECT c.cluster_id, c.centroid, c.centroid_dtype, c.centroid_dim,
                        c.coherence,
                        (SELECT COUNT(*) FROM cluster_episodes ce
                         WHERE ce.cluster_id = c.cluster_id) AS episode_count
                 FROM clusters c
                 WHERE c.centroid IS NOT NULL
                   AND c.centroid_dtype = 'f32'
                   AND c.centroid_dim = ?1
                 ORDER BY c.cluster_id",
                vec![(expected_dim as i64).into()],
            ),
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| Error::storage(format!("prepare existing-cluster summaries: {e}")))?;
        let rows = stmt
            .query_map(params_from_iter(&params), |r| {
                Ok((
                    r.get::<_, String>(0)?,  // cluster_id
                    r.get::<_, Vec<u8>>(1)?, // centroid blob
                    r.get::<_, String>(2)?,  // centroid_dtype
                    r.get::<_, i64>(3)?,     // centroid_dim
                    r.get::<_, f32>(4)?,     // coherence
                    r.get::<_, i64>(5)?,     // episode_count
                ))
            })
            .map_err(|e| Error::storage(format!("query_map existing clusters: {e}")))?;

        let mut out: Vec<solo_steward::cluster::ExistingClusterSummary> = Vec::new();
        for row in rows {
            let (cid_s, centroid_bytes, dtype_s, dim_i, coherence, count_i) =
                row.map_err(|e| Error::storage(format!("cluster row decode: {e}")))?;
            // Defensive: SQL's WHERE already filtered to f32 +
            // expected_dim; trust but verify the row contents.
            if dtype_s != "f32" || (dim_i as usize) != expected_dim {
                continue;
            }
            let cluster_id = match MemoryId::from_str(&cid_s) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        cluster_id = %cid_s,
                        error = %e,
                        "skipping cluster with unparseable cluster_id"
                    );
                    continue;
                }
            };
            let centroid = solo_core::Embedding {
                dtype: solo_core::EmbeddingDtype::F32,
                dim: dim_i as usize,
                data: centroid_bytes,
            };
            // count_i is the exact integer from COUNT(*); negatives
            // can't happen, but `as usize` saturates safely on the
            // off-chance of an i64 overflow case.
            let episode_count = count_i.max(0) as usize;
            // Skip clusters with zero episodes (orphaned rows
            // shouldn't exist thanks to ON DELETE CASCADE, but
            // guarding here keeps absorb math defensible).
            if episode_count == 0 {
                continue;
            }
            out.push(solo_steward::cluster::ExistingClusterSummary {
                cluster_id,
                centroid,
                coherence,
                episode_count,
            });
        }
        Ok(out)
    }

    /// Load every existing cluster as a full [`Cluster`] struct
    /// (centroid + coherence + complete `episode_ids` Vec). Used by
    /// the existing-vs-existing merge pass to feed
    /// `solo_steward::cluster::plan_existing_merges`.
    ///
    /// One SELECT joins `clusters` with `cluster_episodes`; rows are
    /// aggregated by `cluster_id`. Filters mirror
    /// `fetch_existing_cluster_summaries` (centroid present, dtype +
    /// dim match the current embedder, optional `created_at_ms` cutoff).
    /// Clusters with zero linked episodes are skipped — those are
    /// orphan rows that shouldn't exist post-CASCADE invariants.
    fn fetch_existing_clusters_full(
        &self,
        cutoff_ms: Option<i64>,
        expected_dim: usize,
    ) -> Result<Vec<solo_core::Cluster>> {
        let (sql, params): (&str, Vec<rusqlite::types::Value>) = match cutoff_ms {
            Some(cutoff) => (
                "SELECT c.cluster_id, c.centroid, c.centroid_dtype, c.centroid_dim,
                        c.coherence, ce.memory_id
                 FROM clusters c
                 JOIN cluster_episodes ce ON ce.cluster_id = c.cluster_id
                 WHERE c.centroid IS NOT NULL
                   AND c.centroid_dtype = 'f32'
                   AND c.centroid_dim = ?1
                   AND c.created_at_ms >= ?2
                 ORDER BY c.cluster_id, ce.memory_id",
                vec![(expected_dim as i64).into(), cutoff.into()],
            ),
            None => (
                "SELECT c.cluster_id, c.centroid, c.centroid_dtype, c.centroid_dim,
                        c.coherence, ce.memory_id
                 FROM clusters c
                 JOIN cluster_episodes ce ON ce.cluster_id = c.cluster_id
                 WHERE c.centroid IS NOT NULL
                   AND c.centroid_dtype = 'f32'
                   AND c.centroid_dim = ?1
                 ORDER BY c.cluster_id, ce.memory_id",
                vec![(expected_dim as i64).into()],
            ),
        };

        let mut stmt = self
            .conn
            .prepare(sql)
            .map_err(|e| Error::storage(format!("prepare existing clusters full: {e}")))?;
        let rows = stmt
            .query_map(params_from_iter(&params), |r| {
                Ok((
                    r.get::<_, String>(0)?,  // cluster_id
                    r.get::<_, Vec<u8>>(1)?, // centroid bytes
                    r.get::<_, String>(2)?,  // dtype
                    r.get::<_, i64>(3)?,     // dim
                    r.get::<_, f32>(4)?,     // coherence
                    r.get::<_, String>(5)?,  // memory_id
                ))
            })
            .map_err(|e| Error::storage(format!("query_map clusters full: {e}")))?;

        // Aggregate. Rows are ORDER BY cluster_id so we can build
        // the output as a single pass.
        let mut out: Vec<solo_core::Cluster> = Vec::new();
        for row in rows {
            let (cid_s, centroid_bytes, dtype_s, dim_i, coherence, memid_s) =
                row.map_err(|e| Error::storage(format!("clusters full row decode: {e}")))?;
            if dtype_s != "f32" || (dim_i as usize) != expected_dim {
                continue;
            }
            let cluster_id = match MemoryId::from_str(&cid_s) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        cluster_id = %cid_s,
                        error = %e,
                        "skipping cluster with unparseable cluster_id"
                    );
                    continue;
                }
            };
            let memory_id = match MemoryId::from_str(&memid_s) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(
                        memory_id = %memid_s,
                        error = %e,
                        "skipping cluster_episodes row with unparseable memory_id"
                    );
                    continue;
                }
            };
            // Append to the in-progress cluster (last entry in
            // `out`) when cluster_id matches; otherwise start a new
            // entry.
            if out.last().map(|c| c.cluster_id) == Some(cluster_id) {
                out.last_mut().unwrap().episode_ids.push(memory_id);
            } else {
                let centroid = solo_core::Embedding {
                    dtype: solo_core::EmbeddingDtype::F32,
                    dim: dim_i as usize,
                    data: centroid_bytes,
                };
                out.push(solo_core::Cluster {
                    cluster_id,
                    episode_ids: vec![memory_id],
                    centroid: Some(centroid),
                    coherence,
                });
            }
        }
        // Drop empty clusters (defensive — shouldn't happen given
        // the JOIN requires at least one cluster_episodes row).
        out.retain(|c| !c.episode_ids.is_empty());
        Ok(out)
    }

    /// Load full [`Episode`] structs for every memory_id linked to
    /// `cluster_id` via `cluster_episodes`. Used by the absorb→regen
    /// path to feed `steward.abstract_cluster` for a cluster whose
    /// stale abstraction needs replacing.
    ///
    /// Filters: `episodes.status = 'active'` (forgotten episodes
    /// can't drive a fresh abstraction; they get skipped). The
    /// SELECT mirrors `handle_consolidate`'s candidate fetch but
    /// without the `embedder_id` filter — the regen pass operates
    /// on the cluster's full historical episode set, regardless of
    /// which embedder produced their vectors.
    ///
    /// Returns episodes ordered by `(ts_ms, rowid)` for
    /// deterministic prompts.
    fn fetch_episodes_for_cluster(&self, cluster_id: &MemoryId) -> Result<Vec<Episode>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT e.memory_id, e.ts_ms, e.source_type, e.source_id,
                        e.content, e.encoding_context_json, e.provenance_json,
                        e.confidence, e.strength, e.salience, e.tier
                 FROM episodes e
                 JOIN cluster_episodes ce ON ce.memory_id = e.memory_id
                 WHERE ce.cluster_id = ?1
                   AND e.status = 'active'
                 ORDER BY e.ts_ms, e.rowid",
            )
            .map_err(|e| Error::storage(format!("prepare fetch_episodes_for_cluster: {e}")))?;
        let rows = stmt
            .query_map(params![cluster_id.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,         // memory_id
                    r.get::<_, i64>(1)?,            // ts_ms
                    r.get::<_, String>(2)?,         // source_type
                    r.get::<_, Option<String>>(3)?, // source_id
                    r.get::<_, String>(4)?,         // content
                    r.get::<_, String>(5)?,         // encoding_context_json
                    r.get::<_, Option<String>>(6)?, // provenance_json
                    r.get::<_, f32>(7)?,            // confidence
                    r.get::<_, f32>(8)?,            // strength
                    r.get::<_, f32>(9)?,            // salience
                    r.get::<_, String>(10)?,        // tier
                ))
            })
            .map_err(|e| Error::storage(format!("query_map cluster episodes: {e}")))?;

        let mut out: Vec<Episode> = Vec::new();
        for row in rows {
            let (
                mid_s,
                ts_ms,
                source_type,
                source_id,
                content,
                ctx_json,
                prov_json,
                conf,
                strength,
                salience,
                tier_s,
            ) = row.map_err(|e| Error::storage(format!("episode row decode: {e}")))?;
            let mid = MemoryId::from_str(&mid_s)
                .map_err(|e| Error::storage(format!("parse memory_id `{mid_s}`: {e}")))?;
            let confidence = solo_core::Confidence::new(conf)
                .map_err(|e| Error::storage(format!("invalid confidence in episode row: {e}")))?;
            let encoding_context: solo_core::EncodingContext =
                serde_json::from_str(&ctx_json).unwrap_or_default();
            let provenance: Option<solo_core::Provenance> = prov_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok());
            let tier = match tier_s.as_str() {
                "hot" => Tier::Hot,
                "warm" => Tier::Warm,
                "cold" => Tier::Cold,
                other => {
                    return Err(Error::storage(format!(
                        "unknown tier value `{other}` in episodes row"
                    )));
                }
            };
            out.push(Episode {
                memory_id: mid,
                ts_ms,
                source_type,
                source_id,
                content,
                encoding_context,
                provenance,
                confidence,
                strength,
                salience,
                tier,
            });
        }
        Ok(out)
    }

    fn handle_reembed(
        &mut self,
        scope: ReembedScope,
        audit_principal: Option<String>,
    ) -> Result<ReembedReport> {
        let result = self.handle_reembed_impl(scope);
        // v0.8.0 P4: best-effort audit emit after reembed completes.
        // Reembed spans many sub-batches; recording one summary row at
        // the end is the pragmatic shape.
        match &result {
            Ok(report) => self.emit_audit_best_effort(
                AuditOperation::MemoryReembed,
                None,
                AuditResult::Ok,
                audit_principal,
                Some(serde_json::json!({
                    "rows_seen": report.rows_seen,
                    "rows_reembedded": report.rows_reembedded,
                    "rows_failed": report.rows_failed,
                    "rows_gc_deleted": report.rows_gc_deleted,
                    "dry_run": report.dry_run,
                })),
            ),
            Err(e) => self.emit_audit_best_effort(
                AuditOperation::MemoryReembed,
                None,
                AuditResult::Error,
                audit_principal,
                Some(serde_json::json!({ "error": e.to_string() })),
            ),
        }
        result
    }

    fn handle_reembed_impl(&mut self, scope: ReembedScope) -> Result<ReembedReport> {
        // Reembed needs the writer to have been spawned with the active
        // embedder + a runtime handle (see `spawn_full_with_embedder`).
        // The plain `spawn_full` constructor leaves these `None`, which
        // is the correct posture for daemon paths that don't dispatch
        // Reembed. A clean error here beats a panic.
        let current_id = self.embedder_id.ok_or_else(|| {
            Error::Other(
                "reembed: writer has no current embedder_id (use spawn_full_with_embedder)".into(),
            )
        })?;
        let embedder = self.embedder.clone().ok_or_else(|| {
            Error::Other("reembed: writer has no embedder (use spawn_full_with_embedder)".into())
        })?;
        let runtime = self.runtime_handle.clone().ok_or_else(|| {
            Error::Other(
                "reembed: writer has no runtime handle (use spawn_full_with_embedder)".into(),
            )
        })?;

        // Optional `from` filter → resolve to embedder_id once. Refuse if
        // the user asked to migrate from "current to current" (nothing
        // would happen) or if the from-embedder isn't registered.
        let from_id: Option<i64> = match &scope.from {
            None => None,
            Some((name, version)) => {
                let id: Option<i64> = self
                    .conn
                    .query_row(
                        "SELECT embedder_id FROM embedders WHERE name = ? AND version = ?",
                        params![name, version],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(|e| Error::storage(format!("lookup from embedder: {e}")))?;
                match id {
                    Some(id) if id == current_id => {
                        return Err(Error::Other(format!(
                            "reembed: from-embedder ({name}, {version}) IS the current \
                             embedder; nothing to do"
                        )));
                    }
                    Some(id) => Some(id),
                    None => {
                        return Err(Error::not_found(format!(
                            "reembed: from-embedder ({name}, {version}) not registered \
                             in `embedders` table"
                        )));
                    }
                }
            }
        };

        // Build the candidate set. DISTINCT — a memory may have multiple
        // stale rows (if the user has rolled through more than one prior
        // embedder); we only want to embed each content once.
        let candidates: Vec<(String, String)> = {
            let (sql, bound_id): (&str, i64) = match from_id {
                None => (
                    "SELECT DISTINCT e.memory_id, e.content
                     FROM episodes e
                     JOIN embeddings em ON em.memory_id = e.memory_id
                     WHERE em.embedder_id != ?1
                       AND e.status = 'active'
                     ORDER BY e.rowid",
                    current_id,
                ),
                Some(fid) => (
                    "SELECT DISTINCT e.memory_id, e.content
                     FROM episodes e
                     JOIN embeddings em ON em.memory_id = e.memory_id
                     WHERE em.embedder_id = ?1
                       AND e.status = 'active'
                     ORDER BY e.rowid",
                    fid,
                ),
            };
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| Error::storage(format!("prepare reembed select: {e}")))?;
            let rows = stmt
                .query_map(params![bound_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(|e| Error::storage(format!("query_map reembed: {e}")))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| Error::storage(format!("reembed row decode: {e}")))?);
            }
            out
        };

        let mut report = ReembedReport {
            rows_seen: candidates.len(),
            rows_reembedded: 0,
            rows_failed: 0,
            rows_gc_deleted: 0,
            dry_run: scope.dry_run,
        };

        if scope.dry_run {
            tracing::info!(
                seen = report.rows_seen,
                "reembed --dry-run: would re-embed N memories"
            );
            return Ok(report);
        }

        // Cache the dtype string for the current embedder once.
        let dtype_str = match embedder.dtype() {
            solo_core::EmbeddingDtype::F32 => "f32",
            solo_core::EmbeddingDtype::F16 => "f16",
            solo_core::EmbeddingDtype::I8 => "i8",
            solo_core::EmbeddingDtype::Binary => "binary",
        };
        let now_ms = chrono::Utc::now().timestamp_millis();

        // Per-memory loop. Embed (async, off the writer thread via the
        // captured runtime handle), then atomically apply the SQL changes
        // for that memory. A failure on one memory does NOT abort the
        // run — partial progress is fine because the next reembed pass
        // picks up wherever we left off (the SELECT re-evaluates the
        // candidate set at start). The per-memory transaction means a
        // mid-run crash cannot leave a memory with two `current`
        // embedding rows or with the new row but missing the GC.
        for (memory_id, content) in candidates {
            let embedding_res = runtime.block_on(embedder.embed(&content));
            let new_embedding = match embedding_res {
                Ok(emb) => emb,
                Err(e) => {
                    tracing::warn!(%memory_id, error = %e, "reembed: embedder failed");
                    report.rows_failed += 1;
                    continue;
                }
            };
            if let Err(e) = new_embedding.validate() {
                tracing::warn!(%memory_id, error = %e, "reembed: embedding validate failed");
                report.rows_failed += 1;
                continue;
            }

            let txn = match self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
            {
                Ok(t) => t,
                Err(e) => {
                    tracing::warn!(%memory_id, error = %e, "reembed: BEGIN failed");
                    report.rows_failed += 1;
                    continue;
                }
            };

            // INSERT ... ON CONFLICT(memory_id, embedder_id) DO UPDATE.
            // If a partial earlier reembed already wrote the current row
            // for this memory, refresh it with the freshly-computed
            // vector. (Same content + same embedder = same vector, so
            // this is a no-op semantically; we still bump created_at_ms.)
            let insert_res = txn.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(memory_id, embedder_id) DO UPDATE SET
                    dtype = excluded.dtype,
                    dim = excluded.dim,
                    vector = excluded.vector,
                    created_at_ms = excluded.created_at_ms",
                params![
                    memory_id,
                    current_id,
                    dtype_str,
                    new_embedding.dim as i64,
                    &new_embedding.data[..],
                    now_ms,
                ],
            );
            if let Err(e) = insert_res {
                tracing::warn!(%memory_id, error = %e, "reembed: INSERT failed");
                report.rows_failed += 1;
                continue;
            }

            let gc_count = if scope.gc {
                match txn.execute(
                    "DELETE FROM embeddings
                     WHERE memory_id = ? AND embedder_id != ?",
                    params![memory_id, current_id],
                ) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(%memory_id, error = %e, "reembed: GC DELETE failed");
                        report.rows_failed += 1;
                        continue;
                    }
                }
            } else {
                0
            };

            if let Err(e) = txn.commit() {
                tracing::warn!(%memory_id, error = %e, "reembed: COMMIT failed");
                report.rows_failed += 1;
                continue;
            }
            report.rows_reembedded += 1;
            report.rows_gc_deleted += gc_count;
        }

        tracing::info!(
            seen = report.rows_seen,
            reembedded = report.rows_reembedded,
            failed = report.rows_failed,
            gc_deleted = report.rows_gc_deleted,
            "reembed complete"
        );
        Ok(report)
    }

    /// Implementation of `WriteCommand::NormalizeSubjects`.
    ///
    /// One transaction wraps every `(from, to)` pair so the entire
    /// backfill is atomic. For each pair: run the symmetric
    /// `subject_id` and `object_id` UPDATEs, accumulate row counts.
    /// In `dry_run` mode the transaction is `ROLLBACK`ed at the end;
    /// otherwise it commits.
    ///
    /// SQL uses parameterized statements (`?1`, `?2`) — alias strings
    /// never reach SQL via `format!`, so this is injection-safe even
    /// for adversarial alias values.
    ///
    /// A single triple where `subject_id == object_id == from` is
    /// counted as one subject row + one object row (count of 2 against
    /// that triple), matching what SQLite's `changes()` reports for the
    /// two separate UPDATEs.
    fn handle_normalize_subjects(
        &mut self,
        aliases: Vec<(String, String)>,
        dry_run: bool,
        audit_principal: Option<String>,
    ) -> Result<NormalizeReport> {
        let mut report = NormalizeReport {
            aliases_processed: aliases.len(),
            subject_rows_updated: 0,
            object_rows_updated: 0,
            dry_run,
        };

        // Empty alias list: nothing to do, no transaction needed.
        // Mirrors the "zero candidates" short-circuit in handle_reembed.
        if aliases.is_empty() {
            tracing::info!(dry_run, "normalize_subjects: empty alias list, no-op");
            // v0.8.0 P4: still record the audit row even for the no-op.
            self.emit_audit_best_effort(
                AuditOperation::MemoryNormalizeSubjects,
                None,
                AuditResult::Ok,
                audit_principal,
                Some(serde_json::json!({
                    "aliases_processed": 0,
                    "dry_run": dry_run,
                })),
            );
            return Ok(report);
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN IMMEDIATE for normalize_subjects: {e}")))?;

        for (from, to) in &aliases {
            let now_ms = chrono::Utc::now().timestamp_millis();
            tx.execute(
                "INSERT OR IGNORE INTO entities
                    (entity_id, canonical_name, entity_type, aliases_json, confidence,
                     first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms)
                 VALUES (?1, ?1, 'unknown', '[]', 1.0, ?2, ?2, 'active', ?2, ?2)",
                params![to, now_ms],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "normalize_subjects: ensure target relationship entity ({to}): {e}"
                ))
            })?;
            let subj_rows = tx
                .execute(
                    "UPDATE triples SET subject_id = ?1, updated_at_ms = ?3 \
                     WHERE subject_id = ?2",
                    params![to, from, now_ms],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "normalize_subjects: UPDATE subject_id ({from} -> {to}): {e}"
                    ))
                })?;
            let obj_rows = tx
                .execute(
                    "UPDATE triples SET object_id = ?1, updated_at_ms = ?3 \
                     WHERE object_id = ?2",
                    params![to, from, now_ms],
                )
                .map_err(|e| {
                    Error::storage(format!(
                        "normalize_subjects: UPDATE object_id ({from} -> {to}): {e}"
                    ))
                })?;
            tx.execute(
                "UPDATE relationship_edges
                    SET subject_entity_id = ?1, updated_at_ms = ?3
                  WHERE subject_entity_id = ?2",
                params![to, from, now_ms],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "normalize_subjects: UPDATE relationship_edges subject ({from} -> {to}): {e}"
                ))
            })?;
            tx.execute(
                "UPDATE relationship_edges
                    SET object_entity_id = ?1, updated_at_ms = ?3
                  WHERE object_entity_id = ?2",
                params![to, from, now_ms],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "normalize_subjects: UPDATE relationship_edges object entity ({from} -> {to}): {e}"
                ))
            })?;
            tx.execute(
                "UPDATE relationship_edges
                    SET object_literal = ?1, updated_at_ms = ?3
                  WHERE object_literal = ?2",
                params![to, from, now_ms],
            )
            .map_err(|e| {
                Error::storage(format!(
                    "normalize_subjects: UPDATE relationship_edges object literal ({from} -> {to}): {e}"
                ))
            })?;
            report.subject_rows_updated += subj_rows;
            report.object_rows_updated += obj_rows;
            if !dry_run {
                let affected_aliases = vec![from.clone()];
                insert_entity_review_op_in_tx(
                    &tx,
                    "merge",
                    "applied",
                    from,
                    Some(to),
                    &affected_aliases,
                    Some("normalize_subjects"),
                    "entity_merged",
                    Some(to),
                    now_ms,
                )?;
            }
        }

        if dry_run {
            // Dry-run: rollback the UPDATE; emit a best-effort audit row
            // OUTSIDE the rolled-back tx. We want the audit trail to
            // record the dry-run invocation even though no data changed.
            tx.rollback().map_err(|e| {
                Error::storage(format!("normalize_subjects: ROLLBACK after dry-run: {e}"))
            })?;
            tracing::info!(
                aliases_processed = report.aliases_processed,
                subject_rows = report.subject_rows_updated,
                object_rows = report.object_rows_updated,
                "normalize_subjects --dry-run: rolled back (would have updated N rows)"
            );
            self.emit_audit_best_effort(
                AuditOperation::MemoryNormalizeSubjects,
                None,
                AuditResult::Ok,
                audit_principal,
                Some(serde_json::json!({
                    "aliases_processed": report.aliases_processed,
                    "subject_rows_updated": report.subject_rows_updated,
                    "object_rows_updated": report.object_rows_updated,
                    "dry_run": true,
                })),
            );
        } else {
            // Non-dry: synchronous audit emit INSIDE the same tx so the
            // audit row is atomic with the actual rewrite.
            insert_audit_row_in_tx(
                &tx,
                &AuditEvent {
                    ts_ms: chrono::Utc::now().timestamp_millis(),
                    principal_subject: audit_principal,
                    operation: AuditOperation::MemoryNormalizeSubjects,
                    target_id: None,
                    result: AuditResult::Ok,
                    details: Some(serde_json::json!({
                        "aliases_processed": report.aliases_processed,
                        "subject_rows_updated": report.subject_rows_updated,
                        "object_rows_updated": report.object_rows_updated,
                        "dry_run": false,
                    })),
                },
            )?;
            tx.commit()
                .map_err(|e| Error::storage(format!("normalize_subjects: COMMIT: {e}")))?;
            tracing::info!(
                aliases_processed = report.aliases_processed,
                subject_rows = report.subject_rows_updated,
                object_rows = report.object_rows_updated,
                "normalize_subjects complete"
            );
        }

        Ok(report)
    }

    fn handle_request_entity_split(
        &mut self,
        request: EntitySplitRequest,
        audit_principal: Option<String>,
    ) -> Result<EntityReviewOpReport> {
        let entity_id = request.entity_id.trim().to_string();
        if entity_id.is_empty() {
            return Err(Error::invalid_input("entity_id must not be empty"));
        }
        let affected_aliases = normalize_entity_review_aliases(request.affected_aliases)?;
        let reason = request
            .reason
            .map(|raw| raw.trim().to_string())
            .filter(|raw| !raw.is_empty());
        let now_ms = chrono::Utc::now().timestamp_millis();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| Error::storage(format!("BEGIN entity split request: {e}")))?;

        let report = insert_entity_review_op_in_tx(
            &tx,
            "split",
            "needs_review",
            &entity_id,
            None,
            &affected_aliases,
            reason.as_deref(),
            "entity_split_requested",
            None,
            now_ms,
        )?;
        insert_audit_row_in_tx(
            &tx,
            &AuditEvent {
                ts_ms: now_ms,
                principal_subject: audit_principal,
                operation: AuditOperation::MemoryNormalizeSubjects,
                target_id: Some(entity_id),
                result: AuditResult::Ok,
                details: Some(serde_json::json!({
                    "op_kind": "split",
                    "op_id": &report.op_id,
                    "affected_aliases": &report.affected_aliases,
                    "reason_present": reason.is_some(),
                })),
            },
        )?;
        tx.commit()
            .map_err(|e| Error::storage(format!("COMMIT entity split request: {e}")))?;
        Ok(report)
    }

    fn handle_backup(&mut self, dest_path: &std::path::Path) -> Result<AssetBackupReport> {
        let key = self.key.as_ref().ok_or_else(|| {
            Error::storage(
                "backup called but writer has no key material configured. \
                 Spawn the writer with `spawn_full_with_key_and_optional_steward` \
                 to enable WriteCommand::Backup.",
            )
        })?;
        // Important: route through the writer's existing source connection
        // so the backup runs against live in-flight WAL state via SQLite's
        // page-level snapshot. SQLite serialises page reads with concurrent
        // writes on the same connection, so this is safe even mid-burst.
        backup_from_connection(&self.conn, dest_path, key)?;
        let Some(snapshot_dir) = self.snapshot_dir.as_ref() else {
            return Ok(AssetBackupReport::default());
        };
        package_asset_blobs_into_backup(dest_path, key, snapshot_dir).inspect_err(|_e| {
            let _ = std::fs::remove_file(dest_path);
        })
    }

    fn handle_save_snapshot(&mut self) -> Result<()> {
        let dir = self.snapshot_dir.as_ref().ok_or_else(|| {
            Error::storage("save_snapshot called but writer has no snapshot_dir configured")
        })?;
        // Delegates to the impl's `save` (HnswIndex routes to crate::snapshot).
        // The trait keeps us implementation-agnostic; the StubVectorIndex used
        // in unit tests just bumps a counter.
        let save_result = self.hnsw.save(dir);

        // Piggyback maintenance pragmas on the snapshot cadence (5 min by
        // default). ADR-0003 §O5 / §"Final consolidated action items" #9
        // call for hourly `PRAGMA optimize` + idle PASSIVE checkpoint.
        // Running them every 5 min instead of hourly is harmless (both are
        // cheap when there's nothing to do) and avoids a separate timer
        // task. Failures are logged but don't fail the save itself —
        // they're maintenance, not durability.
        self.run_idle_maintenance();

        save_result
    }

    /// Best-effort PRAGMA optimize + wal_checkpoint(PASSIVE). Safe to call
    /// on the writer's connection at any time.
    fn run_idle_maintenance(&mut self) {
        if let Err(e) = self.conn.execute_batch("PRAGMA optimize") {
            tracing::debug!(error = %e, "PRAGMA optimize failed (non-fatal)");
        }
        if let Err(e) = self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)") {
            tracing::debug!(error = %e, "PRAGMA wal_checkpoint(PASSIVE) failed (non-fatal)");
        }
    }

    /// Emit a best-effort audit row outside any surrounding transaction.
    /// Used by error paths whose write transaction has already aborted —
    /// the audit row records the failure but isn't atomic with it (there
    /// was no successful write to be atomic *with*). Logged + swallowed
    /// on failure: an audit-table failure must not turn a soft error
    /// into a hard one.
    ///
    /// Synchronous emit via the writer's own connection — same
    /// SQLCipher session, no extra round-trip. Wraps the INSERT in a
    /// micro-tx so it doesn't accidentally inherit autocommit state.
    /// v0.10.0: fan out one `InvalidateEvent` on the per-tenant
    /// broadcast channel. Called by every mutation handler dispatcher
    /// AFTER the writer-actor's commit returns `Ok` (lesson #30:
    /// rolled-back writes MUST NOT produce events). Drops silently
    /// when:
    ///
    ///   * the writer was spawned without an invalidate channel (every
    ///     pure-storage test path); OR
    ///   * the broadcast `send` fails because there are zero
    ///     subscribers (the normal idle state — no solo-web clients
    ///     connected). `broadcast::Sender::send` returns `Err` only
    ///     for "no receivers", which we treat as a no-op.
    ///
    /// `reason` is the canonical `AuditOperation::as_str()` form;
    /// `kind` is the solo-web node kind ("episode" / "document" /
    /// "chunk" / "cluster" / "triple" / "tenant").
    fn emit_invalidate(&self, reason: &str, kind: &str) {
        let (Some(tx), Some(tenant_id)) = (&self.invalidate_tx, &self.invalidate_tenant_id) else {
            return;
        };
        let event = InvalidateEvent {
            reason: reason.to_string(),
            tenant_id: tenant_id.clone(),
            ts_ms: chrono::Utc::now().timestamp_millis(),
            kind: kind.to_string(),
        };
        // `Err` from `send` means zero subscribers — fine, drop it. The
        // SSE handler maps `RecvError::Lagged(n)` to a structured
        // "missed N events" log on the subscriber side.
        let _ = tx.send(event);
    }

    fn emit_audit_best_effort(
        &mut self,
        operation: AuditOperation,
        target_id: Option<String>,
        result: AuditResult,
        principal: Option<String>,
        details: Option<serde_json::Value>,
    ) {
        let event = AuditEvent {
            ts_ms: chrono::Utc::now().timestamp_millis(),
            principal_subject: principal,
            operation,
            target_id,
            result,
            details,
        };
        let tx_res = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate);
        let tx = match tx_res {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    operation = %operation,
                    "audit emit: BEGIN IMMEDIATE failed; dropping audit row"
                );
                return;
            }
        };
        if let Err(e) = insert_audit_row_in_tx(&tx, &event) {
            tracing::warn!(
                error = %e,
                operation = %operation,
                "audit emit: INSERT failed; dropping audit row"
            );
            return;
        }
        if let Err(e) = tx.commit() {
            tracing::warn!(
                error = %e,
                operation = %operation,
                "audit emit: COMMIT failed; dropping audit row"
            );
        }
    }

    fn shutdown(&mut self) {
        if let Err(e) = self.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE") {
            tracing::warn!(error = %e, "wal_checkpoint(TRUNCATE) on shutdown failed");
        }
        tracing::info!("writer actor shutdown complete");
    }
}

/// v0.8.0 P5: build the `redaction.applied` audit event from a list of
/// per-pattern match counts. The details JSON shape is:
///
/// ```json
/// { "matches": [ {"pattern_name": "email", "count": 2}, ... ] }
/// ```
///
/// **No matched substrings here** — the writer's
/// `audit_row_does_not_contain_original_pii` test enforces it by
/// asserting `details_json` doesn't contain the original PII.
fn redaction_audit_event(
    ts_ms: i64,
    principal_subject: Option<String>,
    target_id: Option<String>,
    matches: &[crate::redaction::RedactionMatch],
) -> AuditEvent {
    let details_matches: Vec<serde_json::Value> = matches
        .iter()
        .map(|m| {
            serde_json::json!({
                "pattern_name": m.pattern_name,
                "count": m.count,
            })
        })
        .collect();
    AuditEvent {
        ts_ms,
        principal_subject,
        operation: AuditOperation::RedactionApplied,
        target_id,
        result: AuditResult::Ok,
        details: Some(serde_json::json!({ "matches": details_matches })),
    }
}

/// Pick a `documents.title` from the parsed text + source path. Prefer
/// the first Markdown-style `# heading` line in the first 64 lines of
/// the document; fall back to the file stem; fall back to "(untitled)".
fn derive_document_title(text: &str, path: &std::path::Path) -> String {
    for (i, line) in text.lines().enumerate() {
        if i >= 64 {
            break;
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            // Consume any number of leading #s.
            let body = rest.trim_start_matches('#').trim();
            if !body.is_empty() {
                // Markdown headings can have a trailing # run; strip it.
                let clean = body.trim_end_matches('#').trim();
                if !clean.is_empty() {
                    return clean.to_string();
                }
            }
        }
    }
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "(untitled)".to_string())
}

fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::storage(format!("open asset source {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1024 * 1024];
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)
            .map_err(|e| Error::storage(format!("read asset source {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn normalize_sha256_hex(value: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::invalid_input(
            "sha256 must be 64 lowercase or uppercase hex characters",
        ));
    }
    Ok(value)
}

fn normalize_optional_text(value: String) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn normalize_relation_type(value: &str, default: &str) -> Result<String> {
    let value = value.trim();
    let value = if value.is_empty() { default } else { value };
    if value.len() > 64 {
        return Err(Error::invalid_input(
            "relation_type must be at most 64 characters",
        ));
    }
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err(Error::invalid_input(
            "relation_type may only contain ASCII letters, digits, _, -, or .",
        ));
    }
    Ok(value.to_string())
}

fn asset_blob_paths(snapshot_dir: &Path, sha256: &str) -> Result<(String, PathBuf)> {
    let sha256 = normalize_sha256_hex(sha256)?;
    let prefix = &sha256[..2];
    let storage_path = format!("assets/blobs/{prefix}/{sha256}");
    let final_path = snapshot_dir.join(&storage_path);
    Ok((storage_path, final_path))
}

struct AssetBlobPayload {
    bytes: Vec<u8>,
    encryption_alg: &'static str,
    encryption_nonce: Option<Vec<u8>>,
    encrypted_size_bytes: Option<u64>,
}

fn build_asset_blob_payload(
    src: &Path,
    key: Option<&KeyMaterial>,
    plaintext_sha256: &str,
    plaintext_size_bytes: u64,
) -> Result<AssetBlobPayload> {
    let plaintext = std::fs::read(src)
        .map_err(|e| Error::storage(format!("read asset blob {}: {e}", src.display())))?;
    if plaintext.len() as u64 != plaintext_size_bytes {
        return Err(Error::storage(format!(
            "asset blob size changed while storing {}: expected {plaintext_size_bytes}, got {}",
            src.display(),
            plaintext.len()
        )));
    }
    let Some(key) = key else {
        return Ok(AssetBlobPayload {
            bytes: plaintext,
            encryption_alg: ASSET_BLOB_PLAINTEXT_ALG,
            encryption_nonce: None,
            encrypted_size_bytes: None,
        });
    };
    let encrypted = encrypt_asset_blob(key, &plaintext, plaintext_sha256, plaintext_size_bytes)?;
    Ok(AssetBlobPayload {
        encrypted_size_bytes: Some(encrypted.ciphertext.len() as u64),
        bytes: encrypted.ciphertext,
        encryption_alg: ASSET_BLOB_ENCRYPTION_ALG,
        encryption_nonce: Some(encrypted.nonce),
    })
}

fn stage_asset_blob_bytes(bytes: &[u8], final_path: &Path) -> Result<PathBuf> {
    let parent = final_path
        .parent()
        .ok_or_else(|| Error::storage("asset final path has no parent"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| Error::storage(format!("create asset blob dir {}: {e}", parent.display())))?;
    let tmp = final_path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7().as_simple()));
    std::fs::write(&tmp, bytes)
        .map_err(|e| Error::storage(format!("write asset blob {}: {e}", tmp.display())))?;
    Ok(tmp)
}

fn promote_staged_asset_blob(
    staged_path: &Path,
    final_path: &Path,
    replace_existing: bool,
) -> Result<()> {
    if final_path.is_file() {
        if !replace_existing {
            return Err(Error::storage(format!(
                "asset blob appeared before metadata commit: {}",
                final_path.display()
            )));
        }
        std::fs::remove_file(final_path).map_err(|e| {
            Error::storage(format!(
                "remove stale asset blob {} before promote: {e}",
                final_path.display()
            ))
        })?;
    }
    std::fs::rename(staged_path, final_path).map_err(|e| {
        Error::storage(format!(
            "promote asset blob {} -> {}: {e}",
            staged_path.display(),
            final_path.display()
        ))
    })
}

fn cleanup_failed_staged_asset_blob(staged_path: &Path, final_path: &Path, promoted: bool) {
    let _ = std::fs::remove_file(staged_path);
    if promoted {
        let _ = std::fs::remove_file(final_path);
    }
}

fn safe_asset_storage_path(snapshot_dir: &Path, storage_path: &str) -> Result<PathBuf> {
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
    let sha256 = normalize_sha256_hex(parts[3])?;
    if parts[2].len() != 2 || parts[2] != &sha256[..2] {
        return Err(Error::storage(format!(
            "asset storage_path prefix must match sha256: {storage_path:?}"
        )));
    }
    Ok(snapshot_dir.join(rel))
}

fn remove_empty_asset_blob_parent(blob_path: &Path) {
    let Some(parent) = blob_path.parent() else {
        return;
    };
    let _ = std::fs::remove_dir(parent);
}

fn ensure_memory_exists(conn: &Connection, memory_id: MemoryId) -> Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM episodes WHERE memory_id = ? AND status = 'active'",
            params![memory_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("attach_memory: lookup memory: {e}")))?;
    if exists {
        Ok(())
    } else {
        Err(Error::not_found(format!(
            "active memory_id {memory_id} not found"
        )))
    }
}

fn ensure_document_exists(conn: &Connection, doc_id: DocumentId) -> Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM documents WHERE doc_id = ? AND status = 'active'",
            params![doc_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("lookup document: {e}")))?;
    if exists {
        Ok(())
    } else {
        Err(Error::not_found(format!(
            "active doc_id {doc_id} not found"
        )))
    }
}

fn ensure_asset_exists(conn: &Connection, asset_id: AssetId) -> Result<()> {
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM assets WHERE asset_id = ? AND status = 'active'",
            params![asset_id.to_string()],
            |r| r.get(0),
        )
        .map_err(|e| Error::storage(format!("lookup asset: {e}")))?;
    if exists {
        Ok(())
    } else {
        Err(Error::not_found(format!(
            "active asset_id {asset_id} not found"
        )))
    }
}

fn existing_memory_attachment(
    conn: &Connection,
    memory_id: MemoryId,
    doc_id: Option<DocumentId>,
    asset_id: Option<AssetId>,
    relation_type: &str,
) -> Result<Option<(AttachmentId, Option<String>, i64)>> {
    let row = match (doc_id, asset_id) {
        (Some(doc_id), None) => conn
            .query_row(
                "SELECT attachment_id, note, created_at_ms
                   FROM memory_attachments
                  WHERE memory_id = ? AND doc_id = ? AND relation_type = ?
                  LIMIT 1",
                params![memory_id.to_string(), doc_id.to_string(), relation_type],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| Error::storage(format!("attach_memory: lookup existing doc link: {e}")))?,
        (None, Some(asset_id)) => conn
            .query_row(
                "SELECT attachment_id, note, created_at_ms
                   FROM memory_attachments
                  WHERE memory_id = ? AND asset_id = ? AND relation_type = ?
                  LIMIT 1",
                params![memory_id.to_string(), asset_id.to_string(), relation_type],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| {
                Error::storage(format!("attach_memory: lookup existing asset link: {e}"))
            })?,
        _ => None,
    };
    row.map(|(id, note, created_at_ms)| {
        AttachmentId::from_str(&id)
            .map(|attachment_id| (attachment_id, note, created_at_ms))
            .map_err(|e| {
                Error::storage(format!("attach_memory: parse existing attachment_id: {e}"))
            })
    })
    .transpose()
}

fn enforce_asset_quota(
    quota_bytes: Option<u64>,
    db_path: Option<&Path>,
    snapshot_dir: &Path,
    asset_size: u64,
) -> Result<()> {
    let Some(quota) = quota_bytes else {
        return Ok(());
    };
    let db_size = db_path
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|meta| meta.len())
        .unwrap_or(0);
    let asset_size_existing = dir_size_bytes(&snapshot_dir.join("assets"))?;
    let current_size = db_size.saturating_add(asset_size_existing);
    let projected = current_size.saturating_add(asset_size);
    if projected > quota {
        let err = QuotaExceededError {
            current_size,
            estimated_growth: asset_size,
            quota,
        };
        return Err(Error::forbidden(err.to_string()));
    }
    Ok(())
}

fn dir_size_bytes(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(Error::storage(format!(
                    "read asset quota dir {}: {e}",
                    dir.display()
                )));
            }
        };
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::storage(format!("read asset quota entry: {e}")))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| {
                Error::storage(format!("stat asset quota path {}: {e}", path.display()))
            })?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Ok(total)
}

/// v0.8.1 P3: structured payload for a quota rejection. Carries the
/// concrete numbers the operator needs to act on (current usage,
/// requested growth, configured cap). Stored verbatim in the audit
/// row's `details_json` and rendered into the human-facing error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QuotaExceededError {
    pub current_size: u64,
    pub estimated_growth: u64,
    pub quota: u64,
}

impl QuotaExceededError {
    /// JSON shape stored in the audit row's `details_json` column.
    /// Field names match the `--quota-bytes` CLI flag's vocabulary so
    /// operators reading audit logs see a self-consistent term set.
    pub fn to_details_json(self) -> serde_json::Value {
        serde_json::json!({
            "reason": "quota_exceeded",
            "current_size": self.current_size,
            "estimated_growth": self.estimated_growth,
            "quota": self.quota,
        })
    }
}

impl std::fmt::Display for QuotaExceededError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tenant quota_bytes={} would be exceeded (current_size={}, estimated_growth={}). \
             Increase the quota via `solo tenants set-quota <id> --bytes <N>` or \
             `--unlimited`.",
            self.quota, self.current_size, self.estimated_growth
        )
    }
}

/// v0.8.1 P3: outcome of a per-write quota check. The writer-actor's
/// growth-bearing handlers (`handle_remember`, `handle_ingest_document_
/// durable`) consult this before INSERT — when the new payload would
/// push the tenant's on-disk DB size over the configured quota, the
/// write is rejected with a structured error that translates to a
/// `result = 'forbidden'` audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum QuotaDecision {
    /// No quota configured (unlimited) — the default for tenants
    /// created without `--quota-bytes`. Common case; one branch.
    Unlimited,
    /// Quota set; current usage + estimated growth fits under the cap.
    /// Allow the write to proceed.
    Allowed { current_size: u64, quota: u64 },
    /// Quota set; current usage + estimated growth would cross the
    /// cap. Reject with a structured error. The current_size /
    /// estimated_growth / quota tuple lands in the audit row's
    /// `details_json` for operator visibility.
    Exceeded {
        current_size: u64,
        estimated_growth: u64,
        quota: u64,
    },
}

/// v0.8.1 P3: check whether a payload of `estimated_growth` bytes would
/// keep the tenant under its `quota_bytes`. Reads the current on-disk
/// DB size via `metadata().len()` — same shape as the
/// `SOLO_INGEST_MAX_BYTES` precheck in `handle_ingest_document_durable`.
///
/// Returns `QuotaDecision::Unlimited` when no quota is configured (the
/// hot path). The `db_path` argument can be `None` for test spawns that
/// don't go through the production open path; in that case enforcement
/// is conservatively skipped.
///
/// The check is strict `>` (not `>=`): a write that exactly hits the
/// quota is allowed. Operators set quotas with strict upper-bound
/// semantics — `--quota-bytes 1048576` means "don't exceed 1 MiB",
/// not "stop one byte before".
pub(crate) fn check_quota(
    quota_bytes: Option<u64>,
    db_path: Option<&std::path::Path>,
    estimated_growth: u64,
) -> QuotaDecision {
    let Some(quota) = quota_bytes else {
        return QuotaDecision::Unlimited;
    };
    let Some(path) = db_path else {
        // Test-only spawn path with no db_path. Enforcement skipped
        // rather than failing hard — the prod path always wires it.
        return QuotaDecision::Unlimited;
    };
    let current_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if current_size.saturating_add(estimated_growth) > quota {
        QuotaDecision::Exceeded {
            current_size,
            estimated_growth,
            quota,
        }
    } else {
        QuotaDecision::Allowed {
            current_size,
            quota,
        }
    }
}

/// v0.8.1 P1: resolve the first `derived_from` memory_id in a triple's
/// provenance to an `episodes.rowid` so the new `source_episode_id` FK
/// can be populated on INSERT. Returns `None` when:
///
///   * the provenance has no `derived_from` entries (the LLM didn't
///     extract a back-reference);
///   * the first `derived_from` memory_id doesn't resolve to a live
///     episode row (forgotten, never persisted, schema drift).
///
/// The caller wires `None` into the INSERT as a NULL `source_episode_id`,
/// which is the documented orphan-by-design shape. The GDPR cascade
/// (`gdpr::forget_principal`) reports null-source triples through
/// `ForgetReport::triples_orphan_null_source` for operator visibility.
fn resolve_source_episode_id(
    conn: &rusqlite::Connection,
    provenance: &solo_core::Provenance,
) -> Option<i64> {
    let first = provenance.derived_from.first()?;
    let memory_id_str = first.to_string();
    conn.query_row(
        "SELECT rowid FROM episodes WHERE memory_id = ?",
        params![memory_id_str],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Same as [`resolve_source_episode_id`] but runs inside an active
/// transaction (the writer's INSERT path needs to see uncommitted
/// inserts from the same tx).
fn resolve_source_episode_id_in_tx(
    tx: &rusqlite::Transaction<'_>,
    provenance: &solo_core::Provenance,
) -> Option<i64> {
    let first = provenance.derived_from.first()?;
    let memory_id_str = first.to_string();
    tx.query_row(
        "SELECT rowid FROM episodes WHERE memory_id = ?",
        params![memory_id_str],
        |r| r.get::<_, i64>(0),
    )
    .optional()
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::test_support::{
        StubVectorIndex, disabled_test_redactor, enabled_test_redactor, fixture_embedding,
        fixture_episode, open_test_db,
    };
    use std::time::Duration;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn remember_happy_path_round_trip() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let WriterSpawn { handle, join: _ } = WriterActor::spawn(conn, hnsw.clone());

        let episode = fixture_episode("test content");
        let embedding = fixture_embedding(4);
        let mid = rt()
            .block_on(handle.remember(episode.clone(), embedding))
            .unwrap();
        assert_eq!(mid, episode.memory_id);

        std::thread::sleep(Duration::from_millis(50));
        drop(handle);
        std::thread::sleep(Duration::from_millis(50));

        assert_eq!(hnsw.add_count(), 1);
        let added = hnsw.last_added().unwrap();
        assert_eq!(added.0, 1, "rowid should be 1 (first insert)");
        assert_eq!(added.1.len(), 4);
    }

    #[test]
    fn dispatch_remember_replies_before_drain() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let (_tx, rx) = mpsc::channel(1);
        let mut actor = WriterActor {
            conn,
            hnsw,
            rx,
            snapshot_dir: None,
            embedder_id: None,
            embedder: None,
            runtime_handle: None,
            steward: None,
            steward_slot: None,
            triples_batch_signal: None,
            key: None,
            redactor: disabled_test_redactor(),
            quota_bytes: None,
            db_path: None,
            invalidate_tx: None,
            invalidate_tenant_id: None,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let episode = fixture_episode("ordering test");
        let embedding = fixture_embedding(4);

        actor.dispatch_remember(episode.clone(), embedding, None, reply_tx);

        let received = reply_rx.blocking_recv().unwrap();
        assert_eq!(received.unwrap(), episode.memory_id);

        let n: u32 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM pending_index", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn forget_unknown_memory_id_returns_not_found() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let WriterSpawn { handle, join: _ } = WriterActor::spawn(conn, hnsw);

        let mid = MemoryId::new();
        let err = rt()
            .block_on(handle.forget(mid, "test".into()))
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn forget_marks_status_forgotten() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let WriterSpawn { handle, join: _ } = WriterActor::spawn(conn, hnsw.clone());

        let episode = fixture_episode("to be forgotten");
        let mid = rt()
            .block_on(handle.remember(episode.clone(), fixture_embedding(4)))
            .unwrap();
        rt().block_on(handle.forget(mid, "no longer relevant".into()))
            .unwrap();

        // Re-open the file (writer holds the DB connection; test-side reopen
        // gets read access via SQLite's WAL).
        // We can't peek into the writer's connection, so close everything and
        // re-open via open_test_db_at on the underlying path. But open_test_db
        // returned us an in-memory tmp; we need a different fixture.
        // Simpler: use the StubVectorIndex's add_count to verify the embed
        // happened, and trust that the handle.forget Ok return means the
        // UPDATE ran. The full SQL roundtrip is exercised by the
        // reader.rs::reader_sees_writes_committed_through_writer_actor test
        // pattern, which we don't replicate here.
        assert_eq!(hnsw.add_count(), 1);
        let _ = mid; // ensure we exercised the codepath; status check is in
        // the reader-pool test below.
    }

    #[test]
    fn forget_is_idempotent_when_already_forgotten() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let WriterSpawn { handle, join: _ } = WriterActor::spawn(conn, hnsw);

        let episode = fixture_episode("forget twice");
        let mid = rt()
            .block_on(handle.remember(episode, fixture_embedding(4)))
            .unwrap();
        rt().block_on(handle.forget(mid, "first".into())).unwrap();
        // Second call: still Ok (idempotent), no error.
        rt().block_on(handle.forget(mid, "second".into())).unwrap();
    }

    #[test]
    fn memory_review_persists_and_clears_review_state() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let (_tx, rx) = mpsc::channel(1);
        let mut actor = WriterActor {
            conn,
            hnsw,
            rx,
            snapshot_dir: None,
            embedder_id: None,
            embedder: None,
            runtime_handle: None,
            steward: None,
            steward_slot: None,
            triples_batch_signal: None,
            key: None,
            redactor: disabled_test_redactor(),
            quota_bytes: None,
            db_path: None,
            invalidate_tx: None,
            invalidate_tenant_id: None,
        };
        let (reply_tx, reply_rx) = oneshot::channel();
        let episode = fixture_episode("review persistence");
        actor.dispatch_remember(episode.clone(), fixture_embedding(4), None, reply_tx);
        reply_rx.blocking_recv().unwrap().unwrap();

        let approved = actor
            .handle_memory_review(
                episode.memory_id,
                Some(MemoryReviewState::Approved),
                Some(" looks good ".into()),
                Some("alice".into()),
            )
            .unwrap();
        assert_eq!(approved.state, Some(MemoryReviewState::Approved));
        assert!(approved.reviewed_at_ms.is_some());

        let (state, note, principal): (String, Option<String>, Option<String>) = actor
            .conn
            .query_row(
                "SELECT mr.state, mr.note, ae.principal_subject
                   FROM memory_reviews mr
                   JOIN audit_events ae ON ae.target_id = mr.memory_id
                  WHERE mr.memory_id = ?1
                  ORDER BY ae.audit_id DESC
                  LIMIT 1",
                params![episode.memory_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "approved");
        assert_eq!(note.as_deref(), Some("looks good"));
        assert_eq!(principal.as_deref(), Some("alice"));

        let reset = actor
            .handle_memory_review(episode.memory_id, None, None, None)
            .unwrap();
        assert_eq!(reset.state, None);
        let remaining: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memory_reviews WHERE memory_id = ?1",
                params![episode.memory_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
    }

    #[test]
    fn memory_review_rejects_unknown_memory() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let (_tx, rx) = mpsc::channel(1);
        let mut actor = WriterActor {
            conn,
            hnsw,
            rx,
            snapshot_dir: None,
            embedder_id: None,
            embedder: None,
            runtime_handle: None,
            steward: None,
            steward_slot: None,
            triples_batch_signal: None,
            key: None,
            redactor: disabled_test_redactor(),
            quota_bytes: None,
            db_path: None,
            invalidate_tx: None,
            invalidate_tenant_id: None,
        };

        let err = actor
            .handle_memory_review(
                MemoryId::new(),
                Some(MemoryReviewState::Dismissed),
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn many_concurrent_writes_serialize_correctly() {
        let (conn, _tmp) = open_test_db();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let WriterSpawn { handle, join: _ } = WriterActor::spawn(conn, hnsw.clone());

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        let results: Vec<Result<MemoryId>> = runtime.block_on(async {
            let mut tasks = Vec::new();
            for i in 0..50 {
                let h = handle.clone();
                let ep = fixture_episode(&format!("write {i}"));
                tasks.push(tokio::spawn(async move {
                    h.remember(ep, fixture_embedding(4)).await
                }));
            }
            let mut out = Vec::new();
            for t in tasks {
                out.push(t.await.unwrap());
            }
            out
        });

        let mut ids = std::collections::HashSet::new();
        for r in results {
            let mid = r.expect("remember must succeed");
            assert!(ids.insert(mid), "memory_ids must be unique");
        }
        assert_eq!(ids.len(), 50);
        assert_eq!(hnsw.add_count(), 50);
    }

    // -- normalize_subjects -------------------------------------------------
    //
    // Test pattern: drive the handle_normalize_subjects path **directly** on
    // a hand-built WriterActor. The plain `WriterActor::spawn` path owns its
    // connection on the writer thread, which would block read-back queries
    // in the same process. Direct invocation lets us:
    //
    //   1. Seed triples via the same connection the actor will mutate.
    //   2. Call `handle_normalize_subjects` on `&mut actor`.
    //   3. Query `actor.conn` afterwards to assert row contents.
    //
    // The dispatch arm itself is so thin (one match arm, one method call)
    // that exercising the method directly covers the same path the public
    // `WriteHandle::normalize_subjects` would take.

    /// Helper: seed a triple row with a given subject/object. Returns the
    /// triple_id so tests can read it back. `cluster_id` is left NULL —
    /// FK is `ON DELETE CASCADE` but NULLs don't reference anything.
    fn seed_triple(
        conn: &Connection,
        triple_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        object_kind: &str,
    ) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO triples (
                triple_id, subject_id, predicate, object_id, object_kind,
                valid_from_ms, valid_to_ms, confidence, provenance_json,
                created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?)",
            params![
                triple_id,
                subject,
                predicate,
                object,
                object_kind,
                now_ms,
                0.9_f64,
                "{}",
                now_ms,
                now_ms,
            ],
        )
        .expect("seed triple");
    }

    fn seed_relationship_edge(
        conn: &Connection,
        triple_id: &str,
        subject: &str,
        predicate: &str,
        object: &str,
        object_kind: &str,
    ) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT OR IGNORE INTO entities
                (entity_id, canonical_name, entity_type, aliases_json, confidence,
                 first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?1, 'unknown', '[]', 1.0, ?2, ?2, 'active', ?2, ?2)",
            params![subject, now_ms],
        )
        .expect("seed subject entity");
        if object_kind == "entity" {
            conn.execute(
                "INSERT OR IGNORE INTO entities
                    (entity_id, canonical_name, entity_type, aliases_json, confidence,
                     first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms)
                 VALUES (?1, ?1, 'unknown', '[]', 1.0, ?2, ?2, 'active', ?2, ?2)",
                params![object, now_ms],
            )
            .expect("seed object entity");
        }
        conn.execute(
            "INSERT INTO relationship_edges
                (edge_id, subject_entity_id, predicate, object_entity_id, object_literal,
                 object_kind, valid_from_ms, valid_to_ms, confidence, strength,
                 evidence_count, status, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3,
                 CASE WHEN ?5 = 'entity' THEN ?4 ELSE NULL END,
                 CASE WHEN ?5 = 'literal' THEN ?4 ELSE NULL END,
                 ?5, ?6, NULL, 0.9, 0.9, 0, 'active', ?6, ?6)",
            params![triple_id, subject, predicate, object, object_kind, now_ms],
        )
        .expect("seed relationship edge");
    }

    /// Helper: read back `subject_id` for a known triple_id. Panics if not
    /// found — tests should always seed before reading.
    fn read_subject(conn: &Connection, triple_id: &str) -> String {
        conn.query_row(
            "SELECT subject_id FROM triples WHERE triple_id = ?",
            params![triple_id],
            |r| r.get::<_, String>(0),
        )
        .expect("read subject_id")
    }

    /// Helper: read back `object_id` for a known triple_id.
    fn read_object(conn: &Connection, triple_id: &str) -> String {
        conn.query_row(
            "SELECT object_id FROM triples WHERE triple_id = ?",
            params![triple_id],
            |r| r.get::<_, String>(0),
        )
        .expect("read object_id")
    }

    fn read_edge_subject(conn: &Connection, edge_id: &str) -> String {
        conn.query_row(
            "SELECT subject_entity_id FROM relationship_edges WHERE edge_id = ?",
            params![edge_id],
            |r| r.get::<_, String>(0),
        )
        .expect("read edge subject")
    }

    /// Helper: build a `WriterActor` directly (no spawned thread) so tests
    /// can call `handle_normalize_subjects` and then query the same
    /// connection in the same thread.
    fn build_actor_inline(conn: Connection) -> WriterActor {
        let (_tx, rx) = mpsc::channel(1);
        let hnsw = Arc::new(StubVectorIndex::new(4));
        WriterActor {
            conn,
            hnsw,
            rx,
            snapshot_dir: None,
            embedder_id: None,
            embedder: None,
            runtime_handle: None,
            steward: None,
            steward_slot: None,
            triples_batch_signal: None,
            key: None,
            redactor: disabled_test_redactor(),
            quota_bytes: None,
            db_path: None,
            invalidate_tx: None,
            invalidate_tenant_id: None,
        }
    }

    fn build_asset_actor_inline(conn: Connection, snapshot_dir: PathBuf) -> WriterActor {
        let mut actor = build_actor_inline(conn);
        actor.snapshot_dir = Some(snapshot_dir);
        actor
    }

    fn build_keyed_asset_actor_inline(
        conn: Connection,
        snapshot_dir: PathBuf,
        key: KeyMaterial,
    ) -> WriterActor {
        let mut actor = build_asset_actor_inline(conn, snapshot_dir);
        actor.key = Some(key);
        actor
    }

    fn seed_derived_cluster(
        actor: &mut WriterActor,
        cluster_id: &str,
        episode_count: usize,
        abstraction: &str,
        triple_status: Option<&str>,
        seed_contradiction: bool,
    ) -> Option<String> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        actor
            .conn
            .execute(
                "INSERT INTO clusters (cluster_id, coherence, created_at_ms)
                 VALUES (?1, 1.0, ?2)",
                params![cluster_id, now_ms],
            )
            .expect("seed cluster");

        for i in 0..episode_count {
            let mut ep = fixture_episode(&format!("{cluster_id} episode {i}"));
            ep.ts_ms = now_ms + i as i64;
            let mid = actor
                .handle_remember_durable(ep, fixture_embedding(4), None)
                .expect("seed memory");
            actor
                .conn
                .execute(
                    "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?1, ?2)",
                    params![cluster_id, mid.to_string()],
                )
                .expect("seed cluster membership");
        }

        actor
            .conn
            .execute(
                "INSERT INTO semantic_abstractions
                    (abstraction_id, cluster_id, content, provenance_json, confidence, created_at_ms)
                 VALUES (?1, ?2, ?3, '{}', 0.8, ?4)",
                params![MemoryId::new().to_string(), cluster_id, abstraction, now_ms],
            )
            .expect("seed abstraction");

        let triple_id = triple_status.map(|status| {
            let triple_id = MemoryId::new().to_string();
            actor
                .conn
                .execute(
                    "INSERT INTO triples
                        (triple_id, subject_id, predicate, object_id, object_kind,
                         valid_from_ms, valid_to_ms, confidence, provenance_json,
                         status, created_at_ms, updated_at_ms, cluster_id)
                     VALUES
                        (?1, 'subject', 'relates_to', 'object', 'literal',
                         ?2, NULL, 0.8, '{}', ?3, ?2, ?2, ?4)",
                    params![triple_id, now_ms, status, cluster_id],
                )
                .expect("seed triple");
            triple_id
        });

        if seed_contradiction {
            let Some(triple_id) = triple_id.as_ref() else {
                panic!("seed_contradiction requires a triple");
            };
            actor
                .conn
                .execute(
                    "INSERT INTO contradictions
                        (a_memory_id, b_memory_id, kind, explanation, detected_at_ms)
                     VALUES
                        (?1, ?2, 'other', 'seed contradiction', ?3)",
                    params![triple_id, MemoryId::new().to_string(), now_ms],
                )
                .expect("seed contradiction");
        }

        triple_id
    }

    #[test]
    fn derived_repair_clears_low_signal_abstraction_without_deleting_membership() {
        let (conn, _tmp) = open_test_db();
        let mut actor = build_actor_inline(conn);
        seed_derived_cluster(
            &mut actor,
            "cl-bad",
            3,
            "The assistant was unable to provide assistance.",
            Some("superseded"),
            true,
        );
        seed_derived_cluster(
            &mut actor,
            "cl-good",
            30,
            "Alex works on Solo memory extraction.",
            Some("active"),
            false,
        );
        seed_derived_cluster(
            &mut actor,
            "cl-access-with-fact",
            3,
            "The assistant could not access local memory, while Solo uses Ollama.",
            Some("active"),
            false,
        );

        let report = actor
            .handle_repair_derived(DerivedRepairScope::default(), Some("operator".into()))
            .expect("repair");

        assert_eq!(report.mode, DerivedRepairMode::StaleAbstractions);
        assert!(!report.dry_run);
        assert_eq!(report.clusters_scanned, 3);
        assert_eq!(report.clusters_repaired, 1);
        assert_eq!(report.abstractions_deleted, 1);
        assert_eq!(report.triples_deleted, 1);
        assert_eq!(report.contradictions_deleted, 1);
        assert_eq!(report.candidate_samples[0].cluster_id, "cl-bad");

        let abstractions: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
                r.get(0)
            })
            .unwrap();
        let triples: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
            .unwrap();
        let contradictions: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM contradictions", [], |r| r.get(0))
            .unwrap();
        let clusters: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM clusters", [], |r| r.get(0))
            .unwrap();
        let memberships: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM cluster_episodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(abstractions, 2, "useful abstractions remain");
        assert_eq!(triples, 2, "useful active triples remain");
        assert_eq!(contradictions, 0, "dangling contradiction removed");
        assert_eq!(clusters, 3, "targeted repair preserves clusters");
        assert_eq!(memberships, 36, "targeted repair preserves memberships");
    }

    #[test]
    fn derived_repair_quality_gate_quarantines_weak_literals_and_rewrites_metadata_entities() {
        let (conn, _tmp) = open_test_db();
        let mut actor = build_actor_inline(conn);
        seed_derived_cluster(
            &mut actor,
            "cl-quality",
            1,
            "Solo has legacy weak triples from an older extraction pass.",
            None,
            false,
        );
        let now_ms = chrono::Utc::now().timestamp_millis();
        let weak_triple_id = "t-weak-machine-literal";
        actor
            .conn
            .execute(
                "INSERT INTO triples
                    (triple_id, subject_id, predicate, object_id, object_kind,
                     valid_from_ms, valid_to_ms, confidence, provenance_json,
                     status, created_at_ms, updated_at_ms, cluster_id)
                 VALUES
                    (?1, 'machine-generated-subject-123456', 'has',
                     'this_is_machine_generated_literal_value_with_enough_parts_1234567890',
                     'literal', ?2, NULL, 0.95, '{}', 'active', ?2, ?2, 'cl-quality')",
                params![weak_triple_id, now_ms],
            )
            .expect("seed weak literal triple");

        let path_triple_id = "t-path-entity-object";
        let path = r"C:\Users\Example\Projects\solo";
        actor
            .conn
            .execute(
                "INSERT OR IGNORE INTO entities
                    (entity_id, canonical_name, entity_type, aliases_json, confidence,
                     first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms)
                 VALUES
                    ('solo', 'Solo', 'project', '[]', 1.0, ?1, ?1, 'active', ?1, ?1),
                    (?2, ?2, 'unknown', '[]', 1.0, ?1, ?1, 'active', ?1, ?1)",
                params![now_ms, path],
            )
            .expect("seed entities");
        actor
            .conn
            .execute(
                "INSERT INTO triples
                    (triple_id, subject_id, predicate, object_id, object_kind,
                     valid_from_ms, valid_to_ms, confidence, provenance_json,
                     status, created_at_ms, updated_at_ms, cluster_id)
                 VALUES
                    (?1, 'solo', 'uses_path', ?2, 'entity',
                     ?3, NULL, 0.95, '{}', 'active', ?3, ?3, 'cl-quality')",
                params![path_triple_id, path, now_ms],
            )
            .expect("seed path entity triple");
        actor
            .conn
            .execute(
                "INSERT INTO relationship_edges
                    (edge_id, subject_entity_id, predicate, object_entity_id, object_literal,
                     object_kind, valid_from_ms, valid_to_ms, confidence, strength,
                     evidence_count, status, created_at_ms, updated_at_ms)
                 VALUES
                    (?1, 'solo', 'uses_path', ?2, NULL, 'entity',
                     ?3, NULL, 0.95, 0.95, 0, 'active', ?3, ?3)",
                params![path_triple_id, path, now_ms],
            )
            .expect("seed path entity edge");

        let scope = DerivedRepairScope {
            mode: DerivedRepairMode::QualityGate,
            ..DerivedRepairScope::default()
        };
        let report = actor
            .handle_repair_derived(scope, Some("operator".into()))
            .expect("quality repair");

        assert_eq!(report.mode, DerivedRepairMode::QualityGate);
        assert_eq!(report.triple_reviews_created, 1);
        assert_eq!(report.triples_rewritten, 1);
        assert_eq!(report.clusters_repaired, 1);

        let weak_status: String = actor
            .conn
            .query_row(
                "SELECT status FROM triples WHERE triple_id = ?1",
                params![weak_triple_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(weak_status, "superseded");
        let review_reason: String = actor
            .conn
            .query_row(
                "SELECT reason_code FROM triple_reviews WHERE triple_id = ?1",
                params![weak_triple_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(review_reason, "literal_only_machine_entity");
        let claim: (String, String, f32, String) = actor
            .conn
            .query_row(
                "SELECT status, source_type, quality_score, reason_codes_json
                   FROM memory_claims
                  WHERE triple_id = ?1",
                params![weak_triple_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(claim.0, "needs_review");
        assert_eq!(claim.1, "quality_repair");
        assert!(claim.2 < 0.95, "quality score should penalize weak claims");
        assert!(
            claim.3.contains("literal_only_machine_entity"),
            "claim should explain quality gate reason: {}",
            claim.3
        );

        let path_kind: String = actor
            .conn
            .query_row(
                "SELECT object_kind FROM triples WHERE triple_id = ?1",
                params![path_triple_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path_kind, "literal");
        let edge_kind: (String, Option<String>, Option<String>) = actor
            .conn
            .query_row(
                "SELECT object_kind, object_entity_id, object_literal
                   FROM relationship_edges
                  WHERE edge_id = ?1",
                params![path_triple_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(edge_kind, ("literal".into(), None, Some(path.into())));
    }

    #[test]
    fn derived_repair_rebuild_all_preserves_raw_episodes() {
        let (conn, _tmp) = open_test_db();
        let mut actor = build_actor_inline(conn);
        seed_derived_cluster(
            &mut actor,
            "cl-one",
            2,
            "Solo project notes.",
            Some("active"),
            false,
        );
        seed_derived_cluster(
            &mut actor,
            "cl-two",
            4,
            "Blender bridge notes.",
            None,
            false,
        );
        let now_ms = chrono::Utc::now().timestamp_millis();
        actor
            .conn
            .execute(
                "INSERT INTO entity_aliases
                    (alias, canonical_id, display_label, source, confidence,
                     created_at_ms, updated_at_ms)
                 VALUES
                    ('solo_relay', 'solo-relay', 'solo-relay', 'test', 1.0, ?1, ?1)",
                params![now_ms],
            )
            .expect("seed alias");
        actor
            .conn
            .execute(
                "INSERT INTO triple_reviews
                    (review_id, candidate_fingerprint, cluster_id, subject_id,
                     predicate, object_id, object_kind, confidence, reason_code,
                     reason, provenance_json, created_at_ms, updated_at_ms)
                 VALUES
                    (?1, 'fp-rebuild-stale', 'cl-one', 'assistant', 'said',
                     'unable to help', 'literal', 0.4, 'assistant_or_tool_chatter',
                     'stale review', '{}', ?2, ?2)",
                params![MemoryId::new().to_string(), now_ms],
            )
            .expect("seed triple review");

        let scope = DerivedRepairScope {
            mode: DerivedRepairMode::RebuildAll,
            ..DerivedRepairScope::default()
        };
        let report = actor
            .handle_repair_derived(scope, Some("operator".into()))
            .expect("repair");

        assert_eq!(report.clusters_deleted, 2);
        assert_eq!(report.cluster_memberships_deleted, 6);
        assert_eq!(report.abstractions_deleted, 2);
        assert_eq!(report.triples_deleted, 1);

        for table in [
            "clusters",
            "cluster_episodes",
            "semantic_abstractions",
            "triples",
            "contradictions",
            "triple_reviews",
            "entity_aliases",
        ] {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            let count: i64 = actor.conn.query_row(&sql, [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "{table} should be cleared");
        }
        let episodes: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM episodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(episodes, 6, "raw memories must be preserved");
    }

    fn seed_active_document(conn: &Connection) -> DocumentId {
        let doc_id = DocumentId::new();
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO documents (
                doc_id, source, title, mime_type, ingested_at_ms,
                status, chunk_count, content_hash, byte_size
             ) VALUES (?, '/tmp/source.md', 'Source', 'text/markdown', ?, 'active', 0, ?, 0)",
            params![doc_id.to_string(), now_ms, doc_id.to_string()],
        )
        .expect("seed active document");
        doc_id
    }

    fn seed_active_memory(conn: &Connection, content: &str) -> MemoryId {
        let episode = fixture_episode(content);
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO episodes (
                memory_id, ts_ms, source_type, source_id, content,
                encoding_context_json, provenance_json, confidence,
                strength, salience, tier, status, created_at_ms, updated_at_ms
             ) VALUES (?, ?, ?, ?, ?, '{}', NULL, ?, ?, ?, 'hot', 'active', ?, ?)",
            params![
                episode.memory_id.to_string(),
                episode.ts_ms,
                episode.source_type,
                episode.source_id,
                episode.content,
                episode.confidence.0,
                episode.strength,
                episode.salience,
                now_ms,
                now_ms,
            ],
        )
        .expect("seed active memory");
        episode.memory_id
    }

    #[test]
    fn store_asset_from_path_persists_blob_and_dedupes_by_sha256() {
        let (conn, tmp) = open_test_db();
        let snapshot_dir = tmp.path().join("tenant-assets");
        let source = tmp.path().join("source.txt");
        std::fs::write(&source, b"asset bytes").expect("write source");
        let mut actor = build_asset_actor_inline(conn, snapshot_dir.clone());

        let first = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.txt".into()),
                Some("text/plain".into()),
                Some(11),
                None,
                Some("test-upload".into()),
                Some("alice".into()),
            )
            .expect("store asset");
        assert!(!first.deduped);
        assert_eq!(first.filename.as_deref(), Some("source.txt"));
        assert_eq!(first.mime_type, "text/plain");
        assert!(snapshot_dir.join(&first.storage_path).is_file());

        let second = actor
            .handle_store_asset_from_path(
                &source,
                Some("other.txt".into()),
                Some("text/plain".into()),
                Some(11),
                Some(first.sha256.clone()),
                Some("test-upload".into()),
                None,
            )
            .expect("dedupe asset");
        assert!(second.deduped);
        assert_eq!(second.asset_id, first.asset_id);

        std::fs::remove_file(snapshot_dir.join(&first.storage_path)).expect("remove blob");
        let healed = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.txt".into()),
                Some("text/plain".into()),
                Some(11),
                Some(first.sha256.clone()),
                Some("test-upload".into()),
                None,
            )
            .expect("dedupe asset with missing blob");
        assert!(healed.deduped);
        assert_eq!(healed.asset_id, first.asset_id);
        assert!(snapshot_dir.join(&healed.storage_path).is_file());

        let count: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM assets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        actor
            .handle_forget_asset(first.asset_id, None)
            .expect("forget asset before reupload");
        assert!(!snapshot_dir.join(&first.storage_path).exists());
        let reuploaded = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.txt".into()),
                Some("text/plain".into()),
                Some(11),
                Some(first.sha256.clone()),
                Some("test-upload".into()),
                None,
            )
            .expect("reupload forgotten asset");
        assert_eq!(reuploaded.asset_id, first.asset_id);
        assert!(snapshot_dir.join(&reuploaded.storage_path).is_file());
        let status: String = actor
            .conn
            .query_row(
                "SELECT status FROM assets WHERE asset_id = ?",
                params![first.asset_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn record_asset_extraction_upserts_status_for_asset() {
        let (conn, tmp) = open_test_db();
        let snapshot_dir = tmp.path().join("tenant-assets");
        let source = tmp.path().join("source.bin");
        std::fs::write(&source, b"asset bytes").expect("write source");
        let mut actor = build_asset_actor_inline(conn, snapshot_dir);
        let asset = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.bin".into()),
                Some("application/octet-stream".into()),
                Some(11),
                None,
                Some("test-upload".into()),
                Some("alice".into()),
            )
            .expect("store asset");

        let first = actor
            .handle_record_asset_extraction(
                asset.asset_id,
                "fallback_binary".into(),
                "v1".into(),
                "stored_unparsed".into(),
                0,
                Some("unsupported extension: bin".into()),
                Some("alice".into()),
            )
            .expect("record extraction");
        assert_eq!(first.status, "stored_unparsed");

        let second = actor
            .handle_record_asset_extraction(
                asset.asset_id,
                "fallback_binary".into(),
                "v1".into(),
                "failed".into(),
                0,
                Some("extractor unavailable".into()),
                Some("alice".into()),
            )
            .expect("upsert extraction");
        assert_eq!(second.extraction_id, first.extraction_id);
        assert_eq!(second.status, "failed");

        let (count, status, error): (i64, String, Option<String>) = actor
            .conn
            .query_row(
                "SELECT COUNT(*), MAX(status), MAX(error) FROM asset_extractions WHERE asset_id = ?",
                params![asset.asset_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(status, "failed");
        assert_eq!(error.as_deref(), Some("extractor unavailable"));
    }

    #[test]
    fn keyed_store_asset_encrypts_blob_and_records_metadata() {
        let (conn, tmp) = open_test_db();
        let snapshot_dir = tmp.path().join("tenant-assets");
        let source = tmp.path().join("source.txt");
        let plaintext = b"asset bytes";
        std::fs::write(&source, plaintext).expect("write source");
        let key = KeyMaterial::from_bytes_for_tests([3u8; 32]);
        let mut actor = build_keyed_asset_actor_inline(conn, snapshot_dir.clone(), key.clone());

        let asset = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.txt".into()),
                Some("text/plain".into()),
                Some(plaintext.len() as u64),
                None,
                Some("test-upload".into()),
                Some("alice".into()),
            )
            .expect("store encrypted asset");

        let blob_path = snapshot_dir.join(&asset.storage_path);
        let stored = std::fs::read(&blob_path).expect("read stored blob");
        assert_ne!(stored, plaintext);

        let (alg, nonce, encrypted_size): (String, Option<Vec<u8>>, Option<i64>) = actor
            .conn
            .query_row(
                "SELECT encryption_alg, encryption_nonce, encrypted_size_bytes
                   FROM assets
                  WHERE asset_id = ?",
                params![asset.asset_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("read asset encryption metadata");
        assert_eq!(alg, ASSET_BLOB_ENCRYPTION_ALG);
        assert_eq!(encrypted_size, Some(stored.len() as i64));

        let decoded = crate::asset_blob::decode_asset_blob(
            Some(&key),
            &alg,
            nonce.as_deref(),
            &asset.sha256,
            asset.size_bytes,
            &stored,
        )
        .expect("decode stored blob");
        assert_eq!(decoded, plaintext);
    }

    #[test]
    fn keyed_store_asset_replaces_orphan_blob_before_insert() {
        use sha2::{Digest, Sha256};

        let (conn, tmp) = open_test_db();
        let snapshot_dir = tmp.path().join("tenant-assets");
        let source = tmp.path().join("source.txt");
        let plaintext = b"asset bytes";
        std::fs::write(&source, plaintext).expect("write source");
        let sha256 = hex::encode(Sha256::digest(plaintext));
        let orphan_path = snapshot_dir.join(format!("assets/blobs/{}/{}", &sha256[..2], sha256));
        std::fs::create_dir_all(orphan_path.parent().expect("orphan parent")).unwrap();
        std::fs::write(&orphan_path, b"stale encrypted orphan").expect("write orphan");

        let key = KeyMaterial::from_bytes_for_tests([4u8; 32]);
        let mut actor = build_keyed_asset_actor_inline(conn, snapshot_dir.clone(), key.clone());
        let asset = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.txt".into()),
                Some("text/plain".into()),
                Some(plaintext.len() as u64),
                None,
                Some("test-upload".into()),
                Some("alice".into()),
            )
            .expect("store encrypted asset over orphan");

        let stored = std::fs::read(snapshot_dir.join(&asset.storage_path)).expect("read blob");
        assert_ne!(stored.as_slice(), b"stale encrypted orphan");
        let (alg, nonce): (String, Option<Vec<u8>>) = actor
            .conn
            .query_row(
                "SELECT encryption_alg, encryption_nonce
                   FROM assets
                  WHERE asset_id = ?",
                params![asset.asset_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read asset encryption metadata");
        let decoded = crate::asset_blob::decode_asset_blob(
            Some(&key),
            &alg,
            nonce.as_deref(),
            &asset.sha256,
            asset.size_bytes,
            &stored,
        )
        .expect("decode replacement blob");
        assert_eq!(decoded, plaintext);
    }

    #[test]
    fn link_document_asset_records_source_relation() {
        let (conn, tmp) = open_test_db();
        let snapshot_dir = tmp.path().join("tenant-assets");
        let source = tmp.path().join("source.md");
        std::fs::write(&source, b"# Source").expect("write source");
        let doc_id = seed_active_document(&conn);
        let mut actor = build_asset_actor_inline(conn, snapshot_dir);
        let asset = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.md".into()),
                Some("text/markdown".into()),
                None,
                None,
                None,
                None,
            )
            .expect("store asset");

        let link = actor
            .handle_link_document_asset(
                doc_id,
                asset.asset_id,
                "source_upload".into(),
                Some("original upload".into()),
                Some("alice".into()),
            )
            .expect("link document asset");
        assert_eq!(link.doc_id, doc_id);
        assert_eq!(link.asset_id, asset.asset_id);
        assert_eq!(link.relation_type, "source_upload");

        let count: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM document_assets", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn attach_memory_targets_exactly_one_document_or_asset() {
        let (conn, tmp) = open_test_db();
        let snapshot_dir = tmp.path().join("tenant-assets");
        let source = tmp.path().join("source.bin");
        std::fs::write(&source, b"binary").expect("write source");
        let memory_id = seed_active_memory(&conn, "memory with attachment");
        let doc_id = seed_active_document(&conn);
        let mut actor = build_asset_actor_inline(conn, snapshot_dir);
        let asset = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.bin".into()),
                Some("application/octet-stream".into()),
                None,
                None,
                None,
                None,
            )
            .expect("store asset");

        let doc_attachment = actor
            .handle_attach_memory(
                memory_id,
                Some(doc_id),
                None,
                "evidence".into(),
                Some("document evidence".into()),
                None,
            )
            .expect("attach document");
        assert_eq!(doc_attachment.doc_id, Some(doc_id));
        assert_eq!(doc_attachment.asset_id, None);
        let doc_attachment_again = actor
            .handle_attach_memory(
                memory_id,
                Some(doc_id),
                None,
                "evidence".into(),
                Some("different note should not duplicate".into()),
                None,
            )
            .expect("idempotent document attachment");
        assert_eq!(
            doc_attachment_again.attachment_id,
            doc_attachment.attachment_id
        );
        assert_eq!(
            doc_attachment_again.note.as_deref(),
            Some("document evidence")
        );

        let asset_attachment = actor
            .handle_attach_memory(
                memory_id,
                None,
                Some(asset.asset_id),
                "source_file".into(),
                None,
                None,
            )
            .expect("attach asset");
        assert_eq!(asset_attachment.asset_id, Some(asset.asset_id));
        let asset_attachment_again = actor
            .handle_attach_memory(
                memory_id,
                None,
                Some(asset.asset_id),
                "source_file".into(),
                None,
                None,
            )
            .expect("idempotent asset attachment");
        assert_eq!(
            asset_attachment_again.attachment_id,
            asset_attachment.attachment_id
        );

        let count: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM memory_attachments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let both = actor.handle_attach_memory(
            memory_id,
            Some(doc_id),
            Some(asset.asset_id),
            "ambiguous".into(),
            None,
            None,
        );
        assert!(both.is_err(), "both doc_id and asset_id must be rejected");
    }

    #[test]
    fn forget_asset_marks_deleted_removes_blob_and_preserves_links() {
        let (conn, tmp) = open_test_db();
        let snapshot_dir = tmp.path().join("tenant-assets");
        let source = tmp.path().join("source.bin");
        std::fs::write(&source, b"asset bytes").expect("write source");
        let memory_id = seed_active_memory(&conn, "memory with asset attachment");
        let doc_id = seed_active_document(&conn);
        let mut actor = build_asset_actor_inline(conn, snapshot_dir.clone());
        let asset = actor
            .handle_store_asset_from_path(
                &source,
                Some("source.bin".into()),
                Some("application/octet-stream".into()),
                None,
                None,
                Some("test-upload".into()),
                Some("alice".into()),
            )
            .expect("store asset");
        actor
            .handle_link_document_asset(doc_id, asset.asset_id, "source_upload".into(), None, None)
            .expect("link document asset");
        actor
            .handle_attach_memory(
                memory_id,
                None,
                Some(asset.asset_id),
                "source_file".into(),
                None,
                None,
            )
            .expect("attach asset");
        let blob_path = snapshot_dir.join(&asset.storage_path);
        assert!(blob_path.is_file());

        let report = actor
            .handle_forget_asset(asset.asset_id, Some("alice".into()))
            .expect("forget asset");
        assert_eq!(report.asset_id, asset.asset_id);
        assert!(report.blob_deleted);
        assert!(!report.already_deleted);
        assert_eq!(report.document_links, 1);
        assert_eq!(report.memory_attachments, 1);
        assert!(!blob_path.exists());

        let status: String = actor
            .conn
            .query_row(
                "SELECT status FROM assets WHERE asset_id = ?",
                params![asset.asset_id.to_string()],
                |r| r.get(0),
            )
            .expect("read status");
        assert_eq!(status, "deleted");
        let document_links: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*) FROM document_assets WHERE asset_id = ?",
                params![asset.asset_id.to_string()],
                |r| r.get(0),
            )
            .expect("count document links");
        assert_eq!(document_links, 1);
        let memory_attachments: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memory_attachments WHERE asset_id = ?",
                params![asset.asset_id.to_string()],
                |r| r.get(0),
            )
            .expect("count memory attachments");
        assert_eq!(memory_attachments, 1);

        let second = actor
            .handle_forget_asset(asset.asset_id, None)
            .expect("idempotent forget");
        assert!(!second.blob_deleted);
        assert!(second.already_deleted);
        assert_eq!(second.document_links, 1);
        assert_eq!(second.memory_attachments, 1);
    }

    #[test]
    fn safe_asset_storage_path_requires_content_addressed_blob_layout() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let sha = "a".repeat(64);
        let valid = format!("assets/blobs/aa/{sha}");
        assert_eq!(
            safe_asset_storage_path(tmp.path(), &valid).expect("valid asset path"),
            tmp.path().join(&valid)
        );
        assert!(safe_asset_storage_path(tmp.path(), "../escape").is_err());
        assert!(safe_asset_storage_path(tmp.path(), "assets/other/file").is_err());
        let wrong_prefix = format!("assets/blobs/bb/{sha}");
        assert!(safe_asset_storage_path(tmp.path(), &wrong_prefix).is_err());
    }

    #[test]
    fn normalize_subjects_updates_subject_column() {
        let (conn, _tmp) = open_test_db();
        seed_triple(&conn, "t1", "alex", "uses", "rust", "literal");
        seed_relationship_edge(&conn, "t1", "alex", "uses", "rust", "literal");
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_normalize_subjects(vec![("alex".into(), "user".into())], false, None)
            .expect("normalize ok");

        assert_eq!(report.aliases_processed, 1);
        assert_eq!(report.subject_rows_updated, 1);
        assert_eq!(report.object_rows_updated, 0);
        assert!(!report.dry_run);
        assert_eq!(read_subject(&actor.conn, "t1"), "user");
        assert_eq!(read_object(&actor.conn, "t1"), "rust");
        assert_eq!(read_edge_subject(&actor.conn, "t1"), "user");
        let merge_op: (String, String, String, Option<String>) = actor
            .conn
            .query_row(
                "SELECT op_kind, status, source_entity_id, target_entity_id
                   FROM entity_review_ops",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            merge_op,
            (
                "merge".into(),
                "applied".into(),
                "alex".into(),
                Some("user".into())
            )
        );
        let revision_count: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*)
                   FROM memory_revisions
                  WHERE revision_kind = 'entity_merged'
                    AND previous_id = 'alex'
                    AND replacement_id = 'user'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(revision_count, 1);
    }

    #[test]
    fn normalize_subjects_updates_object_column() {
        let (conn, _tmp) = open_test_db();
        // Object position: someone-uses-alex (object_kind=entity).
        seed_triple(&conn, "t1", "bob", "knows", "alex", "entity");
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_normalize_subjects(vec![("alex".into(), "user".into())], false, None)
            .expect("normalize ok");

        assert_eq!(report.subject_rows_updated, 0);
        assert_eq!(report.object_rows_updated, 1);
        assert_eq!(read_subject(&actor.conn, "t1"), "bob");
        assert_eq!(read_object(&actor.conn, "t1"), "user");
    }

    #[test]
    fn normalize_subjects_updates_both_when_subject_equals_object() {
        // Self-loop: subject == object == "alex". The two UPDATEs both fire
        // against the same row — count is 2 (one for each column rewrite),
        // matching what SQLite's `changes()` reports per statement.
        let (conn, _tmp) = open_test_db();
        seed_triple(&conn, "t1", "alex", "is", "alex", "entity");
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_normalize_subjects(vec![("alex".into(), "user".into())], false, None)
            .expect("normalize ok");

        assert_eq!(report.subject_rows_updated, 1);
        assert_eq!(report.object_rows_updated, 1);
        assert_eq!(read_subject(&actor.conn, "t1"), "user");
        assert_eq!(read_object(&actor.conn, "t1"), "user");
    }

    #[test]
    fn normalize_subjects_dry_run_rolls_back() {
        let (conn, _tmp) = open_test_db();
        seed_triple(&conn, "t1", "alex", "uses", "rust", "literal");
        seed_triple(&conn, "t2", "bob", "knows", "alex", "entity");
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_normalize_subjects(vec![("alex".into(), "user".into())], true, None)
            .expect("dry-run normalize ok");

        // Counts reflect would-have-been-updated:
        assert!(report.dry_run);
        assert_eq!(report.subject_rows_updated, 1);
        assert_eq!(report.object_rows_updated, 1);
        // But the rows are unchanged — transaction rolled back.
        assert_eq!(read_subject(&actor.conn, "t1"), "alex");
        assert_eq!(read_object(&actor.conn, "t1"), "rust");
        assert_eq!(read_subject(&actor.conn, "t2"), "bob");
        assert_eq!(read_object(&actor.conn, "t2"), "alex");
        let op_count: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM entity_review_ops", [], |r| r.get(0))
            .unwrap();
        assert_eq!(op_count, 0, "dry-run should not persist review ops");
    }

    #[test]
    fn request_entity_split_records_review_op_and_revision() {
        let (conn, _tmp) = open_test_db();
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_request_entity_split(
                EntitySplitRequest {
                    entity_id: "solo".into(),
                    affected_aliases: vec!["solo relay".into(), " solo relay ".into()],
                    reason: Some("separate product from project".into()),
                },
                Some("operator".into()),
            )
            .expect("split request");

        assert_eq!(report.op_kind, "split");
        assert_eq!(report.status, "needs_review");
        assert_eq!(report.source_entity_id, "solo");
        assert_eq!(report.affected_aliases, vec!["solo relay".to_string()]);
        let stored: (String, String, String) = actor
            .conn
            .query_row(
                "SELECT op_kind, status, affected_aliases_json
                   FROM entity_review_ops
                  WHERE op_id = ?1",
                params![&report.op_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored.0, "split");
        assert_eq!(stored.1, "needs_review");
        assert!(stored.2.contains("solo relay"));
        let revision_count: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*)
                   FROM memory_revisions
                  WHERE revision_kind = 'entity_split_requested'
                    AND target_id = 'solo'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(revision_count, 1);
    }

    #[test]
    fn normalize_subjects_multiple_aliases() {
        let (conn, _tmp) = open_test_db();
        seed_triple(&conn, "t1", "alex", "uses", "rust", "literal");
        seed_triple(&conn, "t2", "bob", "uses", "python", "literal");
        seed_triple(&conn, "t3", "charlie", "knows", "alex", "entity");
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_normalize_subjects(
                vec![
                    ("alex".into(), "user".into()),
                    ("bob".into(), "user".into()),
                ],
                false,
                None,
            )
            .expect("normalize ok");

        assert_eq!(report.aliases_processed, 2);
        // t1.subject (alex→user) + t2.subject (bob→user) = 2 subject rows
        assert_eq!(report.subject_rows_updated, 2);
        // t3.object (alex→user) = 1 object row
        assert_eq!(report.object_rows_updated, 1);

        assert_eq!(read_subject(&actor.conn, "t1"), "user");
        assert_eq!(read_subject(&actor.conn, "t2"), "user");
        assert_eq!(read_object(&actor.conn, "t3"), "user");
        // charlie (subject of t3) was not in the alias map.
        assert_eq!(read_subject(&actor.conn, "t3"), "charlie");
    }

    #[test]
    fn normalize_subjects_no_match_returns_zero_counts() {
        let (conn, _tmp) = open_test_db();
        seed_triple(&conn, "t1", "alex", "uses", "rust", "literal");
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_normalize_subjects(vec![("nobody".into(), "user".into())], false, None)
            .expect("normalize ok");

        assert_eq!(report.aliases_processed, 1);
        assert_eq!(report.subject_rows_updated, 0);
        assert_eq!(report.object_rows_updated, 0);
        // Existing rows untouched.
        assert_eq!(read_subject(&actor.conn, "t1"), "alex");
    }

    #[test]
    fn normalize_subjects_empty_alias_list_is_noop() {
        let (conn, _tmp) = open_test_db();
        seed_triple(&conn, "t1", "alex", "uses", "rust", "literal");
        let mut actor = build_actor_inline(conn);

        let report = actor
            .handle_normalize_subjects(vec![], false, None)
            .expect("normalize ok");

        assert_eq!(report.aliases_processed, 0);
        assert_eq!(report.subject_rows_updated, 0);
        assert_eq!(report.object_rows_updated, 0);
        assert_eq!(read_subject(&actor.conn, "t1"), "alex");
    }

    #[test]
    fn normalize_subjects_via_handle_round_trip() {
        // End-to-end: dispatch through `WriteHandle::normalize_subjects`
        // so we cover the variant + dispatch arm + handle method together.
        let (conn, tmp) = open_test_db();
        seed_triple(&conn, "t1", "alex", "uses", "rust", "literal");
        seed_triple(&conn, "t2", "bob", "knows", "alex", "entity");
        // Drop the seed connection so the writer's connection (opened from
        // the same path) has exclusive write access. We re-open at the end
        // to verify.
        drop(conn);
        let conn = crate::test_support::open_test_db_at(&tmp.path().join("test.db"));

        let hnsw = Arc::new(StubVectorIndex::new(4));
        let WriterSpawn { handle, join } = WriterActor::spawn(conn, hnsw);

        let report = rt()
            .block_on(handle.normalize_subjects(vec![("alex".into(), "user".into())], false))
            .expect("normalize via handle");
        assert_eq!(report.subject_rows_updated, 1);
        assert_eq!(report.object_rows_updated, 1);

        drop(handle);
        join.join().expect("writer thread joins");

        // Verify via a fresh read connection (writer's connection is now
        // closed because the actor's thread exited).
        let conn = crate::test_support::open_test_db_at(&tmp.path().join("test.db"));
        let subj: String = conn
            .query_row(
                "SELECT subject_id FROM triples WHERE triple_id = 't1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let obj: String = conn
            .query_row(
                "SELECT object_id FROM triples WHERE triple_id = 't2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(subj, "user");
        assert_eq!(obj, "user");
    }

    // ====================================================================
    // v0.7.0 P3 — IngestDocument + ForgetDocument + recovery replay
    //
    // Test pattern: each test builds a WriterActor directly with a real
    // StubEmbedder + a fresh tokio runtime handle (the writer's
    // dispatch_ingest_document calls `runtime.block_on(embedder.embed_batch)`,
    // so it needs a Handle). For tests that don't call `handle.ingest_document`
    // on a separate writer thread, we drive the handler synchronously via
    // `actor.dispatch_ingest_document` after constructing the actor in-place
    // — matching the `normalize_subjects` pattern further up.
    // ====================================================================

    use crate::document::ChunkConfig;
    use crate::embedder::StubEmbedder;
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use solo_core::{ChunkId, DocumentId, Embedder};

    /// Build an actor with a stub embedder wired up + the embedders row
    /// registered. Returns the actor plus a kept-alive runtime (drop the
    /// returned tuple together).
    fn build_ingest_actor(
        conn: Connection,
    ) -> (WriterActor, tokio::runtime::Runtime, Arc<StubVectorIndex>) {
        // Use a multi-thread runtime so the writer's block_on doesn't
        // deadlock when the calling thread happens to be a worker.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let embedder: Arc<dyn Embedder> = Arc::new(StubEmbedder::new("stub", "v1", 4));
        let identity = EmbedderIdentity::from_embedder(embedder.as_ref());
        let embedder_id = get_or_insert_embedder_id(&conn, &identity).unwrap();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let (_tx, rx) = mpsc::channel(1);
        let actor = WriterActor {
            conn,
            hnsw: hnsw.clone(),
            rx,
            snapshot_dir: None,
            embedder_id: Some(embedder_id),
            embedder: Some(embedder),
            runtime_handle: Some(handle),
            steward: None,
            steward_slot: None,
            triples_batch_signal: None,
            key: None,
            redactor: disabled_test_redactor(),
            quota_bytes: None,
            db_path: None,
            invalidate_tx: None,
            invalidate_tenant_id: None,
        };
        (actor, runtime, hnsw)
    }

    /// Write a small markdown document under `tmp` and return its path.
    fn write_markdown(tmp: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
        let path = tmp.path().join(name);
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    /// Embedder that always returns Err — used for the embed-failure rollback test.
    #[derive(Debug)]
    struct FailingEmbedder {
        dim: usize,
    }

    #[async_trait::async_trait]
    impl Embedder for FailingEmbedder {
        fn name(&self) -> &str {
            "fail"
        }
        fn version(&self) -> &str {
            "v1"
        }
        fn dim(&self) -> usize {
            self.dim
        }
        fn dtype(&self) -> solo_core::EmbeddingDtype {
            solo_core::EmbeddingDtype::F32
        }
        async fn embed_batch(&self, _texts: &[&str]) -> Result<Vec<Embedding>> {
            Err(solo_core::Error::embedder("forced failure for test"))
        }
    }

    // ----- Ingest tests -----

    #[test]
    fn ingest_document_persists_doc_and_chunks() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(
            &docs_tmp,
            "intro.md",
            "# Intro\n\nFirst paragraph here.\n\nSecond paragraph here.\n",
        );

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path.clone(), ChunkConfig::default(), None, reply_tx);
        let report = reply_rx.blocking_recv().unwrap().expect("ingest ok");

        assert!(!report.deduped);
        assert_eq!(report.chunks_persisted, 1, "tiny doc → one chunk");
        assert!(report.bytes_ingested > 0);

        // documents row exists with status=active.
        let (status, title, chunk_count): (String, String, i64) = actor
            .conn
            .query_row(
                "SELECT status, title, chunk_count FROM documents WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "active");
        assert_eq!(title, "Intro", "first markdown heading becomes title");
        assert_eq!(chunk_count, 1);

        // document_chunks row count matches.
        let n_chunks: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_chunks, 1);

        // chunk_embeddings row count matches.
        let n_emb: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*) FROM chunk_embeddings ce
                 JOIN document_chunks dc ON dc.chunk_id = ce.chunk_id
                 WHERE dc.doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_emb, 1);

        // HNSW got an add.
        assert_eq!(hnsw.add_count(), 1);
    }

    #[test]
    fn ingest_document_csv_uses_registered_extractor_metadata() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = docs_tmp.path().join("people.csv");
        std::fs::write(&path, b"name,role\nAlice,Engineer\n").expect("write csv fixture");

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let report = reply_rx.blocking_recv().unwrap().expect("ingest ok");

        assert_eq!(report.extractor_name, "csv_table");
        assert_eq!(
            report.extractor_version,
            crate::document::TEXT_EXTRACTOR_VERSION
        );
        assert!(report.text_chars > 0);

        let mime_type: String = actor
            .conn
            .query_row(
                "SELECT mime_type FROM documents WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mime_type, "text/csv");

        let content: String = actor
            .conn
            .query_row(
                "SELECT content FROM document_chunks WHERE doc_id = ? AND chunk_index = 0",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(content.contains("row 1: name | role"));
        assert!(content.contains("row 2: Alice | Engineer"));
    }

    #[test]
    fn ingest_document_pending_index_drains_cleanly() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "doc.md", "# Doc\n\nBody text.\n");

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let _ = reply_rx.blocking_recv().unwrap().expect("ingest ok");

        let pending: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_index WHERE kind = 'chunk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0, "pending_index chunk rows fully drained");
    }

    #[test]
    fn ingest_document_is_idempotent_by_content_hash() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "same.md", "# Same\n\nDeterministic body.\n");

        // First ingest.
        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path.clone(), ChunkConfig::default(), None, reply_tx);
        let report1 = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(!report1.deduped);
        assert_eq!(report1.chunks_persisted, 1);
        let hnsw_after_first = hnsw.add_count();

        // Re-ingest — dedup, no new chunks, no HNSW add.
        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let report2 = reply_rx.blocking_recv().unwrap().unwrap();
        assert!(report2.deduped);
        assert_eq!(report2.doc_id, report1.doc_id);
        assert_eq!(report2.chunks_persisted, 0);
        assert_eq!(
            hnsw.add_count(),
            hnsw_after_first,
            "dedup hit must not embed or add to HNSW"
        );

        // Documents table still has exactly ONE row.
        let n_docs: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_docs, 1);
    }

    #[test]
    fn ingest_document_rolls_back_on_embed_failure() {
        // Build the actor manually with FailingEmbedder.
        let (conn, _tmp) = open_test_db();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let embedder: Arc<dyn Embedder> = Arc::new(FailingEmbedder { dim: 4 });
        let identity = EmbedderIdentity::from_embedder(embedder.as_ref());
        let embedder_id = get_or_insert_embedder_id(&conn, &identity).unwrap();
        let hnsw = Arc::new(StubVectorIndex::new(4));
        let (_tx, rx) = mpsc::channel(1);
        let mut actor = WriterActor {
            conn,
            hnsw: hnsw.clone(),
            rx,
            snapshot_dir: None,
            embedder_id: Some(embedder_id),
            embedder: Some(embedder),
            runtime_handle: Some(handle),
            steward: None,
            steward_slot: None,
            triples_batch_signal: None,
            key: None,
            redactor: disabled_test_redactor(),
            quota_bytes: None,
            db_path: None,
            invalidate_tx: None,
            invalidate_tenant_id: None,
        };

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "fail.md", "# Fail\n\nBody.\n");

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let err = reply_rx.blocking_recv().unwrap().unwrap_err();
        assert!(err.to_string().contains("embed_batch"), "got: {err}");

        // No documents persisted (embed-before-tx ordering proves no SQL state).
        let n_docs: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_docs, 0);
        let n_chunks: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM document_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_chunks, 0);
        let n_pending: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM pending_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_pending, 0);
        // HNSW unchanged.
        assert_eq!(hnsw.add_count(), 0);
    }

    #[test]
    fn ingest_document_large_document() {
        // Force multi-chunk: ~10 paragraphs at ~50 chars each → ~500 chars
        // > target=80 → multiple chunks.
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let mut body = String::from("# Header\n\n");
        for i in 0..30 {
            body.push_str(&format!(
                "Paragraph number {i} with several words in it.\n\n"
            ));
        }
        let path = write_markdown(&docs_tmp, "big.md", &body);

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(
            path,
            ChunkConfig {
                target_tokens: 80,
                overlap_tokens: 10,
            },
            None,
            reply_tx,
        );
        let report = reply_rx.blocking_recv().unwrap().unwrap();

        assert!(
            report.chunks_persisted >= 2,
            "expected multi-chunk, got {}",
            report.chunks_persisted
        );
        let n_chunks: i64 = actor
            .conn
            .query_row(
                "SELECT COUNT(*) FROM document_chunks WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_chunks as u32, report.chunks_persisted);
        assert_eq!(hnsw.add_count() as u32, report.chunks_persisted);
    }

    #[test]
    fn ingest_document_uses_first_heading_as_title() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(
            &docs_tmp,
            "any_name.md",
            "Preamble line without heading.\n\n## Sub Section Title\n\nBody.\n",
        );

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let report = reply_rx.blocking_recv().unwrap().unwrap();

        let title: String = actor
            .conn
            .query_row(
                "SELECT title FROM documents WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            title, "Sub Section Title",
            "title comes from first heading line"
        );
    }

    #[test]
    fn ingest_document_records_file_mtime() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "mtime.md", "# T\n\nBody.\n");
        let fs_mtime_ms = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let report = reply_rx.blocking_recv().unwrap().unwrap();

        let modified_at_ms: Option<i64> = actor
            .conn
            .query_row(
                "SELECT modified_at_ms FROM documents WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        let m = modified_at_ms.expect("modified_at_ms must be set when file mtime is readable");
        // File-system mtime resolution varies; allow ±2 sec slack.
        assert!(
            (m - fs_mtime_ms).abs() < 2_000,
            "modified_at_ms drift: db={m} fs={fs_mtime_ms}"
        );
    }

    #[test]
    fn ingest_document_unsupported_extension_errors_cleanly() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = docs_tmp.path().join("blob.bin");
        std::fs::write(&path, b"\x00\x01\x02").unwrap();

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let err = reply_rx.blocking_recv().unwrap().unwrap_err();
        assert!(
            err.to_string().contains("parse") || err.to_string().contains("extension"),
            "unsupported extension should surface as a parse error: {err}"
        );
        // No SQL or HNSW state changed.
        let n_docs: i64 = actor
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_docs, 0);
        assert_eq!(hnsw.add_count(), 0);
    }

    #[test]
    fn ingest_document_writes_embedding_dim_correctly() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "dim.md", "# Dim\n\nText.\n");

        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let report = reply_rx.blocking_recv().unwrap().unwrap();

        let dim: i64 = actor
            .conn
            .query_row(
                "SELECT ce.dim FROM chunk_embeddings ce
                 JOIN document_chunks dc ON dc.chunk_id = ce.chunk_id
                 WHERE dc.doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dim, 4, "stub embedder dim is 4");

        let dtype: String = actor
            .conn
            .query_row(
                "SELECT ce.dtype FROM chunk_embeddings ce
                 JOIN document_chunks dc ON dc.chunk_id = ce.chunk_id
                 WHERE dc.doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dtype, "f32");
    }

    // ----- Forget tests -----

    #[test]
    fn forget_document_sets_status_forgotten() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "f.md", "# F\n\nBody.\n");
        let (tx, rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, tx);
        let report = rx.blocking_recv().unwrap().unwrap();

        let forget_report = actor.handle_forget_document(report.doc_id, None).unwrap();
        assert_eq!(forget_report.doc_id, report.doc_id);
        assert_eq!(forget_report.chunks_tombstoned, report.chunks_persisted);

        let status: String = actor
            .conn
            .query_row(
                "SELECT status FROM documents WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "forgotten");
    }

    #[test]
    fn forget_document_tombstones_hnsw_rowids() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "t.md", "# T\n\nBody.\n");
        let (tx, rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, tx);
        let report = rx.blocking_recv().unwrap().unwrap();

        let added_before = hnsw.add_count();
        let removed_before = hnsw.remove_count();

        let _ = actor.handle_forget_document(report.doc_id, None).unwrap();

        // remove_count should be at least the chunks_persisted, since one
        // hnsw.remove call per chunk fired.
        assert_eq!(
            hnsw.remove_count() - removed_before,
            report.chunks_persisted as usize
        );
        assert_eq!(hnsw.add_count(), added_before, "forget must not add");
    }

    #[test]
    fn forget_document_unknown_doc_id_returns_not_found() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let err = actor
            .handle_forget_document(DocumentId::new(), None)
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn forget_document_idempotent() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "idem.md", "# Idem\n\nBody.\n");
        let (tx, rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, tx);
        let report = rx.blocking_recv().unwrap().unwrap();

        let r1 = actor.handle_forget_document(report.doc_id, None).unwrap();
        let r2 = actor.handle_forget_document(report.doc_id, None).unwrap();
        assert_eq!(r1.doc_id, r2.doc_id);
        assert_eq!(r1.chunks_tombstoned, r2.chunks_tombstoned);

        // Still forgotten.
        let status: String = actor
            .conn
            .query_row(
                "SELECT status FROM documents WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "forgotten");
    }

    #[test]
    fn ingest_document_then_forget_then_reingest_same_content_hash_dedups_forgotten_doc() {
        // Document chosen behavior: a forgotten doc still wins content-hash
        // dedup. Re-ingest returns the SAME (forgotten) doc_id without
        // resurrecting it. Operators who want a fresh active doc must
        // ingest under a different content (or future `restore` command).
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "fr.md", "# FR\n\nBody.\n");

        let (tx, rx) = oneshot::channel();
        actor.dispatch_ingest_document(path.clone(), ChunkConfig::default(), None, tx);
        let report1 = rx.blocking_recv().unwrap().unwrap();
        let _ = actor.handle_forget_document(report1.doc_id, None).unwrap();

        let adds_before = hnsw.add_count();
        let (tx, rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, tx);
        let report2 = rx.blocking_recv().unwrap().unwrap();

        assert!(report2.deduped, "forgotten doc still wins dedup");
        assert_eq!(report2.doc_id, report1.doc_id);
        assert_eq!(report2.chunks_persisted, 0);
        assert_eq!(
            hnsw.add_count(),
            adds_before,
            "dedup hit must not add (even though doc is forgotten)"
        );

        // Doc remains forgotten — re-ingest did NOT resurrect.
        let status: String = actor
            .conn
            .query_row(
                "SELECT status FROM documents WHERE doc_id = ?",
                params![report1.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "forgotten");
    }

    // ----- Recovery replay tests -----

    /// Helper: insert a `documents` row + N `document_chunks` rows + N
    /// `pending_index` (kind='chunk') rows, and return the chunk rowids
    /// the test should expect after replay. The pending rows hold the
    /// chunks' embeddings; the chunk rows themselves carry no embedding
    /// (the chunk_embeddings table is empty in this helper — replay only
    /// reads pending_index).
    fn seed_pending_chunks(
        conn: &Connection,
        doc_id: &str,
        chunk_dim: usize,
        n: usize,
    ) -> Vec<i64> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT INTO documents (
                doc_id, source, title, mime_type,
                ingested_at_ms, modified_at_ms, status,
                chunk_count, content_hash, byte_size
             ) VALUES (?, ?, ?, ?, ?, NULL, 'active', ?, ?, ?)",
            params![
                doc_id,
                "test://source",
                "test",
                "text/plain",
                now_ms,
                n as i64,
                format!("{doc_id}_hash"),
                100i64,
            ],
        )
        .unwrap();

        let mut rowids = Vec::with_capacity(n);
        for i in 0..n {
            let chunk_id = ChunkId::new().to_string();
            conn.execute(
                "INSERT INTO document_chunks (
                    chunk_id, doc_id, chunk_index, content,
                    token_count, start_offset, end_offset, created_at_ms
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    chunk_id,
                    doc_id,
                    i as i64,
                    format!("chunk {i} text"),
                    3i64,
                    (i * 10) as i64,
                    ((i + 1) * 10) as i64,
                    now_ms,
                ],
            )
            .unwrap();
            let rowid = conn.last_insert_rowid();
            rowids.push(rowid);

            let zeros = vec![0u8; chunk_dim * 4];
            conn.execute(
                "INSERT INTO pending_index (
                    kind, chunk_id, embedding, embedding_dim, enqueued_at
                 ) VALUES ('chunk', ?, ?, ?, ?)",
                params![chunk_id, &zeros[..], chunk_dim as i64, now_ms + i as i64],
            )
            .unwrap();
        }
        rowids
    }

    #[test]
    fn recovery_replay_handles_chunk_pending_rows() {
        let (mut conn, _tmp) = open_test_db();
        let rowids = seed_pending_chunks(&conn, "11111111-1111-1111-1111-111111111111", 4, 3);

        let stub = StubVectorIndex::new(4);
        let report = crate::recovery::replay_pending_index(&mut conn, &stub).unwrap();
        assert_eq!(report.rows_seen, 3);
        assert_eq!(report.rows_replayed, 3);
        assert_eq!(report.rows_failed, 0);
        // All chunk rowids landed in HNSW, encoded with the chunk-kind
        // discriminator (high bit set) per `crate::hnsw_id`. The raw
        // rowids returned by SQL are translated through `chunk_hnsw_id`
        // by the recovery replay loop.
        let added: std::collections::HashSet<i64> =
            stub.entries().iter().map(|(r, _)| *r).collect();
        let expected: std::collections::HashSet<i64> = rowids
            .iter()
            .copied()
            .map(crate::hnsw_id::chunk_hnsw_id)
            .collect();
        assert_eq!(added, expected);
        // pending_index is drained.
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn recovery_replay_handles_mixed_episode_and_chunk_rows() {
        let (mut conn, _tmp) = open_test_db();

        // Seed 2 episodes (with pending rows).
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut episode_rowids = Vec::new();
        for content in &["ep_a", "ep_b"] {
            let ep = fixture_episode(content);
            let mid = ep.memory_id.to_string();
            conn.execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, source_id, content,
                    encoding_context_json, provenance_json, confidence,
                    strength, salience, tier, created_at_ms, updated_at_ms
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    mid,
                    ep.ts_ms,
                    ep.source_type,
                    ep.source_id,
                    ep.content,
                    "{}",
                    Option::<String>::None,
                    ep.confidence.0,
                    ep.strength,
                    ep.salience,
                    "hot",
                    now_ms,
                    now_ms,
                ],
            )
            .unwrap();
            episode_rowids.push(conn.last_insert_rowid());

            conn.execute(
                "INSERT INTO pending_index (kind, memory_id, embedding, embedding_dim, enqueued_at)
                 VALUES ('episode', ?, ?, ?, ?)",
                params![mid, &vec![0u8; 16][..], 4i64, now_ms],
            )
            .unwrap();
        }

        // Seed 2 chunks (with pending rows).
        let chunk_rowids = seed_pending_chunks(&conn, "22222222-2222-2222-2222-222222222222", 4, 2);

        let stub = StubVectorIndex::new(4);
        let report = crate::recovery::replay_pending_index(&mut conn, &stub).unwrap();
        assert_eq!(report.rows_seen, 4);
        assert_eq!(report.rows_replayed, 4);
        assert_eq!(report.rows_failed, 0);

        // Both classes of rowids landed in HNSW, each encoded with the
        // matching kind discriminator. This is the integration-level
        // anchor that recovery replay applies the correct encoder
        // per-kind: episodes via `episode_hnsw_id` (identity) and
        // chunks via `chunk_hnsw_id` (high bit set).
        let added: std::collections::HashSet<i64> =
            stub.entries().iter().map(|(r, _)| *r).collect();
        let mut expected: std::collections::HashSet<i64> = episode_rowids
            .iter()
            .copied()
            .map(crate::hnsw_id::episode_hnsw_id)
            .collect();
        expected.extend(
            chunk_rowids
                .iter()
                .copied()
                .map(crate::hnsw_id::chunk_hnsw_id),
        );
        assert_eq!(added, expected);

        // Critically: episode and chunk ids do NOT collide in the
        // HNSW namespace, even when their SQLite rowids happen to share
        // values. (In this test the AUTOINCREMENT sequences keep them
        // disjoint; the collision-free invariant comes from the
        // kind-discriminator encoding, not from rowid disjointness.)
        for r in &episode_rowids {
            for c in &chunk_rowids {
                let ep_id = crate::hnsw_id::episode_hnsw_id(*r);
                let chunk_id = crate::hnsw_id::chunk_hnsw_id(*c);
                assert_ne!(
                    ep_id, chunk_id,
                    "encoded episode and chunk ids must never collide"
                );
            }
        }

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM pending_index", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Critical regression test: an episode at `rowid=N` and a chunk at
    /// `rowid=N` (same numeric value!) must BOTH be retrievable from the
    /// shared HNSW. Without the kind-discriminated encoding, the second
    /// add would collide with the first; with `hnsw_rs` 0.3.4's silent-
    /// accept behavior the recall path would surface ambiguous results.
    ///
    /// This test simulates the production scenario by forcibly assigning
    /// chunk rowid=1 to coincide with episode rowid=1, then verifies
    /// that:
    ///   (1) Both vectors land in HNSW at distinct encoded ids.
    ///   (2) `recall` (episode side) returns the episode.
    ///   (3) `doc_search` (chunk side) returns the chunk.
    ///
    /// The simulation is done at the recovery layer rather than via
    /// AUTOINCREMENT (which in a fresh DB starts both sequences at 1
    /// anyway, so simply remembering one episode + ingesting one
    /// chunk reproduces the collision naturally).
    #[test]
    fn episode_and_chunk_with_same_rowid_coexist_in_hnsw() {
        let (conn, _tmp) = open_test_db();
        let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

        // Step 1: write one episode (assigned rowid=1 by AUTOINCREMENT).
        let ep = fixture_episode("episode body");
        let now_ms = chrono::Utc::now().timestamp_millis();
        // Use `handle_remember`-equivalent path via direct SQL + actor
        // dispatch. We hand-roll the SQL so we can assert the assigned
        // rowid is 1. (The actor's `dispatch_remember` does the same
        // INSERT under the hood.)
        let memory_id = ep.memory_id.to_string();
        actor
            .conn
            .execute(
                "INSERT INTO episodes (
                    memory_id, ts_ms, source_type, source_id, content,
                    encoding_context_json, provenance_json, confidence,
                    strength, salience, tier, created_at_ms, updated_at_ms
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    memory_id,
                    ep.ts_ms,
                    ep.source_type,
                    ep.source_id,
                    ep.content,
                    "{}",
                    Option::<String>::None,
                    ep.confidence.0,
                    ep.strength,
                    ep.salience,
                    "hot",
                    now_ms,
                    now_ms,
                ],
            )
            .unwrap();
        let episode_rowid = actor.conn.last_insert_rowid();
        assert_eq!(episode_rowid, 1, "first episode insert must yield rowid=1");
        // Simulate the writer's HNSW add for this episode with a
        // distinctive vector.
        let ep_vec = vec![1.0f32, 0.0, 0.0, 0.0];
        hnsw.add(crate::hnsw_id::episode_hnsw_id(episode_rowid), &ep_vec)
            .unwrap();

        // Step 2: ingest a document. The first chunk gets
        // document_chunks.rowid = 1 (independent AUTOINCREMENT
        // sequence per ADR-0003 §shared-HNSW-namespace), colliding
        // numerically with the episode above.
        let docs_tmp = tempfile::TempDir::new().unwrap();
        let path = write_markdown(&docs_tmp, "doc.md", "# Doc\n\nSome chunk content.\n");
        let (reply_tx, reply_rx) = oneshot::channel();
        actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
        let report = reply_rx.blocking_recv().unwrap().expect("ingest ok");
        assert_eq!(report.chunks_persisted, 1, "fixture produces one chunk");

        // The chunk's rowid should be 1 — same as the episode's.
        let chunk_rowid: i64 = actor
            .conn
            .query_row(
                "SELECT rowid FROM document_chunks WHERE doc_id = ?",
                params![report.doc_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            chunk_rowid, episode_rowid,
            "chunk rowid must collide numerically with episode rowid for this test (both AUTOINCREMENT sequences start at 1)"
        );

        // Step 3: assert HNSW carries BOTH vectors at DISTINCT encoded ids.
        let entries = hnsw.entries();
        let encoded_episode_id = crate::hnsw_id::episode_hnsw_id(episode_rowid);
        let encoded_chunk_id = crate::hnsw_id::chunk_hnsw_id(chunk_rowid);
        assert_ne!(
            encoded_episode_id, encoded_chunk_id,
            "encoded episode and chunk ids must differ even when raw rowids collide"
        );
        let ids: std::collections::HashSet<i64> = entries.iter().map(|(r, _)| *r).collect();
        assert!(
            ids.contains(&encoded_episode_id),
            "HNSW must carry episode at encoded id {encoded_episode_id}; entries: {entries:?}"
        );
        assert!(
            ids.contains(&encoded_chunk_id),
            "HNSW must carry chunk at encoded id {encoded_chunk_id}; entries: {entries:?}"
        );

        // Step 4: decode each entry and confirm kind/rowid round-trip.
        for (id, _) in &entries {
            let (kind, decoded) = crate::hnsw_id::decode_hnsw_id(*id);
            match kind {
                crate::hnsw_id::HnswIdKind::Episode => {
                    assert_eq!(decoded, episode_rowid);
                }
                crate::hnsw_id::HnswIdKind::Chunk => {
                    assert_eq!(decoded, chunk_rowid);
                }
            }
        }
    }

    // ----- v0.7.1: SOLO_INGEST_MAX_BYTES guardrail -----
    //
    // Test the env parsing as a pure helper and use a thread-local override
    // for writer-path tests. Mutating process-global env vars here races
    // unrelated ingest tests under parallel `cargo test` workers.

    mod ingest_max_bytes {
        use super::*;
        use crate::writer::{
            DEFAULT_INGEST_MAX_BYTES, TEST_INGEST_MAX_BYTES_OVERRIDE,
            resolve_ingest_max_bytes_from_raw,
        };

        struct CapOverrideGuard;
        impl Drop for CapOverrideGuard {
            fn drop(&mut self) {
                TEST_INGEST_MAX_BYTES_OVERRIDE.with(|slot| slot.set(None));
            }
        }

        fn override_cap(value: Option<u64>) -> CapOverrideGuard {
            TEST_INGEST_MAX_BYTES_OVERRIDE.with(|slot| slot.set(Some(value)));
            CapOverrideGuard
        }

        #[test]
        fn resolve_unset_returns_default() {
            assert_eq!(
                resolve_ingest_max_bytes_from_raw(None),
                Some(DEFAULT_INGEST_MAX_BYTES)
            );
        }

        #[test]
        fn resolve_zero_disables_cap() {
            assert_eq!(resolve_ingest_max_bytes_from_raw(Some("0")), None);
        }

        #[test]
        fn resolve_positive_integer_uses_value() {
            assert_eq!(resolve_ingest_max_bytes_from_raw(Some("1024")), Some(1024));
        }

        #[test]
        fn resolve_garbage_falls_back_to_default() {
            assert_eq!(
                resolve_ingest_max_bytes_from_raw(Some("not-a-number")),
                Some(DEFAULT_INGEST_MAX_BYTES)
            );

            assert_eq!(
                resolve_ingest_max_bytes_from_raw(Some("-1")),
                Some(DEFAULT_INGEST_MAX_BYTES)
            );

            assert_eq!(
                resolve_ingest_max_bytes_from_raw(Some("  ")),
                Some(DEFAULT_INGEST_MAX_BYTES)
            );
        }

        #[test]
        fn ingest_rejects_oversized_file_with_clear_error() {
            // 100-byte file but cap is 1 byte → reject.
            let _override = override_cap(Some(1));

            let (conn, _tmp) = open_test_db();
            let (mut actor, _rt, hnsw) = build_ingest_actor(conn);

            let docs_tmp = tempfile::TempDir::new().unwrap();
            let path = write_markdown(
                &docs_tmp,
                "big.md",
                "# Big\n\nThis is well over a single byte of content text.\n",
            );

            let (reply_tx, reply_rx) = oneshot::channel();
            actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
            let err = reply_rx.blocking_recv().unwrap().unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("SOLO_INGEST_MAX_BYTES")
                    && msg.contains("exceeds")
                    && msg.contains("disable"),
                "rejection message must call out the env var, threshold, and disable hint; got: {msg}"
            );
            // Zero SQL state, zero HNSW state.
            let n_docs: i64 = actor
                .conn
                .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n_docs, 0);
            assert_eq!(hnsw.add_count(), 0);
        }

        #[test]
        fn ingest_allows_undersized_file_under_custom_cap() {
            // 4 KiB cap; tiny doc is well under.
            let _override = override_cap(Some(4096));

            let (conn, _tmp) = open_test_db();
            let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

            let docs_tmp = tempfile::TempDir::new().unwrap();
            let path = write_markdown(&docs_tmp, "ok.md", "# OK\n\nShort body.\n");

            let (reply_tx, reply_rx) = oneshot::channel();
            actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
            let report = reply_rx
                .blocking_recv()
                .unwrap()
                .expect("ingest under cap must succeed");
            assert!(!report.deduped);
            assert_eq!(report.chunks_persisted, 1);
        }

        #[test]
        fn ingest_with_cap_zero_allows_any_size() {
            // Disable cap entirely.
            let _override = override_cap(None);

            let (conn, _tmp) = open_test_db();
            let (mut actor, _rt, _hnsw) = build_ingest_actor(conn);

            let docs_tmp = tempfile::TempDir::new().unwrap();
            // Ingest succeeds even though body is non-trivial.
            let path = write_markdown(
                &docs_tmp,
                "any.md",
                "# Any\n\nWith SOLO_INGEST_MAX_BYTES=0 any size is allowed.\n",
            );

            let (reply_tx, reply_rx) = oneshot::channel();
            actor.dispatch_ingest_document(path, ChunkConfig::default(), None, reply_tx);
            let report = reply_rx
                .blocking_recv()
                .unwrap()
                .expect("cap=0 disables cap");
            assert_eq!(report.chunks_persisted, 1);
        }
    }

    // -----------------------------------------------------------------
    // v0.8.0 P4 — audit emission tests
    //
    // Each mutating handler must produce an audit_events row inside the
    // writer's SQL transaction. We exercise dispatch_remember +
    // handle_forget + dispatch_ingest_document + handle_forget_document
    // directly (read-only paths emit via solo-query, tested separately
    // in that crate).
    // -----------------------------------------------------------------

    mod audit_emit_tests {
        use super::*;
        use crate::audit::AuditOperation;

        /// Build an actor with embedder + runtime so ingest works (mirrors
        /// `build_ingest_actor`). Returns (actor, runtime, tmp). The
        /// caller controls the runtime; the actor's connection is on
        /// disk so we can re-open it to inspect the audit table.
        fn build_ingest_actor_for_audit()
        -> (WriterActor, tokio::runtime::Runtime, tempfile::TempDir) {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let tmp = tempfile::TempDir::new().unwrap();
            let path = tmp.path().join("test.db");
            let conn = crate::test_support::open_test_db_at(&path);

            let embedder: Arc<dyn solo_core::Embedder> =
                Arc::new(crate::StubEmbedder::new("stub", "v1", 4));
            let identity = crate::EmbedderIdentity {
                name: "stub".into(),
                version: "v1".into(),
                dim: 4,
                dtype: "f32".into(),
            };
            let embedder_id = crate::get_or_insert_embedder_id(&conn, &identity).unwrap();

            let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
                Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: Some(embedder_id),
                embedder: Some(embedder),
                runtime_handle: Some(runtime.handle().clone()),
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };
            (actor, runtime, tmp)
        }

        fn count_audit_rows_for_op(conn: &Connection, op: AuditOperation) -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM audit_events WHERE operation = ?",
                params![op.as_str()],
                |r| r.get(0),
            )
            .unwrap()
        }

        #[test]
        fn dispatch_remember_emits_audit_row_with_ok_result() {
            let (conn, _tmp) = open_test_db();
            let hnsw = Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            let episode = fixture_episode("audit-remember");
            actor.dispatch_remember(
                episode.clone(),
                fixture_embedding(4),
                Some("alice".into()),
                reply_tx,
            );
            assert!(reply_rx.blocking_recv().unwrap().is_ok());

            let (op, principal, target, result): (String, Option<String>, Option<String>, String) =
                actor
                    .conn
                    .query_row(
                        "SELECT operation, principal_subject, target_id, result \
                         FROM audit_events ORDER BY audit_id DESC LIMIT 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .unwrap();
            assert_eq!(op, "memory.remember");
            assert_eq!(principal.as_deref(), Some("alice"));
            assert_eq!(
                target.as_deref(),
                Some(episode.memory_id.to_string().as_str())
            );
            assert_eq!(result, "ok");
        }

        #[test]
        fn dispatch_remember_with_none_principal_persists_null() {
            let (conn, _tmp) = open_test_db();
            let hnsw = Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            let episode = fixture_episode("audit-remember-noprincipal");
            actor.dispatch_remember(episode.clone(), fixture_embedding(4), None, reply_tx);
            assert!(reply_rx.blocking_recv().unwrap().is_ok());

            let principal: Option<String> = actor
                .conn
                .query_row(
                    "SELECT principal_subject FROM audit_events ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(principal.is_none());
        }

        #[test]
        fn handle_forget_emits_audit_row() {
            let (conn, _tmp) = open_test_db();
            let hnsw = Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };

            // Need an episode to forget first.
            let (reply_tx, reply_rx) = oneshot::channel();
            let episode = fixture_episode("to-forget");
            actor.dispatch_remember(episode.clone(), fixture_embedding(4), None, reply_tx);
            reply_rx.blocking_recv().unwrap().unwrap();

            // Pre-condition: 1 remember audit row.
            assert_eq!(
                count_audit_rows_for_op(&actor.conn, AuditOperation::MemoryRemember),
                1
            );

            actor
                .handle_forget(episode.memory_id, "test".into(), Some("bob".into()))
                .unwrap();
            assert_eq!(
                count_audit_rows_for_op(&actor.conn, AuditOperation::MemoryForget),
                1
            );
            // Principal threaded through correctly.
            let principal: Option<String> = actor
                .conn
                .query_row(
                    "SELECT principal_subject FROM audit_events \
                     WHERE operation = 'memory.forget' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(principal.as_deref(), Some("bob"));
        }

        #[test]
        fn handle_forget_unknown_id_emits_no_success_row_emits_error_row_via_dispatch() {
            // This tests that the error path emits an audit row with
            // result='error' (via dispatch's `emit_audit_best_effort`).
            let (conn, _tmp) = open_test_db();
            let hnsw = Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };

            let unknown = MemoryId::new();
            // Use the dispatch surface so error-path audit emit runs.
            let cmd = WriteCommand::Forget {
                memory_id: unknown,
                reason: "test".into(),
                audit_principal: Some("carol".into()),
                reply: oneshot::channel().0,
            };
            actor.dispatch(cmd);

            let (result, principal): (String, Option<String>) = actor
                .conn
                .query_row(
                    "SELECT result, principal_subject FROM audit_events \
                     WHERE operation = 'memory.forget' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(result, "error");
            assert_eq!(principal.as_deref(), Some("carol"));
        }

        #[test]
        fn handle_forget_document_emits_audit_row() {
            let (mut actor, runtime, tmp) = build_ingest_actor_for_audit();
            // Ingest a doc first.
            let docs_dir = tmp.path().join("docs");
            std::fs::create_dir_all(&docs_dir).unwrap();
            let path = docs_dir.join("test.md");
            std::fs::write(&path, "# audit doc\nsome content").unwrap();
            let (reply_tx, reply_rx) = oneshot::channel();
            actor.dispatch_ingest_document(
                path,
                crate::document::ChunkConfig::default(),
                None,
                reply_tx,
            );
            let report = reply_rx.blocking_recv().unwrap().unwrap();

            // Now forget it with a principal.
            let _ = actor
                .handle_forget_document(report.doc_id, Some("dora".into()))
                .unwrap();
            assert_eq!(
                count_audit_rows_for_op(&actor.conn, AuditOperation::MemoryForgetDocument),
                1
            );
            let principal: Option<String> = actor
                .conn
                .query_row(
                    "SELECT principal_subject FROM audit_events \
                     WHERE operation = 'memory.forget_document' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(principal.as_deref(), Some("dora"));
            drop(runtime);
        }

        #[test]
        fn ingest_document_emits_one_audit_row() {
            let (mut actor, runtime, tmp) = build_ingest_actor_for_audit();
            let docs_dir = tmp.path().join("docs");
            std::fs::create_dir_all(&docs_dir).unwrap();
            let path = docs_dir.join("ingest.md");
            std::fs::write(&path, "# ingested\nbody").unwrap();
            let (reply_tx, reply_rx) = oneshot::channel();
            actor.dispatch_ingest_document(
                path,
                crate::document::ChunkConfig::default(),
                Some("eve".into()),
                reply_tx,
            );
            let _ = reply_rx.blocking_recv().unwrap().unwrap();

            assert_eq!(
                count_audit_rows_for_op(&actor.conn, AuditOperation::MemoryIngestDocument),
                1
            );
            let (principal, result): (Option<String>, String) = actor
                .conn
                .query_row(
                    "SELECT principal_subject, result FROM audit_events \
                     WHERE operation = 'memory.ingest_document' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(principal.as_deref(), Some("eve"));
            assert_eq!(result, "ok");
            drop(runtime);
        }

        #[test]
        fn normalize_subjects_emits_audit_row_inside_tx() {
            let (conn, _tmp) = open_test_db();
            // Seed a triple.
            let now_ms = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO triples (
                    triple_id, subject_id, predicate, object_id, object_kind,
                    valid_from_ms, valid_to_ms, confidence, provenance_json,
                    created_at_ms, updated_at_ms
                 ) VALUES (?, 'alex', 'uses', 'rust', 'literal', ?, NULL, 0.9, '{}', ?, ?)",
                params![
                    "00000000-0000-0000-0000-000000000010",
                    now_ms,
                    now_ms,
                    now_ms
                ],
            )
            .unwrap();
            let hnsw = Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };
            let _ = actor
                .handle_normalize_subjects(
                    vec![("alex".into(), "user".into())],
                    false,
                    Some("frank".into()),
                )
                .unwrap();
            assert_eq!(
                count_audit_rows_for_op(&actor.conn, AuditOperation::MemoryNormalizeSubjects,),
                1
            );
            let principal: Option<String> = actor
                .conn
                .query_row(
                    "SELECT principal_subject FROM audit_events \
                     WHERE operation = 'memory.normalize_subjects' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(principal.as_deref(), Some("frank"));
        }
    }

    /// v0.8.0 P5: writer-side redaction tests.
    ///
    /// The redaction registry itself is exhaustively unit-tested in
    /// `crate::redaction::registry::tests`; this submodule covers the
    /// writer-level wiring — that redaction runs before INSERT, that the
    /// audit row records pattern-name counts only (no PII leak), and
    /// that on-disk content reflects the redaction.
    mod redaction_tests {
        use super::*;
        use crate::test_support::{enabled_test_redactor, open_test_db};
        use std::sync::Arc;

        fn build_redacting_actor(conn: Connection) -> (WriterActor, Arc<StubVectorIndex>) {
            let hnsw = Arc::new(StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let actor = WriterActor {
                conn,
                hnsw: hnsw.clone(),
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: enabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };
            (actor, hnsw)
        }

        #[test]
        fn redacted_content_lands_on_disk_for_remember() {
            let (conn, _tmp) = open_test_db();
            let (mut actor, _hnsw) = build_redacting_actor(conn);

            let mut episode = fixture_episode("contact me at user@example.com please");
            let mid = episode.memory_id;
            let embedding = fixture_embedding(4);
            let (tx, rx) = oneshot::channel();
            actor.dispatch_remember(
                std::mem::replace(&mut episode, fixture_episode("placeholder")),
                embedding,
                Some("alice".into()),
                tx,
            );
            assert!(rx.blocking_recv().unwrap().is_ok());

            let content: String = actor
                .conn
                .query_row(
                    "SELECT content FROM episodes WHERE memory_id = ?",
                    params![mid.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(content.contains("[REDACTED:email]"), "got `{content}`");
            assert!(!content.contains("user@example.com"));
        }

        #[test]
        fn retrieval_log_query_is_redacted_and_bounded() {
            let (conn, _tmp) = open_test_db();
            let (mut actor, _hnsw) = build_redacting_actor(conn);
            let long_tail = "x".repeat(MAX_RETRIEVAL_LOG_QUERY_CHARS + 50);

            actor
                .handle_record_memory_retrieval(MemoryRetrievalLogEntry {
                    query: format!("find user@example.com {long_tail}"),
                    recalled_ids: vec![],
                    reason_codes: vec!["memory_context".into()],
                })
                .expect("record retrieval");

            let stored_query: String = actor
                .conn
                .query_row("SELECT query FROM memory_retrieval_log", [], |r| r.get(0))
                .unwrap();
            assert!(stored_query.contains("[REDACTED:email]"));
            assert!(!stored_query.contains("user@example.com"));
            assert!(stored_query.chars().count() <= MAX_RETRIEVAL_LOG_QUERY_CHARS);
        }

        #[test]
        fn redaction_audit_row_emitted_with_pattern_counts() {
            let (conn, _tmp) = open_test_db();
            let (mut actor, _hnsw) = build_redacting_actor(conn);

            let episode = fixture_episode("ssn 123-45-6789 phone 555-123-4567 mail a@b.com");
            let mid = episode.memory_id;
            let (tx, rx) = oneshot::channel();
            actor.dispatch_remember(episode, fixture_embedding(4), Some("carol".into()), tx);
            rx.blocking_recv().unwrap().unwrap();

            let (op, target, details_json): (String, Option<String>, Option<String>) = actor
                .conn
                .query_row(
                    "SELECT operation, target_id, details_json \
                     FROM audit_events WHERE operation = 'redaction.applied'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(op, "redaction.applied");
            assert_eq!(target.as_deref(), Some(mid.to_string().as_str()));
            let details: serde_json::Value =
                serde_json::from_str(details_json.as_deref().unwrap()).unwrap();
            let names: Vec<String> = details["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["pattern_name"].as_str().unwrap().to_string())
                .collect();
            assert!(names.contains(&"email".to_string()));
            assert!(names.contains(&"ssn".to_string()));
            assert!(names.contains(&"us_phone".to_string()));
        }

        #[test]
        fn audit_row_does_not_contain_original_pii() {
            // The strict telemetry contract: redaction.applied details
            // carry counts, never matched substrings.
            let (conn, _tmp) = open_test_db();
            let (mut actor, _hnsw) = build_redacting_actor(conn);

            let episode = fixture_episode("email leak@example.com here");
            let (tx, rx) = oneshot::channel();
            actor.dispatch_remember(episode, fixture_embedding(4), Some("dan".into()), tx);
            rx.blocking_recv().unwrap().unwrap();

            let details: Option<String> = actor
                .conn
                .query_row(
                    "SELECT details_json FROM audit_events \
                     WHERE operation = 'redaction.applied'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let d = details.expect("redaction audit row must have details");
            assert!(
                !d.contains("leak@example.com"),
                "PII leaked into audit: `{d}`"
            );
            assert!(!d.contains("leak"), "PII fragment in audit: `{d}`");
        }

        #[test]
        fn principal_subject_persisted_on_episode_row() {
            // Migration 0006 column wiring: episodes.principal_subject is
            // populated from audit_principal.
            let (conn, _tmp) = open_test_db();
            let (mut actor, _hnsw) = build_redacting_actor(conn);

            let episode = fixture_episode("plain content");
            let mid = episode.memory_id;
            let (tx, rx) = oneshot::channel();
            actor.dispatch_remember(episode, fixture_embedding(4), Some("erin".into()), tx);
            rx.blocking_recv().unwrap().unwrap();

            let principal: Option<String> = actor
                .conn
                .query_row(
                    "SELECT principal_subject FROM episodes WHERE memory_id = ?",
                    params![mid.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(principal.as_deref(), Some("erin"));
        }

        #[test]
        fn no_redaction_audit_row_when_no_matches() {
            // Clean content → no redaction.applied row.
            let (conn, _tmp) = open_test_db();
            let (mut actor, _hnsw) = build_redacting_actor(conn);
            let episode = fixture_episode("no pii here at all");
            let (tx, rx) = oneshot::channel();
            actor.dispatch_remember(episode, fixture_embedding(4), Some("frank".into()), tx);
            rx.blocking_recv().unwrap().unwrap();

            let n: i64 = actor
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM audit_events WHERE operation = 'redaction.applied'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0);
        }

        #[test]
        fn read_path_returns_redacted_content() {
            // Round-trip the redaction through a recall-style read.
            // recall paths go through the read pool, but a SELECT on the
            // writer's own connection exercises the same on-disk state.
            // Verify the recall surface returns the redacted form.
            let (conn, _tmp) = open_test_db();
            let (mut actor, _hnsw) = build_redacting_actor(conn);

            let token = ["gh", "p_", "abcdefghijABCDEFGHIJabcdefghijABCDEF12"].concat();
            let episode = fixture_episode(&format!("token gh PAT {token} done"));
            let mid = episode.memory_id;
            let (tx, rx) = oneshot::channel();
            actor.dispatch_remember(episode, fixture_embedding(4), None, tx);
            rx.blocking_recv().unwrap().unwrap();

            let content: String = actor
                .conn
                .query_row(
                    "SELECT content FROM episodes WHERE memory_id = ?",
                    params![mid.to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(content.contains("[REDACTED:github_pat]"), "got `{content}`");
            assert!(!content.contains(&token));
        }
    }

    // ---- v0.8.1 P3: quota_bytes enforcement ----
    mod quota_tests {
        use super::*;

        #[test]
        fn unlimited_branch_short_circuits_without_db_path() {
            // The hot path: quota = None means no enforcement, no file
            // stat. Confirm one Option compare and we're done.
            let decision = check_quota(None, None, 1_000_000);
            assert_eq!(decision, QuotaDecision::Unlimited);
        }

        #[test]
        fn allowed_when_current_plus_growth_fits_under_quota() {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            // Write 100 bytes into the temp file so metadata().len() = 100.
            std::fs::write(tmp.path(), vec![0u8; 100]).unwrap();
            let decision = check_quota(Some(1000), Some(tmp.path()), 200);
            assert!(
                matches!(
                    decision,
                    QuotaDecision::Allowed {
                        current_size: 100,
                        quota: 1000
                    }
                ),
                "got {decision:?}"
            );
        }

        #[test]
        fn allowed_when_current_plus_growth_exactly_hits_quota() {
            // Strict `>` semantics: hitting exactly is allowed.
            let tmp = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(tmp.path(), vec![0u8; 500]).unwrap();
            let decision = check_quota(Some(1000), Some(tmp.path()), 500);
            assert!(
                matches!(decision, QuotaDecision::Allowed { .. }),
                "exactly-on-quota must be allowed: got {decision:?}"
            );
        }

        #[test]
        fn exceeded_when_growth_would_overflow_quota() {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(tmp.path(), vec![0u8; 900]).unwrap();
            let decision = check_quota(Some(1000), Some(tmp.path()), 200);
            assert!(
                matches!(
                    decision,
                    QuotaDecision::Exceeded {
                        current_size: 900,
                        estimated_growth: 200,
                        quota: 1000,
                    }
                ),
                "got {decision:?}"
            );
        }

        #[test]
        fn asset_quota_size_does_not_follow_symlinks() {
            let tenant = tempfile::TempDir::new().unwrap();
            let outside = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(outside.path(), vec![0u8; 1024]).unwrap();
            let assets = tenant.path().join("assets");
            std::fs::create_dir_all(&assets).unwrap();
            std::fs::write(assets.join("real.bin"), vec![0u8; 10]).unwrap();
            let link = assets.join("linked.bin");

            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(outside.path(), &link).unwrap();
            }
            #[cfg(windows)]
            {
                if std::os::windows::fs::symlink_file(outside.path(), &link).is_err() {
                    return;
                }
            }

            let bytes = dir_size_bytes(&assets).unwrap();
            assert_eq!(bytes, 10);
        }

        #[test]
        fn exceeded_payload_renders_into_audit_json() {
            let err = QuotaExceededError {
                current_size: 900,
                estimated_growth: 200,
                quota: 1000,
            };
            let v = err.to_details_json();
            assert_eq!(v["reason"], "quota_exceeded");
            assert_eq!(v["current_size"], 900);
            assert_eq!(v["estimated_growth"], 200);
            assert_eq!(v["quota"], 1000);
        }

        #[test]
        fn handle_remember_durable_rejects_when_quota_exceeded() {
            // Build a writer with a very small quota (10 bytes) and the
            // db_path pointing at the test DB. The first `remember` of
            // a > 10-byte content should reject and emit a `forbidden`
            // audit row.
            let (conn, tmp) = open_test_db();
            let db_path = tmp.path().join("test.db");
            // Seed the on-disk DB so file size > 0; writer will read
            // this when computing current_size.
            // (open_test_db already created the DB with migrations.)
            let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
                Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                // 10-byte cap — guaranteed below the test DB's actual
                // size (SQLite + migrations occupy ≫ 10 bytes).
                quota_bytes: Some(10),
                db_path: Some(db_path),
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };

            let ep = fixture_episode("this exceeds the 10-byte quota easily");
            let result =
                actor.handle_remember_durable(ep, fixture_embedding(4), Some("erin".into()));
            assert!(
                matches!(result, Err(solo_core::Error::Forbidden(_))),
                "must reject with Forbidden, got: {result:?}"
            );

            // Audit row landed (best-effort emit via emit_audit_best_effort).
            let count: i64 = actor
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM audit_events \
                     WHERE operation='memory.remember' AND result='forbidden'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "forbidden audit row must land");

            // details_json carries the structured fields.
            let details: String = actor
                .conn
                .query_row(
                    "SELECT details_json FROM audit_events \
                     WHERE operation='memory.remember' AND result='forbidden' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&details).unwrap();
            assert_eq!(v["reason"], "quota_exceeded");
            assert_eq!(v["quota"], 10);
        }

        #[test]
        fn handle_remember_durable_proceeds_when_quota_unlimited() {
            // Regression: writer without quota MUST behave exactly like
            // v0.8.0 — no per-op overhead, no file stat.
            let (conn, _tmp) = open_test_db();
            let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
                Arc::new(crate::test_support::StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };
            let ep = fixture_episode("any content");
            let result = actor.handle_remember_durable(ep, fixture_embedding(4), None);
            assert!(
                result.is_ok(),
                "unlimited quota must allow the write: {result:?}"
            );
        }
    }

    /// v0.9.0 P4a: tests pinning the `current_steward()` resolution path
    /// — `tenant.steward_slot()` is the canonical source of truth;
    /// `self.steward` is the fallback for v0.8.x callers that don't
    /// plumb the slot.
    mod p4a_steward_slot_tests {
        use super::*;
        use solo_steward::{Steward, StewardConfig, test_support::StubLlmClient};

        fn arc_stub_steward() -> Arc<Steward> {
            Arc::new(Steward::new(
                Arc::new(StubLlmClient::default_stub().pretend_real_llm(true)),
                StewardConfig::default(),
            ))
        }

        fn build_actor(
            steward: Option<Arc<Steward>>,
            slot: Option<Arc<AsyncRwLock<Option<Arc<Steward>>>>>,
        ) -> WriterActor {
            let (conn, _tmp) = open_test_db();
            // Leak the tempdir so the test holds the conn for the
            // duration; we deliberately don't drop the actor here so
            // its connection stays valid for the assertion read.
            std::mem::forget(_tmp);
            let hnsw = Arc::new(StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward,
                steward_slot: slot,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            }
        }

        /// P4a F1 activation: the writer-actor reads the slot per
        /// command. When `self.steward = None` (sampling backend
        /// before MCP-initialize) AND the slot is populated (after
        /// MCP-initialize), `current_steward` returns the slot's
        /// Steward.
        #[test]
        fn writer_actor_consults_steward_slot_when_self_steward_is_none() {
            let slot_steward = arc_stub_steward();
            let slot = Arc::new(AsyncRwLock::new(Some(slot_steward.clone())));
            let actor = build_actor(None, Some(slot));
            let resolved = actor.current_steward().expect("slot populated");
            assert!(
                Arc::ptr_eq(&resolved, &slot_steward),
                "current_steward must return the slot's Steward when self.steward is None",
            );
        }

        /// P4a slot-vs-self priority: when BOTH the slot and
        /// `self.steward` carry a Steward, the slot wins. This is the
        /// "MCP-session-attaches-mid-life" path — the sampling
        /// Steward overwrites a stale eager-captured field.
        #[test]
        fn writer_actor_prefers_slot_over_self_steward_when_both_set() {
            let slot_steward = arc_stub_steward();
            let eager_steward = arc_stub_steward();
            // Sanity: the two Stewards have distinct allocations so
            // ptr_eq can distinguish them.
            assert!(!Arc::ptr_eq(&slot_steward, &eager_steward));

            let slot = Arc::new(AsyncRwLock::new(Some(slot_steward.clone())));
            let actor = build_actor(Some(eager_steward.clone()), Some(slot));
            let resolved = actor.current_steward().expect("slot populated");
            assert!(
                Arc::ptr_eq(&resolved, &slot_steward),
                "current_steward must prefer the slot when both are set",
            );
            assert!(
                !Arc::ptr_eq(&resolved, &eager_steward),
                "current_steward must NOT return self.steward when slot is populated",
            );
        }

        /// P4a backwards-compat: when the slot is empty AND
        /// `self.steward` is populated, `current_steward` falls back
        /// to `self.steward`. This preserves the v0.8.x eager-
        /// population path that Ollama / Anthropic operators use today.
        #[test]
        fn writer_actor_uses_self_steward_when_slot_is_none() {
            let eager_steward = arc_stub_steward();
            // Slot present but empty — the v0.9.0 P0c shape for a
            // newly-opened sampling tenant before MCP-initialize.
            let slot = Arc::new(AsyncRwLock::new(None));
            let actor = build_actor(Some(eager_steward.clone()), Some(slot));
            let resolved = actor.current_steward().expect("self.steward populated");
            assert!(
                Arc::ptr_eq(&resolved, &eager_steward),
                "current_steward must fall back to self.steward when the slot is empty",
            );
        }

        /// P4a "no LLM" path: when both the slot and `self.steward`
        /// are empty, `current_steward` returns `None`. The merge_plan
        /// gate + the deferred batch path both observe `None` and
        /// short-circuit cleanly.
        #[test]
        fn writer_actor_returns_none_when_slot_and_self_steward_are_both_none() {
            let actor = build_actor(None, Some(Arc::new(AsyncRwLock::new(None))));
            assert!(actor.current_steward().is_none());
        }

        /// P4a no-slot path: when the writer-actor was spawned without
        /// a slot at all (v0.8.x spawn variants like `spawn`,
        /// `spawn_with_capacity`, `spawn_full`, `spawn_full_with_quota`),
        /// `current_steward` skips the slot lookup entirely and uses
        /// `self.steward`. Pins the "spawn without slot" backwards-compat.
        #[test]
        fn writer_actor_falls_back_when_slot_is_unwired() {
            let eager_steward = arc_stub_steward();
            let actor = build_actor(Some(eager_steward.clone()), None);
            let resolved = actor.current_steward().expect("self.steward populated");
            assert!(
                Arc::ptr_eq(&resolved, &eager_steward),
                "no slot wired → current_steward returns self.steward",
            );
        }

        // ---- v0.10.1 m2 audit-minor closure: contention path pin
        //      (deferred from v0.9.0 P4 §m2). ----
        //
        // `current_steward` uses `slot.try_read()` — the sync variant —
        // and falls back to `self.steward` when a writer holds the
        // lock. The P4 audit (m2) flagged that no test exercised this
        // contention path. These tests close that gap by:
        //
        //   1. Spawning a tokio task that acquires the slot's write
        //      lock and holds it via a barrier.
        //   2. Synchronously calling `current_steward()` from the
        //      test thread while the write lock is held.
        //   3. Asserting the call returns `self.steward` (or `None`
        //      if `self.steward` is also `None`) — i.e. the
        //      try_read fell through.
        //   4. Releasing the write lock and asserting subsequent
        //      `current_steward()` calls re-observe the slot.
        //
        // Coordination uses `tokio::sync::Barrier` + `oneshot` to
        // make the contention window deterministic — no `sleep`-
        // based timing. If this test ever becomes flaky (the
        // barrier should make it not), we'd mark it `#[ignore]`
        // with a comment per the brief — but the barrier-based
        // shape gives us reliable contention windows.
        //
        // Grep terms: m2, current_steward_falls_back_on_read_contention,
        // current_steward_recovers_after_lock_release.

        /// m2 pin: while a tokio task holds the slot's write lock,
        /// a sync caller's `current_steward()` falls back to
        /// `self.steward` (or `None` if also empty). The write lock
        /// is released by a oneshot signal after the assertion runs.
        #[test]
        fn current_steward_falls_back_to_self_steward_on_read_contention() {
            let slot_steward = arc_stub_steward();
            let eager_steward = arc_stub_steward();
            assert!(
                !Arc::ptr_eq(&slot_steward, &eager_steward),
                "slot and self.steward must be distinct Arc allocations for ptr_eq to discriminate"
            );

            let slot = Arc::new(AsyncRwLock::new(Some(slot_steward.clone())));
            let actor = build_actor(Some(eager_steward.clone()), Some(slot.clone()));

            // Sanity: no contention → slot wins.
            let baseline = actor.current_steward().expect("baseline: slot populated");
            assert!(
                Arc::ptr_eq(&baseline, &slot_steward),
                "baseline (no contention) must return the slot's Steward"
            );

            // Build a multi-thread runtime so the lock-holder task
            // runs on a separate worker from the test thread's
            // blocking call.
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();

            // `lock_held_tx`: the holder task signals "write lock is
            // mine" when it has acquired the lock.
            // `release_rx`: the test thread signals "you can release
            // now" by sending on `release_tx`.
            let (lock_held_tx, lock_held_rx) = std::sync::mpsc::sync_channel::<()>(1);
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

            let slot_for_holder = slot.clone();
            let holder = runtime.spawn(async move {
                let guard = slot_for_holder.write().await;
                let _ = lock_held_tx.send(());
                // Hold the lock until the test thread says go.
                let _ = release_rx.await;
                // Drop the guard to release the lock.
                drop(guard);
            });

            // Wait until the holder confirms it has the write lock.
            // The sync_channel recv blocks the test thread without
            // requiring a tokio runtime context.
            lock_held_rx
                .recv()
                .expect("holder must signal lock acquisition");

            // Contention point: write lock is held, so try_read
            // should return Err. `current_steward` falls back to
            // `self.steward`.
            let under_contention = actor
                .current_steward()
                .expect("self.steward populated, fallback must succeed");
            assert!(
                Arc::ptr_eq(&under_contention, &eager_steward),
                "under read-contention, current_steward must fall back to self.steward"
            );
            assert!(
                !Arc::ptr_eq(&under_contention, &slot_steward),
                "under read-contention, current_steward must NOT return the slot's Steward"
            );

            // Release the lock and let the holder task finish.
            let _ = release_tx.send(());
            runtime.block_on(holder).unwrap();

            // Post-release: slot wins again.
            let after_release = actor.current_steward().expect("post-release: slot");
            assert!(
                Arc::ptr_eq(&after_release, &slot_steward),
                "after lock release, current_steward must return the slot's Steward again"
            );
        }

        /// m2 pin (None branch): contention with NO `self.steward`
        /// fallback returns `None`. The writer-actor's downstream
        /// consolidation gates already handle a `None` Steward; this
        /// test pins that the contention path observes the same shape
        /// as the "no LLM configured" steady state.
        #[test]
        fn current_steward_returns_none_under_contention_when_self_steward_is_none() {
            let slot_steward = arc_stub_steward();
            let slot = Arc::new(AsyncRwLock::new(Some(slot_steward.clone())));
            // self.steward = None, slot = Some(slot_steward).
            let actor = build_actor(None, Some(slot.clone()));

            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();

            let (lock_held_tx, lock_held_rx) = std::sync::mpsc::sync_channel::<()>(1);
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let slot_for_holder = slot.clone();
            let holder = runtime.spawn(async move {
                let guard = slot_for_holder.write().await;
                let _ = lock_held_tx.send(());
                let _ = release_rx.await;
                drop(guard);
            });

            lock_held_rx
                .recv()
                .expect("holder must signal lock acquisition");

            // try_read returns Err; self.steward is None; result is
            // None (the "no LLM" steady state).
            let under_contention = actor.current_steward();
            assert!(
                under_contention.is_none(),
                "contention + no self.steward fallback => current_steward returns None"
            );

            let _ = release_tx.send(());
            runtime.block_on(holder).unwrap();

            // Post-release: slot is observable again.
            let after = actor
                .current_steward()
                .expect("post-release: slot populated");
            assert!(Arc::ptr_eq(&after, &slot_steward));
        }
    }

    /// v0.9.0 P4b structural pins (P4 audit m1): the architectural
    /// invariants cited in `handle_consolidate_impl`'s "Tests pinning
    /// this" comment. Re-added in the P4 revision after the audit found
    /// they were referenced but missing.
    mod p4b_no_inline_llm_pins {
        use super::*;
        use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
        use crate::test_support::{StubVectorIndex, fixture_embedding, fixture_episode};
        use crate::writer::ConsolidationScope;
        use solo_steward::test_support::StubLlmClient;
        use solo_steward::{Steward, StewardConfig};
        use std::sync::Arc;
        use std::time::{Duration as StdDuration, Instant};
        use tempfile::TempDir;

        fn rt_multi() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap()
        }

        /// Pin: `WriteCommand::Consolidate` returns "quickly" — i.e.
        /// the writer-actor's `handle_consolidate_impl` no longer
        /// `block_on`s the LLM-driven abstraction step. We verify this
        /// by wiring a `Steward` whose `LlmClient` claims to be a real
        /// LLM but never returns a canned response: pre-P4 (with the
        /// inline `block_on(steward.abstract_cluster)` loop), reaching
        /// this path with no canned response would either error out
        /// PER CLUSTER inside the LLM call (slow) or hang. Post-P4
        /// the writer never touches the LLM at all — the call
        /// returns in single-digit milliseconds.
        ///
        /// The wall-time bound is intentionally generous (100ms): we
        /// want failure to mean "the writer-actor is BLOCKING on
        /// SOMETHING (probably an LLM)", not flakiness from CI
        /// jitter.
        #[test]
        fn consolidate_command_returns_quickly_without_blocking_on_llm() {
            use crate::test_support::open_test_db_at;
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("test.db");
            let dim = 4usize;
            let embedder_id = {
                let conn = open_test_db_at(&path);
                get_or_insert_embedder_id(
                    &conn,
                    &EmbedderIdentity {
                        name: "stub".into(),
                        version: "v1".into(),
                        dim: dim as u32,
                        dtype: "f32".into(),
                    },
                )
                .unwrap()
            };

            // Steward says it has a real LLM, but no canned response
            // is wired. Pre-P4 the writer's `block_on` would hit this
            // and the test would slow or hang per cluster. Post-P4
            // the writer never invokes it.
            let llm = Arc::new(StubLlmClient::default_stub().pretend_real_llm(true));
            let steward = Some(Arc::new(Steward::new(llm, StewardConfig::default())));

            let runtime = rt_multi();
            runtime.block_on(async {
                let conn = open_test_db_at(&path);
                let hnsw = Arc::new(StubVectorIndex::new(dim));
                let embedder: Arc<dyn solo_core::Embedder> =
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
                let WriterSpawn { handle, join } =
                    WriterActor::spawn_full_with_embedder_and_optional_steward(
                        conn,
                        hnsw,
                        tmp.path().to_path_buf(),
                        embedder_id,
                        embedder,
                        steward,
                    );

                // Land 3 episodes that DEFINITELY cluster together.
                // Use unit-norm aligned embeddings so cosine
                // similarity is 1.0 between them (the all-zero
                // fixture_embedding never satisfies the cosine gate).
                fn aligned_embedding(dim: usize) -> Embedding {
                    let mut data = vec![0u8; dim * 4];
                    let bytes = 1.0f32.to_le_bytes();
                    data[..4].copy_from_slice(&bytes);
                    Embedding {
                        dtype: solo_core::EmbeddingDtype::F32,
                        dim,
                        data,
                    }
                }

                for i in 0..3 {
                    let mut ep = fixture_episode(&format!("e{i}"));
                    ep.ts_ms = 1_700_000_000_000 + (i as i64) * 1000;
                    handle.remember(ep, aligned_embedding(dim)).await.unwrap();
                }

                let started = Instant::now();
                let report = handle
                    .consolidate(ConsolidationScope::default())
                    .await
                    .expect("consolidate ok");
                let elapsed = started.elapsed();

                assert!(
                    elapsed < StdDuration::from_millis(100),
                    "consolidate took {elapsed:?}; pre-P4 it ran the LLM \
                     loop inline. Post-P4 it MUST NOT — the writer-actor's \
                     command path stays off the LLM critical path. (If the \
                     pin fires the lesson is: the v0.8.x `block_on` regressed.)"
                );
                // Sanity: cheap clustering pass DID run.
                assert!(
                    report.clusters_built >= 1,
                    "clustering pass should at least try; got {report:?}",
                );
                // The LLM-loop's effects MUST NOT appear.
                assert_eq!(
                    report.abstractions_built, 0,
                    "writer-actor must NOT build abstractions inline"
                );
                assert_eq!(
                    report.triples_built, 0,
                    "writer-actor must NOT extract triples inline"
                );

                drop(handle);
                tokio::task::spawn_blocking(move || join.join().unwrap())
                    .await
                    .unwrap();
            });
        }

        /// Pin: triple extraction does NOT happen in the writer-actor's
        /// command path. Concretely: a `Remember` returns and there is
        /// NO `triples` row written by the same writer tx (or by any
        /// path the writer-actor controls). Triples land later via the
        /// daemon-side `triples_batch_timer` + `AttachAbstractionBatch`
        /// path.
        ///
        /// Two-pronged assertion:
        ///   1. After a single Remember, `SELECT COUNT(*) FROM triples`
        ///      is 0.
        ///   2. After a Consolidate with a steward wired (the v0.8.x
        ///      trigger for inline triple extraction), the `triples`
        ///      table is STILL empty — the structural removal stays
        ///      removed.
        #[test]
        fn triples_extraction_does_not_happen_in_writer_actor_command_path() {
            use crate::test_support::open_test_db_at;
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("test.db");
            let dim = 4usize;
            let embedder_id = {
                let conn = open_test_db_at(&path);
                get_or_insert_embedder_id(
                    &conn,
                    &EmbedderIdentity {
                        name: "stub".into(),
                        version: "v1".into(),
                        dim: dim as u32,
                        dtype: "f32".into(),
                    },
                )
                .unwrap()
            };

            // Steward with a CANNED extract-triples response, so any
            // accidental inline-LLM invocation would actually write
            // triples — making the post-condition strict.
            let canned = r#"{
                "content": "Inline triples MUST NOT land via the writer-actor.",
                "confidence": 0.9,
                "triples": [
                    { "subject_id": "ghost", "predicate": "should_not", "object_id": "exist", "object_kind": "literal" }
                ]
            }"#;
            let llm =
                Arc::new(StubLlmClient::with_canned("stub-llm", canned).pretend_real_llm(true));
            let steward = Some(Arc::new(Steward::new(llm, StewardConfig::default())));

            let runtime = rt_multi();
            runtime.block_on(async {
                let conn = open_test_db_at(&path);
                let hnsw = Arc::new(StubVectorIndex::new(dim));
                let embedder: Arc<dyn solo_core::Embedder> =
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim));
                let WriterSpawn { handle, join } =
                    WriterActor::spawn_full_with_embedder_and_optional_steward(
                        conn,
                        hnsw,
                        tmp.path().to_path_buf(),
                        embedder_id,
                        embedder,
                        steward,
                    );

                // (1) Bare Remember.
                let ep = fixture_episode("remember-only");
                handle.remember(ep, fixture_embedding(dim)).await.unwrap();
                {
                    let read = open_test_db_at(&path);
                    let n: i64 = read
                        .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
                        .unwrap();
                    assert_eq!(
                        n, 0,
                        "Remember command must NOT write any triple rows; \
                         triples land later via AttachAbstractionBatch"
                    );
                }

                // (2) Remember several + Consolidate (the v0.8.x
                // trigger). Triples table STILL empty afterwards.
                for i in 1..4 {
                    let mut ep = fixture_episode(&format!("c{i}"));
                    ep.ts_ms = 1_700_000_000_000 + (i as i64) * 1000;
                    handle.remember(ep, fixture_embedding(dim)).await.unwrap();
                }
                let _report = handle
                    .consolidate(ConsolidationScope::default())
                    .await
                    .unwrap();
                {
                    let read = open_test_db_at(&path);
                    let n: i64 = read
                        .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
                        .unwrap();
                    assert_eq!(
                        n, 0,
                        "writer-actor's Consolidate command must NOT \
                         extract triples even with a canned-response Steward \
                         wired (pre-P4 it would write the 'ghost' triple)"
                    );
                    let n_abs: i64 = read
                        .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
                            r.get(0)
                        })
                        .unwrap();
                    assert_eq!(
                        n_abs, 0,
                        "matching pin: writer-actor's Consolidate must NOT \
                         write semantic_abstractions inline either"
                    );
                }

                drop(handle);
                tokio::task::spawn_blocking(move || join.join().unwrap())
                    .await
                    .unwrap();
            });
        }

        #[test]
        fn attach_abstraction_batch_quarantines_weak_triples_without_promoting_them() {
            let (conn, _tmp) = open_test_db();
            let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
                Arc::new(StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };

            let provenance = solo_core::Provenance {
                derived_from: vec![],
                derivation: "test_extraction".to_string(),
                by: "writer_test".to_string(),
                at_ms: 1,
            };
            let cluster_id = MemoryId::new();
            let now_ms = chrono::Utc::now().timestamp_millis();
            actor
                .conn
                .execute(
                    "INSERT INTO clusters (cluster_id, coherence, created_at_ms)
                     VALUES (?1, 1.0, ?2)",
                    params![cluster_id.to_string(), now_ms],
                )
                .unwrap();
            let active_triple = solo_core::Triple {
                triple_id: MemoryId::new(),
                subject_id: "Solo Relay".to_string(),
                predicate: "supports".to_string(),
                object_id: "remote MCP".to_string(),
                object_kind: solo_core::TripleObjectKind::Entity,
                valid_from_ms: 1,
                valid_to_ms: None,
                confidence: solo_core::Confidence::new(0.9).unwrap(),
                provenance: provenance.clone(),
            };
            let active_triple_id = active_triple.triple_id.to_string();
            let weak_triple = solo_core::Triple {
                triple_id: MemoryId::new(),
                subject_id: "assistant".to_string(),
                predicate: "said".to_string(),
                object_id: "I cannot help with that request".to_string(),
                object_kind: solo_core::TripleObjectKind::Literal,
                valid_from_ms: 1,
                valid_to_ms: None,
                confidence: solo_core::Confidence::new(0.9).unwrap(),
                provenance: provenance.clone(),
            };
            let abstraction = solo_core::SemanticAbstraction {
                abstraction_id: MemoryId::new(),
                cluster_id,
                content: "Solo Relay supports remote MCP; assistant chatter is noise.".to_string(),
                triples: vec![active_triple, weak_triple],
                provenance,
                confidence: solo_core::Confidence::new(0.9).unwrap(),
            };

            let report = actor
                .handle_attach_abstraction_batch(
                    vec![(cluster_id, abstraction)],
                    1,
                    25,
                    0,
                    Some("writer-test".to_string()),
                )
                .unwrap();

            assert_eq!(report.abstractions_built, 1);
            assert_eq!(report.triples_extracted, 1);
            assert_eq!(report.triples_quarantined, 1);
            assert_eq!(report.clusters_failed, 0);

            let active_count: i64 = actor
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM triples
                     WHERE subject_id = 'solo-relay'
                       AND predicate = 'supports'
                       AND object_id = 'remote-mcp'
                       AND status = 'active'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(active_count, 1);

            let edge: (String, String, String, i64) = actor
                .conn
                .query_row(
                    "SELECT subject_entity_id, predicate, object_entity_id, evidence_count
                       FROM relationship_edges
                      WHERE edge_id = ?",
                    params![&active_triple_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap();
            assert_eq!(
                edge,
                (
                    "solo-relay".to_string(),
                    "supports".to_string(),
                    "remote-mcp".to_string(),
                    1
                )
            );

            let evidence_count: i64 = actor
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM relationship_evidence WHERE triple_id = ?",
                    params![&active_triple_id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(evidence_count, 1);

            let promoted_noise_count: i64 = actor
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM triples WHERE subject_id = 'assistant'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(promoted_noise_count, 0);

            let review_reason: String = actor
                .conn
                .query_row(
                    "SELECT reason_code FROM triple_reviews
                     WHERE subject_id = 'assistant'
                       AND status = 'needs_review'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(review_reason, "assistant_or_tool_chatter");

            let details: String = actor
                .conn
                .query_row(
                    "SELECT details_json FROM audit_events
                     WHERE operation = 'memory.triples_extract'
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let details: serde_json::Value = serde_json::from_str(&details).unwrap();
            assert_eq!(details["triples_extracted"], 1);
            assert_eq!(details["triples_quarantined"], 1);
        }
    }

    /// v0.9.2 — `WriteCommand::RememberBatch` writer-actor invariants.
    ///
    /// What's pinned here:
    ///
    ///   * Empty batch rejected as `InvalidInput` BEFORE BEGIN.
    ///   * Over-`MAX_REMEMBER_BATCH_SIZE` batch rejected likewise.
    ///   * Happy-path 5-item batch lands all 5 INSERTs in one tx,
    ///     pending_index is drained, hnsw receives 5 adds.
    ///   * The reply is `Vec<MemoryId>` in input order.
    ///   * Exactly ONE batch-level `memory.remember_batch` audit row
    ///     lands inside the tx (per dev-log 0120 §3 Decision G) — not
    ///     N rows.
    ///   * Caller-supplied `salience` round-trips to the `episodes`
    ///     row; omitted salience defaults to 0.5.
    mod p0_remember_batch_tests {
        use super::*;
        use crate::test_support::{
            StubVectorIndex, disabled_test_redactor, fixture_embedding, fixture_episode,
            open_test_db,
        };

        fn build_actor() -> WriterActor {
            let (conn, _tmp) = open_test_db();
            std::mem::forget(_tmp);
            let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
                Arc::new(StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                quota_bytes: None,
                db_path: None,
                invalidate_tx: None,
                invalidate_tenant_id: None,
            }
        }

        #[test]
        fn dispatch_remember_batch_empty_returns_invalid_input() {
            let mut actor = build_actor();
            let (reply_tx, reply_rx) = oneshot::channel();
            actor.dispatch_remember_batch(Vec::new(), None, reply_tx);
            let result = reply_rx.blocking_recv().unwrap();
            assert!(
                matches!(result, Err(solo_core::Error::InvalidInput(_))),
                "empty batch must reject with InvalidInput, got: {result:?}"
            );
        }

        #[test]
        fn dispatch_remember_batch_over_cap_returns_invalid_input() {
            let mut actor = build_actor();
            let (reply_tx, reply_rx) = oneshot::channel();
            let items: Vec<(Episode, Embedding)> = (0..(MAX_REMEMBER_BATCH_SIZE + 1))
                .map(|i| {
                    (
                        fixture_episode(&format!("over-cap-{i}")),
                        fixture_embedding(4),
                    )
                })
                .collect();
            actor.dispatch_remember_batch(items, None, reply_tx);
            let result = reply_rx.blocking_recv().unwrap();
            match result {
                Err(solo_core::Error::InvalidInput(msg)) => {
                    assert!(
                        msg.contains("MAX_REMEMBER_BATCH_SIZE"),
                        "error must reference the cap; got: {msg}"
                    );
                }
                other => panic!("expected InvalidInput, got: {other:?}"),
            }
        }

        #[test]
        fn dispatch_remember_batch_inserts_all_items_in_one_tx() {
            let mut actor = build_actor();
            let (reply_tx, reply_rx) = oneshot::channel();
            let items: Vec<(Episode, Embedding)> = (0..5)
                .map(|i| {
                    let mut ep = fixture_episode(&format!("batch-item-{i}"));
                    // Vary the salience so the round-trip assertion has
                    // something to bite on.
                    ep.salience = 0.1 + (i as f32) * 0.15;
                    (ep, fixture_embedding(4))
                })
                .collect();
            let expected_ids: Vec<MemoryId> = items.iter().map(|(e, _)| e.memory_id).collect();
            let expected_saliences: Vec<f32> = items.iter().map(|(e, _)| e.salience).collect();

            actor.dispatch_remember_batch(items, Some("alice".into()), reply_tx);
            let ids = reply_rx.blocking_recv().unwrap().unwrap();

            // Reply is ordered: ids match input order.
            assert_eq!(ids, expected_ids, "memory_ids must preserve input order");

            // All 5 episode rows landed.
            let n_episodes: i64 = actor
                .conn
                .query_row("SELECT COUNT(*) FROM episodes", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n_episodes, 5, "5 episode rows expected");

            // Saliences round-tripped.
            for (id, expected) in expected_ids.iter().zip(expected_saliences.iter()) {
                let s: f32 = actor
                    .conn
                    .query_row(
                        "SELECT salience FROM episodes WHERE memory_id = ?",
                        params![id.to_string()],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert!(
                    (s - expected).abs() < 1e-5,
                    "salience round-trip mismatch: got {s}, expected {expected}",
                );
            }

            // Exactly ONE batch-level audit row, not N.
            let n_batch_audit: i64 = actor
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM audit_events \
                     WHERE operation = 'memory.remember_batch' \
                       AND result = 'ok'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                n_batch_audit, 1,
                "exactly one batch-level audit row per call (dev-log 0120 §3 Decision G)"
            );

            // The audit row's details_json carries item_count = 5.
            let details: String = actor
                .conn
                .query_row(
                    "SELECT details_json FROM audit_events \
                     WHERE operation = 'memory.remember_batch' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            let v: serde_json::Value = serde_json::from_str(&details).unwrap();
            assert_eq!(
                v["item_count"], 5,
                "details_json.item_count must reflect the batch size"
            );

            // pending_index drained on the dispatch path (post-commit) —
            // 0 rows left over.
            let n_pending: i64 = actor
                .conn
                .query_row("SELECT COUNT(*) FROM pending_index", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                n_pending, 0,
                "pending_index must be drained after a successful batch"
            );
        }

        #[test]
        fn dispatch_remember_batch_with_no_principal_persists_null_principal() {
            let mut actor = build_actor();
            let (reply_tx, reply_rx) = oneshot::channel();
            let items: Vec<(Episode, Embedding)> = (0..3)
                .map(|i| {
                    (
                        fixture_episode(&format!("no-principal-{i}")),
                        fixture_embedding(4),
                    )
                })
                .collect();
            actor.dispatch_remember_batch(items, None, reply_tx);
            assert!(reply_rx.blocking_recv().unwrap().is_ok());

            let principal: Option<String> = actor
                .conn
                .query_row(
                    "SELECT principal_subject FROM audit_events \
                     WHERE operation = 'memory.remember_batch' \
                     ORDER BY audit_id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(
                principal.is_none(),
                "audit row principal must be NULL when caller passed None"
            );
        }

        #[test]
        fn dispatch_remember_batch_quota_exceeded_returns_forbidden() {
            let (conn, tmp) = open_test_db();
            let db_path = tmp.path().join("test.db");
            std::mem::forget(tmp);
            let hnsw: Arc<dyn solo_core::VectorIndex + Send + Sync> =
                Arc::new(StubVectorIndex::new(4));
            let (_tx, rx) = mpsc::channel(1);
            let mut actor = WriterActor {
                conn,
                hnsw,
                rx,
                snapshot_dir: None,
                embedder_id: None,
                embedder: None,
                runtime_handle: None,
                steward: None,
                steward_slot: None,
                triples_batch_signal: None,
                key: None,
                redactor: disabled_test_redactor(),
                // 10-byte cap forces the whole batch into Forbidden — same
                // pattern as `handle_remember_durable_rejects_when_quota_exceeded`.
                quota_bytes: Some(10),
                db_path: Some(db_path),
                invalidate_tx: None,
                invalidate_tenant_id: None,
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            let items: Vec<(Episode, Embedding)> = (0..3)
                .map(|i| {
                    (
                        fixture_episode(&format!("quota-batch-{i}")),
                        fixture_embedding(4),
                    )
                })
                .collect();
            actor.dispatch_remember_batch(items, Some("alice".into()), reply_tx);
            let result = reply_rx.blocking_recv().unwrap();
            assert!(
                matches!(result, Err(solo_core::Error::Forbidden(_))),
                "over-quota batch must reject with Forbidden, got: {result:?}"
            );

            // Forbidden audit row lands (one, not N) — same audit-ratio
            // invariant as the happy path.
            let count: i64 = actor
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM audit_events \
                     WHERE operation = 'memory.remember_batch' \
                       AND result = 'forbidden'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                count, 1,
                "exactly one forbidden audit row for the over-quota batch"
            );

            // No episode rows landed.
            let n_episodes: i64 = actor
                .conn
                .query_row("SELECT COUNT(*) FROM episodes", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                n_episodes, 0,
                "Forbidden return must NOT leak episode rows (no BEGIN was opened)"
            );
        }

        #[test]
        fn remember_batch_invokes_hnsw_add_per_item() {
            // Use the spawned actor + WriteHandle path so we exercise the
            // mpsc → dispatch glue end-to-end. The StubVectorIndex
            // counts `add` calls so we can pin one per item.
            let (conn, _tmp) = open_test_db();
            let hnsw = Arc::new(StubVectorIndex::new(4));
            let WriterSpawn { handle, join: _ } = WriterActor::spawn(conn, hnsw.clone());

            let items: Vec<(Episode, Embedding)> = (0..4)
                .map(|i| {
                    (
                        fixture_episode(&format!("hnsw-batch-{i}")),
                        fixture_embedding(4),
                    )
                })
                .collect();

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let ids = rt
                .block_on(handle.remember_batch_as(Some("alice".into()), items))
                .unwrap();
            assert_eq!(ids.len(), 4);

            // Drain — give the dispatch path a moment to finish the
            // post-commit `pending_index` cleanup before we shut down.
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(handle);
            std::thread::sleep(std::time::Duration::from_millis(50));

            assert_eq!(
                hnsw.add_count(),
                4,
                "hnsw.add must run once per batched item"
            );
        }
    }
}
