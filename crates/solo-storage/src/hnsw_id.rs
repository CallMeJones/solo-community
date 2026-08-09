// SPDX-License-Identifier: Apache-2.0

//! Kind-discriminated rowid encoding for the shared HNSW namespace.
//!
//! ## Why this exists
//!
//! Per ADR-0003 (operational note added in v0.7.0 P7b), the writer-actor and
//! the reader-side query modules share a single HNSW vector index across
//! `episodes` and `document_chunks`. Both tables declare
//! `rowid INTEGER PRIMARY KEY AUTOINCREMENT`, and **SQLite AUTOINCREMENT is
//! per-table** (it maintains separate sequences via `sqlite_sequence`). So
//! `episodes.rowid = 1` AND `document_chunks.rowid = 1` are concurrent,
//! valid values. Solo's `HnswIndex` keyed both directly as i64 IDs in the
//! same shared namespace — collision was a matter of when, not if.
//!
//! Empirically (see test `hnsw_rs_accepts_duplicate_origin_id` in
//! `vector_index.rs`), `hnsw_rs` 0.3 silently **accepts** duplicate `origin_id`
//! inserts: a second `insert((vec_b, 1))` after `insert((vec_a, 1))` produces
//! two distinct internal points sharing the same external id. A subsequent
//! `search` then returns BOTH points, with the same external id reported
//! twice — recall results would be ambiguous (and the SQL JOIN downstream
//! would return whichever rowid matched the right table, regardless of which
//! vector actually matched the query).
//!
//! ## The fix
//!
//! Encode the rowid + a kind discriminator into a single i64 used as the
//! HNSW external id. The high bit (`1 << 62`) is reserved as the "chunk"
//! flag; episodes leave it clear. SQLite rowids are always non-negative
//! 64-bit integers, and in practice never approach 2^62 (~4.6 quintillion);
//! a daemon that ingests 1 million rows per second would take ~146,000 years
//! to overflow. Debug builds assert the input rowid doesn't already carry
//! the flag bit, catching any future schema that produced very large ids.
//!
//! ### Forward compatibility
//!
//! The encoding survives `hnsw_rs` upgrades — even if a future version
//! starts erroring or overwriting on duplicate origin ids, the encoded
//! namespace is collision-free by construction, so the behaviour change
//! is invisible to Solo.
//!
//! Future kinds (e.g., a hypothetical `summaries.rowid`) can extend the
//! scheme by reserving another high bit. With 2 bits used we'd still
//! have rowid headroom up to 2^61 (~2.3 quintillion), more than enough.
//!
//! ### Where it's applied
//!
//! Every site that touches `VectorIndex::{add, remove, search}` MUST use
//! the matching encoder/decoder:
//!
//!   - Writer `dispatch_remember` / `handle_remember` (episode) →
//!     `episode_hnsw_id`.
//!   - Writer `dispatch_ingest_document` (chunks) → `chunk_hnsw_id`.
//!   - Writer `handle_forget` (episode) → `episode_hnsw_id`.
//!   - Writer `handle_forget_document` (chunks) → `chunk_hnsw_id`.
//!   - Recovery `replay_pending_index` → encoder picked from
//!     `pending_index.kind` (per-row).
//!   - Recovery `rebuild_hnsw_from_sql` (episodes only today) →
//!     `episode_hnsw_id`.
//!   - Startup `rebuild_tombstones_from_sql` (episodes only) →
//!     `episode_hnsw_id`.
//!   - Query `run_recall` — decode returned ids, accept only `Episode`
//!     kind, then JOIN against `episodes.rowid` using the **decoded**
//!     rowid (the high bit must NOT leak into SQL).
//!   - Query `run_doc_search` — decode returned ids, accept only
//!     `Chunk` kind, then JOIN against `document_chunks.rowid` using
//!     the decoded rowid.
//!
//! The decoder is the SQL boundary: anything that ends up in a SQL
//! `IN (?, ?, ...)` list MUST be the decoded rowid (raw, no high bit).

/// High bit reserved for chunks. SQLite rowids are non-negative i64;
/// 2^62 is the chunk-kind flag. Episodes have this bit clear; chunks
/// have it set.
pub const HNSW_CHUNK_BIT: i64 = 1 << 62;

/// Discriminator returned by [`decode_hnsw_id`]. The variant tells the
/// caller which SQL table to JOIN against; the paired `i64` is the
/// decoded rowid (high bit stripped).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HnswIdKind {
    Episode,
    Chunk,
}

/// Encode an episode rowid for HNSW. Episode rowids leave the high bit
/// clear, so the encoded form is byte-identical to the input. Pre-v0.7.0
/// episode entries in already-persisted HNSW snapshots round-trip
/// transparently — no migration needed.
///
/// Debug asserts the input rowid doesn't already carry the chunk bit
/// (which would be a sign of an absurdly large rowid or a caller passing
/// an already-encoded value back through). In release builds the assert
/// is compiled out; the encoding is still correct (the high bit just
/// stays set, which would corrupt the kind discriminator).
#[inline]
pub fn episode_hnsw_id(episode_rowid: i64) -> i64 {
    debug_assert!(
        episode_rowid >= 0,
        "episode rowid must be non-negative; got {episode_rowid}"
    );
    debug_assert!(
        episode_rowid & HNSW_CHUNK_BIT == 0,
        "episode rowid {episode_rowid} carries the chunk bit; SQLite shouldn't produce rowids ≥ 2^62"
    );
    episode_rowid
}

