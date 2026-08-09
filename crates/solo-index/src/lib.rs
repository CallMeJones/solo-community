// SPDX-License-Identifier: Apache-2.0

//! Solo vector index: HNSW sidecar (hnswlib-rs) for the hot tier; RaBitQ
//! support lands later for the cold tier.
//!
//! See `solo-v0-architecture.md §3.2`. Production code lands in commit 1.3
//! (HNSW wrapper with atomic-rename save + crash-recovery rebuild).

#![allow(dead_code, unused_imports)]

// TODO(commit 1.3): pub mod hnsw;
// TODO(later): pub mod rabitq_cold;
