-- 0017 - temporal entities and relationship edges v2 groundwork
--
-- Keep the existing triples API canonical for now, but add first-class graph
-- tables that can answer "which relationship is this?" and "what evidence
-- backs it?". This migration intentionally starts with one edge per active
-- triple (edge_id == triple_id); later reconsolidation can merge multiple
-- evidence rows into stronger aggregate edges without changing the triples
-- compatibility surface.

CREATE TABLE entities (
    entity_id       TEXT    PRIMARY KEY,
    canonical_name  TEXT    NOT NULL,
    entity_type     TEXT    NOT NULL DEFAULT 'unknown',
    aliases_json    TEXT    NOT NULL DEFAULT '[]',
    confidence      REAL    NOT NULL DEFAULT 1.0,
    first_seen_ms   INTEGER,
    last_seen_ms    INTEGER,
    status          TEXT    NOT NULL DEFAULT 'active'
                            CHECK (status IN ('candidate','active','merged','superseded','rejected','forgotten')),
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);

CREATE INDEX idx_entities_status_seen
    ON entities(status, last_seen_ms DESC);
CREATE INDEX idx_entities_canonical_name
    ON entities(canonical_name);

CREATE TABLE relationship_edges (
    edge_id           TEXT    PRIMARY KEY,
    subject_entity_id TEXT    NOT NULL REFERENCES entities(entity_id) ON DELETE CASCADE,
    predicate         TEXT    NOT NULL,
    object_entity_id  TEXT    REFERENCES entities(entity_id) ON DELETE CASCADE,
    object_literal    TEXT,
    object_kind       TEXT    NOT NULL CHECK (object_kind IN ('entity','literal')),
    valid_from_ms     INTEGER NOT NULL,
    valid_to_ms       INTEGER,
    confidence        REAL    NOT NULL,
    strength          REAL    NOT NULL DEFAULT 1.0,
    evidence_count    INTEGER NOT NULL DEFAULT 0,
    status            TEXT    NOT NULL DEFAULT 'active'
                              CHECK (status IN ('candidate','active','superseded','contradicted','rejected')),
    created_at_ms     INTEGER NOT NULL,
    updated_at_ms     INTEGER NOT NULL,
    CHECK (
        (object_kind = 'entity' AND object_entity_id IS NOT NULL AND object_literal IS NULL)
        OR
        (object_kind = 'literal' AND object_entity_id IS NULL AND object_literal IS NOT NULL)
    )
);

CREATE INDEX idx_relationship_edges_subject
    ON relationship_edges(subject_entity_id, predicate, status);
CREATE INDEX idx_relationship_edges_object_entity
    ON relationship_edges(object_entity_id, predicate, status);
CREATE INDEX idx_relationship_edges_valid_window
    ON relationship_edges(valid_from_ms, valid_to_ms);
CREATE INDEX idx_relationship_edges_status_strength
    ON relationship_edges(status, strength DESC, updated_at_ms DESC);

CREATE TABLE relationship_evidence (
    evidence_id           TEXT    PRIMARY KEY,
    edge_id               TEXT    NOT NULL REFERENCES relationship_edges(edge_id) ON DELETE CASCADE,
    triple_id             TEXT    NOT NULL UNIQUE REFERENCES triples(triple_id) ON DELETE CASCADE,
    memory_id             TEXT    REFERENCES episodes(memory_id) ON DELETE SET NULL,
    source_episode_id     INTEGER REFERENCES episodes(rowid) ON DELETE SET NULL,
    doc_id                TEXT    REFERENCES documents(doc_id) ON DELETE SET NULL,
    chunk_id              TEXT    REFERENCES document_chunks(chunk_id) ON DELETE SET NULL,
    cluster_id            TEXT    REFERENCES clusters(cluster_id) ON DELETE SET NULL,
    extraction_confidence REAL    NOT NULL,
    created_at_ms         INTEGER NOT NULL
);

CREATE INDEX idx_relationship_evidence_edge
    ON relationship_evidence(edge_id);
CREATE INDEX idx_relationship_evidence_memory
    ON relationship_evidence(memory_id);
CREATE INDEX idx_relationship_evidence_source_episode
    ON relationship_evidence(source_episode_id);
CREATE INDEX idx_relationship_evidence_cluster
    ON relationship_evidence(cluster_id);

