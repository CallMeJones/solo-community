// Thin fetch wrapper around Solo's /v1/graph/* routes (v0.10.0).
// Settings (URL + bearer) come from settingsStore — user-editable via
// the settings dialog (components/SettingsDialog.tsx).

import { DEFAULT_SOLO_API_URL, MCP_BRIDGE_URL } from '../config/defaults';
import { useSettingsStore } from '../store/settingsStore';
import type {
  AssetLifecycleSummary,
  ContradictionHit,
  ContradictionResolution,
  ConsolidationReport,
  DerivedRepairReport,
  DerivedRepairRequest,
  DocumentUploadCommitResponse,
  DocumentUploadAbortResponse,
  DocumentUploadPrepareResponse,
  DocumentUploadStatusResponse,
  DocumentLifecycleSummary,
  EntityHit,
  FactHit,
  ForgetAssetReport,
  ForgetDocumentReport,
  GraphResponse,
  IngestReport,
  InspectResponse,
  LogsResponse,
  MemoryInboxItem,
  MemoryInboxResponse,
  MemoryQualityAuditReport,
  MemoryQualityReviewItem,
  MemoryQualityReviewUpdateRequest,
  MemoryQualityReviewsResponse,
  MemoryReviewReport,
  MemoryReviewRequestState,
  MemoryUpdateResult,
  NativeImportResponse,
  ProjectDecisionAddResponse,
  ProjectDecisionSearchResponse,
  ProjectDescriptor,
  ProjectFactsResponse,
  ProjectPolicyClient,
  ProjectPolicyResponse,
  RememberResponse,
  StagedDocumentIngestResponse,
  TriplesExtractReport,
} from './types';

export function getApiUrl(): string {
  return useSettingsStore.getState().apiUrl.replace(/\/$/, '');
}

/** Authorization bearer token. Read from the settings store at call time. */
function getBearerToken(): string | null {
  const token = useSettingsStore.getState().bearerToken.trim();
  return token.length > 0 ? token : null;
}

function setHeader(headers: Record<string, string>, name: string, value: string): void {
  const existing = Object.keys(headers).find((key) => key.toLowerCase() === name.toLowerCase());
  if (existing && existing !== name) delete headers[existing];
  headers[name] = value;
}

export interface RequestOptions {
  signal?: AbortSignal;
  /**
   * Internal operation snapshot. Long-running workflows pass one immutable
   * connection through every phase so a settings edit cannot split an upload
   * across two Solo instances or leak a bearer to the wrong origin.
   */
  connection?: ApiConnectionSnapshot;
}

export interface ApiConnectionSnapshot {
  readonly apiUrl: string;
  readonly bearerToken: string | null;
}

export function captureApiConnection(): ApiConnectionSnapshot {
  return Object.freeze({
    apiUrl: getApiUrl(),
    bearerToken: getBearerToken(),
  });
}

function requestConnection(opts: RequestOptions): ApiConnectionSnapshot {
  return opts.connection ?? captureApiConnection();
}

interface JsonRequestOptions extends RequestOptions {
  method?: string;
  body?: unknown;
}

const REQUIRED_MCP_TOOLS = ['memory_context', 'memory_inbox', 'memory_review'] as const;

export interface McpToolsProbeReport {
  sessionId: string;
  protocolVersion: string;
  serverName: string;
  serverVersion: string;
  toolCount: number;
  toolNames: string[];
  missingRequiredTools: string[];
  readOnlyCall: {
    toolName: 'memory_context';
    status: 'passed' | 'skipped';
    detail: string;
    contentItems?: number;
  };
}

export interface BackupRequest {
  to: string;
  force?: boolean;
}

export interface BackupResponse {
  path: string;
  elapsed_ms: number;
}

export interface OllamaEmbedderSwitchResponse {
  changed: boolean;
  config_path: string;
  previous: {
    name: string;
    version: string;
    dim: number;
    dtype: string;
  };
  next: {
    name: string;
    version: string;
    dim: number;
    dtype: string;
  };
  restart_required: boolean;
  reembed_required: boolean;
  reembed_command: string;
  environment_commands: string[];
  next_steps: string[];
  note: string;
}

export type StewardLlmMode = 'none' | 'anthropic' | 'openai' | 'ollama';

export interface StewardLlmSwitchRequest {
  mode: StewardLlmMode;
  model?: string;
  base_url?: string;
  api_key_env?: string;
}

export interface LlmSettingsSummary {
  mode: string;
  provider: string | null;
  model: string | null;
  base_url: string | null;
  api_key_env: string | null;
}

export interface StewardLlmSwitchResponse {
  changed: boolean;
  config_path: string;
  previous: LlmSettingsSummary;
  next: LlmSettingsSummary;
  restart_required: boolean;
  environment_commands: string[];
  next_steps: string[];
  note: string;
}

export interface RuntimeRestartResponse {
  accepted: boolean;
  restart_expected: boolean;
  note: string;
}

export interface StewardCadenceSettings {
  trigger_interval_secs: number;
  trigger_episode_count: number;
  consolidate_interval_secs: number;
  cluster_timeout_secs: number;
  cluster_min_size: number;
  cluster_cosine_threshold: number;
}

export type StewardCadenceSettingsRequest = Partial<StewardCadenceSettings>;

export interface StewardCadenceSettingsResponse {
  changed: boolean;
  config_path: string;
  previous: StewardCadenceSettings;
  next: StewardCadenceSettings;
  restart_required: boolean;
  note: string;
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }
  return String(error);
}

export function isAbortError(error: unknown): boolean {
  return (
    typeof error === 'object' &&
    error !== null &&
    'name' in error &&
    (error as { name?: unknown }).name === 'AbortError'
  );
}

export async function jsonRequest<T>(path: string, opts: JsonRequestOptions = {}): Promise<T> {
  const connection = requestConnection(opts);
  const apiUrl = connection.apiUrl;
  const method = opts.method ?? 'GET';
  const url = `${apiUrl}${path}`;
  const bearer = connection.bearerToken;
  const headers: Record<string, string> = {
    Accept: 'application/json',
  };
  if (opts.body !== undefined) {
    headers['Content-Type'] = 'application/json';
  }
  if (bearer) {
    headers.Authorization = `Bearer ${bearer}`;
  }
  let res: Response;
  try {
    res = await fetch(url, {
      method,
      headers,
      signal: opts.signal,
      ...(opts.body !== undefined ? { body: JSON.stringify(opts.body) } : {}),
    });
  } catch (err) {
    if (isAbortError(err)) throw err;
    throw new Error(
      `Failed to reach Solo API at ${apiUrl} (${method} ${path}): ${errorMessage(err)}. ${soloConnectionHint(apiUrl)}`,
    );
  }
  if (!res.ok) {
    const detail = await readErrorDetail(res);
    const status = `${res.status}${res.statusText ? ` ${res.statusText}` : ''}`;
    throw new Error(
      detail
        ? `Solo API ${method} ${path} failed (${status}): ${detail}`
        : `Solo API ${method} ${path} failed (${status})`,
    );
  }
  if (res.status === 204) {
    return undefined as T;
  }
  return (await res.json()) as T;
}

