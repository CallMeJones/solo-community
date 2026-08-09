# Solo SDK Starters

Tiny, dependency-free starters for scripts and agents that talk to a local
Solo daemon at `http://127.0.0.1:17821`.

What is included:

- `sdks/typescript/` - an ESM JavaScript runtime with TypeScript
  declarations, plus a mock-server smoke test.
- `sdks/python/` - a Python standard-library client plus a mock-server
  smoke test.
- `examples/typescript/` and `examples/python/` - short remember/recall,
  direct HTTP, framework, and MCP tools-list examples.

Distribution decision: these are repo-local copy-in starters, not npm/PyPI
packages yet. Build versioned starter zip bundles with:

```bash
python sdks/package_starters.py --out .smoke/sdk-starters --check
```

See `sdks/DISTRIBUTION.md` for the registry release bar.

The starters cover the agent basics:

- Community Memory Library readiness via `status()`;
- remember, Inbox capture/review, recall, context, inspect, update, and forget;
- structured facts/entities, recent memories, and document helpers;
- MCP Streamable HTTP handshake plus `tools/list` and `tools/call`.

## No-daemon smoke tests

These tests start local mock HTTP servers and verify request/response shapes.
They do not need Solo to be installed or running.

```bash
node --test sdks/typescript/smoke-test.mjs
python sdks/python/smoke_test.py
```

Run the full local SDK matrix with:

```bash
python sdks/smoke_matrix.py
```

The matrix runs both SDK smoke tests, syntax-checks the dependency-free direct
HTTP examples, compiles the Python examples, and statically checks the optional
framework starters without installing their framework packages.

## Manual daemon smoke

Start Solo:

```bash
SOLO_PASSPHRASE=change-me solo daemon --http-port 17821
```

Then run the Python example:

```bash
python examples/python/remember_recall.py "Avery prefers planning notes with owners and dates."
```

For TypeScript projects, import `sdks/typescript/solo-client.js` directly.
The runtime has no package dependencies and ships with `solo-client.d.ts` for
type checking.

```ts
import { SoloClient } from "../../sdks/typescript/solo-client.js";

const solo = new SoloClient();
const status = await solo.status();
const saved = await solo.remember("Avery prefers planning notes with owners and dates.");
const facts = await solo.factsAbout("Avery", { includeAsObject: true, limit: 5 });
console.log(status.library.name, saved.memory_id, facts.length);
```

If your daemon uses bearer auth, set `SOLO_BEARER_TOKEN` or pass
`bearerToken` to the client constructor.

For the smallest possible daemon smoke, use the direct HTTP examples:

```bash
node examples/typescript/direct-http.mjs "Avery prefers planning notes with owners and dates."
python examples/python/direct_http.py "Avery prefers planning notes with owners and dates."
```

For framework projects, use the copy-in starters:

```bash
npm install ai @ai-sdk/openai zod
tsx examples/typescript/vercel-ai-sdk-memory.ts "Use my memory to plan Avery's next weekly review."

pip install openai-agents
python examples/python/openai_agents_memory.py "Use my memory to plan Avery's next weekly review."

pip install langgraph
python examples/python/langgraph_memory.py "Use my memory to plan Avery's next weekly review."
```

## Policy reminder

Use these clients with Solo's memory policy pack: retrieve before answering
when prior context matters, remember only durable user-approved facts, and do
not store secrets or raw credentials. For corrections, call `update()` when you
know the memory id or use the MCP `memory_update` tool through `mcp_call_tool`
/ `mcpCallTool`.
