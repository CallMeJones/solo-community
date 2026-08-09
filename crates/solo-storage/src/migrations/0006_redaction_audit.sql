-- SPDX-License-Identifier: Apache-2.0
--
-- 0006 — per-tenant principal-attribution columns for v0.8.0 P5 + P6.
--
-- See docs/dev-log/0090-v0.8.0-implementation-plan.md §2 Priority 5 + 6
-- (PII redaction + GDPR right-to-erasure). 0092 dev log carries the
-- session-specific notes.
--
-- IMPORTANT — this migration applies to per-tenant SQLCipher DBs at
-- `<data_dir>/tenants/<id>.db` (the `MIGRATIONS` chain in `migration.rs`).
-- The tenants_index.db chain is independent.
--
-- ## Why this column lives on the data tables, not in audit_events
--
-- v0.8.0 P4 (migration 0005) added `audit_events.principal_subject` so
-- compliance can answer "who did this operation". For GDPR forget_user
-- we need the inverse question — given a principal, find every row of
-- THEIR data — so the principal must live next to the data, not next to
-- the audit row. Without this, `forget_principal` would have to walk
-- audit_events to find target_ids belonging to a subject, then back-
-- reference each one — slow and racy against retention purges.
--
-- ## Why two separate columns instead of one shared one
--
-- Episodes already use `principal_subject` (matches the audit table's
-- naming and the `AuthenticatedPrincipal.subject` field at the API
-- boundary). Document chunks pick up the same data via the principal
-- whose ingest_document call wrote them, but the column name
-- `ingested_by_principal` distinguishes "who ingested this document"
-- from "who is this document about" — the latter is a v0.9+ concern.
-- Forward-going writes populate both. Backward-compat: nullable so
-- pre-0006 rows survive unchanged; GDPR forget on a fresh tenant with
-- no pre-0006 rows is exhaustive.

-- --------------------------------------------------------------------------
-- episodes — principal_subject column + index
-- --------------------------------------------------------------------------

ALTER TABLE episodes ADD COLUMN principal_subject TEXT;
CREATE INDEX idx_episodes_principal ON episodes(principal_subject);

-- --------------------------------------------------------------------------
-- document_chunks — ingested_by_principal column + index
-- --------------------------------------------------------------------------

ALTER TABLE document_chunks ADD COLUMN ingested_by_principal TEXT;
CREATE INDEX idx_chunks_principal ON document_chunks(ingested_by_principal);
