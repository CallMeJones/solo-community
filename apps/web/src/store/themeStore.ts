// Zustand store for appearance: theme, graph node palette, and graph effects.
//
// Kept separate from settingsStore on purpose: that store is about the Solo
// connection and carries credential-sanitizing migration logic that display
// preferences have no business passing through. These are plain, non-secret,
// persist-always values with their own keys.

import { create } from 'zustand';
import { DEFAULT_THEME_ID, isThemeId, themeDefinition, type ThemeId } from '../lib/theme';
import {
  DEFAULT_NODE_PALETTE_ID,
  isNodePaletteId,
  linkColorsFor,
  nodeColorsFor,
  particleColorsFor,
  type NodePaletteId,
} from '../lib/nodePalettes';

const THEME_KEY = 'solo.theme';
const PALETTE_KEY = 'solo.graph.palette';
const EFFECTS_KEY = 'solo.graph.effects';

export interface ThemeState {
  theme: ThemeId;
  nodePalette: NodePaletteId;
  /** Node glow and animated link particles in the graph. */
  effects: boolean;
  setTheme: (id: ThemeId) => void;
  setNodePalette: (id: NodePaletteId) => void;
  setEffects: (on: boolean) => void;
}

function readStored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    // Storage may be unavailable in sandboxed or private contexts.
    return null;
  }
}

function write(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Non-critical; the in-memory store remains usable for this session.
  }
}

function drop(key: string): void {
  try {
    localStorage.removeItem(key);
  } catch {
    // See above.
  }
}

function loadTheme(): ThemeId {
  const raw = readStored(THEME_KEY);
  if (isThemeId(raw)) return raw;
  // Discard values written by an older or newer build that this one cannot render.
  if (raw !== null) drop(THEME_KEY);
  return DEFAULT_THEME_ID;
}

function loadPalette(): NodePaletteId {
  const raw = readStored(PALETTE_KEY);
  if (isNodePaletteId(raw)) return raw;
  if (raw !== null) drop(PALETTE_KEY);
  return DEFAULT_NODE_PALETTE_ID;
}

/**
 * Effects default to on, except for visitors who have asked their OS for
 * reduced motion — the animated link particles are exactly what that setting
 * is about, so honour it unless the user has explicitly chosen otherwise here.
 */
function loadEffects(): boolean {
  const raw = readStored(EFFECTS_KEY);
  if (raw === '1') return true;
  if (raw === '0') return false;
  if (raw !== null) drop(EFFECTS_KEY);
  return !prefersReducedMotion();
}

export function prefersReducedMotion(): boolean {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return false;
  return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
}

/**
 * Writes the theme to <html> so it reaches everything: the utility overrides
 * in index.css key off `[data-theme]`, and `color-scheme` needs to be on the
 * root element for the browser to restyle form controls and scrollbars.
 */
export function applyTheme(id: ThemeId): void {
  if (typeof document === 'undefined') return;
  const root = document.documentElement;
  root.dataset.theme = id;
  root.style.colorScheme = themeDefinition(id).colorScheme;
}

const initialTheme = loadTheme();

// Applied at module load rather than in an effect so the first paint is already
// themed — an effect would flash the default theme for a frame on reload.
applyTheme(initialTheme);

export const useThemeStore = create<ThemeState>((set) => ({
  theme: initialTheme,
  nodePalette: loadPalette(),
  effects: loadEffects(),
  setTheme: (id) => {
    applyTheme(id);
    write(THEME_KEY, id);
    set({ theme: id });
  },
  setNodePalette: (id) => {
    write(PALETTE_KEY, id);
    set({ nodePalette: id });
  },
  setEffects: (on) => {
    write(EFFECTS_KEY, on ? '1' : '0');
    set({ effects: on });
  },
}));

/** The full definition for the active theme — canvas colors, labels, swatch. */
export function useActiveTheme() {
  return themeDefinition(useThemeStore((s) => s.theme));
}

/**
 * Node fills for the active palette on the active theme's surface. Keeps the
 * graph, the toolbar filters and the inspector badges in step.
 */
export function useNodeKindColors() {
  const scheme = useActiveTheme().colorScheme;
  const palette = useThemeStore((s) => s.nodePalette);
  return nodeColorsFor(palette, scheme);
}

/** Edge strokes derived from the same palette. */
export function useLinkKindColors() {
  const scheme = useActiveTheme().colorScheme;
  const palette = useThemeStore((s) => s.nodePalette);
  return linkColorsFor(palette, scheme);
}

/** Flow-particle fills derived from the same palette. */
export function useParticleColors() {
  const scheme = useActiveTheme().colorScheme;
  const palette = useThemeStore((s) => s.nodePalette);
  return particleColorsFor(palette, scheme);
}
