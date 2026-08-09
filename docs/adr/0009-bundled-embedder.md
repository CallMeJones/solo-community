# ADR-0009: Bundled embedder and zero-config install

**Status:** Accepted
**Date:** 2026-05-20 (retroactive — records v0.9.0 P3 decision)
**Deciders:** Solo project
**Depends on:** ADR-0002

## Status note (retroactive)

The bundled embedder is what made "zero credentials, zero dependencies" the
headline install story. This ADR records the embedder side of that decision.

## TL;DR

| Concern | Decision |
|---|---|
| Default embedder | `all-MiniLM-L6-v2` (384-dim, ~22 MB quantized ONNX, Apache-2.0) |
| Runtime | [fastembed-rs](https://github.com/Anush008/fastembed-rs) + [pykeio/ort](https://github.com/pykeio/ort) on CPU |
| Cargo feature | `bundled-embedder` — **default-on** in [solo-storage/Cargo.toml](../../crates/solo-storage/Cargo.toml) |
| Asset distribution | hf-hub cache (downloaded on first use to `~/.fastembed_cache/`) — NOT `include_bytes!` |
| ONNX runtime distribution | `ort` `download-binaries` feature pulls a prebuilt `libonnxruntime` from pyke's CDN during `cargo build` |
| Binary growth | ~22 MB on supported targets (x86_64/aarch64 glibc Linux, macOS, Windows MSVC) |
| Sibling backends | `OllamaEmbedder` (opt-in), `StubEmbedder` (tests only) |
| First-use cost | ~22 MB HuggingFace download, one-time, cached on disk and in-process via `tokio::sync::OnceCell` |

## Context

ADR-0005 picked `LlmSettings` as the LLM-backend pluggability surface
with `none|anthropic|openai|ollama|mcp_sampling` modes. That handled
*completions*. Embeddings were still gated on either an API key
(Voyage / hosted) or a running Ollama daemon — neither of which
matches the v0.9.0 goal: `solo install && solo init` should yield a
working memory daemon with **no credentials and no external
services**.

The forcing function: every recall path needs a vector. Without a
local embedder, the daemon can't answer `memory.recall` on a fresh
install. ADR-0005's `LlmClient::None` variant disables consolidation,
but recall must keep working.

## Decision

### Pick `all-MiniLM-L6-v2`

Six candidates were considered (notes from the v0.9.0 plan):

| Model | Dim | Size | License | Quality vs MiniLM | Pick? |
|---|---|---|---|---|---|
| `all-MiniLM-L6-v2` | 384 | ~22 MB | Apache-2.0 | baseline | ✅ |
| `all-MiniLM-L12-v2` | 384 | ~33 MB | Apache-2.0 | +5% MTEB | reject — size for marginal gain |
| `bge-small-en-v1.5` | 384 | ~33 MB | MIT | +8% MTEB | reject — see notes |
| `bge-m3` (architecture's original pick) | 1024 | ~600 MB | Apache-2.0 | +20% MTEB | reject — too large for bundled |
| `gte-small` | 384 | ~33 MB | MIT | +6% MTEB | reject |
| `nomic-embed-text-v1.5` | 768 | ~280 MB | Apache-2.0 | +15% MTEB | reject — too large |

MiniLM wins on binary size and license clarity. Quality is the
floor, not the ceiling — operators with stricter quality needs can
swap to `OllamaEmbedder` with a larger model.

### Use `fastembed` + `ort`, not raw `candle`

The original architecture (`solo-v0-architecture.md §3.2`) called for
`candle` (Hugging Face's Rust ML framework) running BGE-M3. Two
problems surfaced in v0.9.0 prep:

1. `candle` ships its own ONNX-and-safetensors stack and was ~60-80
   MB heavier than fastembed + ort for the bundled binary.
2. `candle` at the time of v0.9.0 lacked first-class CPU INT8
   quantization for MiniLM-class models.

`fastembed-rs` wraps `ort` (Rust bindings over the ONNX Runtime C
library) with model presets — `EmbeddingModel::AllMiniLML6V2`
resolves to the canonical quantized ONNX + tokenizer files on
HuggingFace. ~22 MB binary growth, INT8 quantization built in.

### Default-on Cargo feature

```toml
# crates/solo-storage/Cargo.toml
[dependencies]
fastembed = { workspace = true, optional = true }

[features]
default = ["bundled-embedder"]
bundled-embedder = ["dep:fastembed"]
```

Default-on means `cargo build` → working zero-config daemon. The
**musl release shards** explicitly opt out (`--no-default-features`)
because the prebuilt `libonnxruntime` doesn't target musl — they
ship as a smaller binary without bundled embeddings, recall is
disabled until the operator wires Ollama. That's a documented
trade-off, not a regression.

### hf-hub cache, NOT `include_bytes!`

The original v0.9.0 plan called for `include_bytes!` of the ONNX +
tokenizer files under `assets/`. The implementation deviated for
three reasons (lifted from
[bundled.rs](../../crates/solo-storage/src/embedder/bundled.rs)):

1. **Repo bloat.** 22 MB ONNX + 1 MB tokenizer JSONs would put 23 MB
   of binary assets in git history, slowing every clone.
2. **No selection without rebuild.** `include_bytes!` locks the
   bundled model to one choice at compile time; the hf-hub cache lets
   the operator swap to a different fastembed-supported model
   without a custom build.
3. **fastembed already does this.** Its `EmbeddingModel::*` presets
   resolve to hf-hub URLs and cache them on first use — duplicating
   that with `include_bytes!` would have meant maintaining a parallel
   asset-loading code path.

First-use UX: `~22 MB` download from HuggingFace, one time, on a
warm internet connection. Subsequent runs load from disk; the
embedder caches the loaded `TextEmbedding` in-process via
`tokio::sync::OnceCell` so it pays the load cost once per daemon
lifetime.

## Alternatives considered

### Option A — `candle` + BGE-M3 (rejected)

Architecture-doc's original pick. Rejected on binary size (600 MB
embedded vs 22 MB bundled) — incompatible with the "single binary,
install via `cargo install` or scoop" distribution story.

### Option B — Ollama as default (rejected)

Treat Ollama as the default embedder; `solo init` checks for a
reachable Ollama daemon. **Defeats the zero-deps goal** — Ollama is
its own ~1 GB install. Kept as a sibling backend, not the default.

### Option C — `fastembed` + `all-MiniLM-L6-v2` (chosen)

What's described above.

### Option D — Voyage / hosted as default (rejected)

`voyage-3-large` would give the best quality. Requires an API key and
network at recall time — fails the zero-credential goal. Available
via the swap path (would be a fifth `Embedder` impl).

## Consequences

**What this locks in:**

- A fresh install has a working recall path with no operator
  configuration. This is the headline shipping property of v0.9.0.
- Re-embedding is cheap to opt into — swap the `[embedder]` block in
  `solo.config.toml`, run `solo reembed`, and the migration tool
  reads `embedder.name()` + `version()` to decide what to recompute
  (ADR-0002's `Embedder` trait already supports this).
- INT8-quantized 384-dim vectors are the storage default. Down from
  the architecture doc's planned 1024-dim FP32 — recall quality is
  lower, storage is ~10x smaller. That trade is real and worth
  surfacing in user-facing docs.

**What this defers:**

- Cross-lingual recall quality is mediocre on MiniLM. Operators with
  non-English corpora should swap to a multilingual fastembed model
  (`paraphrase-multilingual-MiniLM-L12-v2`) or Ollama with a
  multilingual embedder.
- Embedder versioning across releases. `BundledEmbedder` reports a
  version string, but we have not yet had to ship a breaking model
  upgrade. The first one will exercise `solo reembed`'s detection
  path.

**What this complicates:**

- Build-from-source on musl Linux requires `--no-default-features`
  + manually wiring Ollama. Documented in BUILDING.md.
- The musl release shards have a different recall capability surface
  than glibc/macOS/Windows. Document the asymmetry in release notes
  on every shipped version.

## Implementation map

| Concept | Code |
|---|---|
| `BundledEmbedder` impl | [crates/solo-storage/src/embedder/bundled.rs](../../crates/solo-storage/src/embedder/bundled.rs) |
| `OllamaEmbedder` sibling | [crates/solo-storage/src/embedder/ollama.rs](../../crates/solo-storage/src/embedder/ollama.rs) |
| `StubEmbedder` test impl | [crates/solo-storage/src/embedder/stub.rs](../../crates/solo-storage/src/embedder/stub.rs) |
| Backend selection | [crates/solo-storage/src/embedder_registry.rs](../../crates/solo-storage/src/embedder_registry.rs) |
| Cargo feature flag | [crates/solo-storage/Cargo.toml](../../crates/solo-storage/Cargo.toml) |
| Design rationale + plan deviation | [bundled.rs module docstring](../../crates/solo-storage/src/embedder/bundled.rs#L1) |
