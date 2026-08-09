// SPDX-License-Identifier: Apache-2.0

//! Per-tenant audit log infrastructure (v0.8.0 P4).
//!
//! Every mutating MCP tool emits an audit row from inside the writer-
//! actor's SQL transaction (atomic with the audited write). Every query
//! emits an audit row via a bounded mpsc channel drained by a dedicated
//! tokio task — async, batched, never blocks the query path.
//!
//! See `docs/dev-log/0090-v0.8.0-implementation-plan.md` §2 Priority 4
//! for the design rationale. ADR-0004 (P7) folds audit emission into
//! the per-tenant writer-actor invariants.
//!
//! ## Modules
//!
//!   * [`log`] — `AuditEvent`, `AuditOperation`, `AuditWriter`,
//!     `purge_older_than`.

pub mod log;

pub use log::{
    AUDIT_BATCH_FLUSH_MAX_EVENTS, AUDIT_BATCH_FLUSH_MAX_MILLIS, AUDIT_QUEUE_CAPACITY, AuditEvent,
    AuditOperation, AuditResult, AuditWriter, AuditWriterShutdown, insert_audit_admin_row,
    insert_audit_row_in_tx, purge_older_than,
};
