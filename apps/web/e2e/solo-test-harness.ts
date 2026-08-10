import { expect, type Page, type Route } from '@playwright/test';
import { getMockGraph, getMockInspect } from '../src/api/mocks';

export const SOLO_API_ORIGIN = 'http://127.0.0.1:17821';

type InboxReviewState = 'approved' | 'dismissed' | null;
type ReviewRequestState = 'approved' | 'dismissed' | 'needs_review' | 'reset' | null;
type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

interface MemoryInboxItem {
  memory_id: string;
  label: string;
  preview: string;
  ts_ms: number;
  source_type: string;
  salience: number;
  status: string;
  review_state: InboxReviewState;
  reviewed_at_ms: number | null;
  review_note: string | null;
}

interface ContradictionItem {
  a_id: string;
  b_id: string;
  kind: string;
  explanation: string;
  detected_at_ms: number;
  status: string;
  resolved_at_ms?: number | null;
  resolution_note?: string | null;
  winning_triple_id?: string | null;
}

interface BackupRequestBody {
  to?: string;
  force?: boolean;
}

interface NativeImportRequestBody {
  path?: string;
  source?: string;
  dry_run?: boolean;
  recursive?: boolean;
  max_files?: number;
}

export interface SoloMockState {
  inboxItems: MemoryInboxItem[];
  contradictions: ContradictionItem[];
  logFetchCount: number;
  backupRequests: BackupRequestBody[];
  nativeImportRequests: NativeImportRequestBody[];
  memoryWrites: unknown[];
  documentTransportEvents: string[];
  mcpRequests: unknown[];
}

const runtimeIssues = new WeakMap<Page, string[]>();

export function createSoloMockState(): SoloMockState {
  return {
    inboxItems: [
      {
        memory_id: 'route-matrix-memory',
        label: 'Review queue route matrix memory',
        preview: 'Review queue route matrix memory',
        ts_ms: 1779290000000,
        source_type: 'e2e',
        salience: 0.8,
        status: 'active',
        review_state: null,
        reviewed_at_ms: null,
        review_note: null,
      },
      {
        memory_id: 'already-approved-memory',
        label: 'Already approved workflow memory',
        preview: 'Already approved workflow memory',
        ts_ms: 1779280000000,
        source_type: 'manual',
        salience: 0.6,
        status: 'active',
        review_state: 'approved',
        reviewed_at_ms: 1779285000000,
        review_note: 'Approved before workflow test',
      },
    ],
    contradictions: [
      {
        a_id: 'triple:a',
        b_id: 'triple:b',
        kind: 'fact_conflict',
        explanation: 'Alice moved offices in two different memories.',
        detected_at_ms: 1779290000000,
        status: 'open',
        winning_triple_id: 'triple:a',
      },
    ],
    logFetchCount: 0,
    backupRequests: [],
    nativeImportRequests: [],
    memoryWrites: [],
    documentTransportEvents: [],
    mcpRequests: [],
  };
}

export function installRuntimeIssueTracking(page: Page): void {
  const issues: string[] = [];
  runtimeIssues.set(page, issues);
  page.on('console', (message) => {
    if (message.type() === 'error') {
      issues.push(`console error: ${message.text()}`);
    }
  });
  page.on('pageerror', (error) => {
    issues.push(`page error: ${error.message}`);
  });
  page.on('response', (response) => {
    const url = response.url();
    if (url.startsWith(SOLO_API_ORIGIN) && response.status() >= 400) {
      issues.push(`mocked service request failed: ${response.status()} ${url}`);
    }
  });
}

export function assertNoRuntimeIssues(page: Page): void {
  expect(runtimeIssues.get(page) ?? []).toEqual([]);
  runtimeIssues.delete(page);
}

export async function installSoloServiceMocks(
  page: Page,
  state: SoloMockState = createSoloMockState(),
): Promise<SoloMockState> {
  await page.route('**/*', async (route) => {
    const requestUrl = new URL(route.request().url());

    if (requestUrl.origin === SOLO_API_ORIGIN) {
      return fulfillSoloApi(route, requestUrl, state);
    }

    return route.continue();
  });

  return state;
}

