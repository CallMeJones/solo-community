# Steward setup and capability states

Solo's bundled `all-MiniLM-L6-v2` model is an **encoder**. It powers semantic
recall, but it cannot write prose or structured knowledge. Themes, facts,
entities, relationships, and contradictions require an optional generative
Steward model.

The Settings capability panel reports five states:

- `ready` — available and returned data when relevant;
- `disabled` — the required Steward model is off;
- `pending` — clustering or extraction still has work queued;
- `empty` — the pipeline ran successfully but has no matching data;
- `failed` — the last readiness query or background run failed.

The same states and explanations are returned by `memory_context`, so agents
do not mistake a disabled graph for a graph with no facts.

## Provider choices

| Choice | Processing location | Credential | Notes |
|---|---|---|---|
| Disabled | none | none | Recall and documents still work. |
| Local Ollama | this device | none | Recommended privacy-first default; start with `qwen3:8b` or `qwen3:4b` on smaller machines. |
| Ollama Cloud | Ollama Cloud | `OLLAMA_API_KEY` reference, or a signed-in local Ollama daemon | Requires explicit hosted-processing consent. |
| Custom Ollama | configured operator endpoint | optional environment reference | Non-loopback endpoints require consent. |
| Anthropic | Anthropic API | environment reference | Requires consent; strong on ambiguous extraction. |
| OpenAI | OpenAI API | environment reference | Requires consent; model is selectable. |

Ollama's local API supports structured JSON output. Ollama Cloud does not
currently support that option, so Solo omits the unsupported format hint and
validates the returned JSON instead. Custom endpoints try structured output
and retry without the hint when the server reports it as unsupported.

After changing provider settings, restart Solo so the Steward runtime loads
the new model. Then choose **Backfill existing memories now**. The job first
clusters existing memories and then extracts derived knowledge in bounded
batches; progress, counts, and failures are visible in Settings and
`GET /v1/status`.

## Setup validation

`solo doctor --round-trip` creates an encrypted temporary library and proves
write → bundled MiniLM → HNSW → recall. It deletes that library afterward and
never writes test data to the real memory library.

When the live daemon owns `solo.lock`, `solo doctor --with-stats` queries its
authenticated `GET /v1/status` endpoint instead of attempting to open the
database a second time. Override the default endpoint with `--daemon-url`.

Before changing MiniLM, use `eval/corpora/retrieval-v1.json` as the gold
corpus and record recall@3, MRR, forbidden hits, latency, and vector/lexical
ablations. The legacy offline eval suite is lexical and is intentionally not
used to justify an embedding-model change.
