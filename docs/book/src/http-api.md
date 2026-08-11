# HTTP API

For non-MCP integrations Solo also speaks JSON over HTTP.
Same data dir, same writer + reader pool, same lockfile —
the HTTP transport is just a different surface in front of
the same engine.

This chapter covers:

  - The two ways to run an HTTP server.
  - The core memory endpoints with `curl` examples.
  - The `/v1/*` app-integration endpoints used by Solo Desktop and integrations.
  - Authentication (bearer token).
  - Loopback vs LAN binding and the safety guard around it.

## Two ways to run

### Daemon co-mode (loopback only)

```bash
solo daemon --http-port 17821
```

The daemon serves both its writer/reader pool and the HTTP
API on `127.0.0.1:17821`. Loopback only — no `--bind` flag,
no auth. Suitable for:

  - Local scripts and tools running on the same machine.
  - Browser-based UIs running on a different local port
    (CORS allows localhost origins).
  - Smoke testing the API surface.

### Standalone `solo http-serve`

```bash
solo http-serve --port 17821                                 # loopback
solo http-serve --bind 0.0.0.0 --port 17821 \                # LAN
                --bearer-token-file /etc/solo/token
```

The dedicated `http-serve` subcommand exposes additional
flags for non-loopback deployments:

  - **`--bind <ip>`** — IP address to bind. Defaults to
    `127.0.0.1`. Setting anything else (`0.0.0.0`, a LAN IP,
    a Tailscale IP) requires `--bearer-token-file`.
  - **`--port <port>`** — TCP port. Defaults to `17821`.
  - **`--bearer-token-file <path>`** — path to a file whose
    first line is the bearer token. The token validates on
    every request except `GET /health`.

Setting `--bind` to a non-loopback address without
`--bearer-token-file` is rejected at startup:

```text
Error: binding to 0.0.0.0 (non-loopback) requires
--bearer-token-file. Refusing to expose the API without
authentication.
```

This guard exists so an operator who tweaks `--bind` for
trusted-LAN deployment can't accidentally expose the data
dir without auth.

`http-serve` and `daemon --http-port` are mutually exclusive
on the same data dir — both want the lockfile.

## Endpoints

The `Authorization` column shows whether bearer auth applies
when configured. `GET /health` and `GET /openapi.json` are the
only public routes; everything else is authenticated in bearer/OIDC
deployments.

| method | path | auth | what it does |
|---|---|---|---|
| `GET` | `/health` | none | Returns `ok` (liveness probe). |
| `GET` | `/openapi.json` | none | OpenAPI 3.1 spec for the rest of the API. |
| `GET` | `/v1/status` | required | Community Memory Library readiness payload for local UIs and agent bridges. |
| `GET` | `/v1/steward/backfill` | required | Current or most recent derived-memory backfill progress. |
| `POST` | `/v1/steward/backfill` | required | Start bounded clustering and knowledge-extraction backfill. |
| `POST` | `/v1/settings/llm` | required | Save Steward provider, processing location, secret reference, and consent. |
| `GET` | `/v1/graph/nodes` | required | Paginated graph node catalog for solo-web. |
| `GET` | `/v1/graph/edges` | required | Paginated graph edge catalog for solo-web. |
| `GET` | `/v1/graph/inspect/{id}` | required | Full record drilldown for a graph node. |
| `GET` | `/v1/graph/neighbors/{id}` | required | Explicit + semantic neighbor graph for a node. |
| `GET` | `/v1/graph/stream` | required | SSE invalidation stream for live graph refresh. |
| `POST` | `/mcp` | required | MCP Streamable HTTP JSON-RPC request path. Returns JSON for responses, `202 Accepted` for notifications. First POST without `Mcp-Session-Id` creates a session and echoes the new id back in the response header. |
| `GET` | `/mcp` | required | MCP Streamable HTTP server-to-client SSE path. Requires `Mcp-Session-Id` from a prior POST; resumable via `Last-Event-ID`. |
| `DELETE` | `/mcp` | required | Explicit session termination (v0.11.4+). Requires `Mcp-Session-Id`. Returns `204 No Content` on success, `404` if the session id is unknown, `400` if the header is missing/malformed. |
| `POST` | `/memory` | required | Store a memory. Body: `{content, source_type?, source_id?}`. |
| `POST` | `/memory/search` | required | Vector-search. Body: `{query, limit?}`. |
| `POST` | `/memory/context` | required | Agent context bundle. Body: `{query, subject?, window_days?, limit?}`. |
| `POST` | `/memory/consolidate` | required | Trigger consolidation. Body: `{window_days?, force_merge?}` (or empty). |
| `POST` | `/backup` | required | Hot online backup to a server-side path. Body: `{to, force?}`. |
| `GET` | `/memory/{id}` | required | Inspect a memory by id. |
| `PATCH` | `/memory/{id}` | required | Correct an active memory. Body: `{content}`. |
| `DELETE` | `/memory/{id}` | required | Forget a memory by id. Optional `?reason=` query param. |
| `GET` | `/memory/themes` | required | Recent cluster themes. Optional `?window_days=&limit=`. |
| `GET` | `/memory/facts_about` | required | Structured facts by subject. Query: `?subject=&predicate?=&limit?=`. |
| `GET` | `/memory/entities` | required | Entity discovery over structured facts. Query: `?query=&limit?=`. |
| `GET` | `/memory/contradictions` | required | Steward-flagged contradictions. Optional `?limit=`. |
| `POST` | `/memory/contradictions/resolve` | required | Resolve/reopen a contradiction. |
| `POST` | `/v1/project/facts` | required | Project facts JSON envelope from an explicit project descriptor. |
| `POST` | `/v1/project/decisions` | required | Store a project-scoped decision with structured project metadata. |
| `POST` | `/v1/project/decisions/search` | required | Search project decisions using structured project scope. |
| `POST` | `/v1/project/policy` | required | Render a project memory policy without reading workspace files. |

