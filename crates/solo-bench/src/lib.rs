// SPDX-License-Identifier: Apache-2.0

//! Solo benchmark harness. Runs against fixed corpora at 10K / 100K / 1M
//! rows; CI fails if p99 recall latency exceeds budget by >20%.
//!
//! Target numbers (from `solo-v0-architecture.md §3.5`):
//!   - Recognition tier p99: 0.05–0.5 ms
//!   - Recall tier (HNSW + RRF) p99: 3–7 ms
//!
//! Production benches land in week 5; this is a placeholder.

#![allow(dead_code, unused_imports)]
