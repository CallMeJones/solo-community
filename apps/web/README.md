# Solo Web

Community Web UI for Solo's memory store. Its source lives at `apps/web` in the
same repository and release unit as the Rust Core, API, CLI, and tray. It
renders episodes, documents, chunks, clusters, and entities as an interactive
force-directed graph, modelled on Obsidian's Graph View. It also includes
memory-maintenance controls for correcting episodes, discovering entity
matches, and resolving contradictions.

The app connects to Solo's live `/v1/graph/*` API. Two transports are supported:

| Transport                             | When to use                                                                                                                                                                           |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **HTTP server** (default, port 17821) | Full graph topology — edges, entity nodes, SSE stream. Start `solo http-serve`.                                                                                                       |
| **MCP bridge** (port 7436)            | No `http-serve` available — Solo is already running via Claude Desktop in stdio mode. Start `npm run bridge`. Episode and cluster nodes only; edges and entity nodes are unavailable. |

---

## Quick start — HTTP server (full features)

```bash
# Terminal 1 — start Solo's HTTP server
SOLO_PASSPHRASE=your-passphrase solo http-serve --port 17821

# Terminal 2 — start solo-web
cd /path/to/solo-community/apps/web
npm ci
npm run dev
```

Open http://localhost:5173. The status strip turns green when Solo is reachable.

Solo Community presents one default **Memory Library** with unlimited memories,
documents, and logical projects. Community has no database selector or
multi-library management surface; every local workflow uses the same encrypted
`solo.db`.

---

## Quick start — MCP bridge (no http-serve required)

Use this when Solo is already running via Claude Desktop (stdio mode) and you
don't want to start a second daemon. The bridge spawns its own `solo mcp-stdio`
process — two processes share the same WAL database safely.

```bash
# Terminal 1 — start the bridge
cd /path/to/solo-community/apps/web
SOLO_PASSPHRASE=your-passphrase npm run bridge

# Terminal 2 — start solo-web
npm run dev
```

Open http://localhost:5173. In the Settings dialog, click the **MCP bridge**
chip — it fills the Solo daemon URL to `http://127.0.0.1:7436` automatically.
Save, and the status strip will turn green.

**Bridge limitations vs. HTTP server:**

- Episode and cluster nodes only — entity nodes require a full-table entity
  scan that would need O(N) sequential MCP calls; not done in bridge mode.
- No edges — cluster→episode membership and triple edges require
  `memory_inspect_cluster` calls per cluster; omitted for latency.
- 15-second graph read cache — data written by Claude Desktop appears in
  solo-web after the next cache expiry, not immediately.
- Episode recall uses `"the"` as the broad-match query. Memories that contain
  no common English tokens may not appear in the graph.
- `memory_update` and `memory_contradiction_resolve` invalidate the cache
  immediately so the inspector reflects your write on the next graph fetch.

---

## Env vars

| Var                   | Default                  | Purpose                                                                                           |
| --------------------- | ------------------------ | ------------------------------------------------------------------------------------------------- |
| `VITE_SOLO_API_URL`   | `http://127.0.0.1:17821` | Solo daemon HTTP base URL. Change to `http://127.0.0.1:7436` for bridge mode, or set in Settings. |
| `VITE_SOLO_USE_MOCKS` | unset                    | Set to `1` for deterministic mock graph data (offline UI dev).                                    |

Copy `.env.example` to `.env` to override locally.

### Bridge env vars

| Var                | Default   | Purpose                                                           |
| ------------------ | --------- | ----------------------------------------------------------------- |
| `SOLO_PASSPHRASE`  | —         | Database passphrase. Forwarded automatically to `solo mcp-stdio`. |
| `SOLO_BRIDGE_PORT` | `7436`    | Port the bridge HTTP server listens on.                           |
| `SOLO_BIN`         | `solo`    | Path to the `solo` binary (useful if not on `PATH`).              |

---

## Settings dialog

Open the gear icon (or `Ctrl+,`) to configure:

- **Transport chip** — click **HTTP server** or **MCP bridge** to quick-fill the URL field.
- **Solo daemon URL** — editable; overrides the chip selection.
- **Bearer token** — required when `solo http-serve` is started with `--bearer-token-file`; kept in `sessionStorage`, not persistent storage.

Endpoint settings persist in `localStorage`. Bearer credentials survive reloads in the current browser session but are removed when that session closes. Older persistent bearer values are migrated automatically.

---

## Connection feedback

The status strip polls Solo `/v1/status` every 15 seconds. Click the service
pill to retry immediately. The Solo pill names the active transport:

- `Solo HTTP` means `http://127.0.0.1:17821`; start `solo http-serve --port 17821`.
- `MCP bridge` means `http://127.0.0.1:7436`; start `npm run bridge`.
- `Solo custom` means the Settings dialog contains a non-default Solo URL.

