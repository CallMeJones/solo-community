// SPDX-License-Identifier: Apache-2.0

//! Opt-in PII redaction registry (v0.8.0 P5).
//!
//! Redaction is a write-time pass inserted into the writer-actor BEFORE
//! the row INSERT — see `crate::writer::handle_remember_durable` and
//! `handle_ingest_document_durable`. Patterns operate on the text that
//! will land in `episodes.content` and `document_chunks.content`; matched
//! substrings are replaced inline with a fixed sentinel of the form
//! `[REDACTED:<pattern_name>]`. The un-redacted text never lands on disk
//! and is never logged in the audit trail.
//!
//! ## What ships in [`builtins`]
//!
//! Six built-in detectors:
//!
//!   * `email`        — RFC 5322-ish addresses (bounded quantifier).
//!   * `ssn`          — US social-security `NNN-NN-NNNN`.
//!   * `us_phone`     — `(NNN) NNN-NNNN` or `NNN-NNN-NNNN`.
//!   * `credit_card`  — 13-19 contiguous digit groups; gated by a Luhn
//!     check so 16-digit non-CCs (UUID literal chunks, etc.) don't trip.
//!   * `aws_access_key` — `AKIA[A-Z0-9]{16}` (AWS IAM access key id).
//!   * `github_pat`   — `gh[pousr]_[A-Za-z0-9]{36,}`.
//!
//! Operators disable specific defaults by listing them in
//! `[redaction] exclude_builtin`; add their own under
//! `[[redaction.custom]]` blocks; flip the whole machinery on with
//! `[redaction] enabled = true` (off by default — opt-in per the locked
//! v0.8.0 design).
//!
//! ## Telemetry contract
//!
//! When a write hits one or more patterns, the writer emits ONE audit
//! row of operation `redaction.applied` with `details_json` of shape
//! `{"matches": [{"pattern_name": "<name>", "count": <N>}, ...]}`. The
//! match counts go in, the matched substrings do NOT. This is asserted
//! by the test `audit_row_does_not_contain_original_pii`.
//!
//! See `docs/dev-log/0090-v0.8.0-implementation-plan.md` §2 Priority 5.

pub mod builtins;
pub mod registry;

pub use registry::{RedactionMatch, RedactionRegistry, RedactionResult};
