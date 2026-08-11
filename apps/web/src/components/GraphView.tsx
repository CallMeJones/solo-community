// 2D / 3D force-directed graph canvas.
// Uses react-force-graph-2d and react-force-graph-3d (both by @vasturiano)
// — near-identical APIs, swap based on the viewMode setting.

import { lazy, useLayoutEffect, useMemo, useRef, useState, type ComponentType } from 'react';
import { useGraphData } from '../hooks/useGraphData';
import { useGraphStore } from '../store/graphStore';
import { NODE_KIND_COLORS, NODE_KIND_SIZES } from '../lib/nodeKindTheme';
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

  const filtered = useMemo(() => {
    if (!data) return { nodes: [] as ForceGraphNode[], links: [] as ForceGraphLink[] };
    return buildGraphPresentation(data, visibleKinds, expandedNodeIds, searchQuery);
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
    const baseSize = node.__aggregateForDocumentId
      ? 5
      : NODE_KIND_SIZES[node.kind] * entityImportanceScale(node);
    const size = isSelected ? baseSize * 1.6 : baseSize;
    const color = NODE_KIND_COLORS[node.kind];

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
      ctx.strokeStyle = '#ffffff';
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
      ctx.fillStyle = '#f7ead1';
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
            linkDirectionalParticles={graphLinkDirectionalParticles}
            linkDirectionalParticleColor={graphLinkColor}
            linkDirectionalParticleWidth={(l: ForceGraphLink) => (l.kind === 'triple' ? 2.6 : 0)}
            linkDirectionalArrowLength={(link: ForceGraphLink) =>
              link.kind === 'triple' ? 4 : 0
            }
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
            graphData={{ nodes: filtered.nodes, links: filtered.links }}
            width={width}
            height={height}
            backgroundColor="#080604"
            nodeId="id"
            nodeLabel={(node: ForceGraphNode) => createGraphTooltip(describeGraphNode(node))}
            nodeColor={(n: ForceGraphNode) => NODE_KIND_COLORS[n.kind]}
            nodeVal={(n: ForceGraphNode) =>
              n.__aggregateForDocumentId
                ? 4
                : NODE_KIND_SIZES[n.kind] * entityImportanceScale(n)
            }
            linkColor={graphLinkColor}
            linkLabel={(link: ForceGraphLink) => createGraphTooltip(describeGraphEdge(link))}
            linkWidth={graphLinkWidth}
            linkDirectionalParticles={graphLinkDirectionalParticles}
            linkDirectionalParticleColor={graphLinkColor}
            linkDirectionalParticleWidth={(l: ForceGraphLink) => (l.kind === 'triple' ? 2.6 : 0)}
            linkDirectionalArrowLength={(link: ForceGraphLink) =>
              link.kind === 'triple' ? 4 : 0
            }
            linkDirectionalArrowRelPos={0.82}
            onNodeClick={(node: ForceGraphNode, event: MouseEvent) => {
              handleNodeClick(node, event);
            }}
          />
        ))}
      {!isLoading && !error && (
        <div className="pointer-events-none absolute bottom-3 left-3 max-w-sm rounded-md border border-slate-700/80 bg-slate-950/90 px-3 py-2 text-[11px] text-slate-300 shadow-lg">
          <div className="flex flex-wrap gap-x-3 gap-y-1">
            <GraphLegend color="rgba(101, 214, 163, 0.9)" label="fact relationship" />
            <GraphLegend color="rgba(242, 179, 93, 0.8)" label="memory cluster" />
            <GraphLegend color="rgba(213, 111, 62, 0.9)" label="document section" />
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
