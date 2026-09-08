import type { GraphNode, GraphResponse, NodeKind } from '../api/types';

export interface MemoryGroup {
  id: string;
  label: string;
  members: string[];
}
export function sourceLabel(source: string) {
  return (
    (
      {
        user_message: 'Added by you',
        assistant_message: 'Assistant',
        conversation: 'Conversations',
      } as Record<string, string>
    )[source] || source.replaceAll('_', ' ')
  );
}
export interface Exploration {
  groupId: string | null;
  focusId: string | null;
  query: string;
  source: string;
  since: number;
  kinds: ReadonlySet<NodeKind>;
  expanded: ReadonlySet<string>;
  limit: number;
}

/** UI grouping only: stored cluster membership first, then clearly named source
 * buckets. Never infer facts or rewrite the user's stored clusters. */
export function indexMemoryGroups(graph: GraphResponse) {
  const byId = new Map(graph.nodes.map((n) => [n.id, n]));
  const groups = new Map<string, MemoryGroup>();
  const owner = new Map<string, string>();
  const adjacency = new Map<string, Set<string>>();
  for (const edge of graph.edges) {
    if (!byId.has(edge.source) || !byId.has(edge.target)) continue;
    for (const [a, b] of [
      [edge.source, edge.target],
      [edge.target, edge.source],
    ]) {
      if (!adjacency.has(a)) adjacency.set(a, new Set());
      adjacency.get(a)!.add(b);
    }
  }
  for (const node of [...graph.nodes]
    .filter((n) => n.kind === 'cluster')
    .sort((a, b) => a.id.localeCompare(b.id))) {
    const id = `group:cluster:${node.id}`;
    groups.set(id, { id, label: node.label, members: [node.id] });
    owner.set(node.id, id);
  }
  // Resolve overlapping memberships deterministically without double-counting.
  for (const edge of [...graph.edges]
    .filter((e) => e.kind === 'cluster_member')
    .sort((a, b) => a.id.localeCompare(b.id))) {
    const cluster = byId.get(edge.source)?.kind === 'cluster' ? edge.source : edge.target;
    const member = cluster === edge.source ? edge.target : edge.source;
    const group = groups.get(`group:cluster:${cluster}`);
    if (group && byId.has(member) && !owner.has(member)) {
      group.members.push(member);
      owner.set(member, group.id);
    }
  }
  for (const node of graph.nodes) {
    if (owner.has(node.id)) continue;
    const label =
      node.kind === 'episode'
        ? `${sourceLabel(node.source_type || 'Other')} memories`
        : {
            document: 'Documents',
            chunk: 'Document sections',
            entity: 'People & topics',
            cluster: 'Other groups',
          }[node.kind];
    const id = `group:source:${node.kind}:${node.kind === 'episode' ? node.source_type || 'Other' : ''}`;
    if (!groups.has(id)) groups.set(id, { id, label, members: [] });
    groups.get(id)!.members.push(node.id);
    owner.set(node.id, id);
  }
  return {
    byId,
    adjacency,
    owner,
    groups: [...groups.values()].sort(
      (a, b) => b.members.length - a.members.length || a.id.localeCompare(b.id),
    ),
  };
}

export function exploreMemories(
  graph: GraphResponse,
  index: ReturnType<typeof indexMemoryGroups>,
  options: Exploration,
) {
  const query = options.query.trim().toLocaleLowerCase();
  const group = index.groups.find((g) => g.id === options.groupId);
  let scope: Set<string> | null = group ? new Set(group.members) : null;
  const revealed = new Set<string>();
  if (options.focusId) {
    scope = new Set([options.focusId, ...(index.adjacency.get(options.focusId) || [])]);
    for (const id of options.expanded) {
      // Expansion must originate in the current focus neighborhood.
      if (scope.has(id))
        for (const neighbor of index.adjacency.get(id) || []) {
          scope.add(neighbor);
          revealed.add(neighbor);
        }
    }
  }
  const matches = graph.nodes.filter(
    (n) =>
      (!scope || scope.has(n.id)) &&
      (options.kinds.has(n.kind) || revealed.has(n.id)) &&
      (!options.source || n.source_type === options.source) &&
      (!options.since || (n.ts_ms ?? 0) >= options.since) &&
      (!query || `${n.label} ${n.preview || ''}`.toLocaleLowerCase().includes(query)),
  );
  const anchor =
    options.focusId ?? group?.members.find((id) => index.byId.get(id)?.kind === 'cluster');
  matches.sort(
    (a, b) =>
      Number(b.id === anchor) - Number(a.id === anchor) ||
      (b.ts_ms ?? 0) - (a.ts_ms ?? 0) ||
      a.id.localeCompare(b.id),
  );
  const shown = matches.slice(0, options.limit);
  const ids = new Set(shown.map((n) => n.id));
  return {
    matches,
    graph: {
      nodes: shown,
      edges: graph.edges.filter((e) => ids.has(e.source) && ids.has(e.target)),
    },
  };
}

export function overviewGraph(
  graph: GraphResponse,
  index: ReturnType<typeof indexMemoryGroups>,
  matching: readonly GraphNode[],
  limit = 60,
) {
  const matches = new Set(matching.map((n) => n.id));
  const groups = index.groups
    .map((g) => ({ ...g, members: g.members.filter((id) => matches.has(id)) }))
    .filter((g) => g.members.length);
  const shown = groups.slice(0, limit);
  const ids = new Set(shown.map((g) => g.id));
  const edges = new Map<string, GraphResponse['edges'][number]>();
  for (const edge of graph.edges) {
    if (!matches.has(edge.source) || !matches.has(edge.target)) continue;
    const a = index.owner.get(edge.source),
      b = index.owner.get(edge.target);
    if (!a || !b || a === b || !ids.has(a) || !ids.has(b)) continue;
    const [source, target] = [a, b].sort();
    const id = JSON.stringify([source, target]);
    const previous = edges.get(id);
    edges.set(id, {
      id,
      source,
      target,
      kind: 'semantic',
      meta: {
        grouped_relationships: true,
        evidence_count: (previous?.meta?.evidence_count ?? 0) + 1,
      },
    });
  }
  return {
    groups,
    graph: {
      nodes: shown.map((g) => ({
        id: g.id,
        kind: 'cluster' as const,
        label: g.label,
        preview: `${g.members.length} items. Open this group to explore.`,
        ref_count: g.members.length,
      })),
      edges: [...edges.values()],
    },
  };
}
