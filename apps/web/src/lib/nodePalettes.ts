// Node color palettes for the memory graph.
//
// Independent of the theme: the theme controls the app chrome and the graph's
// backdrop, a palette controls what the nodes and their links are colored. Any
// palette can pair with any theme, so each one carries a `dark` and a `light`
// variant — the same hue family, retuned for the surface it sits on. The
// dark-surface values are far too pale to read against white.
//
// Link colors are derived rather than declared: an edge takes the color of the
// node kind it represents, so recoloring the nodes recolors the whole graph
// coherently. See LINK_SOURCE below.

import type { NodeKind } from '../api/types';
import type { GraphLinkKind } from './theme';

export type NodePaletteId = 'spice' | 'aurora' | 'ember' | 'ocean' | 'neon' | 'mono';

export const DEFAULT_NODE_PALETTE_ID: NodePaletteId = 'spice';

export type NodeColors = Record<NodeKind, string>;

export interface NodePalette {
  id: NodePaletteId;
  label: string;
  description: string;
  dark: NodeColors;
  light: NodeColors;
}

/**
 * Which node kind lends its color to each edge kind, and at what opacity.
 * A `triple` is a fact relationship between clustered memories, so it reads in
 * the cluster color; a `cluster_member` edge ties an episode to its cluster, so
 * it takes the episode color; and so on.
 */
const LINK_SOURCE: Record<GraphLinkKind, { kind: NodeKind; alpha: number }> = {
  triple: { kind: 'cluster', alpha: 0.9 },
  cluster_member: { kind: 'episode', alpha: 0.58 },
  document_chunk: { kind: 'document', alpha: 0.72 },
  semantic: { kind: 'chunk', alpha: 0.72 },
};

export const NODE_PALETTES: Record<NodePaletteId, NodePalette> = {
  spice: {
    id: 'spice',
    label: 'Spice',
    description: 'Okabe-Ito derived. The colorblind-safe default.',
    dark: {
      episode: '#f2b35d',
      document: '#d56f3e',
      chunk: '#b487ff',
      cluster: '#65d6a3',
      entity: '#f7df8a',
    },
    light: {
      episode: '#b06f16',
      document: '#b4501f',
      chunk: '#7541d1',
      cluster: '#0f7a54',
      entity: '#8a6d0f',
    },
  },
  aurora: {
    id: 'aurora',
    label: 'Aurora',
    description: 'Cool cyans and indigos with a warm counterpoint.',
    dark: {
      episode: '#22d3ee',
      document: '#818cf8',
      chunk: '#f472b6',
      cluster: '#4ade80',
      entity: '#fde047',
    },
    light: {
      episode: '#0e7490',
      document: '#4338ca',
      chunk: '#be185d',
      cluster: '#15803d',
      entity: '#a16207',
    },
  },
  ember: {
    id: 'ember',
    label: 'Ember',
    description: 'Hot oranges and crimsons over a violet base.',
    dark: {
      episode: '#fb923c',
      document: '#f43f5e',
      chunk: '#c084fc',
      cluster: '#fbbf24',
      entity: '#fda4af',
    },
    light: {
      episode: '#c2410c',
      document: '#be123c',
      chunk: '#7e22ce',
      cluster: '#a16207',
      entity: '#9f1239',
    },
  },
  ocean: {
    id: 'ocean',
    label: 'Ocean',
    description: 'Deep teals and blues with a sand highlight.',
    dark: {
      episode: '#38bdf8',
      document: '#2dd4bf',
      chunk: '#a78bfa',
      cluster: '#34d399',
      entity: '#fcd34d',
    },
    light: {
      episode: '#0369a1',
      document: '#0f766e',
      chunk: '#6d28d9',
      cluster: '#047857',
      entity: '#a16207',
    },
  },
  neon: {
    id: 'neon',
    label: 'Neon',
    description: 'Maximum saturation. Best on the darker themes.',
    dark: {
      episode: '#f0abfc',
      document: '#22d3ee',
      chunk: '#a3e635',
      cluster: '#fb7185',
      entity: '#fcd34d',
    },
    light: {
      episode: '#a21caf',
      document: '#0e7490',
      chunk: '#4d7c0f',
      cluster: '#e11d48',
      entity: '#b45309',
    },
  },
  mono: {
    id: 'mono',
    label: 'Mono',
    description: 'Greyscale steps. Shape and size carry the meaning.',
    dark: {
      episode: '#d4d4d8',
      document: '#fafafa',
      chunk: '#71717a',
      cluster: '#a1a1aa',
      entity: '#e4e4e7',
    },
    light: {
      episode: '#52525b',
      document: '#18181b',
      chunk: '#a1a1aa',
      cluster: '#71717a',
      entity: '#3f3f46',
    },
  },
};

/** Display order for the settings picker. */
export const NODE_PALETTE_ORDER: readonly NodePaletteId[] = [
  'spice',
  'aurora',
  'ember',
  'ocean',
  'neon',
  'mono',
];

export function isNodePaletteId(value: unknown): value is NodePaletteId {
  return typeof value === 'string' && Object.prototype.hasOwnProperty.call(NODE_PALETTES, value);
}

export function nodePalette(id: NodePaletteId): NodePalette {
  return NODE_PALETTES[id] ?? NODE_PALETTES[DEFAULT_NODE_PALETTE_ID];
}

/** Node fills for a palette on a given surface. */
export function nodeColorsFor(id: NodePaletteId, scheme: 'dark' | 'light'): NodeColors {
  return nodePalette(id)[scheme];
}

/** Edge strokes derived from the same palette — see LINK_SOURCE. */
export function linkColorsFor(
  id: NodePaletteId,
  scheme: 'dark' | 'light',
): Record<GraphLinkKind, string> {
  const nodes = nodeColorsFor(id, scheme);
  return {
    triple: withAlpha(nodes[LINK_SOURCE.triple.kind], LINK_SOURCE.triple.alpha),
    cluster_member: withAlpha(
      nodes[LINK_SOURCE.cluster_member.kind],
      LINK_SOURCE.cluster_member.alpha,
    ),
    document_chunk: withAlpha(
      nodes[LINK_SOURCE.document_chunk.kind],
      LINK_SOURCE.document_chunk.alpha,
    ),
    semantic: withAlpha(nodes[LINK_SOURCE.semantic.kind], LINK_SOURCE.semantic.alpha),
  };
}

/**
 * Flow-particle fills. Brighter than the edges they travel along so the motion
 * reads against its own line rather than blending into it.
 */
export function particleColorsFor(
  id: NodePaletteId,
  scheme: 'dark' | 'light',
): Record<GraphLinkKind, string> {
  const nodes = nodeColorsFor(id, scheme);
  return {
    triple: nodes[LINK_SOURCE.triple.kind],
    cluster_member: nodes[LINK_SOURCE.cluster_member.kind],
    document_chunk: nodes[LINK_SOURCE.document_chunk.kind],
    semantic: nodes[LINK_SOURCE.semantic.kind],
  };
}

/** `#rrggbb` plus an alpha, as an `rgba()` string the canvas accepts. */
export function withAlpha(hex: string, alpha: number): string {
  const value = hex.replace('#', '');
  const full =
    value.length === 3
      ? value
          .split('')
          .map((c) => c + c)
          .join('')
      : value;
  const int = Number.parseInt(full, 16);
  if (Number.isNaN(int)) return hex;
  const r = (int >> 16) & 255;
  const g = (int >> 8) & 255;
  const b = int & 255;
  return `rgba(${r}, ${g}, ${b}, ${alpha})`;
}