export async function jsonFetch<T>(path: string, opts: RequestOptions = {}): Promise<T> {
  return jsonRequest<T>(path, opts);
}

export async function probeMcpTools(opts: RequestOptions = {}): Promise<McpToolsProbeReport> {
  const connection = requestConnection(opts);
  const stableOpts = { ...opts, connection };
  const apiUrl = connection.apiUrl;
  const mcpUrl = `${apiUrl}/mcp`;
  const init = await mcpJsonRpcRequest(
    mcpUrl,
    {
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: {
        protocolVersion: '2025-03-26',
        capabilities: {},
        clientInfo: { name: 'solo-web-connections', version: '0.0.0' },
      },
    },
    stableOpts,
  );
  const sessionId = init.response.headers.get('Mcp-Session-Id');
  if (!sessionId) {
    throw new Error('Solo MCP initialize did not return Mcp-Session-Id');
  }

  let toolsJson: unknown = null;
  let readOnlyCall: McpToolsProbeReport['readOnlyCall'] = {
    toolName: 'memory_context',
    status: 'skipped',
    detail: 'tools/list has not completed',
  };
  try {
    await mcpJsonRpcRequest(
      mcpUrl,
      {
        jsonrpc: '2.0',
        method: 'notifications/initialized',
        params: {},
      },
      stableOpts,
      sessionId,
      false,
    );

    const tools = await mcpJsonRpcRequest(
      mcpUrl,
      {
        jsonrpc: '2.0',
        id: 2,
        method: 'tools/list',
      },
      stableOpts,
      sessionId,
    );
    toolsJson = tools.json;
    const toolNames = parseMcpToolNames(toolsJson);
    readOnlyCall = await probeReadOnlyMemoryContext(mcpUrl, stableOpts, sessionId, toolNames);
  } finally {
    await closeMcpSession(mcpUrl, stableOpts, sessionId).catch(() => undefined);
  }

  const toolNames = parseMcpToolNames(toolsJson);
  const missingRequiredTools = REQUIRED_MCP_TOOLS.filter((tool) => !toolNames.includes(tool));
  return {
    sessionId,
    protocolVersion:
      readString(init.json, ['result', 'protocolVersion']) ??
      readString(init.json, ['result', 'protocol_version']) ??
      'unknown',
    serverName: readString(init.json, ['result', 'serverInfo', 'name']) ?? 'solo',
    serverVersion: readString(init.json, ['result', 'serverInfo', 'version']) ?? 'unknown',
    toolCount: toolNames.length,
    toolNames,
    missingRequiredTools,
    readOnlyCall,
  };
}

async function probeReadOnlyMemoryContext(
  mcpUrl: string,
  opts: RequestOptions,
  sessionId: string,
  toolNames: string[],
): Promise<McpToolsProbeReport['readOnlyCall']> {
  if (!toolNames.includes('memory_context')) {
    return {
      toolName: 'memory_context',
      status: 'skipped',
      detail: 'memory_context is missing from tools/list',
    };
  }

  const context = await mcpJsonRpcRequest(
    mcpUrl,
    {
      jsonrpc: '2.0',
      id: 3,
      method: 'tools/call',
      params: {
        name: 'memory_context',
        arguments: {
          query: 'Solo MCP readiness check',
          subject: 'Solo',
          limit: 1,
        },
      },
    },
    opts,
    sessionId,
  );
  const jsonRpcError = readJsonRpcError(context.json);
  if (jsonRpcError) {
    throw new Error(`memory_context JSON-RPC error: ${jsonRpcError}`);
  }
  const contentItems = readArrayLength(context.json, ['result', 'content']);
  if (readBoolean(context.json, ['result', 'isError'])) {
    throw new Error('memory_context returned isError=true');
  }
  if (contentItems < 1) {
    throw new Error('memory_context returned no content');
  }
  return {
    toolName: 'memory_context',
    status: 'passed',
    detail: `returned ${contentItems} content item${contentItems === 1 ? '' : 's'}`,
    contentItems,
  };
}

async function readErrorDetail(res: Response): Promise<string | null> {
  const raw = await res.text().catch(() => '');
  const trimmed = raw.trim();
  if (!trimmed) return null;

  const contentType = res.headers.get('content-type') ?? '';
  if (contentType.includes('json') || trimmed.startsWith('{') || trimmed.startsWith('[')) {
    try {
      const parsed = JSON.parse(trimmed) as unknown;
      return truncateErrorDetail(extractJsonErrorDetail(parsed) ?? JSON.stringify(parsed));
    } catch {
      return truncateErrorDetail(trimmed);
    }
  }

  return truncateErrorDetail(trimmed);
}

function extractJsonErrorDetail(value: unknown): string | null {
  if (typeof value === 'string') return value;
  if (!value || typeof value !== 'object') return null;

  const record = value as Record<string, unknown>;
  for (const key of ['error', 'message', 'detail', 'reason']) {
    const detail = record[key];
    if (typeof detail === 'string' && detail.trim()) return detail;
    if (detail && typeof detail === 'object') {
      const nested = extractJsonErrorDetail(detail);
      if (nested) return nested;
    }
  }
  return null;
}

async function mcpJsonRpcRequest(
  mcpUrl: string,
  body: unknown,
  opts: RequestOptions,
  sessionId?: string,
  expectJson = true,
): Promise<{ response: Response; json: unknown }> {
  const connection = requestConnection(opts);
  const bearer = connection.bearerToken;
  const headers: Record<string, string> = {
    Accept: 'application/json',
    'Content-Type': 'application/json',
  };
  if (sessionId) {
    headers['Mcp-Session-Id'] = sessionId;
  }
  if (bearer) {
    headers.Authorization = `Bearer ${bearer}`;
  }
  let response: Response;
  try {
    response = await fetch(mcpUrl, {
      method: 'POST',
      headers,
      signal: opts.signal,
      body: JSON.stringify(body),
    });
  } catch (err) {
    if (isAbortError(err)) throw err;
    throw new Error(`Failed to reach Solo MCP at ${mcpUrl}: ${errorMessage(err)}`);
  }
  if (!response.ok) {
    const detail = await readErrorDetail(response);
    const status = `${response.status}${response.statusText ? ` ${response.statusText}` : ''}`;
    throw new Error(
      detail
        ? `Solo MCP request failed (${status}): ${detail}`
        : `Solo MCP request failed (${status})`,
    );
  }
  if (!expectJson || response.status === 204) {
    return { response, json: null };
  }
  const json = (await response.json()) as unknown;
  return { response, json };
}

