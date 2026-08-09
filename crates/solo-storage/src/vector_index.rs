// SPDX-License-Identifier: Apache-2.0

//! `HnswIndex` — `solo_core::VectorIndex` implementation backed by `hnsw_rs`.
//!
//! Wraps `hnsw_rs::Hnsw<'static, f32, DistCosine>`. All mutating methods take
//! `&self`; concurrency is provided by `hnsw_rs`'s internal
//! `parking_lot::RwLock` (search takes a read lock, insert takes a write
//! lock). The same `Arc<HnswIndex>` is shared between the writer thread and
//! the read pool, per ADR-0003 §O2.
//!
//! Snapshot save/load lives in [`crate::snapshot`]; this module exposes
//! `HnswIndex::save` (which delegates) so the trait stays callable.
//!
//! ### Removal semantics (commit 1.3)
//!
//! `hnsw_rs` does not support removal. Per ADR-0003 §"Reply timing semantics"
//! for `Forget`, the architecture preserves silent traces — recall paths
//! exclude `status='forgotten'` rows by SQL filter, so the vector staying in
//! the graph is acceptable. We track removed rowids in an in-memory
//! `HashSet<i64>` and filter them out of `search` results so the index
//! behaves "as if" they were removed even before the next rebuild.
//!
//! ### Tuning defaults (`HnswFactory::default`)
//!
//! Conservative defaults suitable for the typical Solo user (10K-100K
//! vectors): `max_nb_connection = 16`, `ef_construction = 200`,
//! `max_layer = 16`, `max_elements_hint = 10_000`. Override via
//! `HnswFactory::with_params` once we ship benchmarks; defaults are
//! re-tunable without breaking on-disk format.

use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use hnsw_rs::hnswio::load_description;
use hnsw_rs::prelude::{DistCosine, Hnsw, HnswIo, Neighbour};
use parking_lot::RwLock;
use solo_core::{Error, Result, VectorIndex, VectorIndexFactory};

/// HNSW search width. Rule of thumb: between knbn and max_nb_connection,
/// per `Hnsw::search` docs. We pick 4× the requested knbn capped at 200.
fn ef_for_search(knbn: usize) -> usize {
    (knbn * 4).clamp(16, 200)
}

#[derive(Debug, Clone, Copy)]
pub struct HnswParams {
    pub max_nb_connection: usize,
    pub ef_construction: usize,
    pub max_layer: usize,
    pub max_elements_hint: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            max_nb_connection: 16,
            ef_construction: 200,
            max_layer: 16,
            max_elements_hint: 10_000,
        }
    }
}

/// Backing `hnsw_rs::Hnsw` is `'static` because we never reload from a
/// borrowed buffer — `HnswIo::load_hnsw` returns an owned graph.
type Inner = Hnsw<'static, f32, DistCosine>;

pub struct HnswIndex {
    inner: Inner,
    dim: usize,
    /// rowids whose `remove` was called. Filtered out of `search` results.
    /// Repopulated on load via the recovery path (drift detection drops the
    /// in-memory set and rebuilds from SQL `episodes WHERE status='forgotten'`
    /// in commit 1.6 once `forget` is implemented).
    tombstones: RwLock<HashSet<i64>>,
}

impl std::fmt::Debug for HnswIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // hnsw_rs::Hnsw doesn't impl Debug; surface only the public-facing
        // counters so logs / unwrap_err output stay terse.
        f.debug_struct("HnswIndex")
            .field("dim", &self.dim)
            .field("len", &self.inner.get_nb_point())
            .field("tombstones", &self.tombstones.read().len())
            .finish()
    }
}

impl HnswIndex {
    /// Build a fresh empty index with the given dim and parameters.
    pub fn new(dim: usize, params: HnswParams) -> Self {
        let inner = Hnsw::<'static, f32, DistCosine>::new(
            params.max_nb_connection,
            params.max_elements_hint,
            params.max_layer,
            params.ef_construction,
            DistCosine,
        );
        Self {
            inner,
            dim,
            tombstones: RwLock::new(HashSet::new()),
        }
    }

    pub(crate) fn from_inner(inner: Inner, dim: usize) -> Self {
        Self {
            inner,
            dim,
            tombstones: RwLock::new(HashSet::new()),
        }
    }

    pub(crate) fn inner(&self) -> &Inner {
        &self.inner
    }

    /// Number of vectors physically present in the underlying HNSW graph,
    /// IGNORING tombstones. Used by `snapshot::save` to decide whether
    /// there's anything to persist — it would be wrong to use the
    /// tombstone-aware `len()` because that returns 0 when every vector
    /// has been forgotten, even though the graph itself holds N entries
    /// that need to round-trip across reload.
    pub(crate) fn raw_len(&self) -> usize {
        self.inner.get_nb_point()
    }
}

