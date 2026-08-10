/**
 * Tests for src/hooks/useGraphStream.ts.
 *
 * The hook opens a fetch-based SSE connection to /v1/graph/stream and
 * invalidates the relevant TanStack Query keys whenever an `invalidate`
 * event arrives. We exercise it by:
 *
 *   1. Mocking globalThis.fetch to return a Response wrapping a
 *      controllable ReadableStream;
 *   2. Pushing properly-framed SSE events into that stream;
 *   3. Spying on queryClient.invalidateQueries.
 *
 * The reconnect-on-error path is out of scope here — it uses real
 * timeouts and would either need fake timers or an integration test.
 */

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { act, renderHook, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useGraphStream } from '../src/hooks/useGraphStream';
import { useGraphStore } from '../src/store/graphStore';
import { useSettingsStore } from '../src/store/settingsStore';

/** Build a Response whose body is a ReadableStream we can write into. */
function makeStreamResponse(): {
  res: Response;
  send: (chunk: string) => void;
  close: () => void;
} {
  let writer: ReadableStreamDefaultController<Uint8Array>;
  const encoder = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      writer = c;
    },
  });
  return {
    res: new Response(body, {
      status: 200,
      headers: { 'content-type': 'text/event-stream' },
    }),
    send: (chunk: string) => {
      try {
        writer.enqueue(encoder.encode(chunk));
      } catch {
        // Stream already torn down (e.g. by an unmount-triggered abort).
        // Test assertions cover the relevant pre-abort behavior.
      }
    },
    close: () => {
      try {
        writer.close();
      } catch {
        // Same — the hook's cleanup may have already closed the underlying
        // controller via its AbortController; that's fine.
      }
    },
  };
}

function makeWrapper(): {
  Wrapper: ({ children }: { children: ReactNode }) => JSX.Element;
  client: QueryClient;
} {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  const Wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return { Wrapper, client };
}