async function closeMcpSession(
  mcpUrl: string,
  opts: RequestOptions,
  sessionId: string,
): Promise<void> {
  const connection = requestConnection(opts);
  const bearer = connection.bearerToken;
  const headers: Record<string, string> = {
    Accept: 'application/json',
    'Mcp-Session-Id': sessionId,
  };
  if (bearer) {
    headers.Authorization = `Bearer ${bearer}`;
  }
  const response = await fetch(mcpUrl, {
    method: 'DELETE',
    headers,
    signal: opts.signal,
  });
  if (!response.ok) {
    const detail = await readErrorDetail(response);
    const status = `${response.status}${response.statusText ? ` ${response.statusText}` : ''}`;
    throw new Error(
      detail
        ? `Solo MCP session cleanup failed (${status}): ${detail}`
        : `Solo MCP session cleanup failed (${status})`,
    );
  }
}

async function callMcpTool<T>(
  toolName: string,
  args: Record<string, unknown>,
  opts: RequestOptions,
): Promise<T> {
  const connection = requestConnection(opts);
  const stableOpts = { ...opts, connection };
  const mcpUrl = `${connection.apiUrl}/mcp`;
  const init = await mcpJsonRpcRequest(
    mcpUrl,
    {
      jsonrpc: '2.0',
      id: 1,
      method: 'initialize',
      params: {
        protocolVersion: '2025-03-26',
        capabilities: {},
        clientInfo: { name: 'solo-web-lifecycle', version: '0.0.0' },
      },
    },
    stableOpts,
  );
  const sessionId = init.response.headers.get('Mcp-Session-Id');
  if (!sessionId) throw new Error('Solo MCP initialize did not return Mcp-Session-Id');

  try {
    await mcpJsonRpcRequest(
      mcpUrl,
      { jsonrpc: '2.0', method: 'notifications/initialized', params: {} },
      stableOpts,
      sessionId,
      false,
    );
    const response = await mcpJsonRpcRequest(
      mcpUrl,
      {
        jsonrpc: '2.0',
        id: 2,
        method: 'tools/call',
        params: { name: toolName, arguments: args },
      },
      stableOpts,
      sessionId,
    );
    const jsonRpcError = readJsonRpcError(response.json);
    if (jsonRpcError) throw new Error(`${toolName} JSON-RPC error: ${jsonRpcError}`);
    if (readBoolean(response.json, ['result', 'isError'])) {
      throw new Error(`${toolName} returned isError=true`);
    }
    const structured = readValue(response.json, ['result', 'structuredContent']);
    if (structured && typeof structured === 'object') return structured as T;

    const text = readValue(response.json, ['result', 'content', '0', 'text']);
    if (typeof text === 'string') {
      try {
        return JSON.parse(text) as T;
      } catch {
        // Fall through to a contract error below.
      }
    }
    throw new Error(`${toolName} returned no structured result`);
  } finally {
    await closeMcpSession(mcpUrl, stableOpts, sessionId).catch(() => undefined);
  }
}

function parseMcpToolNames(value: unknown): string[] {
  if (!value || typeof value !== 'object') return [];
  const result = (value as Record<string, unknown>).result;
  if (!result || typeof result !== 'object') return [];
  const tools = (result as Record<string, unknown>).tools;
  if (!Array.isArray(tools)) return [];
  return tools
    .map((tool) => {
      if (!tool || typeof tool !== 'object') return null;
      const name = (tool as Record<string, unknown>).name;
      return typeof name === 'string' ? name : null;
    })
    .filter((name): name is string => Boolean(name));
}

function readString(value: unknown, path: string[]): string | null {
  const cursor = readValue(value, path);
  return typeof cursor === 'string' && cursor.trim() ? cursor : null;
}

function readValue(value: unknown, path: string[]): unknown {
  let cursor = value;
  for (const key of path) {
    if (Array.isArray(cursor)) {
      const index = Number.parseInt(key, 10);
      if (!Number.isInteger(index)) return undefined;
      cursor = cursor[index];
      continue;
    }
    if (!cursor || typeof cursor !== 'object') return undefined;
    cursor = (cursor as Record<string, unknown>)[key];
  }
  return cursor;
}

function readBoolean(value: unknown, path: string[]): boolean {
  let cursor = value;
  for (const key of path) {
    if (!cursor || typeof cursor !== 'object') return false;
    cursor = (cursor as Record<string, unknown>)[key];
  }
  return cursor === true;
}

function readArrayLength(value: unknown, path: string[]): number {
  let cursor = value;
  for (const key of path) {
    if (!cursor || typeof cursor !== 'object') return 0;
    cursor = (cursor as Record<string, unknown>)[key];
  }
  return Array.isArray(cursor) ? cursor.length : 0;
}

function readJsonRpcError(value: unknown): string | null {
  if (!value || typeof value !== 'object') return null;
  const error = (value as Record<string, unknown>).error;
  if (!error) return null;
  return extractJsonErrorDetail(error) ?? JSON.stringify(error);
}

function truncateErrorDetail(detail: string): string {
  const compact = detail.replace(/\s+/g, ' ').trim();
  return compact.length <= 500 ? compact : `${compact.slice(0, 497)}...`;
}

function soloConnectionHint(apiUrl: string): string {
  if (apiUrl === DEFAULT_SOLO_API_URL) {
    return 'Start or unlock Solo from the tray, then retry.';
  }
  if (apiUrl === MCP_BRIDGE_URL) {
    return 'Bridge mode is for local development; switch to Solo HTTP unless the bridge is running.';
  }
  return 'Check Settings > Solo API URL, then retry.';
}

const GRAPH_PAGE_LIMIT = 500;
const GRAPH_PAGE_SAFETY_LIMIT = 25;

interface GraphPage<T> {
  next_cursor?: string | null;
  nodes?: T[];
  edges?: T[];
}

async function fetchGraphPages<T>(
  path: '/v1/graph/nodes' | '/v1/graph/edges',
  key: 'nodes' | 'edges',
  opts: RequestOptions,
): Promise<T[]> {
  const items: T[] = [];
  let cursor: string | null = null;
  for (let page = 0; page < GRAPH_PAGE_SAFETY_LIMIT; page += 1) {
    const params = new URLSearchParams({ limit: String(GRAPH_PAGE_LIMIT) });
    if (cursor) params.set('cursor', cursor);
    const res = await jsonFetch<GraphPage<T>>(`${path}?${params.toString()}`, opts);
    items.push(...(res[key] ?? []));
    cursor = res.next_cursor ?? null;
    if (!cursor) return items;
  }
  throw new Error(`Solo graph pagination exceeded ${GRAPH_PAGE_SAFETY_LIMIT} pages for ${path}`);
}

/** GET /v1/graph/nodes + GET /v1/graph/edges, batched client-side into one response. */
export async function fetchGraph(opts: RequestOptions = {}): Promise<GraphResponse> {
  const [nodes, edges] = await Promise.all([
    fetchGraphPages<GraphResponse['nodes'][number]>('/v1/graph/nodes', 'nodes', opts),
    fetchGraphPages<GraphResponse['edges'][number]>('/v1/graph/edges', 'edges', opts),
  ]);
  return {
    nodes,
    edges,
    next_cursor: undefined,
  };
}

