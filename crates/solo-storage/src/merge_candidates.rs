// SPDX-License-Identifier: Apache-2.0

//! Read-side helper for `solo doctor`: count existing-cluster pairs that
//! the existing-vs-existing merge pass would coalesce on the next
//! `consolidate --force-merge` (or `--force-merge-on-timer` daemon
//! cycle).
//!
//! Pure SELECT — no write, no LLM, no Steward. The merge plan is
//! pairwise centroid cosine + union-find, deterministic from the
//! cluster centroids alone (`solo_steward::cluster::plan_existing_merges`).
//!
//! ## Why this exists in the storage layer
//!
//! The full existing-merge pass lives in
//! `WriterActor::handle_consolidate` (writer.rs), which uses a private
//! writer-owned method (`fetch_existing_clusters_full`) to load
//! `Vec<solo_core::Cluster>` for `plan_existing_merges`. That method
//! is private — the writer holds the canonical write-side connection.
//! For the doctor's read-only count we need an equivalent loader that
//! operates on a `&Connection` from the read pool without going through
//! the writer's mpsc.
//!
//! ## Sync requirement with `fetch_existing_clusters_full`
//!
//! Two things must stay in sync between this read-side count and the
//! writer's full existing-merge pass:
//!
//!   1. **The SQL.** If the filters drift (new dtype, new column,
//!      different cutoff semantics), the doctor's count would
//!      diverge from what the daemon would actually merge.
//!   2. **The threshold.** Both sites take a `&StewardConfig`
//!      parameter so the caller can pass the exact config the daemon
//!      is using (v0.11.1: resolved from
//!      `solo_steward::StewardConfig::from_settings_then_env`, layering
//!      `[steward]` TOML under `SOLO_CLUSTER_*` env vars). If only one
//!      site reads from a different source, the count again diverges
//!      from the daemon's actual merge behaviour.
//!
//! Audit discipline: when changing one site, change the other. This
//! module's docstring is a stable reference for the pair.
//!
//! Today both queries:
//!   - SELECT join `clusters` x `cluster_episodes` ON cluster_id.
//!   - WHERE `centroid IS NOT NULL` + `centroid_dtype = 'f32'` +
//!     `centroid_dim = ?expected_dim`.
//!   - ORDER BY cluster_id, memory_id (stable single-pass aggregation).
//!
//! The doctor's read variant omits the optional `created_at_ms` cutoff
//! parameter — drift accumulation is meaningfully an all-time signal,
//! not a windowed one.

use rusqlite::{Connection, params_from_iter};
use solo_core::{Cluster, Embedding, EmbeddingDtype, Error, MemoryId, Result};
use std::str::FromStr;

/// Counts of pre-existing cluster pairs above the merge threshold.
///
/// Wraps the `MergePlan` shape for the counting use case; no full
/// `Vec<MergeOp>` is exposed downstream because callers (today: only
/// `solo doctor`) need only the totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MergeCandidateStats {
    /// Existing clusters loaded from the DB whose centroids match the
    /// current embedder's dim. Equals the input cardinality of
    /// `plan_existing_merges`.
    pub clusters_examined: usize,
    /// Distinct merge operations the plan contains. Each op has one
    /// survivor and 1+ losers.
    pub merge_ops: usize,
    /// Total clusters that would be absorbed across all ops. Equivalent
    /// to `MergePlan::absorbed()`.
    pub clusters_would_absorb: usize,
}

