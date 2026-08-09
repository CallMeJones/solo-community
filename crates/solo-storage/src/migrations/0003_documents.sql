-- SPDX-License-Identifier: Apache-2.0
--
-- 0003 — documents + document_chunks for RAG / document-memory.
--
-- See docs/dev-log/0083-v0.7.0-implementation-plan.md §2 Priority 1.
--
-- Adds:
--   * `documents` table — per-ingestion metadata
--   * `document_chunks` table — ~500-token chunks with byte-offset windows
--   * `document_chunks_fts` virtual table + 3 sync triggers (FTS5 over chunk content)
--   * `chunk_embeddings` table — (chunk_id, embedder_id) → BLOB
--     Option B chosen (separate table, not extending `embeddings`) because it
--     leaves the existing episode-embedding read/write paths untouched. The
--     long-term Option A (one unified `embeddings` table with NULLable
--     memory_id / chunk_id + XOR check) is cleaner conceptually but introduces
--     a NULLable churn on the heavily-used existing `embeddings` table; we'll
--     revisit if a future migration needs the unification.
--   * `pending_index` rebuild to discriminate episode vs chunk outbox rows
--     via a new `kind` column. Pre-0003 rows back-fill as kind='episode'.
--     This rebuild is safe because steady-state pending_index size is ~0
--     (rows drain immediately after `hnsw.add` succeeds).
--
-- Chunks share the existing HNSW namespace with episodes — chunk rowids and
-- episode rowids both index into `hnsw_episodes`. The replay path in
-- recovery.rs disambiguates via `pending_index.kind` (extended in P3).
--
-- Pattern conventions mirror 0001_initial.sql:
--   * UUIDv7 in TEXT for stable identifiers
--   * INTEGER epoch-ms for timestamps
--   * CHECK constraints for enum-like fields
--   * Explicit `rowid INTEGER PRIMARY KEY AUTOINCREMENT` where rowids feed HNSW
--   * FTS5 with `porter unicode61` tokenizer + `content=` external-content mode
--   * Triggers keep FTS sync on INSERT / DELETE / UPDATE of indexed column

-- --------------------------------------------------------------------------
-- documents
-- --------------------------------------------------------------------------

CREATE TABLE documents (
    doc_id          TEXT    PRIMARY KEY NOT NULL,
    source          TEXT,                            -- file path, URL, or other origin string
    title           TEXT,                            -- first heading, filename, or supplied label
    mime_type       TEXT,                            -- e.g. text/markdown, application/pdf, text/html
    ingested_at_ms  INTEGER NOT NULL,
    modified_at_ms  INTEGER,                         -- file mtime if known at ingest
    status          TEXT    NOT NULL DEFAULT 'active'
                    CHECK (status IN ('active','forgotten')),
    chunk_count     INTEGER NOT NULL DEFAULT 0,      -- denormalized count; writer maintains it
    content_hash    TEXT,                            -- sha256 of normalized text for dedup
    byte_size       INTEGER                          -- raw bytes of the ingested source
);

CREATE INDEX idx_documents_status       ON documents(status);
CREATE INDEX idx_documents_source       ON documents(source);
CREATE INDEX idx_documents_content_hash ON documents(content_hash);

-- --------------------------------------------------------------------------
-- document_chunks
-- --------------------------------------------------------------------------

CREATE TABLE document_chunks (
    rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
    chunk_id       TEXT    NOT NULL UNIQUE,
    doc_id         TEXT    NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    chunk_index    INTEGER NOT NULL,
    content        TEXT    NOT NULL,
    token_count    INTEGER NOT NULL,
    start_offset   INTEGER NOT NULL,
    end_offset     INTEGER NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    UNIQUE (doc_id, chunk_index)
);

CREATE INDEX idx_chunks_doc_id ON document_chunks(doc_id);

-- --------------------------------------------------------------------------
-- document_chunks_fts (FTS5 over chunk content)
--
-- Mirrors the episodes_fts pattern from 0001: external-content mode where
-- the FTS index points back at `document_chunks` via `content_rowid='rowid'`.
-- --------------------------------------------------------------------------

CREATE VIRTUAL TABLE document_chunks_fts USING fts5(
    content,
    content='document_chunks',
    content_rowid='rowid',
    tokenize='porter unicode61'
);

CREATE TRIGGER document_chunks_fts_ai AFTER INSERT ON document_chunks BEGIN
    INSERT INTO document_chunks_fts(rowid, content) VALUES (new.rowid, new.content);