### Health

```bash
curl http://127.0.0.1:17821/health
# ok
```

200 OK with body `ok`. No auth required even when bearer auth
is configured — health probes have to work without
credentials.

### Status

```bash
curl http://127.0.0.1:17821/v1/status
```

Response:

```json
{
  "ok": true,
  "version": "0.12.0+<commit>",
  "build": {
    "version": "0.12.0",
    "git_sha": "<commit>"
  },
  "library": {
    "name": "Community Memory Library",
    "ready": true
  },
  "embedder": {
    "name": "bundled:all-MiniLM-L6-v2",
    "version": "v2",
    "dim": 384,
    "dtype": "f32"
  },
  "capabilities": {
    "memory_recall": {"state": "ready", "explanation": "..."},
    "documents": {"state": "ready", "explanation": "..."},
    "clustering": {"state": "pending", "explanation": "..."},
    "knowledge_extraction": {"state": "disabled", "explanation": "..."},
    "facts": {"state": "disabled", "explanation": "..."},
    "entities": {"state": "disabled", "explanation": "..."},
    "graph": {"state": "disabled", "explanation": "..."},
    "contradictions": {"state": "disabled", "explanation": "..."}
  },
  "steward": {
    "config_mode": "none",
    "processing_location": "knowledge extraction disabled",
    "coverage": {
      "active_episodes": 0,
      "clusters": 0,
      "pending_clusters": 0,
      "triples": 0
    },
    "backfill": null
  },
  "mcp": {
    "sessions": 0
  },
  "runtime": {
    "pid": 1234,
    "platform": "windows",
    "data_dir": "C:\\Users\\you\\.solo"
  }
}
```

`/v1/status` is intentionally different from `/health`.
`/health` is public and tiny; `/v1/status` goes through the same authenticated
boundary as the graph and MCP surfaces. Local UIs should use it when they need
operator-facing readiness: package/build identity, Memory Library state,
embedder identity, capability explanations, derived coverage/backfill,
provider processing location, runtime ownership, and MCP session count. Bearer/OIDC
deployments require the same `Authorization` header as the rest of the
authenticated API.

### Configure the Steward

Local Ollama:

```bash
curl -X POST http://127.0.0.1:17821/v1/settings/llm \
  -H 'Content-Type: application/json' \
  -d '{"mode":"ollama","endpoint":"local","base_url":"http://localhost:11434","model":"qwen3:8b","hosted_processing_consent":false}'
```

Direct Ollama Cloud:

```bash
curl -X POST http://127.0.0.1:17821/v1/settings/llm \
  -H 'Content-Type: application/json' \
  -d '{"mode":"ollama","endpoint":"cloud","base_url":"https://ollama.com","model":"gpt-oss:120b-cloud","api_key_env":"OLLAMA_API_KEY","hosted_processing_consent":true}'
```

