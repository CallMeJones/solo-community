# ADR-0008: Document chunking and ingestion

**Status:** Accepted
**Date:** 2026-05-20 (retroactive — records v0.7.0 chunking strategy)
**Deciders:** Solo project
**Depends on:** ADR-0007

## Status note (retroactive)

The chunking strategy and the HNSW row-id namespacing that lets chunks and
episodes share one index shipped together. This ADR records both as one
decision so future contributors see
"how do documents become searchable" in a single place.

## TL;DR

| Concern | Decision |
|---|---|
| Chunker contract | Pure `fn chunk_text(text, ChunkConfig) -> Vec<ChunkSpec>` — no I/O, no allocation of IDs |
| Default chunk size | `target_tokens = 500`, `overlap_tokens = 50` |
| Token approximation | `chars / 4` (English heuristic; intentionally not pluggable) |
| Splitting strategy | Paragraph-aware → accumulate to target → emit with overlap → slide-window over oversized paragraphs |
| Offsets | UTF-8-safe byte offsets into the original text, preserved on `ChunkSpec` |
| Chunk identity | UUID v7 `ChunkId`, allocated by the writer-actor (not the chunker) |
| Embedding unit | One vector per chunk in HNSW (not per document) |
| HNSW rowid | Namespaced via [`hnsw_id`](../../crates/solo-storage/src/hnsw_id.rs) so episode rowids and chunk rowids never collide |
| Document lifecycle | `documents.status = 'active'|'forgotten'` is the user-visible delete unit; chunks cascade |

## Context

ADR-0007 establishes `Document` + `DocumentChunk` as first-class
entities. v0.7.0 shipped the actual ingestion path. Three properties
the chunker had to satisfy:

1. **Reproducibility.** Re-ingesting the same text must produce the
   same chunk boundaries — otherwise `solo reembed` would re-shuffle
   chunk identities and break references.
2. **Preserved provenance.** Each chunk must point back at its
   document and carry exact byte offsets, so recall can stitch
   surrounding context without re-parsing the source.
3. **No drift between chunker and stored metadata.** The
   `token_count` on `DocumentChunk` rows is re-derived by the writer
   from the same `approx_token_count` function the chunker uses —
   one source of truth per concept.

The architectural pressure that ruled out alternatives: chunks share
the HNSW index with episodes. Without rowid namespacing, the two
streams of inserts would collide.

## Decision

### Chunker is a pure function

