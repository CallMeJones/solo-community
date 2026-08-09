// SPDX-License-Identifier: Apache-2.0

//! Internal compatibility handle while Solo's public vocabulary moves from
//! the historical tenant name to `MemoryLibrary`.
//!
//! There is deliberately no registry, lifecycle API, alternate DB filename,
//! or multi-library constructor in Community.

pub mod handle;

pub use handle::{LibraryHandle, LibraryOpenParams};

/// Previous layout names retained only for one-way migration and cleanup.
pub(crate) const TENANTS_INDEX_FILENAME: &str = "tenants_index.db";
pub(crate) const TENANTS_SUBDIR: &str = "tenants";
