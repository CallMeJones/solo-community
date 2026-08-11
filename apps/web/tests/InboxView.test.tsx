import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { InboxView } from '../src/components/InboxView';
import type { MemoryInboxItem } from '../src/api/types';
import { useGraphStore } from '../src/store/graphStore';
import { useSettingsStore } from '../src/store/settingsStore';

function makeWrapper(): ({ children }: { children: ReactNode }) => JSX.Element {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  const Wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return Wrapper;
}

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

function errorResponse(body: unknown, status: number, statusText: string): Response {
  return new Response(JSON.stringify(body), {
    status,
    statusText,
    headers: { 'content-type': 'application/json' },
  });
}

function resetStore(): void {
  useGraphStore.setState({
    selectedNodeId: null,
    viewMode: '2d',
    visibleKinds: new Set(['episode', 'document', 'cluster', 'entity']),
    searchQuery: '',
    expandedNodeIds: new Set(),
    recalledNodeIds: new Set(),
  });
}

function inboxItem(
  overrides: Partial<MemoryInboxItem> & Pick<MemoryInboxItem, 'memory_id' | 'label'>,
): MemoryInboxItem {
  return {
    preview: overrides.label,
    ts_ms: 1718000000000,
    source_type: 'user_message',
    salience: 0.5,
    status: 'active',
    review_state: null,
    reviewed_at_ms: null,
    review_note: null,
    ...overrides,
  };
}