/** GET /v1/graph/inspect/:id */
export async function fetchInspect(
  id: string,
  opts: RequestOptions = {},
): Promise<InspectResponse> {
  return jsonFetch<InspectResponse>(`/v1/graph/inspect/${encodeURIComponent(id)}`, opts);
}

export interface NeighborsQuery {
  kind?: 'explicit' | 'semantic' | 'both';
  threshold?: number;
  limit?: number;
}

/**
 * GET /v1/graph/neighbors/:id — explicit (triple/cluster_member/document_chunk)
 * + HNSW-semantic neighbors unified into one { nodes, edges } envelope. Used
 * by the InspectorPanel "Show similar" button.
 */
export async function fetchNeighbors(
  id: string,
  query: NeighborsQuery = {},
  opts: RequestOptions = {},
): Promise<GraphResponse> {
  const params = new URLSearchParams();
  if (query.kind) params.set('kind', query.kind);
  if (typeof query.threshold === 'number') params.set('threshold', String(query.threshold));
  if (typeof query.limit === 'number') params.set('limit', String(query.limit));
  const qs = params.toString();
  const path = `/v1/graph/neighbors/${encodeURIComponent(id)}${qs ? `?${qs}` : ''}`;
  return jsonFetch<GraphResponse>(path, opts);
}

function stripGraphPrefix(id: string, prefix: string): string {
  return id.startsWith(prefix) ? id.slice(prefix.length) : id;
}

/** POST /memory */
export async function rememberMemory(
  body: {
    content: string;
    source_type?: string;
    source_id?: string;
    salience?: number;
  },
  opts: RequestOptions = {},
): Promise<RememberResponse> {
  return jsonRequest<RememberResponse>('/memory', {
    ...opts,
    method: 'POST',
    body,
  });
}

/** POST /memory/documents */
export async function ingestDocument(
  body: { path: string },
  opts: RequestOptions = {},
): Promise<IngestReport> {
  return jsonRequest<IngestReport>('/memory/documents', {
    ...opts,
    method: 'POST',
    body,
  });
}

/** POST /memory/documents/import */
export async function importDocumentPath(
  body: {
    path: string;
    source?: string;
    dry_run?: boolean;
    recursive?: boolean;
    max_files?: number;
  },
  opts: RequestOptions = {},
): Promise<NativeImportResponse> {
  return jsonRequest<NativeImportResponse>('/memory/documents/import', {
    ...opts,
    method: 'POST',
    body,
  });
}

export type BrowserDocumentImportStage =
  | 'preparing'
  | 'uploading'
  | 'resuming'
  | 'committing'
  | 'extracting'
  | 'complete';

export interface BrowserDocumentImportProgress {
  stage: BrowserDocumentImportStage;
  bytesSent: number;
  totalBytes: number;
}

export interface BrowserDocumentImportRecoveryCheckpoint {
  uploadId: string;
  stagedUri: string | null;
  storeOriginalFile: boolean;
}

export interface BrowserDocumentImportOptions extends RequestOptions {
  /** Undefined means use the retention default returned by Solo at prepare time. */
  storeOriginalFile?: boolean;
  onProgress?: (progress: BrowserDocumentImportProgress) => void;
  /**
   * Durable, non-secret coordinates that the caller may retain so an
   * interrupted commit/extraction can be recovered without source bytes.
   */
  onRecoveryCheckpoint?: (checkpoint: BrowserDocumentImportRecoveryCheckpoint) => void;
}

export class DocumentImportUncertainError extends Error {
  readonly uploadId: string;
  readonly stagedUri: string | null;
  readonly phase: 'commit' | 'ingest';
  readonly storeOriginalFile: boolean | null;

  constructor(
    message: string,
    details: {
      uploadId: string;
      stagedUri?: string | null;
      phase: 'commit' | 'ingest';
      storeOriginalFile?: boolean | null;
      cause: unknown;
    },
  ) {
    super(message, { cause: details.cause });
    this.name = 'DocumentImportUncertainError';
    this.uploadId = details.uploadId;
    this.stagedUri = details.stagedUri ?? null;
    this.phase = details.phase;
    this.storeOriginalFile = details.storeOriginalFile ?? null;
  }
}

export class DocumentImportCleanupUncertainError extends Error {
  readonly uploadId: string;

  constructor(uploadId: string, cause: unknown) {
    super(
      'The import stopped, but Solo could not confirm that its uncommitted staged bytes were deleted. Do not assume cleanup succeeded; reconnect and retry cleanup or wait for the staging TTL.',
      { cause },
    );
    this.name = 'DocumentImportCleanupUncertainError';
    this.uploadId = uploadId;
  }
}

export class DocumentImportRecoveryTerminalError extends Error {
  readonly reason: 'aborted' | 'expired' | 'incomplete';

  constructor(reason: 'aborted' | 'expired' | 'incomplete', message: string) {
    super(message);
    this.name = 'DocumentImportRecoveryTerminalError';
    this.reason = reason;
  }
}

/** POST /memory/documents/uploads */
export async function prepareDocumentUpload(
  file: Pick<File, 'name' | 'size' | 'type'>,
  opts: RequestOptions = {},
): Promise<DocumentUploadPrepareResponse> {
  return jsonRequest<DocumentUploadPrepareResponse>('/memory/documents/uploads', {
    ...opts,
    method: 'POST',
    body: {
      filename: file.name,
      mime_type: file.type || undefined,
      size_bytes: file.size,
    },
  });
}