CREATE TRIGGER relationship_evidence_after_insert
AFTER INSERT ON relationship_evidence
BEGIN
    UPDATE relationship_edges
       SET evidence_count = (
               SELECT COUNT(*)
                 FROM relationship_evidence
                WHERE edge_id = NEW.edge_id
           ),
           updated_at_ms = NEW.created_at_ms
     WHERE edge_id = NEW.edge_id;
END;

CREATE TRIGGER relationship_evidence_after_delete
AFTER DELETE ON relationship_evidence
BEGIN
    UPDATE relationship_edges
       SET evidence_count = (
               SELECT COUNT(*)
                 FROM relationship_evidence
                WHERE edge_id = OLD.edge_id
           ),
           updated_at_ms = CAST(strftime('%s', 'now') AS INTEGER) * 1000
     WHERE edge_id = OLD.edge_id;

    DELETE FROM relationship_edges
     WHERE edge_id = OLD.edge_id
       AND NOT EXISTS (
           SELECT 1
             FROM relationship_evidence
            WHERE edge_id = OLD.edge_id
       );
END;

-- Backfill active triples into the entity table.
INSERT OR IGNORE INTO entities (
    entity_id, canonical_name, entity_type, aliases_json, confidence,
    first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms
)
SELECT t.subject_id,
       COALESCE(
           (SELECT ea.display_label
              FROM entity_aliases ea
             WHERE ea.canonical_id = t.subject_id
             ORDER BY ea.confidence DESC, ea.updated_at_ms DESC
             LIMIT 1),
           t.subject_id
       ),
       'unknown',
       '[]',
       MAX(t.confidence),
       MIN(t.valid_from_ms),
       MAX(COALESCE(t.valid_to_ms, t.valid_from_ms)),
       'active',
       MIN(t.created_at_ms),
       MAX(t.updated_at_ms)
  FROM triples t
 WHERE t.status = 'active'
 GROUP BY t.subject_id;

INSERT OR IGNORE INTO entities (
    entity_id, canonical_name, entity_type, aliases_json, confidence,
    first_seen_ms, last_seen_ms, status, created_at_ms, updated_at_ms
)
SELECT t.object_id,
       COALESCE(
           (SELECT ea.display_label
              FROM entity_aliases ea
             WHERE ea.canonical_id = t.object_id
             ORDER BY ea.confidence DESC, ea.updated_at_ms DESC
             LIMIT 1),
           t.object_id
       ),
       'unknown',
       '[]',
       MAX(t.confidence),
       MIN(t.valid_from_ms),
       MAX(COALESCE(t.valid_to_ms, t.valid_from_ms)),
       'active',
       MIN(t.created_at_ms),
       MAX(t.updated_at_ms)
  FROM triples t
 WHERE t.status = 'active'
   AND t.object_kind = 'entity'
 GROUP BY t.object_id;

-- Backfill one relationship edge per active triple.
INSERT OR IGNORE INTO relationship_edges (
    edge_id, subject_entity_id, predicate, object_entity_id, object_literal,
    object_kind, valid_from_ms, valid_to_ms, confidence, strength,
    evidence_count, status, created_at_ms, updated_at_ms
)
SELECT t.triple_id,
       t.subject_id,
       t.predicate,
       CASE WHEN t.object_kind = 'entity' THEN t.object_id ELSE NULL END,
       CASE WHEN t.object_kind = 'literal' THEN t.object_id ELSE NULL END,
       t.object_kind,
       t.valid_from_ms,
       t.valid_to_ms,
       t.confidence,
       t.confidence,
       0,
       'active',
       t.created_at_ms,
       t.updated_at_ms
  FROM triples t
 WHERE t.status = 'active';

INSERT OR IGNORE INTO relationship_evidence (
    evidence_id, edge_id, triple_id, memory_id, source_episode_id,
    cluster_id, extraction_confidence, created_at_ms
)
SELECT t.triple_id,
       t.triple_id,
       t.triple_id,
       e.memory_id,
       t.source_episode_id,
       t.cluster_id,
       t.confidence,
       t.created_at_ms
  FROM triples t
  LEFT JOIN episodes e ON e.rowid = t.source_episode_id
 WHERE t.status = 'active';
