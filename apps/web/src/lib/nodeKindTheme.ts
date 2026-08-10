import type { NodeKind } from '../api/types';

export const NODE_KINDS: NodeKind[] = ['episode', 'document', 'chunk', 'cluster', 'entity'];

export const NODE_KIND_COLORS: Record<NodeKind, string> = {
  episode: '#f2b35d',
  document: '#d56f3e',
  chunk: '#b487ff',
  cluster: '#65d6a3',
  entity: '#f7df8a',
};

export const NODE_KIND_SIZES: Record<NodeKind, number> = {
  episode: 4,
  document: 8,
  chunk: 2,
  cluster: 10,
  entity: 6,
};