/** Send a browser File through Solo's resumable raw-byte data plane. */
export async function uploadPreparedDocument(
  prepared: DocumentUploadPrepareResponse,
  file: Blob,
  opts: RequestOptions = {},
  onProgress?: (bytesSent: number, resumed?: boolean) => void,
): Promise<void> {
  const connection = requestConnection(opts);
  const stableOpts = { ...opts, connection };
  const apiUrl = connection.apiUrl;
  const uploadUrl = new URL(prepared.upload_url || prepared.upload_path, `${apiUrl}/`);
  const apiOrigin = new URL(apiUrl).origin;
  if (prepared.route_kind === 'direct_local' && uploadUrl.origin !== apiOrigin) {
    throw new Error('Solo returned a cross-origin URL for a direct local document upload.');
  }

  const chunkBytes = Math.max(
    1,
    Math.min(prepared.max_chunk_bytes || prepared.recommended_chunk_bytes || file.size, file.size),
  );
  let offset = 0;
  let recoveries = 0;
  while (offset < file.size) {
    const end = Math.min(offset + chunkBytes, file.size);
    const headers: Record<string, string> = { ...prepared.required_headers };
    setHeader(headers, prepared.upload_offset_header || 'upload-offset', String(offset));
    setHeader(headers, prepared.upload_length_header || 'upload-length', String(file.size));
    if (prepared.route_kind === 'direct_local') {
      const bearer = connection.bearerToken;
      if (bearer) setHeader(headers, 'Authorization', `Bearer ${bearer}`);
    }

    let response: Response;
    try {
      response = await fetch(uploadUrl, {
        method: prepared.upload_method || 'PATCH',
        headers,
        signal: opts.signal,
        body: file.slice(offset, end),
      });
    } catch (err) {
      if (isAbortError(err)) throw err;
      const resumedOffset = await recoverUploadOffset(
        prepared.upload_id,
        file.size,
        stableOpts,
        recoveries,
      );
      if (resumedOffset !== null) {
        recoveries += 1;
        offset = resumedOffset;
        onProgress?.(offset, true);
        continue;
      }
      throw new Error(`Document byte upload failed: ${errorMessage(err)}`);
    }
    if (!response.ok) {
      const detail = await readErrorDetail(response);
      if ([502, 503, 504].includes(response.status)) {
        const resumedOffset = await recoverUploadOffset(
          prepared.upload_id,
          file.size,
          stableOpts,
          recoveries,
        );
        if (resumedOffset !== null) {
          recoveries += 1;
          offset = resumedOffset;
          onProgress?.(offset, true);
          continue;
        }
      }
      throw new Error(
        `Document byte upload failed (${response.status}${
          response.statusText ? ` ${response.statusText}` : ''
        })${detail ? `: ${detail}` : ''}`,
      );
    }
    recoveries = 0;
    offset = end;
    onProgress?.(offset, false);
  }
}

async function recoverUploadOffset(
  uploadId: string,
  fileSize: number,
  opts: RequestOptions,
  recoveries: number,
): Promise<number | null> {
  if (recoveries >= 2 || opts.signal?.aborted) return null;
  try {
    const status = await getDocumentUploadStatus(uploadId, opts);
    if (status.status !== 'open') return null;
    if (status.next_offset < 0 || status.next_offset > fileSize) return null;
    return status.next_offset;
  } catch {
    return null;
  }
}

/** GET /memory/documents/uploads/{upload_id}; used to resume interrupted chunks safely. */
export async function getDocumentUploadStatus(
  uploadId: string,
  opts: RequestOptions = {},
): Promise<DocumentUploadStatusResponse> {
  return jsonRequest<DocumentUploadStatusResponse>(
    `/memory/documents/uploads/${encodeURIComponent(uploadId)}`,
    opts,
  );
}

/** POST /memory/documents/uploads/{upload_id}/commit */
export async function commitDocumentUpload(
  uploadId: string,
  opts: RequestOptions = {},
): Promise<DocumentUploadCommitResponse> {
  return jsonRequest<DocumentUploadCommitResponse>(
    `/memory/documents/uploads/${encodeURIComponent(uploadId)}/commit`,
    { ...opts, method: 'POST', body: {} },
  );
}

/** DELETE /memory/documents/uploads/{upload_id} */
export async function abortDocumentUpload(
  uploadId: string,
  opts: RequestOptions = {},
): Promise<DocumentUploadAbortResponse> {
  return jsonRequest<DocumentUploadAbortResponse>(
    `/memory/documents/uploads/${encodeURIComponent(uploadId)}`,
    {
      ...opts,
      method: 'DELETE',
    },
  );
}

/** POST /memory/documents/staged/ingest */
export async function ingestStagedDocument(
  stagedUri: string,
  storeOriginalFile: boolean,
  opts: RequestOptions = {},
): Promise<StagedDocumentIngestResponse> {
  return jsonRequest<StagedDocumentIngestResponse>('/memory/documents/staged/ingest', {
    ...opts,
    method: 'POST',
    body: {
      staged_uri: stagedUri,
      retain_source_file: false,
      store_original_file: storeOriginalFile,
    },
  });
}

/** Complete the staged upload contract for one browser-selected file. */
export async function importBrowserDocument(
  file: File,
  opts: BrowserDocumentImportOptions,
): Promise<StagedDocumentIngestResponse> {
  const connection = requestConnection(opts);
  const stableOpts = { ...opts, connection };
  const progress = (stage: BrowserDocumentImportStage, bytesSent: number) =>
    opts.onProgress?.({ stage, bytesSent, totalBytes: file.size });
  let uploadId: string | null = null;
  let commitAttempted = false;
  try {
    progress('preparing', 0);
    const prepared = await prepareDocumentUpload(file, stableOpts);
    uploadId = prepared.upload_id;
    const storeOriginalFile = opts.storeOriginalFile ?? prepared.default_store_original_file;
    opts.onRecoveryCheckpoint?.({
      uploadId: prepared.upload_id,
      stagedUri: null,
      storeOriginalFile,
    });
    progress('uploading', 0);
    await uploadPreparedDocument(prepared, file, stableOpts, (bytesSent, resumed) =>
      progress(resumed ? 'resuming' : 'uploading', bytesSent),
    );
    progress('committing', file.size);
    commitAttempted = true;
    let stagedUri: string;
    try {
      const committed = await commitDocumentUpload(prepared.upload_id, stableOpts);
      stagedUri = committed.staged_uri;
    } catch (error) {
      const recovery = await recoverCommitOutcome(prepared.upload_id, connection);
      if (recovery.kind === 'ingested') {
        progress('complete', file.size);
        return recovery.result;
      }
      if (recovery.kind === 'not_committed') {
        throw error;
      }
      if (recovery.kind === 'committed') {
        stagedUri = recovery.stagedUri;
      } else {
        throw new DocumentImportUncertainError(
          'Solo may have committed the upload, but its state could not be confirmed. Do not re-upload it yet; reconnect and retry status or let the staging TTL clean it up.',
          {
            uploadId: prepared.upload_id,
            stagedUri: recovery.stagedUri,
            phase: 'commit',
            storeOriginalFile,
            cause: error,
          },
        );
      }
    }
    opts.onRecoveryCheckpoint?.({
      uploadId: prepared.upload_id,
      stagedUri,
      storeOriginalFile,
    });
    progress('extracting', file.size);
    let result: StagedDocumentIngestResponse;
    try {
      result = await ingestStagedDocument(stagedUri, storeOriginalFile, stableOpts);
    } catch (error) {
      const recoveredResult = await recoverIngestOutcome(prepared.upload_id, connection);
      if (recoveredResult) {
        progress('complete', file.size);
        return recoveredResult;
      }
      throw new DocumentImportUncertainError(
        'The upload is committed but extraction did not finish. Retry extraction from the staged upload; do not upload the file again.',
        {
          uploadId: prepared.upload_id,
          stagedUri,
          phase: 'ingest',
          storeOriginalFile,
          cause: error,
        },
      );
    }
    progress('complete', file.size);
    return result;
  } catch (error) {
    if (uploadId && !commitAttempted) {
      try {
        const aborted = await abortDocumentUpload(uploadId, {
          connection,
        });
        if (!aborted || aborted.status !== 'aborted') {
          throw new Error('Solo returned no terminal abort receipt');
        }
      } catch (cleanupError) {
        throw new DocumentImportCleanupUncertainError(uploadId, cleanupError);
      }
    }
    throw error;
  }
}

