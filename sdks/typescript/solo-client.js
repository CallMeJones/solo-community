export class SoloHttpError extends Error {
  constructor(message, status, body) {
    super(message);
    this.name = "SoloHttpError";
    this.status = status;
    this.body = body;
  }
}

export class SoloMcpError extends Error {
  constructor(message, error) {
    super(message);
    this.name = "SoloMcpError";
    this.error = error;
  }
}

export class SoloClient {
  #rpcId = 0;

  constructor(options = {}) {
    this.baseUrl = (options.baseUrl ?? "http://127.0.0.1:17821").replace(/\/+$/, "");
    this.bearerToken = options.bearerToken;
    this.fetchImpl = options.fetchImpl ?? globalThis.fetch;
    if (!this.fetchImpl) {
      throw new Error("SoloClient needs fetch; use Node 18+ or pass fetchImpl.");
    }
  }

  status() {
    return this.#json("GET", "/v1/status");
  }

  remember(content, options = {}) {
    const body = { content };
    if (options.sourceType !== undefined) body.source_type = options.sourceType;
    if (options.sourceId !== undefined) body.source_id = options.sourceId;
    if (options.salience !== undefined) body.salience = options.salience;
    return this.#json("POST", "/memory", body);
  }

  rememberInbox(content, options = {}) {
    return this.remember(content, {
      ...options,
      sourceType: options.sourceType ?? "solo_desktop.inbox",
    });
  }

  memoryInbox(options = {}) {
    const query = queryString({
      limit: options.limit,
    });
    return this.#json("GET", `/v1/inbox${query}`);
  }

