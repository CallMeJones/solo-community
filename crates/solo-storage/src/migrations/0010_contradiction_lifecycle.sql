-- SPDX-License-Identifier: Apache-2.0
--
-- 0010 - contradiction lifecycle state.
--
-- Contradictions were originally append-only flags. This adds the minimum
-- lifecycle fields needed for agents and users to mark one as resolved while
-- keeping the original detection record for auditability.

ALTER TABLE contradictions
    ADD COLUMN status TEXT NOT NULL DEFAULT 'unresolved'
    CHECK (status IN ('unresolved','resolved','reopened'));

ALTER TABLE contradictions
    ADD COLUMN resolved_at_ms INTEGER;

ALTER TABLE contradictions
    ADD COLUMN resolution_note TEXT;

ALTER TABLE contradictions
    ADD COLUMN winning_triple_id TEXT;

CREATE INDEX idx_contradictions_status ON contradictions(status, detected_at_ms);
