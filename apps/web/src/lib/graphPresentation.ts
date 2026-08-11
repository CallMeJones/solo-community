import type { GraphEdge, GraphNode, GraphResponse, NodeKind } from '../api/types';

const DOCUMENT_SUMMARY_PREFIX = 'ui:document-sections:';

export interface PresentedGraphNode extends GraphNode {
  __highlighted: boolean;
  __relationshipCount: number;
  __hiddenNeighborCount: number;
  __aggregateForDocumentId?: string;
  __aggregateCount?: number;
}

export interface PresentedGraphLink extends GraphEdge {
  source: string;
  target: string;
  __summary?: boolean;
}

export interface PresentedGraph {
  nodes: PresentedGraphNode[];
  links: PresentedGraphLink[];
}

/**
 * Turn the complete API graph into the graph the user sees.
 *
 * Document chunks remain opt-in because rendering every indexed section is
 * noisy. A collapsed section node keeps each document visibly connected and
 * explains what is hidden; expanding the document swaps that summary for the
 * real chunk nodes and edges.
 */
export function buildGraphPresentation(
  graph: GraphResponse,
  visibleKinds: ReadonlySet<NodeKind>,
  expandedNodeIds: ReadonlySet<string>,
  searchQuery: string,
): PresentedGraph {
  const q = searchQuery.trim().toLowerCase();
  const expandedNeighborIds = new Set<string>();
  for (const edge of graph.edges) {
    if (expandedNodeIds.has(edge.source)) expandedNeighborIds.add(edge.target);
    if (expandedNodeIds.has(edge.target)) expandedNeighborIds.add(edge.source);
  }

  const visibleRealNodes = graph.nodes.filter(
    (node) => visibleKinds.has(node.kind) || expandedNeighborIds.has(node.id),
  );
  const visibleRealIds = new Set(visibleRealNodes.map((node) => node.id));
  const relationshipCounts = countRelationships(graph.edges);
  const hiddenNeighborCounts = countHiddenNeighbors(graph.edges, visibleRealIds);
  const nodes: PresentedGraphNode[] = visibleRealNodes.map((node) => ({
    ...node,
    __highlighted:
      q.length > 0 &&
      (node.label.toLowerCase().includes(q) || node.id.toLowerCase().includes(q)),
    __relationshipCount: relationshipCounts.get(node.id) ?? 0,
    __hiddenNeighborCount: hiddenNeighborCounts.get(node.id) ?? 0,
  }));
  const links: PresentedGraphLink[] = graph.edges
    .filter((edge) => visibleRealIds.has(edge.source) && visibleRealIds.has(edge.target))
    .map((edge) => ({ ...edge }));

  if (!visibleKinds.has('chunk')) {
    for (const document of visibleRealNodes.filter((node) => node.kind === 'document')) {
      if (expandedNodeIds.has(document.id)) continue;
      const hiddenChunkEdges = graph.edges.filter(
        (edge) =>
          edge.kind === 'document_chunk' &&
          edge.source === document.id &&
          !visibleRealIds.has(edge.target),
      );
      if (hiddenChunkEdges.length === 0) continue;

      const count = hiddenChunkEdges.length;
      const summaryId = documentSummaryId(document.id);
      nodes.push({
        id: summaryId,
        kind: 'chunk',
        label: `${count} indexed section${count === 1 ? '' : 's'}`,
        preview: 'Select this summary or double-click the document to reveal its indexed sections.',
        ts_ms: document.ts_ms,
        __highlighted: false,
        __relationshipCount: count,
        __hiddenNeighborCount: 0,
        __aggregateForDocumentId: document.id,
        __aggregateCount: count,
      });
      links.push({
        id: `${document.id}--document_chunk--${summaryId}`,
        source: document.id,
        target: summaryId,
        kind: 'document_chunk',
        meta: { hidden_chunk_count: count },
        __summary: true,
      });
    }
  }

  return { nodes, links };
}

export function describeGraphNode(node: PresentedGraphNode): string {
  if (node.__aggregateForDocumentId) {
    return `${node.label}\nClick to reveal these document sections.`;
  }
  const connections = `${node.__relationshipCount} graph connection${
    node.__relationshipCount === 1 ? '' : 's'
  }`;
  const hidden =
    node.__hiddenNeighborCount > 0
      ? `\n${node.__hiddenNeighborCount} connection${
          node.__hiddenNeighborCount === 1 ? '' : 's'
        } hidden by the current filters`
      : '';
  return `${friendlyKind(node.kind)}: ${node.label}\n${connections}${hidden}`;
}

export function describeGraphEdge(edge: PresentedGraphLink): string {
  switch (edge.kind) {
    case 'triple': {
      const relationship = friendlyPredicate(edge.predicate ?? 'relationship');
      const evidenceCount = asFiniteNumber(edge.meta?.evidence_count);
      const confidence = asFiniteNumber(edge.meta?.confidence ?? edge.weight);
      const details = [
        evidenceCount === null
          ? null
          : `${evidenceCount} evidence source${evidenceCount === 1 ? '' : 's'}`,
        confidence === null ? null : `${Math.round(confidence * 100)}% confidence`,
      ].filter((value): value is string => value !== null);
      return details.length > 0 ? `${relationship}\n${details.join(' · ')}` : relationship;
    }
    case 'document_chunk': {
      const count = asFiniteNumber(edge.meta?.hidden_chunk_count);
      return count === null
        ? 'Indexed document section'
        : `${count} indexed section${count === 1 ? '' : 's'} (collapsed)`;
    }
    case 'cluster_member':
      return 'Memory grouped in this cluster';
    case 'semantic': {
      const similarity = asFiniteNumber(edge.weight);
      return similarity === null
        ? 'Semantically similar'
        : `Semantically similar · ${Math.round(similarity * 100)}%`;
    }
  }
}

export function documentIdForSummary(node: PresentedGraphNode): string | null {
  return node.__aggregateForDocumentId ?? null;
}

function documentSummaryId(documentId: string): string {
  return `${DOCUMENT_SUMMARY_PREFIX}${encodeURIComponent(documentId)}`;
}

function countRelationships(edges: GraphEdge[]): Map<string, number> {
  const counts = new Map<string, number>();
  for (const edge of edges) {
    counts.set(edge.source, (counts.get(edge.source) ?? 0) + 1);
    counts.set(edge.target, (counts.get(edge.target) ?? 0) + 1);
  }
  return counts;
}

function countHiddenNeighbors(
  edges: GraphEdge[],
  visibleNodeIds: ReadonlySet<string>,
): Map<string, number> {
  const counts = new Map<string, number>();
  for (const edge of edges) {
    if (visibleNodeIds.has(edge.source) && !visibleNodeIds.has(edge.target)) {
      counts.set(edge.source, (counts.get(edge.source) ?? 0) + 1);
    }
    if (visibleNodeIds.has(edge.target) && !visibleNodeIds.has(edge.source)) {
      counts.set(edge.target, (counts.get(edge.target) ?? 0) + 1);
    }
  }
  return counts;
}

function friendlyKind(kind: NodeKind): string {
  switch (kind) {
    case 'episode':
      return 'Memory';
    case 'document':
      return 'Document';
    case 'chunk':
      return 'Document section';
    case 'cluster':
      return 'Memory cluster';
    case 'entity':
      return 'Entity';
  }
}

function friendlyPredicate(predicate: string): string {
  return predicate.replace(/[_-]+/g, ' ');
}

function asFiniteNumber(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}
