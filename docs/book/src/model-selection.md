# Model Selection

Solo has two models you can swap independently:

  - **An embedder** — turns text into a vector. Used for both
    storing memories and matching recall queries. Solo ships
    with a built-in `StubEmbedder` and supports BGE-M3 for real
    semantic embeddings.
  - **A Steward LLM** — the consolidation pipeline's reasoning
    engine. Reads clusters of related memories and emits
    structured abstractions, triples, and contradictions. Solo
    speaks the OpenAI Chat Completions wire format, so any
    backend that exposes it works — local-first via Ollama is
    the recommended path; hosted Anthropic / OpenAI are also
    supported. The Steward is optional — without one,
    consolidation runs the cheap clustering pass only.

You can mix and match — every combination is supported. This
chapter covers what each option costs you and which to pick
when.

## Embedder choice

### StubEmbedder (default)

If you don't tell Solo which embedder to use, it uses the
StubEmbedder: a deterministic BLAKE3 hash of the input text,
projected to a 1024-dim vector.

What it gives you:

  - **Fast.** No model load, no inference. Sub-millisecond
    per memory.
  - **Offline.** No model download, no network round-trip.
  - **Tiny.** Zero disk besides the database itself.
  - **Identity-match recall.** `recall("foo")` finds memories
    whose text was exactly `"foo"`.

What it doesn't give you:

  - **Semantic recall.** `recall("memory")` won't find a
    memory containing the word `"remembrance"` — the BLAKE3
    hashes of those two strings are unrelated. Stub vectors
    are identity hashes, not embeddings.

The stub exists so Solo runs end-to-end out of the box without
a 1.2 GB download. It's the right pick for smoke tests, local
development, and one-off scripts where you only need exact-
text recall. It is **not** the right pick for an AI assistant
trying to remember what you said last week using paraphrased
phrasing.

#### Consolidation with the stub (v0.11.2+)

When `solo daemon` or `solo consolidate` runs while the stub is
active, a `tracing::error!` event fires once per consolidate pass:

```
consolidation running with StubEmbedder — cluster membership is
BLAKE3-hash proximity, not semantic. Configure SOLO_EMBEDDER=bundled
or =ollama for real vectors.
```

The clusters that result group episodes by surface-text similarity, not
meaning — downstream `semantic_abstractions` (LLM-generated) will read
as plausible but the data backing them isn't useful. **In production,
either configure a real embedder or set `SOLO_REFUSE_STUB_EMBEDDER=1`
to make this case a hard error.** See Environment Variables for the
flag.

### BGE-M3

For real semantic recall, point Solo at a local BGE-M3 model
directory. BGE-M3 is BAAI's open-weights multilingual
embedding model — strong recall in English plus reasonable
behavior across Chinese, Japanese, and most European
languages.

The simplest way to install it:

```bash
solo download-model
```

This pulls three files (`config.json`, `tokenizer.json`,
`model.safetensors`) from HuggingFace into
`<data-dir>/models/BAAI/bge-m3/`. Total ~1.2 GB. The
download is resumable — if it gets interrupted, just re-run
the command. SHA256 verification runs on completion.

When it finishes, the command prints:

```text
✓ all 3 files verified

Set:
  export SOLO_BGE_M3_DIR=/home/me/.solo/models/BAAI/bge-m3
```

Set that env var (or `set` it for cmd.exe / `$env:` for
PowerShell), and the next `solo init` or `solo daemon` run
loads BGE-M3 instead of the stub.

If you already have a BGE-M3 directory from `huggingface-cli`
or a manual `git clone https://huggingface.co/BAAI/bge-m3`,
just point `SOLO_BGE_M3_DIR` at it — Solo doesn't care how
the files got there, only that they're present.

### Switching between embedders requires migration

Stub vectors and BGE-M3 vectors live in **different vector
spaces**. A BLAKE3 hash and a real embedding represent
completely different things; they don't compare meaningfully.
If you store some memories with the stub and then point Solo
at BGE-M3, your HNSW index ends up with a mix of the two,
and recall returns incoherent results.

Solo logs a warning when it detects an unsafe embedder switch
against a non-empty database. The supported fix is a supervised
embedder migration, which validates the new embedder, backs up
the config and HNSW snapshots, rewrites every stored vector, and
then garbage-collects stale embedding rows. For Ollama:

