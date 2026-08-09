# Derived Layer

The Steward writes three "memory ABOUT memory" tables on top
of your raw episodes:

  - **`semantic_abstractions`** — one cluster's distilled summary,
    LLM-generated.
  - **`triples`** — structured subject-predicate-object facts the
    Steward extracted from each abstraction.
  - **`contradictions`** — pairs of triples that disagree (rule
    filter + LLM judge).

Until Solo v0.4.0, those tables were write-only: the Steward
populated them on every consolidate cycle, but agents and HTTP
clients had no first-class way to query the data. The four MCP
tools (`memory.{remember,recall,inspect,forget}`) returned only raw
episodes; the four HTTP endpoints did the same.

**v0.4.0 added three new MCP tools and three new HTTP endpoints**
that surface the derived layer. Same data, four ways to read it
now: SQL (always was), MCP, HTTP, and `solo doctor --with-stats`
(counts only, not content).

## When to use the derived layer

Three concrete patterns:

  1. **"What has the user been thinking about lately?"** Before
     deciding what to recall, an agent can call `memory_themes`
     to see the cluster abstractions for the last week / month
     / etc. Better signal than recalling against an empty query.

  2. **"What do we know about Sam?"** Instead of recalling for
     "Sam" and getting N noisy episodes, query
     `memory_facts_about` with `subject=Sam` and get the
     deduplicated SPO list the Steward extracted.

  3. **"Are there any contradictions to resolve?"** An agent that
     wants to be honest can call `memory_contradictions` and
     surface them to the user rather than confidently asserting
     one side. Each result includes both sides' SPO via LEFT
     JOIN so the agent can render context.

You can use any combination of derived + raw — they're just
different views over the same underlying memory.

## Three new tools / endpoints

### `memory_themes` / `GET /memory/themes`

List recent cluster themes with their (optional) abstractions.

Args:

  - `window_days` (optional integer ≥ 1) — restrict to clusters
    created in the last N days. Omit for all-time.
  - `limit` (optional integer 1-100, default 5) — max results.

Returns an array of objects:

```json
[
  {
    "cluster_id": "01J...",
    "abstraction_id": "01J...",
    "abstraction_text": "Sam works as an SRE at Stripe in Berlin, ...",
    "episode_count": 4,
    "coherence": 0.87,
    "created_at_ms": 1715432100000
  }
]
```

`abstraction_id` and `abstraction_text` are nullable — a cluster
without an abstraction (Steward not yet wired, or LLM call failed
mid-consolidate) still appears in the list with the abstraction
fields set to `null`.

CLI smoke (HTTP):

```bash
curl -s http://localhost:17821/memory/themes?window_days=7 | jq
```

### `memory_facts_about` / `GET /memory/facts_about`

Query the Steward's structured-fact knowledge graph by subject +
optional predicate + optional time window.

Args:

  - `subject` (required string) — subject ID to query (e.g. `Sam`).
    Predicate-only scans are not supported (would scan the whole
    `triples` table); pass a non-empty subject.
  - `predicate` (optional string) — exact-match predicate filter
    (e.g. `works_at`, `lives_in`).
  - `since_ms` (optional integer) — `valid_from_ms >= since_ms`.
  - `until_ms` (optional integer) — `valid_to_ms IS NULL OR
    valid_to_ms <= until_ms`. Open-ended (still-valid) facts pass
    through any `until_ms` filter.
  - `limit` (optional integer 1-100, default 5).

Skips `status != 'active'` triples. Ordered by `valid_from_ms`
descending (newest fact first).

Returns an array of:

```json
[
  {
    "triple_id": "01J...",
    "subject_id": "Sam",
    "predicate": "works_at",
    "object_id": "Stripe",
    "object_kind": "literal",
    "valid_from_ms": 1715000000000,
    "valid_to_ms": null,
    "confidence": 0.92,
    "cluster_id": "01J..."
  }
]
```

CLI smoke (HTTP):

```bash
curl -s 'http://localhost:17821/memory/facts_about?subject=Sam&predicate=works_at' | jq
```

