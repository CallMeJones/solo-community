/**
 * Tests for src/hooks/useGraphData.ts.
 *
 * Two paths to exercise:
 *   1. Live path — VITE_SOLO_USE_MOCKS unset; the hook calls fetchGraph,
 *      which in turn fetches /v1/graph/nodes + /v1/graph/edges. We mock
 *      `globalThis.fetch` and assert the fixed Community connection is used.
 *   2. Mock path — VITE_SOLO_USE_MOCKS=1; the hook returns getMockGraph
 *      and never touches fetch. Used by the Toolbar tests too.
 *
 * The query key follows the connection revision, so endpoint/auth changes
 * cause a refetch without introducing a database selector.
 */

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useGraphData } from '../src/hooks/useGraphData';
import { useSettingsStore } from '../src/store/settingsStore';

function makeWrapper(): {
  Wrapper: ({ children }: { children: ReactNode }) => JSX.Element;
  client: QueryClient;
} {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  const Wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return { Wrapper, client };
}

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

describe('useGraphData (live path)', () => {
  beforeEach(() => {
    vi.stubEnv('VITE_SOLO_USE_MOCKS', '');
    useSettingsStore.setState({
      apiUrl: 'http://solo-cache.test',
      bearerToken: 'first-secret',
      connectionRevision: 0,
    });
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it('calls fetchGraph against /v1/graph/nodes + /v1/graph/edges', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const pathname = new URL(url).pathname;
      if (pathname.endsWith('/v1/graph/nodes'))
        return jsonResponse({ nodes: [{ id: 'ep:1', kind: 'episode', label: 'a' }] });
      if (pathname.endsWith('/v1/graph/edges')) return jsonResponse({ edges: [] });
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    const { result } = renderHook(() => useGraphData(), { wrapper: Wrapper });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data?.nodes).toHaveLength(1);
    expect(result.current.data?.nodes[0].id).toBe('ep:1');
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringMatching(/\/v1\/graph\/nodes\?limit=500$/),
      expect.any(Object),
    );
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringMatching(/\/v1\/graph\/edges\?limit=500$/),
      expect.any(Object),
    );
  });

  it('never sends a database selector header', async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ nodes: [], edges: [] }));
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    renderHook(() => useGraphData(), { wrapper: Wrapper });

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const selectorHeaders = fetchMock.mock.calls.map(([, init]) => {
      const headers = (init as RequestInit | undefined)?.headers as
        | Record<string, string>
        | undefined;
      return headers?.['X-Solo-Tenant'];
    });
    expect(selectorHeaders).toStrictEqual([undefined, undefined]);
  });

  it('surfaces fetch errors via result.error', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('boom', { status: 500, statusText: 'Server Error' })),
    );

    const { Wrapper } = makeWrapper();
    const { result } = renderHook(() => useGraphData(), { wrapper: Wrapper });

    await waitFor(() => expect(result.current.isError).toBe(true));
    expect(result.current.error).toBeInstanceOf(Error);
    expect((result.current.error as Error).message).toMatch(/500/);
  });

  it('uses a new cache identity when the session bearer changes without exposing the bearer', async () => {
    const fetchMock = vi.fn(async () => jsonResponse({ nodes: [], edges: [] }));
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper, client } = makeWrapper();
    const { result } = renderHook(() => useGraphData(), { wrapper: Wrapper });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    const initialCallCount = fetchMock.mock.calls.length;

    act(() => useSettingsStore.getState().setBearerToken('second-secret'));

    await waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThan(initialCallCount));
    const serializedKeys = JSON.stringify(
      client.getQueryCache().getAll().map((query) => query.queryKey),
    );
    expect(serializedKeys).not.toContain('first-secret');
    expect(serializedKeys).not.toContain('second-secret');
  });
});

describe('useGraphData (mock path)', () => {
  beforeEach(() => {
    vi.stubEnv('VITE_SOLO_USE_MOCKS', '1');
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    vi.resetModules();
  });

  it('returns the deterministic mock graph without touching fetch', async () => {
    // Force a fresh module evaluation so USE_MOCKS picks up the stubbed env.
    vi.resetModules();
    const { useGraphData: hook } = await import('../src/hooks/useGraphData');
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    const { result } = renderHook(() => hook(), { wrapper: Wrapper });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data?.nodes.length).toBeGreaterThan(0);
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