  reviewMemory(memoryId, options = {}) {
    const body = {};
    if (options.state !== undefined) body.state = options.state;
    if (options.note !== undefined) body.note = options.note;
    return this.#json(
      "POST",
      `/v1/inbox/${encodeURIComponent(memoryId)}/review`,
      body,
    );
  }

  recall(query, options = {}) {
    const body = { query };
    if (options.limit !== undefined) body.limit = options.limit;
    return this.#json("POST", "/memory/search", body);
  }

  context(query, options = {}) {
    const body = { query };
    if (options.subject !== undefined) body.subject = options.subject;
    if (options.windowDays !== undefined) body.window_days = options.windowDays;
    if (options.limit !== undefined) body.limit = options.limit;
    return this.#json("POST", "/memory/context", body);
  }

  factsAbout(subject, options = {}) {
    const query = queryString({
      subject,
      predicate: options.predicate,
      since_ms: options.sinceMs,
      until_ms: options.untilMs,
      include_as_object: options.includeAsObject,
      limit: options.limit,
    });
    return this.#json("GET", `/memory/facts_about${query}`);
  }

  entities(query, options = {}) {
    const params = queryString({ query, limit: options.limit });
    return this.#json("GET", `/memory/entities${params}`);
  }

  inspect(memoryId) {
    return this.#json(
      "GET",
      `/memory/${encodeURIComponent(memoryId)}`,
      undefined,
    );
  }

  update(memoryId, content) {
    return this.#json(
      "PATCH",
      `/memory/${encodeURIComponent(memoryId)}`,
      { content },
    );
  }

  forget(memoryId, options = {}) {
    const reason = options.reason ? `?reason=${encodeURIComponent(options.reason)}` : "";
    return this.#json(
      "DELETE",
      `/memory/${encodeURIComponent(memoryId)}${reason}`,
      undefined,
    );
  }

  listDocuments(options = {}) {
    const query = queryString({
      limit: options.limit,
      offset: options.offset,
      include_forgotten: options.includeForgotten,
    });
    return this.#json("GET", `/memory/documents${query}`);
  }

  ingestDocument(path) {
    return this.#json("POST", "/memory/documents", { path });
  }

  searchDocuments(query, options = {}) {
    const body = { query };
    if (options.limit !== undefined) body.limit = options.limit;
    return this.#json("POST", "/memory/documents/search", body);
  }

  inspectDocument(docId) {
    return this.#json(
      "GET",
      `/memory/documents/${encodeURIComponent(docId)}`,
      undefined,
    );
  }

  forgetDocument(docId) {
    return this.#json(
      "DELETE",
      `/memory/documents/${encodeURIComponent(docId)}`,
      undefined,
    );
  }

  recentMemories(options = {}) {
    const query = queryString({
      kind: "episode",
      limit: options.limit ?? 100,
      cursor: options.cursor,
      since_ms: options.sinceMs,
      until_ms: options.untilMs,
    });
    return this.#json("GET", `/v1/graph/nodes${query}`);
  }

  async mcpConnect(clientInfo = { name: "solo-sdk", version: "0.0.0" }) {
    const initialized = await this.mcpInitialize(clientInfo);
    if (!initialized.sessionId) {
      throw new Error("Solo MCP initialize did not return an Mcp-Session-Id header.");
    }
    await this.mcpNotifyInitialized(initialized);
    return initialized;
  }

  async mcpInitialize(clientInfo = { name: "solo-sdk", version: "0.0.0" }) {
    const response = await this.#request("POST", "/mcp", {
      jsonrpc: "2.0",
      id: this.#nextRpcId(),
      method: "initialize",
      params: {
        protocolVersion: "2025-03-26",
        capabilities: {},
        clientInfo,
      },
    });
    const raw = await this.#parseJson(response);
    const result = this.#jsonRpcResult(raw);
    return {
      sessionId: response.headers.get("mcp-session-id"),
      result,
      raw,
    };
  }

  async mcpNotifyInitialized(session) {
    const sessionId = this.#sessionId(session);
    const response = await this.#request(
      "POST",
      "/mcp",
      { jsonrpc: "2.0", method: "notifications/initialized", params: {} },
      { "Mcp-Session-Id": sessionId },
    );
    await response.text();
  }

  async mcpListTools(session) {
    const sessionId = this.#sessionId(session);
    const raw = await this.#json("POST", "/mcp", {
      jsonrpc: "2.0",
      id: this.#nextRpcId(),
      method: "tools/list",
    }, { "Mcp-Session-Id": sessionId });
    const result = this.#jsonRpcResult(raw);
    return result?.tools ?? [];
  }

  async mcpCallTool(session, name, args = {}) {
    const sessionId = this.#sessionId(session);
    const raw = await this.#json("POST", "/mcp", {
      jsonrpc: "2.0",
      id: this.#nextRpcId(),
      method: "tools/call",
      params: {
        name,
        arguments: args ?? {},
      },
    }, { "Mcp-Session-Id": sessionId });
    return this.#jsonRpcResult(raw);
  }

  async #json(method, path, body, headers = {}) {
    const response = await this.#request(method, path, body, headers);
    return this.#parseJson(response);
  }

  async #request(method, path, body, extraHeaders = {}) {
    const headers = {
      Accept: "application/json",
      ...extraHeaders,
    };
    if (body !== undefined) headers["Content-Type"] = "application/json";
    if (this.bearerToken) headers.Authorization = `Bearer ${this.bearerToken}`;

    let response;
    try {
      response = await this.fetchImpl(`${this.baseUrl}${path}`, {
        method,
        headers,
        body: body === undefined ? undefined : JSON.stringify(body),
      });
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error);
      throw new Error(
        `Could not reach Solo daemon at ${this.baseUrl}. Start Solo Desktop/tray or run solo daemon. ${detail}`,
      );
    }

    if (!response.ok) {
      const text = await response.text();
      const parsed = tryJson(text);
      const message = parsed?.error ?? (text || `Solo HTTP ${response.status}`);
      throw new SoloHttpError(message, response.status, parsed ?? text);
    }
    return response;
  }

  async #parseJson(response) {
    const text = await response.text();
    return text ? JSON.parse(text) : {};
  }

  #nextRpcId() {
    this.#rpcId += 1;
    return this.#rpcId;
  }

  #sessionId(session) {
    if (typeof session === "string") {
      if (!session) throw new Error("Solo MCP session id is required.");
      return session;
    }
    if (!session?.sessionId) throw new Error("Solo MCP session id is required.");
    return session.sessionId;
  }

  #jsonRpcResult(raw) {
    if (raw?.error) {
      const message = raw.error.message ?? raw.error.error ?? "Solo MCP JSON-RPC error";
      throw new SoloMcpError(message, raw.error);
    }
    return raw?.result;
  }
}

function tryJson(text) {
  try {
    return text ? JSON.parse(text) : undefined;
  } catch {
    return undefined;
  }
}

function queryString(params) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== null) query.set(key, String(value));
  }
  const text = query.toString();
  return text ? `?${text}` : "";
}