[chunk.rs:91](../../crates/solo-storage/src/document/chunk.rs#L91)
implements:

```rust
pub fn chunk_text(text: &str, config: &ChunkConfig) -> Vec<ChunkSpec>;
```

`ChunkSpec` is `(content, start_offset, end_offset, token_count)` —
no ID, no document reference, no timestamp. The writer-actor
materializes `ChunkSpec` → `DocumentChunk` by allocating a fresh
`ChunkId`, setting `doc_id`, assigning `chunk_index`, and stamping
`created_at_ms` ([chunk.rs:53-69](../../crates/solo-storage/src/document/chunk.rs#L53)).

This separation keeps the chunker testable without database fixtures
and makes the chunking algorithm replaceable without touching ID
allocation.

### Splitting strategy

1. **Single-chunk fast path:** if `total_tokens ≤ target * 1.5`,
   emit the whole text as one chunk.
2. **Paragraph-aware accumulation:** split on `\n\n` or
   Markdown-style heading lines. Accumulate paragraphs into a chunk
   until `running_tokens >= target`; emit, then start the next
   chunk with the last ~`overlap_tokens` worth of characters.
3. **Oversized-paragraph window:** if a single paragraph exceeds
   `target * 1.5`, slide a window across it, preferring sentence-
   ending punctuation (`.!?` or newline) within the last ~10% of
   the window.

### Token approximation: `chars / 4`

[approx_token_count](../../crates/solo-storage/src/document/chunk.rs#L72)
returns `chars.count() / 4`. This is an English heuristic; non-Latin
scripts under-estimate by ~2x, so chunks may come out larger than
configured. **Intentionally not pluggable** — using a real tokenizer
(tiktoken, transformers) would create drift between the chunker and
the writer-actor's `token_count` field, and the cost of correctness
isn't worth the added dependency for v0.7.0.

### Default `ChunkConfig`: 500 / 50

```rust
pub struct ChunkConfig {
    pub target_tokens: u32,      // 500
    pub overlap_tokens: u32,     // 50
}
```

500 keeps chunks well under the 8K-token context of typical embedder
models (all-MiniLM-L6-v2 caps at 512). 50 (10% overlap) preserves
context across boundaries without inflating storage materially.

### UTF-8-safe offsets

Offsets are byte offsets into the source text. The chunker walks
`text.char_indices()` so every offset lands on a UTF-8 boundary —
slicing `text[start..end]` is always valid. Tests enforce this on
non-ASCII corpora.

### HNSW rowid namespacing

Episodes and chunks both live in HNSW. To prevent collision,
[hnsw_id.rs](../../crates/solo-storage/src/hnsw_id.rs) encodes the
SQLite rowid plus a *kind tag* into the i64 HNSW key. Recovery and
recall both decode the tag before resolving rows.

## Alternatives considered

### Option A — Fixed-size character windows (rejected)

Take every 2,000 characters with 200 overlap. Simpler, faster.

**Pros:** Pure-arithmetic, no parsing.
**Cons:** Breaks mid-sentence and mid-word, hurts retrieval
quality. Rejected for v0.7.0; would be revisited if perf forced it.

### Option B — Tree-sitter / proper sentence tokenization (rejected for v0.7.0)

Parse the document, chunk on sentence/paragraph trees from a real
parser.

**Pros:** Highest retrieval quality on prose.
**Cons:** Adds tree-sitter (multi-MB) per language to the binary,
slow on cold parse, fails open on unknown formats. Defer until
recall-quality metrics show paragraph-aware chunking is the
bottleneck.

### Option C — Paragraph-aware with sliding-window fallback (chosen)

What's described above. Cheap, deterministic, UTF-8-safe, no extra
deps.

## Consequences

**What this locks in:**

- Re-chunking on `solo reembed` is content-stable as long as
  `ChunkConfig` is unchanged. Bumping the config invalidates all
  prior chunk identities — surface that in the migration tool.
- Recall snippet expansion can rely on `start_offset`/`end_offset` to
  fetch ~N surrounding chars from the source — no re-parsing.
- One vector per chunk → memory ingestion is O(chunks) HNSW
  inserts. A 50-page PDF (~25K tokens at 4 chars/token = ~100KB
  text) chunks to ~50 vectors at 500 tokens each. Manageable.

**What this defers:**

- Real tokenizer-based counts (would close the non-Latin
  under-estimate gap).
- Format-aware chunking (Markdown headings, code blocks, tables).
  Tree-sitter is the obvious upgrade path.
- Embedding-aware chunking (re-chunk on cosine-distance drop). High
  retrieval-quality ceiling, high complexity floor; revisit if
  recall metrics demand it.

## Implementation map

| Concept | Code |
|---|---|
| `chunk_text` + `ChunkConfig` + `ChunkSpec` | [crates/solo-storage/src/document/chunk.rs](../../crates/solo-storage/src/document/chunk.rs) |
| `Document` + `DocumentChunk` materialization | [crates/solo-storage/src/document/](../../crates/solo-storage/src/document/) |
| Text parsing (Markdown / plain / etc.) | [crates/solo-storage/src/document/parse.rs](../../crates/solo-storage/src/document/parse.rs) |
| HNSW rowid namespacing | [crates/solo-storage/src/hnsw_id.rs](../../crates/solo-storage/src/hnsw_id.rs) |
| Documents schema | [migrations/0003_documents.sql](../../crates/solo-storage/src/migrations/0003_documents.sql) |
| Design rationale | [dev-log/0083-v0.7.0-implementation-plan.md](../dev-log/0083-v0.7.0-implementation-plan.md), [dev-log/0084-v0.7.0-hnsw-rowid-encoding.md](../dev-log/0084-v0.7.0-hnsw-rowid-encoding.md) |
