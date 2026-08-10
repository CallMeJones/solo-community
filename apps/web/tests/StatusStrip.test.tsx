import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { StatusStrip } from '../src/components/StatusStrip';
import { DEFAULT_SOLO_API_URL, MCP_BRIDGE_URL } from '../src/config/defaults';
import { useGraphStore } from '../src/store/graphStore';
import { useSettingsStore } from '../src/store/settingsStore';

function wrap(node: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  return <QueryClientProvider client={client}>{node}</QueryClientProvider>;
}

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

function soloStatus(overrides: Partial<Record<string, unknown>> = {}) {
  return {
    ok: true,
    version: '0.11.1',
    build: { version: '0.11.1', version_with_build: '0.11.1' },
    library: { name: 'Community Memory Library', ready: true },
    embedder: {
      name: 'stub',
      version: 'v1',
      dim: 16,
      dtype: 'f32',
    },
    mcp: {
      sessions: 0,
    },
    ...overrides,
  };
}

describe('StatusStrip', () => {
  beforeEach(() => {
    useGraphStore.setState({
      lastGraphInvalidateAtMs: null,
      visibleKinds: new Set(['episode', 'document', 'cluster', 'entity']),
      expandedNodeIds: new Set(),
      recalledNodeIds: new Set(),
      selectedNodeId: null,
      searchQuery: '',
    });
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
  });

  it('shows service health, library, and graph refresh state', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === 'http://solo.test/v1/status') return jsonResponse(soloStatus());
      if (url.startsWith('http://solo.test/v1/graph/nodes'))
        return jsonResponse({
          nodes: [{ id: 'ep:1', kind: 'episode', label: 'A' }],
        });
      if (url.startsWith('http://solo.test/v1/graph/edges')) return jsonResponse({ edges: [] });
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<StatusStrip />));

    await waitFor(() => expect(screen.getByText('Solo custom')).toBeInTheDocument());
    await waitFor(() => expect(screen.getAllByText('online')).toHaveLength(1));
    expect(screen.getByText('Community Memory Library')).toBeInTheDocument();
    expect(screen.getByText('stub@v1')).toBeInTheDocument();
    expect(screen.getByText(/16d f32/)).toBeInTheDocument();
    expect(screen.getByText('MCP')).toBeInTheDocument();
    expect(
      screen.getByRole('button', { name: /Solo custom online at http:\/\/solo.test/i }),
    ).toHaveAttribute('title', expect.stringContaining('Endpoint: http://solo.test'));
    await waitFor(() =>
      expect(screen.getByText(/Graph 1 nodes, 0 links/)).toBeInTheDocument(),
    );
  });

  it('shows visible and loaded graph counts when filters hide graph items', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === 'http://solo.test/v1/status') return jsonResponse(soloStatus());
      if (url.startsWith('http://solo.test/v1/graph/nodes'))
        return jsonResponse({
          nodes: [
            { id: 'ep:1', kind: 'episode', label: 'A' },
            { id: 'chunk:1', kind: 'chunk', label: 'Chunk' },
            { id: 'entity:solo', kind: 'entity', label: 'Solo' },
          ],
        });
      if (url.startsWith('http://solo.test/v1/graph/edges'))
        return jsonResponse({
          edges: [
            {
              id: 'ep:1--document_chunk--chunk:1',
              source: 'ep:1',
              target: 'chunk:1',
              kind: 'document_chunk',
            },
            {
              id: 'ep:1--triple--entity:solo',
              source: 'ep:1',
              target: 'entity:solo',
              kind: 'triple',
            },
          ],
        });
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<StatusStrip />));

    await waitFor(() =>
      expect(screen.getByText(/Graph 2\/3 nodes, 1\/2 links/)).toBeInTheDocument(),
    );
  });

  it('retries a health check when a service pill is clicked', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === 'http://solo.test/v1/status') return jsonResponse(soloStatus());
      if (url.startsWith('http://solo.test/v1/graph/nodes')) return jsonResponse({ nodes: [] });
      if (url.startsWith('http://solo.test/v1/graph/edges')) return jsonResponse({ edges: [] });
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<StatusStrip />));

    await waitFor(() => expect(screen.getAllByText('online')).toHaveLength(1));
    const before = fetchMock.mock.calls.filter(
      ([input]) => String(input) === 'http://solo.test/v1/status',
    ).length;

    fireEvent.click(screen.getByRole('button', { name: /Solo custom online/i }));

    await waitFor(() => {
      const after = fetchMock.mock.calls.filter(
        ([input]) => String(input) === 'http://solo.test/v1/status',
      ).length;
      expect(after).toBeGreaterThan(before);
    });
  });

  it('shows the latest graph stream invalidation timestamp', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url === 'http://solo.test/v1/status') return jsonResponse(soloStatus());
        if (url.startsWith('http://solo.test/v1/graph/nodes')) return jsonResponse({ nodes: [] });
        if (url.startsWith('http://solo.test/v1/graph/edges')) return jsonResponse({ edges: [] });
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );
    useGraphStore.setState({ lastGraphInvalidateAtMs: Date.UTC(2026, 4, 20, 10, 11, 12) });

    render(wrap(<StatusStrip />));

    expect(screen.getByText('Stream')).toBeInTheDocument();
    expect(screen.getByText(/update/)).toBeInTheDocument();
  });

  it('labels the default 17821 Solo endpoint as HTTP and explains the startup hint', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: DEFAULT_SOLO_API_URL,
      bearerToken: '',
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url === `${DEFAULT_SOLO_API_URL}/v1/status`) return jsonResponse(soloStatus());
        if (url.startsWith(`${DEFAULT_SOLO_API_URL}/v1/graph/nodes`))
          return jsonResponse({ nodes: [] });
        if (url.startsWith(`${DEFAULT_SOLO_API_URL}/v1/graph/edges`))
          return jsonResponse({ edges: [] });
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );

    render(wrap(<StatusStrip />));

    const pill = await screen.findByRole('button', {
      name: /Solo HTTP online at http:\/\/127\.0\.0\.1:17821/i,
    });
    expect(pill).toHaveAttribute('title', expect.stringContaining('Start or unlock Solo'));
  });

  it('labels the developer bridge endpoint and explains the bridge startup hint', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: MCP_BRIDGE_URL,
      bearerToken: '',
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        if (url === `${MCP_BRIDGE_URL}/v1/status`) return jsonResponse(soloStatus());
        if (url.startsWith(`${MCP_BRIDGE_URL}/v1/graph/nodes`))
          return jsonResponse({ nodes: [] });
        if (url.startsWith(`${MCP_BRIDGE_URL}/v1/graph/edges`))
          return jsonResponse({ edges: [] });
        throw new Error(`unexpected fetch: ${url}`);
      }),
    );

    render(wrap(<StatusStrip />));

    const pill = await screen.findByRole('button', {
      name: /Development bridge online at http:\/\/127\.0\.0\.1:7436/i,
    });
    expect(pill).toHaveAttribute('title', expect.stringContaining('Local development fallback'));
  });
});
