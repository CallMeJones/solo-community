-- SPDX-License-Identifier: Apache-2.0

-- Persistent raw-file assets and first-class memory attachments.
--
-- Assets are metadata rows for content-addressed files stored on disk under
-- the tenant-owned snapshot directory. SQLite stores searchable metadata and
-- relationships; raw bytes stay outside the encrypted DB so 100 MiB uploads do
-- not bloat WAL/checkpoint behavior.

CREATE TABLE assets (
    asset_id             TEXT    PRIMARY KEY NOT NULL,
    sha256               TEXT    NOT NULL UNIQUE,
    mime_type            TEXT    NOT NULL DEFAULT 'application/octet-stream',
    filename             TEXT,
    size_bytes           INTEGER NOT NULL CHECK (size_bytes >= 0),
    storage_path         TEXT    NOT NULL UNIQUE,
    source               TEXT,
    status               TEXT    NOT NULL DEFAULT 'active'
                         CHECK (status IN ('active','deleted')),
    created_by_principal TEXT,
    created_at_ms        INTEGER NOT NULL,
    updated_at_ms        INTEGER NOT NULL
);

CREATE INDEX idx_assets_status_created
    ON assets(status, created_at_ms DESC);

CREATE INDEX idx_assets_filename
    ON assets(filename);

CREATE TABLE document_assets (
    link_id       TEXT    PRIMARY KEY NOT NULL,
    doc_id        TEXT    NOT NULL REFERENCES documents(doc_id) ON DELETE CASCADE,
    asset_id      TEXT    NOT NULL REFERENCES assets(asset_id) ON DELETE CASCADE,
    relation_type TEXT    NOT NULL DEFAULT 'source',
    note          TEXT,
    created_at_ms INTEGER NOT NULL,
    UNIQUE (doc_id, asset_id, relation_type)
);

CREATE INDEX idx_document_assets_doc
    ON document_assets(doc_id);

CREATE INDEX idx_document_assets_asset
    ON document_assets(asset_id);

CREATE TABLE memory_attachments (
    attachment_id   TEXT    PRIMARY KEY NOT NULL,
    memory_id       TEXT    NOT NULL REFERENCES episodes(memory_id) ON DELETE CASCADE,
    doc_id          TEXT    REFERENCES documents(doc_id) ON DELETE CASCADE,
    asset_id        TEXT    REFERENCES assets(asset_id) ON DELETE CASCADE,
    relation_type   TEXT    NOT NULL DEFAULT 'related',
    note            TEXT,
    provenance_json TEXT,
    created_at_ms   INTEGER NOT NULL,
    CHECK (
        (doc_id IS NOT NULL AND asset_id IS NULL)
        OR
        (doc_id IS NULL AND asset_id IS NOT NULL)
    )
);

CREATE INDEX idx_memory_attachments_memory
    ON memory_attachments(memory_id, created_at_ms DESC);

CREATE INDEX idx_memory_attachments_doc
    ON memory_attachments(doc_id);

CREATE INDEX idx_memory_attachments_asset
    ON memory_attachments(asset_id);

CREATE UNIQUE INDEX idx_memory_attachments_unique_doc_relation
    ON memory_attachments(memory_id, doc_id, relation_type)
    WHERE doc_id IS NOT NULL;

CREATE UNIQUE INDEX idx_memory_attachments_unique_asset_relation
    ON memory_attachments(memory_id, asset_id, relation_type)
    WHERE asset_id IS NOT NULL;
