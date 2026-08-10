/**
 * RTL tests for src/components/InspectorPanel.tsx.
 *
 * Renders the panel inside a fresh QueryClient. Inspect data comes via
 * useSelectedNode (fetch-mocked to /v1/graph/inspect/:id). The "Show
 * similar" flow uses fetchNeighbors directly (/v1/graph/neighbors/:id
 * ?kind=semantic), which we also mock — and assert that successful
 * neighbor IDs land in graphStore.recalledNodeIds so the graph view
 * can light them up.
 */

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { InspectorPanel } from '../src/components/InspectorPanel';
import { useGraphStore } from '../src/store/graphStore';
import { useSettingsStore } from '../src/store/settingsStore';

const NODE_ID = 'ep:01935b9c-1234-7abc-89de-fedcba987654';
const SIMILAR_NODE_ID = 'ep:01935b9c-aaaa-7bbb-8ccc-dddddddddddd';

const SAMPLE_INSPECT = {
  node: {
    id: NODE_ID,
    kind: 'episode' as const,
    label: 'Met Alice for coffee at the new place downtown',
    ts_ms: 1715000000000,
  },
  full_text: 'Met Alice for coffee at the new place downtown — talked about Helsinki',
  triples_in: [
    {
      id: 'ent:alice--mentions--ep',
      source: 'ent:alice',
      target: NODE_ID,
      kind: 'triple' as const,
      predicate: 'mentions',
    },
  ],
  triples_out: [
    {
      id: `${NODE_ID}--cluster_member--cl:coffee`,
      source: NODE_ID,
      target: 'cl:coffee',
      kind: 'cluster_member' as const,
    },
  ],
};

function makeQueryClient(): QueryClient {
  return new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
}

