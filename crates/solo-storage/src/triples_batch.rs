// SPDX-License-Identifier: Apache-2.0

//! v0.9.0 P4c: daemon-side background batch driver for triple
//! extraction.
//!
//! The v0.8.x writer-actor ran `block_on(steward.abstract_cluster)`
//! inline inside `WriteCommand::Consolidate`, blocking the writer
//! thread on every LLM hop. v0.9.0 P4b removed those inline LLM
//! calls (see `crates/solo-storage/src/writer.rs::handle_consolidate_impl`
//! "v0.9.0 P4b: LLM-driven steps deferred to background batch"
//! comment). This module is the destination: an async, batchable,
//! audit-aware path that the daemon's consolidate timer drives
//! independently.
//!
//! ## Flow
//!
//! 1. **Snapshot clusters needing abstraction**.
//!    [`fetch_clusters_without_abstractions`] walks the reader pool to
//!    pull persisted clusters with no `semantic_abstractions` row, plus
//!    retryable clusters whose current abstraction is known low-signal,
//!    capped by `limit`.
//!
//! 2. **Read the active Steward**.
//!    Caller resolves `tenant.steward_slot()` → `Option<Arc<Steward>>`.
//!    For sampling backends this is `None` until an MCP session
//!    attaches; the batch driver short-circuits cleanly when `None`.
//!
//! 3. **Call `Steward::extract_triples_batch`**. The Steward fans out
//!    LLM calls per cluster (the sampling-backed Steward's
//!    `SamplingCoordinator` from P4c — separate module — coalesces
//!    them into one `peer.create_message` per batch window). Returns
//!    `Vec<(cluster_id, SemanticAbstraction)>`.
//!
//! 4. **Send `WriteCommand::AttachAbstractionBatch`**. The writer-
//!    actor persists the abstractions + their triples in ONE tx +
//!    emits ONE `MemoryTriplesExtract` audit row (lesson #30 ACID).
//!
//! ## Cadence
//!
//! [`run_triples_batch_tick`] is called from the daemon's
//! `triples_batch_timer` (in `crates/solo-cli/src/commands/daemon.rs`),
//! which fires on the
//! [`crate::config::TriplesConfig::trigger_interval_secs`] cadence OR
//! after [`crate::config::TriplesConfig::trigger_episode_count`] new
//! episodes have accumulated since the last successful batch
//! (whichever first).

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::params;
use solo_core::{Cluster, Embedding, EmbeddingDtype, Episode, Error, MemoryId, Result, Tier};
use tokio::sync::Notify;

use crate::reader::ReaderPool;
use crate::writer::{AttachAbstractionBatchReport, WriteHandle};

fn low_signal_abstraction_exists_sql() -> String {
    let clauses = solo_steward::abstraction::LOW_SIGNAL_ABSTRACTION_PHRASES
        .iter()
        .map(|phrase| {
            let escaped = phrase.replace('\'', "''");
            format!("instr(LOWER(sa.content), '{escaped}') > 0")
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "EXISTS (
             SELECT 1
               FROM semantic_abstractions sa
              WHERE sa.cluster_id = c.cluster_id
                AND ({clauses})
         )"
    )
}

/// v0.9.0 P4-revision (P4 audit M1): count-based trigger signal for the
/// daemon-side `triples_batch_timer`.
///
/// The pre-revision daemon used a purely time-based
/// `tokio::time::interval(trigger_interval_secs)` loop and ignored
/// `trigger_episode_count` entirely. This struct wires the missing
/// count-based half of the "whichever fires first" contract documented
/// in v0.9.0 P1's TriplesConfig docstring.
///
/// ## Design
///
/// * **Writer side** (`WriterActor::dispatch_remember`): on every
///   successful `Remember`, increments `episodes_since_batch` and pings
///   `notify` IF the new counter value crosses `trigger_episode_count`.
///   The notify is `Notify::notify_one` so spurious wake-ups stack at
///   most one "pending" notification — duplicate increments above the
///   threshold without an intervening batch run still result in one
///   batch trigger.
///
/// * **Daemon side** (`daemon::triples_batch_timer`): `select!`s
///   between `tokio::time::interval`'s tick AND `notify.notified()`.
///   On either signal, runs the batch and resets the counter to 0.
///   Either signal racing in during a batch run is fine — the batch
///   reads + clears atomically.
///
/// Concurrency: the counter is daemon-wide / writer-wide (not
/// per-tenant). The current writer-actor is per-tenant but the daemon
/// today only drives the `default` tenant's triples_batch_timer
/// (matches the v0.9.0 P4c scope). Multi-tenant fan-out is a
/// v0.9.1+ concern.
///
/// Lock-free: `AtomicU64` with `Relaxed` ordering. We don't need a
/// happens-before relationship between the counter bump and any other
/// memory state; the worst-case missed-wake-up is a one-tick delay
/// (the `tokio::time::interval` half of the select! catches it).
#[derive(Debug)]
pub struct TriplesBatchSignal {
    episodes_since_batch: AtomicU64,
    notify: Notify,
    trigger_episode_count: u64,
}

impl TriplesBatchSignal {
    /// Construct a signal armed with the operator-configured threshold.
    /// `trigger_episode_count == 0` disables the count-based trigger
    /// (only the time-based path will fire).
    pub fn new(trigger_episode_count: u64) -> Self {
        Self {
            episodes_since_batch: AtomicU64::new(0),
            notify: Notify::new(),
            trigger_episode_count,
        }
    }

    /// Writer-side hook: increment the counter after a successful
    /// `Remember`. If the new value reaches the threshold (and the
    /// threshold is > 0), ping the notify.
    pub fn note_episode_remembered(&self) {
        if self.trigger_episode_count == 0 {
            return;
        }
        let new_count = self.episodes_since_batch.fetch_add(1, Ordering::Relaxed) + 1;
        if new_count >= self.trigger_episode_count {
            self.notify.notify_one();
        }
    }

    /// Daemon-side hook: reset the counter to 0. Called after a batch
    /// completes (whether time- or count-triggered) so the next round
    /// of accumulation starts fresh.
    pub fn reset(&self) {
        self.episodes_since_batch.store(0, Ordering::Relaxed);
    }

    /// Daemon-side hook: wait for the count-based trigger to fire.
    /// Returns a future the daemon's `select!` arm awaits.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    /// Test/inspection hook: read the current counter value.
    pub fn episodes_since_batch(&self) -> u64 {
        self.episodes_since_batch.load(Ordering::Relaxed)
    }

    /// Test/inspection hook: the configured threshold.
    pub fn trigger_episode_count(&self) -> u64 {
        self.trigger_episode_count
    }
}

