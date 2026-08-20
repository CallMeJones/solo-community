import type { NodeKind } from '../api/types';

// Node colors moved to lib/nodePalettes.ts when palettes became selectable —
// they now vary by palette and by light/dark surface. Sizes stay here: they are
// structural, not thematic.
export const NODE_KINDS: NodeKind[] = ['episode', 'document', 'chunk', 'cluster', 'entity'];

export const NODE_KIND_SIZES: Record<NodeKind, number> = {
  episode: 4,
  document: 8,
  chunk: 2,
  cluster: 10,
  entity: 6,
};
