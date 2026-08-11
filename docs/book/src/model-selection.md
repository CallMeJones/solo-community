# Model Selection

Solo uses two independent model layers:

- The **embedder** converts memories and queries to vectors for semantic
  recall and clustering.
- The **Steward model** generates themes, abstractions, facts, entities,
  relationships, and contradiction candidates from clusters.

An embedding model cannot replace the Steward. MiniLM is an encoder: it
produces useful vectors, but it does not generate or reason over structured
knowledge.

## Embedder

### Bundled MiniLM (default)

Official Windows and Linux packages include `all-MiniLM-L6-v2` with
384-dimensional vectors. It runs locally and enables semantic recall without
Ollama, an API key, or a first-use download.

This is the right default for Community. It is small, private, and already
works with Solo's hybrid vector plus lexical retrieval. Before replacing it,
measure candidates on `eval/corpora/retrieval-v1.json`, including:

- recall@3 and mean reciprocal rank;
- forbidden-result violations for corrections and negations;
- vector-only, lexical-only, and hybrid ablations;
- indexing and query latency on Windows and Linux.

### Ollama embeddings (optional)

Use Ollama when a user deliberately wants a different local embedding model:

```bash
ollama pull nomic-embed-text
solo migrate-embedder ollama --model nomic-embed-text
```

Changing vector models requires a supervised re-embed. Do not edit the
persisted embedder identity by hand or mix vectors from different models.

### Stub embedder (tests only)

The deterministic stub exists for tests and development. Its vectors do not
carry semantic meaning and are not a production alternative to MiniLM.

## Steward model

The Steward reads selected memory clusters and returns structured JSON. Its
work is optional: raw recall and documents remain available when the Steward
is disabled.

### No Steward (fresh-install default)

Fresh libraries persist `[llm] mode = "none"`. Clustering can still run, but
knowledge extraction is shown as disabled and facts/entities/graph/
contradictions remain unavailable. An API key inherited from the shell never
enables hosted memory processing automatically.

### Local Ollama (recommended default)

Local Ollama keeps model inference and memory content on the device:

```bash
ollama pull qwen3:8b
```

Use `qwen3:8b` as the normal starting point and `qwen3:4b` on constrained
machines. Larger models may improve ambiguous entity resolution and
contradiction judgments, but also require more memory and increase latency.

Configure it in **Solo Web → Settings → Steward LLM → Ollama → Local**.
The persisted shape is:

```toml
[llm]
mode = "ollama"
endpoint = "local"
base_url = "http://localhost:11434"
model = "qwen3:8b"
hosted_processing_consent = false
```

Local Ollama supports native JSON mode. Solo still validates responses before
writing derived knowledge.

### Ollama Cloud

Ollama Cloud is supported in two forms:

1. Direct Cloud API at `https://ollama.com/api`, authenticated with a bearer
   token referenced by `OLLAMA_API_KEY`.
2. A signed-in local Ollama daemon using a `-cloud` model. The HTTP connection
   is loopback, but the prompt is processed in Ollama Cloud.

Both forms require explicit hosted-processing consent. The API key value is
never stored in `solo.config.toml`; only its environment-variable name is
persisted.

```toml
[llm]
mode = "ollama"
endpoint = "cloud"
base_url = "https://ollama.com"
model = "gpt-oss:120b-cloud"
api_key_env = "OLLAMA_API_KEY"
hosted_processing_consent = true
```

Ollama Cloud currently does not support the local API's structured-output
switch. Solo prompts for JSON, validates it, and makes one bounded repair
attempt. Persistent invalid output is reported as a failed extraction; raw
memory is preserved.

### Custom Ollama endpoint

A custom loopback endpoint is treated as local. A non-loopback custom endpoint
is treated as off-device and requires consent. Review the endpoint operator's
logging, retention, and training policy before enabling it.

### Anthropic and OpenAI

Hosted Anthropic and OpenAI models are useful when local hardware is limited
or the derived layer needs stronger handling of ambiguity. They usually offer
better difficult-case reasoning than small local models, at the cost of
off-device processing, latency, and API charges.

Solo requires explicit consent and stores only secret references:

```toml
[llm]
mode = "anthropic"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-4-6"
hosted_processing_consent = true
```

```toml
[llm]
mode = "openai"
api_key_env = "OPENAI_API_KEY"
model = "gpt-5o"
hosted_processing_consent = true
```

Use the Web setup flow rather than editing TOML when possible. It explains
where content is processed and will not save a hosted configuration until the
user consents.

## Practical recommendation

1. Keep bundled MiniLM for recall.
2. Start with knowledge extraction disabled until the user chooses it.
3. Recommend local `qwen3:8b`; fall back to `qwen3:4b` on smaller systems.
4. Offer Ollama Cloud, Anthropic, or OpenAI when the user accepts off-device
   processing or needs stronger extraction quality.
5. Run **Backfill existing memories now** after setup and review coverage,
   failures, and sample provenance before trusting the graph broadly.

See [Steward Setup](./steward-setup.md) for the full privacy and operational
flow.