/// One batch tick. Resolves the steward from the slot, snapshots
/// pending clusters, calls
/// [`solo_steward::Steward::extract_triples_batch`], dispatches the
/// resulting abstractions to the writer-actor via
/// [`WriteHandle::attach_abstraction_batch`].
///
/// Returns the writer-actor's `AttachAbstractionBatchReport` so the
/// caller can log + emit metrics. When clusters were attempted but every
/// LLM call failed or timed out, returns a synthetic report with zero
/// persisted rows and the failed/deferred counts. `Ok(None)` is the
/// "nothing-to-do" fast path:
///
///   * Slot empty (no Steward attached yet — sampling backend or
///     no LLM configured).
///   * No clusters need abstractions.
///
/// Errors propagate from the reader pool, the writer-actor's
/// `AttachAbstractionBatch` reply, or audit emit failures.
pub async fn run_triples_batch_tick(
    reader: &ReaderPool,
    write_handle: &WriteHandle,
    steward_slot: &Arc<tokio::sync::RwLock<Option<Arc<solo_steward::Steward>>>>,
    current_embedder_id: i64,
    limit: usize,
    per_cluster_timeout: Duration,
    audit_principal: Option<String>,
) -> Result<Option<AttachAbstractionBatchReport>> {
    // Snapshot the slot. Cloning the inner Arc is cheap.
    let steward: Arc<solo_steward::Steward> = {
        let guard = steward_slot.read().await;
        match guard.as_ref() {
            Some(s) => Arc::clone(s),
            None => {
                tracing::debug!("triples_batch: steward_slot empty; nothing to do this tick");
                return Ok(None);
            }
        }
    };

    if !steward.has_llm() {
        tracing::debug!("triples_batch: steward has no LLM client; skipping batch");
        return Ok(None);
    }

    let started = Instant::now();
    let clusters_with_eps =
        fetch_clusters_without_abstractions(reader, current_embedder_id, limit).await?;

    if clusters_with_eps.is_empty() {
        tracing::debug!("triples_batch: no clusters need abstractions; skipping");
        return Ok(None);
    }
    let cluster_count = clusters_with_eps.len();
    let episode_count: usize = clusters_with_eps.iter().map(|(_, eps)| eps.len()).sum();

    // Run the LLM batch. Per-cluster failures log inside
    // `extract_triples_batch` and DON'T propagate — the partial
    // success persists; failures retry next tick. v0.10.1 (m5):
    // each per-cluster call is wrapped in `tokio::time::timeout`
    // with `per_cluster_timeout`; timed-out clusters are surfaced
    // in `outcome.deferred_count` and re-tried on the next tick.
    let outcome = steward
        .extract_triples_batch(clusters_with_eps, per_cluster_timeout)
        .await;

    let deferred_count = outcome.deferred_count;
    let failed_count = outcome.failed_count;
    let last_error = outcome.last_error;
    let items: Vec<(MemoryId, solo_core::SemanticAbstraction)> = outcome.abstractions;

    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

    if items.is_empty() {
        // v0.10.1 (m5): even an all-empty outcome may have non-zero
        // deferrals worth flagging — operators reading the log line
        // benefit from seeing "5 deferred" rather than just silence.
        // Still no writer dispatch (nothing to persist + no audit row
        // needed); return a synthetic report for status surfaces.
        tracing::info!(
            cluster_count,
            episode_count,
            duration_ms,
            failed_count,
            deferred_count,
            "triples_batch: no abstractions to persist this tick \
             (all clusters failed or were deferred); skipping \
             AttachAbstractionBatch dispatch"
        );
        return Ok(Some(AttachAbstractionBatchReport {
            abstractions_built: 0,
            triples_extracted: 0,
            triples_quarantined: 0,
            clusters_failed: failed_count,
            clusters_deferred: deferred_count,
            last_error,
        }));
    }

    let mut report = write_handle
        .attach_abstraction_batch(
            items,
            episode_count,
            duration_ms,
            deferred_count,
            audit_principal,
        )
        .await?;
    report.clusters_failed += failed_count;
    if report.last_error.is_none() {
        report.last_error = last_error;
    }

    tracing::info!(
        cluster_count,
        episode_count,
        abstractions_built = report.abstractions_built,
        triples_extracted = report.triples_extracted,
        triples_quarantined = report.triples_quarantined,
        clusters_failed = report.clusters_failed,
        clusters_deferred = report.clusters_deferred,
        duration_ms,
        "triples_batch: tick complete"
    );

    Ok(Some(report))
}

/// Walk the reader pool to pull every persisted cluster that still
/// needs a Steward abstraction/triples pass, capped by `limit`.
///
/// Returns `(Cluster, Vec<Episode>)` pairs: the cluster's centroid
/// + member-episode set, ready for `Steward::extract_triples_batch`.
///
/// Order: `clusters.rowid ASC` (insert order) — deterministic across
/// re-runs so audit rows are reproducible.
pub async fn fetch_clusters_without_abstractions(
    reader: &ReaderPool,
    current_embedder_id: i64,
    limit: usize,
) -> Result<Vec<(Cluster, Vec<Episode>)>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let cluster_rows: Vec<ClusterRow> = reader
        .interact(move |conn| {
            let low_signal_exists = low_signal_abstraction_exists_sql();
            let sql = format!(
                "SELECT c.cluster_id, c.centroid, c.centroid_dtype, c.centroid_dim, c.coherence
                   FROM clusters c
                  WHERE c.cluster_id NOT IN (SELECT cluster_id FROM semantic_abstractions)
                     OR {low_signal_exists}
                  ORDER BY c.rowid ASC
                  LIMIT ?1"
            );
            let mut stmt = conn.prepare(&sql)?;
            let iter = stmt.query_map(params![limit as i64], |row| {
                Ok(ClusterRow {
                    cluster_id: row.get::<_, String>(0)?,
                    centroid: row.get::<_, Option<Vec<u8>>>(1)?,
                    centroid_dtype: row.get::<_, Option<String>>(2)?,
                    centroid_dim: row.get::<_, Option<i64>>(3)?,
                    coherence: row.get::<_, f64>(4)?,
                })
            })?;
            let mut rows = Vec::new();
            for r in iter {
                rows.push(r?);
            }
            Ok(rows)
        })
        .await?;

    let mut out: Vec<(Cluster, Vec<Episode>)> = Vec::with_capacity(cluster_rows.len());
    for row in cluster_rows {
        let cluster_id = match MemoryId::from_str(&row.cluster_id) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    cluster_id = %row.cluster_id,
                    error = %e,
                    "fetch_clusters_without_abstractions: parse cluster_id failed; skipping"
                );
                continue;
            }
        };

        let episodes = fetch_episodes_for_cluster(reader, current_embedder_id, cluster_id).await?;
        if episodes.is_empty() {
            tracing::warn!(
                cluster_id = %cluster_id,
                "fetch_clusters_without_abstractions: cluster has no member episodes; skipping"
            );
            continue;
        }
        let episode_ids: Vec<MemoryId> = episodes.iter().map(|e| e.memory_id).collect();
        let centroid = build_centroid(
            row.centroid.as_deref(),
            row.centroid_dtype.as_deref(),
            row.centroid_dim,
        );
        let cluster = Cluster {
            cluster_id,
            episode_ids,
            centroid,
            coherence: row.coherence as f32,
        };
        out.push((cluster, episodes));
    }
    Ok(out)
}