```bash
ollama pull nomic-embed-text
solo migrate-embedder ollama --model nomic-embed-text
```

The Solo Controls Settings page exposes the same flow as
**Embedder Migration** for the installed Windows app.

Migration re-runs the active embedder on every episode in the
database, deletes stale HNSW snapshots, and lets the next daemon
start rebuild the graph from SQL embeddings. Time scales with
corpus size and embedder speed.

If you add BGE-M3 before storing a single memory, you can
skip reembed entirely; the database is empty so there's
nothing to regenerate.

## LLM Steward choice

The Steward is separate from the embedder. Embeddings power vector
search; the Steward/LLM path creates abstractions, triples, and
contradictions after memories have been clustered. It does three
things:

  1. **Abstraction**: turns a cluster of related memories
     into a one-paragraph summary.
  2. **Triples**: extracts subject-predicate-object facts
     from each abstraction.
  3. **Contradiction detection**: flags pairs of triples
     that disagree.

All three need an LLM that can follow structured output
instructions reliably. Solo treats the LLM as opaque — it
sends prompts, parses the response, and stores the result
with a provenance trail.

### No LLM (default)

If no Steward LLM is configured,
Solo runs the clustering pass only. Memories get grouped into
clusters by vector similarity, but no abstraction or
contradiction detection happens. `consolidate` reports
`abstractions_built=0 triples_built=0 contradictions_found=0`
in this mode.

This is fine if you're using Solo as a recall-only store and
don't need the structured derived layer. It's also the
fallback for low-cost / offline development.

Re-consolidation passes that fold drift across runs (existing-
vs-existing cluster merge) are also LLM-gated — they need the
Steward to resolve which cluster wins. Without an LLM, drift
catch-up doesn't happen.

### Ollama (recommended for local)

[Ollama](https://ollama.com) is a separate single-binary tool
that runs LLMs locally. It handles model download, GPU/CPU
detection, automatic layer offload when a model doesn't fit
fully in VRAM, and exposes an OpenAI-compatible HTTP endpoint
on `localhost:11434/v1`. Solo speaks that wire format, so
Ollama drops in as a Steward backend with no Solo code
changes.

Three reasons Ollama is the recommended local path over the
hosted alternatives below:

  1. **Privacy.** Your conversations never leave your machine.
     Hosted LLMs see every cluster Solo asks them to abstract.
  2. **Cost.** Zero per-call cost. A daily consolidate against
     a large corpus costs nothing in API tokens.
  3. **Air-gapped support.** Works in environments that can't
     reach `api.anthropic.com` / `api.openai.com`.

Setup:

```bash
# 1. Install Ollama (Linux/macOS)
curl -fsSL https://ollama.com/install.sh | sh
# Windows: download installer from https://ollama.com

# 2. Pull a model that fits your GPU (see the table below)
ollama pull qwen2.5-coder:7b

# 3. Point Solo at it (one-flag shorthand)
solo consolidate --ollama-model qwen2.5-coder:7b
# Or for the daemon:
solo daemon --consolidate-interval-secs 3600 --ollama-model qwen2.5-coder:7b
```

Ollama runs as a background daemon after install — it stays
running after `ollama pull` completes, listening on port
11434. `solo consolidate` connects to it via HTTP just like it
would `api.openai.com`.

`--ollama-model <MODEL>` is shorthand for setting:

```text
OPENAI_API_KEY=ollama
OPENAI_BASE_URL=http://localhost:11434/v1
OPENAI_MODEL=<MODEL>
```

and unsetting `ANTHROPIC_API_KEY` (so the explicit flag wins
over the Anthropic > OpenAI precedence rule). Override the base
URL or API key by setting `OPENAI_BASE_URL` / `OPENAI_API_KEY`
explicitly — useful for a non-default Ollama port, a remote
Ollama instance, or an auth-proxy in front of Ollama. The env-
var dance below is equivalent if you'd rather configure Solo
via your shell profile:

```bash
export OPENAI_API_KEY=ollama                                # any non-empty string
export OPENAI_BASE_URL=http://localhost:11434/v1
export OPENAI_MODEL=qwen2.5-coder:7b
solo consolidate
```

#### Hardware tiers

Pick a model that fits your GPU's VRAM (or CPU's RAM, with
caveats):

