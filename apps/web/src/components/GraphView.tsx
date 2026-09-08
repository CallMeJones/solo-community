// 2D / 3D force-directed graph canvas.
// Uses react-force-graph-2d and react-force-graph-3d (both by @vasturiano)
// — near-identical APIs, swap based on the viewMode setting.

import {
  lazy,
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentType,
} from 'react';
import { ArrowsOut, Plus, Minus } from '@phosphor-icons/react';
import { useGraphData } from '../hooks/useGraphData';
import { useGraphStore } from '../store/graphStore';
import { NODE_KIND_SIZES } from '../lib/nodeKindTheme';
import { withAlpha } from '../lib/nodePalettes';
import {
  useActiveTheme,
  useLinkKindColors,
  useNodeKindColors,
  useParticleColors,
  useThemeStore,
} from '../store/themeStore';
import {
  buildGraphPresentation,
  createGraphTooltip,
  describeGraphEdge,
  describeGraphNode,
  documentIdForSummary,
  type PresentedGraphLink,
  type PresentedGraphNode,
  type PresentedGraph,
} from '../lib/graphPresentation';

const ForceGraph2D = lazy(() => import('react-force-graph-2d')) as ComponentType<
  Record<string, unknown>
>;
const ForceGraph3D = lazy(() => import('react-force-graph-3d')) as ComponentType<
  Record<string, unknown>
>;

interface ForceGraphNode extends PresentedGraphNode {
  // Force-graph adds these at runtime; we type them as optional so we can read them safely.
  x?: number;
  y?: number;
  z?: number;
  __drawRadius?: number;
}

type ForceGraphLink = PresentedGraphLink;

/** The slice of three's EffectComposer this component uses. */
interface EffectComposerLike {
  addPass: (pass: BloomPassLike) => void;
  removePass?: (pass: BloomPassLike) => void;
}

interface BloomPassLike {
  dispose?: () => void;
}

/** The slice of the ForceGraph3D imperative handle this component uses. */
interface ForceGraph3DHandle {
  camera?: () => { fov: number };
  cameraPosition?: (
    position: { x: number; y: number; z: number },
    lookAt: { x: number; y: number; z: number },
    duration: number,
  ) => void;
  getGraphBbox?: () => { x: [number, number]; y: [number, number]; z: [number, number] } | null;
  d3Force?: (name: string) => {
    strength?: (value: number) => void;
    distance?: (value: number) => void;
  };
  zoomToFit?: (duration?: number, padding?: number) => void;
  zoom?: (scale?: number, duration?: number) => number;
  postProcessingComposer?: () => EffectComposerLike | undefined;
}

function graphLinkWidth(link: ForceGraphLink): number {
  switch (link.kind) {
    case 'triple':
      return 2.2;
    case 'cluster_member':
      return 1.4;
    case 'document_chunk':
      return 1.8;
    case 'semantic':
      return 1.2;
  }
}

/**
 * Roughly how many particles the 2D canvas can animate before the render loop
 * stops being free. Every particle is drawn every frame, forever — unlike the
 * force layout, this work never settles, so it is the one effect that has to be
 * budgeted against graph size rather than switched on flat.
 */
const PARTICLE_BUDGET = 260;

/**
 * Particles per edge, weighted so the strongest relationships read as the
 * busiest, then thinned to keep the total near [`PARTICLE_BUDGET`].
 *
 * Returns 0 wholesale when effects are off — force-graph skips the per-frame
 * particle work entirely at 0, which is the point of the toggle.
 */
function graphLinkParticleCount(link: ForceGraphLink, effects: boolean, linkCount: number): number {
  if (!effects) return 0;
  const weight = link.kind === 'triple' ? 3 : link.kind === 'semantic' ? 1 : 2;
  if (linkCount <= 0) return weight;

  // Average weight is ~2, so this is the fraction of edges that can carry one.
  const share = PARTICLE_BUDGET / (linkCount * 2);
  if (share >= 1) return weight;
  // Below budget, keep particles only on the strongest edges and only on a
  // deterministic subset of them, so the flow still reads without every edge
  // paying for it. linkSeed is stable, so the chosen subset does not flicker.
  if (link.kind !== 'triple') return 0;
  return linkSeed(link) < share * 2 ? 1 : 0;
}

