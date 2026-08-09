import { strict as assert } from "node:assert";
import { createServer } from "node:http";
import test from "node:test";
import { SoloClient } from "./solo-client.js";

test("SoloClient sends memory and MCP requests without a daemon", async (t) => {
  const seen = [];
  const server = createServer(async (req, res) => {
    const body = await readJson(req);
    seen.push({ method: req.method, url: req.url, headers: req.headers, body });

    if (req.url === "/v1/status" && req.method === "GET") {
      return sendJson(res, 200, {
        ok: true,
        version: "0.0.0",
        build: {},
        library: { name: "Community Memory Library", ready: true },
        embedder: { name: "mock", version: "0", dim: 384, dtype: "f32" },
        mcp: { sessions: 0 },
        steward: {},
        runtime: { pid: 1, platform: "test", data_dir: "/tmp/solo" },
      });
    }
    if (req.url === "/memory") {
      return sendJson(res, 200, { memory_id: "mem_1" });
    }
    if (req.url === "/v1/inbox?limit=10" && req.method === "GET") {
      return sendJson(res, 200, {
        items: [{
          memory_id: "mem_1",
          label: "Remember this",
          preview: "Remember this",
          ts_ms: 10,
          source_type: "solo_desktop.inbox",
          salience: 0.6,
          status: "active",
          review_state: null,
          reviewed_at_ms: null,
          review_note: null,
        }],
      });
    }
    if (req.url === "/v1/inbox/mem_1/review" && req.method === "POST") {
      return sendJson(res, 200, { memory_id: "mem_1", state: body.state, reviewed_at_ms: 11 });
    }
    if (req.url === "/memory/search") {
      return sendJson(res, 200, {
        hits: [{ rowid: 1, memory_id: "mem_1", cos_distance: 0, content: body.query, source_type: "sdk_smoke", tier: "hot" }],
        index_len: 1,
        candidates_considered: 1,
      });
    }
    if (req.url === "/memory/context") {
      return sendJson(res, 200, {
        query: body.query,
        subject: body.subject,
        resolved_subject: body.subject,
        sections: {
          recall: { status: "ok", count: 0, warning: null },
          themes: { status: "ok", count: 0, warning: null },
          entities: { status: "ok", count: 0, warning: null },
          facts: { status: "ok", count: 0, warning: null },
          contradictions: { status: "ok", count: 0, warning: null },
          graph: { status: "ok", count: 1, warning: null },
        },
        recall: { hits: [], index_len: 0, candidates_considered: 0 },
        themes: [],
        entities: [],
        facts: [],
        contradictions: [],
        graph: {
          seed_entities: ["Solo"],
          aliases: [],
          relationship_facts: [{
            edge_id: "edge_1",
            subject_id: "Solo",
            predicate: "uses",
            object_id: "Ollama",
            object_kind: "entity",
            confidence: 0.94,
            strength: 0.94,
            evidence_count: 1,
            valid_from_ms: 11,
            valid_to_ms: null,
            cluster_id: "cluster_1",
            source_episode_id: 1,
            memory_id: "mem_1",
            evidence_preview: "Solo uses Ollama.",
          }],
          literal_facts: [],
          review_warnings: [],
        },
      });
    }
    if (req.url === "/memory/facts_about?subject=Maya&include_as_object=true&limit=2") {
      return sendJson(res, 200, [{
        triple_id: "triple_1",
        subject_id: "Maya",
        predicate: "prefers",
        object_id: "concise notes",
        object_kind: "text",
        valid_from_ms: 1,
        valid_to_ms: null,
        confidence: 0.9,
        cluster_id: null,
      }]);
    }
    if (req.url === "/memory/entities?query=May&limit=3") {
      return sendJson(res, 200, [{
        entity_id: "Maya",
        subject_count: 1,
        object_count: 0,
        fact_count: 1,
        predicates: ["prefers"],
        match_score: 3,
      }]);
    }
    if (req.url === "/memory/mem_1" && req.method === "GET") {
      return sendJson(res, 200, { memory_id: "mem_1", content: "Remember this", status: "active" });
    }
    if (req.url === "/memory/mem_1" && req.method === "PATCH") {
      return sendJson(res, 200, { memory_id: "mem_1", content: body.content, updated: true });
    }
    if (req.url === "/memory/mem_1?reason=stale%20test" && req.method === "DELETE") {
      return sendJson(res, 200, { memory_id: "mem_1", status: "forgotten" });
    }
    if (req.url === "/memory/documents?limit=2&offset=1&include_forgotten=true" && req.method === "GET") {
      return sendJson(res, 200, [{
        doc_id: "doc_1",
        title: "Solo notes",
        source: "solo.md",
        mime_type: "text/markdown",
        ingested_at_ms: 10,
        chunk_count: 2,
        status: "active",
      }]);
    }
    if (req.url === "/memory/documents" && req.method === "POST") {
      return sendJson(res, 200, { doc_id: "doc_1", chunks_persisted: 2, bytes_ingested: 123, deduped: false });
    }
    if (req.url === "/memory/documents/search" && req.method === "POST") {
      return sendJson(res, 200, [{
        chunk_id: "chunk_1",
        doc_id: "doc_1",
        doc_title: "Solo notes",
        doc_source: "solo.md",
        doc_mime_type: "text/markdown",
        chunk_index: 0,
        content: body.query,
        cos_distance: 0.1,
        start_offset: 0,
        end_offset: 12,
      }]);
    }
    if (req.url === "/memory/documents/doc_1" && req.method === "GET") {
      return sendJson(res, 200, {
        document: {
          doc_id: "doc_1",
          title: "Solo notes",
          source: "solo.md",
          mime_type: "text/markdown",
          ingested_at_ms: 10,
          modified_at_ms: null,
          status: "active",
          chunk_count: 2,
          content_hash: "hash",
          byte_size: 123,
        },
        chunks: [{ chunk_id: "chunk_1", chunk_index: 0, content_preview: "hello", token_count: 1 }],
      });
    }
    if (req.url === "/memory/documents/doc_1" && req.method === "DELETE") {
      return sendJson(res, 200, { doc_id: "doc_1", chunks_tombstoned: 2 });
    }
    if (req.url === "/v1/graph/nodes?kind=episode&limit=5" && req.method === "GET") {
      return sendJson(res, 200, {
        nodes: [{
          id: "episode:mem_1",
          kind: "episode",
          label: "Remember this",
          ts_ms: 10,
          preview: "Remember this",
          source_type: "sdk_smoke",
          salience: 0.8,
          status: "active",
        }],
        next_cursor: null,
      });
    }
    if (req.url === "/mcp" && body.method === "initialize") {
      res.setHeader("Mcp-Session-Id", "session_1");
      return sendJson(res, 200, { jsonrpc: "2.0", id: body.id, result: { serverInfo: { name: "solo" } } });
    }
    if (req.url === "/mcp" && body.method === "notifications/initialized") {
      assert.equal(req.headers["mcp-session-id"], "session_1");
      return sendJson(res, 202, {});
    }
    if (req.url === "/mcp" && body.method === "tools/list") {
      assert.equal(req.headers["mcp-session-id"], "session_1");
      return sendJson(res, 200, { jsonrpc: "2.0", id: body.id, result: { tools: [{ name: "memory_remember" }] } });
    }
    if (req.url === "/mcp" && body.method === "tools/call") {
      assert.equal(req.headers["mcp-session-id"], "session_1");
      return sendJson(res, 200, {
        jsonrpc: "2.0",
        id: body.id,
        result: { content: [{ type: "text", text: JSON.stringify({ ok: true, query: body.params.arguments.query }) }] },
      });
    }
    sendJson(res, 404, { error: "not found" });
  });

  await listen(server);
  t.after(() => server.close());

  const { port } = server.address();
  const client = new SoloClient({
    baseUrl: `http://127.0.0.1:${port}`,
    bearerToken: "test-token",
  });

  assert.equal((await client.status()).library.name, "Community Memory Library");
  assert.deepEqual(await client.remember("Remember this", { sourceType: "sdk_smoke", sourceId: "ts", salience: 0.8 }), { memory_id: "mem_1" });
  assert.deepEqual(await client.rememberInbox("Review this later", { salience: 0.6 }), { memory_id: "mem_1" });
  assert.equal((await client.memoryInbox({ limit: 10 })).items[0].memory_id, "mem_1");
  assert.deepEqual(await client.reviewMemory("mem_1", { state: "approved", note: "looks right" }), {
    memory_id: "mem_1",
    state: "approved",
    reviewed_at_ms: 11,
  });
  assert.equal((await client.recall("Remember this", { limit: 2 })).hits[0].memory_id, "mem_1");
  const context = await client.context("Remember this", { subject: "Solo", limit: 1 });
  assert.equal(context.query, "Remember this");
  assert.equal(context.sections.recall.status, "ok");
  assert.equal(context.graph.relationship_facts[0].object_id, "Ollama");
  assert.equal((await client.factsAbout("Maya", { includeAsObject: true, limit: 2 }))[0].predicate, "prefers");
  assert.equal((await client.entities("May", { limit: 3 }))[0].entity_id, "Maya");
  assert.equal((await client.inspect("mem_1")).content, "Remember this");
  assert.equal((await client.update("mem_1", "Updated memory")).content, "Updated memory");
  assert.equal((await client.forget("mem_1", { reason: "stale test" })).status, "forgotten");
  assert.equal((await client.listDocuments({ limit: 2, offset: 1, includeForgotten: true }))[0].doc_id, "doc_1");
  assert.equal((await client.ingestDocument("C:\\notes\\solo.md")).chunks_persisted, 2);
  assert.equal((await client.searchDocuments("Solo notes", { limit: 4 }))[0].chunk_id, "chunk_1");
  assert.equal((await client.inspectDocument("doc_1")).chunks[0].content_preview, "hello");
  assert.equal((await client.forgetDocument("doc_1")).chunks_tombstoned, 2);
  assert.equal((await client.recentMemories({ limit: 5 })).nodes[0].id, "episode:mem_1");

  const initialized = await client.mcpConnect({ name: "smoke", version: "0" });
  assert.equal(initialized.sessionId, "session_1");
  assert.equal((await client.mcpListTools(initialized))[0].name, "memory_remember");
  const toolResult = await client.mcpCallTool(initialized, "memory_context", { query: "Remember this" });
  assert.equal(JSON.parse(toolResult.content[0].text).query, "Remember this");

  for (const request of seen) {
    assert.equal(request.headers.authorization, "Bearer test-token");
    assert.equal(request.headers["x-solo-tenant"], undefined);
  }
  assert.deepEqual(findRequest(seen, "POST", "/memory", (body) => body.content === "Remember this").body, {
    content: "Remember this",
    source_type: "sdk_smoke",
    source_id: "ts",
    salience: 0.8,
  });
  assert.deepEqual(findRequest(seen, "POST", "/memory", (body) => body.content === "Review this later").body, {
    content: "Review this later",
    source_type: "solo_desktop.inbox",
    salience: 0.6,
  });
  assert.deepEqual(findRequest(seen, "POST", "/v1/inbox/mem_1/review").body, {
    state: "approved",
    note: "looks right",
  });
  assert.deepEqual(findRequest(seen, "POST", "/memory/search").body, { query: "Remember this", limit: 2 });
  assert.deepEqual(findRequest(seen, "POST", "/memory/context").body, { query: "Remember this", subject: "Solo", limit: 1 });
  assert.deepEqual(findRequest(seen, "POST", "/memory/documents").body, { path: "C:\\notes\\solo.md" });
  assert.deepEqual(findRequest(seen, "POST", "/memory/documents/search").body, { query: "Solo notes", limit: 4 });
  assert.deepEqual(
    seen.find((request) => request.body?.method === "notifications/initialized").body,
    { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
  );
  await assert.rejects(() => client.mcpListTools({ sessionId: null }), /session id is required/);
});

function listen(server) {
  return new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
}

function readJson(req) {
  return new Promise((resolve) => {
    let data = "";
    req.setEncoding("utf8");
    req.on("data", (chunk) => {
      data += chunk;
    });
    req.on("end", () => {
      resolve(data ? JSON.parse(data) : {});
    });
  });
}

function sendJson(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(payload) });
  res.end(payload);
}

function findRequest(seen, method, url, bodyPredicate = () => true) {
  const request = seen.find((candidate) => (
    candidate.method === method
    && candidate.url === url
    && bodyPredicate(candidate.body ?? {})
  ));
  assert.ok(request, `missing ${method} ${url}`);
  return request;
}
