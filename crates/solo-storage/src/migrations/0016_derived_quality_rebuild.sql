-- 0016 - one-time derived graph invalidation after quality gate rollout
--
-- Migration 0015 introduced the triple quality gate, alias canonicalization,
-- and review quarantine tables. Existing tenants may already have weak
-- pre-gate derived graph state: machine identifiers promoted as entities,
-- literal values rendered as relationship nodes, low-confidence facts, and
-- stale aliases.
--
-- Raw memories remain canonical and are intentionally untouched here. This
-- migration clears only the derived layer so the normal steward pipeline can
-- rebuild clusters, abstractions, triples, aliases, and review items under the
-- new quality rules. The daemon's startup catch-up path detects the empty
-- graph and runs the first rebuild immediately for non-trivial corpora.

DELETE FROM contradictions;
DELETE FROM triple_reviews;
DELETE FROM entity_aliases;
DELETE FROM triples;
DELETE FROM semantic_abstractions;
DELETE FROM cluster_episodes;
DELETE FROM clusters;