describe('InboxView', () => {
  beforeEach(() => {
    vi.stubEnv('VITE_SOLO_USE_MOCKS', '');
    resetStore();
    useSettingsStore.setState({
      apiUrl: 'http://solo-original.test',
      bearerToken: 'original-bearer',
      connectionRevision: 0,
    });
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it('renders recent daemon inbox episodes newest first', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/v1/inbox')) {
          return jsonResponse({
            items: [
              inboxItem({
                memory_id: 'old',
                label: 'Older memory',
                ts_ms: 1715000000000,
              }),
              inboxItem({
                memory_id: 'new',
                label: 'Newer memory',
                ts_ms: 1718000000000,
              }),
            ],
          });
        }
        if (url.includes('/memory/contradictions')) return jsonResponse([]);
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );

    render(
      <QueryClientProvider
        client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
      >
        <InboxView />
      </QueryClientProvider>,
    );

    const list = screen.getByLabelText('Recent episodes');
    await waitFor(() => expect(within(list).getAllByRole('listitem')).toHaveLength(2));
    const items = within(list).getAllByRole('listitem');
    expect(items).toHaveLength(2);
    expect(items[0]).toHaveTextContent('Newer memory');
    expect(items[1]).toHaveTextContent('Older memory');
  });

  it('sorts unresolved contradictions before resolved recent items', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/v1/inbox')) return jsonResponse({ items: [] });
        if (url.includes('/memory/contradictions')) {
          return jsonResponse([
            {
              a_id: 'a-resolved',
              b_id: 'b-resolved',
              kind: 'preference',
              explanation: 'Resolved but newer',
              detected_at_ms: 1719000000000,
              status: 'resolved',
            },
            {
              a_id: 'a-unresolved',
              b_id: 'b-unresolved',
              kind: 'preference',
              explanation: 'Unresolved but older',
              detected_at_ms: 1715000000000,
              status: 'unresolved',
            },
          ]);
        }
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );

    render(
      <QueryClientProvider
        client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
      >
        <InboxView />
      </QueryClientProvider>,
    );

    const list = await screen.findByLabelText('Contradictions');
    await waitFor(() => expect(within(list).getAllByRole('listitem')).toHaveLength(2));
    const items = within(list).getAllByRole('listitem');
    expect(items[0]).toHaveTextContent('Unresolved but older');
    expect(items[1]).toHaveTextContent('Resolved but newer');
  });

  it('can hand an episode selection back to the graph shell', async () => {
    const onSelectEpisode = vi.fn();
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/v1/inbox')) {
          return jsonResponse({
            items: [inboxItem({ memory_id: 'new', label: 'Newer memory' })],
          });
        }
        if (url.includes('/memory/contradictions')) return jsonResponse([]);
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView onSelectEpisode={onSelectEpisode} />
      </Wrapper>,
    );

    fireEvent.click(await screen.findByRole('button', { name: /open newer memory in graph/i }));
    expect(onSelectEpisode).toHaveBeenCalledWith('ep:new');
  });

  it('loads full episode text before saving an edit', async () => {
    const episode = inboxItem({
      memory_id: 'editable',
      label: 'Editable memory',
      preview: 'Short preview',
    });
    let fullText = 'Original full memory text';
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox')) {
        return jsonResponse({ items: [{ ...episode, preview: fullText }] });
      }
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      if (url.includes('/v1/graph/inspect/')) {
        return jsonResponse({
          node: {
            id: 'ep:editable',
            kind: 'episode',
            label: episode.label,
            ts_ms: episode.ts_ms,
          },
          full_text: fullText,
          triples_in: [],
          triples_out: [],
        });
      }
      if (url.includes('/memory/') && init?.method === 'PATCH') {
        fullText = JSON.parse(String(init.body)).content;
        return jsonResponse({
          memory_id: 'editable',
          rowid: 1,
          content: fullText,
          updated_at_ms: 1718000001000,
        });
      }
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    fireEvent.click(await screen.findByRole('button', { name: /edit editable memory/i }));
    const textarea = await screen.findByLabelText(/edit text for editable memory/i);
    expect(textarea).toHaveValue('Original full memory text');

    fireEvent.change(textarea, { target: { value: 'Updated full memory text' } });
    fireEvent.click(screen.getByRole('button', { name: /^save$/i }));

    await waitFor(() => expect(screen.getByText(/^saved$/i)).toBeInTheDocument());
    const patchCall = fetchMock.mock.calls.find(([, init]) => init?.method === 'PATCH');
    expect(String(patchCall?.[0])).toContain('/memory/editable');
    expect(JSON.parse(String(patchCall?.[1]?.body))).toStrictEqual({
      content: 'Updated full memory text',
    });
  });

  it('shows an alert and leaves editing open when saving an edit fails', async () => {
    const episode = inboxItem({
      memory_id: 'locked',
      label: 'Locked memory',
      preview: 'Original full memory text',
    });
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox')) return jsonResponse({ items: [episode] });
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      if (url.includes('/v1/graph/inspect/')) {
        return jsonResponse({
          node: {
            id: 'ep:locked',
            kind: 'episode',
            label: episode.label,
            ts_ms: episode.ts_ms,
          },
          full_text: 'Original full memory text',
          triples_in: [],
          triples_out: [],
        });
      }
      if (url.includes('/memory/') && init?.method === 'PATCH') {
        return errorResponse({ error: 'memory row is locked' }, 409, 'Conflict');
      }
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    fireEvent.click(await screen.findByRole('button', { name: /edit locked memory/i }));
    const textarea = await screen.findByLabelText(/edit text for locked memory/i);
    fireEvent.change(textarea, { target: { value: 'Updated but rejected' } });
    fireEvent.click(screen.getByRole('button', { name: /^save$/i }));

    await waitFor(() => {
      expect(screen.getByRole('alert')).toHaveTextContent('memory row is locked');
    });
    expect(screen.getByLabelText(/edit text for locked memory/i)).toHaveValue(
      'Updated but rejected',
    );
    expect(screen.getByRole('button', { name: /^save$/i })).toBeEnabled();
  });

  it('forgets an episode through DELETE /memory/:id after confirmation', async () => {
    useGraphStore.setState({ selectedNodeId: 'ep:forget-me' });
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox')) {
        return jsonResponse({
          items: [inboxItem({ memory_id: 'forget-me', label: 'Forgettable memory' })],
        });
      }
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      if (url.includes('/memory/') && init?.method === 'DELETE') {
        return new Response(null, { status: 204 });
      }
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    fireEvent.click(await screen.findByRole('button', { name: /forget forgettable memory/i }));

    await waitFor(() => {
      const deleteCall = fetchMock.mock.calls.find(([, init]) => init?.method === 'DELETE');
      expect(deleteCall).toBeTruthy();
    });
    expect(confirmSpy).toHaveBeenCalledWith('Forget memory "Forgettable memory"?');
    expect(useGraphStore.getState().selectedNodeId).toBeNull();
    const deleteCall = fetchMock.mock.calls.find(([, init]) => init?.method === 'DELETE');
    expect(String(deleteCall?.[0])).toContain(
      '/memory/forget-me?reason=Forgotten+from+Solo+Memory+inbox',
    );
  });

  it('shows an alert and keeps the episode selected when forget fails', async () => {
    useGraphStore.setState({ selectedNodeId: 'ep:forget-fails' });
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox')) {
        return jsonResponse({
          items: [inboxItem({ memory_id: 'forget-fails', label: 'Forget failure memory' })],
        });
      }
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      if (url.includes('/memory/') && init?.method === 'DELETE') {
        return errorResponse({ error: 'delete refused' }, 500, 'Internal Server Error');
      }
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    fireEvent.click(await screen.findByRole('button', { name: /forget forget failure memory/i }));

    await waitFor(() => {
      expect(screen.getByRole('alert')).toHaveTextContent('delete refused');
    });
    expect(useGraphStore.getState().selectedNodeId).toBe('ep:forget-fails');
    expect(screen.getByRole('button', { name: /forget forget failure memory/i })).toBeEnabled();
  });

  it('persists daemon-backed inbox review decisions', async () => {
    let reviewState: MemoryInboxItem['review_state'] = null;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox/review-me/review') && init?.method === 'POST') {
        reviewState = JSON.parse(String(init.body)).state;
        return jsonResponse({
          memory_id: 'review-me',
          state: reviewState,
          reviewed_at_ms: 1718000001000,
        });
      }
      if (url.includes('/v1/inbox')) {
        return jsonResponse({
          items: [
            inboxItem({
              memory_id: 'review-me',
              label: 'Reviewable memory',
              review_state: reviewState,
            }),
          ],
        });
      }
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    fireEvent.click(await screen.findByRole('button', { name: /approve reviewable memory/i }));

    await waitFor(() => {
      expect(screen.getByText(/^approved$/i)).toBeInTheDocument();
    });
    const reviewCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes('/v1/inbox/review-me/review'),
    );
    expect(reviewCall?.[1]?.method).toBe('POST');
    expect(JSON.parse(String(reviewCall?.[1]?.body))).toStrictEqual({
      state: 'approved',
      note: 'Approved from Solo Memory inbox',
    });
  });

  it('filters inbox reviews by state and source and copies a content-safe summary', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/v1/inbox')) {
          return jsonResponse({
            items: [
              inboxItem({
                memory_id: 'needs-chatgpt',
                label: 'Private salary memory',
                preview: 'Private salary preview',
                source_type: 'chatgpt_export',
                review_state: null,
              }),
              inboxItem({
                memory_id: 'approved-codex',
                label: 'Approved Codex memory',
                source_type: 'codex',
                review_state: 'approved',
              }),
              inboxItem({
                memory_id: 'dismissed-chatgpt',
                label: 'Dismissed ChatGPT memory',
                source_type: 'chatgpt_export',
                review_state: 'dismissed',
              }),
            ],
          });
        }
        if (url.includes('/memory/contradictions')) return jsonResponse([]);
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    await screen.findByText('Private salary memory');
    fireEvent.change(screen.getByLabelText('Review filter'), {
      target: { value: 'approved' },
    });
    expect(screen.getByText('Approved Codex memory')).toBeInTheDocument();
    expect(screen.queryByText('Private salary memory')).not.toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Review filter'), {
      target: { value: 'all' },
    });
    fireEvent.change(screen.getByLabelText('Source filter'), {
      target: { value: 'chatgpt_export' },
    });
    expect(screen.getByText('Private salary memory')).toBeInTheDocument();
    expect(screen.getByText('Dismissed ChatGPT memory')).toBeInTheDocument();
    expect(screen.queryByText('Approved Codex memory')).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^copy summary$/i }));

    await waitFor(() => expect(writeText).toHaveBeenCalledTimes(1));
    const summary = String(writeText.mock.calls[0][0]);
    expect(summary).toContain('Visible episodes: 2');
    expect(summary).toContain('Source filter: chatgpt export');
    expect(summary).not.toContain('Private salary memory');
    expect(summary).not.toContain('Private salary preview');
  });

  it('bulk reviews only the visible applicable inbox rows', async () => {
    const items = [
      inboxItem({
        memory_id: 'needs-chatgpt',
        label: 'Needs ChatGPT memory',
        source_type: 'chatgpt_export',
        review_state: null,
      }),
      inboxItem({
        memory_id: 'needs-codex',
        label: 'Needs Codex memory',
        source_type: 'codex',
        review_state: null,
      }),
      inboxItem({
        memory_id: 'approved-chatgpt',
        label: 'Approved ChatGPT memory',
        source_type: 'chatgpt_export',
        review_state: 'approved',
      }),
    ];
    const reviewCalls: Array<{ id: string; body: unknown }> = [];
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox/') && url.includes('/review') && init?.method === 'POST') {
        const id = decodeURIComponent(url.split('/v1/inbox/')[1].split('/review')[0]);
        const body = JSON.parse(String(init.body)) as { state: MemoryInboxItem['review_state'] };
        reviewCalls.push({ id, body });
        const item = items.find((entry) => entry.memory_id === id);
        if (item) item.review_state = body.state === 'approved' ? 'approved' : body.state;
        return jsonResponse({
          memory_id: id,
          state: body.state,
          reviewed_at_ms: 1718000001000,
        });
      }
      if (url.includes('/v1/inbox')) return jsonResponse({ items });
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    await screen.findByText('Needs ChatGPT memory');
    fireEvent.change(screen.getByLabelText('Review filter'), {
      target: { value: 'needs_review' },
    });
    fireEvent.change(screen.getByLabelText('Source filter'), {
      target: { value: 'chatgpt_export' },
    });
    fireEvent.click(screen.getByRole('button', { name: /^approve visible$/i }));

    await waitFor(() => expect(reviewCalls).toHaveLength(1));
    expect(reviewCalls[0]).toStrictEqual({
      id: 'needs-chatgpt',
      body: {
        state: 'approved',
        note: 'Bulk approved from Solo Memory inbox',
      },
    });
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/needs-codex/review'))).toBe(
      false,
    );
  });

  it('keeps one immutable connection throughout a deferred bulk review', async () => {
    const items = [
      inboxItem({ memory_id: 'first', label: 'First memory', review_state: null }),
      inboxItem({ memory_id: 'second', label: 'Second memory', review_state: null }),
    ];
    const reviewCalls: Array<{ url: string; authorization: string | null }> = [];
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox/') && url.includes('/review') && init?.method === 'POST') {
        reviewCalls.push({
          url,
          authorization: new Headers(init.headers).get('authorization'),
        });
        if (reviewCalls.length === 1) {
          useSettingsStore.getState().setAll({
            apiUrl: 'http://solo-replacement.test',
            bearerToken: 'replacement-bearer',
          });
        }
        return jsonResponse({
          memory_id: url.includes('/first/') ? 'first' : 'second',
          state: 'approved',
          reviewed_at_ms: 1718000001000,
        });
      }
      if (url.includes('/v1/inbox')) return jsonResponse({ items });
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    await screen.findByText('First memory');
    fireEvent.click(screen.getByRole('button', { name: /^approve visible$/i }));

    await waitFor(() => expect(reviewCalls).toHaveLength(2));
    expect(reviewCalls).toStrictEqual([
      {
        url: 'http://solo-original.test/v1/inbox/first/review',
        authorization: 'Bearer original-bearer',
      },
      {
        url: 'http://solo-original.test/v1/inbox/second/review',
        authorization: 'Bearer original-bearer',
      },
    ]);
  });

  it('refreshes the inbox after a partially successful bulk review', async () => {
    const items = [
      inboxItem({ memory_id: 'committed', label: 'Committed memory', review_state: null }),
      inboxItem({ memory_id: 'failed', label: 'Failed memory', review_state: null }),
    ];
    let inboxFetches = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox/committed/review') && init?.method === 'POST') {
        items[0].review_state = 'approved';
        return jsonResponse({
          memory_id: 'committed',
          state: 'approved',
          reviewed_at_ms: 1718000001000,
        });
      }
      if (url.includes('/v1/inbox/failed/review') && init?.method === 'POST') {
        return errorResponse({ error: 'second review failed' }, 503, 'Service Unavailable');
      }
      if (url.includes('/v1/inbox')) {
        inboxFetches += 1;
        return jsonResponse({ items });
      }
      if (url.includes('/memory/contradictions')) return jsonResponse([]);
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    await screen.findByText('Committed memory');
    fireEvent.click(screen.getByRole('button', { name: /^approve visible$/i }));

    await waitFor(() => expect(screen.getByRole('alert')).toHaveTextContent('second review failed'));
    await waitFor(() => expect(inboxFetches).toBeGreaterThanOrEqual(2));
    expect(screen.getByText('Committed memory').closest('li')).toHaveTextContent(/approved/i);
    expect(screen.getByText('Failed memory').closest('li')).toHaveTextContent(/needs review/i);
  });

  it('resolves a contradiction from the inbox', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox')) return jsonResponse({ items: [] });
      if (url.includes('/memory/contradictions/resolve')) {
        return jsonResponse({
          a_id: 't-a',
          b_id: 't-b',
          kind: 'preference',
          explanation: 'Alice prefers tea and coffee',
          detected_at_ms: 1715000000000,
          status: 'resolved',
          resolved_at_ms: 1715000001000,
          resolution_note: 'Resolved from Solo Memory inbox',
        });
      }
      if (url.includes('/memory/contradictions')) {
        return jsonResponse([
          {
            a_id: 't-a',
            b_id: 't-b',
            kind: 'preference',
            explanation: 'Alice prefers tea and coffee',
            detected_at_ms: 1715000000000,
            status: 'unresolved',
          },
        ]);
      }
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    await screen.findByText(/Alice prefers tea and coffee/i);
    fireEvent.click(screen.getByRole('button', { name: /^resolve$/i }));

    await waitFor(() => expect(screen.getByRole('button', { name: /^resolved$/i })).toBeDisabled());
    const resolveCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes('/memory/contradictions/resolve'),
    );
    expect(resolveCall?.[1]?.method).toBe('POST');
    expect(JSON.parse(String(resolveCall?.[1]?.body))).toMatchObject({
      a_id: 't-a',
      b_id: 't-b',
      kind: 'preference',
      status: 'resolved',
      resolution_note: 'Resolved from Solo Memory inbox',
    });
  });

  it('shows an alert and keeps an unresolved contradiction actionable when resolve fails', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/inbox')) return jsonResponse({ items: [] });
      if (url.includes('/memory/contradictions/resolve')) {
        return errorResponse({ error: 'resolution write failed' }, 503, 'Service Unavailable');
      }
      if (url.includes('/memory/contradictions')) {
        return jsonResponse([
          {
            a_id: 't-a',
            b_id: 't-b',
            kind: 'preference',
            explanation: 'Alice prefers tea and coffee',
            detected_at_ms: 1715000000000,
            status: 'unresolved',
          },
        ]);
      }
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InboxView />
      </Wrapper>,
    );

    await screen.findByText(/Alice prefers tea and coffee/i);
    fireEvent.click(screen.getByRole('button', { name: /^resolve$/i }));

    await waitFor(() => {
      expect(screen.getByRole('alert')).toHaveTextContent('resolution write failed');
    });
    expect(screen.getByRole('button', { name: /^resolve$/i })).toBeEnabled();
  });
});
