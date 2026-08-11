#!/usr/bin/env node
// solo-web MCP bridge - spawns `solo mcp-stdio` and exposes its tools as a
// minimal HTTP surface that solo-web's api/client.ts can consume.
//
// Use this when you don't have `solo http-serve` running (e.g. Solo is already
// in stdio mode via Claude Desktop and you just want to open solo-web).
//
// Usage:
//   SOLO_PASSPHRASE=xxx node scripts/mcp-bridge.mjs
//
// Env vars:
//   SOLO_BRIDGE_PORT  Port the bridge listens on (default 7436)
//   SOLO_BIN          Path to the solo binary (default "solo")
//   SOLO_PASSPHRASE   Forwarded to solo automatically via env
//   SOLO_BRIDGE_TOKEN Optional bearer token (random and printed when omitted)
//
// Then open solo-web Settings and set the Solo API URL to:
//   http://127.0.0.1:7436

import { spawn } from 'node:child_process';
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { createServer } from 'node:http';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const _rawPort = Number(process.env.SOLO_BRIDGE_PORT ?? '7436');
const PORT = Number.isInteger(_rawPort) && _rawPort > 0 && _rawPort < 65536 ? _rawPort : 7436;
const SOLO_BIN = process.env.SOLO_BIN ?? 'solo';
const MCP_CALL_TIMEOUT_MS = 30_000;
const MAX_BODY_BYTES = 1024 * 1024;

export function resolveBridgeToken(configuredToken) {
  const token = configuredToken?.trim();
  if (!token) return randomBytes(32).toString('hex');
  if (Buffer.byteLength(token, 'utf8') < 32) {
    throw new Error('SOLO_BRIDGE_TOKEN must contain at least 32 bytes');
  }
  return token;
}

export function soloProcessEnvironment(environment) {
  const childEnvironment = { ...environment };
  delete childEnvironment.SOLO_BRIDGE_TOKEN;
  return childEnvironment;
}

const BRIDGE_TOKEN = resolveBridgeToken(process.env.SOLO_BRIDGE_TOKEN);

// MCP JSON-RPC 2.0 stdio client

class McpClient {
  constructor() {
    this._proc = null;
    this._pending = new Map(); // id -> { resolve, reject }
    this._buf = '';
    this._seq = 1;
    this._dead = false; // set true on exit so new calls reject immediately
  }

  start() {
    const args = ['mcp-stdio'];

    this._proc = spawn(SOLO_BIN, args, {
      env: soloProcessEnvironment(process.env),
      stdio: ['pipe', 'pipe', 'pipe'],
    });

    this._proc.stdout.setEncoding('utf8');
    this._proc.stdout.on('data', (chunk) => this._onData(chunk));
    this._proc.stderr.on('data', (d) => process.stderr.write(`[solo] ${d}`));
    // Suppress EPIPE - emitted when solo exits while we're mid-write.
    // Without this Node throws an uncaught 'error' event and crashes the bridge.
    this._proc.stdin.on('error', (e) => {
      console.error(`[bridge] stdin error (solo may have exited): ${e.message}`);
      this._failPending(e);
    });
    this._proc.on('error', (error) => {
      console.error(`[bridge] could not start solo: ${error.message}`);
      this._failPending(error);
    });
    this._proc.on('exit', (code) => {
      console.error(`[bridge] solo process exited (code ${code})`);
      this._failPending(new Error(`solo process exited with code ${code}`));
    });

    return this._initialize();
  }

  async _initialize() {
    await this._call('initialize', {
      protocolVersion: '2024-11-05',
      capabilities: {},
      clientInfo: { name: 'solo-web-bridge', version: '1.0.0' },
    });
    // Required handshake notification - no response expected
    this._send({ jsonrpc: '2.0', method: 'notifications/initialized', params: {} });
  }

