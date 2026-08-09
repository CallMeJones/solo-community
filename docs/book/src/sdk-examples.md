# SDK Examples

Solo now includes small SDK starters for local scripts and agents:

- `sdks/typescript/` - dependency-free ESM runtime with TypeScript
  declarations.
- `sdks/python/` - dependency-free Python standard-library client.
- `examples/typescript/` and `examples/python/` - short remember/recall,
  direct HTTP, framework, and MCP tools-list examples.

For now these are copy-in starters, not published npm/PyPI packages. Build
release-ready zip bundles with:

```bash
python sdks/package_starters.py --out .smoke/sdk-starters --check
```

The distribution policy lives in `sdks/DISTRIBUTION.md`.

The SDKs cover the practical agent loop:

- check Community Memory Library readiness with `status`;
- call remember, Inbox capture, recall, context, inspect, update, and
  forget;
- query structured facts/entities and recent Inbox memories;
- list, search, inspect, ingest, and forget documents;
- open a Streamable HTTP MCP session and call `tools/list` or
  `tools/call`.

The clients default to `http://127.0.0.1:17821`, matching:

```bash
SOLO_PASSPHRASE=change-me solo daemon --http-port 17821
```

If your daemon uses bearer auth, pass `bearerToken`/`bearer_token` or set
`SOLO_BEARER_TOKEN` in the example scripts.

## No-daemon smoke tests

The starter includes contract smoke tests that use mock HTTP servers. They
verify JSON body shapes, update/forget helpers, and MCP
session-header handling without starting Solo:

```bash
node --test sdks/typescript/smoke-test.mjs
python sdks/python/smoke_test.py
```

For the full local SDK matrix, including direct HTTP syntax checks and static
checks for the optional framework examples:

```bash
python sdks/smoke_matrix.py
```

## Direct HTTP

Use these when you want the smallest possible integration path or a quick
daemon smoke without importing the SDK client:

```bash
node examples/typescript/direct-http.mjs "Avery prefers planning notes with owners and dates."
python examples/python/direct_http.py "Avery prefers planning notes with owners and dates."
```

Both scripts read `SOLO_URL` and `SOLO_BEARER_TOKEN`, call
`/v1/status`, store a memory with `/memory`, and retrieve context with
`/memory/context`.

## Remember and recall

Python:

```bash
python examples/python/remember_recall.py "Avery prefers planning notes with owners and dates."
```

TypeScript:

```ts
import { SoloClient } from "../../sdks/typescript/solo-client.js";

const solo = new SoloClient();
const status = await solo.status();
const saved = await solo.remember("Avery prefers planning notes with owners and dates.");
const inbox = await solo.rememberInbox("Review Avery's planning-note preference.", {
  salience: 0.6,
});
const inboxItems = await solo.memoryInbox({ limit: 10 });
await solo.reviewMemory(inbox.memory_id, { state: "approved" });
const recall = await solo.recall("planning notes", { limit: 3 });
const context = await solo.context("planning notes", { subject: "Avery", limit: 3 });
const entities = await solo.entities("Avery", { limit: 5 });
const facts = await solo.factsAbout("Avery", { includeAsObject: true, limit: 5 });
const recent = await solo.recentMemories({ limit: 10 });
console.log(status.library.name, inboxItems.items.length, recall.hits.length, context.sections.recall.count, entities.length, facts.length, recent.nodes.length);
```

Python:

```python
status = solo.status()
saved = solo.remember("Avery prefers planning notes with owners and dates.")
inbox = solo.remember_inbox("Review Avery's planning-note preference.", salience=0.6)
inbox_items = solo.memory_inbox(limit=10)
solo.review_memory(inbox["memory_id"], state="approved")
recall = solo.recall("planning notes", limit=3)
context = solo.context("planning notes", subject="Avery", limit=3)
entities = solo.entities("Avery", limit=5)
facts = solo.facts_about("Avery", include_as_object=True, limit=5)
recent = solo.recent_memories(limit=10)
```

## Documents

Document helpers use server-side paths, so `ingestDocument` /
`ingest_document` can read only files that the Solo daemon process can
open.

TypeScript:

```ts
const docs = await solo.listDocuments({ limit: 5 });
const hits = await solo.searchDocuments("planning notes", { limit: 3 });
const detail = docs[0] ? await solo.inspectDocument(docs[0].doc_id) : null;
```

Python:

```python
docs = solo.list_documents(limit=5)
hits = solo.search_documents("planning notes", limit=3)
detail = solo.inspect_document(docs[0]["doc_id"]) if docs else None
```

## MCP helper

The starters include a small MCP helper: initialize a Streamable HTTP session,
send `notifications/initialized`, call `tools/list`, and make safe
`tools/call` requests. Full MCP clients should still use an MCP SDK for
production transports, but this is enough to smoke a Solo `/mcp` endpoint and
test a real memory tool:

```bash
python examples/python/mcp_tools_list.py
```

Python:

```python
session = solo.mcp_connect("my-agent", "0.1.0")
tools = solo.mcp_list_tools(session)
context = solo.mcp_call_tool(session, "memory_context", {
    "query": "planning notes",
    "subject": "Avery",
})
```

## Memory policy

When these SDKs are used inside agents, pair them with the
[Memory Policy Pack](./memory-policy-pack.md): retrieve context before
answering when prior context matters, remember durable facts only, use update
paths for corrections, and do not store secrets.

## Framework starters

The framework examples are copy-in starters for projects that already use the
agent framework. They are not part of the dependency-free runtime smoke tests,
but `python sdks/smoke_matrix.py` verifies their basic shape without installing
framework packages.

Vercel AI SDK:

```bash
npm install ai @ai-sdk/openai zod
tsx examples/typescript/vercel-ai-sdk-memory.ts "Use my memory to plan Avery's next weekly review."
```

The example preloads Solo context, exposes a `memoryContext` tool for
additional retrieval, and exposes a `rememberDurableFact` tool for durable,
user-approved facts.

OpenAI Agents SDK:

```bash
pip install openai-agents
python examples/python/openai_agents_memory.py "Use my memory to plan Avery's next weekly review."
```

LangGraph:

```bash
pip install langgraph
python examples/python/langgraph_memory.py "Use my memory to plan Avery's next weekly review."
```

The LangGraph starter keeps Solo as the long-term memory source and leaves
LangGraph free to handle orchestration, checkpoints, and model nodes.
