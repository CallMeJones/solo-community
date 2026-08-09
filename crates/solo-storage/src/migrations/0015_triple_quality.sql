-- 0015 - entity aliases + quarantined triple review candidates
--
-- Raw memories remain canonical. These tables protect the derived layer:
-- extraction can keep weak triples for review without promoting them into
-- active graph facts, and separator/case aliases can resolve to one entity id.

CREATE TABLE entity_aliases (
    alias         TEXT    PRIMARY KEY,
    canonical_id  TEXT    NOT NULL,
    display_label TEXT    NOT NULL,
    source        TEXT    NOT NULL DEFAULT 'triple_quality_gate',
    confidence    REAL    NOT NULL DEFAULT 1.0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX idx_entity_aliases_canonical ON entity_aliases(canonical_id);

CREATE TABLE triple_reviews (
    review_id             TEXT    PRIMARY KEY,
    candidate_fingerprint TEXT    NOT NULL UNIQUE,
    triple_id             TEXT,
    cluster_id            TEXT,
    source_episode_id     INTEGER REFERENCES episodes(rowid) ON DELETE SET NULL,
    subject_id            TEXT    NOT NULL,
    predicate             TEXT    NOT NULL,
    object_id             TEXT    NOT NULL,
    object_kind           TEXT    NOT NULL CHECK (object_kind IN ('entity','literal')),
    confidence            REAL    NOT NULL,
    reason_code           TEXT    NOT NULL,
    reason                TEXT    NOT NULL,
    provenance_json       TEXT    NOT NULL,
    status                TEXT    NOT NULL DEFAULT 'needs_review'
                          CHECK (status IN ('needs_review','approved','dismissed','rewritten')),
    created_at_ms         INTEGER NOT NULL,
    updated_at_ms         INTEGER NOT NULL,
    reviewed_at_ms        INTEGER,
    review_note           TEXT
);

CREATE INDEX idx_triple_reviews_status_created
    ON triple_reviews(status, created_at_ms DESC);
CREATE INDEX idx_triple_reviews_cluster
    ON triple_reviews(cluster_id);
CREATE INDEX idx_triple_reviews_source_episode
    ON triple_reviews(source_episode_id);