type CommitRecovery =
  | { kind: 'committed'; stagedUri: string }
  | { kind: 'ingested'; result: StagedDocumentIngestResponse }
  | { kind: 'not_committed' }
  | { kind: 'unknown'; stagedUri: string | null };

const RECOVERY_POLL_DELAYS_MS = [0, 25, 75, 200, 500] as const;

async function recoverCommitOutcome(
  uploadId: string,
  connection: ApiConnectionSnapshot,
): Promise<CommitRecovery> {
  let lastStagedUri: string | null = null;
  for (const delayMs of RECOVERY_POLL_DELAYS_MS) {
    await recoveryDelay(delayMs);
    const status = await getDocumentUploadStatus(uploadId, {
      connection,
    }).catch(() => null);
    if (!status) continue;
    lastStagedUri = status.staged_uri ?? status.commit_result?.staged_uri ?? lastStagedUri;
    if (status.ingest_result) return { kind: 'ingested', result: status.ingest_result };
    if (status.status === 'busy' || status.operation_in_progress) continue;
    if (status.status === 'committed' && lastStagedUri) {
      return { kind: 'committed', stagedUri: lastStagedUri };
    }
    if (status.status === 'aborted' || status.status === 'expired') {
      return { kind: 'not_committed' };
    }
    if (status.status === 'open') {
      try {
        const aborted = await abortDocumentUpload(uploadId, {
          connection,
        });
        if (aborted?.status === 'aborted') return { kind: 'not_committed' };
      } catch {
        // A commit may have won the lock between status and abort. Poll again.
      }
    }
  }
  return { kind: 'unknown', stagedUri: lastStagedUri };
}

async function recoverIngestOutcome(
  uploadId: string,
  connection: ApiConnectionSnapshot,
): Promise<StagedDocumentIngestResponse | null> {
  for (const delayMs of RECOVERY_POLL_DELAYS_MS) {
    await recoveryDelay(delayMs);
    const status = await getDocumentUploadStatus(uploadId, {
      connection,
    }).catch(() => null);
    if (status?.ingest_result) return status.ingest_result;
  }
  return null;
}

async function recoveryDelay(delayMs: number): Promise<void> {
  if (delayMs === 0) return;
  await new Promise<void>((resolve) => globalThis.setTimeout(resolve, delayMs));
}

type ResumeRecovery =
  | { kind: 'committed'; stagedUri: string }
  | { kind: 'ingested'; result: StagedDocumentIngestResponse };

const USER_RECOVERY_POLL_DELAYS_MS = [0, 50, 150, 400, 1_000] as const;

async function recoverUploadForUserResume(
  uploadId: string,
  connection: ApiConnectionSnapshot,
  signal?: AbortSignal,
): Promise<ResumeRecovery> {
  let lastError: unknown = new Error('Solo did not return a stable upload status');
  let lastStagedUri: string | null = null;

  for (const delayMs of USER_RECOVERY_POLL_DELAYS_MS) {
    if (signal?.aborted) throw new DOMException('The recovery request was aborted.', 'AbortError');
    await recoveryDelay(delayMs);
    let status: DocumentUploadStatusResponse;
    try {
      status = await getDocumentUploadStatus(uploadId, {
        connection,
        signal,
      });
    } catch (error) {
      if (isAbortError(error)) throw error;
      lastError = error;
      continue;
    }

    lastStagedUri = status.staged_uri ?? status.commit_result?.staged_uri ?? lastStagedUri;
    if (status.ingest_result) return { kind: 'ingested', result: status.ingest_result };
    if (status.status === 'committed' && lastStagedUri) {
      return { kind: 'committed', stagedUri: lastStagedUri };
    }
    if (status.status === 'aborted' || status.status === 'expired') {
      throw new DocumentImportRecoveryTerminalError(
        status.status,
        `Solo upload ${uploadId} is ${status.status} and cannot be resumed.`,
      );
    }
    if (status.status === 'open' && !status.operation_in_progress) {
      if (status.bytes_received !== status.size_bytes) {
        throw new DocumentImportRecoveryTerminalError(
          'incomplete',
          `Solo upload ${uploadId} is incomplete (${status.bytes_received} of ${status.size_bytes} bytes); keep the original file selected and start a fresh import after the staging TTL.`,
        );
      }
      try {
        const committed = await commitDocumentUpload(uploadId, {
          connection,
          signal,
        });
        return { kind: 'committed', stagedUri: committed.staged_uri };
      } catch (error) {
        if (isAbortError(error)) throw error;
        lastError = error;
      }
    }
  }

  throw new DocumentImportUncertainError(
    'Solo still cannot confirm this upload. Retry recovery when Core is reachable; do not select the file again.',
    {
      uploadId,
      stagedUri: lastStagedUri,
      phase: 'commit',
      cause: lastError,
    },
  );
}

/** Recover an uncertain commit/ingest by durable upload id without sending source bytes again. */
export async function resumeUncertainDocumentImport(
  uploadId: string,
  stagedUri: string | null,
  storeOriginalFile: boolean,
  opts: RequestOptions = {},
): Promise<StagedDocumentIngestResponse> {
  const connection = requestConnection(opts);
  const stableOpts = { ...opts, connection };
  let resolvedStagedUri = stagedUri;

  if (!resolvedStagedUri) {
    const recovery = await recoverUploadForUserResume(uploadId, connection, opts.signal);
    if (recovery.kind === 'ingested') return recovery.result;
    resolvedStagedUri = recovery.stagedUri;
  }

  try {
    return await ingestStagedDocument(resolvedStagedUri, storeOriginalFile, stableOpts);
  } catch (error) {
    if (isAbortError(error)) throw error;
    const recoveredResult = await recoverIngestOutcome(uploadId, connection);
    if (recoveredResult) return recoveredResult;
    throw new DocumentImportUncertainError(
      'Solo has the committed upload, but extraction is still uncertain. Retry recovery; do not upload the source bytes again.',
      {
        uploadId,
        stagedUri: resolvedStagedUri,
        phase: 'ingest',
        storeOriginalFile,
        cause: error,
      },
    );
  }
}

/** Retry extraction for a known committed upload without duplicating source bytes. */
export async function resumeStagedDocumentImport(
  stagedUri: string,
  storeOriginalFile: boolean,
  opts: RequestOptions = {},
): Promise<StagedDocumentIngestResponse> {
  const connection = requestConnection(opts);
  return ingestStagedDocument(stagedUri, storeOriginalFile, { ...opts, connection });
}

