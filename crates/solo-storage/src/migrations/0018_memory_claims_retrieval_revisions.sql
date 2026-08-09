-- 0018 - memory claims, retrieval log, revisions, and entity review ops
--
-- Completes the storage groundwork for Phase 6/7:
--   * memory_claims: claim-first quality gate state before activation.
--   * memory_retrieval_log: explainable recall/retrieval audit trail.
--   * memory_revisions: supersession/reconsolidation history.
--   * entity_review_ops: merge/split review operation ledger.

CREATE TABLE memory_claims (
    claim_id              TEXT    PRIMARY KEY,
    candidate_fingerprint TEXT    NOT NULL UNIQUE,
    triple_id             TEXT,
    review_id             TEXT,
    subject_id            TEXT    NOT NULL,
    predicate             TEXT    NOT NULL,
    object_id             TEXT    NOT NULL,
    object_kind           TEXT    NOT NULL CHECK (object_kind IN ('entity','literal')),
    source_type           TEXT    NOT NULL DEFAULT 'steward_triple',
    cluster_id            TEXT,
    source_episode_id     INTEGER REFERENCES episodes(rowid) ON DELETE SET NULL,
    doc_id                TEXT    REFERENCES documents(doc_id) ON DELETE SET NULL,
    chunk_id              TEXT    REFERENCES document_chunks(chunk_id) ON DELETE SET NULL,
    confidence            REAL    NOT NULL,
    quality_score         REAL    NOT NULL,
    status                TEXT    NOT NULL
                                  CHECK (status IN (
                                      'candidate',
                                      'active',
                                      'needs_review',
                                      'rejected',
                                      'quarantined',
                                      'superseded'
                                  )),
    reason_codes_json     TEXT    NOT NULL DEFAULT '[]',
    evidence_count        INTEGER NOT NULL DEFAULT 1,
    user_approved         INTEGER NOT NULL DEFAULT 0 CHECK (user_approved IN (0,1)),
    created_at_ms         INTEGER NOT NULL,
    activated_at_ms       INTEGER,
    reviewed_at_ms        INTEGER,
    updated_at_ms         INTEGER NOT NULL,
    provenance_json       TEXT    NOT NULL DEFAULT '{}'
);

CREATE INDEX idx_memory_claims_status_score
    ON memory_claims(status, quality_score DESC, updated_at_ms DESC);
CREATE INDEX idx_memory_claims_subject
    ON memory_claims(subject_id, predicate, status);
CREATE INDEX idx_memory_claims_review
    ON memory_claims(review_id);
CREATE INDEX idx_memory_claims_triple
    ON memory_claims(triple_id);

CREATE TABLE memory_retrieval_log (
    retrieval_id      TEXT    PRIMARY KEY,
    query             TEXT    NOT NULL,
    recalled_ids_json TEXT    NOT NULL DEFAULT '[]',
    reason_codes_json TEXT    NOT NULL DEFAULT '[]',
    created_at_ms     INTEGER NOT NULL
);

CREATE INDEX idx_memory_retrieval_log_created
    ON memory_retrieval_log(created_at_ms DESC);

CREATE TABLE memory_revisions (
    revision_id       TEXT    PRIMARY KEY,
    revision_kind     TEXT    NOT NULL
                              CHECK (revision_kind IN (
                                  'claim_activated',
                                  'claim_rejected',
                                  'claim_rewritten',
                                  'entity_merged',
                                  'entity_split_requested',
                                  'reconsolidated'
                              )),
    target_kind       TEXT    NOT NULL,
    target_id         TEXT    NOT NULL,
    previous_id       TEXT,
    replacement_id    TEXT,
    reason            TEXT,
    metadata_json     TEXT    NOT NULL DEFAULT '{}',
    created_at_ms     INTEGER NOT NULL
);

CREATE INDEX idx_memory_revisions_target
    ON memory_revisions(target_kind, target_id, created_at_ms DESC);
CREATE INDEX idx_memory_revisions_created
    ON memory_revisions(created_at_ms DESC);

