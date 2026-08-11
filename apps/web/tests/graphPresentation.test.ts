import { describe, expect, it } from 'vitest';
import type { GraphResponse, NodeKind } from '../src/api/types';
import {
  buildGraphPresentation,
  describeGraphEdge,
  describeGraphNode,
  documentIdForSummary,
} from '../src/lib/graphPresentation';

const graph: GraphResponse = {
  nodes: [
    { id: 'doc:guide', kind: 'document', label: 'Solo guide' },
    { id: 'chunk:one', kind: 'chunk', label: 'First section' },
    { id: 'chunk:two', kind: 'chunk', label: 'Second section' },
    { id: 'ent:Solo', kind: 'entity', label: 'Solo', ref_count: 1 },
    { id: 'ent:Ollama', kind: 'entity', label: 'Ollama', ref_count: 1 },
  ],
  edges: [
    {
      id: 'doc-one',
      source: 'doc:guide',
      target: 'chunk:one',
      kind: 'document_chunk',
    },
    {
      id: 'doc-two',
      source: 'doc:guide',
      target: 'chunk:two',
      kind: 'document_chunk',
    },
    {
      id: 'relationship-solo-ollama',
      source: 'ent:Solo',
      target: 'ent:Ollama',
      kind: 'triple',
      predicate: 'uses_for_embeddings',
      weight: 0.88,
      meta: { evidence_count: 2, confidence: 0.88 },
    },
  ],
};

const defaultKinds: ReadonlySet<NodeKind> = new Set(['document', 'entity']);

describe('graph presentation', () => {
  it('keeps a document connected through one truthful collapsed section summary', () => {
    const result = buildGraphPresentation(graph, defaultKinds, new Set(), '');
    const summary = result.nodes.find((node) => documentIdForSummary(node) === 'doc:guide');

    expect(summary).toMatchObject({
      kind: 'chunk',
      label: '2 indexed sections',
      __aggregateCount: 2,
    });
    expect(result.nodes.some((node) => node.id === 'chunk:one')).toBe(false);
    expect(result.links).toContainEqual(
      expect.objectContaining({
        source: 'doc:guide',
        target: summary?.id,
        kind: 'document_chunk',
        __summary: true,
      }),
    );
    expect(describeGraphNode(summary!)).toContain('Click to reveal');
  });

  it('replaces the summary with real document sections when expanded', () => {
    const result = buildGraphPresentation(graph, defaultKinds, new Set(['doc:guide']), '');

    expect(result.nodes.filter((node) => node.kind === 'chunk').map((node) => node.id)).toEqual([
      'chunk:one',
      'chunk:two',
    ]);
    expect(result.links.filter((edge) => edge.kind === 'document_chunk')).toHaveLength(2);
    expect(result.links.some((edge) => edge.__summary)).toBe(false);
  });

  it('preserves relationship metadata and explains predicates, evidence, and confidence', () => {
    const result = buildGraphPresentation(graph, defaultKinds, new Set(), 'solo');
    const relationship = result.links.find((edge) => edge.kind === 'triple');
    const solo = result.nodes.find((node) => node.id === 'ent:Solo');

    expect(relationship?.predicate).toBe('uses_for_embeddings');
    expect(relationship?.meta?.evidence_count).toBe(2);
    expect(describeGraphEdge(relationship!)).toBe(
      'uses for embeddings\n2 evidence sources · 88% confidence',
    );
    expect(solo).toMatchObject({ __highlighted: true, __relationshipCount: 1 });
  });
});