function makeWrapper(
  client = makeQueryClient(),
): ({ children }: { children: ReactNode }) => JSX.Element {
  const Wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return Wrapper;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
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

describe('InspectorPanel', () => {
  beforeEach(() => {
    vi.stubEnv('VITE_SOLO_USE_MOCKS', '');
    resetStore();
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it('renders empty state when no node is selected', () => {
    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );
    expect(screen.getByText(/no node selected/i)).toBeInTheDocument();
  });

  it('selects a cached graph search match without canvas interaction', () => {
    const client = makeQueryClient();
    const { apiUrl, connectionRevision } = useSettingsStore.getState();
    client.setQueryData(['graph', apiUrl, connectionRevision, 'live'], {
      nodes: [
        SAMPLE_INSPECT.node,
        {
          id: SIMILAR_NODE_ID,
          kind: 'episode' as const,
          label: 'Coffee with Bob at the same place',
        },
      ],
      edges: [],
    });
    useGraphStore.setState({ searchQuery: 'alice' });

    const Wrapper = makeWrapper(client);
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    fireEvent.click(screen.getByRole('button', { name: /met alice for coffee/i }));

    expect(useGraphStore.getState().selectedNodeId).toBe(NODE_ID);
  });

  it('shows the node label, id, and full text once inspect resolves', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => jsonResponse(SAMPLE_INSPECT)),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => expect(screen.getByText(SAMPLE_INSPECT.node.label)).toBeInTheDocument());
    expect(screen.getByText(NODE_ID)).toBeInTheDocument();
    expect(screen.getByText(/Full text/i)).toBeInTheDocument();
    expect(screen.getAllByText(/talked about Helsinki/i).length).toBeGreaterThanOrEqual(1);
  });

  it('clears the selection when "Clear" is clicked', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => jsonResponse(SAMPLE_INSPECT)),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /close inspector/i }));
    expect(useGraphStore.getState().selectedNodeId).toBeNull();
  });

  it('saves edited episode text through PATCH /memory/:id', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    let currentInspect = SAMPLE_INSPECT;
    let inspectCalls = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/graph/inspect/')) {
        inspectCalls++;
        return jsonResponse(currentInspect);
      }
      if (url.includes('/memory/')) {
        currentInspect = {
          ...SAMPLE_INSPECT,
          node: {
            ...SAMPLE_INSPECT.node,
            label: 'Updated memory text',
            preview: 'Updated memory text',
          },
          full_text: 'Updated memory text',
        };
        return jsonResponse({
          memory_id: NODE_ID.replace('ep:', ''),
          rowid: 1,
          content: 'Updated memory text',
          updated_at_ms: 1715000001000,
        });
      }
      throw new Error(`unexpected fetch: ${url} ${init?.method ?? 'GET'}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.change(screen.getByLabelText(/memory correction text/i), {
      target: { value: 'Updated memory text' },
    });
    fireEvent.click(screen.getByRole('button', { name: /^save$/i }));

    await waitFor(() => expect(screen.getByText(/^saved$/i)).toBeInTheDocument());
    await waitFor(() => expect(inspectCalls).toBeGreaterThanOrEqual(2));
    expect(screen.getAllByText(/Updated memory text/i).length).toBeGreaterThanOrEqual(1);
    const patchCall = fetchMock.mock.calls.find(([url, init]) => {
      return String(url).includes('/memory/') && init?.method === 'PATCH';
    });
    expect(String(patchCall?.[0])).toContain(
      `/memory/${encodeURIComponent(NODE_ID.replace('ep:', ''))}`,
    );
    expect(JSON.parse(String(patchCall?.[1]?.body))).toStrictEqual({
      content: 'Updated memory text',
    });
  });

  it('finds matching entities for an entity node', async () => {
    const entityId = 'ent:Alice';
    useGraphStore.setState({ selectedNodeId: entityId });
    const entityInspect = {
      node: {
        id: entityId,
        kind: 'entity' as const,
        label: 'Alice',
        ref_count: 2,
      },
      triples_in: [],
      triples_out: [],
    };
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes('/v1/graph/inspect/')) return jsonResponse(entityInspect);
      if (url.includes('/memory/entities')) {
        return jsonResponse([
          {
            entity_id: 'Alice',
            subject_count: 2,
            object_count: 1,
            fact_count: 3,
            predicates: ['knows', 'works_at'],
            match_score: 0,
          },
        ]);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText('Alice'));
    fireEvent.click(screen.getByRole('button', { name: /^find$/i }));
    await waitFor(() => expect(screen.getByText(/3 facts/i)).toBeInTheDocument());
    const entityCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes('/memory/entities'),
    );
    expect(String(entityCall?.[0])).toContain('query=Alice');
  });

  it('loads and resolves contradictions', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    let inspectCalls = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes('/v1/graph/inspect/')) {
        inspectCalls++;
        return jsonResponse(SAMPLE_INSPECT);
      }
      if (url.includes('/memory/contradictions/resolve')) {
        return jsonResponse({
          a_id: 't-a',
          b_id: 't-b',
          kind: 'other',
          status: 'resolved',
          resolved_at_ms: 1715000001000,
          resolution_note: 'Resolved from Solo Memory inspector',
          winning_triple_id: null,
        });
      }
      if (url.includes('/memory/contradictions')) {
        return jsonResponse([
          {
            a_id: 't-a',
            b_id: 't-b',
            kind: 'other',
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
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /^load$/i }));
    await waitFor(() =>
      expect(screen.getByText(/Alice prefers tea and coffee/i)).toBeInTheDocument(),
    );
    fireEvent.click(screen.getByRole('button', { name: /^resolve$/i }));
    await waitFor(() => expect(screen.getByRole('button', { name: /^resolved$/i })).toBeDisabled());
    await waitFor(() => expect(inspectCalls).toBeGreaterThanOrEqual(2));

    const resolveCall = fetchMock.mock.calls.find(([url]) =>
      String(url).includes('/memory/contradictions/resolve'),
    );
    expect(resolveCall?.[1]?.method).toBe('POST');
    expect(JSON.parse(String(resolveCall?.[1]?.body))).toMatchObject({
      a_id: 't-a',
      b_id: 't-b',
      kind: 'other',
      status: 'resolved',
    });
  });

  it('Show similar fetches /v1/graph/neighbors and populates recalledNodeIds', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    const neighborsResult = {
      nodes: [
        {
          id: SIMILAR_NODE_ID,
          kind: 'episode' as const,
          label: 'Coffee with Bob at the same place',
        },
      ],
      edges: [],
    };
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes('/v1/graph/inspect/')) return jsonResponse(SAMPLE_INSPECT);
      if (url.includes('/v1/graph/neighbors/')) return jsonResponse(neighborsResult);
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /show similar/i }));

    await waitFor(() =>
      expect(screen.getByText(neighborsResult.nodes[0].label)).toBeInTheDocument(),
    );

    const neighborsUrl = fetchMock.mock.calls.find(([u]) =>
      String(u).includes('/v1/graph/neighbors/'),
    )?.[0];
    expect(String(neighborsUrl)).toContain('kind=semantic');
    expect(String(neighborsUrl)).toContain('limit=8');

    const recalled = useGraphStore.getState().recalledNodeIds;
    expect(recalled.has(NODE_ID)).toBe(true);
    expect(recalled.has(SIMILAR_NODE_ID)).toBe(true);
  });

  it('Show related uses explicit neighbors for cluster nodes', async () => {
    const clusterId = 'cl:019ec21d-d7bb-7422-84ea-349a4a77f7b9';
    const clusterInspect = {
      node: {
        id: clusterId,
        kind: 'cluster' as const,
        label: 'Coffee cluster',
      },
      triples_in: [],
      triples_out: [],
    };
    const neighborsResult = {
      nodes: [
        {
          id: NODE_ID,
          kind: 'episode' as const,
          label: SAMPLE_INSPECT.node.label,
        },
      ],
      edges: [],
    };
    useGraphStore.setState({ selectedNodeId: clusterId });
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes('/v1/graph/inspect/')) return jsonResponse(clusterInspect);
      if (url.includes('/v1/graph/neighbors/')) return jsonResponse(neighborsResult);
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText('Coffee cluster'));
    fireEvent.click(screen.getByRole('button', { name: /show related/i }));
    await waitFor(() => expect(screen.getByText(SAMPLE_INSPECT.node.label)).toBeInTheDocument());

    const neighborsUrl = fetchMock.mock.calls.find(([u]) =>
      String(u).includes('/v1/graph/neighbors/'),
    )?.[0];
    expect(String(neighborsUrl)).toContain('kind=explicit');
    expect(String(neighborsUrl)).toContain('limit=8');
  });

  it('surfaces a Show similar error inline', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes('/v1/graph/inspect/')) return jsonResponse(SAMPLE_INSPECT);
      if (url.includes('/v1/graph/neighbors/'))
        return new Response('boom', { status: 500, statusText: 'Server Error' });
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /show similar/i }));

    await waitFor(() => expect(screen.getByText(/500/)).toBeInTheDocument());
  });

  it('renders "No similar nodes found" when neighbors returns an empty list', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/v1/graph/inspect/')) return jsonResponse(SAMPLE_INSPECT);
        return jsonResponse({ nodes: [], edges: [] });
      }),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /show similar/i }));

    await waitFor(() => expect(screen.getByText(/no similar nodes found/i)).toBeInTheDocument());
  });

  it('clicking a similar-list entry selects that node', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    const neighborsResult = {
      nodes: [
        {
          id: SIMILAR_NODE_ID,
          kind: 'episode' as const,
          label: 'Coffee with Bob at the same place',
        },
      ],
      edges: [],
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/v1/graph/inspect/')) return jsonResponse(SAMPLE_INSPECT);
        return jsonResponse(neighborsResult);
      }),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /show similar/i }));
    const similarItem = await screen.findByText(neighborsResult.nodes[0].label);
    fireEvent.click(similarItem);

    expect(useGraphStore.getState().selectedNodeId).toBe(SIMILAR_NODE_ID);
  });

  it('disables the Show similar button while loading', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    let resolveNeighbors: ((r: Response) => void) | null = null;
    vi.stubGlobal(
      'fetch',
      vi.fn((input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes('/v1/graph/inspect/'))
          return Promise.resolve(jsonResponse(SAMPLE_INSPECT));
        return new Promise<Response>((resolve) => {
          resolveNeighbors = resolve;
        });
      }),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    const button = screen.getByRole('button', { name: /show similar/i });
    fireEvent.click(button);

    await waitFor(() => expect(screen.getByRole('button', { name: /searching/i })).toBeDisabled());

    resolveNeighbors?.(jsonResponse({ nodes: [], edges: [] }));
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /show similar/i })).not.toBeDisabled(),
    );
  });

  it('clears the stale similar overlay when the selected node changes', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    const firstNeighbor = {
      id: 'ep:first-neighbor',
      kind: 'episode' as const,
      label: 'First neighbor (relevant to NODE_ID)',
    };
    const otherInspect = {
      ...SAMPLE_INSPECT,
      node: { ...SAMPLE_INSPECT.node, id: SIMILAR_NODE_ID, label: 'Different node' },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes(encodeURIComponent(SIMILAR_NODE_ID)) && url.includes('/inspect/'))
          return jsonResponse(otherInspect);
        if (url.includes('/v1/graph/inspect/')) return jsonResponse(SAMPLE_INSPECT);
        return jsonResponse({ nodes: [firstNeighbor], edges: [] });
      }),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    // Show similar on the first node — the neighbor list appears.
    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /show similar/i }));
    await waitFor(() => expect(screen.getByText(firstNeighbor.label)).toBeInTheDocument());

    // Switch to a different node. The first node's similar overlay should
    // disappear (forNodeId no longer matches selectedNodeId), and the new
    // node's "Show similar" button should be available again.
    useGraphStore.setState({ selectedNodeId: SIMILAR_NODE_ID });
    await waitFor(() => expect(screen.getByText('Different node')).toBeInTheDocument());
    expect(screen.queryByText(firstNeighbor.label)).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /show similar/i })).not.toBeDisabled();
  });

  it('clears a stale similar-fetch error when the selected node changes', async () => {
    useGraphStore.setState({ selectedNodeId: NODE_ID });
    const otherInspect = {
      ...SAMPLE_INSPECT,
      node: { ...SAMPLE_INSPECT.node, id: SIMILAR_NODE_ID, label: 'Other node' },
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url.includes(encodeURIComponent(SIMILAR_NODE_ID)) && url.includes('/inspect/'))
          return jsonResponse(otherInspect);
        if (url.includes('/v1/graph/inspect/')) return jsonResponse(SAMPLE_INSPECT);
        return new Response('boom', { status: 500, statusText: 'Server Error' });
      }),
    );

    const Wrapper = makeWrapper();
    render(
      <Wrapper>
        <InspectorPanel />
      </Wrapper>,
    );

    await waitFor(() => screen.getByText(SAMPLE_INSPECT.node.label));
    fireEvent.click(screen.getByRole('button', { name: /show similar/i }));
    await waitFor(() => expect(screen.getByText(/500/)).toBeInTheDocument());

    // Switch nodes — the stale error should vanish.
    useGraphStore.setState({ selectedNodeId: SIMILAR_NODE_ID });
    await waitFor(() => expect(screen.getByText('Other node')).toBeInTheDocument());
    expect(screen.queryByText(/500/)).not.toBeInTheDocument();
  });
});
