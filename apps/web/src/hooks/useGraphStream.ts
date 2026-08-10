// SSE invalidation subscription for Solo's `/v1/graph/stream` (v0.10.0).
//
// Why not browser EventSource: it's GET-only AND can't attach custom
// headers, so bearer authentication does not fit. We use fetch + the
// readSSE parser from lib/sse.ts.
//
// Event shapes from Solo Community's public graph-stream contract:
//   event: invalidate
//   data: {"kind":"episode","ids":["ep:..."],"reason":"memory.remember"}
//
//   event: heartbeat
//   data: {"ts_ms":1234567890}
//
// On any invalidate we refetch Community's single graph query.
// Granular per-id invalidation is overkill for the page-load scale
// solo-web targets (≤10k nodes); a full refetch every few seconds when
// memory is changing is fine.

import { useQueryClient } from '@tanstack/react-query';
import { useEffect } from 'react';
import { readSSE } from '../lib/sse';
import { useGraphStore } from '../store/graphStore';
import { useSettingsStore } from '../store/settingsStore';

const USE_MOCKS = import.meta.env.VITE_SOLO_USE_MOCKS === '1';
const RECONNECT_DELAY_MS = 2000;

export function useGraphStream(): void {
  const apiUrl = useSettingsStore((s) => s.apiUrl).replace(/\/$/, '');
  const bearerToken = useSettingsStore((s) => s.bearerToken);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const queryClient = useQueryClient();

  useEffect(() => {
    if (USE_MOCKS) return;

    const controller = new AbortController();
    let stopped = false;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    async function connect(): Promise<void> {
      if (stopped) return;
      try {
        const headers: Record<string, string> = {
          Accept: 'text/event-stream',
        };
        const bearer = bearerToken.trim();
        if (bearer.length > 0) headers.Authorization = `Bearer ${bearer}`;
        const res = await fetch(`${apiUrl}/v1/graph/stream`, {
          headers,
          signal: controller.signal,
        });
        if (!res.ok || !res.body) {
          throw new Error(`/v1/graph/stream returned ${res.status}`);
        }
        for await (const event of readSSE(res.body, controller.signal)) {
          if (event.event === 'invalidate') {
            handleInvalidate(event.data);
          }
          // heartbeat — ignored; the stream staying open IS the heartbeat
        }
      } catch (err) {
        if (controller.signal.aborted) return;
        console.warn('[solo-web] graph stream dropped; reconnecting…', err);
      }
      if (!stopped) {
        reconnectTimer = setTimeout(() => void connect(), RECONNECT_DELAY_MS);
      }
    }

    function handleInvalidate(raw: string): void {
      try {
        JSON.parse(raw);
        useGraphStore.getState().markGraphInvalidated();
        // Solo's invalidate event tells us the affected kind. For v0.1
        // we just refetch the whole graph; granular per-id refetches
        // are a future optimization once page-load gets expensive.
        queryClient.invalidateQueries({ queryKey: ['graph'] });
        // Any graph mutation can change the selected-node detail surface:
        // documents, triples, contradictions, clusters, and episodes all
        // feed the inspect panel.
        queryClient.invalidateQueries({ queryKey: ['inspect'] });
      } catch {
        // Malformed event — ignore.
      }
    }

    void connect();

    return () => {
      stopped = true;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      controller.abort();
    };
  }, [apiUrl, bearerToken, connectionRevision, queryClient]);
}