async function fulfillSoloApi(route: Route, url: URL, state: SoloMockState): Promise<void> {
  const request = route.request();
  const method = request.method();

  if (method === 'OPTIONS') {
    return route.fulfill({
      status: 204,
      headers: corsHeaders(),
    });
  }

  if (url.pathname === '/v1/status') {
    return fulfillJson(route, {
      ok: true,
      version: '0.12.0',
      build: {
        version: '0.12.0',
        version_with_build: '0.12.0+e2e',
        git_sha_short: 'e2e0000',
        git_dirty: 'clean',
      },
      library: {
        name: 'Community Memory Library',
        ready: true,
      },
      embedder: {
        name: 'stub',
        version: 'v1',
        dim: 16,
        dtype: 'f32',
      },
      mcp: {
        sessions: 1,
      },
      runtime: {
        pid: 4242,
        platform: 'win32',
        data_dir: 'C:\\SoloData',
      },
    });
  }

  if (method === 'GET' && url.pathname === '/v1/graph/nodes') {
    const graph = getMockGraph();
    return fulfillJson(route, { nodes: graph.nodes, next_cursor: null });
  }

  if (method === 'GET' && url.pathname === '/v1/graph/edges') {
    const graph = getMockGraph();
    return fulfillJson(route, { edges: graph.edges, next_cursor: null });
  }

  if (method === 'GET' && url.pathname === '/v1/graph/stream') {
    return route.fulfill({
      status: 200,
      headers: corsHeaders(),
      contentType: 'text/event-stream',
      body: sseEvent('heartbeat', { ts_ms: 1779300000000 }),
    });
  }

  if (url.pathname === '/v1/inbox') {
    return fulfillJson(route, { items: state.inboxItems });
  }

  const inboxReviewMatch = url.pathname.match(/^\/v1\/inbox\/([^/]+)\/review$/);
  if (method === 'POST' && inboxReviewMatch) {
    const memoryId = decodeURIComponent(inboxReviewMatch[1]);
    const body = await readJsonBody<{ state?: ReviewRequestState; note?: string }>(route);
    const nextReviewState =
      body.state === 'reset' || body.state === 'needs_review' ? null : (body.state ?? null);
    const reviewedAt = nextReviewState ? 1779300000000 : null;
    state.inboxItems = state.inboxItems.map((item) =>
      item.memory_id === memoryId
        ? {
            ...item,
            review_state: nextReviewState,
            reviewed_at_ms: reviewedAt,
            review_note: nextReviewState ? (body.note ?? null) : null,
          }
        : item,
    );
    return fulfillJson(route, {
      memory_id: memoryId,
      state: nextReviewState,
      reviewed_at_ms: reviewedAt,
    });
  }

  if (url.pathname === '/memory/contradictions') {
    return fulfillJson(route, state.contradictions);
  }

  if (method === 'POST' && url.pathname === '/memory/contradictions/resolve') {
    const body = await readJsonBody<Partial<ContradictionItem>>(route);
    state.contradictions = state.contradictions.map((item) =>
      item.a_id === body.a_id && item.b_id === body.b_id && item.kind === body.kind
        ? {
            ...item,
            status: 'resolved',
            resolved_at_ms: 1779300000000,
            resolution_note: body.resolution_note ?? null,
            winning_triple_id: body.winning_triple_id ?? item.winning_triple_id ?? null,
          }
        : item,
    );
    return fulfillJson(route, {
      a_id: body.a_id,
      b_id: body.b_id,
      kind: body.kind,
      status: 'resolved',
      resolved_at_ms: 1779300000000,
      resolution_note: body.resolution_note ?? null,
      winning_triple_id: body.winning_triple_id ?? null,
    });
  }

  if (url.pathname === '/memory/entities') {
    return fulfillJson(route, [
      {
        entity_id: 'ent:alice',
        subject_count: 2,
        object_count: 3,
        fact_count: 5,
        predicates: ['mentioned_in', 'works_on'],
        match_score: 0.91,
      },
    ]);
  }

  if (method === 'GET' && url.pathname === '/memory/facts_about') {
    const subject = url.searchParams.get('subject') ?? 'solo';
    return fulfillJson(route, [
      {
        triple_id: `triple:${subject}:uses-solo`,
        subject_id: subject,
        predicate: 'uses',
        object_id: 'solo',
        object_kind: 'entity',
        valid_from_ms: 1779290000000,
        valid_to_ms: null,
        confidence: 0.94,
        cluster_id: null,
      },
    ]);
  }

  if (method === 'GET' && url.pathname === '/memory/quality/audit') {
    return fulfillJson(route, {
      generated_at_ms: 1779300000000,
      config: {
        low_confidence_below: 0.85,
        low_coherence_below: 0.72,
        long_literal_chars: 70,
        sample_limit: 8,
      },
      totals: {
        active_episodes: 89,
        clustered_episodes: 80,
        clusters: 7,
        abstractions: 7,
        active_triples: 12,
        entity_triples: 4,
        literal_triples: 8,
        triple_reviews_needs_review: 0,
        distinct_entities: 10,
        contradictions: state.contradictions.filter((item) => item.status === 'open').length,
      },
      health: {
        score: 94,
        grade: 'excellent',
        critical_issues: 0,
        warning_issues: 0,
        info_issues: 0,
      },
      issues: [],
      alias_groups: [],
    });
  }

  if (method === 'GET' && url.pathname === '/memory/quality/reviews') {
    return fulfillJson(route, { items: [] });
  }

  const graphInspectMatch = url.pathname.match(/^\/v1\/graph\/inspect\/(.+)$/);
  if (graphInspectMatch) {
    const id = decodeURIComponent(graphInspectMatch[1]);
    const graphInspect = getMockInspect(id);
    if (graphInspect) return fulfillJson(route, graphInspect);
    const memoryId = id.startsWith('ep:') ? id.slice(3) : id;
    const inboxItem = state.inboxItems.find((item) => item.memory_id === memoryId);
    if (!inboxItem) {
      return fulfillJson(route, { error: `Unknown graph node: ${id}` }, 404);
    }
    return fulfillJson(route, {
      node: {
        id,
        kind: 'episode',
        label: inboxItem.label,
        preview: inboxItem.preview,
        ts_ms: inboxItem.ts_ms,
      },
      full_text: `Full text for ${inboxItem.label}`,
      triples_in: [],
      triples_out: [],
    });
  }

  if (url.pathname === '/v1/logs') {
    state.logFetchCount += 1;
    const limit = Number(url.searchParams.get('limit') ?? '200');
    return fulfillJson(route, {
      source: 'tray',
      path: 'C:\\SoloData\\tray.log',
      exists: true,
      limit,
      size_bytes: 84,
      modified_at_ms: 1779290000000,
      lines: [
        { level: 'info' as LogLevel, text: `INFO ready (limit ${limit})` },
        { level: 'debug' as LogLevel, text: `DEBUG fetch ${state.logFetchCount}` },
      ],
    });
  }

  if (method === 'POST' && url.pathname === '/memory/documents/import') {
    const body = await readJsonBody<NativeImportRequestBody>(route);
    state.nativeImportRequests.push(body);
    const dryRun = body.dry_run !== false;
    return fulfillJson(route, {
      path: body.path ?? '',
      source: body.source ?? 'markdown_text',
      source_label: 'Markdown/Text',
      dry_run: dryRun,
      recursive: body.recursive ?? true,
      truncated: false,
      total_files: 2,
      total_bytes: 1536,
      imported: dryRun ? 0 : 1,
      deduped: dryRun ? 0 : 1,
      failed: 0,
      chunks_persisted: dryRun ? 0 : 3,
      files: [
        { path: `${body.path ?? 'C:\\Solo Imports'}\\notes.md`, bytes: 1024 },
        { path: `${body.path ?? 'C:\\Solo Imports'}\\archive.txt`, bytes: 512 },
      ],
      results: dryRun
        ? []
        : [
            {
              path: `${body.path ?? 'C:\\Solo Imports'}\\notes.md`,
              bytes: 1024,
              doc_id: 'doc:e2e-notes',
              chunks_persisted: 3,
              bytes_ingested: 1024,
              deduped: false,
            },
            {
              path: `${body.path ?? 'C:\\Solo Imports'}\\archive.txt`,
              bytes: 512,
              chunks_persisted: 0,
              bytes_ingested: 0,
              deduped: true,
            },
          ],
    });
  }

  if (method === 'POST' && url.pathname === '/memory/documents/uploads') {
    const body = await readJsonBody<{ filename?: string; mime_type?: string; size_bytes?: number }>(
      route,
    );
    state.documentTransportEvents.push('prepare');
    return fulfillJson(route, {
      upload_id: 'e2e-upload',
      upload_url: '/uploads/e2e-upload',
      upload_path: '/uploads/e2e-upload',
      route_kind: 'direct_local',
      upload_method: 'PATCH',
      upload_offset_header: 'upload-offset',
      upload_length_header: 'upload-length',
      required_headers: { 'content-type': 'application/octet-stream' },
      max_file_bytes: 104857600,
      max_chunk_bytes: 4,
      recommended_chunk_bytes: 4,
      expires_at_ms: Date.now() + 60_000,
      default_store_original_file: true,
      filename: body.filename,
    });
  }

  if (method === 'PATCH' && url.pathname === '/uploads/e2e-upload') {
    state.documentTransportEvents.push(`patch:${request.headers()['upload-offset'] ?? 'missing'}`);
    return route.fulfill({ status: 204, headers: corsHeaders() });
  }

  if (method === 'POST' && url.pathname === '/memory/documents/uploads/e2e-upload/commit') {
    state.documentTransportEvents.push('commit');
    return fulfillJson(route, {
      upload_id: 'e2e-upload',
      staged_uri: 'solo-staged://upload/e2e-upload',
      filename: 'pilot-e2e.txt',
      mime_type: 'text/plain',
      size_bytes: 10,
      sha256: 'e2e-sha',
    });
  }

  if (method === 'POST' && url.pathname === '/memory/documents/staged/ingest') {
    const body = await readJsonBody<{ staged_uri?: string; store_original_file?: boolean }>(route);
    state.documentTransportEvents.push(`ingest:${String(body.store_original_file)}`);
    return fulfillJson(route, {
      staged_uri: body.staged_uri,
      document_id: 'doc-e2e-upload',
      chunks_persisted: 2,
      bytes_ingested: 10,
      deduped: false,
      stored_original_file: body.store_original_file === true,
      asset: {
        asset_id: 'asset-e2e-upload',
        sha256: 'e2e-sha',
        mime_type: 'text/plain',
        filename: 'pilot-e2e.txt',
        size_bytes: 10,
        storage_path: 'assets/e2e-sha',
        deduped: false,
      },
      document_asset_link: {
        link_id: 'link-e2e-upload',
        doc_id: 'doc-e2e-upload',
        asset_id: 'asset-e2e-upload',
      },
      extraction_status: 'extracted',
      extraction_error: null,
      deleted_staged_file: true,
      retained_source_file: false,
      idempotent_replay: false,
      ingest_completed_at_ms: 1779300000000,
    });
  }

  if (method === 'POST' && url.pathname === '/memory') {
    const body = await readJsonBody<unknown>(route);
    state.memoryWrites.push(body);
    return fulfillJson(route, { memory_id: `mem:e2e-${state.memoryWrites.length}` });
  }

  const memoryWriteMatch = url.pathname.match(/^\/memory\/([^/]+)$/);
  if (method === 'PATCH' && memoryWriteMatch) {
    const memoryId = decodeURIComponent(memoryWriteMatch[1]);
    const body = await readJsonBody<{ content?: string }>(route);
    state.inboxItems = state.inboxItems.map((item) =>
      item.memory_id === memoryId
        ? {
            ...item,
            preview: body.content ?? item.preview,
          }
        : item,
    );
    return fulfillJson(route, {
      memory_id: memoryId,
      rowid: 1,
      content: body.content ?? '',
      updated_at_ms: 1779300000000,
    });
  }
  if (method === 'DELETE' && memoryWriteMatch) {
    const memoryId = decodeURIComponent(memoryWriteMatch[1]);
    state.inboxItems = state.inboxItems.filter((item) => item.memory_id !== memoryId);
    return route.fulfill({ status: 204, headers: corsHeaders() });
  }

  if (method === 'POST' && url.pathname === '/backup') {
    const body = await readJsonBody<BackupRequestBody>(route);
    state.backupRequests.push(body);
    return fulfillJson(route, {
      path: body.to ?? 'C:\\SoloData\\solo-backup-e2e.db',
      elapsed_ms: 12,
    });
  }

  if (method === 'POST' && url.pathname === '/mcp') {
    return fulfillMcpPost(route, state);
  }

  if (method === 'DELETE' && url.pathname === '/mcp') {
    return route.fulfill({ status: 204, headers: corsHeaders() });
  }

  return fulfillJson(route, { error: `Unhandled Solo API mock: ${method} ${url.pathname}` }, 501);
}