/** DELETE /memory/documents/{id}; this is a soft forget, not a hard purge. */
export async function forgetDocument(
  id: string,
  opts: RequestOptions = {},
): Promise<ForgetDocumentReport> {
  return jsonRequest<ForgetDocumentReport>(`/memory/documents/${encodeURIComponent(id)}`, {
    ...opts,
    method: 'DELETE',
  });
}

/** Delete retained source-file bytes through Solo's destructive MCP tool. */
export async function forgetRetainedAsset(
  assetId: string,
  opts: RequestOptions = {},
): Promise<ForgetAssetReport> {
  return callMcpTool<ForgetAssetReport>('memory_forget_asset', { asset_id: assetId }, opts);
}

/** Durable lifecycle catalogs survive navigation/reload because Solo is authoritative. */
export interface LifecycleCatalogResult<T> {
  items: T[];
  truncated: boolean;
  limit: number;
}

export async function listDocumentLifecycle(
  opts: RequestOptions = {},
): Promise<LifecycleCatalogResult<DocumentLifecycleSummary>> {
  const connection = requestConnection(opts);
  return listLifecyclePages<DocumentLifecycleSummary>(
    'memory_list_documents',
    'documents',
    { include_forgotten: true },
    { ...opts, connection },
  );
}

export async function listAssetLifecycle(
  opts: RequestOptions = {},
): Promise<LifecycleCatalogResult<AssetLifecycleSummary>> {
  const connection = requestConnection(opts);
  return listLifecyclePages<AssetLifecycleSummary>(
    'memory_list_assets',
    'assets',
    { include_deleted: true },
    { ...opts, connection },
  );
}

const LIFECYCLE_PAGE_SIZE = 100;
const MAX_LIFECYCLE_RECORDS = 1_000;

async function listLifecyclePages<T>(
  toolName: 'memory_list_documents' | 'memory_list_assets',
  resultKey: 'documents' | 'assets',
  filters: Record<string, unknown>,
  opts: RequestOptions,
): Promise<LifecycleCatalogResult<T>> {
  const rows: T[] = [];
  while (rows.length <= MAX_LIFECYCLE_RECORDS) {
    const requestLimit = Math.min(LIFECYCLE_PAGE_SIZE, MAX_LIFECYCLE_RECORDS + 1 - rows.length);
    const result = await callMcpTool<Record<'documents' | 'assets', T[]>>(
      toolName,
      { limit: requestLimit, offset: rows.length, ...filters },
      opts,
    );
    const page = result[resultKey];
    if (!Array.isArray(page)) throw new Error(`${toolName} returned no ${resultKey} array`);
    rows.push(...page);
    if (rows.length > MAX_LIFECYCLE_RECORDS) {
      return {
        items: rows.slice(0, MAX_LIFECYCLE_RECORDS),
        truncated: true,
        limit: MAX_LIFECYCLE_RECORDS,
      };
    }
    if (page.length < requestLimit) {
      return { items: rows, truncated: false, limit: MAX_LIFECYCLE_RECORDS };
    }
  }
  return { items: rows, truncated: false, limit: MAX_LIFECYCLE_RECORDS };
}

/** POST /backup */
export async function runBackup(
  body: BackupRequest,
  opts: RequestOptions = {},
): Promise<BackupResponse> {
  return jsonRequest<BackupResponse>('/backup', {
    ...opts,
    method: 'POST',
    body,
  });
}

/** GET /v1/inbox */
export async function fetchInbox(
  limit = 100,
  opts: RequestOptions = {},
): Promise<MemoryInboxItem[]> {
  const params = new URLSearchParams({ limit: String(limit) });
  const res = await jsonFetch<MemoryInboxResponse>(`/v1/inbox?${params.toString()}`, opts);
  return res.items;
}

/** GET /v1/logs */
export async function fetchLogs(limit = 200, opts: RequestOptions = {}): Promise<LogsResponse> {
  const params = new URLSearchParams({ source: 'tray', limit: String(limit) });
  return jsonFetch<LogsResponse>(`/v1/logs?${params.toString()}`, opts);
}

/** POST /v1/inbox/{id}/review */
export async function reviewMemory(
  id: string,
  state: MemoryReviewRequestState,
  opts: RequestOptions & { note?: string } = {},
): Promise<MemoryReviewReport> {
  const memoryId = stripGraphPrefix(id, 'ep:');
  return jsonRequest<MemoryReviewReport>(`/v1/inbox/${encodeURIComponent(memoryId)}/review`, {
    signal: opts.signal,
    method: 'POST',
    body: {
      state,
      note: opts.note,
    },
  });
}

/** PATCH /memory/{id} */
export async function updateMemory(
  id: string,
  content: string,
  opts: RequestOptions = {},
): Promise<MemoryUpdateResult> {
  const memoryId = stripGraphPrefix(id, 'ep:');
  return jsonRequest<MemoryUpdateResult>(`/memory/${encodeURIComponent(memoryId)}`, {
    ...opts,
    method: 'PATCH',
    body: { content },
  });
}

/** DELETE /memory/{id}?reason= */
export async function forgetMemory(
  id: string,
  reason = 'solo-web',
  opts: RequestOptions = {},
): Promise<void> {
  const memoryId = stripGraphPrefix(id, 'ep:');
  const params = new URLSearchParams();
  if (reason.trim()) params.set('reason', reason.trim());
  const qs = params.toString();
  await jsonRequest<void>(`/memory/${encodeURIComponent(memoryId)}${qs ? `?${qs}` : ''}`, {
    ...opts,
    method: 'DELETE',
  });
}

/** GET /memory/entities */
export async function fetchEntities(
  query: string,
  limit = 8,
  opts: RequestOptions = {},
): Promise<EntityHit[]> {
  const params = new URLSearchParams({ query, limit: String(limit) });
  return jsonFetch<EntityHit[]>(`/memory/entities?${params.toString()}`, opts);
}

export interface FactsAboutQuery {
  subject: string;
  predicate?: string;
  includeAsObject?: boolean;
  sinceMs?: number;
  untilMs?: number;
  limit?: number;
}

/** GET /memory/facts_about */
export async function fetchFactsAbout(
  query: FactsAboutQuery,
  opts: RequestOptions = {},
): Promise<FactHit[]> {
  const params = new URLSearchParams({
    subject: query.subject,
    limit: String(query.limit ?? 8),
  });
  if (query.predicate?.trim()) params.set('predicate', query.predicate.trim());
  if (query.includeAsObject) params.set('include_as_object', 'true');
  if (typeof query.sinceMs === 'number') params.set('since_ms', String(query.sinceMs));
  if (typeof query.untilMs === 'number') params.set('until_ms', String(query.untilMs));
  return jsonFetch<FactHit[]>(`/memory/facts_about?${params.toString()}`, opts);
}

export interface MemoryQualityAuditQuery {
  lowConfidenceBelow?: number;
  lowCoherenceBelow?: number;
  longLiteralChars?: number;
  sampleLimit?: number;
}

