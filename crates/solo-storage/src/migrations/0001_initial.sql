-- SPDX-License-Identifier: Apache-2.0
--
-- Solo v0 initial schema.
--
-- Conventions:
--   * IDs are UUID v7 stored as TEXT (16 hex bytes is more compact but harder
--     to debug; we'll binary-pack in a future migration if it matters).
--   * Timestamps are epoch-ms (`INTEGER`, fits i64).
--   * Nested structures (provenance, encoding_context) are JSON in TEXT
--     columns. The storage layer validates shape on insert; SQLite does not.
--   * Enum-like fields use CHECK constraints so an invalid `tier`/`status`
--     can't sneak in via raw SQL.
--   * `rowid` is exposed as an explicit `INTEGER PRIMARY KEY AUTOINCREMENT`
--     because the HNSW sidecar keys on it; we want stable rowids that don't
--     get reused after a delete.
--
-- See ADR-0002 for the rationale behind the type shapes and ADR-0003 for the
-- pending_index design.

-- Embedder registry. Every embedder model that has produced vectors in this
-- database has a row here. `solo reembed` reads (name, version) to decide
-- what needs regenerating after a model upgrade.
CREATE TABLE embedders (
    embedder_id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT    NOT NULL,
    version       TEXT    NOT NULL,
    dim           INTEGER NOT NULL,
    dtype         TEXT    NOT NULL CHECK (dtype IN ('f32','f16','i8','binary')),
    first_seen_ms INTEGER NOT NULL,
    UNIQUE(name, version)
);

-- Episodic memories: time-keyed events with full encoding context.
-- See `solo_core::Episode`.
CREATE TABLE episodes (
    rowid                 INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id             TEXT    NOT NULL UNIQUE,
    ts_ms                 INTEGER NOT NULL,
    source_type           TEXT    NOT NULL,
    source_id             TEXT,
    content               TEXT    NOT NULL,
    encoding_context_json TEXT    NOT NULL DEFAULT '{}',
    provenance_json       TEXT,
    confidence            REAL    NOT NULL,
    strength              REAL    NOT NULL,
    salience              REAL    NOT NULL,
    tier                  TEXT    NOT NULL CHECK (tier IN ('hot','warm','cold')),
    status                TEXT    NOT NULL DEFAULT 'active'
                          CHECK (status IN ('active','forgotten')),
    created_at_ms         INTEGER NOT NULL,
    updated_at_ms         INTEGER NOT NULL
);

CREATE INDEX idx_episodes_ts          ON episodes(ts_ms);
CREATE INDEX idx_episodes_tier_status ON episodes(tier, status);
CREATE INDEX idx_episodes_source      ON episodes(source_type, source_id);

-- Per-episode embeddings. One row per (episode, embedder) pair so re-embedding
-- with a new model doesn't bloat the episodes table itself. The HNSW sidecar
-- only indexes the *active* embedder's vectors; older ones are kept for
-- comparison until the user runs `solo reembed --gc`.
CREATE TABLE embeddings (
    rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
    memory_id      TEXT    NOT NULL REFERENCES episodes(memory_id) ON DELETE CASCADE,
    embedder_id    INTEGER NOT NULL REFERENCES embedders(embedder_id),
    dtype          TEXT    NOT NULL CHECK (dtype IN ('f32','f16','i8','binary')),
    dim            INTEGER NOT NULL,
    vector         BLOB    NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    UNIQUE(memory_id, embedder_id)
);

CREATE INDEX idx_embeddings_memory ON embeddings(memory_id);

-- Outbox table per ADR-0003. Rows queue between SQL COMMIT and HNSW.add;
-- writer drains immediately on success. Steady-state size is ~1 row;
-- non-zero counts at startup mean recovery is needed (see ADR-0003 §replay).
CREATE TABLE pending_index (
    memory_id     TEXT    PRIMARY KEY REFERENCES episodes(memory_id) ON DELETE CASCADE,
    embedding     BLOB    NOT NULL,
    embedding_dim INTEGER NOT NULL,
    enqueued_at   INTEGER NOT NULL
);

CREATE INDEX idx_pending_enqueued ON pending_index(enqueued_at);

-- Semantic-memory triples: (subject, predicate, object) with bi-temporal
-- validity windows. See `solo_core::Triple`.
CREATE TABLE triples (
    rowid           INTEGER PRIMARY KEY AUTOINCREMENT,
    triple_id       TEXT    NOT NULL UNIQUE,
    subject_id      TEXT    NOT NULL,
    predicate       TEXT    NOT NULL,
    object_id       TEXT    NOT NULL,
    object_kind     TEXT    NOT NULL CHECK (object_kind IN ('entity','literal')),
    valid_from_ms   INTEGER NOT NULL,
    valid_to_ms     INTEGER,
    confidence      REAL    NOT NULL,
    provenance_json TEXT    NOT NULL,
    status          TEXT    NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active','superseded','forgotten')),
    created_at_ms   INTEGER NOT NULL,
    updated_at_ms   INTEGER NOT NULL
);

CREATE INDEX idx_triples_subject_predicate ON triples(subject_id, predicate);
CREATE INDEX idx_triples_object            ON triples(object_id, object_kind);
CREATE INDEX idx_triples_valid_window      ON triples(valid_from_ms, valid_to_ms);

-- Steward outputs --------------------------------------------------------
-- See `solo_core::Cluster` / `SemanticAbstraction` / `Contradiction`.

CREATE TABLE clusters (
    rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
    cluster_id     TEXT    NOT NULL UNIQUE,
    centroid       BLOB,
    centroid_dtype TEXT    CHECK (centroid_dtype IN ('f32','f16','i8','binary')),
    centroid_dim   INTEGER,
    coherence      REAL    NOT NULL,
    created_at_ms  INTEGER NOT NULL
);

CREATE TABLE cluster_episodes (
    cluster_id TEXT NOT NULL REFERENCES clusters(cluster_id)  ON DELETE CASCADE,
    memory_id  TEXT NOT NULL REFERENCES episodes(memory_id)   ON DELETE CASCADE,
    PRIMARY KEY (cluster_id, memory_id)
);

CREATE INDEX idx_cluster_episodes_memory ON cluster_episodes(memory_id);

CREATE TABLE semantic_abstractions (
    rowid           INTEGER PRIMARY KEY AUTOINCREMENT,
    abstraction_id  TEXT    NOT NULL UNIQUE,
    cluster_id      TEXT    NOT NULL REFERENCES clusters(cluster_id) ON DELETE CASCADE,
    content         TEXT    NOT NULL,
    provenance_json TEXT    NOT NULL,
    confidence      REAL    NOT NULL,
    created_at_ms   INTEGER NOT NULL
);

CREATE INDEX idx_abstractions_cluster ON semantic_abstractions(cluster_id);

CREATE TABLE contradictions (
    rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
    a_memory_id    TEXT    NOT NULL,
    b_memory_id    TEXT    NOT NULL,
    kind           TEXT    NOT NULL CHECK (kind IN (
                       'overlapping_single_valued_predicate',
                       'direct_negation',
                       'numeric_inconsistency',
                       'other'
                   )),
    explanation    TEXT    NOT NULL,
    detected_at_ms INTEGER NOT NULL,
    UNIQUE(a_memory_id, b_memory_id, kind)
);

CREATE INDEX idx_contradictions_a ON contradictions(a_memory_id);
CREATE INDEX idx_contradictions_b ON contradictions(b_memory_id);

-- FTS5 over episodes.content for the keyword recall path.
CREATE VIRTUAL TABLE episodes_fts USING fts5(
    content,
    content='episodes',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

-- Triggers keep the FTS index in sync with the episodes table.
CREATE TRIGGER episodes_fts_ai AFTER INSERT ON episodes BEGIN
    INSERT INTO episodes_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER episodes_fts_ad AFTER DELETE ON episodes BEGIN
    INSERT INTO episodes_fts(episodes_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
END;

CREATE TRIGGER episodes_fts_au AFTER UPDATE OF content ON episodes BEGIN
    INSERT INTO episodes_fts(episodes_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
    INSERT INTO episodes_fts(rowid, content) VALUES (new.rowid, new.content);
END;