Memory inbox edit, forget, and resolve actions surface response details from
Solo or the bridge when a mutation fails.

---

## Scripts

| Command                    | What it does                                                                                              |
| -------------------------- | --------------------------------------------------------------------------------------------------------- |
| `npm run dev`              | Start the Vite dev server (port 5173)                                                                     |
| `npm run bridge`           | Start the MCP stdio bridge (port 7436)                                                                    |
| `npm run build`            | TypeScript + Vite production build                                                                        |
| `npm run build:pilot`      | Community production build plus emitted-artifact boundary check                                           |
| `npm test`                 | Vitest unit suite                                                                                         |
| `npm run test:live`        | Live contract tests against a running Solo at `SOLO_API_URL`                                              |
| `npm run e2e`              | Full deterministic Playwright suite, including Windows visual snapshots                                   |
| `npm run e2e:ci`           | Cross-platform route, workflow, staged-document, and accessibility suite against the built pilot artifact |
| `npm run e2e:visual:pilot` | Windows visual regression suite against the built pilot artifact                                          |
| `npm run e2e:live`         | Opt-in Playwright smoke against a running Solo at `SOLO_API_URL`                                          |
| `npm run lint`             | ESLint                                                                                                    |
| `npm run typecheck`        | `tsc --noEmit`                                                                                            |
| `npm run format`           | Prettier                                                                                                  |

`npm run e2e` starts Vite with deterministic graph mocks and Playwright-owned
Solo HTTP mocks. It covers desktop and mobile route rendering, accessibility,
Windows visual regression, settings, MCP probe, inbox review/edit/forget/resolve,
export import, native path import, backup, logs, memories search, and the default
Community product boundary. `npm run e2e:ci` first creates and verifies the
production pilot artifact, then runs route, workflow, staged browser-document,
and accessibility coverage against that artifact without platform-specific
visual snapshots.

Browser document imports snapshot the Solo URL, bearer, and internal library route for the full
prepare/upload/commit/ingest operation. They can cancel before commit, resume a
known committed extraction without re-uploading, honor Solo's configured
original-file retention default, and expose document forget and retained-asset
deletion through a daemon-backed lifecycle catalog that survives reloads.

`npm run e2e:live` starts Vite without `VITE_SOLO_USE_MOCKS` and points the UI
at `SOLO_API_URL`. Use it after starting a real Solo daemon:

```bash
SOLO_API_URL=http://127.0.0.1:17821 npm run e2e:live
```

---

## Tech stack

- **Vite 5** + **React 18** + **TypeScript 5**
- **Tailwind CSS 3** for styling
- **react-force-graph-2d** + **react-force-graph-3d** (vasturiano) for rendering
- **TanStack Query 5** for server state
- **Zustand 5** for client state (selection, filters, view mode)
- **ESLint 8** + **Prettier 3** + **Vitest 2** for the dev loop

---

## Feature status

| Feature                                       | HTTP server       | MCP bridge                |
| --------------------------------------------- | ----------------- | ------------------------- |
| Episode nodes                                 | ✅                | ✅                        |
| Cluster nodes                                 | ✅                | ✅                        |
| Document/chunk nodes                          | ✅                | ❌                        |
| Entity nodes                                  | ✅                | ❌                        |
| Edges (triples, cluster members)              | ✅                | ❌                        |
| SSE live stream                               | ✅                | ❌                        |
| Inspector (episode)                           | ✅                | ✅                        |
| Inspector (cluster)                           | ✅                | ✅                        |
| Episode correction (PATCH)                    | ✅                | ✅                        |
| Contradiction resolution                      | ✅                | ✅                        |
| Entity lookup                                 | ✅                | ✅ (non-empty query only) |
| One default Memory Library                    | ✅                | ✅                        |
| Browser document upload + extraction status   | ✅                | ❌                        |

---

## Architecture

```
Browser (localhost:5173)
  │
  ├─ HTTP mode  ──▶  solo http-serve :17821 ──▶  Solo DB
  │
  └─ Bridge mode ──▶  mcp-bridge :7436  ──▶  solo mcp-stdio  ──▶  Solo DB
                       (Node.js, no deps)       (child process)
```

The bridge is a zero-dependency Node.js HTTP server (`scripts/mcp-bridge.mjs`)
that speaks MCP JSON-RPC 2.0 over the child process's stdio and translates
solo-web's REST calls to MCP tool calls. It is **loopback-only and has no
bearer-token auth** — treat it as a dev tool, not a production server.

---

## Reference

The authoritative HTTP contract is documented in
[`docs/book/src/http-api.md`](../../docs/book/src/http-api.md), and MCP/Web
integration is documented in
[`docs/book/src/mcp-integration.md`](../../docs/book/src/mcp-integration.md).