function graphLinkParticleWidth(link: ForceGraphLink): number {
  return link.kind === 'triple' ? 2.8 : 2;
}

/**
 * Above this many nodes the 3D view trades sphere smoothness for frame time.
 * Chosen to sit under a realistic library rather than a demo one.
 */
const LARGE_GRAPH_NODES = 250;

/**
 * Fraction of the canvas resolution the 3D bloom is computed at. Halving each
 * axis quarters the pixels the blur mips touch.
 */
const BLOOM_RESOLUTION_SCALE = 0.5;

/** How far the halo extends past the node, as a multiple of its radius. */
const GLOW_SPREAD = 2.6;

/** Pixel size of the cached halo. Soft edges tolerate being scaled. */
const GLOW_SPRITE_PX = 96;

/**
 * One pre-rendered halo per colour.
 *
 * There are five node kinds, so this settles at five small canvases no matter
 * how large the graph is, and each frame becomes a `drawImage` per node rather
 * than a fresh gaussian blur.
 */
const glowSprites = new Map<string, HTMLCanvasElement | null>();

function glowSprite(color: string): HTMLCanvasElement | null {
  const cached = glowSprites.get(color);
  if (cached !== undefined) return cached;

  let sprite: HTMLCanvasElement | null = null;
  if (typeof document !== 'undefined') {
    const canvas = document.createElement('canvas');
    canvas.width = GLOW_SPRITE_PX;
    canvas.height = GLOW_SPRITE_PX;
    const g = canvas.getContext('2d');
    if (g) {
      const c = GLOW_SPRITE_PX / 2;
      const gradient = g.createRadialGradient(c, c, 0, c, c, c);
      // Opaque core, then a fast falloff — a linear fade reads as a flat disc.
      gradient.addColorStop(0, withAlpha(color, 0.55));
      gradient.addColorStop(1 / GLOW_SPREAD, withAlpha(color, 0.28));
      gradient.addColorStop(1, withAlpha(color, 0));
      g.fillStyle = gradient;
      g.fillRect(0, 0, GLOW_SPRITE_PX, GLOW_SPRITE_PX);
      sprite = canvas;
    }
  }
  glowSprites.set(color, sprite);
  return sprite;
}

/**
 * force-graph mutates `source`/`target` from id strings into node object
 * references once the data is ingested, so an accessor has to read both shapes.
 */
function endpointId(endpoint: unknown): string {
  if (typeof endpoint === 'string') return endpoint;
  const node = endpoint as { id?: unknown } | null;
  return typeof node?.id === 'string' ? node.id : '';
}

/**
 * A stable pseudo-random number in [0, 1) per edge (FNV-1a over its identity).
 * Stable matters: the value feeds particle phase and speed, and re-deriving a
 * different number on re-render would make the whole graph visibly jump.
 */