/// Count of cluster pairs the existing-vs-existing merge pass would
/// coalesce on the next force-merge cycle.
///
/// Useful for `solo doctor` to surface drift accumulation: as the daemon
/// absorbs new episodes into existing clusters, post-absorb centroid
/// updates can push two pre-existing clusters across the cosine
/// threshold. Without `--force-merge` (manual one-shot) or
/// `--force-merge-on-timer` (daemon flag), the empty-CANDIDATES early
/// return prevents these from coalescing on an idle cycle, and drift
/// accumulates silently.
///
/// ## Pure read-side
///
/// One SELECT joining `clusters` x `cluster_episodes`, filtered by
/// the current embedder's dim. Plan computation is deterministic
/// math (pairwise cosine + union-find). No LLM, no Steward, no write
/// — safe to call from a `ReaderPool` connection.
///
/// ## Threshold
///
/// `config.cluster_cosine_threshold` is what `plan_existing_merges`
/// will compare centroid pairs against. Callers MUST pass the same
/// `StewardConfig` the writer's existing-merge pass uses (v0.11.1:
/// both sites resolve from
/// `solo_steward::StewardConfig::from_settings_then_env()`, which
/// layers `[steward]` TOML overrides under `SOLO_CLUSTER_*` env
/// vars). Otherwise the doctor's count diverges from what the daemon
/// would actually merge — the SQL-sync invariant in this module's
/// docstring loses meaning if the threshold drifts.
///
/// ## Returns
///
/// `Ok(MergeCandidateStats { N, 0, 0 })` if zero or one cluster matches
/// the dim filter (the merge plan is trivially empty).
///
/// ## Errors
///
/// `Error::Storage` for SQL prepare/query/decode failures, or if
/// `plan_existing_merges` returns an error.
pub fn count_existing_merge_candidates(
    conn: &Connection,
    expected_dim: usize,
    config: &solo_steward::StewardConfig,
) -> Result<MergeCandidateStats> {
    // SELECT mirrors `WriterActor::fetch_existing_clusters_full` (no
    // cutoff variant) — see module comment for the sync requirement.
    let sql = "SELECT c.cluster_id, c.centroid, c.centroid_dtype, c.centroid_dim,
                      c.coherence, ce.memory_id
               FROM clusters c
               JOIN cluster_episodes ce ON ce.cluster_id = c.cluster_id
               WHERE c.centroid IS NOT NULL
                 AND c.centroid_dtype = 'f32'
                 AND c.centroid_dim = ?1
               ORDER BY c.cluster_id, ce.memory_id";
    let params: Vec<rusqlite::types::Value> = vec![(expected_dim as i64).into()];

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| Error::storage(format!("prepare merge-candidate clusters: {e}")))?;
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
        .map_err(|e| Error::storage(format!("query_map merge-candidate clusters: {e}")))?;

    // Aggregate. Rows are ORDER BY cluster_id so we can build the
    // output as a single pass.
    let mut clusters: Vec<Cluster> = Vec::new();
    for row in rows {
        let (cid_s, centroid_bytes, dtype_s, dim_i, coherence, memid_s) =
            row.map_err(|e| Error::storage(format!("merge-candidate row decode: {e}")))?;
        if dtype_s != "f32" || (dim_i as usize) != expected_dim {
            continue;
        }
        let cluster_id = match MemoryId::from_str(&cid_s) {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(
                    cluster_id = %cid_s,
                    error = %e,
                    "skipping cluster with unparseable cluster_id (merge-candidate count)"
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
                    "skipping cluster_episodes row with unparseable memory_id (merge-candidate count)"
                );
                continue;
            }
        };
        if clusters.last().map(|c| c.cluster_id) == Some(cluster_id) {
            clusters.last_mut().unwrap().episode_ids.push(memory_id);
        } else {
            let centroid = Embedding {
                dtype: EmbeddingDtype::F32,
                dim: dim_i as usize,
                data: centroid_bytes,
            };
            clusters.push(Cluster {
                cluster_id,
                episode_ids: vec![memory_id],
                centroid: Some(centroid),
                coherence,
            });
        }
    }
    // Defensive: drop empty clusters. The JOIN guarantees ≥1 episode
    // row per cluster, but cluster-id parse failures above could leave
    // an in-progress entry empty.
    clusters.retain(|c| !c.episode_ids.is_empty());

    if clusters.len() < 2 {
        return Ok(MergeCandidateStats {
            clusters_examined: clusters.len(),
            merge_ops: 0,
            clusters_would_absorb: 0,
        });
    }

    let plan = solo_steward::cluster::plan_existing_merges(&clusters, config)
        .map_err(|e| Error::storage(format!("plan_existing_merges (count): {e}")))?;

    Ok(MergeCandidateStats {
        clusters_examined: clusters.len(),
        merge_ops: plan.merges.len(),
        clusters_would_absorb: plan.absorbed(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_test_db;

    /// Smoke: empty DB has no clusters → all-zero stats. Exercises the
    /// SQL validity (parses + binds + executes), the early-return when
    /// `clusters.len() < 2`, and that the function returns
    /// `Ok(Default::default())` rather than an error on the
    /// no-data case. The expensive per-cluster loop and
    /// `plan_existing_merges` invocation are covered by the
    /// existing-merge integration tests in `writer.rs` (same SQL,
    /// same plan call).
    #[test]
    fn empty_db_returns_zero_stats() {
        let (conn, _tmp) = open_test_db();
        let cfg = solo_steward::StewardConfig::default();
        let stats = count_existing_merge_candidates(&conn, 384, &cfg).unwrap();
        assert_eq!(stats, MergeCandidateStats::default());
    }
}
