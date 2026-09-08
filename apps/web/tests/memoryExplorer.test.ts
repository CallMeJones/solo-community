import { describe, expect, it } from 'vitest';
import {
  exploreMemories,
  indexMemoryGroups,
  overviewGraph,
  type Exploration,
} from '../src/lib/memoryExplorer';
import type { GraphResponse, NodeKind } from '../src/api/types';

const defaults: Exploration = {
  groupId: null,
  focusId: null,
  query: '',
  source: '',
  since: 0,
  kinds: new Set<NodeKind>(['episode', 'document', 'cluster', 'entity']),
  expanded: new Set(),
  limit: 100,
};
function fixture(count: number): GraphResponse {
  const nodes: GraphResponse['nodes'] = Array.from({ length: count }, (_, i) => ({
    id: `ep:${i}`,
    kind: 'episode',
    label: `Memory ${i}`,
    source_type: i % 2 ? 'Claude' : 'Codex',
    ts_ms: i,
  }));
  const edges: GraphResponse['edges'] = [];
  for (let i = 0; i < 20; i++) nodes.push({ id: `cl:${i}`, kind: 'cluster', label: `Topic ${i}` });
  for (let i = 0; i < count; i++)
    edges.push({
      id: `member:${i}`,
      source: `cl:${i % 20}`,
      target: `ep:${i}`,
      kind: 'cluster_member',
    });
  edges.push({ id: 'cross', source: 'ep:1', target: 'ep:2', kind: 'semantic', weight: 0.9 });
  return { nodes, edges };
}
describe('large memory exploration', () => {
  it('reveals document sections on explicit expansion even when sections are hidden by default', () => {
    const graph: GraphResponse = {
      nodes: [
        { id: 'doc:1', kind: 'document', label: 'Document' },
        { id: 'chunk:1', kind: 'chunk', label: 'Section' },
      ],
      edges: [{ id: 'part', source: 'doc:1', target: 'chunk:1', kind: 'document_chunk' }],
    };
    const index = indexMemoryGroups(graph);
    expect(
      exploreMemories(graph, index, { ...defaults, focusId: 'doc:1' }).graph.nodes,
    ).toHaveLength(1);
    expect(
      exploreMemories(graph, index, { ...defaults, focusId: 'doc:1', expanded: new Set(['doc:1']) })
        .graph.nodes,
    ).toHaveLength(2);
  });
  for (const count of [1000, 10000])
    it(`keeps all ${count} memories reachable while bounding the initial view`, () => {
      const graph = fixture(count);
      const original = JSON.stringify(graph);
      const start = performance.now();
      const index = indexMemoryGroups(graph);
      const result = exploreMemories(graph, index, defaults);
      const overview = overviewGraph(graph, index, result.matches);
      expect(result.graph.nodes).toHaveLength(100);
      expect(result.matches).toHaveLength(count + 20);
      expect(overview.groups).toHaveLength(20);
      expect(new Set(overview.groups.flatMap((g) => g.members)).size).toBe(count + 20);
      expect(overview.graph.edges).toHaveLength(1); // Only the real cross-group edge.
      expect(
        exploreMemories(graph, index, { ...defaults, limit: count + 20 }).graph.nodes,
      ).toHaveLength(count + 20);
      expect(JSON.stringify(graph)).toBe(original);
      console.info(`explore ${count}: ${(performance.now() - start).toFixed(1)}ms`);
    });
  it('filters data rather than just highlighting labels', () => {
    const graph = fixture(1000),
      index = indexMemoryGroups(graph);
    const result = exploreMemories(graph, index, {
      ...defaults,
      query: 'Memory 999',
      source: 'Claude',
      since: 900,
    });
    expect(result.graph.nodes.map((n) => n.id)).toEqual(['ep:999']);
    expect(exploreMemories(graph, index, { ...defaults, query: 'missing' }).matches).toEqual([]);
  });
  it('keeps the group anchor and its real edges in a paged neighborhood', () => {
    const graph = fixture(10000),
      index = indexMemoryGroups(graph);
    const result = exploreMemories(graph, index, { ...defaults, groupId: 'group:cluster:cl:1' });
    expect(result.graph.nodes[0].id).toBe('cl:1');
    expect(result.graph.nodes).toHaveLength(100);
    expect(result.graph.edges.filter((e) => e.kind === 'cluster_member')).toHaveLength(99);
  });
  it('bounds an overview with many groups without losing group membership', () => {
    const graph: GraphResponse = {
      nodes: Array.from({ length: 1000 }, (_, i) => ({
        id: `cl:${i}`,
        kind: 'cluster',
        label: `Group ${i}`,
      })),
      edges: [],
    };
    const index = indexMemoryGroups(graph);
    const overview = overviewGraph(graph, index, graph.nodes);
    expect(overview.graph.nodes).toHaveLength(60);
    expect(overview.groups).toHaveLength(1000);
    expect(overviewGraph(graph, index, graph.nodes, 1000).graph.nodes).toHaveLength(1000);
  });
  it('focuses direct neighbors and expands only reachable nodes', () => {
    const graph = fixture(100),
      index = indexMemoryGroups(graph);
    const focus = exploreMemories(graph, index, { ...defaults, focusId: 'ep:1' });
    expect(new Set(focus.matches.map((n) => n.id))).toEqual(new Set(['ep:1', 'cl:1', 'ep:2']));
    const expanded = exploreMemories(graph, index, {
      ...defaults,
      focusId: 'ep:1',
      expanded: new Set(['cl:1', 'cl:17']),
    });
    expect(expanded.matches.some((n) => n.id === 'ep:21')).toBe(true);
    expect(expanded.matches.some((n) => n.id === 'ep:17')).toBe(false);
  });
  it('keeps ungrouped records and resolves duplicate membership deterministically', () => {
    const graph = fixture(5);
    graph.nodes.push({ id: 'orphan', kind: 'document', label: 'Orphan' });
    graph.edges.push({ id: 'extra', source: 'cl:3', target: 'ep:1', kind: 'cluster_member' });
    const index = indexMemoryGroups(graph),
      reverse = indexMemoryGroups({
        nodes: [...graph.nodes].reverse(),
        edges: [...graph.edges].reverse(),
      });
    expect(index.owner.get('ep:1')).toBe(reverse.owner.get('ep:1'));
    expect(index.owner.has('orphan')).toBe(true);
    expect(index.groups.flatMap((g) => g.members).filter((id) => id === 'ep:1')).toHaveLength(1);
  });
});