The Cloud secret stays in `OLLAMA_API_KEY`; the response and config contain
only that variable name. Hosted Ollama, Anthropic, OpenAI, and non-loopback
custom endpoints are rejected unless explicit consent is true. Restart Solo
after saving so the runtime loads the new provider.

### Backfill derived memory

```bash
curl -X POST http://127.0.0.1:17821/v1/steward/backfill \
  -H 'Content-Type: application/json' \
  -d '{"limit":50,"max_batches":20}'

curl http://127.0.0.1:17821/v1/steward/backfill
```

The start call returns `202 Accepted`. The job clusters existing memories,
then extracts abstractions, facts, entities, relationships, and contradiction
candidates in bounded batches. Only one backfill runs at a time. Progress and
failure details also appear under `steward.backfill` in `/v1/status`.

### Remember

```bash
curl -X POST http://127.0.0.1:17821/memory \
  -H 'Content-Type: application/json' \
  -d '{"content": "Solo daemon started using on 2026-05-07."}'
```

Response:

```json
{"memory_id": "019e0425-51fa-7ff2-a095-871df676d440"}
```

200 on success. The `source_type`, `source_id`, and `salience` fields are
optional; omitting them defaults `source_type` to `"user_message"`,
`source_id` to `null`, and `salience` to `0.5`.

#### `salience`

Available since v0.11.2. Optional `number` in the closed range
`[0.0, 1.0]` that hints at how important an episode is. Higher salience
boosts the episode's score in recall and protects it from tier-decay
during consolidation. Defaults to `0.5` if omitted.

Validation:
  - NaN or out-of-range values are rejected with `400 Bad Request`
    (`"salience must be a finite value in [0.0, 1.0]"`).
  - This matches the parity of the MCP `memory_remember` tool's
    `salience` arg.

```bash
curl -X POST http://127.0.0.1:17821/memory \
  -H 'Content-Type: application/json' \
  -d '{"content": "Quarterly board meeting moved to Friday.", "salience": 0.9}'
```

### Recall

```bash
curl -X POST http://127.0.0.1:17821/memory/search \
  -H 'Content-Type: application/json' \
  -d '{"query": "memory daemon", "limit": 3}'
```

Response:

```json
{
  "hits": [
    {
      "rowid": 1,
      "memory_id": "019e0425-...",
      "cos_distance": 0.0,
      "content": "Solo daemon started using on 2026-05-07.",
      "source_type": "user_message",
      "tier": "hot"
    }
  ],
  "index_len": 1,
  "candidates_considered": 1
}
```

`hits` is empty when no episodes match. `index_len` is the
total number of vectors in the HNSW index at query time —
useful for distinguishing "the index is empty" from "every
match was forgotten." `candidates_considered` is the number
of raw HNSW candidates Solo examined before filtering out
document chunks and inactive or forgotten episodes. `limit`
is clamped to `[1, 100]`; default is 5.

`cos_distance` semantics: 0.0 = identical to the query
vector, larger = less similar. (See the Model Selection
chapter for embedder-specific notes on what "similar"
means.)

### Memory Context

```bash
curl -X POST http://127.0.0.1:17821/memory/context \
  -H 'Content-Type: application/json' \
  -d '{"query": "Quotient launch work", "subject": "Quotient", "limit": 5}'
```

Response shape:

```json
{
  "query": "Quotient launch work",
  "subject": "Quotient",
  "recall": { "hits": [], "index_len": 0, "candidates_considered": 0 },
  "themes": [],
  "facts": [],
  "entities": [],
  "contradictions": [],
  "graph": {"relationship_facts": [], "literal_facts": [], "relationship_paths": []},
  "sections": {
    "themes": {"status": "pending", "count": 0, "explanation": "Clustering has not produced a cluster yet.", "warning": null},
    "facts": {"status": "disabled", "count": 0, "explanation": "Knowledge extraction is off because no Steward model is active.", "warning": null},
    "entities": {"status": "disabled", "count": 0, "explanation": "Knowledge extraction is off because no Steward model is active.", "warning": null},
    "graph": {"status": "disabled", "count": 0, "explanation": "Knowledge extraction is off because no Steward model is active.", "warning": null},
    "contradictions": {"status": "disabled", "count": 0, "explanation": "Knowledge extraction is off because no Steward model is active.", "warning": null}
  }
}
```

