# MCP Integration

[Model Context Protocol](https://modelcontextprotocol.io/) (MCP) is
the standard way for AI assistants to talk to local tools. Solo
speaks MCP over **two transports**:

- **Streamable HTTP** (recommended) — a long-running Solo daemon
  serves `/mcp` on an HTTP port. Every MCP client connects to the
  same daemon. All clients share state, see each other's writes,
  and the `solo-web` UI updates in real time as memories change.
- **Stdio** (legacy / fallback) — an MCP client spawns
  `solo mcp-stdio` as a subprocess and communicates via JSON-RPC on
  the child's stdin/stdout. The historical default; still required by
  Claude Desktop unless you proxy through `mcp-remote`.

For nearly every user, the HTTP-daemon pattern is the right answer.
Stdio remains supported and necessary for clients that don't yet
speak HTTP transport directly.

## Architecture at a glance

```
              ┌────────────────────────────────────────────────┐
              │  solo daemon  (one long-running process)       │
              │                                                │
              │   writer-actor + SQLCipher DB + HNSW index     │
              │   consolidate + triples-batch timers           │
              │   HTTP server on 127.0.0.1:17821               │
              │     /mcp        — Streamable HTTP MCP          │
              │     /memory/*   — REST                         │
              │     /v1/graph/* — REST + SSE live stream       │
              │     /v1/status  — health + metadata            │
              └─────┬──────────────┬─────────────┬─────────────┘
                    │              │             │
        SSE (live)  │   HTTP+MCP   │   HTTP+MCP  │   HTTP+MCP
                    │              │             │  (via tunnel)
                    ▼              ▼             ▼
              ┌─────────┐    ┌──────────┐   ┌──────────┐
              │ solo-web│    │ Claude   │   │ ChatGPT  │
              │ (browser│    │ Code     │   │ Connector│
              │  UI)    │    │ Codex    │   │ (HTTPS + │
              │         │    │ Cursor   │   │  OAuth)  │
              └─────────┘    └──────────┘   └──────────┘

                            ┌── npx mcp-remote (stdio→HTTP shim) ──┐
                            ▼                                      ▼
                       Claude Desktop                        Cursor (legacy)
```

**One writer, many subscribers.** When any MCP client writes a
memory through `/mcp`, the writer-actor broadcasts an invalidation
event. solo-web's SSE subscription picks it up live; other MCP
sessions see consistent reads on their next call.

## Quick start — recommended setup

```bash
# 1. Start the daemon once (auto-start on login recommended)
SOLO_PASSPHRASE=your-passphrase solo daemon

# Output:
# INFO  solo daemon: HTTP server listening on http://127.0.0.1:17821
# INFO  Community Memory Library opened; writer-actor ready
# INFO  consolidate timer enabled (interval 3600s)
```

Then configure each MCP client to talk to `http://127.0.0.1:17821/mcp`.

The daemon owns the data dir for as long as it runs. Stop it with
`Ctrl+C`; the snapshot saves and the lock releases automatically.

Solo can also detect the local config path and manage client config for
Claude Desktop, Cursor, and Codex. Dry-run is the default. `--apply`
creates parent directories, preserves existing top-level keys and other
MCP server entries, makes a timestamped backup when the config file
already exists, then writes through a temporary file before renaming it
into place. The generated config never includes `SOLO_PASSPHRASE` or
plaintext bearer-token headers.

```bash
solo setup-client list
solo setup-client claude-desktop --dry-run
solo setup-client cursor --dry-run
solo setup-client codex --scope user --dry-run
solo setup-client codex --scope project --dry-run
solo setup-client claude-desktop --apply
solo setup-client cursor --apply
solo setup-client codex --scope user --apply
solo setup-client verify
solo setup-client doctor
```

Every client addresses Community's one Memory Library. Use separate Solo data
directories and daemon ports when a workflow requires hard isolation.

`solo setup-client verify [claude-desktop|cursor|codex]` checks the local
JSON or TOML shape and confirms the Solo MCP server entry is present
without probing the live daemon. It also flags `SOLO_PASSPHRASE` and
`Authorization: Bearer ...` values in client config as plaintext secret
leaks. For Codex project scope, use `solo setup-client verify codex
--scope project`.

`solo setup-client doctor [claude-desktop|cursor|codex]` combines the
local config check with a short `/mcp` reachability probe and a safe
`tools/list` call when the endpoint is reachable. It reports missing
config files, malformed JSON/TOML, whether the Solo server entry is
installed, whether the MCP endpoint is reachable, the MCP tool count,
and whether critical memory tools such as `memory_context`,
`memory_inbox`, and `memory_review` are present. Use `--format json`
for structured diagnostics.

Solo Desktop's Connected Tools panel separates those checks: **Config**
means the local client file contains a valid Solo entry, **Daemon MCP**
means the tray successfully initialized Solo MCP and listed tools for the
Memory Library, and **Client** remains a manual smoke check until the
actual app has loaded Solo. For Codex rows, Solo Desktop can run
`codex mcp list` directly and report whether Codex lists the `solo`
server. The Windows smoke helper can also classify Claude Code with
`claude mcp list`. Claude Desktop and Cursor still require an app-side
check. Use the per-client **Doctor** action there for the combined config
and endpoint diagnostic without copying a command.

On Windows release machines, `scripts/windows_mcp_client_smoke.ps1`
packages the same boundary into one repeatable support check. It starts
or reuses a local daemon, hard-fails broken `/mcp` initialization,
`tools/list`, required memory tools, and invalid client config, then
classifies Codex, Claude Code, Claude Desktop, and Cursor app-loading
separately. Missing apps or trust prompts are reported as manual checks
rather than mistaken for endpoint failures. The helper prints a summary
that separates endpoint checks from app-load checks; pass
`-ReportPath <file>` to also write the same summary as JSON.

---

## Per-client configuration

### Claude Code (native HTTP MCP)

```bash
claude mcp add --transport http --scope user \
  solo http://127.0.0.1:17821/mcp
```

Put `--transport` and `--scope` before the server name. Use `--scope user`
for a private cross-project Solo route, or `--scope project` when the MCP
entry should live in that project's `.mcp.json`.

If you started the daemon with a bearer token (`--bearer-token-file`),
append the authorization header after the server URL:

```bash
--header "Authorization: Bearer <your-token>"
```

Use a bearer token any time the daemon is bound to a non-loopback
interface. This stores the token in the client config; `solo
setup-client verify` will report that as a plaintext secret leak.

### Codex CLI / IDE (native HTTP MCP)

In `~/.codex/config.toml` (or `.codex/config.toml` for project scope):

```toml
[mcp_servers.solo]
url = "http://127.0.0.1:17821/mcp"
# If bearer auth is enabled, prefer an env var instead of a static token:
# bearer_token_env_var = "SOLO_BEARER_TOKEN"
```

Or let Solo merge the local file safely:

```bash
solo setup-client codex --scope user --apply
solo setup-client verify codex

# From a trusted project root:
solo setup-client codex --scope project --apply
solo setup-client verify codex --scope project
```

Solo writes the URL (or a stdio command if you choose `--transport stdio`);
add `bearer_token_env_var` manually if your daemon requires bearer auth.
Verify the live connection with `codex mcp list` or `/mcp` inside Codex.

### Claude Desktop (HTTP via `mcp-remote` shim)

Claude Desktop only speaks stdio. The community-maintained
`mcp-remote` npm shim translates stdio↔HTTP so Claude Desktop can
reach the daemon's `/mcp` endpoint.

Edit `claude_desktop_config.json`:

  - **macOS**: `~/Library/Application Support/Claude/claude_desktop_config.json`
  - **Windows**: `%APPDATA%\Claude\claude_desktop_config.json`
  - **Linux**: `~/.config/Claude/claude_desktop_config.json`

```json
{
  "mcpServers": {
    "solo": {
      "command": "npx",
      "args": [
        "mcp-remote",
        "http://127.0.0.1:17821/mcp",
        "--header",
        "Authorization: Bearer <your-token>",
        "--transport",
        "http-only"
      ]
    }
  }
}
```

Drop the `--header` line if your daemon has no bearer auth. Restart
Claude Desktop; on next launch, `solo` appears under available tools
and operates against the shared daemon (so writes from Claude Code,
Codex, or solo-web are immediately visible).
Keeping the bearer header in this file stores the token in plaintext;
Solo's setup-client verifier treats that as a config problem.

Or let Solo merge the local file safely:

```bash
solo setup-client claude-desktop --apply
solo setup-client verify claude-desktop
```

If your daemon requires bearer auth, add the `--header` arguments
manually after applying; the setup helper avoids writing secrets into
client config.

### Cursor (HTTP via `mcp-remote` shim)

Same shape as Claude Desktop, configured in `~/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "solo": {
      "command": "npx",
      "args": [
        "mcp-remote",
        "http://127.0.0.1:17821/mcp",
        "--transport",
        "http-only"
      ]
    }
  }
}
```

Cursor picks up MCP config changes without a full restart.

Or let Solo merge the local file safely:

```bash
solo setup-client cursor --apply
solo setup-client verify cursor
```

### ChatGPT MCP connector (HTTPS + OAuth)

ChatGPT requires:

1. **HTTPS** (not HTTP)
2. **Publicly reachable URL** (OpenAI's cloud cannot reach `localhost`)
3. **OAuth 2.1 + Dynamic Client Registration** — bearer tokens are
   **NOT accepted** by ChatGPT's MCP connector. This is a hard
   requirement at the spec level.

#### Status

Solo **does not yet implement OAuth 2.1 + DCR**. Until that ships,
**ChatGPT MCP connector cannot connect to Solo**, regardless of
tunnel setup. This is the one MCP client that Solo can't satisfy
today.

#### Workaround if you must

You can run a third-party MCP gateway in front of Solo that
terminates the OAuth flow for ChatGPT and forwards authenticated
requests to Solo with a bearer token. Examples: `mcp-proxy`,
`supergateway`. This is an operator burden and not recommended for
casual use.

#### What ships in v0.12.0

The OAuth 2.1 work will let `solo daemon` expose its own OAuth
endpoints, register dynamic clients, issue tokens, and accept
ChatGPT's connector directly. See the dev-log entry for the design.

For everyone else (Claude Code, Codex, Claude Desktop, Cursor),
bearer-token + HTTP works today.

### solo-web (browser UI)

In solo-web's Settings dialog:

  - **Solo daemon URL**: `http://127.0.0.1:17821` (or wherever the
    daemon is bound)
  - **Bearer token**: paste the same token the MCP clients use, if
    any

Real-time updates come from the daemon's `/v1/graph/stream` SSE
endpoint. When any MCP client writes a memory, the graph view
refreshes live. See `apps/web/README.md` for details.

### Generic MCP clients

Any MCP client that supports the Streamable HTTP transport works
the same way:

  - **URL**: `http://127.0.0.1:17821/mcp` (or your tunnel URL)
  - **Optional `Authorization: Bearer <token>` header**
  - **`Mcp-Session-Id` is server-assigned** on the first POST; the
    client echoes it back on subsequent calls

If the client only supports stdio, use `npx mcp-remote` (see Claude
Desktop / Cursor sections).

---

## Tools reference

Once any MCP client is connected, the model sees memory tools under
the `memory_*` namespace:

| Tool | Arguments | Returns |
|---|---|---|
| `memory_remember` | `content`, optional `source_type`, `source_id`, `salience` | new `MemoryId` (UUID v7) |
| `memory_remember_batch` | array of remember items (max 200) | ordered array of new `MemoryId`s |
| `memory_recall` | `query`, optional `limit` (1–100, default 5) | array of `{rowid, memory_id, cos_distance, content, source_type, tier}` |
| `memory_context` | `query`, optional `subject`, `window_days`, `limit` | bounded recall + themes + facts + contradictions bundle |
| `memory_update` | `memory_id`, `content` | updated memory metadata |
| `memory_inbox` | optional `limit` | recent active memories with review state |
| `memory_review` | `memory_id`, optional `state`, `note` | inbox review update without changing memory content |
| `memory_forget` | `memory_id`, optional `reason` | confirmation text |
| `memory_inspect` | `memory_id` | full episode record |
| `memory_themes` | optional `window_days`, `limit` | recent cluster themes |
| `memory_entities` | `query`, optional `limit` | structured-graph entity ids |
| `memory_facts_about` | `subject`, optional filters | structured facts about a person/project/topic |
| `memory_contradictions` | optional `limit` | flagged disagreements and lifecycle fields |
| `memory_contradiction_resolve` | `a_id`, `b_id`, `kind`, optional status/note/winner | contradiction lifecycle update (atomic UPDATE + audit row via writer-actor) |
| `memory_inspect_cluster` | `cluster_id`, optional `full_content` | cluster summary and source episodes |
| `memory_ingest_document` | `path` | document ingest report |
| `memory_search_docs` | `query`, optional `limit` | matching document chunks |
| `memory_inspect_document` | `doc_id` | document metadata and chunk previews |
| `memory_list_documents` | optional `limit`, `offset`, `include_forgotten` | paginated document list |
| `memory_forget_document` | `doc_id` | document forget report |

The descriptions are crafted to give the assistant enough context
to use the tools correctly without further prompting. Override
them with system-prompt hints if your workflow needs different
phrasing.

---

## Authentication

### Today: bearer token

Generate a strong random token, store it in a file, point Solo at
it:

```bash
openssl rand -hex 32 > ~/.solo/bearer.token
chmod 600 ~/.solo/bearer.token
SOLO_PASSPHRASE=xxx solo daemon \
  --bind 127.0.0.1 --port 17821 \
  --bearer-token-file ~/.solo/bearer.token
```

When the daemon is bound to **anything other than loopback**
(`127.0.0.1`), `--bearer-token-file` is **required** — Solo refuses
to start without it as a safety check against accidental open
exposure on a LAN.

Every protected request must carry:

```
Authorization: Bearer <contents of the token file>
```

`/health` is exempt from auth. Everything else requires the header.

### Future: OAuth 2.1 + DCR

This would let MCP clients authenticate via standard OAuth flows without
operators having to pre-distribute bearer tokens. It is not part of the 0.12.0
Community release.

---

## Real-time updates (solo-web + SSE)

When the daemon's writer-actor commits any write (remember, update,
forget, contradiction resolve, document ingest), it publishes an
**invalidation event** on the Memory Library broadcast channel. Two
classes of subscriber receive it:

1. **solo-web** subscribes to `GET /v1/graph/stream` (an SSE
   endpoint) and refreshes the graph view live.
2. **MCP clients with an active session** (connected via `GET /mcp`
   SSE stream) receive the event as a `notifications/message`
   JSON-RPC envelope. The client can then refetch or invalidate
   caches.

This is what makes the one-daemon-many-clients pattern actually
useful — write from one client, see it everywhere immediately.

---

## Stdio mode (legacy / fallback)

`solo mcp-stdio` runs Solo's MCP server over stdin/stdout — no HTTP
listener, no shared state with other clients. This was the original
deployment model; the recommended path is now the HTTP daemon.

### When to use stdio mode

  - **You don't want a long-running daemon process** — fine if you
    only ever use one MCP client and don't run solo-web.
  - **Claude Desktop direct-spawn** — if you can't use the
    `mcp-remote` shim for some reason (offline environment, npx
    unavailable). Configure Claude Desktop with `command: solo`,
    `args: ["mcp-stdio"]`, env block with `SOLO_PASSPHRASE`. Single-
    client only; no concurrent Cursor, Codex, or solo-web.
  - **Single-user ad-hoc scripts** — embedding Solo in shell
    pipelines or test fixtures.

### Limitations

  - **Lockfile contention** — only one `solo mcp-stdio` (or daemon)
    per data dir at a time. Two MCP hosts both spawning stdio
    against `~/.solo` will conflict; the second fails to acquire
    the lock.
  - **No live updates to solo-web** — solo-web reads the HTTP API
    that stdio mode doesn't expose. Browser UI stays static
    relative to the running stdio session.
  - **Argon2 cost per spawn** — SQLCipher key derivation runs on
    each subprocess startup (~500 ms). Daemon mode pays this once.

### Gateway / proxy mode (`--no-lockfile`)

v0.11.5+ adds `--no-lockfile` to `solo mcp-stdio`. When a gateway
(Cloudflare Access, Pomerium, identity-aware proxy, containerised
multi-pod deployment) needs to spawn multiple ephemeral stdio
subprocesses against one shared data dir, the per-data-dir
lockfile gets in the way.

```bash
SOLO_PASSPHRASE=xxx solo mcp-stdio --no-lockfile
# or
SOLO_NO_LOCKFILE=1 solo mcp-stdio
```

At startup, Solo emits a `tracing::warn!`:

```text
WARN starting WITHOUT solo.lock acquisition (--no-lockfile).
Concurrent solo processes against the same data dir can corrupt
writer-actor state (ADR-0003). Only safe behind a gateway that
serialises writes externally.
```

**Safety**: `--no-lockfile` violates the writer-actor single-process
invariant. SQLite WAL keeps committed writes durable, but each
process's in-memory HNSW snapshot and `pending_index` outbox can
drift independently. Safe only when:

  - all spawned subprocesses are read-only, OR
  - sticky-session gateway routing ensures one user's writes always
    go to the same subprocess.

For most multi-client scenarios, **use the HTTP daemon instead** —
its `Mcp-Session-Id` multiplexing serves the same goal without
violating any invariants.

Use the HTTP daemon for multi-client scenarios; stdio proxy mode remains an
advanced local-only option.

---

## Verifying the connection

### Daemon health check

```bash
curl http://127.0.0.1:17821/health
# → ok
```

If you have bearer auth on:

```bash
curl http://127.0.0.1:17821/v1/status \
  -H "Authorization: Bearer <token>"
# → {"ok": true, "version": "0.12.0", "library": {"name": "Community Memory Library", "ready": true}, ...}
```

### MCP handshake check

```bash
curl http://127.0.0.1:17821/mcp \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "initialize",
    "params": {
      "protocolVersion": "2025-03-26",
      "capabilities": {},
      "clientInfo": {"name":"smoke","version":"0.0.0"}
    }
  }' -i
```

You should see `Mcp-Session-Id: <uuid>` in the response headers
and `"serverInfo": {"name": "solo", "version": "..."}` in the body.

### Tools list

```bash
curl http://127.0.0.1:17821/mcp \
  -H "Authorization: Bearer <token>" \
  -H "Mcp-Session-Id: <id from above>" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/list"}'
```

You should see all 20 `memory_*` tools listed.

### MCP Inspector

For full interactive testing, use the [MCP
Inspector](https://github.com/modelcontextprotocol/inspector):

```bash
npx @modelcontextprotocol/inspector \
  http://127.0.0.1:17821/mcp \
  --transport streamable-http \
  --header "Authorization: Bearer <token>"
```

It opens a web UI for sending tool calls and watching responses.

---

## Troubleshooting

### "another solo process already running"

`solo.lock` is held by another `solo daemon`, `solo mcp-stdio`, or
`solo http-serve` against the same data dir. Either:

  - **Stop the other one** (most likely what you want), or
  - **Use a different data dir** with `--data-dir <path>` or
    `SOLO_DATA_DIR=...`, or
  - **`--no-lockfile`** (stdio mode only; read the safety guidance
    in the section above before doing this).

### "session expired" / 404 on `GET /mcp`

`GET /mcp` requires a session opened by a prior `POST /mcp` whose
response carried `Mcp-Session-Id`. If your client lost the session
id (process restart, server-side eviction after idle timeout), do
another `initialize` POST first.

### Claude Desktop shows "solo" as failed / disconnected

Check the Claude Desktop logs (`Help → View Logs` on macOS). Common
causes:

  - **`mcp-remote` not installed** — `npm install -g mcp-remote` or
    just rely on `npx` (which auto-installs on first use; needs
    network).
  - **Daemon not running** — start `solo daemon` first.
  - **Wrong port or path** — confirm the daemon's HTTP bind matches
    the URL in `claude_desktop_config.json`.
  - **Bearer token mismatch** — if you manually added a plaintext
    bearer header, the token in the config doesn't match the file the
    daemon was started with.

### ChatGPT connector "auth required" / "401"

Solo doesn't yet support OAuth 2.1, which ChatGPT requires. There
is no current workaround for direct integration; see the ChatGPT
section above for status. v0.12.0 will close this gap.

### Recall returns nothing but you remembered something

If you remembered via stdio mode and are now reading via HTTP (or
vice versa), confirm both transports point at the same
`SOLO_DATA_DIR`. Different data dirs hold separate corpora.

If both point at the same data dir and recall still returns
nothing, check the daemon logs for `consolidate` and `embedder`
warnings — the StubEmbedder fallback (32-dim BLAKE3 hash) doesn't
return semantic matches.

---

## What this chapter doesn't cover

  - **OAuth 2.1 + DCR design** — not included in Community 0.12.0.
  - **`--no-lockfile` design and safety analysis** — an advanced local-only
    operator responsibility.
  - **The full HTTP REST surface** (`/memory/*`, `/v1/graph/*`,
    `/v1/status`, `/openapi.json`) — see
    [HTTP API](./http-api.md).
  - **HTTPS / native TLS** — currently use a tunnel (Cloudflare
    Tunnel, ngrok, Tailscale Funnel) or reverse proxy
    (Caddy, nginx); native `--tls-cert / --tls-key` is scoped for
    a future minor release.