| VRAM | Model | Quantization | Notes |
|---|---|---|---|
| **8 GB** | `qwen2.5-coder:7b` | Q4_K_M (~5 GB) | Good for personal corpus; fits RTX 3060/4060/2060 Super |
| **12 GB** | `qwen2.5-coder:7b` | Q8_0 (~8 GB) | Same model, higher quality bits |
| **16 GB** | `qwen2.5-coder:14b` | Q4_K_M (~9 GB) | Better on ambiguous contradiction detection |
| **24 GB** | `qwen2.5-coder:32b` | Q4_K_M (~20 GB) | Closest to hosted-Sonnet quality |
| **48 GB** | `qwen2.5-coder:32b` | Q8_0 (~35 GB) | Near-lossless local quality |
| **CPU only, 16+ GB RAM** | `qwen2.5-coder:3b` | Q4_K_M (~2 GB RAM) | Slow (~30 sec to several min per cluster) but works |
| **Apple Silicon, 64+ GB unified** | `qwen2.5-coder:32b` | Q4-Q8 | Metal-backed; slower than discrete GPU but works |

#### Why qwen2.5-coder

Solo's Steward prompts return JSON. The "coder" variant of
Qwen2.5 is heavily trained on structured output, so it adheres
to the JSON schema reliably. Other Ollama-supported models work
— `llama3.3`, `phi4`, `mistral-small`, `deepseek-v2.5`,
`gemma3` — just `ollama pull` whichever fits your hardware and
set `OPENAI_MODEL` accordingly. If JSON parsing failures show
up in `solo consolidate` output, that usually means the model
isn't reliable enough for structured output; try a coder
variant or step up the parameter count.

#### Quality vs hosted

A 7B-Q4 local model is meaningfully behind hosted Sonnet 3.5
on subtle contradiction detection and on producing tight,
information-dense abstractions. A 32B-Q4 local model closes
most of that gap. For a personal-memory corpus where the
abstractions are summaries you'll re-read rather than ship to
production, the local-quality trade is usually worth the
privacy + cost win. For a high-stakes corpus where contradiction
accuracy matters, hosted Anthropic remains the highest-quality
option.

### Anthropic

Set:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
export ANTHROPIC_MODEL=claude-3-5-sonnet-20241022   # default
```

Solo posts to `https://api.anthropic.com/v1/messages` with
the standard three-role schema (system / user / assistant).
The model name is whatever you set; the default works well.

Anthropic is the recommended Steward when available — the
re-consolidation tetralogy was designed and tuned against
Sonnet 3.5, and the structured-output prompts are stable.

### OpenAI

Set:

```bash
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=gpt-4o-mini                  # default
```

Solo posts to `https://api.openai.com/v1/chat/completions`.
Defaults are tuned for the conventional chat models
(`gpt-4o-mini`, `gpt-4o`, `gpt-4-turbo`, `gpt-3.5-turbo`).

> **Note.** OpenAI's reasoning models (o1, o3, o4) require
> `max_completion_tokens` instead of `max_tokens`. Solo
> currently sends `max_tokens`, which the reasoning models
> reject with a 400. Use a conventional chat model for now.
> If you need reasoning-model support, file an issue with
> your use case.

### Other OpenAI-compatible services

For Ollama, see the dedicated section above — it's the
recommended local backend. For any other OpenAI-compatible
endpoint (LM Studio, Together, Groq, Mistral, DeepInfra), set
`OPENAI_BASE_URL` to their `/v1` URL:

```bash
export OPENAI_API_KEY=lm-studio                              # any non-empty string
export OPENAI_BASE_URL=http://127.0.0.1:1234/v1
export OPENAI_MODEL=mistral-nemo-instruct-2407@q4_k_m
```

Endpoint defaults that work without further config:

  - **LM Studio** — `http://127.0.0.1:1234/v1` (default port 1234).
  - **Together, Groq, Mistral, DeepInfra, et al.** — their public
    `/v1` endpoint with your issued API key.