/// Count clusters that still need a Steward abstraction/triples pass.
///
/// This is the cheap status twin of [`fetch_clusters_without_abstractions`].
/// It deliberately uses the same retryable-cluster predicate so operator UI
/// can explain whether the background triples batch has work queued without
/// loading every cluster member episode.
pub async fn count_clusters_without_abstractions(reader: &ReaderPool) -> Result<usize> {
    let count: i64 = reader
        .interact(|conn| {
            let low_signal_exists = low_signal_abstraction_exists_sql();
            let sql = format!(
                "SELECT COUNT(*)
                   FROM clusters c
                  WHERE c.cluster_id NOT IN (SELECT cluster_id FROM semantic_abstractions)
                     OR {low_signal_exists}"
            );
            conn.query_row(&sql, [], |row| row.get(0))
        })
        .await?;
    Ok(count.max(0) as usize)
}

#[derive(Debug)]
struct ClusterRow {
    cluster_id: String,
    centroid: Option<Vec<u8>>,
    centroid_dtype: Option<String>,
    centroid_dim: Option<i64>,
    coherence: f64,
}

fn build_centroid(
    bytes: Option<&[u8]>,
    dtype_str: Option<&str>,
    dim: Option<i64>,
) -> Option<Embedding> {
    let bytes = bytes?;
    let dim = usize::try_from(dim.unwrap_or(0)).ok().filter(|d| *d > 0)?;
    let dtype = match dtype_str {
        Some("f32") => EmbeddingDtype::F32,
        Some("f16") => EmbeddingDtype::F16,
        Some("i8") => EmbeddingDtype::I8,
        Some("binary") => EmbeddingDtype::Binary,
        _ => return None,
    };
    Some(Embedding {
        data: bytes.to_vec(),
        dim,
        dtype,
    })
}

async fn fetch_episodes_for_cluster(
    reader: &ReaderPool,
    current_embedder_id: i64,
    cluster_id: MemoryId,
) -> Result<Vec<Episode>> {
    let cluster_id_str = cluster_id.to_string();
    reader
        .interact(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT e.memory_id, e.ts_ms, e.source_type, e.content,
                        e.confidence, e.strength, e.salience
                 FROM episodes e
                 JOIN cluster_episodes ce ON ce.memory_id = e.memory_id
                 WHERE ce.cluster_id = ?1
                   AND e.status = 'active'
                 ORDER BY e.ts_ms ASC",
            )?;
            let iter = stmt.query_map(params![cluster_id_str], |row| {
                let memory_id_s: String = row.get(0)?;
                let ts_ms: i64 = row.get(1)?;
                let source_type: String = row.get(2)?;
                let content: String = row.get(3)?;
                let confidence: f64 = row.get(4)?;
                let strength: f64 = row.get(5)?;
                let salience: f64 = row.get(6)?;
                Ok(EpisodeRow {
                    memory_id_s,
                    ts_ms,
                    source_type,
                    content,
                    confidence,
                    strength,
                    salience,
                })
            })?;
            let mut rows = Vec::new();
            for r in iter {
                rows.push(r?);
            }
            Ok(rows)
        })
        .await
        .and_then(|rows| {
            let mut out: Vec<Episode> = Vec::with_capacity(rows.len());
            for r in rows {
                let memory_id = MemoryId::from_str(&r.memory_id_s).map_err(|e| {
                    Error::storage(format!(
                        "fetch_episodes_for_cluster: parse memory_id {}: {e}",
                        r.memory_id_s
                    ))
                })?;
                out.push(Episode {
                    memory_id,
                    ts_ms: r.ts_ms,
                    source_type: r.source_type,
                    content: r.content,
                    encoding_context: Default::default(),
                    provenance: None,
                    confidence: solo_core::Confidence::new(r.confidence as f32)
                        .unwrap_or(solo_core::Confidence(0.5)),
                    strength: r.strength as f32,
                    salience: r.salience as f32,
                    tier: Tier::Hot,
                    source_id: None,
                });
            }
            let _ = current_embedder_id; // currently unused; kept for forward compat (filter by embedder).
            Ok(out)
        })
}

#[derive(Debug)]
struct EpisodeRow {
    memory_id_s: String,
    ts_ms: i64,
    source_type: String,
    content: String,
    confidence: f64,
    strength: f64,
    salience: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditOperation;
    use crate::embedder_registry::{EmbedderIdentity, get_or_insert_embedder_id};
    use crate::test_support::{StubVectorIndex, open_test_db_at};
    use crate::writer::{WriterActor, WriterSpawn};
    use solo_core::{Confidence, SemanticAbstraction};
    use solo_steward::test_support::StubLlmClient;
    use solo_steward::{Steward, StewardConfig};
    use std::sync::Arc;
    use std::time::Duration as StdDuration;
    use tokio::sync::RwLock as AsyncRwLock;

    // ------------------------------------------------------------------
    // v0.9.0 P4-revision (P4 audit M1): TriplesBatchSignal trigger tests.
    // ------------------------------------------------------------------

    /// M1 test 1: the count-based trigger fires (i.e. `notify_one` pings
    /// the daemon's select arm) as soon as the operator-configured
    /// threshold is reached — BEFORE the time-based interval would have.
    ///
    /// We simulate the daemon's await + writer's increments + use
    /// `tokio::time::timeout` to put a tight upper bound on how long
    /// the count-based path takes. If the trigger isn't wired, the
    /// `signal.notified()` future never resolves and the test fails
    /// via the timeout (the previously-shipped P4c behavior).
    #[test]
    fn count_based_trigger_fires_when_threshold_reached() {
        let runtime = rt_multi();
        runtime.block_on(async {
            let signal = Arc::new(TriplesBatchSignal::new(5));

            // Spawn a "writer-side" task that bumps the counter
            // 5 times. The 5th bump must fire notify_one.
            let writer_signal = signal.clone();
            let writer = tokio::spawn(async move {
                for _ in 0..5 {
                    writer_signal.note_episode_remembered();
                    tokio::task::yield_now().await;
                }
            });

            // Daemon-side: await the notify with a tight timeout. If
            // the trigger doesn't fire, we time out + fail.
            let result = tokio::time::timeout(StdDuration::from_secs(2), signal.notified()).await;
            writer.await.unwrap();

            assert!(
                result.is_ok(),
                "count-based trigger MUST fire once the writer reaches \
                 the threshold; instead the test timed out (the M1 \
                 regression — `trigger_episode_count` wasn't actually \
                 wired and the daemon's select arm never received any \
                 ping)."
            );
            // Counter should be at threshold (or beyond, if more bumps
            // raced past).
            assert!(
                signal.episodes_since_batch() >= 5,
                "counter must have reached the threshold; got {}",
                signal.episodes_since_batch()
            );
        });
    }

