# Solo TypeScript Starter

This starter is intentionally small: one ESM runtime file, one TypeScript
declaration file, and one no-daemon smoke test.

```ts
import { SoloClient } from "./solo-client.js";

const solo = new SoloClient({
  baseUrl: "http://127.0.0.1:17821",
  bearerToken: process.env.SOLO_BEARER_TOKEN,
});

const status = await solo.status();
const saved = await solo.remember("Dana likes weekly summaries on Friday.", {
  sourceType: "sdk_example",
  salience: 0.7,
});
const inbox = await solo.rememberInbox("Review Dana's Friday summary preference.", {
  salience: 0.6,
});
const inboxItems = await solo.memoryInbox({ limit: 10 });
await solo.reviewMemory(inbox.memory_id, { state: "approved" });
const recall = await solo.recall("weekly summaries", { limit: 3 });
const context = await solo.context("weekly summaries", { subject: "Dana" });
const facts = await solo.factsAbout("Dana", { includeAsObject: true, limit: 3 });
const recent = await solo.recentMemories({ limit: 10 });
const updated = await solo.update(saved.memory_id, "Dana likes concise weekly summaries on Friday.");
console.log(status.library.name, inboxItems.items.length, recall.hits.length, context.sections.recall.count, facts.length, recent.nodes.length, updated.memory_id);
```

The starter always addresses Community's one Memory Library. Run separate Solo
instances with separate data directories when you need hard isolation.

For MCP smoke and custom agent tools:

```ts
const session = await solo.mcpConnect({ name: "my-agent", version: "0.1.0" });
const tools = await solo.mcpListTools(session);
const result = await solo.mcpCallTool(session, "memory_context", {
  query: "weekly summaries",
  subject: "Dana",
});
console.log(tools.length, result.content?.length ?? 0);
```

## Smoke test

```bash
node --test sdks/typescript/smoke-test.mjs
```

The smoke test uses a local mock server, not a Solo daemon.

The cross-SDK matrix also syntax-checks the JavaScript runtime and direct HTTP
example:

```bash
python sdks/smoke_matrix.py
```

## Structured memory and documents

```ts
const entities = await solo.entities("Dana", { limit: 5 });
const facts = await solo.factsAbout("Dana", { includeAsObject: true, limit: 10 });
const documents = await solo.listDocuments({ limit: 5 });
const hits = await solo.searchDocuments("weekly summaries", { limit: 3 });
const inspected = documents[0]
  ? await solo.inspectDocument(documents[0].doc_id)
  : null;
```

`ingestDocument(path)` accepts a path readable by the Solo daemon process,
which is usually a local filesystem path on the same machine.

## Manual daemon check

Start Solo with:

```bash
SOLO_PASSPHRASE=change-me solo daemon --http-port 17821
```

Then run `examples/typescript/remember-recall.ts` with your preferred
TypeScript runner, or copy the import into an existing TypeScript project.

For a zero-dependency HTTP smoke without importing the SDK client:

```bash
node examples/typescript/direct-http.mjs "Dana likes weekly summaries on Friday."
```

For a Vercel AI SDK agent starter:

```bash
npm install ai @ai-sdk/openai zod
tsx examples/typescript/vercel-ai-sdk-memory.ts "Use my memory for Dana's next weekly review."
```

The AI SDK example preloads Solo context, adds a retrieval tool, and adds a
durable-fact write tool. Keep the write tool gated by your product's memory
policy so the model stores only user-approved durable facts.