`/memory/context` is the agent-oriented retrieval bundle. It combines
episodic recall, recent themes, optional facts about `subject`, and
known contradictions into one bounded response. Each derived section reports
`ready`, `disabled`, `pending`, `empty`, or `failed` with an explanation, so an
agent can distinguish an unavailable graph from a valid empty result. Agents should use it
when they need working context before answering, then drill into
specific items with `/memory/{id}` or the derived/document endpoints.

### Consolidate

```bash
# Default scope (unbounded window, no force-merge):
curl -X POST http://127.0.0.1:17821/memory/consolidate

# Or with explicit scope:
curl -X POST http://127.0.0.1:17821/memory/consolidate \
  -H 'Content-Type: application/json' \
  -d '{"window_days": 30, "force_merge": false}'
```

Response (full `ConsolidationReport`):

```json
{
  "episodes_seen": 42,
  "clusters_built": 3,
  "episodes_clustered": 27,
  "abstractions_built": 3,
  "triples_built": 12,
  "contradictions_found": 1,
  "clusters_merged": 0,
  "clusters_absorbed": 1,
  "existing_clusters_merged": 0,
  "abstractions_regenerated": 1
}
```

The HTTP report includes the four re-consolidation counters
(`clusters_merged`, `clusters_absorbed`,
`existing_clusters_merged`, `abstractions_regenerated`) that
the CLI one-liner doesn't yet surface. See _Consolidation
Cycle_ for what each pass does.

The body is optional — `POST /memory/consolidate` with no
body uses default scope (`window_days: null`, `force_merge:
false`). Empty body is parsed as defaults rather than
rejected.

### Backup (hot, against a running daemon)

```bash
curl -X POST http://127.0.0.1:17821/backup \
  -H 'Content-Type: application/json' \
  -d '{"to": "/var/backups/solo/2026-05-07.db"}'
```

Response:

```json
{"path": "/var/backups/solo/2026-05-07.db", "elapsed_ms": 11}
```