    /// M1 test 2: time AND count triggers don't double-run.
    ///
    /// The daemon's `triples_batch_timer` `select!`s between the
    /// time-interval tick AND the count-based notify. When both arms
    /// fire near-simultaneously, the `select!` returns whichever
    /// raced; the next loop iteration observes a reset counter (so
    /// the count arm awaits a fresh threshold crossing) AND a fresh
    /// interval (so the time arm waits another full period). No
    /// duplicate batch run.
    ///
    /// We model this by:
    ///   * Setting a threshold of 1 (so a single increment fires).
    ///   * Notifying + immediately calling `reset()` (simulates a
    ///     batch run).
    ///   * Awaiting another `notified()` with a tight timeout — it
    ///     must NOT resolve (we didn't increment again).
    #[test]
    fn time_and_count_triggers_dont_double_run() {
        let runtime = rt_multi();
        runtime.block_on(async {
            let signal = Arc::new(TriplesBatchSignal::new(1));

            // Single bump → notify fires.
            signal.note_episode_remembered();
            let first =
                tokio::time::timeout(StdDuration::from_millis(500), signal.notified()).await;
            assert!(first.is_ok(), "first notification must arrive");

            // Simulate the daemon running a batch + resetting the
            // counter.
            signal.reset();
            assert_eq!(signal.episodes_since_batch(), 0);

            // Now the second notified() should NOT resolve quickly —
            // the counter was reset and we haven't incremented since.
            let second =
                tokio::time::timeout(StdDuration::from_millis(200), signal.notified()).await;
            assert!(
                second.is_err(),
                "after reset, the count-based arm MUST wait for \
                 fresh accumulation; if it fired immediately the \
                 daemon would batch-run twice for one count crossing"
            );
        });
    }

    /// M1 test 3: the counter resets after a batch.
    ///
    /// After the daemon calls `signal.reset()` post-batch, fresh
    /// increments must accumulate from 0 again. Pins that
    /// `note_episode_remembered` reads the COUNTER, not a stamp.
    #[test]
    fn count_resets_after_batch() {
        let runtime = rt_multi();
        runtime.block_on(async {
            let signal = Arc::new(TriplesBatchSignal::new(3));

            // Drive the first trigger crossing.
            for _ in 0..3 {
                signal.note_episode_remembered();
            }
            // Drain the (potentially-pending) first notify so the
            // second crossing's notify isn't shadowed by it.
            let first =
                tokio::time::timeout(StdDuration::from_millis(500), signal.notified()).await;
            assert!(first.is_ok());

            signal.reset();
            assert_eq!(signal.episodes_since_batch(), 0);

            // Two more increments — below threshold, no fire.
            signal.note_episode_remembered();
            signal.note_episode_remembered();
            assert_eq!(signal.episodes_since_batch(), 2);
            let mid = tokio::time::timeout(StdDuration::from_millis(150), signal.notified()).await;
            assert!(
                mid.is_err(),
                "below-threshold increments must NOT fire the count arm"
            );

            // Third — crosses threshold → notify fires.
            signal.note_episode_remembered();
            let second =
                tokio::time::timeout(StdDuration::from_millis(500), signal.notified()).await;
            assert!(
                second.is_ok(),
                "post-reset, the threshold must fire on its own count \
                 (the counter is per-batch, not cumulative across resets)"
            );
        });
    }

    /// M1 supplementary: when `trigger_episode_count == 0`, the
    /// count-based path is disabled — `note_episode_remembered` is a
    /// no-op and `notified()` never fires from counts alone. The
    /// time-based arm of the daemon's `select!` still works.
    #[test]
    fn count_based_trigger_disabled_when_threshold_is_zero() {
        let runtime = rt_multi();
        runtime.block_on(async {
            let signal = Arc::new(TriplesBatchSignal::new(0));
            for _ in 0..100 {
                signal.note_episode_remembered();
            }
            assert_eq!(
                signal.episodes_since_batch(),
                0,
                "with threshold==0, the increment must be a no-op"
            );
            let attempt =
                tokio::time::timeout(StdDuration::from_millis(200), signal.notified()).await;
            assert!(
                attempt.is_err(),
                "threshold==0 must NOT fire the count arm — the daemon \
                 falls back to the time-based path only"
            );
        });
    }

