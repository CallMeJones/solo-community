// SPDX-License-Identifier: Apache-2.0

//! Pure-deterministic episode clustering — the SWS-equivalent dedup
//! pass that opens every consolidation cycle.
//!
//! Per ADR-0002, this is the "IP" of Solo's consolidation: clustering
//! logic lives in one place, tested deterministically, refined over
//! time. The LLM is the swap point (used later by `abstract_cluster`),
//! not the clustering math.
//!
//! ## Algorithm (v0.2.0 — "simple time-bucketed cosine")
//!
//!   1. **Validate inputs.** All embeddings must share the same `dim`
//!      and dtype `F32`; reject otherwise. (HNSW also requires F32 in
//!      v0.x — this matches.)
//!
//!   2. **Bucket by day (UTC).** A "cluster of related memories" in
//!      practice means "things you said in the same conversation
//!      window". Two semantically-similar episodes from days apart
//!      are usually noteworthy *because* of the gap, not duplicates.
//!      1-day buckets reflect that. Bucket boundaries are aligned to
//!      UTC midnight (`ts_ms` divided by 86_400_000).
//!
//!   3. **Build a similarity graph per bucket.** For every pair of
//!      episodes within a bucket, compute cosine similarity (dot
//!      product on unit vectors). Add an edge when sim ≥
//!      `config.cluster_cosine_threshold` (default 0.55).
//!
//!   4. **Find connected components.** Standard union-find. Each
//!      connected component is a candidate cluster.
//!
//!   5. **Filter by size.** Keep components with `≥ cluster_min_size`
//!      episodes (default 2).
//!
//!   6. **Build [`Cluster`] structs.** For each kept component:
//!      - `cluster_id`: fresh UUID v7 (time-ordered, but not
//!        deterministic — tests assert on episode-id sets, not on
//!        cluster_id).
//!      - `episode_ids`: sorted by `(ts_ms, memory_id)` — stable
//!        ordering across re-runs over the same input.
//!      - `centroid`: mean of unit embeddings, then re-normalised to
//!        unit length so cosine search against the centroid is well-
//!        defined. `None` if the component has zero coherent
//!        embeddings (shouldn't happen post-validation).
//!      - `coherence`: average of pairwise cosines within the cluster
//!        (i.e. the same metric used to admit edges, averaged across
//!        all C(n,2) pairs in the component, not just edges that
//!        crossed threshold).
//!
//!   7. **Return clusters sorted by their min-episode `ts_ms`** — the
//!      earliest cluster first. Stable across re-runs.
//!
//! ## Why this design
//!
//! - **Pure-deterministic**: same input → same output (modulo UUID v7
//!   randomness on `cluster_id`). Easy to golden-corpus test.
//! - **No LLM dependency**: clustering is the cheap dedup pass that
//!   runs every consolidation cycle. The expensive LLM
//!   `abstract_cluster` call only runs on accepted clusters.
//! - **No DB dependency**: caller pairs `(Episode, Embedding)` and
//!   passes them in. Steward stays decoupled from the storage layer
//!   per ADR-0002 ("Steward depends only on solo-core").
//! - **Time-bucket first, then cosine**: cheaper and more accurate for
//!   the typical "same conversation" use case than global cosine.
//!   Pure-cosine would conflate today's "recipe for pasta" with last
//!   month's "recipe for pasta" — usually wrong for consolidation.
//!
//! ## What's not here yet
//!
//! - **Entity-aware bucketing.** ADR-0002 mentions "by entity + time
//!   bucket + cosine". v0.2.0 ships time + cosine; entity bucketing
//!   needs the triples-extraction pass which is post-`abstract_cluster`.
//!   Once we have triples, a v0.3 bump can re-cluster by shared
//!   subject-IDs within each time bucket for finer-grained groups.
//!
//! - **Streaming / incremental clustering.** Today every call processes
//!   the full input batch from scratch. Daemon timer should pass only
//!   "episodes since last consolidation" so the work is bounded.
//!
//! - **Adaptive threshold.** 0.85 is a reasonable default for BGE-M3
//!   F32 vectors. A future pass can auto-calibrate per-corpus.

use std::collections::HashMap;

use solo_core::{Cluster, Embedding, EmbeddingDtype, Episode, Error, MemoryId, Result};

use crate::StewardConfig;

const MS_PER_DAY: i64 = 86_400_000;