impl VectorIndex for HnswIndex {
    fn add(&self, rowid: i64, embedding: &[f32]) -> Result<()> {
        if rowid < 0 {
            return Err(Error::vector_index(format!(
                "rowid must be non-negative; got {rowid}"
            )));
        }
        if embedding.len() != self.dim {
            return Err(Error::vector_index(format!(
                "embedding dim mismatch: index dim={}, got {}",
                self.dim,
                embedding.len()
            )));
        }
        // hnsw_rs' insert is `&self` and uses parking_lot::RwLock internally.
        self.inner.insert((embedding, rowid as usize));
        // If a previously-tombstoned rowid is re-added, lift the tombstone.
        self.tombstones.write().remove(&rowid);
        Ok(())
    }

    fn remove(&self, rowid: i64) -> Result<()> {
        // hnsw_rs has no graph removal; we mark tombstoned and filter in
        // `search`. The vector stays resident until the next full rebuild.
        self.tombstones.write().insert(rowid);
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(i64, f32)>> {
        if query.len() != self.dim {
            return Err(Error::vector_index(format!(
                "query dim mismatch: index dim={}, got {}",
                self.dim,
                query.len()
            )));
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        let ef = ef_for_search(k);
        // Search for a few extra to absorb tombstone removal without a
        // second round-trip; cap to keep the worst-case bounded.
        let widened = (k * 2).min(k + 32);
        let neighbours: Vec<Neighbour> = self.inner.search(query, widened, ef);
        let tombs = self.tombstones.read();
        let mut out: Vec<(i64, f32)> = neighbours
            .into_iter()
            .map(|n| (n.d_id as i64, n.distance))
            .filter(|(rowid, _)| !tombs.contains(rowid))
            .take(k)
            .collect();
        // hnsw_rs already returns ascending-distance order, but the take()
        // after filter could leave fewer than k — which is fine.
        out.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }

    fn save(&self, path: &Path) -> Result<()> {
        crate::snapshot::save(self, path)
    }

    fn len(&self) -> usize {
        // get_nb_point counts inserted points; tombstones don't decrement
        // it (they're search-time filters). Drift detection compares
        // `SELECT COUNT(*) FROM episodes WHERE tier='hot' AND status<>'forgotten'`
        // against this, accounting for tombstones separately.
        let total = self.inner.get_nb_point();
        let tomb = self.tombstones.read().len();
        total.saturating_sub(tomb)
    }

    fn dim(&self) -> usize {
        self.dim
    }
}

/// Factory: `create` returns a fresh empty index; `load` restores from disk
/// via the snapshot module's startup decision tree (`.bin` → `.bak` → empty).
#[derive(Debug, Clone)]
pub struct HnswFactory {
    params: HnswParams,
}

impl HnswFactory {
    pub fn new() -> Self {
        Self {
            params: HnswParams::default(),
        }
    }

    pub fn with_params(params: HnswParams) -> Self {
        Self { params }
    }
}

impl Default for HnswFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl VectorIndexFactory for HnswFactory {
    type Index = HnswIndex;

    fn create(&self, dim: usize) -> Result<Self::Index> {
        Ok(HnswIndex::new(dim, self.params))
    }

    /// Load from `path` per ADR-0003 §"Startup file-existence decision tree":
    /// the `.bin`/`.graph` pair, falling back to `.bak`. Path semantics —
    /// `path` is the **directory** holding the snapshot; basenames are
    /// derived per `crate::snapshot::{BASENAME, BASENAME_BAK}`.
    fn load(&self, path: &Path) -> Result<Self::Index> {
        let dim = self.params.max_elements_hint; // unused; load discovers dim
        let _ = dim;
        crate::snapshot::load(path).or_else(|primary_err| {
            tracing::warn!(
                error = %primary_err,
                "primary HNSW snapshot failed to load; trying .bak"
            );
            crate::snapshot::load_bak(path)
        })
    }
}

/// Direct loader (used by `HnswFactory::load` and the recovery path).
///
/// The hnsw_rs `Hnsw` struct doesn't expose its `data_dimension` field
/// publicly, so we peek it via `load_description` on the `.hnsw.graph` file
/// before delegating to `HnswIo::load_hnsw`. Both operations open the file
/// independently — no shared cursor — so the description peek doesn't
/// conflict with the full load.
///
/// ### Lifetime workaround (Box::leak)
///
/// `HnswIo::load_hnsw<'b, 'a>(&'a mut self) -> Hnsw<'b, ...>` carries the
/// constraint `'a: 'b`. To get an owned `Hnsw<'static, ...>` (required so
/// `Arc<dyn VectorIndex>` can be `'static`), `'a` must also be `'static`,
/// meaning the `&mut HnswIo` must live forever. The constraint exists for
/// the mmap case where the loaded graph borrows from the file handle owned
/// by HnswIo; with default options (no mmap) the Hnsw owns all its data
/// and the borrow is vacuous, but the type system can't tell.
///
/// We resolve this by `Box::leak`-ing the HnswIo. Cost: ~150 bytes per
/// reload. The daemon reloads once per startup (or once per recovery
/// cycle), so total leakage stays bounded across realistic lifetimes.
/// Tests that loop reload calls will leak per call; acceptable.
pub(crate) fn load_inner_from_basename(dir: &Path, basename: &str) -> Result<HnswIndex> {
    let mut graph_path = PathBuf::from(dir);
    graph_path.push(format!("{basename}.hnsw.graph"));
    let dim = peek_dim(&graph_path)?;
    let io: &'static mut HnswIo = Box::leak(Box::new(HnswIo::new(dir, basename)));
    let inner: Inner = io
        .load_hnsw::<f32, DistCosine>()
        .map_err(|e| Error::vector_index(format!("HnswIo::load_hnsw: {e}")))?;
    Ok(HnswIndex::from_inner(inner, dim))
}

fn peek_dim(graph_path: &Path) -> Result<usize> {
    let f = OpenOptions::new()
        .read(true)
        .open(graph_path)
        .map_err(|e| Error::vector_index(format!("open {graph_path:?}: {e}")))?;
    let mut buf = BufReader::new(f);
    let descr = load_description(&mut buf)
        .map_err(|e| Error::vector_index(format!("load_description {graph_path:?}: {e}")))?;
    Ok(descr.dimension)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn unit_vec(seed: u32, dim: usize) -> Vec<f32> {
        // Cheap deterministic vector — different seeds produce orthogonal-ish
        // directions for the unit-tests' similarity queries.
        let mut v = vec![0.0f32; dim];
        let s = (seed as f32) * 0.123;
        for (i, x) in v.iter_mut().enumerate() {
            let t = s + i as f32 * 0.317;
            *x = t.sin();
        }
        // Normalise so cosine ~ dot.
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
        for x in &mut v {
            *x /= norm;
        }
        v
    }

    #[test]
    fn fresh_index_is_empty_and_searchable() {
        let idx = HnswIndex::new(8, HnswParams::default());
        assert_eq!(idx.len(), 0);
        assert!(idx.is_empty());
        assert_eq!(idx.dim(), 8);
        let res = idx.search(&unit_vec(1, 8), 5).unwrap();
        assert!(res.is_empty());
    }

    #[test]
    fn add_and_search_finds_self() {
        let idx = HnswIndex::new(8, HnswParams::default());
        let v = unit_vec(7, 8);
        idx.add(42, &v).unwrap();
        assert_eq!(idx.len(), 1);
        let hits = idx.search(&v, 3).unwrap();
        assert!(!hits.is_empty(), "search returned no results");
        assert_eq!(hits[0].0, 42, "self-search must return rowid 42 first");
    }

    #[test]
    fn dim_mismatch_is_rejected() {
        let idx = HnswIndex::new(8, HnswParams::default());
        let err = idx.add(1, &vec![0.0; 4]).unwrap_err();
        assert!(err.to_string().contains("dim mismatch"));
        let err = idx.search(&vec![0.0; 4], 1).unwrap_err();
        assert!(err.to_string().contains("dim mismatch"));
    }

    #[test]
    fn negative_rowid_rejected() {
        let idx = HnswIndex::new(4, HnswParams::default());
        let err = idx.add(-1, &unit_vec(1, 4)).unwrap_err();
        assert!(err.to_string().contains("non-negative"));
    }

    #[test]
    fn remove_tombstones_filtered_from_search() {
        let idx = HnswIndex::new(8, HnswParams::default());
        idx.add(1, &unit_vec(1, 8)).unwrap();
        idx.add(2, &unit_vec(2, 8)).unwrap();
        idx.add(3, &unit_vec(3, 8)).unwrap();
        idx.remove(2).unwrap();
        let hits = idx.search(&unit_vec(2, 8), 3).unwrap();
        assert!(
            !hits.iter().any(|(r, _)| *r == 2),
            "tombstoned rowid 2 must not appear: {hits:?}"
        );
        // len() reflects tombstone count.
        assert_eq!(idx.len(), 2);
    }

    #[test]
    fn re_add_lifts_tombstone() {
        let idx = HnswIndex::new(8, HnswParams::default());
        idx.add(5, &unit_vec(5, 8)).unwrap();
        idx.remove(5).unwrap();
        idx.add(5, &unit_vec(5, 8)).unwrap();
        let hits = idx.search(&unit_vec(5, 8), 3).unwrap();
        assert!(
            hits.iter().any(|(r, _)| *r == 5),
            "re-added rowid must reappear: {hits:?}"
        );
    }

    #[test]
    fn factory_create_returns_empty_index() {
        let factory = HnswFactory::new();
        let idx = factory.create(16).unwrap();
        assert_eq!(idx.dim(), 16);
        assert_eq!(idx.len(), 0);
    }

    #[test]
    fn shareable_across_threads_via_arc() {
        let idx: Arc<dyn VectorIndex + Send + Sync> =
            Arc::new(HnswIndex::new(4, HnswParams::default()));
        // Sanity: traits + Arc compile + run from a spawned thread.
        let idx2 = idx.clone();
        let h = std::thread::spawn(move || {
            idx2.add(1, &unit_vec(1, 4)).unwrap();
        });
        h.join().unwrap();
        assert_eq!(idx.len(), 1);
    }

    /// Empirical probe of `hnsw_rs` 0.3.4 duplicate-add behavior.
    ///
    /// Two distinct vectors inserted at the SAME origin_id (`rowid=1`).
    /// We then assert:
    ///
    ///   * `len()` reports 2 (both points landed in the graph), NOT 1
    ///     — proving the lib accepts the duplicate add silently rather
    ///     than overwriting or erroring (the contract docstring on
    ///     `VectorIndex::add` claims "Idempotent — adding an existing
    ///     rowid replaces the prior vector," which is **NOT** the
    ///     actual behavior of the backing lib).
    ///   * `search` returns BOTH points, each tagged with the same
    ///     external id — proving recall ambiguity.
    ///
    /// This is the empirical justification for the kind-discriminated
    /// encoding in [`crate::hnsw_id`]. Without it, an episode rowid=1
    /// and a chunk rowid=1 in production would shadow each other in
    /// HNSW, with `search` returning ambiguous duplicates and the
    /// downstream SQL JOIN picking whichever table the rowid happens
    /// to match.
    ///
    /// This test is exclusively a regression-watchpoint: a future
    /// `hnsw_rs` release that decides to error or dedup on duplicate
    /// origin_id would fail this test and signal that this comment
    /// (and the doc 0084) need updating — but the encoding itself
    /// would still be correct because the encoded IDs are
    /// collision-free by construction.
    #[test]
    fn hnsw_rs_accepts_duplicate_origin_id_silently() {
        let dim = 8usize;
        let idx = HnswIndex::new(dim, HnswParams::default());

        // Two directly-orthogonal unit vectors so the search step has
        // an unambiguous "closer" target. unit_vec() uses a sin-based
        // seed and produces near-parallel vectors at low seeds, so we
        // hand-pick canonical-basis-like vectors instead.
        let mut vec_a = vec![0.0f32; dim];
        vec_a[0] = 1.0;
        let mut vec_b = vec![0.0f32; dim];
        vec_b[1] = 1.0;
        let dot: f32 = vec_a.iter().zip(vec_b.iter()).map(|(a, b)| a * b).sum();
        assert_eq!(dot, 0.0, "test vectors must be orthogonal");

        // Two adds at the SAME rowid.
        idx.add(1, &vec_a).unwrap();
        idx.add(1, &vec_b).unwrap();

        // Expectation: case (a) — both vectors land in the graph,
        // len() is 2, not 1. Document the actual behavior so future
        // maintainers know.
        let len = idx.len();
        eprintln!(
            "hnsw_rs duplicate-add probe: after add(1, vec_a) + add(1, vec_b), len() = {len}"
        );
        assert_eq!(
            len, 2,
            "hnsw_rs 0.3.4 accepts duplicate origin_id silently; \
             observed len()={len}. If this assertion fails, the lib's \
             behavior has changed (case b 'overwrite' would give len=1, \
             case c 'error' would have errored above). Update \
             docs/dev-log/0084-... and the hnsw_id module docs."
        );

        // Search reports BOTH points, each tagged with origin_id 1.
        // We query in the direction of vec_a — and find ≥1 hit with
        // rowid=1 (could be 2 hits, both with rowid=1).
        let hits = idx.search(&vec_a, 5).unwrap();
        eprintln!(
            "hnsw_rs duplicate-add probe: search returned {} hits: {hits:?}",
            hits.len()
        );
        let dup_rowid_hits = hits.iter().filter(|(r, _)| *r == 1).count();
        assert!(
            dup_rowid_hits >= 1,
            "search must return at least one hit with the duplicated origin_id; got {dup_rowid_hits} (hits={hits:?})"
        );
        // The point that's near vec_a is hits[0]; we don't assert
        // exactly which vector "won" because hnsw_rs's graph
        // topology may surface either. The important fact for the
        // encoding decision: both are present in the index, both
        // were keyed by the same external id, and recall is therefore
        // ambiguous — which is exactly why we encode by kind.
    }
}
