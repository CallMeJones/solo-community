/**
 * themeStore reads browser storage on module import, so the render-quality
 * cases each use a fresh module evaluation — the same reason settingsStore.test
 * does it.
 *
 * The profile assertions exist because "Advanced" is only worth having if it
 * actually lifts the caps. A regression that quietly left a budget in place
 * would look identical in the UI and cost nothing to miss.
 */

import { beforeEach, describe, expect, it, vi } from 'vitest';
import {
  LARGE_GRAPH_NODES,
  PARTICLE_BUDGET,
  qualityProfile,
} from '../src/lib/graphQuality';

async function importStore() {
  vi.resetModules();
  return (await import('../src/store/themeStore')).useThemeStore;
}

describe('render quality preference', () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it('defaults to the optimized profile', async () => {
    const store = await importStore();
    expect(store.getState().renderQuality).toBe('optimized');
  });

  it('remembers the choice', async () => {
    const store = await importStore();
    store.getState().setRenderQuality('advanced');
    expect(store.getState().renderQuality).toBe('advanced');
    expect(localStorage.getItem('solo.graph.quality')).toBe('advanced');

    const reloaded = await importStore();
    expect(reloaded.getState().renderQuality).toBe('advanced');
  });

  it('discards a value this build cannot render', async () => {
    // Written by an older or newer build. Falling back beats rendering nothing.
    localStorage.setItem('solo.graph.quality', 'ultra');
    const store = await importStore();
    expect(store.getState().renderQuality).toBe('optimized');
    expect(localStorage.getItem('solo.graph.quality')).toBeNull();
  });
});

describe('quality profiles', () => {
  it('keeps optimized exactly as it was tuned', () => {
    // These are the 0.12.1 perf-pass numbers. Optimized is the default every
    // machine gets, so changing one of these is a change to everyone's
    // viewport and should be a deliberate edit to this test too.
    expect(qualityProfile('optimized')).toEqual({
      particleBudget: PARTICLE_BUDGET,
      largeGraphNodes: LARGE_GRAPH_NODES,
      bloomResolutionScale: 0.5,
      flattenGroupedOverview: true,
      effectsInGroupedOverview: false,
    });
  });

  it('lifts every cap in advanced', () => {
    const advanced = qualityProfile('advanced');
    // null, not a bigger number: every edge flows rather than a subset.
    expect(advanced.particleBudget).toBeNull();
    expect(advanced.largeGraphNodes).toBeNull();
    expect(advanced.bloomResolutionScale).toBe(1);
    // The two the grouped overview cares about — this is where the modes look
    // most different, because the overview is where people spend their time.
    expect(advanced.flattenGroupedOverview).toBe(false);
    expect(advanced.effectsInGroupedOverview).toBe(true);
  });
});
