// Convenience hook that resolves the currently-selected node + its inspect data.
// Pulls selectedNodeId from the Zustand store; loads inspect details via TanStack Query.
//
// Wired to Solo's /v1/graph/inspect/:id (v0.10.0). Mocks via
// `VITE_SOLO_USE_MOCKS=1` for offline dev.

import { useQuery } from '@tanstack/react-query';
import { fetchInspect } from '../api/client';
import type { InspectResponse } from '../api/types';
import { useGraphStore } from '../store/graphStore';
import { useSettingsStore } from '../store/settingsStore';

const USE_MOCKS = import.meta.env.VITE_SOLO_USE_MOCKS === '1';

export function useSelectedNode() {
  const selectedNodeId = useGraphStore((s) => s.selectedNodeId);
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);

  return useQuery<InspectResponse | null>({
    queryKey: [
      'inspect',
      selectedNodeId,
      apiUrl,
      connectionRevision,
      USE_MOCKS ? 'mock' : 'live',
    ],
    enabled: selectedNodeId !== null,
    queryFn: async ({ signal }) => {
      if (!selectedNodeId) return null;
      if (USE_MOCKS) {
        await new Promise((r) => setTimeout(r, 25));
        const { getMockInspect } = await import('../api/mocks');
        return getMockInspect(selectedNodeId);
      }
      return fetchInspect(selectedNodeId, { signal });
    },
    staleTime: 30_000,
  });
}
