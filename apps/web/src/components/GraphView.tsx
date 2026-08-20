// 2D / 3D force-directed graph canvas.
// Uses react-force-graph-2d and react-force-graph-3d (both by @vasturiano)
// — near-identical APIs, swap based on the viewMode setting.

import {
  lazy,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentType,
} from 'react';
import { useGraphData } from '../hooks/useGraphData';
import { useGraphStore } from '../store/graphStore';
import { NODE_KIND_SIZES } from '../lib/nodeKindTheme';
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
 * Particles per edge, weighted so the strongest relationships read as the
 * busiest. Returns 0 wholesale when effects are off — force-graph skips the
 * per-frame particle work entirely at 0, which is the point of the toggle.
 */
function graphLinkParticleCount(link: ForceGraphLink, effects: boolean): number {
  if (!effects) return 0;
  switch (link.kind) {
    case 'triple':
      return 3;
    case 'cluster_member':
      return 2;
    case 'document_chunk':
      return 2;
    case 'semantic':
      return 1;
  }
}

function graphLinkParticleWidth(link: ForceGraphLink): number {
  return link.kind === 'triple' ? 2.8 : 2;
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

export function GraphView() {
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
  const effects = useThemeStore((s) => s.effects);
  const graphLinkColor = (link: ForceGraphLink) => linkColors[link.kind];

  // Container ref for sizing — ResizeObserver-backed so dimensions track
  // the actual painted canvas area, not a stale first-render snapshot.
  // (The old code read `containerRef.current?.clientWidth ?? 800` during
  // render — on first render the ref is null, so the canvas got 800x600
  // regardless of viewport; the force layout then settled inside those
  // wrong bounds and visibly clipped on the right edge.)
  const containerRef = useRef<HTMLDivElement>(null);
  const fg3dRef = useRef<ForceGraph3DHandle | null>(null);
  const [dimensions, setDimensions] = useState({ width: 0, height: 0 });

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
    if (!data) return { nodes: [] as ForceGraphNode[], links: [] as ForceGraphLink[] };
    return buildGraphPresentation(data, visibleKinds, expandedNodeIds, searchQuery);
  }, [data, visibleKinds, searchQuery, expandedNodeIds]);

  const { width, height } = dimensions;

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
          const pass = new UnrealBloomPass(
            { x: width || 1, y: height || 1 },
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
      : NODE_KIND_SIZES[node.kind] * entityImportanceScale(node);
    const size = isSelected ? baseSize * 1.6 : baseSize;
    const color = nodeColors[node.kind];

    // Glow pass. Canvas shadowBlur is measured in device pixels and ignores the
    // current transform, so it is scaled by globalScale to keep the halo
    // proportional to the node as the user zooms. Drawn as a separate filled
    // pass, then cleared — leaving shadowBlur set would bleed onto the label
    // and every later node.
    if (effects) {
      ctx.save();
      ctx.shadowColor = color;
      ctx.shadowBlur = Math.min(size * globalScale * 1.8, 34);
      ctx.beginPath();
      ctx.arc(x, y, size, 0, 2 * Math.PI, false);
      ctx.fillStyle = color;
      ctx.fill();
      // A second pass deepens the bloom on the nodes the user is acting on.
      if (isSelected || isHighlighted || isRecalled) ctx.fill();
      ctx.restore();
    }

    ctx.beginPath();
    ctx.arc(x, y, size, 0, 2 * Math.PI, false);
    ctx.fillStyle = color;
    ctx.fill();

    if (node.__aggregateCount) {
      const fontSize = Math.max(8 / globalScale, 2);
      ctx.font = `600 ${fontSize}px ui-sans-serif, system-ui, sans-serif`;
      ctx.fillStyle = '#fff7ed';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'middle';
      ctx.fillText(String(node.__aggregateCount), x, y);
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
    // until the user zooms in.
    if (isSelected || isHighlighted || shouldShowNodeLabel(node, globalScale)) {
      const fontSize = Math.max(10 / globalScale, 2);
      ctx.font = `${fontSize}px ui-sans-serif, system-ui, sans-serif`;
      ctx.fillStyle = palette.nodeLabel;
      ctx.textAlign = 'center';
      ctx.textBaseline = 'top';
      ctx.fillText(node.label.slice(0, 32), x, y + size + 2);
    }
  };

  const handleNodeClick = (node: ForceGraphNode, event: MouseEvent) => {
    const aggregateDocumentId = documentIdForSummary(node);
    if (aggregateDocumentId) {
      setSelectedNodeId(aggregateDocumentId);
      if (!expandedNodeIds.has(aggregateDocumentId)) toggleExpansion(aggregateDocumentId);
      return;
    }
    if (event.detail === 2) {
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
            graphData={{ nodes: filtered.nodes, links: filtered.links }}
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
            linkDirectionalParticles={(l: ForceGraphLink) => graphLinkParticleCount(l, effects)}
            linkDirectionalParticleColor={(l: ForceGraphLink) => particleColors[l.kind]}
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
            graphData={{ nodes: filtered.nodes, links: filtered.links }}
            width={width}
            height={height}
            // Transparent for the same reason as the 2D canvas: the themed
            // gradient on the container behind it becomes the graph backdrop.
            backgroundColor="rgba(0, 0, 0, 0)"
            nodeId="id"
            nodeLabel={(node: ForceGraphNode) => createGraphTooltip(describeGraphNode(node))}
            nodeColor={(n: ForceGraphNode) => nodeColors[n.kind]}
            nodeVal={(n: ForceGraphNode) =>
              n.__aggregateForDocumentId ? 4 : NODE_KIND_SIZES[n.kind] * entityImportanceScale(n)
            }
            linkColor={graphLinkColor}
            linkLabel={(link: ForceGraphLink) => createGraphTooltip(describeGraphEdge(link))}
            linkWidth={graphLinkWidth}
            linkDirectionalParticles={(l: ForceGraphLink) => graphLinkParticleCount(l, effects)}
            linkDirectionalParticleColor={(l: ForceGraphLink) => particleColors[l.kind]}
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
        <div className="pointer-events-none absolute bottom-3 left-3 max-w-sm rounded-md border border-slate-700/80 bg-slate-950/90 px-3 py-2 text-[11px] text-slate-300 shadow-lg">
          <div className="flex flex-wrap gap-x-3 gap-y-1">
            <GraphLegend color={linkColors.triple} label="fact relationship" />
            <GraphLegend color={linkColors.cluster_member} label="memory cluster" />
            <GraphLegend color={linkColors.document_chunk} label="document section" />
          </div>
          <p className="mt-1 text-slate-400">
            Hover links for meaning. Double-click a node to reveal hidden neighbors.
          </p>
        </div>
      )}
    </div>
  );
}

function GraphLegend({ color, label }: { color: string; label: string }) {
  return (
    <span className="inline-flex items-center gap-1">
      <span className="h-0.5 w-4" style={{ backgroundColor: color }} aria-hidden="true" />
      {label}
    </span>
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
