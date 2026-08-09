// SPDX-License-Identifier: Apache-2.0

//! `VectorIndex` and `VectorIndexFactory` traits. See ADR-0002 and ADR-0003.
//!
//! All methods take `&self`. Implementations MUST handle concurrency
//! internally (e.g., `hnsw_rs` uses `parking_lot::RwLock` under the hood;
//! search takes a read lock, insert takes a write lock). This shape lets the
//! writer thread and read-pool tasks share a single
//! `Arc<dyn VectorIndex + Send + Sync>` instance without application-level
//! locking — see ADR-0003 § Operational invariants for the rationale.

use crate::error::Result;
use std::path::Path;

/// Approximate nearest-neighbor index over FP32 vectors keyed by SQLite rowid.
///
/// All mutating methods take `&self`. The discipline that makes this safe is
/// twofold: (1) the impl handles its own concurrency, and (2) only one task
/// (the WriterActor on its dedicated OS thread) issues mutations — read tasks
/// only call `search`. The trait does not enforce (2); callers must.
pub trait VectorIndex: Send + Sync {
    /// Add a vector keyed by SQLite rowid. Idempotent — adding an existing
    /// rowid replaces the prior vector.
    fn add(&self, rowid: i64, embedding: &[f32]) -> Result<()>;

    /// Remove a vector by rowid. Idempotent — removing a missing rowid is OK.
    /// May leave a tombstone (HNSW does); index is rebuilt periodically to
    /// compact.
    fn remove(&self, rowid: i64) -> Result<()>;

    /// Approximate nearest-neighbor search. Returns up to k results sorted by
    /// distance (ascending — smaller distance is more similar).
    fn search(&self, query: &[f32], k: usize) -> Result<Vec<(i64, f32)>>;

    /// Snapshot the index to disk atomically. Implementations MUST write to a
    /// `.tmp` file, fsync, then atomically rename over the target. The
    /// previous snapshot should be preserved as a `.bak` file until the new
    /// snapshot is fully in place. See `solo-v0-architecture.md §3.2` —
    /// "the HNSW sidecar is a cache, not source-of-truth."
    fn save(&self, path: &Path) -> Result<()>;

    /// Number of vectors currently in the index. Used at startup to detect
    /// drift against `SELECT COUNT(*) FROM episodes WHERE tier = 'hot'`. On
    /// mismatch, the index is rebuilt from SQLite.
    fn len(&self) -> usize;

    /// True if the index contains no vectors.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Vector dimension. Must match the Embedder that produced the vectors.
    fn dim(&self) -> usize;
}

/// Separates "create a fresh index" from "load existing index from disk."
/// The factory holds the configuration (HNSW M, efConstruction, etc.) that
/// is otherwise duplicated between the two operations.
pub trait VectorIndexFactory: Send + Sync {
    type Index: VectorIndex;

    /// Create a new empty index with the given vector dimension.
    fn create(&self, dim: usize) -> Result<Self::Index>;

    /// Load an existing index from disk. Validates internal invariants on
    /// load.
    fn load(&self, path: &Path) -> Result<Self::Index>;
}
