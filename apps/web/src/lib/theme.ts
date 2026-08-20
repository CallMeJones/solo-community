// Theme registry.
//
// Themes are expressed as CSS custom properties in index.css, selected by a
// `data-theme` attribute on <html>. This module holds the parts a theme needs
// that CSS cannot reach: the canvas colors for the force graph (drawn to a
// bitmap, not styled by CSS) and the metadata the settings picker renders.
//
// Adding a theme means adding an entry here AND a `:root[data-theme='…']`
// token block in index.css. Keep the two in sync — the id is the contract.

export type ThemeId = 'dune' | 'dark' | 'light' | 'blue';

export const DEFAULT_THEME_ID: ThemeId = 'dune';

/**
 * Colors the graph canvas paints directly. These cannot be Tailwind classes:
 * react-force-graph renders to a <canvas>, so every color is a literal passed
 * to the 2D context or the WebGL scene.
 */
export interface GraphPalette {
  /** Canvas clear color (3D scene background; 2D inherits the surface). */
  background: string;
  /** Node label text drawn under each node in 2D mode. */
  nodeLabel: string;
  /** Stroke around the selected node — must contrast with the background. */
  nodeOutline: string;
  /** Glow tint layered under nodes when graph effects are on. */
  glow: string;
  /**
   * UnrealBloom settings for the 3D view, or null to skip bloom entirely.
   *
   * Bloom brightens whatever already exceeds `threshold`, which is why the
   * light theme opts out: on a near-white backdrop the *background* is the
   * brightest thing on screen, so any threshold low enough to catch the nodes
   * blows out the whole canvas. The 2D view has no such problem — its glow is
   * a shadow pass that can be tinted dark.
   */
  bloom: { strength: number; radius: number; threshold: number } | null;
}

export type GraphLinkKind = 'triple' | 'cluster_member' | 'document_chunk' | 'semantic';

export interface ThemeDefinition {
  id: ThemeId;
  label: string;
  description: string;
  /** Drives the native `color-scheme` so form controls and scrollbars match. */
  colorScheme: 'dark' | 'light';
  /** Three representative colors for the picker preview: surface, panel, accent. */
  swatch: readonly [string, string, string];
  graph: GraphPalette;
}

export const THEMES: Record<ThemeId, ThemeDefinition> = {
  dune: {
    id: 'dune',
    label: 'Dune',
    description: 'Warm near-black with spice copper accents.',
    colorScheme: 'dark',
    swatch: ['#120d09', '#2d1f13', '#d28a3a'],
    graph: {
      background: '#080604',
      nodeLabel: '#f7ead1',
      nodeOutline: '#ffffff',
      glow: 'rgba(242, 179, 93, 0.30)',
      bloom: { strength: 0.72, radius: 0.55, threshold: 0.26 },
    },
  },
  dark: {
    id: 'dark',
    label: 'Dark',
    description: 'Neutral greys with no color cast.',
    colorScheme: 'dark',
    swatch: ['#0c0c0d', '#1c1c1f', '#e6e6e8'],
    graph: {
      background: '#0c0c0d',
      nodeLabel: '#ededef',
      nodeOutline: '#ffffff',
      glow: 'rgba(255, 255, 255, 0.22)',
      bloom: { strength: 0.66, radius: 0.5, threshold: 0.28 },
    },
  },
  light: {
    id: 'light',
    label: 'Light',
    description: 'Bright neutral surfaces for well-lit rooms.',
    colorScheme: 'light',
    swatch: ['#f7f7f8', '#ffffff', '#18181b'],
    graph: {
      background: '#f7f7f8',
      nodeLabel: '#27272a',
      nodeOutline: '#18181b',
      glow: 'rgba(24, 24, 27, 0.16)',
      bloom: null,
    },
  },
  blue: {
    id: 'blue',
    label: 'Dark Blue',
    description: 'Cool slate blues with a sky accent.',
    colorScheme: 'dark',
    swatch: ['#020617', '#0f172a', '#0369a1'],
    graph: {
      background: '#020617',
      nodeLabel: '#e2e8f0',
      nodeOutline: '#ffffff',
      glow: 'rgba(56, 189, 248, 0.28)',
      bloom: { strength: 0.82, radius: 0.6, threshold: 0.22 },
    },
  },
};

/** Display order for the settings picker. */
export const THEME_ORDER: readonly ThemeId[] = ['dune', 'dark', 'light', 'blue'];

export function isThemeId(value: unknown): value is ThemeId {
  return typeof value === 'string' && Object.prototype.hasOwnProperty.call(THEMES, value);
}

export function themeDefinition(id: ThemeId): ThemeDefinition {
  return THEMES[id] ?? THEMES[DEFAULT_THEME_ID];
}
