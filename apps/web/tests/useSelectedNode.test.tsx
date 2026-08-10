/**
 * Tests for src/hooks/useSelectedNode.ts.
 *
 * The hook is `enabled: selectedNodeId !== null` so we explicitly cover
 * the disabled state (returns no data, never calls fetch) and the
 * enabled state (calls fetchInspect and refetches on id change).
 */

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useSelectedNode } from '../src/hooks/useSelectedNode';
import { useGraphStore } from '../src/store/graphStore';

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

const SAMPLE_INSPECT = {
  id: 'ep:01935b9c-1234-7abc-89de-fedcba987654',
  kind: 'episode' as const,
  label: 'Sample',
  text: 'inspect body',
};

describe('useSelectedNode', () => {
  beforeEach(() => {
    vi.stubEnv('VITE_SOLO_USE_MOCKS', '');
    useGraphStore.setState({ selectedNodeId: null });
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it('stays idle (fetchInspect never called) when no node is selected', async () => {
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    const { result } = renderHook(() => useSelectedNode(), { wrapper: Wrapper });

    // Give the query machinery a turn just in case it tries to run.
    await new Promise((r) => setTimeout(r, 20));

    expect(result.current.data).toBeUndefined();
    expect(result.current.fetchStatus).toBe('idle');
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('calls /v1/graph/inspect/:id when a node is selected', async () => {
    useGraphStore.setState({ selectedNodeId: SAMPLE_INSPECT.id });
    const fetchMock = vi.fn(async () => jsonResponse(SAMPLE_INSPECT));
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    const { result } = renderHook(() => useSelectedNode(), { wrapper: Wrapper });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data?.id).toBe(SAMPLE_INSPECT.id);
    expect(fetchMock).toHaveBeenCalledOnce();
    const calledUrl = String(fetchMock.mock.calls[0]?.[0]);
    expect(calledUrl).toContain('/v1/graph/inspect/');
    expect(calledUrl).toContain(encodeURIComponent(SAMPLE_INSPECT.id));
  });

  it('never sends a tenant or library selector', async () => {
    useGraphStore.setState({ selectedNodeId: SAMPLE_INSPECT.id });
    const fetchMock = vi.fn(async () => jsonResponse(SAMPLE_INSPECT));
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    renderHook(() => useSelectedNode(), { wrapper: Wrapper });

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const headers = (fetchMock.mock.calls[0]?.[1] as RequestInit | undefined)?.headers as
      | Record<string, string>
      | undefined;
    expect(headers).not.toHaveProperty('X-Solo-Tenant');
  });

  it('refetches when selectedNodeId changes', async () => {
    useGraphStore.setState({ selectedNodeId: 'ep:first' });
    const fetchMock = vi.fn(async (input: RequestInfo | URL) =>
      jsonResponse({ ...SAMPLE_INSPECT, id: String(input).split('/').pop() }),
    );
    vi.stubGlobal('fetch', fetchMock);

    const Wrapper = makeWrapper();
    const { result } = renderHook(() => useSelectedNode(), { wrapper: Wrapper });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    const initialCalls = fetchMock.mock.calls.length;

    act(() => {
      useGraphStore.setState({ selectedNodeId: 'ep:second' });
    });

    await waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThan(initialCalls));
    const lastUrl = String(fetchMock.mock.calls.at(-1)?.[0]);
    expect(lastUrl).toContain('ep%3Asecond');
  });

  it('surfaces a non-2xx response as result.error', async () => {
    useGraphStore.setState({ selectedNodeId: 'ep:missing' });
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => new Response('not found', { status: 404, statusText: 'Not Found' })),
    );

    const Wrapper = makeWrapper();
    const { result } = renderHook(() => useSelectedNode(), { wrapper: Wrapper });

    await waitFor(() => expect(result.current.isError).toBe(true));
    expect((result.current.error as Error).message).toMatch(/404/);
  });
});
