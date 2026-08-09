-- SPDX-License-Identifier: Apache-2.0

-- Metadata for encrypted retained asset blobs.
--
-- `sha256` and `size_bytes` remain the plaintext identity and public API
-- contract. `encrypted_size_bytes` validates the sidecar file size when
-- `encryption_alg` is not `none`. Existing plaintext blobs are left as
-- legacy-readable rows with `encryption_alg='none'`.

ALTER TABLE assets
    ADD COLUMN encryption_alg TEXT NOT NULL DEFAULT 'none'
    CHECK (encryption_alg IN ('none','xchacha20poly1305-blake3-v1'));

ALTER TABLE assets
    ADD COLUMN encryption_nonce BLOB;

ALTER TABLE assets
    ADD COLUMN encrypted_size_bytes INTEGER CHECK (
        encrypted_size_bytes IS NULL OR encrypted_size_bytes >= 0
    );

CREATE INDEX idx_assets_encryption_alg
    ON assets(encryption_alg);
