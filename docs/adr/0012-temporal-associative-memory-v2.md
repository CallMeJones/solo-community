# ADR-0012: Temporal Associative Memory Graph v2

**Status:** Accepted for roadmap design
**Date:** 2026-06-28
**Deciders:** Solo project
**Depends on:** ADR-0007

## Status note

This ADR records the target architecture for the next graph generation.
It does not claim the schema is implemented today. Current Solo memory
has episodes, triples, clusters, abstractions, contradictions, document
chunks, assets, graph APIs, and `memory_context`. The current graph is
useful, but entities are mostly synthetic from triples and relationships
are mostly triple rows.

## Decision

Solo will evolve triples into a first-class temporal associative memory
graph while preserving the existing triples API.

The graph v2 model has these first-class records:

| Record | Purpose |
|---|---|
| `entities` | Canonical people, projects, tools, places, files, organizations, and concepts. |
| `entity_aliases` | Names and spelling variants mapped to canonical entities with confidence and source. |
| `relationship_edges` | Subject-predicate-object relationships with validity windows, confidence, strength, evidence counts, and status. |
| `relationship_evidence` | Links from each relationship edge to episodes, documents, chunks, or other source records. |
| `memory_claims` | Candidate facts/edges produced by extraction before activation. |
| `memory_retrieval_log` | Recall traces: query, recalled ids, reason codes, and graph expansion decisions. |
| `memory_revisions` | Supersession, reconsolidation, merge, split, and rejection history. |

Relationship statuses are:

| Status | Meaning |
|---|---|
| `candidate` | Proposed by extraction or import, not active yet. |
| `active` | Eligible for recall and graph answers. |
| `superseded` | Replaced by a newer or more precise relationship. |
| `contradicted` | Conflicts with another active or candidate relationship. |
| `rejected` | Reviewed or scored as noise or not useful. |

Every active relationship must be explainable by evidence. A graph edge
without evidence is not an active memory relationship.

## Compatibility rules

Existing triples remain compatible:

1. Backfill `relationship_edges` from active triples.
2. Dual-write new triples into `relationship_edges`.
3. Keep triple read/write tools working while graph v2 APIs are added.
4. Do not require existing clients to understand entities or edges before
   they can continue using current memory tools.

Entity canonicalization is additive. Alias merging and entity splitting
must be reversible through review operations and recorded in
`memory_revisions`.

## Temporal semantics

Graph v2 must answer "what was true when?" The validity model is:

- `valid_from` and `valid_to` describe the real-world assertion window.
- `created_at` describes when Solo learned or derived the relationship.
- `first_seen` and `last_seen` on entities describe observation windows,
  not necessarily real-world validity.
- Supersession does not delete older edges; it changes active status and
  records revision history.

When a predicate is effectively single-valued for a subject over a time
window, overlapping active edges are contradiction candidates.

## Recall and explainability

`memory_context` should become graph-aware:

1. Detect query entities from lexical and semantic matches.
2. Expand one or two hops through `relationship_edges`.
3. Rank by confidence, strength, recency, evidence count, source type,
   salience, and contradiction risk.
4. Return relationship paths when useful, for example:
   `User -> works_on -> Project -> uses -> Component`.
5. Include reason codes such as semantic match, lexical match, graph
   neighbor, recent confirmation, user-approved fact, and contradiction
   warning.

New explainability tools should include:

- `memory_explain_recall`
- `memory_explain_provenance`
- `memory_graph_paths`

## Quality gate

The steward should emit `memory_claims` before activating new graph
relationships. Claims are scored by:

- durable subject
- useful predicate
- source type
- recurrence
- cluster coherence
- model confidence
- evidence count
- user approval
- contradiction risk

High scoring claims may activate automatically. Medium and low scoring
claims go to review. Obvious transcript noise is rejected or
quarantined.

## Consequences

This preserves Solo's local-first memory identity while moving beyond a
simple fact table. The graph becomes a temporal, evidence-backed recall
surface, not a clone of Mem0, Zep, Graphiti, Letta, or LangMem.

The migration must be incremental because triples, contradictions, graph
APIs, and existing MCP tools are already public contracts.

## Implementation map

| Area | Existing anchor |
|---|---|
| Memory model | `crates/solo-core/src/types.rs` |
| Triples storage | `crates/solo-storage/src/migrations/0001_initial.sql` and later triple migrations |
| Graph read APIs | `crates/solo-api/src/http.rs` graph endpoints |
| Recall context | `crates/solo-query/src/context.rs` |
| Contradictions | `crates/solo-steward/src/contradiction.rs` |

