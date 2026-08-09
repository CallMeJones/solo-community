# ADR-0007: Memory data model — episodes, triples, clusters, documents

**Status:** Accepted
**Date:** 2026-05-20 (retroactive — records a model shipped incrementally from v0.1.0 through v0.7.0)
**Deciders:** Solo project
**Depends on:** ADR-0001, ADR-0002, ADR-0003

## Status note (retroactive)

This ADR records a decision that was made and shipped without a
dedicated ADR. The data model has been load-bearing since `ec482b4`
("Commit 1.1"), extended in v0.5.0 (clusters), v0.6.x (facts/themes
derived views), and v0.7.0 (documents/chunks). It is referenced
implicitly by every other ADR but never had its own decision record;
writing one now so future contributors don't have to reverse-engineer
the schema rationale from `solo-v0-architecture.md` + migrations.

The architecture doc (`solo-v0-architecture.md`) was the design source;
this ADR locks the concrete entity set and their invariants as
implemented.

## TL;DR

| Concern | Decision |
|---|---|
| Primary entities | `Episode` (events) · `Triple` (semantic facts, bi-temporal) · `Cluster` + `SemanticAbstraction` (consolidation outputs) · `Contradiction` (detected conflicts) · `Document` + `DocumentChunk` (RAG memory) |
| ID scheme | UUID v7 (time-ordered) — `MemoryId`, `DocumentId`, `ChunkId` |
| Bi-temporality | Triples carry `valid_from_ms` + `valid_to_ms` (Option) — overlapping windows on single-valued predicates are the contradiction signal |
| Provenance | Every derived memory carries `Provenance { derived_from, derivation, by, at_ms }` — non-optional on `SemanticAbstraction`, optional on raw `Episode` |
| Embeddings | `Embedding { dtype, dim, raw bytes }` — dtype-aware to support tiering (F32 hot / F16 / I8 warm / Binary cold) without per-call casts |
| Timestamps | INTEGER ms-since-epoch everywhere (no mixed `TIMESTAMPTZ`/`TEXT`/`INTEGER` — lesson #1) |
| Confidence | `Confidence(f32)` newtype, validated to `[0.0, 1.0]` |
| Tier | enum `{ Hot, Warm, Cold }` — drives storage location + recall ordering |
| Soft delete | `status` column on episodes + documents; rows preserved (the "silent trace") and excluded from recall by SQL filter |

## Context

Solo is a memory daemon. Its core IP is the *shape* of memory it
stores — not the storage engine, not the vector library, not the LLM
backend. The architecture doc (`solo-v0-architecture.md §3.1`) sets
out four claims that every entity in the schema must honor:

1. **Episodes are the source-of-truth.** Everything else is derived.
   No information lives only in a cluster or abstraction.
2. **Triples are bi-temporal.** "X works at Acme" is true *over a
   window*, not eternally. Contradiction detection depends on this.
3. **Provenance is non-optional for derived memory.** Without
   `derived_from`, reconstructive retrieval confabulates confidently
   (`human-brain-memory.md §6.13`).
4. **Documents are first-class, not flattened to episodes.** A 50-page
   PDF is an ingestion artifact with its own lifecycle; chunks are
   the embedding unit, but the document is the user-visible delete
   unit.

The forcing function for writing this down was that ADRs 0003–0006
all reference these entities ("episodes," "triples," "clusters") as
if they were defined elsewhere — they weren't, except by reading
[crates/solo-core/src/types.rs](../../crates/solo-core/src/types.rs)
and the migration chain.

## Decision

### Identifier types

All entity IDs are **UUID v7** (time-ordered) via `uuid::Uuid::now_v7`.
Three newtypes wrap them so the type system catches cross-entity
misuse:

| Type | Wraps | Used by |
|---|---|---|
| `MemoryId` | `Uuid` | episodes, triples, clusters, abstractions |
| `DocumentId` | `Uuid` | documents |
| `ChunkId` | `Uuid` | document chunks |

UUID v7 was chosen over UUID v4 + a separate `created_at_ms` column
because lexicographic ordering matches chronological ordering — this
makes keyset pagination cheap and gives the FTS5 / BM25 path
sequential locality without a secondary index.

### Episode (source of truth)

```rust
pub struct Episode {
    pub memory_id: MemoryId,
    pub ts_ms: i64,
    pub source_type: String,        // user_message | tool_output | observation | ...
    pub source_id: Option<String>,
    pub content: String,
    pub encoding_context: EncodingContext,
    pub provenance: Option<Provenance>, // None for raw episodes; Some for derived
    pub confidence: Confidence,
    pub strength: f32,
    pub salience: f32,
    pub tier: Tier,
}
```

[types.rs:297](../../crates/solo-core/src/types.rs#L297). Backed by
the `episodes` table in
[0001_initial.sql](../../crates/solo-storage/src/migrations/0001_initial.sql).
`provenance` is `Option<...>` because user-authored episodes have no
upstream sources; everything the steward emits sets it.

### Triple (bi-temporal semantic facts)

```rust
pub struct Triple {
    pub triple_id: MemoryId,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: String,
    pub object_kind: TripleObjectKind,  // Entity | Literal
    pub valid_from_ms: i64,
    pub valid_to_ms: Option<i64>,        // None = open-ended
    pub confidence: Confidence,
    pub provenance: Provenance,          // non-optional
}
```

[types.rs:314](../../crates/solo-core/src/types.rs#L314). Backed by
the `triples` table in 0001 (cluster_id added in
[0002_triples_cluster_id.sql](../../crates/solo-storage/src/migrations/0002_triples_cluster_id.sql),
source provenance pointer added in
[0007_triples_source.sql](../../crates/solo-storage/src/migrations/0007_triples_source.sql)).

Single-valued predicates (`works_at`, `born_in`) are flagged in code;
two active triples on the same (subject, predicate) with overlapping
windows is the contradiction signal — see `Contradiction` below.

### Cluster + SemanticAbstraction (consolidation outputs)

```rust
pub struct Cluster {
    pub cluster_id: MemoryId,
    pub episode_ids: Vec<MemoryId>,
    pub centroid: Option<Embedding>,
    pub coherence: f32,
}

pub struct SemanticAbstraction {
    pub abstraction_id: MemoryId,
    pub cluster_id: MemoryId,
    pub content: String,
    pub triples: Vec<Triple>,
    pub provenance: Provenance,
    pub confidence: Confidence,
}
```

[types.rs:458,469](../../crates/solo-core/src/types.rs#L458). A
cluster is the SWS-equivalent dedup output (pure deterministic,
no LLM). An abstraction is the REM-equivalent integration output
(steward LLM call). The split lets us regression-test clustering
without LLM calls.

### Contradiction (detected conflicts)

```rust
pub struct Contradiction {
    pub a: MemoryId,
    pub b: MemoryId,
    pub kind: ContradictionKind,  // OverlappingSingleValuedPredicate | DirectNegation | NumericInconsistency | Other
    pub explanation: String,
}
```

[types.rs:481](../../crates/solo-core/src/types.rs#L481). Two-stage
detection per ADR-0002: cheap rule-based filter (SQL bi-temporal
check) + LLM judge for ambiguous cases.

### Document + DocumentChunk (v0.7.0 RAG memory)

```rust
pub struct Document {
    pub doc_id: DocumentId,
    pub source: Option<String>,
    pub title: Option<String>,
    pub mime_type: Option<String>,
    pub ingested_at_ms: i64,
    pub modified_at_ms: Option<i64>,
    pub status: DocumentStatus,    // Active | Forgotten
    pub chunk_count: u32,
    pub content_hash: Option<String>,
    pub byte_size: Option<u64>,
}

pub struct DocumentChunk {
    pub chunk_id: ChunkId,
    pub doc_id: DocumentId,
    pub chunk_index: u32,
    pub content: String,
    pub token_count: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub created_at_ms: i64,
}
```

[types.rs:421,440](../../crates/solo-core/src/types.rs#L421). Backed
by `documents` + `document_chunks` tables in
[0003_documents.sql](../../crates/solo-storage/src/migrations/0003_documents.sql).
Chunks are the embedding unit (one HNSW vector per chunk); the
document is the user-visible delete unit. See ADR-0008 for the chunk
strategy.

### Supporting types

- **`EncodingContext`** ([types.rs:280](../../crates/solo-core/src/types.rs#L280)) — Tulving's encoding specificity: `session_id`, `task`, `recent_summary`, `affect`, `extra`. Stored verbatim alongside each episode for encoding-context re-ranking at recall.
- **`Provenance`** ([types.rs:264](../../crates/solo-core/src/types.rs#L264)) — `derived_from: Vec<MemoryId>` + `derivation` (`"summary"|"inference"|"extraction"|"consolidation"|"user_edit"`) + `by` (LLM name / tool / `"user"`) + `at_ms`.
- **`Confidence(f32)`** ([types.rs:237](../../crates/solo-core/src/types.rs#L237)) — newtype, validated `[0.0, 1.0]`.
- **`Tier`** ([types.rs:252](../../crates/solo-core/src/types.rs#L252)) — enum `Hot|Warm|Cold`; drives storage location + recall ordering.
- **`Embedding`** ([types.rs:202](../../crates/solo-core/src/types.rs#L202)) — `(dtype, dim, raw bytes)`. Dtype-aware so the storage layer doesn't double-cast.

## Alternatives considered

### Option A — Flat "all memories are episodes" (rejected)

Encode triples, abstractions, and documents as `Episode` rows with a
discriminator column. Single-table design.

**Pros:** Fewer tables. Uniform recall path.
**Cons:** Loses the bi-temporal `valid_from/valid_to` window on
triples (the contradiction signal). Forces every consumer to handle
discriminator switching. Documents wouldn't fit cleanly — chunk-
level vectors but document-level lifecycle. Rejected.

### Option B — Schema-less JSON blobs

Store memories as JSON in a single `memories` table with a `kind`
column.

**Pros:** Maximum schema flexibility.
**Cons:** Loses SQL-level enforcement of provenance, validity windows,
and references. FTS5 over JSON blobs is awkward. Rejected.

### Option C — Separate tables per entity (chosen)

What's described above. Each entity gets its own table with foreign
keys; FTS5 + HNSW reference rows by SQLite `rowid`.

**Pros:** Schema enforces invariants. FTS5 + HNSW work naturally.
Migrations are explicit. Each entity has a clear lifecycle.
**Cons:** Recall path queries N tables. Acceptable — recall is
already cross-source by design (BM25 + HNSW + facts + abstractions).

## Consequences

**What this locks in:**

- Adding a new memory *kind* (e.g., "skill," "habit") requires a new
  table + entity type, not a new discriminator value. This is a
  feature: every kind earns its own invariants and lifecycle.
- HNSW row-id encoding must namespace episode rowids and chunk rowids
  (different tables, both indexed). See [hnsw_id.rs](../../crates/solo-storage/src/hnsw_id.rs).
- Migrations are additive — no destructive column drops without an
  explicit downgrade path (see [0004_tenants_downgrade.sql](../../crates/solo-storage/src/migrations/0004_tenants_downgrade.sql)).

**What this defers:**

- Multi-modal memory (image + text). The `Embedding { dtype, dim,
  data }` shape is general enough, but no entity carries an image
  reference yet. Would add an `Asset` entity.
- Cross-tenant references. Multi-tenancy (ADR-0004) hard-isolates per
  DB file; there is no cross-tenant link primitive and adding one
  would need its own ADR.

## Implementation map

| Concept | Code |
|---|---|
| All entity types | [crates/solo-core/src/types.rs](../../crates/solo-core/src/types.rs) |
| Episodes + triples tables | [migrations/0001_initial.sql](../../crates/solo-storage/src/migrations/0001_initial.sql) |
| Cluster_id on triples | [migrations/0002_triples_cluster_id.sql](../../crates/solo-storage/src/migrations/0002_triples_cluster_id.sql) |
| Documents + chunks | [migrations/0003_documents.sql](../../crates/solo-storage/src/migrations/0003_documents.sql) |
| Triple source provenance | [migrations/0007_triples_source.sql](../../crates/solo-storage/src/migrations/0007_triples_source.sql) |
| HNSW rowid namespacing | [crates/solo-storage/src/hnsw_id.rs](../../crates/solo-storage/src/hnsw_id.rs) |
