import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ImportView } from '../src/components/ImportView';
import { useSettingsStore } from '../src/store/settingsStore';

function wrap(node: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  return <QueryClientProvider client={client}>{node}</QueryClientProvider>;
}

describe('ImportView', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
  });

  it('previews selected ChatGPT records and imports them to Solo memory', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === 'http://solo.test/memory') {
        expect(init?.method).toBe('POST');
        expect(init?.headers).not.toHaveProperty('X-Solo-Tenant');
        const body = JSON.parse(String(init?.body));
        expect(body).toMatchObject({
          source_type: 'import.chatgpt',
          source_id: 'chat-1',
        });
        expect(body.content).toContain('ChatGPT conversation: Solo plan');
        return new Response(JSON.stringify({ memory_id: 'mem-1' }), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));
    fireEvent.click(screen.getByRole('button', { name: /^chatgpt$/i }));

    const payload = JSON.stringify([
      {
        id: 'chat-1',
        title: 'Solo plan',
        messages: [{ role: 'user', content: 'Ship the import UX.' }],
      },
    ]);
    const file = new File([payload], 'conversations.json', { type: 'application/json' });

    fireEvent.change(screen.getByLabelText(/^files$/i), {
      target: { files: [file] },
    });

    expect(await screen.findByText('Solo plan')).toBeInTheDocument();
    expect(screen.getByText(/Ship the import UX\./)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^import selected$/i }));

    await waitFor(() =>
      expect(fetchMock).toHaveBeenCalledWith('http://solo.test/memory', expect.any(Object)),
    );
    expect(await screen.findByText('mem-1')).toBeInTheDocument();
    expect(screen.getByText('1/1 imported')).toBeInTheDocument();
  });

  it('keeps one connection snapshot for every record in a parsed import batch', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo-a.test',
      bearerToken: 'token-a',
    });
    const memoryCalls: Array<{ url: string; headers: Record<string, string>; sourceId: string }> = [];
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (!url.endsWith('/memory')) throw new Error(`unexpected fetch: ${url}`);
      const body = JSON.parse(String(init?.body));
      memoryCalls.push({
        url,
        headers: init?.headers as Record<string, string>,
        sourceId: body.source_id,
      });
      if (memoryCalls.length === 1) {
        useSettingsStore.getState().setAll({
          apiUrl: 'http://solo-b.test',
          bearerToken: 'token-b',
        });
      }
      return new Response(JSON.stringify({ memory_id: `mem-${memoryCalls.length}` }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      });
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));
    fireEvent.click(screen.getByRole('button', { name: /^chatgpt$/i }));
    const file = new File(
      [
        JSON.stringify([
          { id: 'chat-a', title: 'First', messages: [{ role: 'user', content: 'One' }] },
          { id: 'chat-b', title: 'Second', messages: [{ role: 'user', content: 'Two' }] },
        ]),
      ],
      'batch.json',
      { type: 'application/json' },
    );
    fireEvent.change(screen.getByLabelText(/^files$/i), { target: { files: [file] } });
    await screen.findByText('First');
    fireEvent.click(screen.getByRole('button', { name: /^import selected$/i }));

    expect(await screen.findByText('2/2 imported')).toBeInTheDocument();
    expect(memoryCalls.map((call) => call.sourceId)).toStrictEqual(['chat-a', 'chat-b']);
    for (const call of memoryCalls) {
      expect(call.url).toBe('http://solo-a.test/memory');
      expect(call.headers).toMatchObject({
        Authorization: 'Bearer token-a',
      });
      expect(call.headers).not.toHaveProperty('X-Solo-Tenant');
    }
  });

  it('re-parses existing files when the source changes without crashing the view', async () => {
    render(wrap(<ImportView />));

    fireEvent.click(screen.getByRole('button', { name: /^markdown\/text$/i }));
    const file = new File(['# Solo notes\nKeep imports portable.'], 'notes.md', {
      type: 'text/markdown',
    });

    fireEvent.change(screen.getByLabelText(/^files$/i), {
      target: { files: [file] },
    });

    expect(await screen.findByText('Solo notes')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^chatgpt$/i }));

    expect(await screen.findByText(/Invalid JSON in notes\.md/)).toBeInTheDocument();
    expect(screen.getByText('No records parsed')).toBeInTheDocument();
  });

  it('scans and imports a local path through document memory', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === 'http://solo.test/memory/documents/import') {
        expect(init?.method).toBe('POST');
        expect(init?.headers).not.toHaveProperty('X-Solo-Tenant');
        const body = JSON.parse(String(init?.body));
        expect(body).toMatchObject({
          path: 'C:\\Notes',
          source: 'native',
          recursive: true,
          max_files: 500,
        });
        return new Response(
          JSON.stringify({
            path: 'C:\\Notes',
            source: body.source,
            source_label: body.source === 'native' ? 'Documents' : 'Markdown/Text',
            dry_run: body.dry_run,
            recursive: true,
            truncated: false,
            total_files: 1,
            total_bytes: 128,
            imported: body.dry_run ? 0 : 1,
            deduped: 0,
            failed: 0,
            chunks_persisted: body.dry_run ? 0 : 2,
            files: [{ path: 'C:\\Notes\\plan.md', bytes: 128 }],
            results: body.dry_run
              ? []
              : [
                  {
                    path: 'C:\\Notes\\plan.md',
                    bytes: 128,
                    doc_id: 'doc-1',
                    chunks_persisted: 2,
                    bytes_ingested: 128,
                    deduped: false,
                  },
                ],
          }),
          {
            status: 200,
            headers: { 'content-type': 'application/json' },
          },
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));

    expect(screen.getByText('Mode: Local files / Codex project')).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText(/^local path$/i), {
      target: { value: 'C:\\Notes' },
    });
    fireEvent.click(screen.getByRole('button', { name: /^scan path$/i }));

    expect(await screen.findByText('Local files / Codex project path scan')).toBeInTheDocument();
    expect(screen.getByText('C:\\Notes\\plan.md')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^chatgpt$/i }));
    expect(screen.getByText('Local files / Codex project path scan')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^import path$/i }));

    expect(await screen.findByText('Local files / Codex project path import')).toBeInTheDocument();
    expect(screen.getByText('1 new')).toBeInTheDocument();
    expect(screen.getByText('doc-1')).toBeInTheDocument();
    expect(screen.getByText('1/1 imported')).toBeInTheDocument();
  });

  it('keeps local path mode independent from selected upload export source', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === 'http://solo.test/memory/documents/import') {
        const body = JSON.parse(String(init?.body));
        expect(body.source).toBe('native');
        return new Response(
          JSON.stringify({
            path: body.path,
            source: body.source,
            source_label: 'Documents',
            dry_run: true,
            recursive: true,
            truncated: false,
            total_files: 1,
            total_bytes: 128,
            imported: 0,
            deduped: 0,
            failed: 0,
            chunks_persisted: 0,
            files: [{ path: 'C:\\Users\\Alex\\Documents\\project\\README.md', bytes: 128 }],
            results: [],
          }),
          {
            status: 200,
            headers: { 'content-type': 'application/json' },
          },
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));

    fireEvent.click(screen.getByRole('button', { name: /^chatgpt$/i }));
    expect(screen.getByText('Mode: Local files / Codex project')).toBeInTheDocument();
    fireEvent.change(screen.getByLabelText(/^local path$/i), {
      target: { value: 'C:\\Users\\Alex\\Documents\\project' },
    });
    fireEvent.click(screen.getByRole('button', { name: /^scan path$/i }));

    expect(await screen.findByText('Local files / Codex project path scan')).toBeInTheDocument();
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/documents/import',
      expect.objectContaining({
        body: expect.stringContaining('"source":"native"'),
      }),
    );
  });

  it('distinguishes deduped local path imports from new imports', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === 'http://solo.test/memory/documents/import') {
        const body = JSON.parse(String(init?.body));
        return new Response(
          JSON.stringify({
            path: body.path,
            source: body.source,
            source_label: 'Markdown/Text',
            dry_run: false,
            recursive: true,
            truncated: false,
            total_files: 1,
            total_bytes: 128,
            imported: 0,
            deduped: 1,
            failed: 0,
            chunks_persisted: 0,
            files: [{ path: 'C:\\Notes\\plan.md', bytes: 128 }],
            results: [
              {
                path: 'C:\\Notes\\plan.md',
                bytes: 128,
                doc_id: 'doc-1',
                chunks_persisted: 0,
                bytes_ingested: 128,
                deduped: true,
              },
            ],
          }),
          {
            status: 200,
            headers: { 'content-type': 'application/json' },
          },
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));

    fireEvent.click(screen.getByRole('button', { name: /^markdown\/text only$/i }));
    fireEvent.change(screen.getByLabelText(/^local path$/i), {
      target: { value: 'C:\\Notes' },
    });
    fireEvent.click(screen.getByRole('button', { name: /^import path$/i }));

    expect(await screen.findByText('Markdown/Text path import')).toBeInTheDocument();
    expect(screen.getByText('0 new')).toBeInTheDocument();
    expect(screen.getByText('1 deduped')).toBeInTheDocument();
    expect(screen.getByText('0 new, 1 deduped / 1')).toBeInTheDocument();
  });

  it('uploads a browser document, reports extraction, and exposes separate lifecycle controls', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === 'http://solo.test/memory/documents/uploads' && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        expect(body).toMatchObject({ filename: 'pilot.docx', size_bytes: 13 });
        return new Response(
          JSON.stringify({
            upload_id: 'upload-ui',
            upload_url: '/uploads/upload-ui',
            upload_path: '/uploads/upload-ui',
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: { 'content-type': 'application/octet-stream' },
            max_file_bytes: 104857600,
            max_chunk_bytes: 8388608,
            recommended_chunk_bytes: 8388608,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: true,
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      if (url === 'http://solo.test/uploads/upload-ui' && init?.method === 'PATCH') {
        expect(init.headers).toMatchObject({
          'upload-offset': '0',
          'upload-length': '13',
        });
        expect(init.headers).not.toHaveProperty('X-Solo-Tenant');
        return new Response(null, { status: 204 });
      }
      if (
        url === 'http://solo.test/memory/documents/uploads/upload-ui/commit' &&
        init?.method === 'POST'
      ) {
        return new Response(
          JSON.stringify({
            upload_id: 'upload-ui',
            staged_uri: 'solo-staged://upload/upload-ui',
            filename: 'pilot.docx',
            mime_type: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
            size_bytes: 13,
            sha256: 'abc',
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      if (url === 'http://solo.test/memory/documents/staged/ingest' && init?.method === 'POST') {
        expect(JSON.parse(String(init.body))).toMatchObject({
          store_original_file: true,
          retain_source_file: false,
        });
        return new Response(
          JSON.stringify({
            staged_uri: 'solo-staged://upload/upload-ui',
            document_id: 'doc-ui',
            chunks_persisted: 2,
            bytes_ingested: 13,
            deduped: false,
            stored_original_file: true,
            asset: {
              asset_id: 'asset-ui',
              sha256: 'abc',
              mime_type: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
              filename: 'pilot.docx',
              size_bytes: 13,
              storage_path: 'assets/abc',
              deduped: false,
            },
            document_asset_link: {
              link_id: 'link-ui',
              doc_id: 'doc-ui',
              asset_id: 'asset-ui',
            },
            extraction_status: 'extracted',
            extraction_error: null,
            deleted_staged_file: false,
            retained_source_file: false,
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      if (url === 'http://solo.test/memory/documents/doc-ui' && init?.method === 'DELETE') {
        return new Response(JSON.stringify({ doc_id: 'doc-ui', chunks_tombstoned: 2 }), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        });
      }
      if (url === 'http://solo.test/mcp' && init?.method === 'DELETE') {
        return new Response(null, { status: 204 });
      }
      if (url === 'http://solo.test/mcp' && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        if (body.method === 'initialize') {
          return new Response(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }), {
            status: 200,
            headers: { 'content-type': 'application/json', 'Mcp-Session-Id': 'session-ui' },
          });
        }
        if (body.method === 'notifications/initialized') {
          return new Response(null, { status: 202 });
        }
        if (body.method === 'tools/call') {
          expect(body.params).toStrictEqual({
            name: 'memory_forget_asset',
            arguments: { asset_id: 'asset-ui' },
          });
          return new Response(
            JSON.stringify({
              jsonrpc: '2.0',
              id: 2,
              result: {
                structuredContent: {
                  asset_id: 'asset-ui',
                  blob_deleted: true,
                  already_deleted: false,
                  document_links: 1,
                  memory_attachments: 0,
                },
              },
            }),
            { status: 200, headers: { 'content-type': 'application/json' } },
          );
        }
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));
    const file = new File(['document body'], 'pilot.docx', {
      type: 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
    });
    fireEvent.change(screen.getByLabelText(/^files$/i), { target: { files: [file] } });

    expect(await screen.findByText('pilot.docx')).toBeInTheDocument();
    expect(screen.getByText('Searchable text')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Import 1 file' }));

    expect(
      await screen.findByText('Searchable - Solo indexed 2 chunks of document content.'),
    ).toBeInTheDocument();
    expect(screen.getByText(/Original source file retained locally/)).toBeInTheDocument();
    expect(
      screen.getByText(/Cleanup warning: Solo retained the staged upload/),
    ).toBeInTheDocument();
    expect(screen.getByText(/This is not a hard purge/)).toBeInTheDocument();
    expect(screen.getByText('1/1 imported')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Forget searchable document pilot.docx' }));
    expect(await screen.findByText('Searchable document forgotten')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Delete retained original pilot.docx' }));
    expect(
      await screen.findByText('Original source bytes deleted; the provenance record remains.'),
    ).toBeInTheDocument();
    expect(confirmSpy).toHaveBeenCalledTimes(2);
  });

  it('keeps one connection snapshot for every file in a document import batch', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo-a.test',
      bearerToken: 'token-a',
    });
    const requests: Array<{ url: string; headers: Record<string, string> }> = [];
    let prepareCount = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (!url.startsWith('http://solo-a.test/')) throw new Error(`wrong connection: ${url}`);
      requests.push({ url, headers: init?.headers as Record<string, string> });
      if (url.endsWith('/memory/documents/uploads') && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        prepareCount += 1;
        const uploadId = body.filename.startsWith('one') ? 'upload-one' : 'upload-two';
        if (prepareCount === 1) {
          useSettingsStore.getState().setAll({
            apiUrl: 'http://solo-b.test',
            bearerToken: 'token-b',
          });
        }
        return new Response(
          JSON.stringify({
            upload_id: uploadId,
            upload_url: `/uploads/${uploadId}`,
            upload_path: `/uploads/${uploadId}`,
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: {},
            max_file_bytes: 100,
            max_chunk_bytes: 100,
            recommended_chunk_bytes: 100,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: false,
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      const uploadMatch = url.match(/\/uploads\/(upload-(?:one|two))$/);
      if (uploadMatch && init?.method === 'PATCH') return new Response(null, { status: 204 });
      const commitMatch = url.match(/\/uploads\/(upload-(?:one|two))\/commit$/);
      if (commitMatch && init?.method === 'POST') {
        return new Response(
          JSON.stringify({
            upload_id: commitMatch[1],
            staged_uri: `solo-staged://upload/${commitMatch[1]}`,
            filename: `${commitMatch[1]}.txt`,
            mime_type: 'text/plain',
            size_bytes: 3,
            sha256: commitMatch[1],
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      if (url.endsWith('/memory/documents/staged/ingest') && init?.method === 'POST') {
        const stagedUri = JSON.parse(String(init.body)).staged_uri as string;
        const uploadId = stagedUri.split('/').at(-1) ?? 'upload';
        return new Response(
          JSON.stringify({
            staged_uri: stagedUri,
            document_id: `doc-${uploadId}`,
            chunks_persisted: 1,
            bytes_ingested: 3,
            deduped: false,
            stored_original_file: false,
            asset: null,
            document_asset_link: null,
            extraction_status: 'extracted',
            extraction_error: null,
            deleted_staged_file: true,
            retained_source_file: false,
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));
    const files = [
      new File(['one'], 'one.txt', { type: 'text/plain' }),
      new File(['two'], 'two.txt', { type: 'text/plain' }),
    ];
    fireEvent.change(screen.getByLabelText(/^files$/i), { target: { files } });
    fireEvent.click(await screen.findByRole('button', { name: 'Import 2 files' }));

    expect(await screen.findByText('2/2 imported')).toBeInTheDocument();
    expect(prepareCount).toBe(2);
    for (const request of requests) {
      expect(request.headers).toMatchObject({
        Authorization: 'Bearer token-a',
      });
      expect(request.headers).not.toHaveProperty('X-Solo-Tenant');
    }
  });

  it('offers upload-id recovery after commit status was temporarily unreachable', async () => {
    useSettingsStore.getState().setBearerToken('private-recovery-bearer');
    let userInitiatedRecovery = false;
    let patchCalls = 0;
    let commitCalls = 0;
    let statusCalls = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      if (url === 'http://solo.test/memory/documents/uploads' && method === 'POST') {
        return new Response(
          JSON.stringify({
            upload_id: 'upload-ui-recovery',
            upload_url: '/uploads/upload-ui-recovery',
            upload_path: '/uploads/upload-ui-recovery',
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: {},
            max_file_bytes: 100,
            max_chunk_bytes: 100,
            recommended_chunk_bytes: 100,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: false,
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      if (url === 'http://solo.test/uploads/upload-ui-recovery' && method === 'PATCH') {
        patchCalls += 1;
        return new Response(null, { status: 204 });
      }
      if (
        url === 'http://solo.test/memory/documents/uploads/upload-ui-recovery/commit' &&
        method === 'POST'
      ) {
        commitCalls += 1;
        if (!userInitiatedRecovery) {
          throw new TypeError('commit succeeded but its response was lost');
        }
        return new Response(
          JSON.stringify({
            upload_id: 'upload-ui-recovery',
            staged_uri: 'solo-staged://upload/upload-ui-recovery',
            filename: 'outage.txt',
            mime_type: 'text/plain',
            size_bytes: 4,
            sha256: 'abc',
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      if (
        url === 'http://solo.test/memory/documents/uploads/upload-ui-recovery' &&
        method === 'GET'
      ) {
        statusCalls += 1;
        if (!userInitiatedRecovery) {
          return new Response('status unavailable', { status: 503 });
        }
        return new Response(
          JSON.stringify({
            upload_id: 'upload-ui-recovery',
            status: 'open',
            bytes_received: 4,
            size_bytes: 4,
            next_offset: 4,
            expires_at_ms: Date.now() + 60_000,
            operation_in_progress: false,
            active_operation: null,
            staged_uri: null,
            commit_result: null,
            ingest_result: null,
            terminal: false,
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      if (url === 'http://solo.test/memory/documents/staged/ingest' && method === 'POST') {
        return new Response(
          JSON.stringify({
            staged_uri: 'solo-staged://upload/upload-ui-recovery',
            document_id: 'doc-ui-recovery',
            chunks_persisted: 1,
            bytes_ingested: 4,
            deduped: false,
            stored_original_file: false,
            asset: null,
            document_asset_link: null,
            extraction_status: 'extracted',
            extraction_error: null,
            deleted_staged_file: true,
            retained_source_file: false,
            idempotent_replay: false,
            ingest_completed_at_ms: 1_700_000_000_789,
          }),
          { status: 200, headers: { 'content-type': 'application/json' } },
        );
      }
      throw new Error(`unexpected fetch: ${method} ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const firstRender = render(wrap(<ImportView />));
    const file = new File(['data'], 'outage.txt', { type: 'text/plain' });
    fireEvent.change(screen.getByLabelText(/^files$/i), { target: { files: [file] } });
    fireEvent.click(await screen.findByRole('button', { name: 'Import 1 file' }));

    await screen.findByRole(
      'button',
      { name: 'Recover import outage.txt' },
      { timeout: 4_000 },
    );
    expect(screen.getByText(/may have committed the upload/i)).toBeInTheDocument();
    const checkpoint = sessionStorage.getItem('solo.import.document-recovery.session.v1');
    expect(checkpoint).toContain('upload-ui-recovery');
    expect(checkpoint).not.toContain('private-recovery-bearer');
    expect(checkpoint).not.toContain('outage.txt');

    act(() => {
      useSettingsStore.getState().setAll({
        apiUrl: 'http://other-solo.test',
        bearerToken: 'other-bearer',
      });
    });
    await screen.findByRole('button', {
      name: 'Recover import Interrupted document upload',
    });
    expect(screen.queryByText('outage.txt')).not.toBeInTheDocument();
    act(() => {
      useSettingsStore.getState().setAll({
        apiUrl: 'http://solo.test',
        bearerToken: 'private-recovery-bearer',
      });
    });

    firstRender.unmount();
    render(wrap(<ImportView />));
    const recoveredAfterNavigation = await screen.findByRole('button', {
      name: 'Recover import Interrupted document upload',
    });
    expect(screen.getByText('Recovery receipt')).toBeInTheDocument();
    expect(screen.queryByText('outage.txt')).not.toBeInTheDocument();
    userInitiatedRecovery = true;
    fireEvent.click(recoveredAfterNavigation);

    expect(
      await screen.findByText('Searchable - Solo indexed 1 chunk of document content.'),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole('button', { name: 'Recover import Interrupted document upload' }),
    ).toBeNull();
    expect(screen.getByText('1/1 imported')).toBeInTheDocument();
    expect(sessionStorage.getItem('solo.import.document-recovery.session.v1')).toBeNull();
    expect(patchCalls).toBe(1);
    expect(commitCalls).toBe(2);
    expect(statusCalls).toBe(6);
  });

  it('loads durable document and asset lifecycle controls from Solo after a fresh render', async () => {
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    let documentStatus: 'active' | 'forgotten' = 'active';
    let assetStatus: 'active' | 'deleted' = 'active';
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === 'http://solo.test/memory/documents/doc-persisted' && init?.method === 'DELETE') {
        documentStatus = 'forgotten';
        return new Response(JSON.stringify({ doc_id: 'doc-persisted', chunks_tombstoned: 2 }), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        });
      }
      if (url === 'http://solo.test/mcp' && init?.method === 'DELETE') {
        return new Response(null, { status: 204 });
      }
      if (url === 'http://solo.test/mcp' && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        if (body.method === 'initialize') {
          return new Response(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }), {
            status: 200,
            headers: { 'content-type': 'application/json', 'Mcp-Session-Id': 'catalog-session' },
          });
        }
        if (body.method === 'notifications/initialized') {
          return new Response(null, { status: 202 });
        }
        if (body.method === 'tools/call' && body.params.name === 'memory_list_documents') {
          return new Response(
            JSON.stringify({
              jsonrpc: '2.0',
              id: 2,
              result: {
                structuredContent: {
                  documents: [
                    {
                      doc_id: 'doc-persisted',
                      title: 'Persistent pilot document',
                      source: 'upload',
                      mime_type: 'text/plain',
                      ingested_at_ms: 1_700_000_000_000,
                      chunk_count: 2,
                      status: documentStatus,
                      extraction_status: 'extracted',
                    },
                  ],
                },
              },
            }),
            { status: 200, headers: { 'content-type': 'application/json' } },
          );
        }
        if (body.method === 'tools/call' && body.params.name === 'memory_list_assets') {
          return new Response(
            JSON.stringify({
              jsonrpc: '2.0',
              id: 2,
              result: {
                structuredContent: {
                  assets: [
                    {
                      asset_id: 'asset-persisted',
                      sha256: 'abc',
                      mime_type: 'text/plain',
                      filename: 'persistent.txt',
                      size_bytes: 12,
                      storage_path: 'assets/abc',
                      source: 'upload',
                      status: assetStatus,
                      created_at_ms: 1_700_000_000_000,
                      updated_at_ms: 1_700_000_000_000,
                    },
                  ],
                },
              },
            }),
            { status: 200, headers: { 'content-type': 'application/json' } },
          );
        }
        if (body.method === 'tools/call' && body.params.name === 'memory_forget_asset') {
          assetStatus = 'deleted';
          return new Response(
            JSON.stringify({
              jsonrpc: '2.0',
              id: 2,
              result: {
                structuredContent: {
                  asset_id: 'asset-persisted',
                  blob_deleted: true,
                  already_deleted: false,
                  document_links: 1,
                  memory_attachments: 0,
                },
              },
            }),
            { status: 200, headers: { 'content-type': 'application/json' } },
          );
        }
      }
      throw new Error(`unexpected fetch: ${init?.method ?? 'GET'} ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ImportView />));
    fireEvent.click(
      screen.getByRole('button', { name: 'Manage saved documents and retained originals' }),
    );

    expect(await screen.findByText('Persistent pilot document')).toBeInTheDocument();
    expect(screen.getByText('persistent.txt')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Forget' }));
    await waitFor(() => expect(screen.queryByRole('button', { name: 'Forget' })).toBeNull());
    fireEvent.click(screen.getByRole('button', { name: 'Delete bytes' }));
    await waitFor(() => expect(screen.queryByRole('button', { name: 'Delete bytes' })).toBeNull());
    expect(confirmSpy).toHaveBeenCalledTimes(2);
  });

  it('labels unsupported files honestly and blocks them when original retention is off', async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
    render(wrap(<ImportView />));

    fireEvent.change(screen.getByRole('combobox', { name: 'Original-file retention' }), {
      target: { value: 'discard' },
    });
    fireEvent.change(screen.getByLabelText(/^files$/i), {
      target: { files: [new File(['binary'], 'model.bin')] },
    });

    expect(await screen.findByText('model.bin')).toBeInTheDocument();
    expect(screen.getByText('No default extractor')).toBeInTheDocument();
    expect(screen.getByText(/Original-file retention is explicitly off/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Import 1 file' }));

    expect(
      await screen.findByText(/This file has no default extractor.*Enable original-file retention/),
    ).toBeInTheDocument();
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
