// Zustand store for client-only UI state. Server state (the graph itself)
// lives in TanStack Query — see hooks/useGraphData.ts.

import { create } from 'zustand';
import type { NodeKind } from '../api/types';

export type ViewMode = '2d' | '3d';

/** Stable display identity for Community's one physical Memory Library. */
export const COMMUNITY_LIBRARY_NAME = 'Community Memory Library';

export interface GraphState {
  /** Currently selected node id (clicked in the graph). */
  selectedNodeId: string | null;
  /** Render mode toggle. */
  viewMode: ViewMode;
  /** Set of node kinds that are *visible* in the rendered graph. */
  visibleKinds: Set<NodeKind>;
  /** Free-text search query (client-side fuzzy filter against node labels). */
  searchQuery: string;
  /**
   * Node IDs the user has "expanded" via double-click. Expanded nodes pull
   * their immediate neighbors into the visible set even when those neighbors'
   * kinds are filtered off — so e.g. double-clicking a document reveals its
   * chunks regardless of whether the `chunk` kind toggle is on.
   *
   * Stubbed against already-loaded edges; will be backed by a
   * `GET /v1/graph/expand` endpoint later.
   */
  expandedNodeIds: Set<string>;

  /**
   * Node IDs connected agents have recently touched via memory tools
   * (memory_recall, memory_facts_about, memory_inspect, etc.). Rendered
   * with a distinct ring color in the graph view so the user can SEE
   * what the agent is reading as it reasons.
   *
   * Populated by graph-inspection actions such as "Show similar" so related
   * nodes stay visually highlighted while the user explores memory context.
   */
  recalledNodeIds: Set<string>;
  /** Last graph invalidation timestamp observed from Solo's SSE stream. */
  lastGraphInvalidateAtMs: number | null;

  // actions
  setSelectedNodeId: (id: string | null) => void;
  setViewMode: (mode: ViewMode) => void;
  toggleKind: (kind: NodeKind) => void;
  setSearchQuery: (q: string) => void;
  toggleExpansion: (nodeId: string) => void;
  clearExpansions: () => void;
  addRecalled: (ids: Iterable<string>) => void;
  clearRecalled: () => void;
  markGraphInvalidated: (atMs?: number) => void;
}

const DEFAULT_VISIBLE_KINDS: Set<NodeKind> = new Set([
  'episode',
  'document',
  'cluster',
  'entity',
  // 'chunk' off by default (per scoping doc §3 — Decision A)
]);

export const useGraphStore = create<GraphState>((set) => ({
  selectedNodeId: null,
  viewMode: '2d',
  visibleKinds: DEFAULT_VISIBLE_KINDS,
  searchQuery: '',
  expandedNodeIds: new Set<string>(),
  recalledNodeIds: new Set<string>(),
  lastGraphInvalidateAtMs: null,

  setSelectedNodeId: (id) => set({ selectedNodeId: id }),
  setViewMode: (mode) => set({ viewMode: mode }),
  toggleKind: (kind) =>
    set((state) => {
      const next = new Set(state.visibleKinds);
      if (next.has(kind)) {
        next.delete(kind);
      } else {
        next.add(kind);
      }
      return { visibleKinds: next };
    }),
  setSearchQuery: (q) => set({ searchQuery: q }),
  toggleExpansion: (nodeId) =>
    set((state) => {
      const next = new Set(state.expandedNodeIds);
      if (next.has(nodeId)) {
        next.delete(nodeId);
      } else {
        next.add(nodeId);
      }
      return { expandedNodeIds: next };
    }),
  clearExpansions: () => set({ expandedNodeIds: new Set() }),
  addRecalled: (ids) =>
    set((state) => {
      const next = new Set(state.recalledNodeIds);
      for (const id of ids) next.add(id);
      return { recalledNodeIds: next };
    }),
  clearRecalled: () => set({ recalledNodeIds: new Set() }),
  markGraphInvalidated: (atMs = Date.now()) => set({ lastGraphInvalidateAtMs: atMs }),
}));