CREATE TABLE entity_review_ops (
    op_id             TEXT    PRIMARY KEY,
    op_kind           TEXT    NOT NULL CHECK (op_kind IN ('merge','split')),
    status            TEXT    NOT NULL DEFAULT 'needs_review'
                              CHECK (status IN ('needs_review','applied','dismissed')),
    source_entity_id  TEXT    NOT NULL,
    target_entity_id  TEXT,
    affected_aliases_json TEXT NOT NULL DEFAULT '[]',
    reason            TEXT,
    created_at_ms     INTEGER NOT NULL,
    applied_at_ms     INTEGER,
    updated_at_ms     INTEGER NOT NULL
);

CREATE INDEX idx_entity_review_ops_status_created
    ON entity_review_ops(status, created_at_ms DESC);
CREATE INDEX idx_entity_review_ops_source
    ON entity_review_ops(source_entity_id);

INSERT OR IGNORE INTO memory_claims (
    claim_id, candidate_fingerprint, triple_id, review_id, subject_id,
    predicate, object_id, object_kind, source_type, cluster_id,
    source_episode_id, confidence, quality_score, status, reason_codes_json,
    evidence_count, user_approved, created_at_ms, activated_at_ms,
    reviewed_at_ms, updated_at_ms, provenance_json
)
SELECT 'mc-active-' || t.triple_id,
       'active|' || t.triple_id,
       t.triple_id,
       NULL,
       t.subject_id,
       t.predicate,
       t.object_id,
       t.object_kind,
       'legacy_triple',
       t.cluster_id,
       t.source_episode_id,
       t.confidence,
       MIN(1.0, MAX(0.0, t.confidence)),
       'active',
       '["legacy_active_triple","activated"]',
       1,
       0,
       t.created_at_ms,
       t.created_at_ms,
       NULL,
       t.updated_at_ms,
       t.provenance_json
  FROM triples t
 WHERE t.status = 'active';

INSERT OR IGNORE INTO memory_claims (
    claim_id, candidate_fingerprint, triple_id, review_id, subject_id,
    predicate, object_id, object_kind, source_type, cluster_id,
    source_episode_id, confidence, quality_score, status, reason_codes_json,
    evidence_count, user_approved, created_at_ms, activated_at_ms,
    reviewed_at_ms, updated_at_ms, provenance_json
)
SELECT 'mc-' || tr.candidate_fingerprint,
       tr.candidate_fingerprint,
       tr.triple_id,
       tr.review_id,
       tr.subject_id,
       tr.predicate,
       tr.object_id,
       tr.object_kind,
       'legacy_review',
       tr.cluster_id,
       tr.source_episode_id,
       tr.confidence,
       CASE tr.reason_code
           WHEN 'assistant_or_tool_chatter' THEN MAX(0.0, tr.confidence - 0.45)
           WHEN 'long_machine_blob' THEN MAX(0.0, tr.confidence - 0.40)
           WHEN 'metadata_subject' THEN MAX(0.0, tr.confidence - 0.35)
           WHEN 'incomplete_shape' THEN MAX(0.0, tr.confidence - 0.35)
           WHEN 'invalid_object_kind' THEN MAX(0.0, tr.confidence - 0.35)
           WHEN 'low_confidence' THEN MAX(0.0, tr.confidence - 0.25)
           ELSE MAX(0.0, tr.confidence - 0.20)
       END,
       CASE
           WHEN tr.status = 'approved' THEN 'active'
           WHEN tr.status = 'dismissed' THEN 'rejected'
           WHEN tr.status = 'rewritten' THEN 'superseded'
           WHEN tr.reason_code IN ('assistant_or_tool_chatter','long_machine_blob') THEN 'quarantined'
           WHEN tr.reason_code IN ('metadata_subject','incomplete_shape','invalid_object_kind') THEN 'rejected'
           ELSE 'needs_review'
       END,
       '["legacy_review","quality_gate"]',
       1,
       CASE WHEN tr.status = 'approved' THEN 1 ELSE 0 END,
       tr.created_at_ms,
       CASE WHEN tr.status = 'approved' THEN tr.reviewed_at_ms ELSE NULL END,
       tr.reviewed_at_ms,
       tr.updated_at_ms,
       tr.provenance_json
  FROM triple_reviews tr;
