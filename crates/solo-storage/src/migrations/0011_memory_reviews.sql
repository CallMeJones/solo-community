-- SPDX-License-Identifier: Apache-2.0

-- Memory Inbox review state.
--
-- Review state belongs to the one Community Memory Library. A memory missing from this
-- table is still "needs_review"; approved/dismissed rows are explicit
-- review decisions.

CREATE TABLE memory_reviews (
    memory_id      TEXT    PRIMARY KEY REFERENCES episodes(memory_id) ON DELETE CASCADE,
    state          TEXT    NOT NULL CHECK (state IN ('approved', 'dismissed')),
    reviewed_at_ms INTEGER NOT NULL,
    note           TEXT,
    created_at_ms  INTEGER NOT NULL,
    updated_at_ms  INTEGER NOT NULL
);

CREATE INDEX idx_memory_reviews_state
    ON memory_reviews(state, reviewed_at_ms DESC);

CREATE INDEX idx_memory_reviews_reviewed_at
    ON memory_reviews(reviewed_at_ms DESC);