async function fulfillMcpPost(route: Route, state: SoloMockState): Promise<void> {
  const body = await readJsonBody<{ method?: string; id?: number }>(route);
  state.mcpRequests.push(body);

  if (body.method === 'initialize') {
    return fulfillJson(
      route,
      {
        jsonrpc: '2.0',
        id: body.id,
        result: {
          protocolVersion: '2025-03-26',
          serverInfo: {
            name: 'solo',
            version: '0.12.0',
          },
        },
      },
      200,
      { 'Mcp-Session-Id': 'e2e-mcp-session' },
    );
  }

  if (body.method === 'notifications/initialized') {
    return route.fulfill({ status: 204, headers: corsHeaders() });
  }

  if (body.method === 'tools/list') {
    return fulfillJson(route, {
      jsonrpc: '2.0',
      id: body.id,
      result: {
        tools: [{ name: 'memory_context' }, { name: 'memory_inbox' }, { name: 'memory_review' }],
      },
    });
  }

  if (body.method === 'tools/call') {
    return fulfillJson(route, {
      jsonrpc: '2.0',
      id: body.id,
      result: {
        content: [{ type: 'text', text: 'Solo MCP readiness check passed' }],
        isError: false,
      },
    });
  }

  return fulfillJson(route, { error: `Unhandled MCP method: ${body.method ?? 'unknown'}` }, 501);
}

async function readJsonBody<T>(route: Route): Promise<T> {
  const raw = route.request().postData();
  if (!raw) return {} as T;
  return JSON.parse(raw) as T;
}

async function fulfillJson(
  route: Route,
  body: unknown,
  status = 200,
  headers: Record<string, string> = {},
): Promise<void> {
  await route.fulfill({
    status,
    headers: {
      ...corsHeaders(),
      ...headers,
    },
    contentType: 'application/json',
    body: JSON.stringify(body),
  });
}

function sseEvent(event: string, data: unknown): string {
  return `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
}

function corsHeaders(): Record<string, string> {
  return {
    'Access-Control-Allow-Origin': '*',
    'Access-Control-Allow-Headers': 'Accept, Authorization, Content-Type, Mcp-Session-Id',
    'Access-Control-Allow-Methods': 'DELETE, GET, OPTIONS, PATCH, POST',
    'Access-Control-Expose-Headers': 'Mcp-Session-Id',
  };
}