Runs SQLite's online backup API through Solo's writer connection,
producing a self-contained SQLCipher database file at the given
**server-side** path. Encrypted with the same Argon2id-derived
raw key as the source — restore by copying the file (and the
source's `solo.config.toml`) to a fresh data dir and opening
with the same passphrase. See [Backups & Recovery](./backups-and-recovery.md).

  - **`to`** (required) — server-side absolute path. The
    parent directory must exist; the file itself is created.
  - **`force`** (optional, default `false`) — overwrite an
    existing file at `to`. Without `force`, an existing file
    returns 400.

The backup runs against the writer's existing connection
without acquiring `solo.lock`, so the daemon keeps serving
reads + writes during the operation. SQLite takes a
page-level snapshot of the source at backup-start time;
concurrent writes after that point land in the source but
don't appear in the backup. Typical backup time for personal
corpora (single-digit GB): tens of milliseconds.

400 if `to` is empty, the file exists without `force`, or
the parent directory doesn't exist. 500 if the daemon was
spawned without key material (older daemon binary) or the
backup itself fails (disk full, permission denied).

### Inspect

```bash
curl http://127.0.0.1:17821/memory/019e0425-51fa-7ff2-a095-871df676d440
```

Response (full `EpisodeRecord`):

```json
{
  "memory_id": "019e0425-...",
  "ts_ms": 1730932345214,
  "source_type": "user_message",
  "source_id": null,
  "content": "Solo daemon started using on 2026-05-07.",
  "tier": "hot",
  "status": "active",
  "confidence": 0.9,
  "strength": 0.5,
  "salience": 0.5,
  "created_at_ms": 1730932345220,
  "updated_at_ms": 1730932345220,
  "encoding_context_json": "{}",
  "provenance_json": null
}
```

`encoding_context_json` and `provenance_json` are **raw JSON
strings** as stored in the database — clients that want
structured access should `JSON.parse` them client-side. The
shape lets future extensions add fields to the encoded
context without changing the wire schema.

404 if the id doesn't exist or has already been hard-deleted
(soft-deleted = `status: "forgotten"` is still returned by
inspect — the row is preserved for forensics).

### Update

```bash
curl -X PATCH http://127.0.0.1:17821/memory/019e0425-... \
  -H 'Content-Type: application/json' \
  -d '{"content": "Solo daemon started using HTTP on 2026-05-07."}'
```

Response shape:

```json
{
  "memory_id": "019e0425-...",
  "rowid": 1,
  "content": "Solo daemon started using HTTP on 2026-05-07.",
  "updated_at_ms": 1779303143409
}
```

`PATCH /memory/{id}` rewrites one active episode, refreshes its
embedding, and updates the HNSW entry used by recall. It returns 404
for an unknown id, 400 for malformed ids or empty content, and 409 if
the memory exists but is no longer active.

### Forget

```bash
curl -X DELETE http://127.0.0.1:17821/memory/019e0425-... \
  -G --data-urlencode 'reason=test'
```

Response: `204 No Content`. The row's `status` flips to
`forgotten`; the HNSW vector stays in the index but
recall results filter out non-`active` rows. The optional
`reason` query parameter is logged but not yet persisted —
provenance for forgotten memories is a v0.4 candidate.

### MCP transport (`/mcp`)

Solo serves the MCP Streamable HTTP transport (spec version
`2025-03-26`) on a single `/mcp` endpoint. Any MCP-aware
client that speaks Streamable HTTP can connect — see
[MCP Integration](./mcp-integration.md#http-transport-streamable-mcp)
for ChatGPT-connector setup.

#### Session lifecycle

```bash
# 1. POST without Mcp-Session-Id → server assigns one; echoed in
#    the `Mcp-Session-Id` response header.
curl -v -X POST http://127.0.0.1:17821/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize",
       "params":{"protocolVersion":"2025-03-26","capabilities":{},
                 "clientInfo":{"name":"my-client","version":"0"}}}'
# Response includes:  Mcp-Session-Id: 019e0425-...

# 2. Subsequent POSTs reuse the session id:
curl -X POST http://127.0.0.1:17821/mcp \
  -H 'Content-Type: application/json' \
  -H 'Mcp-Session-Id: 019e0425-...' \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'

# 3. Optional GET — opens a long-lived SSE stream for
#    server-pushed notifications (memory_remember events,
#    progress, etc.). Resumable via `Last-Event-ID`.
curl -N http://127.0.0.1:17821/mcp \
  -H 'Mcp-Session-Id: 019e0425-...' \
  -H 'Accept: text/event-stream'

# 4. v0.11.4+: explicit session termination.
curl -X DELETE http://127.0.0.1:17821/mcp \
  -H 'Mcp-Session-Id: 019e0425-...'
# 204 No Content on success; 404 if the session is unknown;
# 400 if the header is missing or malformed.
```

#### `protocolVersion` notes

Solo's HTTP `initialize` response reports
`"protocolVersion": "2025-03-26"` — the spec version that
introduced Streamable HTTP. The stdio transport (`solo
mcp-stdio`) reports `"2024-11-05"` because rmcp 0.1.x (the
crate Solo embeds for stdio) only implements that spec.
Different transports, different versions, each honest. Modern
MCP clients tolerate the difference; both interoperate.

### Derived Memory

The derived endpoints read the Steward's consolidation outputs.

```bash
curl 'http://127.0.0.1:17821/memory/entities?query=Quot&limit=5'
curl 'http://127.0.0.1:17821/memory/facts_about?subject=Quotient&include_as_object=true'
curl 'http://127.0.0.1:17821/memory/contradictions?limit=5'
```

`/memory/entities` is the discovery step when a client has a partial
name and needs the graph's canonical entity id. `/memory/facts_about`
returns active SPO triples for a subject. `/memory/contradictions`
returns flagged disagreements, including lifecycle fields such as
`status`, `resolved_at_ms`, `resolution_note`, and `winning_triple_id`.

To resolve a contradiction after the user clarifies which side is
current:

```bash
curl -X POST http://127.0.0.1:17821/memory/contradictions/resolve \
  -H 'Content-Type: application/json' \
  -d '{
    "a_id": "triple-a",
    "b_id": "triple-b",
    "kind": "other",
    "status": "resolved",
    "resolution_note": "The newer preference is current.",
    "winning_triple_id": "triple-b"
  }'
```

`status` defaults to `resolved`; valid values are `unresolved`,
`resolved`, and `reopened`.

### Project Memory

Project endpoints mirror the `solo project ... --json` command shapes
for Desktop and local integrations while keeping the daemon as the only
database owner:

```bash
curl -X POST http://127.0.0.1:17821/v1/project/decisions \
  -H 'Content-Type: application/json' \
  -d '{
    "project": {
      "name": "Solo",
      "id": "solo",
      "root": "C:\\Users\\Example\\Projects\\solo-community",
      "tags": ["memory", "desktop"]
    },
    "decision": "Use daemon HTTP endpoints for Desktop project memory."
  }'
```

The daemon does not read the supplied root path for these routes. The
project descriptor is metadata used for scoping, JSON envelopes, and
policy text. Desktop reads `.solo/project.toml` locally, then sends the
descriptor to the daemon.

## Authentication

When started with `--bearer-token-file`, the server requires
every request (except `GET /health`) to carry:

```text
Authorization: Bearer <token>
```

Where `<token>` is the first whitespace-trimmed line of the
token file. Missing or wrong token returns:

```text
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Bearer realm="solo"
```

Comparison is byte-exact (constant-time on equal-length
inputs). The token file should be owned by the user running
Solo and `chmod 0400` to keep other local users out.

Generate a token however you like — `openssl rand -hex 32`,
`pwgen 64 1`, or a UUID. Solo doesn't care what shape the
token is, only that requests match the file's contents.

## Loopback vs LAN

`127.0.0.1` is the safe default. Solo's CORS layer permits
**any localhost origin** (any `localhost:*` or
`127.0.0.1:*`), which lets browser UIs running on a
different local port call the API without preflight
friction, including the `PATCH /memory/{id}` correction flow.
Cross-origin attacks via a victim's browser are
prevented by the loopback bind itself.

For LAN access (Tailscale, ssh tunnel, trusted home network),
use `--bind <lan-ip> --bearer-token-file <path>`. The bearer
token guard is mandatory in this mode — the server refuses
to start non-loopback without it.

For internet exposure, **don't**. Solo's threat model is
local-or-trusted-LAN. If you need internet access to your
memory, terminate at a reverse proxy with TLS, IP allowlists,
and additional auth — and even then, audit before you do it.

## OpenAPI spec

`GET /openapi.json` returns the OpenAPI 3.1 schema for every
authenticated route. Suitable for code generators, Postman /
Insomnia / Bruno imports, or quick API documentation viewers.

```bash
curl http://127.0.0.1:17821/openapi.json | jq '.paths'
```

The schema is hand-written in
`crates/solo-api/src/http.rs` — kept in sync with the
handlers but not auto-generated from them. If the schema
disagrees with the runtime behavior, the runtime is the
source of truth (file an issue with both).

## Errors

Failure responses use a consistent JSON shape:

```json
{"error": "human-readable message", "status": 400}
```

The `status` field mirrors the HTTP status code — redundant
with the response line but convenient for clients that log
the body separately. With these status codes:

| status | meaning |
|---|---|
| 400 | Invalid request (malformed JSON, unknown id format, validation failure). |
| 401 | Missing or wrong bearer token. |
| 404 | Memory id not found. |
| 409 | Conflict (e.g. embedder identity drift, lockfile already held by another Solo process). |
| 500 | Internal error (DB failure, embedder failure, LLM failure). |

5xx responses include the underlying error message in
`error`. They do **not** include stack traces or
data-directory paths — internal details stay in the daemon
log, not the wire response.

## Limitations

  - **Single data dir per server.** No multi-tenancy. If you
    want multiple independent memory stores, run multiple
    Solo instances on different ports + data dirs.
  - **No streaming.** Recall returns the full result list in
    one JSON document; consolidate runs to completion before
    responding. Long-running consolidates can block the
    HTTP request — use the daemon's
    `--consolidate-interval-secs` for fire-and-forget
    scheduling instead.
  - **No batch endpoints.** `POST /memory` writes one
    memory per request. For bulk imports, the most
    efficient pattern is N concurrent writes against the
    daemon — the writer actor serialises internally without
    HTTP-level batching being a meaningful win.
  - **No row-payload streaming.** The graph stream is SSE, but it
    emits invalidation notifications only. Clients refetch the
    affected REST pages instead of receiving row payloads in the
    stream.
