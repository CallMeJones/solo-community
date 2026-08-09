-- SPDX-License-Identifier: Apache-2.0

-- Extraction attempts for retained original-file assets.
--
-- Phase 3 separates "Solo retained the original file" from "Solo extracted
-- searchable text from that file". Unsupported formats and failed extractors
-- are recorded here without deleting the asset row or blob.

CREATE TABLE asset_extractions (
    extraction_id      TEXT    PRIMARY KEY NOT NULL,
    asset_id           TEXT    NOT NULL REFERENCES assets(asset_id) ON DELETE CASCADE,
    extractor_name     TEXT    NOT NULL,
    extractor_version  TEXT    NOT NULL,
    status             TEXT    NOT NULL
                             CHECK (status IN ('extracted','stored_unparsed','failed')),
    text_chars         INTEGER NOT NULL DEFAULT 0 CHECK (text_chars >= 0),
    error              TEXT,
    created_at_ms      INTEGER NOT NULL,
    UNIQUE (asset_id, extractor_name, extractor_version)
);

CREATE INDEX idx_asset_extractions_asset
    ON asset_extractions(asset_id, created_at_ms DESC);

CREATE INDEX idx_asset_extractions_status
    ON asset_extractions(status, created_at_ms DESC);