  _onData(chunk) {
    this._buf += chunk;
    const lines = this._buf.split('\n');
    this._buf = lines.pop() ?? ''; // keep any incomplete trailing fragment
    for (const line of lines) {
      if (!line.trim()) continue;
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        continue;
      }
      if (msg.id != null && this._pending.has(msg.id)) {
        const { resolve, reject, timeout } = this._pending.get(msg.id);
        this._pending.delete(msg.id);
        clearTimeout(timeout);
        if (msg.error) {
          reject(Object.assign(new Error(msg.error.message ?? 'MCP error'), { mcpCode: msg.error.code }));
        } else {
          resolve(msg.result);
        }
      }
    }
  }

  _send(obj) {
    this._proc.stdin.write(JSON.stringify(obj) + '\n');
  }

  _failPending(error) {
    this._dead = true;
    for (const { reject, timeout } of this._pending.values()) {
      clearTimeout(timeout);
      reject(error);
    }
    this._pending.clear();
  }

  _call(method, params) {
    if (this._dead) {
      return Promise.reject(new Error('solo process has exited - restart the bridge'));
    }
    return new Promise((resolve, reject) => {
      const id = this._seq++;
      const timeout = setTimeout(() => {
        this._pending.delete(id);
        reject(new Error(`MCP ${method} timed out after ${MCP_CALL_TIMEOUT_MS}ms`));
      }, MCP_CALL_TIMEOUT_MS);
      timeout.unref?.();
      this._pending.set(id, { resolve, reject, timeout });
      try {
        this._send({ jsonrpc: '2.0', id, method, params });
      } catch (error) {
        clearTimeout(timeout);
        this._pending.delete(id);
        reject(error);
      }
    });
  }

  /** Call a Solo MCP tool. Parses the text content as JSON; returns raw string if parse fails. */
  async tool(name, args = {}) {
    const result = await this._call('tools/call', { name, arguments: args });
    if (result?.isError) {
      const msg = result.content?.[0]?.text ?? 'MCP tool error';
      throw new Error(msg);
    }
    const text = result?.content?.[0]?.text ?? '[]';
    try {
      return JSON.parse(text);
    } catch {
      return text;
    }
  }
}

// Graph cache (shared across /nodes and /edges requests)

const GRAPH_CACHE_TTL_MS = 15_000;
let _graphCache = null;
let _graphCacheTs = 0;

async function getGraph(client) {
  const now = Date.now();
  if (_graphCache && now - _graphCacheTs < GRAPH_CACHE_TTL_MS) return _graphCache;
  _graphCache = await buildGraph(client);
  _graphCacheTs = now;
  return _graphCache;
}

async function buildGraph(client) {
  // Fetch episodes + clusters in parallel.
  // memory_recall requires a non-empty query - use "the" as the broadest
  // common English token. Returns up to limit=500 hits ordered by relevance.
  // memory_entities requires non-empty query too, so entity nodes are skipped
  // in MCP mode (no graph endpoint for a full entity list).
  const [recallRaw, themesRaw] = await Promise.allSettled([
    client.tool('memory_recall', { query: 'the', limit: 500 }),
    client.tool('memory_themes', { limit: 100 }),
  ]);

  const recallHits = recallRaw.status === 'fulfilled' && Array.isArray(recallRaw.value)
    ? recallRaw.value
    : [];
  const themes = themesRaw.status === 'fulfilled' && Array.isArray(themesRaw.value)
    ? themesRaw.value
    : [];

  const nodes = [];
  const edges = [];

  // Episode nodes
  for (const hit of recallHits) {
    nodes.push({
      id: `ep:${hit.memory_id}`,
      kind: 'episode',
      label: hit.content.slice(0, 80),
      preview: hit.content,
    });
  }

  // Cluster nodes
  for (const theme of themes) {
    nodes.push({
      id: `cluster:${theme.cluster_id}`,
      kind: 'cluster',
      label: theme.abstraction_text?.slice(0, 80) ?? `Cluster ${theme.cluster_id.slice(0, 8)}`,
      preview: theme.abstraction_text ?? null,
      ts_ms: theme.created_at_ms,
    });
  }

  // Note: edges (cluster->episode, triples) require O(N) extra MCP calls and
  // are omitted in bridge mode. Switch to http-serve for full graph topology.

  return { nodes, edges };
}

// HTTP helpers

const CORS = {
  'Access-Control-Allow-Origin': '*',
  'Access-Control-Allow-Headers': 'Content-Type, Authorization',
  'Access-Control-Allow-Methods': 'GET, POST, PATCH, DELETE, OPTIONS',
};

function jsonResponse(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, { 'Content-Type': 'application/json', ...CORS });
  res.end(payload);
}

function noContentResponse(res) {
  res.writeHead(204, CORS);
  res.end();
}

export function isAuthorized(authorization, expectedToken) {
  if (typeof authorization !== 'string' || !expectedToken) return false;
  const match = /^Bearer\s+(.+)$/i.exec(authorization.trim());
  if (!match) return false;
  const supplied = Buffer.from(match[1], 'utf8');
  const expected = Buffer.from(expectedToken, 'utf8');
  return supplied.length === expected.length && timingSafeEqual(supplied, expected);
}

export function readJsonBody(req, maxBytes = MAX_BODY_BYTES) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let received = 0;
    let tooLarge = false;
    req.on('data', (chunk) => {
      received += chunk.length;
      if (received > maxBytes) {
        tooLarge = true;
        chunks.length = 0;
        return;
      }
      if (!tooLarge) chunks.push(chunk);
    });
    req.on('error', reject); // client disconnect - propagates to handle()'s catch
    req.on('end', () => {
      if (tooLarge) {
        reject(Object.assign(new Error(`request body exceeds ${maxBytes} bytes`), { statusCode: 413 }));
        return;
      }
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
      } catch {
        reject(Object.assign(new Error('request body must be valid JSON'), { statusCode: 400 }));
      }
    });
  });
}