function linkSeed(link: ForceGraphLink): number {
  const key = `${endpointId(link.source)}>${endpointId(link.target)}:${link.kind}`;
  let hash = 2166136261;
  for (let i = 0; i < key.length; i += 1) {
    hash ^= key.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return ((hash >>> 0) % 100000) / 100000;
}

/**
 * Starting phase, as a fraction of the gap between one particle and the next.
 * Without it every edge starts its cycle at exactly the same moment and the
 * whole graph pulses in lockstep.
 */
function graphLinkParticleOffset(link: ForceGraphLink): number {
  return linkSeed(link);
}

/**
 * Per-edge speed spread over roughly a 3x range. The offset above scatters the
 * starting phase; varying the speed keeps edges from drifting back into sync.
 */
function graphLinkParticleSpeed(link: ForceGraphLink): number {
  return 0.0035 + linkSeed(link) * 0.007;
}

export function GraphView({
  presentation,
  onOpenGroup,
  onFocusNode,
}: {
  presentation?: PresentedGraph;
  onOpenGroup?: (id: string) => void;
  onFocusNode?: (id: string) => void;
} = {}) {
  const { data, isLoading, error } = useGraphData();
  const viewMode = useGraphStore((s) => s.viewMode);
  const visibleKinds = useGraphStore((s) => s.visibleKinds);
  const searchQuery = useGraphStore((s) => s.searchQuery);
  const selectedNodeId = useGraphStore((s) => s.selectedNodeId);
  const setSelectedNodeId = useGraphStore((s) => s.setSelectedNodeId);
  const expandedNodeIds = useGraphStore((s) => s.expandedNodeIds);
  const toggleExpansion = useGraphStore((s) => s.toggleExpansion);
  const recalledNodeIds = useGraphStore((s) => s.recalledNodeIds);
  // Canvas colors come from the theme registry, not CSS: force-graph paints to
  // a bitmap, so nothing here is reachable by a stylesheet.
  const palette = useActiveTheme().graph;
  const nodeColors = useNodeKindColors();
  const linkColors = useLinkKindColors();
  const particleColors = useParticleColors();
  const requestedEffects = useThemeStore((s) => s.effects);
  const effects = requestedEffects && !onOpenGroup;
  const labels = useThemeStore((s) => s.labels);

  // Container ref for sizing — ResizeObserver-backed so dimensions track
  // the actual painted canvas area, not a stale first-render snapshot.
  // (The old code read `containerRef.current?.clientWidth ?? 800` during
  // render — on first render the ref is null, so the canvas got 800x600
  // regardless of viewport; the force layout then settled inside those
  // wrong bounds and visibly clipped on the right edge.)
  const containerRef = useRef<HTMLDivElement>(null);
  const fg2dRef = useRef<ForceGraph3DHandle | null>(null);
  const labelBoxes = useRef<Array<[number, number, number, number]>>([]);
  const fitted = useRef(false);
  const forceConfigured = useRef(false);
  const fg3dRef = useRef<ForceGraph3DHandle | null>(null);
  const [dimensions, setDimensions] = useState({ width: 0, height: 0 });
  const [SpriteText, setSpriteText] = useState<typeof import('three-spritetext').default | null>(
    null,
  );
  useEffect(() => {
    if (viewMode !== '3d') return;
    let active = true;
    void import('three-spritetext').then((module) => {
      if (active) setSpriteText(() => module.default);
    });
    return () => {
      active = false;
    };
  }, [viewMode]);
  const groupColors = Object.values(nodeColors);
  const groupColor = (id: string) =>
    groupColors[
      Array.from(id).reduce((hash, c) => (Math.imul(hash, 31) + c.charCodeAt(0)) >>> 0, 0) %
        groupColors.length
    ];
  const label3d = useCallback(
    (node: ForceGraphNode) => {
      if (!SpriteText || (!onOpenGroup && node.id !== selectedNodeId)) return undefined;
      const sprite = new SpriteText(
        `${node.label.slice(0, 40)}${onOpenGroup ? `\n${node.ref_count ?? 0} items` : ''}`,
        onOpenGroup ? 12 : 8,
        palette.nodeLabel,
      );
      sprite.backgroundColor = palette.background;
      // Keep labels readable as the user orbits through near and far groups.
      // SpriteText inherits this Three material at runtime; Three is untyped
      // in this app, so describe only the material property we use.
      (sprite as unknown as { material: { sizeAttenuation: boolean } }).material.sizeAttenuation =
        false;
      sprite.textHeight = 0.016;
      sprite.offsetY = -0.045;
      sprite.padding = 0.003;
      sprite.borderRadius = 0.002;
      return sprite;
    },
    [SpriteText, onOpenGroup, selectedNodeId, palette],
  );

  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!el) return;

    // Seed with current size synchronously (avoids a frame of 0×0 rendering
    // before the first ResizeObserver callback fires).
    setDimensions({ width: el.clientWidth, height: el.clientHeight });

    const observer = new ResizeObserver((entries) => {
      const entry = entries[0];
      if (!entry) return;
      const { width: w, height: h } = entry.contentRect;
      setDimensions({ width: w, height: h });
    });

    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const filtered = useMemo(() => {
    if (presentation) return presentation;
    if (!data) return { nodes: [] as ForceGraphNode[], links: [] as ForceGraphLink[] };
    return buildGraphPresentation(data, visibleKinds, expandedNodeIds, searchQuery);
  }, [data, visibleKinds, searchQuery, expandedNodeIds, presentation]);

  const { width, height } = dimensions;

  // A fresh object literal here would be a new `graphData` prop on every
  // render, and force-graph re-ingests the whole graph when that identity
  // changes — reheating the layout each time the status strip or a store value
  // ticks. `filtered` is already memoised; this keeps the wrapper stable too.
  const grouped = Boolean(onOpenGroup);
  const graphData = useMemo(
    () => ({
      // A shallow overview keeps groups readable from the initial camera angle.
      // Individual-memory neighborhoods retain the full three-dimensional layout.
      nodes: filtered.nodes.map((node, i) => ({
        ...node,
        ...(grouped ? { fz: ((i % 3) - 1) * 25 } : {}),
      })),
      links: filtered.links.map((link) => ({ ...link })),
    }),
    [filtered, grouped],
  );
  const linkCount = filtered.links.length;
  useEffect(() => {
    fitted.current = false;
    forceConfigured.current = false;
  }, [filtered, viewMode]);
  const configureForces = () => {
    if (forceConfigured.current) return;
    const fg = (viewMode === '2d' ? fg2dRef : fg3dRef).current;
    if (!fg) return;
    fg.d3Force?.('charge')?.strength?.(onOpenGroup ? -550 : -80);
    fg.d3Force?.('link')?.distance?.(onOpenGroup ? 150 : 65);
    forceConfigured.current = true;
  };
  const fitGraph = useCallback(() => {
    if (viewMode === '2d') {
      fg2dRef.current?.zoomToFit?.(400, 80);
      return;
    }
    const fg = fg3dRef.current,
      box = fg?.getGraphBbox?.();
    if (!box) return;
    const center = {
      x: (box.x[0] + box.x[1]) / 2,
      y: (box.y[0] + box.y[1]) / 2,
      z: (box.z[0] + box.z[1]) / 2,
    };
    const tangent = Math.tan(((fg?.camera?.().fov ?? 50) * Math.PI) / 360);
    const halfHeight = (box.y[1] - box.y[0]) / 2 + 45;
    const halfWidth = (box.x[1] - box.x[0]) / 2 + 90;
    const distance =
      Math.max(halfHeight, halfWidth / (width / height)) / tangent + (box.z[1] - box.z[0]) / 2;
    fg?.cameraPosition?.({ ...center, z: center.z + distance }, center, 400);
  }, [viewMode, width, height]);
  useEffect(() => {
    if (!width || !height) return;
    fitted.current = false;
    const frame = requestAnimationFrame(fitGraph);
    return () => cancelAnimationFrame(frame);
  }, [fitGraph, width, height]);
  const fitOnce = () => {
    if (!fitted.current) {
      fitGraph();
      fitted.current = true;
    }
  };

  // 3D bloom. The 2D view gets its glow from a blurred canvas pass, which has
  // no equivalent in WebGL — there, glow is a post-processing stage on the
  // renderer. react-force-graph-3d exposes its EffectComposer, so an
  // UnrealBloomPass is appended to it.
  //
  // The pass is imported dynamically: this only runs in 3D mode, and a static
  // import would pull the postprocessing chunk into the 2D path too.
  useEffect(() => {
    const bloom = palette.bloom;
    if (viewMode !== '3d' || !effects || !bloom) return;

    let cancelled = false;
    let attached: { composer: EffectComposerLike; pass: BloomPassLike } | null = null;
    let frame = 0;
    let attempts = 0;

    const attach = () => {
      if (cancelled) return;
      const composer = fg3dRef.current?.postProcessingComposer?.();
      if (!composer) {
        // The composer only exists once the lazy 3D component has mounted and
        // built its renderer. Retry for a bounded number of frames rather than
        // racing Suspense.
        if (attempts++ < 180) frame = requestAnimationFrame(attach);
        return;
      }
      void import('three/examples/jsm/postprocessing/UnrealBloomPass.js').then(
        ({ UnrealBloomPass }) => {
          if (cancelled) return;
          // Half resolution. UnrealBloom runs a bright-pass plus five blur
          // mips every frame, so its cost scales with the pixel count — and a
          // glow is the one effect that loses nothing to being blurred at lower
          // resolution. Full-res bloom cost about five times the frame rate.
          const pass = new UnrealBloomPass(
            {
              x: Math.max(1, Math.round((width || 1) * BLOOM_RESOLUTION_SCALE)),
              y: Math.max(1, Math.round((height || 1) * BLOOM_RESOLUTION_SCALE)),
            },
            bloom.strength,
            bloom.radius,
            bloom.threshold,
          ) as unknown as BloomPassLike;
          // Keep the canvas transparent. As the composer's last pass,
          // UnrealBloomPass blits the rendered scene to the screen through an
          // opaque MeshBasicMaterial, which stamps alpha 1 across the whole
          // canvas and hides the CSS backdrop behind it. Marking that blit
          // material transparent carries the scene's own alpha through, so
          // empty space stays see-through and only the bloom adds light.
          // Guarded: if three renames the internal, the graph just renders on
          // an opaque background rather than breaking.
          const blit = (pass as unknown as { _basic?: { transparent: boolean } })._basic;
          if (blit) blit.transparent = true;

          composer.addPass(pass);
          attached = { composer, pass };
        },
      );
    };

    attach();

    return () => {
      cancelled = true;
      if (frame) cancelAnimationFrame(frame);
      if (attached) {
        attached.composer.removePass?.(attached.pass);
        attached.pass.dispose?.();
      }
    };
  }, [viewMode, effects, palette.bloom, width, height]);

  // Accessors are memoised because force-graph reconfigures itself whenever one
  // changes identity; recreating them each render made every unrelated re-render
  // touch the renderer.
  const graphLinkColor = useCallback((link: ForceGraphLink) => linkColors[link.kind], [linkColors]);
  const particleCount = useCallback(
    (link: ForceGraphLink) => graphLinkParticleCount(link, effects, linkCount),
    [effects, linkCount],
  );
  const particleColor = useCallback(
    (link: ForceGraphLink) => particleColors[link.kind],
    [particleColors],
  );
  const nodeColorFor = (node: ForceGraphNode) =>
    onOpenGroup ? groupColor(node.id) : nodeColors[node.kind];

  // Shared node-paint logic for 2D.
  const nodeCanvasObject = (
    node: ForceGraphNode,
    ctx: CanvasRenderingContext2D,
    globalScale: number,
  ) => {
    const x = node.x ?? 0;
    const y = node.y ?? 0;
    const isSelected = node.id === selectedNodeId;
    const isExpanded = expandedNodeIds.has(node.id);
    const isRecalled = recalledNodeIds.has(node.id);
    const isHighlighted = node.__highlighted;
    const baseSize = node.__aggregateForDocumentId
      ? 5
      : onOpenGroup
        ? 8 + Math.min(6, Math.log2((node.ref_count ?? 1) + 1) * 0.6)
        : NODE_KIND_SIZES[node.kind] * entityImportanceScale(node);
    const scaledSize = onOpenGroup
      ? Math.max(baseSize, (width < 500 ? 16 : 24) / globalScale)
      : isSelected
        ? baseSize * 1.6
        : baseSize;
    const size = Math.min(
      scaledSize,
      (onOpenGroup ? 32 : isSelected ? 14 : node.kind === 'cluster' ? 18 : 9) / globalScale,
    );
    node.__drawRadius = size;
    const color = onOpenGroup ? groupColor(node.id) : nodeColors[node.kind];

    // Glow. This used to set `shadowBlur` and fill a disc per node per frame,
    // which re-blurred on every one of them and cost about seven times the CPU
    // of a flat graph. The halo is the same for a given colour, so it is
    // rendered once into an offscreen sprite and blitted here instead.
    if (effects) {
      const halo = glowSprite(color);
      if (halo) {
        const r = size * GLOW_SPREAD;
        ctx.drawImage(halo, x - r, y - r, r * 2, r * 2);
        // A second blit deepens the bloom on the nodes the user is acting on.
        if (isSelected || isHighlighted || isRecalled) {
          ctx.drawImage(halo, x - r, y - r, r * 2, r * 2);
        }
      }
    }

    ctx.beginPath();
    ctx.arc(x, y, size, 0, 2 * Math.PI, false);
    ctx.fillStyle = onOpenGroup ? withAlpha(color, 0.16) : color;
    ctx.fill();
    if (onOpenGroup) {
      ctx.strokeStyle = color;
      ctx.lineWidth = 1.8 / globalScale;
      ctx.stroke();
    }

    if (node.__aggregateCount || onOpenGroup) {
      const fontSize = 13 / globalScale;
      ctx.font = `600 ${fontSize}px ui-sans-serif, system-ui, sans-serif`;
      ctx.fillStyle = '#fff7ed';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(String(node.__aggregateCount ?? node.ref_count ?? 0), x, y);
    }

    // Recall ring — emerald — drawn at radius+5 so it sits OUTSIDE the
    // expansion ring (radius+3) when both apply. Recall is the headline
    // chat-drawer signal: "the agent is reading this node RIGHT NOW".
    if (isRecalled) {
      ctx.beginPath();
      ctx.arc(x, y, size + 5, 0, 2 * Math.PI, false);
      ctx.lineWidth = 2 / globalScale;
      ctx.strokeStyle = 'rgba(16, 185, 129, 0.9)'; // emerald-500
      ctx.stroke();
    }

    // Expansion ring (drawn beneath selection/highlight strokes so they win on overlap).
    if (isExpanded) {
      ctx.beginPath();
      ctx.arc(x, y, size + 3, 0, 2 * Math.PI, false);
      ctx.lineWidth = 1.5 / globalScale;
      ctx.strokeStyle = 'rgba(96, 165, 250, 0.9)'; // blue-400
      ctx.stroke();
    }

    if (isSelected) {
      ctx.lineWidth = 2 / globalScale;
      ctx.strokeStyle = palette.nodeOutline;
      ctx.stroke();
    } else if (isHighlighted) {
      ctx.lineWidth = 1.5 / globalScale;
      ctx.strokeStyle = '#fbbf24'; // amber-400
      ctx.stroke();
    }

    // Keep structural labels discoverable while deferring dense memory labels
    // until the user zooms in. The Labels toolbar toggle overrides all of that:
    // off means off, including for the selected node, because the point of
    // turning it off is to see the shape of the graph rather than read it. The
    // hover tooltip and the inspector still name whatever is under the pointer.
    if (
      labels &&
      (isSelected || isHighlighted || onOpenGroup || shouldShowNodeLabel(node, globalScale))
    ) {
      const fontSize = 13 / globalScale;
      ctx.font = `${fontSize}px ui-sans-serif, system-ui, sans-serif`;
      ctx.fillStyle = palette.nodeLabel;
      ctx.textAlign = 'center';
      ctx.textBaseline = 'top';
      const text = node.label.length > 36 ? node.label.slice(0, 35) + '…' : node.label;
      const textWidth = ctx.measureText(text).width;
      const box: [number, number, number, number] = [
        x - textWidth / 2 - 3 / globalScale,
        y + size + 2,
        x + textWidth / 2 + 3 / globalScale,
        y + size + 2 + fontSize + 3 / globalScale,
      ];
      const overlaps = labelBoxes.current.some(
        (b) => box[0] < b[2] && box[2] > b[0] && box[1] < b[3] && box[3] > b[1],
      );
      if (!overlaps || isSelected) {
        labelBoxes.current.push(box);
        ctx.fillText(text, x, y + size + 2);
      }
    }
  };

  const handleNodeClick = (node: ForceGraphNode, event: MouseEvent) => {
    if (onOpenGroup) {
      onOpenGroup(node.id);
      return;
    }
    const aggregateDocumentId = documentIdForSummary(node);
    if (aggregateDocumentId) {
      setSelectedNodeId(aggregateDocumentId);
      if (!expandedNodeIds.has(aggregateDocumentId)) toggleExpansion(aggregateDocumentId);
      return;
    }
    if (event.detail === 2) {
      onFocusNode?.(node.id);
      toggleExpansion(node.id);
    } else {
      setSelectedNodeId(node.id);
    }
  };

  // Container is rendered unconditionally so the ResizeObserver (useLayoutEffect
  // with [] deps) attaches on first paint. Loading/error states render as
  // overlays inside it rather than as early returns, otherwise the ref is null
  // on first render, the effect bails out, and dimensions stay at 0×0 forever.
  return (
    <div ref={containerRef} className="solo-graph-canvas relative h-full w-full">
      {isLoading && (
        <div className="flex h-full items-center justify-center text-slate-400">
          Loading graph...
        </div>
      )}
      {error && (
        <div className="flex h-full items-center justify-center text-red-400">
          Failed to load graph: {String(error)}
        </div>
      )}
      {!isLoading &&
        !error &&
        (viewMode === '2d' ? (
          <ForceGraph2D
            ref={fg2dRef}
            onRenderFramePre={() => {
              labelBoxes.current = [];
            }}
            onEngineStop={fitOnce}
            onEngineTick={configureForces}
            graphData={graphData}
            width={width}
            height={height}
            // Transparent so the themed gradient painted by `.solo-graph-canvas`
            // on the container below shows through. The 3D view keeps a solid
            // clear color — WebGL composites its own scene.
            backgroundColor="rgba(0, 0, 0, 0)"
            nodeId="id"
            nodeLabel={(node: ForceGraphNode) => createGraphTooltip(describeGraphNode(node))}
            nodeCanvasObject={nodeCanvasObject}
            nodePointerAreaPaint={(
              node: ForceGraphNode,
              color: string,
              ctx: CanvasRenderingContext2D,
            ) => {
              const x = node.x ?? 0;
              const y = node.y ?? 0;
              const size =
                node.__drawRadius ??
                (node.__aggregateForDocumentId
                  ? 5
                  : NODE_KIND_SIZES[node.kind] * entityImportanceScale(node)) + 2;
              ctx.fillStyle = color;
              ctx.beginPath();
              ctx.arc(x, y, size, 0, 2 * Math.PI, false);
              ctx.fill();
            }}
            linkColor={graphLinkColor}
            linkLabel={(link: ForceGraphLink) => createGraphTooltip(describeGraphEdge(link))}
            linkWidth={graphLinkWidth}
            linkDirectionalParticles={particleCount}
            linkDirectionalParticleColor={particleColor}
            linkDirectionalParticleWidth={graphLinkParticleWidth}
            linkDirectionalParticleSpeed={graphLinkParticleSpeed}
            linkDirectionalParticleOffset={graphLinkParticleOffset}
            linkDirectionalArrowLength={(link: ForceGraphLink) => (link.kind === 'triple' ? 4 : 0)}
            linkDirectionalArrowRelPos={0.82}
            onNodeClick={(node: ForceGraphNode, event: MouseEvent) => {
              // event.detail === 2 means this click is part of a double-click;
              // the first click of the pair still fires with detail===1, so a
              // double-click selects AND expands — intentional.
              handleNodeClick(node, event);
            }}
            cooldownTicks={100}
          />
        ) : (
          <ForceGraph3D
            ref={fg3dRef}
            onEngineStop={fitOnce}
            onEngineTick={configureForces}
            cooldownTicks={100}
            graphData={graphData}
            // Geometry detail, not visual detail. Every node is a sphere and
            // every flow particle is another one, so segment counts multiply by
            // the graph size — the default 8x8 sphere is 128 triangles that a
            // node a few pixels wide cannot show. Dropped further once the
            // graph is large enough for the totals to matter.
            nodeResolution={filtered.nodes.length > LARGE_GRAPH_NODES ? 6 : 8}
            linkDirectionalParticleResolution={2}
            width={width}
            height={height}
            // Transparent for the same reason as the 2D canvas: the themed
            // gradient on the container behind it becomes the graph backdrop.
            backgroundColor="rgba(0, 0, 0, 0)"
            nodeId="id"
            nodeLabel={(node: ForceGraphNode) => createGraphTooltip(describeGraphNode(node))}
            nodeColor={nodeColorFor}
            nodeThreeObject={label3d}
            nodeThreeObjectExtend={true}
            linkOpacity={0.65}
            nodeVal={(n: ForceGraphNode) =>
              n.__aggregateForDocumentId
                ? 4
                : onOpenGroup
                  ? 15 + Math.log2((n.ref_count ?? 1) + 1) * 4
                  : NODE_KIND_SIZES[n.kind] * entityImportanceScale(n)
            }
            linkColor={graphLinkColor}
            linkLabel={(link: ForceGraphLink) => createGraphTooltip(describeGraphEdge(link))}
            linkWidth={graphLinkWidth}
            linkDirectionalParticles={particleCount}
            linkDirectionalParticleColor={particleColor}
            linkDirectionalParticleWidth={graphLinkParticleWidth}
            linkDirectionalParticleSpeed={graphLinkParticleSpeed}
            linkDirectionalParticleOffset={graphLinkParticleOffset}
            linkDirectionalArrowLength={(link: ForceGraphLink) => (link.kind === 'triple' ? 4 : 0)}
            linkDirectionalArrowRelPos={0.82}
            onNodeClick={(node: ForceGraphNode, event: MouseEvent) => {
              handleNodeClick(node, event);
            }}
          />
        ))}
      {!isLoading && !error && (
        <div className="graph-navigation">
          {viewMode === '2d' && (
            <>
              <button
                aria-label="Zoom in"
                onClick={() => {
                  const fg = fg2dRef.current;
                  fg?.zoom?.((fg.zoom?.() ?? 1) * 1.4, 200);
                }}
              >
                <Plus />
              </button>
              <button
                aria-label="Zoom out"
                onClick={() => {
                  const fg = fg2dRef.current;
                  fg?.zoom?.((fg.zoom?.() ?? 1) / 1.4, 200);
                }}
              >
                <Minus />
              </button>
            </>
          )}
          <button aria-label="Fit graph" onClick={fitGraph}>
            <ArrowsOut />
          </button>
          <span>
            {onOpenGroup
              ? 'Groups are connected where their items share relationships.'
              : 'Select a memory to read it. Double-click to expand connections.'}
          </span>
        </div>
      )}
    </div>
  );
}

function entityImportanceScale(node: PresentedGraphNode): number {
  if (node.kind !== 'entity') return 1;
  return 1 + Math.min(Math.log2((node.ref_count ?? 0) + 1) * 0.12, 0.72);
}

function shouldShowNodeLabel(node: PresentedGraphNode, globalScale: number): boolean {
  if (node.__aggregateForDocumentId || node.kind === 'document' || node.kind === 'cluster') {
    return true;
  }
  if (node.kind === 'entity') {
    return globalScale > ((node.ref_count ?? 0) >= 2 ? 1.15 : 1.65);
  }
  return globalScale > (node.kind === 'episode' ? 2 : 2.5);
}
