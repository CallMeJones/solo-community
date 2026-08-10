// TanStack Query hook that loads Community's single memory graph.
//
// Now wired against Solo's real /v1/graph/* surface (v0.10.0). Mock data
// stays available in `api/mocks` for offline dev / Storybook — set
// `VITE_SOLO_USE_MOCKS=1` to switch back.

import { useQuery } from '@tanstack/react-query';
import { fetchGraph } from '../api/client';
import type { GraphResponse } from '../api/types';
import { useSettingsStore } from '../store/settingsStore';

const USE_MOCKS = import.meta.env.VITE_SOLO_USE_MOCKS === '1';

export function useGraphData() {
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);

  return useQuery<GraphResponse>({
    queryKey: ['graph', apiUrl, connectionRevision, USE_MOCKS ? 'mock' : 'live'],
    queryFn: async ({ signal }) => {
      if (USE_MOCKS) {
        await new Promise((r) => setTimeout(r, 50));
        const { getMockGraph } = await import('../api/mocks');
        return getMockGraph();
      }
      return fetchGraph({ signal });
    },
    // 60s stale time matches Solo's expected write cadence at human-scale
    // memory churn; the SSE /v1/graph/stream consumer (see lib/sseInvalidations.ts)
    // refetches sooner when an invalidate event fires for the affected page.
    staleTime: 60_000,
  });
}