/// Encode a chunk rowid for HNSW. Sets the high bit so chunk ids occupy
/// a non-overlapping namespace from episode ids.
///
/// Debug asserts the input rowid doesn't already carry the chunk bit
/// (which would indicate either a rowid ≥ 2^62 or double-encoding).
#[inline]
pub fn chunk_hnsw_id(chunk_rowid: i64) -> i64 {
    debug_assert!(
        chunk_rowid >= 0,
        "chunk rowid must be non-negative; got {chunk_rowid}"
    );
    debug_assert!(
        chunk_rowid & HNSW_CHUNK_BIT == 0,
        "chunk rowid {chunk_rowid} carries the chunk bit; SQLite shouldn't produce rowids ≥ 2^62"
    );
    chunk_rowid | HNSW_CHUNK_BIT
}

/// Decode an HNSW id back to `(kind, rowid)`. The returned `rowid` is the
/// SQL-side value — caller passes it straight to a SQL `IN (...)` list.
///
/// The high bit is the kind discriminator; everything else is the
/// underlying rowid.
#[inline]
pub fn decode_hnsw_id(hnsw_id: i64) -> (HnswIdKind, i64) {
    if hnsw_id & HNSW_CHUNK_BIT != 0 {
        (HnswIdKind::Chunk, hnsw_id & !HNSW_CHUNK_BIT)
    } else {
        (HnswIdKind::Episode, hnsw_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn episode_id_is_identity_for_low_rowids() {
        // Round-trip: an episode rowid encodes to itself (high bit clear),
        // which means pre-v0.7.0 HNSW snapshots still work as-is — no
        // re-keying needed.
        for rowid in [0i64, 1, 2, 42, 1_000, 1_000_000, (1_i64 << 30)] {
            let enc = episode_hnsw_id(rowid);
            assert_eq!(
                enc, rowid,
                "episode_hnsw_id must be identity for rowid={rowid}"
            );
            let (kind, decoded) = decode_hnsw_id(enc);
            assert_eq!(kind, HnswIdKind::Episode);
            assert_eq!(decoded, rowid);
        }
    }

    #[test]
    fn chunk_id_sets_high_bit() {
        for rowid in [0i64, 1, 2, 42, 1_000, 1_000_000, (1_i64 << 30)] {
            let enc = chunk_hnsw_id(rowid);
            assert_ne!(
                enc, rowid,
                "chunk_hnsw_id must differ from input rowid={rowid}"
            );
            assert_eq!(
                enc & HNSW_CHUNK_BIT,
                HNSW_CHUNK_BIT,
                "chunk_hnsw_id must set the chunk bit (rowid={rowid})"
            );
            let (kind, decoded) = decode_hnsw_id(enc);
            assert_eq!(kind, HnswIdKind::Chunk);
            assert_eq!(decoded, rowid);
        }
    }

    #[test]
    fn episode_and_chunk_with_same_rowid_have_distinct_hnsw_ids() {
        // The reason the encoding exists: the same rowid in two tables
        // produces two distinct HNSW ids.
        let rowid = 1i64;
        let ep = episode_hnsw_id(rowid);
        let chunk = chunk_hnsw_id(rowid);
        assert_ne!(
            ep, chunk,
            "episode and chunk with rowid=1 must encode differently"
        );
        // Both decode back to the same rowid, with different kinds.
        let (kind_ep, decoded_ep) = decode_hnsw_id(ep);
        let (kind_chunk, decoded_chunk) = decode_hnsw_id(chunk);
        assert_eq!(decoded_ep, rowid);
        assert_eq!(decoded_chunk, rowid);
        assert_eq!(kind_ep, HnswIdKind::Episode);
        assert_eq!(kind_chunk, HnswIdKind::Chunk);
    }

    #[test]
    fn decode_legacy_episode_id_zero_is_episode() {
        // A legacy snapshot might have rowid=0 entries. The encoding
        // says rowid=0 with high bit clear → Episode kind.
        let (kind, decoded) = decode_hnsw_id(0);
        assert_eq!(kind, HnswIdKind::Episode);
        assert_eq!(decoded, 0);
    }

    #[test]
    fn chunk_bit_value_is_2_pow_62() {
        // Sanity: the constant is what we promised in the docs.
        assert_eq!(HNSW_CHUNK_BIT, 1i64 << 62);
        // And it's representable as a positive i64 (not sign-bit).
        assert!(HNSW_CHUNK_BIT > 0);
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn episode_negative_rowid_panics_in_debug() {
        let _ = episode_hnsw_id(-1);
    }

    #[test]
    #[should_panic(expected = "non-negative")]
    fn chunk_negative_rowid_panics_in_debug() {
        let _ = chunk_hnsw_id(-1);
    }

    #[test]
    #[should_panic(expected = "chunk bit")]
    fn episode_rowid_with_chunk_bit_panics_in_debug() {
        // A rowid that would somehow carry the high bit — protect
        // against future drift.
        let _ = episode_hnsw_id(HNSW_CHUNK_BIT | 1);
    }

    #[test]
    #[should_panic(expected = "chunk bit")]
    fn chunk_rowid_with_chunk_bit_panics_in_debug() {
        let _ = chunk_hnsw_id(HNSW_CHUNK_BIT | 1);
    }
}
