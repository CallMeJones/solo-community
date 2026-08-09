-- SPDX-License-Identifier: Apache-2.0
--
-- 0005 — per-tenant audit_events table for v0.8.0 Priority 4 (audit log).
--
-- See docs/dev-log/0090-v0.8.0-implementation-plan.md §2 Priority 4 for
-- the full design rationale.
--
-- IMPORTANT — this migration applies to per-tenant SQLCipher DBs at
-- `<data_dir>/tenants/<id>.db`. The admin-tier audit table
-- `audit_events_admin` lives in `tenants_index.db` and was created by
-- migration 0004 (tenants_index chain).
--
-- Pattern conventions mirror 0001_initial.sql + 0004_tenants.sql:
--   * AUTOINCREMENT id (lesson #23 — per-table; doesn't collide with
--     HNSW namespace which keys by episodes/document_chunks rowids).
--   * INTEGER epoch-ms for timestamps.
--   * CHECK constraint on `result` enum.
--   * Two indices: ts_ms (for retention-sweep + recency ordering) and
--     principal_subject (for "what did <user> do" queries).
--
-- The audit row is implicitly tenant-bound by its physical location
-- (one audit_events table per per-tenant DB file). No `tenant_id`
-- column needed; cross-tenant queries are intentionally not supported
-- at the SQL layer (admin tier in v0.8.0 GDPR / hard-delete drops the
-- whole DB file, audit included — exactly the desired GDPR semantics).

CREATE TABLE audit_events (
    audit_id              INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms                 INTEGER NOT NULL,
    principal_subject     TEXT,
    operation             TEXT    NOT NULL,
    target_id             TEXT,
    result                TEXT    NOT NULL
                          CHECK (result IN ('ok','error','forbidden')),
    details_json          TEXT
);

CREATE INDEX idx_audit_ts        ON audit_events(ts_ms);
CREATE INDEX idx_audit_principal ON audit_events(principal_subject);
