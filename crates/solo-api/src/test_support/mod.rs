// SPDX-License-Identifier: Apache-2.0

//! Test fixtures shared across `solo-api`'s tests and (via the
//! `test-support` feature) downstream-crate tests.
//!
//! Lives behind `#[cfg(any(test, feature = "test-support"))]` so it
//! never compiles into release builds.
//!
//! v0.9.0 P2 introduces [`fake_mcp_client::FakeMcpClient`] — a tiny
//! in-process mock of the rmcp client side that satisfies the
//! `peer.create_message` shape from server-side without needing a real
//! `Peer<RoleServer>` (whose constructors are private inside rmcp).
//! P4's `SamplingCoordinator` batching tests live in `solo-steward`
//! and reuse this same fixture via the `test-support` feature.

#![cfg(any(test, feature = "test-support"))]

pub mod fake_mcp_client;

pub use fake_mcp_client::{FakeMcpClient, FakeResponse, FakeSamplingError};