/** GET /memory/quality/audit */
export async function fetchMemoryQualityAudit(
  query: MemoryQualityAuditQuery = {},
  opts: RequestOptions = {},
): Promise<MemoryQualityAuditReport> {
  const params = new URLSearchParams();
  if (typeof query.lowConfidenceBelow === 'number') {
    params.set('low_confidence_below', String(query.lowConfidenceBelow));
  }
  if (typeof query.lowCoherenceBelow === 'number') {
    params.set('low_coherence_below', String(query.lowCoherenceBelow));
  }
  if (typeof query.longLiteralChars === 'number') {
    params.set('long_literal_chars', String(query.longLiteralChars));
  }
  if (typeof query.sampleLimit === 'number') {
    params.set('sample_limit', String(query.sampleLimit));
  }
  const qs = params.toString();
  return jsonFetch<MemoryQualityAuditReport>(`/memory/quality/audit${qs ? `?${qs}` : ''}`, opts);
}

/** GET /memory/quality/reviews */
export async function fetchMemoryQualityReviews(
  limit = 20,
  opts: RequestOptions = {},
): Promise<MemoryQualityReviewsResponse> {
  const params = new URLSearchParams({ status: 'needs_review', limit: String(limit) });
  return jsonFetch<MemoryQualityReviewsResponse>(
    `/memory/quality/reviews?${params.toString()}`,
    opts,
  );
}

/** POST /memory/quality/reviews/{review_id} */
export async function updateMemoryQualityReview(
  reviewId: string,
  body: MemoryQualityReviewUpdateRequest,
  opts: RequestOptions = {},
): Promise<MemoryQualityReviewItem> {
  return jsonRequest<MemoryQualityReviewItem>(
    `/memory/quality/reviews/${encodeURIComponent(reviewId)}`,
    {
      ...opts,
      method: 'POST',
      body,
    },
  );
}

/** POST /memory/consolidate */
export async function consolidateMemory(opts: RequestOptions = {}): Promise<ConsolidationReport> {
  return jsonRequest<ConsolidationReport>('/memory/consolidate', {
    ...opts,
    method: 'POST',
  });
}

/** POST /memory/triples/extract */
export async function extractTriplesNow(opts: RequestOptions = {}): Promise<TriplesExtractReport> {
  return jsonRequest<TriplesExtractReport>('/memory/triples/extract', {
    ...opts,
    method: 'POST',
  });
}

/** POST /memory/derived/repair */
export async function repairDerivedMemory(
  body: DerivedRepairRequest = {},
  opts: RequestOptions = {},
): Promise<DerivedRepairReport> {
  return jsonRequest<DerivedRepairReport>('/memory/derived/repair', {
    ...opts,
    method: 'POST',
    body,
  });
}

/** POST /v1/settings/embedder/ollama */
export async function switchOllamaEmbedder(
  body: { model?: string; dim?: number; base_url?: string },
  opts: RequestOptions = {},
): Promise<OllamaEmbedderSwitchResponse> {
  return jsonRequest<OllamaEmbedderSwitchResponse>('/v1/settings/embedder/ollama', {
    ...opts,
    method: 'POST',
    body,
  });
}

/** POST /v1/settings/llm */
export async function switchStewardLlm(
  body: StewardLlmSwitchRequest,
  opts: RequestOptions = {},
): Promise<StewardLlmSwitchResponse> {
  return jsonRequest<StewardLlmSwitchResponse>('/v1/settings/llm', {
    ...opts,
    method: 'POST',
    body,
  });
}

/** POST /v1/runtime/restart */
export async function restartSoloRuntime(
  opts: RequestOptions = {},
): Promise<RuntimeRestartResponse> {
  return jsonRequest<RuntimeRestartResponse>('/v1/runtime/restart', {
    ...opts,
    method: 'POST',
  });
}

/** POST /v1/settings/steward/cadence */
export async function switchStewardCadence(
  body: StewardCadenceSettingsRequest,
  opts: RequestOptions = {},
): Promise<StewardCadenceSettingsResponse> {
  return jsonRequest<StewardCadenceSettingsResponse>('/v1/settings/steward/cadence', {
    ...opts,
    method: 'POST',
    body,
  });
}

/** GET /memory/contradictions */
export async function fetchContradictions(
  limit = 10,
  opts: RequestOptions = {},
): Promise<ContradictionHit[]> {
  const params = new URLSearchParams({ limit: String(limit) });
  return jsonFetch<ContradictionHit[]>(`/memory/contradictions?${params.toString()}`, opts);
}

/** POST /memory/contradictions/resolve */
export async function resolveContradiction(
  hit: Pick<ContradictionHit, 'a_id' | 'b_id' | 'kind'>,
  opts: RequestOptions & { note?: string; winningTripleId?: string } = {},
): Promise<ContradictionResolution> {
  return jsonRequest<ContradictionResolution>('/memory/contradictions/resolve', {
    signal: opts.signal,
    method: 'POST',
    body: {
      a_id: hit.a_id,
      b_id: hit.b_id,
      kind: hit.kind,
      status: 'resolved',
      resolution_note: opts.note,
      winning_triple_id: opts.winningTripleId,
    },
  });
}

/** POST /v1/project/policy */
export async function renderProjectPolicy(
  project: ProjectDescriptor,
  client: ProjectPolicyClient,
  opts: RequestOptions = {},
): Promise<ProjectPolicyResponse> {
  return jsonRequest<ProjectPolicyResponse>('/v1/project/policy', {
    ...opts,
    method: 'POST',
    body: { project, client },
  });
}

/** POST /v1/project/facts */
export async function fetchProjectFacts(
  project: ProjectDescriptor,
  body: { subject?: string; limit?: number } = {},
  opts: RequestOptions = {},
): Promise<ProjectFactsResponse> {
  return jsonRequest<ProjectFactsResponse>('/v1/project/facts', {
    ...opts,
    method: 'POST',
    body: {
      project,
      subject: body.subject?.trim() || undefined,
      limit: body.limit,
    },
  });
}

/** POST /v1/project/decisions */
export async function addProjectDecision(
  project: ProjectDescriptor,
  decision: string,
  opts: RequestOptions = {},
): Promise<ProjectDecisionAddResponse> {
  return jsonRequest<ProjectDecisionAddResponse>('/v1/project/decisions', {
    ...opts,
    method: 'POST',
    body: { project, decision },
  });
}

/** POST /v1/project/decisions/search */
export async function searchProjectDecisions(
  project: ProjectDescriptor,
  query: string,
  opts: RequestOptions & { limit?: number } = {},
): Promise<ProjectDecisionSearchResponse> {
  return jsonRequest<ProjectDecisionSearchResponse>('/v1/project/decisions/search', {
    signal: opts.signal,
    method: 'POST',
    body: { project, query, limit: opts.limit },
  });
}
