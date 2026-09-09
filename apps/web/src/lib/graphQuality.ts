// What the render-quality choice decides for the memory viewport.
//
// Kept out of GraphView so the numbers can be read and tested without pulling
// in the renderer, and so there is one place to look when asking what
// "Advanced" actually costs.
//
// The optimized numbers are the tuned defaults measured in the 0.12.1 perf
// pass — the 2D graph went from 51fps at 17% of a core to 165fps at 1.9% — and
// are what every machine got before this was a choice. Advanced lifts each cap
// for a machine with the GPU to spare. Nothing here changes what the viewport
// shows or how it navigates, only how richly it is drawn.

import type { RenderQuality } from '../store/themeStore';

/**
 * Roughly how many particles the 2D canvas can animate before the render loop
 * stops being free. Every particle is drawn every frame, forever — unlike the
 * force layout, this work never settles, so it is the one effect that has to be
 * budgeted against graph size rather than switched on flat.
 */
export const PARTICLE_BUDGET = 260;

/**
 * Above this many nodes the optimized profile trades sphere smoothness for
 * frame time. Chosen to sit under a realistic library rather than a demo one.
 */
export const LARGE_GRAPH_NODES = 250;

export interface QualityProfile {
  /**
   * Particle budget for the whole graph, or `null` for no budget — meaning
   * every edge carries its own flow rather than a deterministic subset of the
   * strongest ones.
   */
  particleBudget: number | null;
  /** Node count past which 3D spheres get coarser, or `null` to never coarsen. */
  largeGraphNodes: number | null;
  /**
   * Fraction of canvas resolution the 3D bloom is computed at. UnrealBloom runs
   * a bright-pass plus five blur mips every frame, so its cost scales with the
   * pixel count.
   */
  bloomResolutionScale: number;
  /**
   * Whether the grouped overview pins nodes to a few shallow z-planes. Doing so
   * keeps groups legible from the opening camera angle; not doing so gives the
   * overview the same depth the individual-memory view has always had.
   */
  flattenGroupedOverview: boolean;
  /**
   * Whether glow and flow survive into the grouped overview. Optimized drops
   * them there because the overview is the densest thing the viewport draws.
   */
  effectsInGroupedOverview: boolean;
}

const QUALITY_PROFILES: Record<RenderQuality, QualityProfile> = {
  optimized: {
    particleBudget: PARTICLE_BUDGET,
    largeGraphNodes: LARGE_GRAPH_NODES,
    bloomResolutionScale: 0.5,
    flattenGroupedOverview: true,
    effectsInGroupedOverview: false,
  },
  advanced: {
    particleBudget: null,
    largeGraphNodes: null,
    bloomResolutionScale: 1,
    flattenGroupedOverview: false,
    effectsInGroupedOverview: true,
  },
};

export function qualityProfile(quality: RenderQuality): QualityProfile {
  return QUALITY_PROFILES[quality];
}