// Route handler

export async function handle(client, req, res, expectedToken = BRIDGE_TOKEN) {
  const url = new URL(req.url, `http://localhost:${PORT}`);
  const path = url.pathname;
  const method = req.method?.toUpperCase() ?? 'GET';

  if (method === 'OPTIONS') {
    res.writeHead(204, CORS);
    return res.end();
  }

  if (!isAuthorized(req.headers.authorization, expectedToken)) {
    res.setHeader('WWW-Authenticate', 'Bearer realm="solo-bridge"');
    return jsonResponse(res, 401, { error: 'valid Solo bridge bearer token required' });
  }

  try {
    // GET /health
    if (method === 'GET' && path === '/health') {
      res.writeHead(200, { 'Content-Type': 'text/plain', ...CORS });
      return res.end('ok');
    }

    // GET /v1/status - synthesised; enough for StatusStrip reachability check
    if (method === 'GET' && path === '/v1/status') {
      return jsonResponse(res, 200, {
        ok: true,
        version: '0.12.0',
        build: { version: '0.12.0', version_with_build: '0.12.0+mcp-bridge' },
        library: { name: 'Community Memory Library', ready: true },
        embedder: { name: 'mcp-bridge', version: '1.0', dim: 0, dtype: 'f32' },
        mcp: { sessions: 1 },
      });
    }

    // GET /v1/inbox
    if (method === 'GET' && path === '/v1/inbox') {
      const limit = Number(url.searchParams.get('limit') ?? 100);
      const items = await client.tool('memory_inbox', { limit });
      return jsonResponse(res, 200, { items: Array.isArray(items) ? items : [] });
    }

    // POST /v1/inbox/:id/review
    const reviewMatch = path.match(/^\/v1\/inbox\/([^/]+)\/review$/);
    if (reviewMatch && method === 'POST') {
      const body = await readJsonBody(req);
      const result = await client.tool('memory_review', {
        memory_id: decodeURIComponent(reviewMatch[1]),
        state: body.state,
        note: body.note,
      });
      _graphCache = null;
      return jsonResponse(res, 200, typeof result === 'object' ? result : { ok: true });
    }

    // GET /v1/graph/nodes
    if (method === 'GET' && path === '/v1/graph/nodes') {
      const { nodes } = await getGraph(client);
      return jsonResponse(res, 200, { nodes });
    }

    // GET /v1/graph/edges
    if (method === 'GET' && path === '/v1/graph/edges') {
      const { edges } = await getGraph(client);
      return jsonResponse(res, 200, { edges });
    }

    // GET /v1/graph/inspect/:id
    const inspectMatch = path.match(/^\/v1\/graph\/inspect\/(.+)$/);
    if (inspectMatch && method === 'GET') {
      const rawId = decodeURIComponent(inspectMatch[1]);

      // Cluster nodes have id "cluster:<uuid>" - route to memory_inspect_cluster.
      if (rawId.startsWith('cluster:')) {
        const clusterId = rawId.slice('cluster:'.length);
        const row = await client.tool('memory_inspect_cluster', { cluster_id: clusterId });
        // ClusterRecord serialises as { cluster: {...}, abstraction: { content, ... }, episodes: [...] }
        // Fall back gracefully if the abstraction pass hasn't run yet (abstraction is null).
        const text = typeof row === 'object'
          ? (row?.abstraction?.content ?? `Cluster ${clusterId.slice(0, 8)} (no abstraction yet)`)
          : String(row ?? '');
        return jsonResponse(res, 200, {
          node: {
            id: rawId,
            kind: 'cluster',
            label: text.slice(0, 80),
            preview: text,
          },
          full_text: text,
          triples_in: [],
          triples_out: [],
        });
      }

      // Episode nodes have id "ep:<uuid>" - strip prefix for memory_inspect.
      const memId = rawId.startsWith('ep:') ? rawId.slice(3) : rawId;
      const row = await client.tool('memory_inspect', { memory_id: memId });
      const content = typeof row === 'object' ? (row.content ?? '') : String(row ?? '');
      return jsonResponse(res, 200, {
        node: {
          id: `ep:${memId}`,
          kind: 'episode',
          label: content.slice(0, 80),
          preview: content,
          ts_ms: typeof row === 'object' ? row.created_at_ms : undefined,
        },
        full_text: content,
        triples_in: [],
        triples_out: [],
      });
    }

    // GET /v1/graph/neighbors/:id - not feasible over MCP without many calls
    const neighborsMatch = path.match(/^\/v1\/graph\/neighbors\/(.+)$/);
    if (neighborsMatch && method === 'GET') {
      return jsonResponse(res, 200, { nodes: [], edges: [] });
    }

    // GET /memory/themes
    if (method === 'GET' && path === '/memory/themes') {
      const limit = Number(url.searchParams.get('limit') ?? 20);
      const hits = await client.tool('memory_themes', { limit });
      return jsonResponse(res, 200, Array.isArray(hits) ? hits : []);
    }

    // GET /memory/entities?query=&limit=
    if (method === 'GET' && path === '/memory/entities') {
      const query = url.searchParams.get('query') ?? '';
      const limit = Number(url.searchParams.get('limit') ?? 8);
      if (!query.trim()) return jsonResponse(res, 200, []);
      const hits = await client.tool('memory_entities', { query, limit });
      return jsonResponse(res, 200, Array.isArray(hits) ? hits : []);
    }

    // GET /memory/contradictions?limit=
    if (method === 'GET' && path === '/memory/contradictions') {
      const limit = Number(url.searchParams.get('limit') ?? 10);
      const hits = await client.tool('memory_contradictions', { limit });
      return jsonResponse(res, 200, Array.isArray(hits) ? hits : []);
    }

    // POST /memory/contradictions/resolve
    if (method === 'POST' && path === '/memory/contradictions/resolve') {
      const body = await readJsonBody(req);
      const result = await client.tool('memory_contradiction_resolve', {
        a_id: body.a_id,
        b_id: body.b_id,
        kind: body.kind,
        status: body.status ?? 'resolved',
        resolution_note: body.resolution_note,
        winning_triple_id: body.winning_triple_id,
      });
      // Invalidate graph cache so next fetch reflects the resolution.
      _graphCache = null;
      return jsonResponse(res, 200, typeof result === 'object' ? result : { ok: true });
    }

    // PATCH /memory/:id
    const memoryMatch = path.match(/^\/memory\/([^/]+)$/);
    if (memoryMatch && method === 'PATCH') {
      const body = await readJsonBody(req);
      const result = await client.tool('memory_update', {
        memory_id: memoryMatch[1],
        content: body.content,
      });
      // Invalidate graph cache - episode label/preview may have changed.
      _graphCache = null;
      return jsonResponse(res, 200, typeof result === 'object' ? result : { ok: true });
    }

    // DELETE /memory/:id?reason=
    if (memoryMatch && method === 'DELETE') {
      await client.tool('memory_forget', {
        memory_id: memoryMatch[1],
        reason: url.searchParams.get('reason') ?? 'solo-web bridge',
      });
      _graphCache = null;
      return noContentResponse(res);
    }

    return jsonResponse(res, 404, { error: `no bridge handler for ${method} ${path}` });
  } catch (err) {
    console.error(`[bridge] ${method} ${path} ->`, err.message);
    return jsonResponse(res, err.statusCode ?? 500, { error: err.message });
  }
}