/// Run the clustering algorithm over `inputs` with `config`'s
/// thresholds. Returns clusters sorted by their earliest episode
/// timestamp.
///
/// `inputs` is a slice of `(Episode, Embedding)` pairs the caller has
/// already aligned (typically by joining `episodes` and `embeddings`
/// in SQL upstream of this call).
pub fn cluster_episodes(
    inputs: &[(Episode, Embedding)],
    config: &StewardConfig,
) -> Result<Vec<Cluster>> {
    // ----- 1. Validate -----
    if inputs.is_empty() {
        return Ok(Vec::new());
    }
    let dim = inputs[0].1.dim;
    let dtype = inputs[0].1.dtype;
    if dtype != EmbeddingDtype::F32 {
        return Err(Error::steward(format!(
            "cluster_episodes requires F32 embeddings; got {dtype:?}"
        )));
    }
    for (i, (_, emb)) in inputs.iter().enumerate() {
        if emb.dim != dim {
            return Err(Error::steward(format!(
                "cluster_episodes: embedding dim mismatch at index {i}: {} vs {dim}",
                emb.dim
            )));
        }
        if emb.dtype != EmbeddingDtype::F32 {
            return Err(Error::steward(format!(
                "cluster_episodes: embedding dtype mismatch at index {i}: {:?} vs F32",
                emb.dtype
            )));
        }
        emb.validate()?;
    }

    // Decode all embeddings to &[f32] up front; verifies alignment.
    let f32_views: Vec<&[f32]> = inputs
        .iter()
        .enumerate()
        .map(|(i, (_, emb))| {
            emb.as_f32_slice().ok_or_else(|| {
                Error::steward(format!(
                    "cluster_episodes: embedding at index {i} failed F32 cast"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // ----- 2. Bucket by UTC day -----
    let mut buckets: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, (ep, _)) in inputs.iter().enumerate() {
        let day = ep.ts_ms.div_euclid(MS_PER_DAY);
        buckets.entry(day).or_default().push(i);
    }

    // Walk buckets in deterministic (chronological) order.
    let mut day_keys: Vec<i64> = buckets.keys().copied().collect();
    day_keys.sort();

    let mut clusters: Vec<Cluster> = Vec::new();

    for day in day_keys {
        let indices = &buckets[&day];
        if indices.len() < config.cluster_min_size {
            // Whole bucket is too small to ever produce a cluster.
            continue;
        }

        // ----- 3+4. Similarity edges + union-find -----
        let mut uf = UnionFind::new(indices.len());
        for a_pos in 0..indices.len() {
            for b_pos in (a_pos + 1)..indices.len() {
                let a = indices[a_pos];
                let b = indices[b_pos];
                let sim = cosine_similarity_f32(f32_views[a], f32_views[b]);
                if sim >= config.cluster_cosine_threshold {
                    uf.union(a_pos, b_pos);
                }
            }
        }

        // Group bucket-local positions by their root.
        let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
        for pos in 0..indices.len() {
            let root = uf.find(pos);
            groups.entry(root).or_default().push(pos);
        }

        // ----- 5. Size filter + 6. Build Cluster -----
        for (_root, members_pos) in groups {
            if members_pos.len() < config.cluster_min_size {
                continue;
            }
            for global_indices in split_component_by_time(&members_pos, indices, inputs, config) {
                clusters.push(build_cluster(&global_indices, inputs, &f32_views, dim));
            }
        }
    }

    // ----- 7. Sort clusters by their earliest episode timestamp -----
    clusters.sort_by_key(|c| {
        // Each cluster's `episode_ids` is sorted by (ts_ms, memory_id);
        // we kept the original episode_ids vec but lost the ts_ms map.
        // Re-derive the min ts_ms by looking up each member's Episode.
        let memid_set: std::collections::HashSet<MemoryId> =
            c.episode_ids.iter().copied().collect();
        inputs
            .iter()
            .filter(|(ep, _)| memid_set.contains(&ep.memory_id))
            .map(|(ep, _)| ep.ts_ms)
            .min()
            .unwrap_or(i64::MAX)
    });

    Ok(clusters)
}

fn split_component_by_time(
    members_pos: &[usize],
    bucket_indices: &[usize],
    inputs: &[(Episode, Embedding)],
    config: &StewardConfig,
) -> Vec<Vec<usize>> {
    let max_size = config.cluster_max_size.max(config.cluster_min_size);
    let mut global_indices: Vec<usize> = members_pos.iter().map(|&p| bucket_indices[p]).collect();
    global_indices.sort_by_key(|&i| {
        let ep = &inputs[i].0;
        (ep.ts_ms, ep.memory_id)
    });
    global_indices
        .chunks(max_size)
        .filter(|chunk| chunk.len() >= config.cluster_min_size)
        .map(|chunk| chunk.to_vec())
        .collect()
}

/// Build a [`Cluster`] from the resolved global indices for one
/// connected component. Computes centroid + coherence in a single pass.
fn build_cluster(
    global_indices: &[usize],
    inputs: &[(Episode, Embedding)],
    f32_views: &[&[f32]],
    dim: usize,
) -> Cluster {
    debug_assert!(!global_indices.is_empty());

    // ----- episode_ids sorted by (ts_ms, memory_id) for determinism -----
    let mut sorted: Vec<usize> = global_indices.to_vec();
    sorted.sort_by_key(|&i| {
        let ep = &inputs[i].0;
        (ep.ts_ms, ep.memory_id)
    });
    let episode_ids: Vec<MemoryId> = sorted.iter().map(|&i| inputs[i].0.memory_id).collect();

    // ----- centroid: mean of unit-normalised vectors, then re-normalise -----
    let mut sum = vec![0.0f32; dim];
    for &i in &sorted {
        add_normalized_weighted(&mut sum, f32_views[i], 1.0);
    }
    let n = sorted.len() as f32;
    for v in sum.iter_mut() {
        *v /= n;
    }
    normalize_in_place(&mut sum);
    // Reconstruct an Embedding from the centroid bytes. Same dim/dtype
    // as the inputs.
    let centroid_bytes: Vec<u8> = bytemuck::cast_slice(&sum).to_vec();
    let centroid = Embedding {
        dtype: EmbeddingDtype::F32,
        dim,
        data: centroid_bytes,
    };

    // ----- coherence: average pairwise cosine across all C(n,2) pairs -----
    let mut total = 0.0f32;
    let mut pairs = 0u32;
    for a in 0..sorted.len() {
        for b in (a + 1)..sorted.len() {
            total += cosine_similarity_f32(f32_views[sorted[a]], f32_views[sorted[b]]);
            pairs += 1;
        }
    }
    let coherence = if pairs > 0 { total / pairs as f32 } else { 1.0 };

    Cluster {
        cluster_id: MemoryId::new(),
        episode_ids,
        centroid: Some(centroid),
        coherence,
    }
}

/// Cosine similarity for two F32 vectors. This computes true cosine
/// instead of assuming unit-normalized embeddings, so thresholding stays
/// correct for custom embedders. Zero vectors have no direction and return
/// 0.0 rather than NaN.
fn cosine_similarity_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom <= f32::EPSILON {
        0.0
    } else {
        (dot / denom).clamp(-1.0, 1.0)
    }
}

fn add_normalized_weighted(sum: &mut [f32], v: &[f32], weight: f32) {
    debug_assert_eq!(sum.len(), v.len());
    if weight <= 0.0 {
        return;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return;
    }
    for (acc, &x) in sum.iter_mut().zip(v.iter()) {
        *acc += (x / norm) * weight;
    }
}

fn normalize_in_place(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > f32::EPSILON {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// ---------------------------------------------------------------------------
// Re-consolidation: post-cluster centroid merge
// ---------------------------------------------------------------------------

/// One absorption decision: a `survivor_id` cluster takes over the
/// episodes of all clusters in `loser_ids`. The merged centroid +
/// coherence reflect the post-merge weighted state.
///
/// Used by both:
///   - The in-memory mutation path
///     ([`merge_clusters_by_centroid`]) which applies the plan to a
///     `Vec<Cluster>` so the persistence layer downstream sees the
///     merged shape.
///   - The DB-application path ([`plan_existing_merges`]) which
///     hands the plan to the storage layer to translate into
///     `UPDATE cluster_episodes` + `DELETE FROM clusters` operations
///     against pre-existing rows.
#[derive(Debug, Clone)]
pub struct MergeOp {
    pub survivor_id: MemoryId,
    pub loser_ids: Vec<MemoryId>,
    pub merged_episode_ids: Vec<MemoryId>,
    pub merged_centroid: Embedding,
    pub merged_coherence: f32,
}

/// Result of [`plan_existing_merges`]. Empty when no clusters meet
/// the merge threshold.
#[derive(Debug, Clone, Default)]
pub struct MergePlan {
    pub merges: Vec<MergeOp>,
}

impl MergePlan {
    /// Total clusters absorbed (= sum of `loser_ids.len()` across
    /// every merge in the plan). Equivalent to the count returned
    /// by [`merge_clusters_by_centroid`].
    pub fn absorbed(&self) -> usize {
        self.merges.iter().map(|m| m.loser_ids.len()).sum()
    }
}

/// Plan-only variant: build a [`MergePlan`] without mutating the
/// input. Used for **existing-vs-existing** cluster merging where
/// the storage layer needs to translate the plan into
/// `UPDATE cluster_episodes` + `DELETE FROM clusters` SQL —
/// in-memory mutation isn't useful since the truth-of-record is
/// in SQLite.
///
/// Same algorithm as [`merge_clusters_by_centroid`]: pairwise
/// centroid cosine + union-find + survivor-selection (most
/// episodes wins, tie-break: smallest `cluster_id` lex).
pub fn plan_existing_merges(clusters: &[Cluster], config: &StewardConfig) -> Result<MergePlan> {
    let merges = compute_merge_plan(clusters, config)?;
    Ok(MergePlan { merges })
}

/// Merge clusters whose centroids are above `cluster_cosine_threshold`
/// in cosine similarity. Operates on `clusters` in place.
///
/// **Closes the cross-bucket case** in [`cluster_episodes`]: episodes
/// whose `ts_ms` straddles UTC midnight land in different day buckets
/// and form separate clusters even when their content is essentially
/// the same conversation. The merge pass here detects that via
/// pairwise centroid cosine and folds the smaller cluster into the
/// larger.
///
/// ## Algorithm
///
///   1. Pairwise cosine over centroids; union-find to group merge sets.
///   2. For each group with > 1 cluster: pick the **survivor** (most
///      episodes, tie-break: lexicographically smallest `cluster_id` —
///      stable across runs).
///   3. Merged `episode_ids` = union, sorted by `MemoryId` for
///      deterministic ordering.
///   4. Merged `centroid` = episode-count-weighted mean of input
///      centroids, re-normalised to unit length.
///   5. Merged `coherence` = episode-count-weighted average of input
///      coherences. **Approximation**: the true definition is the
///      mean pairwise within-cluster cosine, which would require
///      reloading all member embeddings. The weighted-input estimate
///      is a strict overestimate (it ignores cross-pair similarity
///      between merged clusters, which is necessarily ≥ threshold)
///      and is monotonic with cluster size — close enough for the
///      coherence field to remain a useful sort key.
///
/// ## What's not here
///
///   - **Cross-run merge.** Today this only sees freshly-clustered
///     output from `cluster_episodes`. It does NOT pull existing
///     clusters from DB. Cross-run merging is a strictly bigger
///     change requiring abstraction-regeneration plumbing in the
///     storage layer.
///
///   - **Separate merge threshold.** Reuses
///     `cluster_cosine_threshold`. If the within-cluster threshold
///     proves too tight for centroid similarity, a future commit
///     can add `merge_centroid_threshold` to `StewardConfig`.
///
/// Returns the number of clusters that were absorbed (i.e. the
/// drop in `clusters.len()` after the merge). 0 = no merges
/// performed.
pub fn merge_clusters_by_centroid(
    clusters: &mut Vec<Cluster>,
    config: &StewardConfig,
) -> Result<usize> {
    let merges = compute_merge_plan(clusters, config)?;
    if merges.is_empty() {
        return Ok(0);
    }
    apply_merge_plan_in_place(clusters, &merges);
    Ok(merges.iter().map(|m| m.loser_ids.len()).sum())
}

/// Apply a list of [`MergeOp`]s to `clusters` in place. Survivors
/// keep their position in the Vec (their fields update); losers are
/// removed; clusters not mentioned in the plan pass through
/// unchanged.
fn apply_merge_plan_in_place(clusters: &mut Vec<Cluster>, merges: &[MergeOp]) {
    use std::collections::HashSet;
    let losers: HashSet<MemoryId> = merges
        .iter()
        .flat_map(|m| m.loser_ids.iter().copied())
        .collect();
    let by_survivor: HashMap<MemoryId, &MergeOp> =
        merges.iter().map(|m| (m.survivor_id, m)).collect();
    let mut out: Vec<Cluster> = Vec::with_capacity(clusters.len());
    for c in clusters.iter() {
        if losers.contains(&c.cluster_id) {
            continue;
        }
        if let Some(op) = by_survivor.get(&c.cluster_id) {
            out.push(Cluster {
                cluster_id: op.survivor_id,
                episode_ids: op.merged_episode_ids.clone(),
                centroid: Some(op.merged_centroid.clone()),
                coherence: op.merged_coherence,
            });
        } else {
            out.push(c.clone());
        }
    }
    *clusters = out;
}

/// Pure planner shared by [`merge_clusters_by_centroid`] (in-memory
/// mutation) and [`plan_existing_merges`] (DB application). Validates
/// inputs, runs union-find on pairwise centroid cosine, builds one
/// [`MergeOp`] per multi-cluster group. Single-cluster groups produce
/// no entries.
fn compute_merge_plan(clusters: &[Cluster], config: &StewardConfig) -> Result<Vec<MergeOp>> {
    if clusters.len() < 2 {
        return Ok(Vec::new());
    }
    // Validate centroid availability + dim/dtype consistency. Any
    // cluster without a centroid (shouldn't happen post-build, but
    // defensive) cannot participate in cosine merging — bail with
    // a recoverable error.
    let dim = match clusters[0].centroid.as_ref() {
        Some(c) => c.dim,
        None => {
            return Err(Error::steward(
                "compute_merge_plan: cluster[0] has no centroid".to_string(),
            ));
        }
    };
    for (i, c) in clusters.iter().enumerate() {
        let centroid = c.centroid.as_ref().ok_or_else(|| {
            Error::steward(format!("compute_merge_plan: cluster[{i}] has no centroid"))
        })?;
        if centroid.dtype != EmbeddingDtype::F32 {
            return Err(Error::steward(format!(
                "compute_merge_plan: cluster[{i}] centroid dtype is {:?}, want F32",
                centroid.dtype
            )));
        }
        if centroid.dim != dim {
            return Err(Error::steward(format!(
                "compute_merge_plan: cluster[{i}] centroid dim {} != {dim}",
                centroid.dim
            )));
        }
    }

    let centroid_views: Vec<&[f32]> = clusters
        .iter()
        .enumerate()
        .map(|(i, c)| {
            c.centroid.as_ref().unwrap().as_f32_slice().ok_or_else(|| {
                Error::steward(format!(
                    "compute_merge_plan: cluster[{i}] centroid F32 cast failed"
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // ----- Pairwise + union-find -----
    let n = clusters.len();
    let mut uf = UnionFind::new(n);
    for a in 0..n {
        for b in (a + 1)..n {
            let sim = cosine_similarity_f32(centroid_views[a], centroid_views[b]);
            if sim >= config.cluster_cosine_threshold {
                uf.union(a, b);
            }
        }
    }

    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        groups.entry(root).or_default().push(i);
    }
    let mut roots: Vec<usize> = groups.keys().copied().collect();
    roots.sort();

    // ----- Build one MergeOp per multi-cluster group -----
    let mut merges: Vec<MergeOp> = Vec::new();
    let max_size = config.cluster_max_size.max(config.cluster_min_size);
    for root in roots {
        let members = &groups[&root];
        if members.len() == 1 {
            continue;
        }
        let merged_episode_count: usize =
            members.iter().map(|&m| clusters[m].episode_ids.len()).sum();
        if merged_episode_count > max_size {
            continue;
        }

        // Survivor selection: most episodes, tie-break: smallest
        // cluster_id lex.
        let survivor_pos = *members
            .iter()
            .max_by(|&&a, &&b| {
                let len_a = clusters[a].episode_ids.len();
                let len_b = clusters[b].episode_ids.len();
                len_a
                    .cmp(&len_b)
                    .then_with(|| clusters[b].cluster_id.cmp(&clusters[a].cluster_id))
            })
            .expect("members non-empty");
        let survivor_id = clusters[survivor_pos].cluster_id;
        let loser_ids: Vec<MemoryId> = members
            .iter()
            .filter(|&&m| m != survivor_pos)
            .map(|&m| clusters[m].cluster_id)
            .collect();

        // Union episode_ids, sort by MemoryId for determinism.
        let mut merged_episode_ids: Vec<MemoryId> = members
            .iter()
            .flat_map(|&m| clusters[m].episode_ids.iter().copied())
            .collect();
        merged_episode_ids.sort();
        merged_episode_ids.dedup();

        // Weighted centroid.
        let mut sum = vec![0.0f32; dim];
        let mut total_weight: f32 = 0.0;
        for &m in members {
            let w = clusters[m].episode_ids.len() as f32;
            add_normalized_weighted(&mut sum, centroid_views[m], w);
            total_weight += w;
        }
        if total_weight > 0.0 {
            for v in sum.iter_mut() {
                *v /= total_weight;
            }
        }
        normalize_in_place(&mut sum);
        let centroid_bytes: Vec<u8> = bytemuck::cast_slice(&sum).to_vec();
        let merged_centroid = Embedding {
            dtype: EmbeddingDtype::F32,
            dim,
            data: centroid_bytes,
        };

        // Weighted coherence.
        let mut coh_sum = 0.0f32;
        let mut coh_weight = 0.0f32;
        for &m in members {
            let w = clusters[m].episode_ids.len() as f32;
            coh_sum += clusters[m].coherence * w;
            coh_weight += w;
        }
        let merged_coherence = if coh_weight > 0.0 {
            coh_sum / coh_weight
        } else {
            1.0
        };

        merges.push(MergeOp {
            survivor_id,
            loser_ids,
            merged_episode_ids,
            merged_centroid,
            merged_coherence,
        });
    }

    Ok(merges)
}

// ---------------------------------------------------------------------------
// Re-consolidation: cross-run absorb-into-existing
// ---------------------------------------------------------------------------

/// Compact summary of a pre-existing cluster, sufficient for the
/// absorb decision without loading every episode row. The storage
/// layer builds these from a single SELECT against `clusters` +
/// `COUNT(cluster_episodes)`.
#[derive(Debug, Clone)]
pub struct ExistingClusterSummary {
    pub cluster_id: MemoryId,
    pub centroid: Embedding,
    pub coherence: f32,
    pub episode_count: usize,
}

/// One absorption: a freshly-built cluster (`new_cluster_id`) was
/// folded into an existing DB cluster (`existing_cluster_id`).
/// Caller's storage logic uses this to:
///
///   - INSERT `cluster_episodes` rows linking the new cluster's
///     episodes under `existing_cluster_id` (NOT under
///     `new_cluster_id`).
///   - Skip the INSERT into `clusters` for `new_cluster_id` — it
///     never gets a row of its own.
///   - UPDATE the existing cluster's centroid + coherence to
///     `merged_centroid` / `merged_coherence`.
///   - DELETE the existing cluster's `semantic_abstractions` (now
///     stale) so the missing-abstraction regen pass can rebuild it.
#[derive(Debug, Clone)]
pub struct AbsorbedCluster {
    pub new_cluster_id: MemoryId,
    pub existing_cluster_id: MemoryId,
    pub merged_centroid: Embedding,
    pub merged_coherence: f32,
    /// Episode_ids from the new cluster only — caller links these
    /// under `existing_cluster_id`. Pre-existing episodes for the
    /// existing cluster don't appear here (they're already linked).
    pub absorbed_episode_ids: Vec<MemoryId>,
}

/// Output of [`absorb_into_existing`].
#[derive(Debug, Clone, Default)]
pub struct AbsorbPlan {
    /// Each new-cluster-into-existing-cluster decision.
    pub absorptions: Vec<AbsorbedCluster>,
}

impl AbsorbPlan {
    /// Was this freshly-built cluster absorbed into an existing one?
    pub fn was_absorbed(&self, new_cluster_id: MemoryId) -> bool {
        self.absorptions
            .iter()
            .any(|a| a.new_cluster_id == new_cluster_id)
    }

    /// Distinct existing cluster_ids whose centroid + coherence
    /// changed (i.e. they absorbed at least one new cluster).
    /// Their `semantic_abstractions` row is stale and the storage
    /// layer should DELETE it for re-generation.
    pub fn modified_existing_ids(&self) -> Vec<MemoryId> {
        let mut ids: Vec<MemoryId> = self
            .absorptions
            .iter()
            .map(|a| a.existing_cluster_id)
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

/// Decide which freshly-built clusters should fold into a pre-existing
/// DB cluster instead of being written as standalone clusters.
///
/// **Closes the cross-run case**: yesterday's "pasta" cluster +
/// today's new "pasta" cluster (which would have formed standalone
/// today, since the v0.2 `NOT IN cluster_episodes` guard hides
/// yesterday's episodes from the candidate set) → today's new
/// cluster gets absorbed into yesterday's, its episodes link under
/// the existing `cluster_id`, and the existing cluster's centroid
/// is refreshed.
///
/// ## Algorithm
///
/// For each freshly-built cluster `N`:
///
///   1. Compute centroid cosine against every `existing` cluster.
///   2. If the best match is ≥ `config.cluster_cosine_threshold`,
///      absorb `N` into the matched existing cluster `E`.
///      Tie-break across multiple ≥-threshold matches: pick the
///      existing cluster with the **most episodes** (tie-tie:
///      smallest `cluster_id` lex). This matches the in-run
///      [`merge_clusters_by_centroid`] survivor rule.
///   3. Compute the post-absorb centroid + coherence using the
///      same episode-count-weighted formula as
///      [`merge_clusters_by_centroid`].
///
/// Each `existing` cluster can absorb **multiple** freshly-built
/// clusters in the same call. The merged centroid for an
/// existing cluster that absorbs both N1 and N2 is computed
/// step-wise: E + N1 → E', then E' + N2 → E''. Order is the
/// `new_clusters` Vec's existing order (which is already
/// `min ts_ms`-sorted from `cluster_episodes`).
///
/// ## What's not here
///
/// - **Existing-to-existing merging.** Two pre-existing clusters
///   that drift toward each other over time stay separate. This
///   is a slow-burn problem; existing-merge is its own iteration.
///
/// - **`cluster_min_size` re-check.** The post-absorb cluster
///   trivially satisfies it (existing ≥ min_size + new ≥
///   min_size). No re-validation.
///
/// Returns the [`AbsorbPlan`]. The caller mutates `new_clusters`
/// based on the plan: skip persisting absorbed clusters, link
/// their episodes under the existing cluster_id, refresh existing
/// rows, drop stale abstractions.
pub fn absorb_into_existing(
    new_clusters: &[Cluster],
    existing: &[ExistingClusterSummary],
    config: &StewardConfig,
) -> Result<AbsorbPlan> {
    if new_clusters.is_empty() || existing.is_empty() {
        return Ok(AbsorbPlan::default());
    }

    // Validate dim/dtype on first existing summary; reuse for the
    // rest. Skip new_clusters validation — assumed to come from
    // `cluster_episodes` / `merge_clusters_by_centroid`, which
    // already enforce F32 + dim consistency.
    let dim = existing[0].centroid.dim;
    for (i, s) in existing.iter().enumerate() {
        if s.centroid.dtype != EmbeddingDtype::F32 {
            return Err(Error::steward(format!(
                "absorb_into_existing: existing[{i}] dtype is {:?}, want F32",
                s.centroid.dtype
            )));
        }
        if s.centroid.dim != dim {
            return Err(Error::steward(format!(
                "absorb_into_existing: existing[{i}] dim {} != {dim}",
                s.centroid.dim
            )));
        }
    }
    if let Some(first_new) = new_clusters.first() {
        let new_centroid = first_new.centroid.as_ref().ok_or_else(|| {
            Error::steward("absorb_into_existing: new_clusters[0] has no centroid".to_string())
        })?;
        if new_centroid.dim != dim {
            return Err(Error::steward(format!(
                "absorb_into_existing: new_clusters[0] dim {} != existing dim {dim}",
                new_centroid.dim
            )));
        }
    }

    // Mutable working copy of existing centroids/counts/coherence —
    // when one existing absorbs multiple new clusters, later
    // absorbs see the updated state.
    let mut working: Vec<(MemoryId, Vec<f32>, f32, usize)> = existing
        .iter()
        .map(|s| {
            let v = s
                .centroid
                .as_f32_slice()
                .ok_or_else(|| {
                    Error::steward(format!(
                        "absorb_into_existing: existing[{}] centroid F32 cast failed",
                        s.cluster_id
                    ))
                })
                .map(|sl| sl.to_vec());
            v.map(|vec_f32| (s.cluster_id, vec_f32, s.coherence, s.episode_count))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut plan = AbsorbPlan::default();
    let max_size = config.cluster_max_size.max(config.cluster_min_size);

    for n in new_clusters {
        let n_centroid = n
            .centroid
            .as_ref()
            .ok_or_else(|| {
                Error::steward(format!(
                    "absorb_into_existing: new cluster {} has no centroid",
                    n.cluster_id
                ))
            })?
            .as_f32_slice()
            .ok_or_else(|| {
                Error::steward(format!(
                    "absorb_into_existing: new cluster {} centroid F32 cast failed",
                    n.cluster_id
                ))
            })?;

        // Find best existing match above threshold.
        let mut best: Option<usize> = None;
        let mut best_sim = config.cluster_cosine_threshold;
        for (i, (_id, e_centroid, _coh, count)) in working.iter().enumerate() {
            if *count + n.episode_ids.len() > max_size {
                continue;
            }
            let sim = cosine_similarity_f32(n_centroid, e_centroid);
            if sim >= best_sim {
                // ≥ to allow first match at exactly the threshold;
                // tie-break is "most episodes, smaller cluster_id".
                let take = match best {
                    None => true,
                    Some(prev) => {
                        let prev_count = working[prev].3;
                        let cur_count = working[i].3;
                        match cur_count.cmp(&prev_count) {
                            std::cmp::Ordering::Greater => true,
                            std::cmp::Ordering::Less => false,
                            std::cmp::Ordering::Equal => {
                                // Smaller cluster_id wins.
                                working[i].0 < working[prev].0
                            }
                        }
                    }
                };
                if take {
                    best = Some(i);
                    best_sim = sim;
                }
            }
        }

        let target_idx = match best {
            Some(i) => i,
            None => continue, // No existing absorbs this new cluster.
        };

        // Compute weighted-mean centroid + weighted-avg coherence.
        let (existing_id, ref mut e_centroid, ref mut e_coh, ref mut e_count) = working[target_idx];
        let n_w = n.episode_ids.len() as f32;
        let e_w = *e_count as f32;
        let total_w = n_w + e_w;
        let mut sum = vec![0.0f32; dim];
        if total_w > 0.0 {
            add_normalized_weighted(&mut sum, e_centroid, e_w);
            add_normalized_weighted(&mut sum, n_centroid, n_w);
            for v in sum.iter_mut() {
                *v /= total_w;
            }
        }
        normalize_in_place(&mut sum);
        let merged_coh = (*e_coh * e_w + n.coherence * n_w) / total_w.max(1.0);

        // Update working state so subsequent absorbs into the same
        // existing cluster see the fresher centroid/coherence/count.
        *e_centroid = sum.clone();
        *e_coh = merged_coh;
        *e_count += n.episode_ids.len();

        let centroid_bytes: Vec<u8> = bytemuck::cast_slice(&sum).to_vec();
        let merged_centroid = Embedding {
            dtype: EmbeddingDtype::F32,
            dim,
            data: centroid_bytes,
        };

        plan.absorptions.push(AbsorbedCluster {
            new_cluster_id: n.cluster_id,
            existing_cluster_id: existing_id,
            merged_centroid,
            merged_coherence: merged_coh,
            absorbed_episode_ids: n.episode_ids.clone(),
        });
    }

    Ok(plan)
}

// ---------------------------------------------------------------------------
// Union-find
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        // Path compression.
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        // Union by rank.
        let (small, big) = if self.rank[ra] < self.rank[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        if self.rank[small] == self.rank[big] {
            self.rank[big] += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use solo_core::{Confidence, EncodingContext, Tier};

    /// Build a unit-norm F32 embedding from a sparse list of (index,
    /// value) pairs. Convenient for hand-written deterministic test
    /// vectors.
    fn unit_emb(dim: usize, components: &[(usize, f32)]) -> Embedding {
        let mut v = vec![0.0f32; dim];
        for &(i, x) in components {
            v[i] = x;
        }
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
        Embedding {
            dtype: EmbeddingDtype::F32,
            dim,
            data: bytemuck::cast_slice(&v).to_vec(),
        }
    }

    fn raw_emb(dim: usize, components: &[(usize, f32)]) -> Embedding {
        let mut v = vec![0.0f32; dim];
        for &(i, x) in components {
            v[i] = x;
        }
        Embedding {
            dtype: EmbeddingDtype::F32,
            dim,
            data: bytemuck::cast_slice(&v).to_vec(),
        }
    }

    fn ep(ts_ms: i64, content: &str) -> Episode {
        Episode {
            memory_id: MemoryId::new(),
            ts_ms,
            source_type: "user_message".into(),
            source_id: None,
            content: content.into(),
            encoding_context: EncodingContext::default(),
            provenance: None,
            confidence: Confidence::new(0.9).unwrap(),
            strength: 0.5,
            salience: 0.5,
            tier: Tier::Hot,
        }
    }

    fn cfg(min: usize, threshold: f32) -> StewardConfig {
        StewardConfig {
            cluster_min_size: min,
            cluster_cosine_threshold: threshold,
            ..StewardConfig::default()
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        let r = cluster_episodes(&[], &StewardConfig::default()).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn cluster_min_size_one_emits_singleton_clusters() {
        let day_a = 1_700_000_000_000i64;
        let inputs = vec![
            (ep(day_a, "a"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 1000, "b"), unit_emb(4, &[(1, 1.0)])),
        ];

        let r = cluster_episodes(&inputs, &cfg(1, 0.95)).unwrap();

        assert_eq!(r.len(), 2);
        assert_eq!(
            r.iter().map(|c| c.episode_ids.len()).collect::<Vec<_>>(),
            vec![1, 1]
        );
        assert!(r.iter().all(|c| c.coherence == 1.0));
    }

    #[test]
    fn rejects_non_f32_embedding() {
        let bad = Embedding {
            dtype: EmbeddingDtype::F16,
            dim: 4,
            data: vec![0u8; 8],
        };
        let inputs = vec![(ep(0, "x"), bad)];
        let err = cluster_episodes(&inputs, &StewardConfig::default()).unwrap_err();
        assert!(err.to_string().contains("F32"), "got: {err}");
    }

    #[test]
    fn rejects_dim_mismatch() {
        let inputs = vec![
            (ep(0, "a"), unit_emb(4, &[(0, 1.0)])),
            (ep(0, "b"), unit_emb(8, &[(0, 1.0)])),
        ];
        let err = cluster_episodes(&inputs, &StewardConfig::default()).unwrap_err();
        assert!(err.to_string().contains("dim"), "got: {err}");
    }

    #[test]
    fn cosine_uses_vector_direction_not_magnitude() {
        let day_a = 1_700_000_000_000i64;
        let inputs = vec![
            (ep(day_a, "a1"), raw_emb(4, &[(0, 10.0)])),
            (ep(day_a + 1000, "a2"), raw_emb(4, &[(0, 20.0)])),
            (ep(day_a + 2000, "orthogonal"), raw_emb(4, &[(1, 50.0)])),
        ];

        let r = cluster_episodes(&inputs, &cfg(2, 0.9)).unwrap();

        assert_eq!(r.len(), 1);
        assert_eq!(r[0].episode_ids.len(), 2);
        assert!(
            !r[0].episode_ids.contains(&inputs[2].0.memory_id),
            "scaled orthogonal vector must not pass the cosine threshold"
        );
        assert!(
            r[0].coherence <= 1.0,
            "true cosine coherence must stay bounded, got {}",
            r[0].coherence
        );
    }

    #[test]
    fn zero_vector_has_zero_similarity_instead_of_nan() {
        let day_a = 1_700_000_000_000i64;
        let inputs = vec![
            (ep(day_a, "zero"), raw_emb(4, &[])),
            (ep(day_a + 1000, "x"), raw_emb(4, &[(0, 1.0)])),
        ];

        let r = cluster_episodes(&inputs, &cfg(2, 0.1)).unwrap();

        assert!(r.is_empty(), "zero vector must not cluster by accident");
    }

    #[test]
    fn oversized_component_is_split_into_bounded_time_chunks() {
        let day_a = 1_700_000_000_000i64;
        let inputs: Vec<(Episode, Embedding)> = (0..8)
            .map(|i| {
                (
                    ep(day_a + i as i64 * 1000, &format!("same-topic-{i}")),
                    raw_emb(4, &[(0, 10.0)]),
                )
            })
            .collect();
        let mut config = cfg(2, 0.9);
        config.cluster_max_size = 3;

        let r = cluster_episodes(&inputs, &config).unwrap();

        assert_eq!(r.len(), 3);
        assert_eq!(
            r.iter().map(|c| c.episode_ids.len()).collect::<Vec<_>>(),
            vec![3, 3, 2]
        );
        assert!(
            r.iter()
                .all(|c| c.episode_ids.len() <= config.cluster_max_size),
            "every emitted cluster should respect cluster_max_size"
        );
        let total_members: usize = r.iter().map(|c| c.episode_ids.len()).sum();
        assert_eq!(total_members, inputs.len());
    }

    /// Three near-identical vectors in the same day → one cluster of 3
    /// with high coherence. A different vector in the same bucket is
    /// excluded (no edges to anyone).
    #[test]
    fn one_cluster_three_similar_one_outlier() {
        let day_a = 1_700_000_000_000i64; // arbitrary epoch ms
        let inputs = vec![
            (ep(day_a, "a1"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 1000, "a2"), unit_emb(4, &[(0, 0.99), (1, 0.01)])),
            (ep(day_a + 2000, "a3"), unit_emb(4, &[(0, 0.98), (1, 0.02)])),
            (ep(day_a + 3000, "outlier"), unit_emb(4, &[(2, 1.0)])),
        ];
        let r = cluster_episodes(&inputs, &cfg(3, 0.85)).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].episode_ids.len(), 3);
        assert!(r[0].coherence > 0.95, "coherence: {}", r[0].coherence);
        // Outlier's id NOT in any cluster.
        let outlier_id = inputs[3].0.memory_id;
        for c in &r {
            assert!(!c.episode_ids.contains(&outlier_id));
        }
    }

    /// Below the size threshold → no cluster.
    #[test]
    fn below_min_size_yields_no_cluster() {
        let day_a = 1_700_000_000_000i64;
        let inputs = vec![
            (ep(day_a, "a1"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 1000, "a2"), unit_emb(4, &[(0, 1.0)])),
        ];
        let r = cluster_episodes(&inputs, &cfg(3, 0.85)).unwrap();
        assert!(r.is_empty(), "expected no cluster: {r:?}");
    }

    /// Same content on two different UTC days → two separate buckets,
    /// no cross-bucket clustering, no clusters formed (each bucket
    /// has only 1 episode).
    #[test]
    fn same_content_different_days_dont_cluster() {
        let day_a = 1_700_000_000_000i64;
        let day_b = day_a + MS_PER_DAY * 3;
        let inputs = vec![
            (ep(day_a, "today"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_b, "three days later"), unit_emb(4, &[(0, 1.0)])),
        ];
        let r = cluster_episodes(&inputs, &cfg(2, 0.85)).unwrap();
        assert!(r.is_empty(), "should NOT cross-bucket: {r:?}");
    }

    /// Two distinct themes within a single day → two clusters,
    /// coherence reflects within-cluster similarity.
    #[test]
    fn two_clusters_per_bucket_when_two_themes() {
        let day_a = 1_700_000_000_000i64;
        let inputs = vec![
            // Theme A: dim 0
            (ep(day_a, "a1"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 1000, "a2"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 2000, "a3"), unit_emb(4, &[(0, 1.0)])),
            // Theme B: dim 1
            (ep(day_a + 3000, "b1"), unit_emb(4, &[(1, 1.0)])),
            (ep(day_a + 4000, "b2"), unit_emb(4, &[(1, 1.0)])),
            (ep(day_a + 5000, "b3"), unit_emb(4, &[(1, 1.0)])),
        ];
        let r = cluster_episodes(&inputs, &cfg(3, 0.85)).unwrap();
        assert_eq!(r.len(), 2);
        // Each cluster has exactly 3 episodes; total covers all 6.
        let total: usize = r.iter().map(|c| c.episode_ids.len()).sum();
        assert_eq!(total, 6);
        // Sorted by min ts_ms — A first (starts day_a), then B (starts
        // day_a+3000).
        assert!(r[0].episode_ids[0] == inputs[0].0.memory_id);
        assert!(r[1].episode_ids[0] == inputs[3].0.memory_id);
    }

    /// Transitive grouping: A↔B and B↔C below threshold pairwise but
    /// transitively-connected via union-find.
    #[test]
    fn transitive_cluster_via_union_find() {
        let day_a = 1_700_000_000_000i64;
        // A, B, C arranged so A·B and B·C are above threshold, but
        // A·C may be slightly below — union-find still groups all 3.
        let a = unit_emb(4, &[(0, 1.0)]);
        let b = unit_emb(4, &[(0, 0.93), (1, 0.37)]); // tilted toward dim 1
        let c = unit_emb(4, &[(1, 1.0)]);
        // A·B ≈ 0.93, B·C ≈ 0.37 — pick a threshold that allows A↔B
        // and we'll verify the algorithm still works correctly even
        // when transitive coupling is weaker than direct similarity.
        let inputs = vec![
            (ep(day_a, "a"), a),
            (ep(day_a + 1000, "b"), b),
            (ep(day_a + 2000, "c"), c),
        ];
        // Threshold 0.9 → only A↔B is an edge. C is isolated.
        // Cluster {A,B} has size 2 → below min_size 3 → no cluster.
        let r = cluster_episodes(&inputs, &cfg(3, 0.9)).unwrap();
        assert!(r.is_empty(), "no cluster expected at threshold 0.9");

        // Lower threshold to 0.3 → A·B (0.93), B·C (0.37) both edges,
        // A·C (0.0) not — transitive group of 3. min_size 3 → 1 cluster.
        let r2 = cluster_episodes(
            &[
                (ep(day_a, "a"), unit_emb(4, &[(0, 1.0)])),
                (ep(day_a + 1000, "b"), unit_emb(4, &[(0, 0.93), (1, 0.37)])),
                (ep(day_a + 2000, "c"), unit_emb(4, &[(1, 1.0)])),
            ],
            &cfg(3, 0.3),
        )
        .unwrap();
        assert_eq!(r2.len(), 1);
        assert_eq!(r2[0].episode_ids.len(), 3);
    }

    /// Determinism: the same input produces the same cluster shapes
    /// (episode_id sets) across re-runs. cluster_id varies (UUID v7
    /// is time+random) but the structural output doesn't.
    #[test]
    fn output_is_deterministic_modulo_cluster_id() {
        let day_a = 1_700_000_000_000i64;
        // Build inputs once, clone the tuples for each pass so we
        // test shape stability with the SAME memory_ids on each run.
        let inputs: Vec<(Episode, Embedding)> = vec![
            (ep(day_a, "a1"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 1000, "a2"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 2000, "a3"), unit_emb(4, &[(0, 1.0)])),
            (ep(day_a + 3000, "b1"), unit_emb(4, &[(1, 1.0)])),
            (ep(day_a + 4000, "b2"), unit_emb(4, &[(1, 1.0)])),
            (ep(day_a + 5000, "b3"), unit_emb(4, &[(1, 1.0)])),
        ];
        let r1 = cluster_episodes(&inputs, &cfg(3, 0.85)).unwrap();
        let r2 = cluster_episodes(&inputs, &cfg(3, 0.85)).unwrap();
        assert_eq!(r1.len(), r2.len());
        for (a, b) in r1.iter().zip(r2.iter()) {
            assert_eq!(a.episode_ids, b.episode_ids);
            // Coherence is a deterministic float computation — bit-
            // exact equality holds.
            assert_eq!(a.coherence.to_bits(), b.coherence.to_bits());
        }
    }

    // -----------------------------------------------------------------
    // merge_clusters_by_centroid
    // -----------------------------------------------------------------

    /// Build a Cluster directly for merge tests — bypasses
    /// `cluster_episodes` so we can hand-craft centroids.
    fn cluster_with(
        episode_ids: Vec<MemoryId>,
        centroid_components: &[(usize, f32)],
        coherence: f32,
        dim: usize,
    ) -> Cluster {
        Cluster {
            cluster_id: MemoryId::new(),
            episode_ids,
            centroid: Some(unit_emb(dim, centroid_components)),
            coherence,
        }
    }

    #[test]
    fn merge_empty_or_singleton_is_noop() {
        let mut empty: Vec<Cluster> = Vec::new();
        let n = merge_clusters_by_centroid(&mut empty, &cfg(3, 0.85)).unwrap();
        assert_eq!(n, 0);
        assert!(empty.is_empty());

        let mut one = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 1.0)],
            0.95,
            4,
        )];
        let n = merge_clusters_by_centroid(&mut one, &cfg(3, 0.85)).unwrap();
        assert_eq!(n, 0);
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn merge_unrelated_clusters_no_op() {
        // Two orthogonal centroids — cosine 0.0, well below 0.85.
        let mut clusters = vec![
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(0, 1.0)],
                0.95,
                4,
            ),
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(2, 1.0)],
                0.92,
                4,
            ),
        ];
        let n = merge_clusters_by_centroid(&mut clusters, &cfg(3, 0.85)).unwrap();
        assert_eq!(n, 0);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn merge_two_similar_clusters_into_one() {
        // Two centroids that are within threshold (cosine ≈ 0.99).
        let big_ids = vec![
            MemoryId::new(),
            MemoryId::new(),
            MemoryId::new(),
            MemoryId::new(),
        ];
        let small_ids = vec![MemoryId::new(), MemoryId::new(), MemoryId::new()];
        let big_centroid = &[(0, 1.0)];
        let small_centroid = &[(0, 0.99), (1, 0.01)];

        let mut clusters = vec![
            cluster_with(small_ids.clone(), small_centroid, 0.93, 4),
            cluster_with(big_ids.clone(), big_centroid, 0.97, 4),
        ];
        let absorbed = merge_clusters_by_centroid(&mut clusters, &cfg(3, 0.85)).unwrap();
        assert_eq!(absorbed, 1);
        assert_eq!(clusters.len(), 1);
        // Survivor is the bigger cluster (4 episodes) — its cluster_id
        // is preserved.
        assert_eq!(
            clusters[0].episode_ids.len(),
            big_ids.len() + small_ids.len()
        );
        // All input episodes present (sorted union, deduped).
        let merged_set: std::collections::HashSet<_> =
            clusters[0].episode_ids.iter().copied().collect();
        for id in big_ids.iter().chain(small_ids.iter()) {
            assert!(merged_set.contains(id), "missing {id}");
        }
        // Coherence is between the two inputs (weighted avg toward the
        // bigger cluster).
        assert!(clusters[0].coherence < 0.97);
        assert!(clusters[0].coherence > 0.93);
        // Centroid stays unit-norm.
        let v = clusters[0]
            .centroid
            .as_ref()
            .unwrap()
            .as_f32_slice()
            .unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "centroid norm: {norm}");
    }

    #[test]
    fn merge_does_not_create_cluster_over_max_size() {
        let mut clusters = vec![
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(0, 1.0)],
                0.95,
                4,
            ),
            cluster_with(
                vec![
                    MemoryId::new(),
                    MemoryId::new(),
                    MemoryId::new(),
                    MemoryId::new(),
                ],
                &[(0, 0.99), (1, 0.01)],
                0.94,
                4,
            ),
        ];
        let mut config = cfg(3, 0.85);
        config.cluster_max_size = 5;

        let absorbed = merge_clusters_by_centroid(&mut clusters, &config).unwrap();

        assert_eq!(absorbed, 0);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn merge_transitive_three_way() {
        // A↔B and B↔C above threshold; A↔C may dip below — union-find
        // groups all 3. Same property exercised for cluster_episodes
        // already; verify it carries to the merge fn.
        let a_ids = vec![MemoryId::new(), MemoryId::new(), MemoryId::new()];
        let b_ids = vec![MemoryId::new(), MemoryId::new(), MemoryId::new()];
        let c_ids = vec![MemoryId::new(), MemoryId::new(), MemoryId::new()];
        let mut clusters = vec![
            cluster_with(a_ids.clone(), &[(0, 1.0)], 0.95, 4),
            cluster_with(b_ids.clone(), &[(0, 0.93), (1, 0.37)], 0.94, 4),
            cluster_with(c_ids.clone(), &[(1, 1.0)], 0.95, 4),
        ];
        // Threshold 0.3 lets B↔C through (≈0.37); A↔B is ≈0.93.
        // A↔C is 0.0 — fails directly, but union-find connects them.
        let absorbed = merge_clusters_by_centroid(&mut clusters, &cfg(3, 0.3)).unwrap();
        assert_eq!(absorbed, 2);
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].episode_ids.len(),
            a_ids.len() + b_ids.len() + c_ids.len()
        );
    }

    #[test]
    fn merge_below_threshold_keeps_separate() {
        // sim ≈ 0.7 — below 0.85, no merge.
        let mut clusters = vec![
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(0, 1.0)],
                0.95,
                4,
            ),
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(0, 0.7), (1, 0.7)],
                0.92,
                4,
            ),
        ];
        let absorbed = merge_clusters_by_centroid(&mut clusters, &cfg(3, 0.85)).unwrap();
        assert_eq!(absorbed, 0);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn merge_survivor_picks_largest_cluster_id() {
        // Both clusters have 3 episodes — ties broken by smallest
        // cluster_id (lex). Construct two clusters and inspect the
        // result's preserved cluster_id.
        let small_id_str = "00000000-0000-0000-0000-000000000001";
        let big_id_str = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let mut clusters = vec![
            Cluster {
                cluster_id: MemoryId::from_str(big_id_str).unwrap(),
                episode_ids: vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                centroid: Some(unit_emb(4, &[(0, 1.0)])),
                coherence: 0.95,
            },
            Cluster {
                cluster_id: MemoryId::from_str(small_id_str).unwrap(),
                episode_ids: vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                centroid: Some(unit_emb(4, &[(0, 0.99), (1, 0.01)])),
                coherence: 0.95,
            },
        ];
        let absorbed = merge_clusters_by_centroid(&mut clusters, &cfg(3, 0.85)).unwrap();
        assert_eq!(absorbed, 1);
        assert_eq!(clusters.len(), 1);
        // Tie-break favours the smaller cluster_id.
        assert_eq!(clusters[0].cluster_id.to_string(), small_id_str);
    }

    #[test]
    fn merge_rejects_centroid_dim_mismatch() {
        let mut clusters = vec![
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(0, 1.0)],
                0.95,
                4,
            ),
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(0, 1.0)],
                0.95,
                8, // different dim
            ),
        ];
        let err = merge_clusters_by_centroid(&mut clusters, &cfg(3, 0.85)).unwrap_err();
        assert!(err.to_string().contains("dim"), "got: {err}");
    }

    /// End-to-end shape check: cluster_episodes that produces 2
    /// per-day clusters → merge_clusters_by_centroid folds them into
    /// 1 when the centroids are similar enough. Closes the cross-
    /// UTC-midnight case in the docstring.
    #[test]
    fn merge_collapses_cross_day_clusters() {
        // Day A and Day B (next day), each with 3 near-identical
        // pasta-themed episodes. cluster_episodes produces 2
        // clusters (one per bucket). merge_clusters_by_centroid
        // recognises the centroid similarity and folds.
        let day_a = 1_700_000_000_000i64;
        let day_b = day_a + MS_PER_DAY;
        let inputs = vec![
            (ep(day_a, "pa1"), unit_emb(4, &[(0, 1.0)])),
            (
                ep(day_a + 1000, "pa2"),
                unit_emb(4, &[(0, 0.99), (1, 0.01)]),
            ),
            (
                ep(day_a + 2000, "pa3"),
                unit_emb(4, &[(0, 0.98), (1, 0.02)]),
            ),
            (ep(day_b, "pb1"), unit_emb(4, &[(0, 1.0)])),
            (
                ep(day_b + 1000, "pb2"),
                unit_emb(4, &[(0, 0.99), (1, 0.01)]),
            ),
            (
                ep(day_b + 2000, "pb3"),
                unit_emb(4, &[(0, 0.98), (1, 0.02)]),
            ),
        ];
        let mut clusters = cluster_episodes(&inputs, &cfg(3, 0.85)).unwrap();
        assert_eq!(clusters.len(), 2, "expected one cluster per day pre-merge");
        let absorbed = merge_clusters_by_centroid(&mut clusters, &cfg(3, 0.85)).unwrap();
        assert_eq!(absorbed, 1);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].episode_ids.len(), 6);
    }

    // -----------------------------------------------------------------
    // plan_existing_merges (DB-application API)
    // -----------------------------------------------------------------

    #[test]
    fn plan_empty_or_singleton_yields_empty_plan() {
        let p = plan_existing_merges(&[], &cfg(3, 0.85)).unwrap();
        assert!(p.merges.is_empty());
        assert_eq!(p.absorbed(), 0);

        let one = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 1.0)],
            0.95,
            4,
        )];
        let p = plan_existing_merges(&one, &cfg(3, 0.85)).unwrap();
        assert!(p.merges.is_empty());
    }

    #[test]
    fn plan_two_similar_yields_one_merge_op() {
        let big_id = MemoryId::new();
        let small_id = MemoryId::new();
        let big_eps: Vec<MemoryId> = (0..4).map(|_| MemoryId::new()).collect();
        let small_eps: Vec<MemoryId> = (0..3).map(|_| MemoryId::new()).collect();
        let clusters = vec![
            Cluster {
                cluster_id: small_id,
                episode_ids: small_eps.clone(),
                centroid: Some(unit_emb(4, &[(0, 0.99), (1, 0.01)])),
                coherence: 0.93,
            },
            Cluster {
                cluster_id: big_id,
                episode_ids: big_eps.clone(),
                centroid: Some(unit_emb(4, &[(0, 1.0)])),
                coherence: 0.97,
            },
        ];
        let plan = plan_existing_merges(&clusters, &cfg(3, 0.85)).unwrap();
        assert_eq!(plan.merges.len(), 1);
        assert_eq!(plan.absorbed(), 1);
        let op = &plan.merges[0];
        // Survivor = bigger cluster (4 eps).
        assert_eq!(op.survivor_id, big_id);
        assert_eq!(op.loser_ids, vec![small_id]);
        assert_eq!(op.merged_episode_ids.len(), big_eps.len() + small_eps.len());
        // Inputs un-mutated (this fn is plan-only).
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].cluster_id, small_id);
        assert_eq!(clusters[1].cluster_id, big_id);
    }

    #[test]
    fn plan_unrelated_yields_empty() {
        let clusters = vec![
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(0, 1.0)],
                0.95,
                4,
            ),
            cluster_with(
                vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
                &[(2, 1.0)],
                0.95,
                4,
            ),
        ];
        let plan = plan_existing_merges(&clusters, &cfg(3, 0.85)).unwrap();
        assert!(plan.merges.is_empty());
    }

    #[test]
    fn plan_three_way_transitive_one_op_two_losers() {
        let a_eps: Vec<MemoryId> = (0..3).map(|_| MemoryId::new()).collect();
        let b_eps: Vec<MemoryId> = (0..3).map(|_| MemoryId::new()).collect();
        let c_eps: Vec<MemoryId> = (0..3).map(|_| MemoryId::new()).collect();
        let clusters = vec![
            cluster_with(a_eps.clone(), &[(0, 1.0)], 0.95, 4),
            cluster_with(b_eps.clone(), &[(0, 0.93), (1, 0.37)], 0.94, 4),
            cluster_with(c_eps.clone(), &[(1, 1.0)], 0.95, 4),
        ];
        let plan = plan_existing_merges(&clusters, &cfg(3, 0.3)).unwrap();
        assert_eq!(plan.merges.len(), 1, "transitive group → one op");
        assert_eq!(plan.absorbed(), 2);
        let op = &plan.merges[0];
        assert_eq!(op.loser_ids.len(), 2);
        assert_eq!(op.merged_episode_ids.len(), 9);
    }

    #[test]
    fn plan_preserves_in_memory_mutation_equivalence() {
        // The in-memory mutation API and the plan API must produce
        // congruent output: same survivor cluster_id, same merged
        // episode_ids, same centroid bytes, same coherence.
        let big_eps: Vec<MemoryId> = (0..4).map(|_| MemoryId::new()).collect();
        let small_eps: Vec<MemoryId> = (0..3).map(|_| MemoryId::new()).collect();
        let big = Cluster {
            cluster_id: MemoryId::new(),
            episode_ids: big_eps.clone(),
            centroid: Some(unit_emb(4, &[(0, 1.0)])),
            coherence: 0.97,
        };
        let small = Cluster {
            cluster_id: MemoryId::new(),
            episode_ids: small_eps.clone(),
            centroid: Some(unit_emb(4, &[(0, 0.99), (1, 0.01)])),
            coherence: 0.93,
        };

        // Plan-only.
        let plan = plan_existing_merges(&[small.clone(), big.clone()], &cfg(3, 0.85)).unwrap();
        assert_eq!(plan.merges.len(), 1);
        let op = &plan.merges[0];

        // Mutation-API.
        let mut mut_clusters = vec![small.clone(), big.clone()];
        let absorbed = merge_clusters_by_centroid(&mut mut_clusters, &cfg(3, 0.85)).unwrap();
        assert_eq!(absorbed, 1);
        assert_eq!(mut_clusters.len(), 1);
        let post = &mut_clusters[0];

        // Same survivor.
        assert_eq!(op.survivor_id, post.cluster_id);
        // Same merged episode_ids (sorted in both paths).
        let mut a = op.merged_episode_ids.clone();
        let mut b = post.episode_ids.clone();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        // Same centroid bytes (deterministic float math).
        let post_centroid = post.centroid.as_ref().unwrap();
        assert_eq!(op.merged_centroid.data, post_centroid.data);
        assert_eq!(op.merged_centroid.dim, post_centroid.dim);
        // Same coherence (bit-exact: same arithmetic).
        assert_eq!(op.merged_coherence.to_bits(), post.coherence.to_bits());
    }

    // -----------------------------------------------------------------
    // absorb_into_existing
    // -----------------------------------------------------------------

    fn summary(
        cluster_id: MemoryId,
        centroid: &[(usize, f32)],
        coherence: f32,
        count: usize,
        dim: usize,
    ) -> ExistingClusterSummary {
        ExistingClusterSummary {
            cluster_id,
            centroid: unit_emb(dim, centroid),
            coherence,
            episode_count: count,
        }
    }

    #[test]
    fn absorb_empty_inputs_yield_empty_plan() {
        let plan = absorb_into_existing(&[], &[], &cfg(3, 0.85)).unwrap();
        assert!(plan.absorptions.is_empty());

        let only_new = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 1.0)],
            0.95,
            4,
        )];
        let plan = absorb_into_existing(&only_new, &[], &cfg(3, 0.85)).unwrap();
        assert!(plan.absorptions.is_empty());

        let only_existing = vec![summary(MemoryId::new(), &[(0, 1.0)], 0.95, 3, 4)];
        let plan = absorb_into_existing(&[], &only_existing, &cfg(3, 0.85)).unwrap();
        assert!(plan.absorptions.is_empty());
    }

    #[test]
    fn absorb_below_threshold_no_op() {
        // sim ≈ 0.7, below 0.85.
        let new = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 1.0)],
            0.95,
            4,
        )];
        let existing = vec![summary(MemoryId::new(), &[(0, 0.7), (1, 0.7)], 0.92, 3, 4)];
        let plan = absorb_into_existing(&new, &existing, &cfg(3, 0.85)).unwrap();
        assert!(plan.absorptions.is_empty());
    }

    #[test]
    fn absorb_above_threshold_folds_into_existing() {
        let new_id = MemoryId::new();
        let existing_id = MemoryId::new();
        let new_episode_ids = vec![MemoryId::new(), MemoryId::new(), MemoryId::new()];
        let new = vec![Cluster {
            cluster_id: new_id,
            episode_ids: new_episode_ids.clone(),
            centroid: Some(unit_emb(4, &[(0, 0.99), (1, 0.01)])),
            coherence: 0.94,
        }];
        let existing = vec![summary(existing_id, &[(0, 1.0)], 0.97, 5, 4)];
        let plan = absorb_into_existing(&new, &existing, &cfg(3, 0.85)).unwrap();
        assert_eq!(plan.absorptions.len(), 1);
        let a = &plan.absorptions[0];
        assert_eq!(a.new_cluster_id, new_id);
        assert_eq!(a.existing_cluster_id, existing_id);
        assert_eq!(a.absorbed_episode_ids, new_episode_ids);
        // Merged centroid is unit-norm.
        let v = a.merged_centroid.as_f32_slice().unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "centroid norm: {norm}");
        // Coherence is between the two inputs.
        assert!(a.merged_coherence >= 0.94 && a.merged_coherence <= 0.97);
        // Helpers report correctly.
        assert!(plan.was_absorbed(new_id));
        assert_eq!(plan.modified_existing_ids(), vec![existing_id]);
    }

    #[test]
    fn absorb_does_not_create_cluster_over_max_size() {
        let new = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 0.99), (1, 0.01)],
            0.94,
            4,
        )];
        let existing = vec![summary(MemoryId::new(), &[(0, 1.0)], 0.97, 5, 4)];
        let mut config = cfg(3, 0.85);
        config.cluster_max_size = 7;

        let plan = absorb_into_existing(&new, &existing, &config).unwrap();

        assert!(plan.absorptions.is_empty());
    }

    #[test]
    fn absorb_picks_largest_existing_on_tie() {
        // Two existing clusters both above threshold; absorb picks
        // the one with more episodes.
        let new = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 0.99), (1, 0.01)],
            0.95,
            4,
        )];
        let small_id = MemoryId::new();
        let big_id = MemoryId::new();
        let existing = vec![
            summary(small_id, &[(0, 1.0)], 0.95, 3, 4),
            summary(big_id, &[(0, 1.0)], 0.95, 7, 4),
        ];
        let plan = absorb_into_existing(&new, &existing, &cfg(3, 0.85)).unwrap();
        assert_eq!(plan.absorptions.len(), 1);
        // Bigger existing (7 episodes) wins.
        assert_eq!(plan.absorptions[0].existing_cluster_id, big_id);
    }

    #[test]
    fn absorb_tie_break_smallest_cluster_id() {
        // Two existing clusters with same episode count + same
        // similarity — tie-break by smallest cluster_id.
        let small_id = MemoryId::from_str("00000000-0000-0000-0000-000000000001").unwrap();
        let big_id = MemoryId::from_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();
        let new = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 1.0)],
            0.95,
            4,
        )];
        let existing = vec![
            summary(big_id, &[(0, 1.0)], 0.95, 3, 4),
            summary(small_id, &[(0, 1.0)], 0.95, 3, 4),
        ];
        let plan = absorb_into_existing(&new, &existing, &cfg(3, 0.85)).unwrap();
        assert_eq!(plan.absorptions.len(), 1);
        assert_eq!(plan.absorptions[0].existing_cluster_id, small_id);
    }

    #[test]
    fn absorb_multiple_new_into_same_existing_updates_state() {
        // Two new clusters both want to fold into the same existing
        // cluster. The second absorb should see the post-first
        // updated centroid + count.
        let existing_id = MemoryId::new();
        let n1 = Cluster {
            cluster_id: MemoryId::new(),
            episode_ids: vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            centroid: Some(unit_emb(4, &[(0, 0.99), (1, 0.01)])),
            coherence: 0.94,
        };
        let n2 = Cluster {
            cluster_id: MemoryId::new(),
            episode_ids: vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            centroid: Some(unit_emb(4, &[(0, 0.97), (1, 0.03)])),
            coherence: 0.93,
        };
        let new = vec![n1.clone(), n2.clone()];
        let existing = vec![summary(existing_id, &[(0, 1.0)], 0.97, 5, 4)];
        let plan = absorb_into_existing(&new, &existing, &cfg(3, 0.85)).unwrap();
        assert_eq!(plan.absorptions.len(), 2);
        assert_eq!(plan.absorptions[0].new_cluster_id, n1.cluster_id);
        assert_eq!(plan.absorptions[1].new_cluster_id, n2.cluster_id);
        // Both target the same existing.
        assert_eq!(plan.absorptions[0].existing_cluster_id, existing_id);
        assert_eq!(plan.absorptions[1].existing_cluster_id, existing_id);
        // modified_existing_ids dedups.
        assert_eq!(plan.modified_existing_ids(), vec![existing_id]);
    }

    #[test]
    fn absorb_partial_some_match_some_dont() {
        // Three new clusters, one matches an existing, two don't.
        let existing_id = MemoryId::new();
        let n_match = Cluster {
            cluster_id: MemoryId::new(),
            episode_ids: vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            centroid: Some(unit_emb(4, &[(0, 0.99), (1, 0.01)])),
            coherence: 0.94,
        };
        let n_orth1 = Cluster {
            cluster_id: MemoryId::new(),
            episode_ids: vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            centroid: Some(unit_emb(4, &[(2, 1.0)])),
            coherence: 0.95,
        };
        let n_orth2 = Cluster {
            cluster_id: MemoryId::new(),
            episode_ids: vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            centroid: Some(unit_emb(4, &[(3, 1.0)])),
            coherence: 0.95,
        };
        let new = vec![n_match.clone(), n_orth1.clone(), n_orth2.clone()];
        let existing = vec![summary(existing_id, &[(0, 1.0)], 0.97, 5, 4)];
        let plan = absorb_into_existing(&new, &existing, &cfg(3, 0.85)).unwrap();
        assert_eq!(plan.absorptions.len(), 1);
        assert_eq!(plan.absorptions[0].new_cluster_id, n_match.cluster_id);
        assert!(!plan.was_absorbed(n_orth1.cluster_id));
        assert!(!plan.was_absorbed(n_orth2.cluster_id));
    }

    #[test]
    fn absorb_rejects_dim_mismatch_in_existing() {
        let new = vec![cluster_with(
            vec![MemoryId::new(), MemoryId::new(), MemoryId::new()],
            &[(0, 1.0)],
            0.95,
            4,
        )];
        let existing = vec![
            summary(MemoryId::new(), &[(0, 1.0)], 0.95, 3, 4),
            summary(MemoryId::new(), &[(0, 1.0)], 0.95, 3, 8), // wrong dim
        ];
        let err = absorb_into_existing(&new, &existing, &cfg(3, 0.85)).unwrap_err();
        assert!(err.to_string().contains("dim"), "got: {err}");
    }

    /// Centroid is unit-length within fp32 tolerance.
    #[test]
    fn centroid_is_unit_length() {
        let day_a = 1_700_000_000_000i64;
        let inputs = vec![
            (ep(day_a, "a1"), unit_emb(8, &[(0, 1.0)])),
            (ep(day_a + 1000, "a2"), unit_emb(8, &[(0, 1.0)])),
            (ep(day_a + 2000, "a3"), unit_emb(8, &[(0, 1.0)])),
        ];
        let r = cluster_episodes(&inputs, &cfg(3, 0.85)).unwrap();
        assert_eq!(r.len(), 1);
        let c = r[0].centroid.as_ref().unwrap();
        let v = c.as_f32_slice().unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "centroid norm: {norm}");
    }
}
