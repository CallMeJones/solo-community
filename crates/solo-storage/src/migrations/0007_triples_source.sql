-- SPDX-License-Identifier: Apache-2.0
--
-- 0007 — per-tenant `triples.source_episode_id` FK + index for GDPR
-- cascade. v0.8.1 P1.
--
-- See `docs/dev-log/0095-v0.8.1-implementation.md` (this release's dev
-- log) and the v0.8.0 release notes Known Issues §1 — `solo gdpr forget
-- --subject X` correctly hard-deletes episodes + chunks but the v0.8.0
-- schema lacked a per-episode FK on triples, so derived triples
-- referencing the deleted episodes' ids were orphaned. The audit row
-- honestly reported `triples_deleted = 0`.
--
-- v0.8.1 adds the FK + a best-effort backfill from existing
-- `provenance_json` (the only place v0.8.0 records the source memory id
-- per triple). The cascade in `gdpr.rs::forget_principal` then picks up
-- the column and reports the real `triples_deleted` count.
--
-- IMPORTANT — this migration applies to per-tenant SQLCipher DBs at
-- `<data_dir>/tenants/<id>.db` (the `MIGRATIONS` chain in `migration.rs`).
-- The tenants_index.db chain is independent and unaffected.
--
-- ## Backfill discipline (lesson #28 — partial-completion safe)
--
-- The backfill UPDATE is keyed `WHERE source_episode_id IS NULL` so re-
-- running on an already-backfilled DB is a no-op. Triples whose
-- `provenance_json.derived_from[0]` doesn't resolve to a live episode
-- (deleted, or never-resolved memory_id) keep `source_episode_id IS
-- NULL` — those are orphans-by-design (documented), and the GDPR
-- cascade reports the orphan count for operator visibility.
--
-- ## Why nullable, not NOT NULL
--
-- Pre-v0.8.1 triples may carry a `derived_from` that no longer resolves
-- (the source episode was forgotten before v0.8.1 shipped). Forcing
-- NOT NULL would require a destructive backfill choice (delete?
-- placeholder?); nullable defers that to operator workflow. The GDPR
-- cascade's `WHERE source_episode_id IN (...)` skips NULL rows, which
-- is the right shape for GDPR — a triple with no resolvable source
-- isn't attributable to the forgotten principal.

-- --------------------------------------------------------------------------
-- triples — add nullable source_episode_id FK + index
-- --------------------------------------------------------------------------
--
-- SQLite ALTER TABLE ADD COLUMN with a REFERENCES clause is supported in
-- SQLite 3.6+ (Solo bundles 3.45+). The FK is enforced only when
-- `PRAGMA foreign_keys = ON` is set; the storage layer's init path sets
-- this. ON DELETE SET NULL means a manual DELETE FROM episodes WHERE
-- rowid = X leaves orphan triples with NULL source — same shape as
-- pre-v0.8.1, not a regression.

ALTER TABLE triples ADD COLUMN source_episode_id INTEGER
    REFERENCES episodes(rowid) ON DELETE SET NULL;

CREATE INDEX idx_triples_source_episode ON triples(source_episode_id);

-- --------------------------------------------------------------------------
-- Best-effort backfill from `provenance_json`
-- --------------------------------------------------------------------------
--
-- The Provenance struct (solo_core::types::Provenance) serializes its
-- `derived_from: Vec<MemoryId>` as a JSON array of UUID strings under
-- the `derived_from` key. We extract the first entry and JOIN to
-- `episodes.memory_id` to resolve its rowid.
--
-- json_extract is SQLite's built-in JSON1 function (always available in
-- the SQLCipher build Solo ships). `json_extract(provenance_json,
-- '$.derived_from[0]')` returns NULL if the field is absent / the
-- column is non-JSON / the array is empty — the LEFT JOIN then leaves
-- `source_episode_id` NULL, which is the documented orphan-by-design
-- state.
--
-- Idempotency: keyed `WHERE source_episode_id IS NULL` — re-runs are
-- no-ops for already-populated rows.

UPDATE triples
   SET source_episode_id = (
        SELECT e.rowid
          FROM episodes e
         WHERE e.memory_id = json_extract(triples.provenance_json,
                                          '$.derived_from[0]')
        )
 WHERE source_episode_id IS NULL
   AND provenance_json IS NOT NULL;