    fn rt_multi() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn make_steward_with_canned_abstraction() -> Arc<Steward> {
        // Stub returns a parseable abstraction with one triple.
        let canned = r#"{
            "content": "Test abstraction",
            "confidence": 0.9,
            "triples": [
                { "subject_id": "user", "predicate": "likes",
                  "object_id": "pasta", "object_kind": "literal" }
            ]
        }"#;
        let stub = StubLlmClient::with_canned("stub-llm", canned).pretend_real_llm(true);
        Arc::new(Steward::new(Arc::new(stub), StewardConfig::default()))
    }

    fn seed_cluster_with_episodes(
        path: &std::path::Path,
        embedder_id: i64,
        cluster_id: MemoryId,
        n_episodes: usize,
    ) {
        let conn = open_test_db_at(path);
        let now_ms = chrono::Utc::now().timestamp_millis();
        let centroid = bytemuck::cast_slice(&[1.0f32, 0.0, 0.0, 0.0]).to_vec();
        conn.execute(
            "INSERT INTO clusters (cluster_id, centroid, centroid_dtype, centroid_dim, coherence, created_at_ms)
             VALUES (?, ?, 'f32', 4, 0.9, ?)",
            params![cluster_id.to_string(), centroid, now_ms],
        )
        .unwrap();
        for i in 0..n_episodes {
            let mid = MemoryId::new();
            conn.execute(
                "INSERT INTO episodes (memory_id, ts_ms, source_type, content,
                                       encoding_context_json, confidence, strength, salience,
                                       tier, created_at_ms, updated_at_ms)
                 VALUES (?, ?, 'user_message', ?, '{}', 0.9, 0.5, 0.5, 'hot', ?, ?)",
                params![
                    mid.to_string(),
                    now_ms + i as i64 * 1000,
                    format!("ep-{i}"),
                    now_ms,
                    now_ms
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO embeddings (memory_id, embedder_id, dtype, dim, vector, created_at_ms)
                 VALUES (?, ?, 'f32', 4, ?, ?)",
                params![mid.to_string(), embedder_id, centroid, now_ms],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO cluster_episodes (cluster_id, memory_id) VALUES (?, ?)",
                params![cluster_id.to_string(), mid.to_string()],
            )
            .unwrap();
        }
    }

    /// P4c happy path: the timer tick reads the slot's Steward,
    /// fetches one un-abstracted cluster, calls extract_triples_batch,
    /// and persists via AttachAbstractionBatch. The audit row lands
    /// in the per-tenant `audit_events` table.
    #[test]
    fn run_triples_batch_tick_persists_abstractions_and_emits_audit_row() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        let cluster_id = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, cluster_id, 3);

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );

            let reader_pool = ReaderPool::new(&path, None, hnsw.clone()).unwrap();

            let steward = make_steward_with_canned_abstraction();
            let slot = Arc::new(AsyncRwLock::new(Some(steward)));

            let report = run_triples_batch_tick(
                &reader_pool,
                &handle,
                &slot,
                embedder_id,
                100,
                StdDuration::from_secs(60),
                None,
            )
            .await
            .expect("tick ok")
            .expect("Some(report) — we have one un-abstracted cluster");

            assert_eq!(report.abstractions_built, 1);
            assert_eq!(report.triples_extracted, 1);
            assert_eq!(report.clusters_failed, 0);

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        // Storage state: one abstraction + one triple persisted.
        let read = open_test_db_at(&path);
        let n_abs: i64 = read
            .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n_abs, 1);
        let n_triples: i64 = read
            .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_triples, 1);

        // Audit row: one MemoryTriplesExtract event with the right
        // details_json shape.
        let (op_str, details_str): (String, Option<String>) = read
            .query_row(
                "SELECT operation, details_json FROM audit_events
                 WHERE operation = ?
                 ORDER BY ts_ms DESC, audit_id DESC LIMIT 1",
                params![AuditOperation::MemoryTriplesExtract.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(op_str, "memory.triples_extract");
        let details: serde_json::Value = serde_json::from_str(&details_str.unwrap()).unwrap();
        assert_eq!(details["abstractions_built"], 1);
        assert_eq!(details["triples_extracted"], 1);
        assert_eq!(details["cluster_count"], 1);
        assert_eq!(details["clusters_failed"], 0);
        // v0.10.1 (m5): clusters_deferred is in the audit row's
        // details_json — at 0 on the happy path (no timeouts).
        assert_eq!(details["clusters_deferred"], 0);
        assert_eq!(details["episode_count"], 3);
        // `duration_ms` is a non-negative integer; just spot-check
        // shape.
        assert!(details["duration_ms"].is_number());
    }

    /// P4c slot-empty fast path: when `tenant.steward_slot()` is
    /// empty (sampling backend, pre-MCP-initialize), the tick
    /// returns `Ok(None)` cleanly without touching SQL.
    #[test]
    fn run_triples_batch_tick_returns_none_when_slot_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );
            let reader_pool = ReaderPool::new(&path, None, hnsw.clone()).unwrap();
            let slot: Arc<AsyncRwLock<Option<Arc<Steward>>>> = Arc::new(AsyncRwLock::new(None));

            let report = run_triples_batch_tick(
                &reader_pool,
                &handle,
                &slot,
                embedder_id,
                100,
                StdDuration::from_secs(60),
                None,
            )
            .await
            .expect("tick ok");
            assert!(
                report.is_none(),
                "empty slot must short-circuit without dispatching a batch"
            );

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });
    }

    /// P4c no-clusters fast path: when no clusters need abstraction
    /// (every cluster already has a `semantic_abstractions` row),
    /// the tick returns `Ok(None)` without calling the LLM.
    #[test]
    fn run_triples_batch_tick_returns_none_when_no_clusters_need_abstraction() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        // Seed one cluster + give it an abstraction.
        let cluster_id = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, cluster_id, 3);
        {
            let conn = open_test_db_at(&path);
            let now_ms = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO semantic_abstractions
                    (abstraction_id, cluster_id, content, provenance_json,
                     confidence, created_at_ms)
                 VALUES (?, ?, ?, '{}', 0.8, ?)",
                params![
                    MemoryId::new().to_string(),
                    cluster_id.to_string(),
                    "pre-existing",
                    now_ms,
                ],
            )
            .unwrap();
        }

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );
            let reader_pool = ReaderPool::new(&path, None, hnsw.clone()).unwrap();
            let steward = make_steward_with_canned_abstraction();
            let slot = Arc::new(AsyncRwLock::new(Some(steward)));

            let report = run_triples_batch_tick(
                &reader_pool,
                &handle,
                &slot,
                embedder_id,
                100,
                StdDuration::from_secs(60),
                None,
            )
            .await
            .expect("tick ok");
            assert!(
                report.is_none(),
                "no clusters need abstraction → tick is a no-op"
            );

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });
    }

    /// A previous bad/noise abstraction should not permanently block
    /// triple extraction. This is the regression behind the "graph has
    /// nodes but no useful edges" case: the cluster had an abstraction row,
    /// but its only content was low-signal refusal chatter.
    #[test]
    fn run_triples_batch_tick_retries_low_signal_existing_abstraction() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        let cluster_id = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, cluster_id, 3);
        {
            let conn = open_test_db_at(&path);
            let now_ms = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO semantic_abstractions
                    (abstraction_id, cluster_id, content, provenance_json,
                     confidence, created_at_ms)
                 VALUES (?, ?, ?, '{}', 0.2, ?)",
                params![
                    MemoryId::new().to_string(),
                    cluster_id.to_string(),
                    "The assistant was unable to provide assistance.",
                    now_ms,
                ],
            )
            .unwrap();
        }

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );
            let reader_pool = ReaderPool::new(&path, None, hnsw.clone()).unwrap();

            assert_eq!(
                count_clusters_without_abstractions(&reader_pool)
                    .await
                    .expect("count ok"),
                1,
                "low-signal abstraction rows must remain runnable"
            );

            let steward = make_steward_with_canned_abstraction();
            let slot = Arc::new(AsyncRwLock::new(Some(steward)));

            let report = run_triples_batch_tick(
                &reader_pool,
                &handle,
                &slot,
                embedder_id,
                100,
                StdDuration::from_secs(60),
                None,
            )
            .await
            .expect("tick ok")
            .expect("low-signal cluster should be retried");

            assert_eq!(report.abstractions_built, 1);
            assert_eq!(report.triples_extracted, 1);
            assert_eq!(report.clusters_failed, 0);

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        let read = open_test_db_at(&path);
        let (n_abs, content): (i64, String) = read
            .query_row(
                "SELECT COUNT(*), MAX(content) FROM semantic_abstractions WHERE cluster_id = ?",
                params![cluster_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(n_abs, 1);
        assert_eq!(content, "Test abstraction");

        let n_triples: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM triples WHERE cluster_id = ?",
                params![cluster_id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_triples, 1);
    }

    /// If the LLM is called but every cluster fails before producing a
    /// parseable abstraction, the tick should report that attempted
    /// failure instead of looking identical to the no-work fast path.
    #[test]
    fn run_triples_batch_tick_reports_all_failed_attempt() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        let cluster_id = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, cluster_id, 3);

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );
            let reader_pool = ReaderPool::new(&path, None, hnsw.clone()).unwrap();
            let stub =
                StubLlmClient::with_canned("garbage-stub", "NOT-JSON").pretend_real_llm(true);
            stub.push_canned("STILL-NOT-JSON");
            let steward = Arc::new(Steward::new(Arc::new(stub), StewardConfig::default()));
            let slot = Arc::new(AsyncRwLock::new(Some(steward)));

            let report = run_triples_batch_tick(
                &reader_pool,
                &handle,
                &slot,
                embedder_id,
                100,
                StdDuration::from_secs(60),
                None,
            )
            .await
            .expect("tick ok")
            .expect("attempted cluster failures must be reported");

            assert_eq!(report.abstractions_built, 0);
            assert_eq!(report.triples_extracted, 0);
            assert_eq!(report.clusters_failed, 1);
            assert_eq!(report.clusters_deferred, 0);

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        let read = open_test_db_at(&path);
        let n_abs: i64 = read
            .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n_abs, 0);
        let n_audit: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE operation = 'memory.triples_extract'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_audit, 0, "no persistence means no audit row");
    }

    /// P4c "no-llm" steward fast path: a Steward wrapping a
    /// stub that reports `is_real_llm() == false` should skip the
    /// batch even with the slot populated (matches the v0.8.x
    /// `Steward::has_llm()` gate).
    #[test]
    fn run_triples_batch_tick_skips_when_steward_has_no_llm() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        let cluster_id = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, cluster_id, 3);

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );
            let reader_pool = ReaderPool::new(&path, None, hnsw.clone()).unwrap();
            // Default stub: pretend_real_llm(false) → has_llm() = false.
            let stub_no_llm = Arc::new(StubLlmClient::default_stub());
            let steward = Arc::new(Steward::new(stub_no_llm, StewardConfig::default()));
            let slot = Arc::new(AsyncRwLock::new(Some(steward)));

            let report = run_triples_batch_tick(
                &reader_pool,
                &handle,
                &slot,
                embedder_id,
                100,
                StdDuration::from_secs(60),
                None,
            )
            .await
            .expect("tick ok");
            assert!(report.is_none(), "stub-only steward → tick is a no-op");

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });
    }

    /// P4c attach_abstraction_batch report: verify the writer-actor
    /// returns the right counts when given a multi-cluster batch.
    #[test]
    fn attach_abstraction_batch_persists_multi_cluster_batch() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        // Seed two clusters.
        let c1 = MemoryId::new();
        let c2 = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, c1, 2);
        seed_cluster_with_episodes(&path, embedder_id, c2, 3);

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );

            // Build two abstractions manually (skipping LLM).
            let abs1 = mk_abs(c1, 2);
            let abs2 = mk_abs(c2, 1);
            let items = vec![(c1, abs1), (c2, abs2)];

            let report = handle
                .attach_abstraction_batch(items, 5, 42, 0, Some("op".into()))
                .await
                .expect("batch ok");
            assert_eq!(report.abstractions_built, 2);
            assert_eq!(report.triples_extracted, 3);
            assert_eq!(report.clusters_failed, 0);
            assert_eq!(report.clusters_deferred, 0);

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        // Verify SQL state.
        let read = open_test_db_at(&path);
        let n_abs: i64 = read
            .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n_abs, 2);
        let n_triples: i64 = read
            .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n_triples, 3);

        // Audit row lands once.
        let (audit_count,): (i64,) = read
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE operation = 'memory.triples_extract'",
                [],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_eq!(
            audit_count, 1,
            "one batch must emit exactly one MemoryTriplesExtract audit row"
        );
    }

    /// P4c idempotency: if the same batch's INSERT runs twice (rare
    /// retry scenario), the handler DELETES + RE-INSERTs the
    /// per-cluster abstraction + triples so the on-disk state is
    /// the latest call's view. No UNIQUE-constraint violations.
    #[test]
    fn attach_abstraction_batch_is_idempotent_on_re_run() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        let c = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, c, 2);

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );

            // First batch — 1 triple per abstraction.
            let abs_v1 = mk_abs(c, 1);
            let r1 = handle
                .attach_abstraction_batch(vec![(c, abs_v1)], 2, 10, 0, None)
                .await
                .expect("v1 batch ok");
            assert_eq!(r1.abstractions_built, 1);
            assert_eq!(r1.triples_extracted, 1);

            // Second batch — 3 triples now; the older 1 is gone.
            let abs_v2 = mk_abs(c, 3);
            let r2 = handle
                .attach_abstraction_batch(vec![(c, abs_v2)], 2, 10, 0, None)
                .await
                .expect("v2 batch ok");
            assert_eq!(r2.abstractions_built, 1);
            assert_eq!(r2.triples_extracted, 3);

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        let read = open_test_db_at(&path);
        let n_abs: i64 = read
            .query_row("SELECT COUNT(*) FROM semantic_abstractions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            n_abs, 1,
            "second batch replaces the first cluster's abstraction; only one row remains"
        );
        let n_triples: i64 = read
            .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n_triples, 3,
            "the second batch's 3 triples replaced the first batch's 1"
        );

        // Two audit rows (one per batch).
        let (audit_count,): (i64,) = read
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE operation = 'memory.triples_extract'",
                [],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_eq!(audit_count, 2);
    }

    /// P4c attach_abstraction_batch rejects shape mismatch: the
    /// tuple's MemoryId MUST equal the embedded abstraction's
    /// cluster_id. Pin the validation.
    #[test]
    fn attach_abstraction_batch_rejects_mismatched_cluster_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        let dim = 4usize;

        let _embedder_id = {
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

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    1,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );

            let a_id = MemoryId::new();
            let b_id = MemoryId::new();
            // Build abstraction with cluster_id = b_id, but key the
            // tuple by a_id. Mismatch.
            let mut abs = mk_abs(b_id, 0);
            abs.cluster_id = b_id;
            let err = handle
                .attach_abstraction_batch(vec![(a_id, abs)], 0, 0, 0, None)
                .await
                .expect_err("mismatch must error");
            assert!(
                format!("{err}").contains("cluster_id mismatch"),
                "expected mismatch error; got: {err}"
            );

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });
    }

    /// v0.9.0 P4-revision (P4 audit M2): partial-failure preservation.
    ///
    /// Three clusters in the batch. Cluster #2's INSERT fails (we
    /// force it via a UNIQUE constraint collision on `triples.
    /// triple_id` — we pre-insert a triple with the colliding id
    /// keyed to a DIFFERENT cluster_id so the per-cluster DELETE FROM
    /// triples WHERE cluster_id = c2 won't clear it). Assertions:
    ///
    ///   * Clusters 1 + 3 land their new abstractions (1 each).
    ///   * Cluster 2 KEEPS its old stale abstraction (the SAVEPOINT
    ///     ROLLBACK undid the per-cluster DELETE).
    ///   * `report.clusters_failed == 1`, `abstractions_built == 2`.
    ///   * One audit row.
    #[test]
    fn attach_abstraction_batch_partial_failure_preserves_other_clusters() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        let c1 = MemoryId::new();
        let c2 = MemoryId::new();
        let c3 = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, c1, 1);
        seed_cluster_with_episodes(&path, embedder_id, c2, 1);
        seed_cluster_with_episodes(&path, embedder_id, c3, 1);

        // Pre-seed cluster #2 with a STALE abstraction we expect to
        // see preserved after the partial-failure rollback. The
        // abstraction's id is what we'll read back to confirm it's
        // the OLD one, not a NEW replacement.
        let stale_abs_id = MemoryId::new();
        {
            let conn = open_test_db_at(&path);
            let now_ms = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO semantic_abstractions
                    (abstraction_id, cluster_id, content, provenance_json,
                     confidence, created_at_ms)
                 VALUES (?, ?, 'STALE-CONTENT', '{}', 0.5, ?)",
                params![stale_abs_id.to_string(), c2.to_string(), now_ms],
            )
            .unwrap();
        }

        // Pre-seed a triple with a specific triple_id, keyed to c1
        // (a cluster that EXISTS so the FK to clusters(cluster_id) is
        // satisfied). The per-cluster DELETE FROM triples WHERE
        // cluster_id = c2 will NOT clear this row (it belongs to c1).
        // We'll then build cluster #2's abstraction with a triple
        // that reuses this triple_id → the INSERT INTO triples will
        // trip UNIQUE(triple_id) and fail. The DELETE for cluster #1
        // happens INSIDE cluster #1's savepoint — that delete + new
        // insert will REPLACE this row, so cluster #1 ends up with
        // its own fresh triple. Meaning: we use c1's slot only to
        // sneak in a triple_id that's "borrowed" by c2's INSERT.
        // (cluster #1 lands first; ITS savepoint releases; then
        // cluster #2 runs and its INSERT trips the UNIQUE because
        // cluster #1's new abstraction's triple_id is different from
        // collision_triple_id — and the collision row was DELETEd by
        // cluster #1's per-cluster path.)
        //
        // To make this work robustly, we pre-seed the collision into
        // cluster #3 instead — cluster #3 runs LAST and its
        // savepoint's DELETE+INSERT replaces the collision row only
        // AFTER cluster #2 has already tripped on it. So:
        //   1. Cluster 1 lands cleanly.
        //   2. Cluster 2 INSERT trips on the collision triple_id
        //      pre-seeded in cluster 3 → savepoint ROLLBACK.
        //   3. Cluster 3 lands cleanly (its DELETE wipes the
        //      collision row, then its INSERT lays fresh triples).
        let collision_triple_id = MemoryId::new();
        {
            let conn = open_test_db_at(&path);
            let now_ms = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO triples
                    (triple_id, subject_id, predicate, object_id, object_kind,
                     valid_from_ms, valid_to_ms, confidence, provenance_json,
                     created_at_ms, updated_at_ms, cluster_id, source_episode_id)
                 VALUES (?, ?, ?, ?, 'literal', 0, NULL, 0.5, '{}', ?, ?, ?, NULL)",
                params![
                    collision_triple_id.to_string(),
                    "foreign-subj",
                    "foreign-pred",
                    "foreign-obj",
                    now_ms,
                    now_ms,
                    c3.to_string()
                ],
            )
            .unwrap();
        }

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );

            // Build three abstractions. Cluster 2's first triple
            // reuses `collision_triple_id` → INSERT fails on the
            // UNIQUE(triple_id) constraint.
            let abs1 = mk_abs(c1, 1);
            let mut abs2 = mk_abs(c2, 1);
            abs2.triples[0].triple_id = collision_triple_id;
            let abs3 = mk_abs(c3, 1);

            let report = handle
                .attach_abstraction_batch(vec![(c1, abs1), (c2, abs2), (c3, abs3)], 3, 100, 0, None)
                .await
                .expect("batch must succeed at the outer-tx level");

            assert_eq!(
                report.abstractions_built, 2,
                "clusters 1+3 should both land their new abstractions"
            );
            assert_eq!(
                report.clusters_failed, 1,
                "cluster 2's INSERT must fail and be booked as a failure"
            );
            assert_eq!(
                report.triples_extracted, 2,
                "two successful clusters' triples (1 each)"
            );

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        // Verify SQL state: cluster 2's STALE abstraction is still there.
        let read = open_test_db_at(&path);
        let c2_abs_id: Option<String> = read
            .query_row(
                "SELECT abstraction_id FROM semantic_abstractions WHERE cluster_id = ?",
                params![c2.to_string()],
                |r| r.get(0),
            )
            .ok();
        assert_eq!(
            c2_abs_id.as_deref(),
            Some(stale_abs_id.to_string().as_str()),
            "cluster 2's pre-existing abstraction must be preserved \
             (savepoint rollback undid the per-cluster DELETE)"
        );

        // Clusters 1 + 3 have fresh abstractions.
        let n_c1: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM semantic_abstractions WHERE cluster_id = ?",
                params![c1.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        let n_c3: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM semantic_abstractions WHERE cluster_id = ?",
                params![c3.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n_c1, 1, "cluster 1 has a fresh abstraction");
        assert_eq!(n_c3, 1, "cluster 3 has a fresh abstraction");

        // Cluster 2's UNIQUE triple_id collision row was the pre-
        // seeded row keyed to cluster 3. After cluster 3's
        // successful run, its per-cluster DELETE wiped this row
        // (DELETE FROM triples WHERE cluster_id = c3 covers it) and
        // its INSERT laid fresh triples. So the collision row is
        // gone; cluster 3's fresh triple replaces it.
        let n_triples_c3: i64 = read
            .query_row(
                "SELECT COUNT(*) FROM triples WHERE cluster_id = ?",
                params![c3.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n_triples_c3, 1,
            "cluster 3's successful run replaced the pre-seeded \
             collision row with its own fresh triple"
        );

        // One audit row (the outer batch).
        let (audit_count,): (i64,) = read
            .query_row(
                "SELECT COUNT(*) FROM audit_events WHERE operation = 'memory.triples_extract'",
                [],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_eq!(audit_count, 1);
    }

    /// v0.10.1 (P4 audit m5): the per-tick audit row's details_json
    /// carries `clusters_deferred` echoed from the caller's argument.
    ///
    /// We don't simulate a hung LLM call here (covered by the steward-
    /// crate test `extract_triples_batch_continues_after_cluster_timeout`).
    /// Instead, we drive the writer-actor's `attach_abstraction_batch`
    /// directly with a non-zero `clusters_deferred` argument and verify
    /// the count lands in the audit row + the returned report.
    #[test]
    fn triples_batch_audit_row_includes_deferred_count() {
        let tmp = tempfile::TempDir::new().unwrap();
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

        // Seed one cluster for the successful path.
        let c1 = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, c1, 2);

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );

            // One real abstraction lands; caller reports 2 clusters
            // were deferred upstream (the LLM timed out on them).
            let abs1 = mk_abs(c1, 1);
            let report = handle
                .attach_abstraction_batch(
                    vec![(c1, abs1)],
                    /* episode_count */ 2,
                    /* duration_ms */ 100,
                    /* clusters_deferred */ 2,
                    /* audit_principal */ None,
                )
                .await
                .expect("batch ok");

            assert_eq!(report.abstractions_built, 1);
            assert_eq!(
                report.clusters_deferred, 2,
                "report must echo the caller's deferred count"
            );

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        // Audit row's details_json carries clusters_deferred = 2.
        let read = open_test_db_at(&path);
        let details_str: Option<String> = read
            .query_row(
                "SELECT details_json FROM audit_events
                 WHERE operation = 'memory.triples_extract'
                 ORDER BY ts_ms DESC, audit_id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let details: serde_json::Value = serde_json::from_str(&details_str.unwrap()).unwrap();
        assert_eq!(
            details["clusters_deferred"], 2,
            "the m5 deferred counter must surface in the audit row's \
             details_json so operators see 'something is slow' in the \
             audit log without grepping `tracing::warn!` lines"
        );
        // Other fields still present (regression guard).
        assert_eq!(details["abstractions_built"], 1);
    }

    #[test]
    fn attach_abstraction_batch_applies_quality_gate_and_aliases() {
        use solo_core::TripleObjectKind;

        let tmp = tempfile::TempDir::new().unwrap();
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

        let cluster_id = MemoryId::new();
        seed_cluster_with_episodes(&path, embedder_id, cluster_id, 1);
        let mut abs = mk_abs(cluster_id, 3);
        abs.triples[0].subject_id = "solo_relay".into();
        abs.triples[0].predicate = "integrates-with".into();
        abs.triples[0].object_id = "solo-relay".into();
        abs.triples[0].object_kind = TripleObjectKind::Entity;
        abs.triples[1].subject_id = "solo_relay".into();
        abs.triples[1].predicate = "uses_path".into();
        abs.triples[1].object_id = r"C:\Users\Example\Projects\solo".into();
        abs.triples[1].object_kind = TripleObjectKind::Entity;
        abs.triples[2].subject_id = "assistant".into();
        abs.triples[2].predicate = "said".into();
        abs.triples[2].object_id = "The assistant was unable to provide assistance".into();
        {
            let conn = open_test_db_at(&path);
            let now_ms = chrono::Utc::now().timestamp_millis();
            conn.execute(
                "INSERT INTO triple_reviews
                    (review_id, candidate_fingerprint, cluster_id, subject_id,
                     predicate, object_id, object_kind, confidence, reason_code,
                     reason, provenance_json, created_at_ms, updated_at_ms)
                 VALUES
                    (?1, 'fp-stale-rerun', ?2, 'stale_subject', 'stale_predicate',
                     'stale_object', 'literal', 0.1, 'low_confidence',
                     'stale review from previous run', '{}', ?3, ?3)",
                params![MemoryId::new().to_string(), cluster_id.to_string(), now_ms],
            )
            .expect("seed stale review");
        }

        let runtime = rt_multi();
        runtime.block_on(async {
            let conn = open_test_db_at(&path);
            let hnsw = Arc::new(StubVectorIndex::new(dim));
            let WriterSpawn { handle, join } =
                WriterActor::spawn_full_with_embedder_and_optional_steward(
                    conn,
                    hnsw.clone(),
                    tmp.path().to_path_buf(),
                    embedder_id,
                    Arc::new(crate::embedder::StubEmbedder::new("stub", "v1", dim)),
                    None,
                );

            let report = handle
                .attach_abstraction_batch(vec![(cluster_id, abs)], 1, 100, 0, None)
                .await
                .expect("batch should persist accepted facts and reviews");
            assert_eq!(report.abstractions_built, 1);
            assert_eq!(report.triples_extracted, 2);
            assert_eq!(report.triples_quarantined, 1);

            drop(handle);
            tokio::task::spawn_blocking(move || join.join().unwrap())
                .await
                .unwrap();
        });

        let read = open_test_db_at(&path);
        let active: i64 = read
            .query_row("SELECT COUNT(*) FROM triples", [], |r| r.get(0))
            .unwrap();
        assert_eq!(active, 2);
        let canonical: String = read
            .query_row(
                "SELECT canonical_id FROM entity_aliases WHERE alias = 'solo_relay'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(canonical, "solo-relay");
        let path_kind: String = read
            .query_row(
                "SELECT object_kind FROM triples WHERE predicate = 'uses_path'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path_kind, "literal");
        let review_reason: String = read
            .query_row(
                "SELECT reason_code FROM triple_reviews WHERE status = 'needs_review'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(review_reason, "assistant_or_tool_chatter");
        let review_count: i64 = read
            .query_row("SELECT COUNT(*) FROM triple_reviews", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            review_count, 1,
            "stale per-cluster reviews should be replaced"
        );
    }

    /// Helper: build a `SemanticAbstraction` with `n_triples` triples,
    /// all pointing at the supplied `cluster_id`.
    fn mk_abs(cluster_id: MemoryId, n_triples: usize) -> SemanticAbstraction {
        use solo_core::{Provenance, Triple, TripleObjectKind};
        let mut triples: Vec<Triple> = Vec::new();
        for i in 0..n_triples {
            triples.push(Triple {
                triple_id: MemoryId::new(),
                subject_id: "subject".into(),
                predicate: format!("pred_{i}"),
                object_id: format!("obj_{i}"),
                object_kind: TripleObjectKind::Literal,
                valid_from_ms: 0,
                valid_to_ms: None,
                confidence: Confidence::new(0.9).unwrap(),
                provenance: Provenance {
                    derived_from: vec![],
                    derivation: "consolidation".into(),
                    by: "stub-llm".into(),
                    at_ms: 0,
                },
            });
        }
        SemanticAbstraction {
            abstraction_id: MemoryId::new(),
            cluster_id,
            content: "test abs".into(),
            confidence: Confidence::new(0.7).unwrap(),
            triples,
            provenance: Provenance {
                derived_from: vec![],
                derivation: "consolidation".into(),
                by: "stub-llm".into(),
                at_ms: 0,
            },
        }
    }
}
