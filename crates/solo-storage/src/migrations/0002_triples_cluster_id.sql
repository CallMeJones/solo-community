-- 0002 — link triples to their originating cluster.
--
-- Adds a NULLABLE `cluster_id` column to `triples` so the consolidate
-- pass can identify which triples belong to which cluster, and CASCADE-
-- delete them when an absorb invalidates the existing abstraction.
--
-- Pre-0002 rows get NULL — they predate the wiring and don't participate
-- in the cluster-cascade-delete on absorb. Once an old cluster's
-- abstraction is regenerated (via absorb), the new triples land with
-- cluster_id populated and the NULL rows naturally rotate out via
-- existing dedup paths in the contradiction sweep.
--
-- SQLite's ALTER TABLE ADD COLUMN supports adding a column with a
-- foreign-key reference, but only if the referenced table already
-- exists (it does — clusters is in 0001) AND the FK is enforced via
-- `PRAGMA foreign_keys = ON`, which the writer connection already sets
-- (open_sqlcipher applies it post-migration along with WAL/busy_timeout).
-- The ON DELETE CASCADE means deleting a cluster row drops its
-- triples — useful for the absorb→regen flow, where the regen pass
-- DELETEs both the abstraction and triples for the modified existing
-- cluster_id.

ALTER TABLE triples
    ADD COLUMN cluster_id TEXT
        REFERENCES clusters(cluster_id) ON DELETE CASCADE;

CREATE INDEX idx_triples_cluster ON triples(cluster_id);
