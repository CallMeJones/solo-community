// 2D / 3D force-directed graph canvas.
// Uses react-force-graph-2d and react-force-graph-3d (both by @vasturiano)
// — near-identical APIs, swap based on the viewMode setting.

import { lazy, useLayoutEffect, useMemo, useRef, useState, type ComponentType } from 'react';
import { useGraphData } from '../hooks/useGraphData';
import { useGraphStore } from '../store/graphStore';
import type { GraphEdge, GraphNode } from '../api/types';
import { NODE_KIND_COLORS, NODE_KIND_SIZES } from '../lib/nodeKindTheme';

const ForceGraph2D = lazy(() => import('react-force-graph-2d')) as ComponentType<
  Record<string, unknown>
>;
const ForceGraph3D = lazy(() => import('react-force-graph-3d')) as ComponentType<
  Record<string, unknown>
>;

interface ForceGraphNode extends GraphNode {
  // Force-graph adds these at runtime; we type them as optional so we can read them safely.
  x?: number;
  y?: number;
  z?: number;
  /** Whether this node matches the current search/filter for highlight purposes. */
  __highlighted?: boolean;
}

interface ForceGraphLink {
  source: string;
  target: string;
  kind: GraphEdge['kind'];
  id: string;
}

function graphLinkColor(link: ForceGraphLink): string {
  switch (link.kind) {
    case 'triple':
      return 'rgba(101, 214, 163, 0.9)';
    case 'cluster_member':
      return 'rgba(242, 179, 93, 0.58)';
    case 'document_chunk':
      return 'rgba(213, 111, 62, 0.72)';
    case 'semantic':
      return 'rgba(180, 135, 255, 0.72)';
  }
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

function graphLinkDirectionalParticles(link: ForceGraphLink): number {
  return link.kind === 'triple' ? 2 : 0;
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

  // Container ref for sizing — ResizeObserver-backed so dimensions track
  // the actual painted canvas area, not a stale first-render snapshot.
  // (The old code read `containerRef.current?.clientWidth ?? 800` during
  // render — on first render the ref is null, so the canvas got 800x600
  // regardless of viewport; the force layout then settled inside those
  // wrong bounds and visibly clipped on the right edge.)
  const containerRef = useRef<HTMLDivElement>(null);
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

  // Apply filters: hide nodes whose kind is toggled off; only edges where both endpoints visible.
  // Expansion override: any neighbor (via any edge) of a node in `expandedNodeIds`
  // is forced visible even if its kind would normally be hidden. This is what
  // lets a double-click on a document reveal its chunks while `chunk` is off.
  const filtered = useMemo(() => {
    if (!data) return { nodes: [] as ForceGraphNode[], links: [] as ForceGraphLink[] };

    const q = searchQuery.trim().toLowerCase();

    // First pass: compute neighbor ids of every expanded node.
    const expandedNeighborIds = new Set<string>();
    if (expandedNodeIds.size > 0) {
      for (const e of data.edges) {
        if (expandedNodeIds.has(e.source)) expandedNeighborIds.add(e.target);
        if (expandedNodeIds.has(e.target)) expandedNeighborIds.add(e.source);
      }
    }

    const nodes: ForceGraphNode[] = data.nodes
      .filter((n) => visibleKinds.has(n.kind) || expandedNeighborIds.has(n.id))
      .map((n) => ({
        ...n,
        __highlighted: q.length > 0 ? n.label.toLowerCase().includes(q) : false,
      }));

    const nodeIds = new Set(nodes.map((n) => n.id));

    const links: ForceGraphLink[] = data.edges
      .filter((e) => nodeIds.has(e.source) && nodeIds.has(e.target))
      .map((e) => ({
        id: e.id,
        source: e.source,
        target: e.target,
        kind: e.kind,
      }));

    return { nodes, links };
  }, [data, visibleKinds, searchQuery, expandedNodeIds]);

  const { width, height } = dimensions;

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
    const baseSize = NODE_KIND_SIZES[node.kind];
    const size = isSelected ? baseSize * 1.6 : baseSize;
    const color = NODE_KIND_COLORS[node.kind];

    ctx.beginPath();
    ctx.arc(x, y, size, 0, 2 * Math.PI, false);
    ctx.fillStyle = color;
    ctx.fill();

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
      ctx.strokeStyle = '#ffffff';
      ctx.stroke();
    } else if (isHighlighted) {
      ctx.lineWidth = 1.5 / globalScale;
      ctx.strokeStyle = '#fbbf24'; // amber-400
      ctx.stroke();
    }

    // Label only visible at high zoom or when selected/highlighted.
    if (isSelected || isHighlighted || globalScale > 2) {
      const fontSize = Math.max(10 / globalScale, 2);
      ctx.font = `${fontSize}px ui-sans-serif, system-ui, sans-serif`;
      ctx.fillStyle = '#f7ead1';
      ctx.textAlign = 'center';
      ctx.textBaseline = 'top';
      ctx.fillText(node.label.slice(0, 32), x, y + size + 2);
    }
  };

  // Container is rendered unconditionally so the ResizeObserver (useLayoutEffect
  // with [] deps) attaches on first paint. Loading/error states render as
  // overlays inside it rather than as early returns, otherwise the ref is null
  // on first render, the effect bails out, and dimensions stay at 0×0 forever.
  return (
    <div ref={containerRef} className="relative h-full w-full">
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
            backgroundColor="#080604"
            nodeId="id"
            nodeLabel={(n: ForceGraphNode) => `${n.kind}: ${n.label}`}
            nodeCanvasObject={nodeCanvasObject}
            nodePointerAreaPaint={(
              node: ForceGraphNode,
              color: string,
              ctx: CanvasRenderingContext2D,
            ) => {
              const x = node.x ?? 0;
              const y = node.y ?? 0;
              const size = NODE_KIND_SIZES[node.kind] + 2;
              ctx.fillStyle = color;
              ctx.beginPath();
              ctx.arc(x, y, size, 0, 2 * Math.PI, false);
              ctx.fill();
            }}
            linkColor={graphLinkColor}
            linkWidth={graphLinkWidth}
            linkDirectionalParticles={graphLinkDirectionalParticles}
            linkDirectionalParticleColor={graphLinkColor}
            linkDirectionalParticleWidth={(l: ForceGraphLink) => (l.kind === 'triple' ? 2.6 : 0)}
            onNodeClick={(node: ForceGraphNode, event: MouseEvent) => {
              // event.detail === 2 means this click is part of a double-click;
              // the first click of the pair still fires with detail===1, so a
              // double-click selects AND expands — intentional.
              if (event.detail === 2) {
                toggleExpansion(node.id);
              } else {
                setSelectedNodeId(node.id);
              }
            }}
            cooldownTicks={100}
          />
        ) : (
          <ForceGraph3D
            graphData={{ nodes: filtered.nodes, links: filtered.links }}
            width={width}
            height={height}
            backgroundColor="#080604"
            nodeId="id"
            nodeLabel={(n: ForceGraphNode) => `${n.kind}: ${n.label}`}
            nodeColor={(n: ForceGraphNode) => NODE_KIND_COLORS[n.kind]}
            nodeVal={(n: ForceGraphNode) => NODE_KIND_SIZES[n.kind]}
            linkColor={graphLinkColor}
            linkWidth={graphLinkWidth}
            linkDirectionalParticles={graphLinkDirectionalParticles}
            linkDirectionalParticleColor={graphLinkColor}
            linkDirectionalParticleWidth={(l: ForceGraphLink) => (l.kind === 'triple' ? 2.6 : 0)}
            onNodeClick={(node: ForceGraphNode, event: MouseEvent) => {
              if (event.detail === 2) {
                toggleExpansion(node.id);
              } else {
                setSelectedNodeId(node.id);
              }
            }}
          />
        ))}
    </div>
  );
}