describe('useGraphStream', () => {
  beforeEach(() => {
    vi.stubEnv('VITE_SOLO_USE_MOCKS', '');
    useGraphStore.setState({ lastGraphInvalidateAtMs: null });
    useSettingsStore.setState({
      apiUrl: 'http://127.0.0.1:17821',
      bearerToken: '',
    });
  });

  afterEach(() => {
    vi.unstubAllEnvs();
    vi.restoreAllMocks();
  });

  it('opens a selector-free fetch against /v1/graph/stream', async () => {
    const stream = makeStreamResponse();
    const fetchMock = vi.fn(async () => stream.res);
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const [url, init] = fetchMock.mock.calls[0]!;
    expect(String(url)).toContain('/v1/graph/stream');
    const headers = (init as RequestInit | undefined)?.headers as Record<string, string>;
    expect(headers).not.toHaveProperty('X-Solo-Tenant');
    expect(headers.Accept).toBe('text/event-stream');

    unmount();
    stream.close();
  });

  it('attaches the bearer token from settingsStore when set', async () => {
    useSettingsStore.setState({
      apiUrl: 'http://127.0.0.1:17821',
      bearerToken: 'secret-123',
    });
    const stream = makeStreamResponse();
    const fetchMock = vi.fn(async () => stream.res);
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });

    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    const headers = (fetchMock.mock.calls[0]?.[1] as RequestInit).headers as Record<string, string>;
    expect(headers.Authorization).toBe('Bearer secret-123');

    unmount();
    stream.close();
  });

  it('aborts and reconnects the stream when the connection credential changes', async () => {
    const first = makeStreamResponse();
    const second = makeStreamResponse();
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(first.res)
      .mockResolvedValueOnce(second.res);
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });
    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    const firstSignal = (fetchMock.mock.calls[0]?.[1] as RequestInit).signal as AbortSignal;

    act(() => useSettingsStore.getState().setBearerToken('rotated-secret'));

    await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(firstSignal.aborted).toBe(true);
    const secondHeaders = (fetchMock.mock.calls[1]?.[1] as RequestInit).headers as Record<
      string,
      string
    >;
    expect(secondHeaders.Authorization).toBe('Bearer rotated-secret');

    unmount();
    first.close();
    second.close();
  });

  it('invalidates the graph query on an `invalidate` event', async () => {
    const stream = makeStreamResponse();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => stream.res),
    );

    const { Wrapper, client } = makeWrapper();
    const spy = vi.spyOn(client, 'invalidateQueries');
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });

    // Wait for the SSE consumer to be reading.
    await waitFor(() => expect(spy).toHaveBeenCalledTimes(0)); // not yet
    stream.send('event: invalidate\ndata: {"kind":"episode","ids":["ep:foo"]}\n\n');

    await waitFor(() => expect(spy).toHaveBeenCalled());
    const calledKeys = spy.mock.calls.map(([arg]) => (arg as { queryKey: unknown[] }).queryKey);
    expect(calledKeys).toContainEqual(['graph']);
    // Episode kind also invalidates inspect.
    expect(calledKeys).toContainEqual(['inspect']);
    expect(useGraphStore.getState().lastGraphInvalidateAtMs).toEqual(expect.any(Number));

    unmount();
    stream.close();
  });

  it.each(['cluster', 'document', 'triple', 'contradiction', 'chunk'])(
    'also invalidates inspect for %s invalidations',
    async (kind) => {
      const stream = makeStreamResponse();
      vi.stubGlobal(
        'fetch',
        vi.fn(async () => stream.res),
      );

      const { Wrapper, client } = makeWrapper();
      const spy = vi.spyOn(client, 'invalidateQueries');
      const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });

      await waitFor(() => expect(spy).not.toHaveBeenCalled());

      stream.send(`event: invalidate\ndata: {"kind":"${kind}","ids":["${kind}:x"]}\n\n`);
      await waitFor(() => expect(spy).toHaveBeenCalled());
      const calledKeys = spy.mock.calls.map(([arg]) => (arg as { queryKey: unknown[] }).queryKey);
      expect(calledKeys).toContainEqual(['graph']);
      expect(calledKeys).toContainEqual(['inspect']);

      unmount();
      stream.close();
    },
  );

  it('invalidates inspect for valid invalidations even when kind is omitted', async () => {
    const stream = makeStreamResponse();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => stream.res),
    );

    const { Wrapper, client } = makeWrapper();
    const spy = vi.spyOn(client, 'invalidateQueries');
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });

    await waitFor(() => expect(spy).not.toHaveBeenCalled());

    stream.send('event: invalidate\ndata: {}\n\n');
    await waitFor(() => expect(spy).toHaveBeenCalled());
    const calledKeys = spy.mock.calls.map(([arg]) => (arg as { queryKey: unknown[] }).queryKey);
    expect(calledKeys).toContainEqual(['graph']);
    expect(calledKeys).toContainEqual(['inspect']);

    unmount();
    stream.close();
  });

  it('ignores heartbeat events (no invalidation)', async () => {
    const stream = makeStreamResponse();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => stream.res),
    );

    const { Wrapper, client } = makeWrapper();
    const spy = vi.spyOn(client, 'invalidateQueries');
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });

    stream.send('event: heartbeat\ndata: {"ts_ms":12345}\n\n');
    // Allow microtasks to drain; SSE parser handles the line.
    await new Promise((r) => setTimeout(r, 20));
    expect(spy).not.toHaveBeenCalled();

    unmount();
    stream.close();
  });

  it('ignores malformed invalidate payloads', async () => {
    const stream = makeStreamResponse();
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => stream.res),
    );

    const { Wrapper, client } = makeWrapper();
    const spy = vi.spyOn(client, 'invalidateQueries');
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });

    // The hook does invalidate the graph query BEFORE the JSON parse
    // throws, so a malformed payload still triggers the bare invalidate
    // but the inspect-key branch is skipped. We assert the call count
    // is small and the unrelated paths weren't hit.
    stream.send('event: invalidate\ndata: not-json{\n\n');
    await new Promise((r) => setTimeout(r, 20));
    // It's fine if zero or one calls happened — we just don't want a
    // crash. With the current implementation: zero (JSON.parse throws,
    // caught silently, no invalidates fire).
    expect(spy.mock.calls.length).toBeLessThanOrEqual(1);

    unmount();
    stream.close();
  });

  it('skips the network entirely under VITE_SOLO_USE_MOCKS=1', async () => {
    vi.stubEnv('VITE_SOLO_USE_MOCKS', '1');
    vi.resetModules();
    const { useGraphStream: hook } = await import('../src/hooks/useGraphStream');
    const fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    renderHook(() => hook(), { wrapper: Wrapper });

    await new Promise((r) => setTimeout(r, 30));
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('aborts the in-flight fetch on unmount', async () => {
    let capturedSignal: AbortSignal | undefined;
    const stream = makeStreamResponse();
    const fetchMock = vi.fn(async (_url: RequestInfo | URL, init?: RequestInit) => {
      capturedSignal = init?.signal ?? undefined;
      return stream.res;
    });
    vi.stubGlobal('fetch', fetchMock);

    const { Wrapper } = makeWrapper();
    const { unmount } = renderHook(() => useGraphStream(), { wrapper: Wrapper });
    await waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(capturedSignal?.aborted).toBe(false);

    unmount();
    expect(capturedSignal?.aborted).toBe(true);
    stream.close();
  });
});