Solo strips trailing slashes from the base URL defensively, so
both `https://api.example.com/v1/` and
`https://api.example.com/v1` resolve to the same endpoint.

### Precedence: Anthropic wins on conflict

If both `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` are set, Solo
uses Anthropic. The reasoning:

  - v0.2 shipped Anthropic-only. Users who upgraded with
    `ANTHROPIC_API_KEY` already in their environment shouldn't
    suddenly get a different provider in v0.3+.
  - The natural workflow for "switch to OpenAI" is "I'll set
    `OPENAI_API_KEY`" — having to additionally remember to
    *unset* `ANTHROPIC_API_KEY` would be a footgun.

If you want OpenAI specifically, set `OPENAI_API_KEY` and
make sure `ANTHROPIC_API_KEY` is not set in the shell that
launches Solo.

There's no `SOLO_LLM_PROVIDER=openai` selector — Solo picks
based on which env var is present. If you ever need to
override on a per-run basis, use a wrapper:

```bash
env -u ANTHROPIC_API_KEY solo daemon                # force OpenAI
ANTHROPIC_API_KEY=sk-ant-... solo daemon            # force Anthropic
```

### Retry and backoff

Both Anthropic and OpenAI HTTP clients retry transient
failures automatically:

  - **Retried**: HTTP 429, HTTP 500–599, network timeouts,
    connection failures.
  - **NOT retried**: HTTP 408 (server says *you* were too
    slow), 4xx other than 429 (your request is bad), decode
    errors (the response was malformed in a way that won't
    fix on retry).
  - **Schedule**: full-jitter exponential backoff. 3 retries,
    500 ms base, 10 s cap. `Retry-After` header is honoured
    when present (seconds form only — the HTTP-date form is
    not parsed).

Worst-case added latency per `complete()` call is ~30 s
before giving up. For a `consolidate` run with N clusters and
M LLM calls per cluster, the worst case is `N * M * 30 s`
extra — operators notice and stop.

There are no config knobs for retry today. If you need to
disable retries (e.g., during testing) you'd have to call
`with_retry_config(RetryConfig::none())` from Rust code; the
CLI doesn't expose this. File an issue if it bites.

## Cost considerations

For a corpus of N clusters, each consolidate produces up to
`N` LLM calls (one per cluster for abstraction) plus `N`
contradiction-check calls. Re-consolidation passes add
roughly `N` more for stale-abstraction regen. Pricing back
of envelope:

  - Anthropic Sonnet 3.5: ~$3 / 1M input tokens, ~$15 / 1M
    output. Solo's prompts are short (1-2 KB input each) and
    output is bounded (~1 KB). Per-cluster cost ~$0.003
    input + $0.015 output ≈ $0.02 round-trip.
  - OpenAI gpt-4o-mini: ~$0.15 / 1M input, ~$0.6 / 1M output.
    ~$0.0008 per cluster.
  - LM Studio / Ollama / Groq: $0 (your hardware) to a
    fraction of the cloud rates.

A 100-cluster corpus consolidating once per day with
Sonnet 3.5 costs ~$2 / day. With gpt-4o-mini, ~$0.08 / day.
With local models, free.

For most personal use cases (single-digit GB of memory, daily
consolidate), even cloud LLMs are well under a dollar a day.

## Recommendations

| Use case | Embedder | Steward |
|---|---|---|
| Smoke testing the install | Stub | none |
| Personal use, default | BGE-M3 | **Ollama qwen2.5-coder (size to GPU)** |
| Personal use, max quality online | BGE-M3 | Anthropic Sonnet 3.5 |
| Cost-constrained, but cloud-OK | BGE-M3 | OpenAI gpt-4o-mini |
| Air-gapped / privacy-strict | BGE-M3 | Ollama (any local model) |
| CI / automated tests | Stub | none |
| Read-only memory store (no consolidation) | BGE-M3 | none |
| Multi-language corpus | BGE-M3 | Anthropic Sonnet 3.5 (or Ollama with a multilingual model) |

When in doubt, start with **BGE-M3 + Ollama qwen2.5-coder
sized to your GPU**. If contradiction-detection accuracy
matters more than privacy/cost, swap the Steward for
**Anthropic Sonnet 3.5** — that's the combination Solo was
originally tuned against and has the most road miles.