### `memory_contradictions` / `GET /memory/contradictions`

List Steward-flagged contradictions. Each result includes both
sides' triple summaries via LEFT JOIN so consumers can render
human-readable context without a follow-up call.

Args:

  - `limit` (optional integer 1-100, default 5).

Returns an array of:

```json
[
  {
    "a_id": "01J...",
    "b_id": "01J...",
    "kind": "overlapping_single_valued_predicate",
    "explanation": "Sam can't live in two places at the same time",
    "detected_at_ms": 1715432200000,
    "a_triple": {
      "triple_id": "01J...",
      "subject_id": "Sam",
      "predicate": "lives_in",
      "object_id": "Berlin",
      "object_kind": "literal",
      "valid_from_ms": 1715000000000,
      "valid_to_ms": null
    },
    "b_triple": {
      "triple_id": "01J...",
      "subject_id": "Sam",
      "predicate": "lives_in",
      "object_id": "Tokyo",
      "object_kind": "literal",
      "valid_from_ms": 1715200000000,
      "valid_to_ms": null
    }
  }
]
```

`a_triple` and `b_triple` are nullable — if the underlying triple
has been deleted or moved to `status='superseded'`, the JOIN misses
and the side is `null`.

The `kind` field is one of:
  - `overlapping_single_valued_predicate` (most common — same
    subject + predicate, overlapping validity windows, different
    objects)
  - `direct_negation` (one is the negation of the other)
  - `numeric_inconsistency` (numeric values differ enough to
    matter)
  - `other` (catch-all)

The schema doesn't track resolution state, so there's no
`unresolved_only` filter today — every flagged contradiction is
returned. A `resolved_at_ms` column + filter is a reasonable post-
v0.4.0 addition once consumer feedback shows it's needed.

CLI smoke (HTTP):

```bash
curl -s http://localhost:17821/memory/contradictions | jq
```

## How the data gets there

The derived layer is populated by consolidation plus the daemon
triples-batch path. Consolidation clusters related episodes; a
configured Steward LLM then turns clustered memories into
abstractions, triples, and contradictions. See
[Consolidation Cycle](./consolidation-cycle.md):

```text
remember -> embedding row + pending_index
        -> cluster (pure-deterministic)      -> INSERT clusters, cluster_episodes
        -> merge/absorb drift passes         -> fold drift, refresh centroids
        -> triples-batch timer (Steward LLM) -> INSERT semantic_abstractions, triples
        -> contradiction detection           -> INSERT contradictions
```

If memories have not been clustered, or no Steward LLM is
configured, the derived tables can stay empty and all three
tools / endpoints return empty arrays. They never error on
"no data" - empty `[]` is the success shape.

## Authentication + transport notes

The HTTP endpoints sit under the same bearer-auth gate as the
existing `/memory` endpoints — when `solo http-serve --bind <ip>
--bearer-token-file <path>` is configured, all three derived
endpoints require the bearer token. Loopback-only deployments
(default) leave them unauthenticated, same as the existing
endpoints.

The MCP tools have no transport-level auth; they're scoped to the
subprocess the user (or their AI assistant) spawned via
`solo mcp-stdio`.

## Performance

All three pipelines are single SQL queries against existing v0.3
tables (no new schema, no migration in v0.4.0). Indexes:

  - `themes` — uses `clusters` PK ordering for `ORDER BY
    created_at_ms`, plus `idx_cluster_episodes_memory` for the
    episode-count subquery per cluster.
  - `facts_about` — uses `idx_triples_subject_predicate` for
    subject + optional predicate filter, `idx_triples_valid_window`
    for time-window filter and ORDER BY.
  - `contradictions` — no `idx_contradictions_detected_at` today;
    `ORDER BY detected_at_ms DESC` is fine for typical N (single-
    digit thousands of flagged contradictions). Add the index if
    you accumulate millions.

The LEFT JOIN in `contradictions` runs against `triples.triple_id`
(unique-indexed via the table PK), so each contradiction-row is at
most two index lookups.