// Main

export async function startBridge() {
  console.log(`[bridge] spawning ${SOLO_BIN} mcp-stdio ...`);
  console.log(`[bridge] ephemeral bearer token (sensitive): ${BRIDGE_TOKEN}`);
  console.log('[bridge] paste this token into Solo Web Settings for the bridge connection');
  const client = new McpClient();
  try {
    await client.start();
  } catch (error) {
    client._proc?.kill();
    throw error;
  }
  console.log('[bridge] MCP handshake complete');

  const server = createServer((req, res) => {
    handle(client, req, res).catch((err) => {
      console.error('[bridge] unhandled error:', err);
      try {
        jsonResponse(res, 500, { error: 'internal bridge error' });
      } catch {
        // Response was already sent.
      }
    });
  });

  server.on('error', (error) => {
    console.error(`[bridge] HTTP server failed: ${error.message}`);
    client._proc?.kill();
    process.exitCode = 1;
  });

  server.listen(PORT, '127.0.0.1', () => {
    console.log(`[bridge] listening -> http://127.0.0.1:${PORT}`);
    console.log('[bridge] open Solo Web Settings and select the Developer bridge');
  });

  function shutdown() {
    console.log('\n[bridge] shutting down');
    server.close();
    client._proc?.kill();
    process.exit(0);
  }

  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);
}

const entryPoint = process.argv[1];
if (entryPoint && import.meta.url === pathToFileURL(resolve(entryPoint)).href) {
  startBridge().catch((error) => {
    console.error(`[bridge] startup failed: ${error.message}`);
    process.exitCode = 1;
  });
}