END;

CREATE TRIGGER document_chunks_fts_ad AFTER DELETE ON document_chunks BEGIN
    INSERT INTO document_chunks_fts(document_chunks_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
END;

CREATE TRIGGER document_chunks_fts_au AFTER UPDATE OF content ON document_chunks BEGIN
    INSERT INTO document_chunks_fts(document_chunks_fts, rowid, content)
    VALUES ('delete', old.rowid, old.content);
    INSERT INTO document_chunks_fts(rowid, content) VALUES (new.rowid, new.content);
END;

-- --------------------------------------------------------------------------
-- chunk_embeddings (Option B — separate table from episodes' `embeddings`)
--
-- One row per (chunk, embedder). Mirrors the shape of the existing
-- `embeddings` table: dtype + dim self-describing, FK CASCADE on the chunk.
-- --------------------------------------------------------------------------

CREATE TABLE chunk_embeddings (
    rowid          INTEGER PRIMARY KEY AUTOINCREMENT,
    chunk_id       TEXT    NOT NULL REFERENCES document_chunks(chunk_id) ON DELETE CASCADE,
    embedder_id    INTEGER NOT NULL REFERENCES embedders(embedder_id),
    dtype          TEXT    NOT NULL CHECK (dtype IN ('f32','f16','i8','binary')),
    dim            INTEGER NOT NULL,
    vector         BLOB    NOT NULL,
    created_at_ms  INTEGER NOT NULL,
    UNIQUE (chunk_id, embedder_id)
);

CREATE INDEX idx_chunk_embeddings_chunk ON chunk_embeddings(chunk_id);

-- --------------------------------------------------------------------------
-- pending_index rebuild — add `kind` discriminator + nullable `chunk_id`.
--
-- The original 0001 shape has `memory_id` as PRIMARY KEY NOT NULL with FK
-- to `episodes`. To let chunks share the same outbox, we rebuild the table
-- so:
--   * `rowid` is the primary key (mirrors episodes / document_chunks)
--   * `memory_id` becomes nullable, still FK CASCADE on episodes
--   * `chunk_id` is added, nullable, FK CASCADE on document_chunks
--   * `kind` is the discriminator with CHECK constraint
--   * A CHECK constraint enforces XOR: exactly one of memory_id / chunk_id
--     is non-NULL, and `kind` matches whichever is set
--   * UNIQUE constraints on (memory_id) and (chunk_id) preserve at-most-one
--     outbox-row-per-source-row semantics (SQLite treats NULLs as distinct
--     in UNIQUE, so multiple chunk rows can have memory_id NULL and vice
--     versa — the partial uniqueness is exactly what we want)
--
-- Pre-0003 rows are preserved with `kind='episode'` and `chunk_id=NULL`.
-- Steady-state pending_index size is ~0 in practice (rows drain immediately
-- after hnsw.add succeeds, per ADR-0003), so the rebuild copies almost
-- nothing.
-- --------------------------------------------------------------------------

CREATE TABLE pending_index_new (
    rowid         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind          TEXT    NOT NULL DEFAULT 'episode'
                          CHECK (kind IN ('episode','chunk')),
    memory_id     TEXT    UNIQUE REFERENCES episodes(memory_id)         ON DELETE CASCADE,
    chunk_id      TEXT    UNIQUE REFERENCES document_chunks(chunk_id)   ON DELETE CASCADE,
    embedding     BLOB    NOT NULL,
    embedding_dim INTEGER NOT NULL,
    enqueued_at   INTEGER NOT NULL,
    -- XOR: exactly one of memory_id / chunk_id is set, and `kind` agrees.
    CHECK (
        (kind = 'episode' AND memory_id IS NOT NULL AND chunk_id IS NULL)
        OR
        (kind = 'chunk'   AND chunk_id  IS NOT NULL AND memory_id IS NULL)
    )
);

-- Copy any existing (pre-0003) rows, defaulting kind='episode'.
INSERT INTO pending_index_new (kind, memory_id, chunk_id, embedding, embedding_dim, enqueued_at)
SELECT 'episode', memory_id, NULL, embedding, embedding_dim, enqueued_at
FROM pending_index;

DROP TABLE pending_index;
ALTER TABLE pending_index_new RENAME TO pending_index;

CREATE INDEX idx_pending_enqueued ON pending_index(enqueued_at);
CREATE INDEX idx_pending_kind     ON pending_index(kind);
