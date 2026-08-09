-- SPDX-License-Identifier: Apache-2.0
--
-- Community has one SQLCipher database. Administrative events therefore
-- live beside the Memory Library instead of in a separate registry DB.

CREATE TABLE IF NOT EXISTS audit_events_admin (
    audit_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms             INTEGER NOT NULL,
    principal_subject TEXT,
    operation         TEXT    NOT NULL,
    target_tenant_id  TEXT,
    result            TEXT    NOT NULL
                      CHECK (result IN ('ok','error','forbidden')),
    details_json      TEXT
);

CREATE INDEX IF NOT EXISTS idx_audit_admin_ts
    ON audit_events_admin(ts_ms);
CREATE INDEX IF NOT EXISTS idx_audit_admin_target
    ON audit_events_admin(target_tenant_id);
