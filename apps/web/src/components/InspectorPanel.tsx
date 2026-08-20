// Right-side panel that shows details for the currently selected node.

import { useQueryClient } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import {
  fetchContradictions,
  fetchEntities,
  fetchNeighbors,
  resolveContradiction,
  updateMemory,
} from '../api/client';
import type { ContradictionHit, EntityHit, GraphNode, GraphResponse, NodeKind } from '../api/types';
import { useSelectedNode } from '../hooks/useSelectedNode';
import { useNodeKindColors } from '../store/themeStore';
import { useGraphStore } from '../store/graphStore';
import { useSettingsStore } from '../store/settingsStore';

const USE_MOCKS = import.meta.env.VITE_SOLO_USE_MOCKS === '1';

function KindBadge({ kind }: { kind: NodeKind }) {
  const nodeColors = useNodeKindColors();
  return (
    <span
      className="inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider"
      style={{ backgroundColor: `${nodeColors[kind]}22`, color: nodeColors[kind] }}
    >
      <span
        className="h-1.5 w-1.5 rounded-full"
        style={{ backgroundColor: nodeColors[kind] }}
      />
      {kind}
    </span>
  );
}

interface SimilarState {
  loading: boolean;
  error: string | null;
  result: GraphResponse | null;
  /**
   * Which selectedNodeId the `result` was fetched for. When the user
   * selects a different node, `forNodeId !== selectedNodeId` and the
   * stale list hides — without this we'd keep showing the previous
   * node's similar list against the new node's inspector.
   */
  forNodeId: string | null;
}

interface EntityState {
  loading: boolean;
  error: string | null;
  result: EntityHit[] | null;
  forNodeId: string | null;
}

interface ContradictionState {
  loading: boolean;
  error: string | null;
  items: ContradictionHit[] | null;
  resolvingKey: string | null;
}

export function InspectorPanel() {
  const queryClient = useQueryClient();
  const selectedNodeId = useGraphStore((s) => s.selectedNodeId);
  const setSelectedNodeId = useGraphStore((s) => s.setSelectedNodeId);
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const searchQuery = useGraphStore((s) => s.searchQuery);
  const visibleKinds = useGraphStore((s) => s.visibleKinds);
  const addRecalled = useGraphStore((s) => s.addRecalled);
  const expandedNodeIds = useGraphStore((s) => s.expandedNodeIds);
  const toggleExpansion = useGraphStore((s) => s.toggleExpansion);
  const { data, isLoading } = useSelectedNode();
  const [similar, setSimilar] = useState<SimilarState>({
    loading: false,
    error: null,
    result: null,
    forNodeId: null,
  });
  const [editText, setEditText] = useState('');
  const [editStatus, setEditStatus] = useState<{
    saving: boolean;
    error: string | null;
    saved: boolean;
  }>({ saving: false, error: null, saved: false });
  const [entities, setEntities] = useState<EntityState>({
    loading: false,
    error: null,
    result: null,
    forNodeId: null,
  });
  const [contradictions, setContradictions] = useState<ContradictionState>({
    loading: false,
    error: null,
    items: null,
    resolvingKey: null,
  });

  // The similar list belongs to whichever node was selected when the
  // user clicked "Show similar". If the user navigates to a different
  // node afterwards, drop the stale list (its results aren't about the
  // current selection). The error is also tied to the same node id.
  const similarForCurrent =
    similar.forNodeId !== null && similar.forNodeId === selectedNodeId ? similar : null;
  const graphCache =
    queryClient.getQueryData<GraphResponse>([
      'graph',
      apiUrl,
      connectionRevision,
      USE_MOCKS ? 'mock' : 'live',
    ]) ?? null;
  const searchMatches = !selectedNodeId
    ? (graphCache?.nodes
        .filter((node) => {
          const q = searchQuery.trim().toLowerCase();
          return (
            q.length > 0 &&
            visibleKinds.has(node.kind) &&
            (node.label.toLowerCase().includes(q) || node.id.toLowerCase().includes(q))
          );
        })
        .slice(0, 8) ?? [])
    : [];

  useEffect(() => {
    setEditText(data?.full_text ?? '');
    setEditStatus({ saving: false, error: null, saved: false });
    setSimilar({ loading: false, error: null, result: null, forNodeId: null });
    setEntities({ loading: false, error: null, result: null, forNodeId: null });
    setContradictions({ loading: false, error: null, items: null, resolvingKey: null });
  }, [apiUrl, connectionRevision, data?.full_text, selectedNodeId]);

  const handleShowSimilar = async () => {
    if (!selectedNodeId || !data) return;
    const neighborKind = neighborQueryKind(data.node.kind);
    setSimilar({ loading: true, error: null, result: null, forNodeId: selectedNodeId });
    try {
      const result = await fetchNeighbors(
        selectedNodeId,
        { kind: neighborKind, limit: 8 },
        {},
      );
      const ids = new Set<string>([selectedNodeId, ...result.nodes.map((n) => n.id)]);
      addRecalled(ids);
      setSimilar({ loading: false, error: null, result, forNodeId: selectedNodeId });
    } catch (err) {
      setSimilar({
        loading: false,
        error: err instanceof Error ? err.message : String(err),
        result: null,
        forNodeId: selectedNodeId,
      });
    }
  };

  const handleSaveMemory = async () => {
    if (!selectedNodeId) return;
    setEditStatus({ saving: true, error: null, saved: false });
    try {
      const updated = await updateMemory(selectedNodeId, editText, {});
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['graph'] }),
        queryClient.invalidateQueries({ queryKey: ['inspect', selectedNodeId] }),
      ]);
      setEditText(updated.content);
      setEditStatus({ saving: false, error: null, saved: true });
    } catch (err) {
      setEditStatus({
        saving: false,
        error: err instanceof Error ? err.message : String(err),
        saved: false,
      });
    }
  };

  const handleFindEntities = async () => {
    if (!selectedNodeId || !data) return;
    const query = data.node.id.startsWith('ent:') ? data.node.id.slice(4) : data.node.label;
    setEntities({ loading: true, error: null, result: null, forNodeId: selectedNodeId });
    try {
      const result = await fetchEntities(query, 8, {});
      setEntities({ loading: false, error: null, result, forNodeId: selectedNodeId });
    } catch (err) {
      setEntities({
        loading: false,
        error: err instanceof Error ? err.message : String(err),
        result: null,
        forNodeId: selectedNodeId,
      });
    }
  };

  const handleLoadContradictions = async () => {
    setContradictions({ loading: true, error: null, items: null, resolvingKey: null });
    try {
      const items = await fetchContradictions(10, {});
      setContradictions({ loading: false, error: null, items, resolvingKey: null });
    } catch (err) {
      setContradictions({
        loading: false,
        error: err instanceof Error ? err.message : String(err),
        items: null,
        resolvingKey: null,
      });
    }
  };

  const handleResolveContradiction = async (hit: ContradictionHit) => {
    const key = `${hit.a_id}:${hit.b_id}:${hit.kind}`;
    setContradictions((s) => ({ ...s, error: null, resolvingKey: key }));
    try {
      const resolved = await resolveContradiction(hit, {
        note: 'Resolved from Solo Memory inspector',
        winningTripleId: hit.winning_triple_id ?? undefined,
      });
      setContradictions((s) => ({
        loading: false,
        error: null,
        resolvingKey: null,
        items:
          s.items?.map((item) =>
            item.a_id === hit.a_id && item.b_id === hit.b_id && item.kind === hit.kind
              ? { ...item, ...resolved }
              : item,
          ) ?? null,
      }));
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['graph'] }),
        queryClient.invalidateQueries({ queryKey: ['inspect'] }),
      ]);
    } catch (err) {
      setContradictions((s) => ({
        ...s,
        error: err instanceof Error ? err.message : String(err),
        resolvingKey: null,
      }));
    }
  };

  if (!selectedNodeId) {
    if (searchMatches.length > 0) {
      return <SearchMatchList nodes={searchMatches} onSelect={setSelectedNodeId} />;
    }

    return (
      <div className="flex h-full flex-col items-center justify-center gap-2 text-center text-slate-400">
        <p className="text-sm">No node selected</p>
        <p className="text-xs">Click any node in the graph to inspect it.</p>
      </div>
    );
  }

  if (isLoading || !data) {
    return <p className="text-sm text-slate-400">Loading inspector...</p>;
  }

  const { node, full_text, triples_in, triples_out } = data;
  const literalFacts = data.literal_facts ?? [];
  const incidentGraphEdges =
    graphCache?.edges.filter(
      (edge) => edge.source === selectedNodeId || edge.target === selectedNodeId,
    ) ?? [];
  const graphConnectionCount = incidentGraphEdges.length;
  const documentChunkCount = incidentGraphEdges.filter(
    (edge) => edge.kind === 'document_chunk',
  ).length;
  const isExpanded = expandedNodeIds.has(node.id);
  const displayedRefCount =
    node.ref_count ?? graphCache?.nodes.find((candidate) => candidate.id === node.id)?.ref_count;
  const canEditMemory = node.kind === 'episode' && full_text !== undefined;
  const editDirty = editText !== (full_text ?? '');
  const entitiesForCurrent =
    entities.forNodeId !== null && entities.forNodeId === selectedNodeId ? entities : null;
  const neighborKind = neighborQueryKind(node.kind);
  const neighborHeading =
    neighborKind === 'semantic' ? 'Semantically similar' : 'Related graph nodes';
  const neighborButtonLabel = neighborKind === 'semantic' ? 'Show similar' : 'Show related';
  const neighborLoadingLabel = neighborKind === 'semantic' ? 'Searching...' : 'Loading...';
  const neighborEmptyLabel =
    neighborKind === 'semantic' ? 'No similar nodes found.' : 'No related nodes found.';

  return (
    <div className="flex flex-col gap-4 text-sm">
      <div className="flex items-start justify-between">
        <KindBadge kind={node.kind} />
        <button
          onClick={() => setSelectedNodeId(null)}
          className="text-xs text-slate-400 hover:text-slate-300"
          aria-label="Close inspector"
        >
          Clear
        </button>
      </div>

      <div>
        <h2 className="text-base font-semibold leading-tight text-slate-100">{node.label}</h2>
        <p className="mt-1 font-mono text-[10px] text-slate-400">{node.id}</p>
      </div>

      {full_text && (
        <section>
          <h3 className="mb-1 text-xs font-semibold uppercase tracking-wider text-slate-400">
            {node.kind === 'document' ? 'Document text' : 'Full text'}
          </h3>
          <p className="max-h-80 overflow-y-auto whitespace-pre-wrap rounded-md bg-slate-900 p-2 text-slate-200">
            {full_text}
          </p>
        </section>
      )}

      {node.kind === 'document' && (
        <section className="rounded-md border border-orange-900/60 bg-orange-950/20 p-3">
          <div className="flex items-center justify-between gap-3">
            <div>
              <h3 className="text-xs font-semibold uppercase tracking-wider text-orange-200">
                Indexed sections
              </h3>
              <p className="mt-1 text-xs text-slate-400">
                {documentChunkCount} searchable section{documentChunkCount === 1 ? '' : 's'} belong
                to this document.
              </p>
            </div>
            <button
              type="button"
              onClick={() => toggleExpansion(node.id)}
              disabled={documentChunkCount === 0}
              className="shrink-0 rounded-md border border-orange-800 bg-slate-950 px-2 py-1 text-[10px] uppercase tracking-wider text-orange-200 hover:border-orange-600 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {isExpanded ? 'Collapse' : 'Reveal'}
            </button>
          </div>
          <p className="mt-2 text-[11px] text-slate-500">
            Sections are collapsed into one summary node by default so large documents do not
            overwhelm the memory graph.
          </p>
        </section>
      )}

      {canEditMemory && (
        <section>
          <div className="mb-1 flex items-center justify-between">
            <h3 className="text-xs font-semibold uppercase tracking-wider text-slate-400">
              Correction
            </h3>
            <button
              type="button"
              onClick={handleSaveMemory}
              disabled={editStatus.saving || editText.trim().length === 0 || !editDirty}
              className="rounded-md border border-slate-700 bg-slate-900 px-2 py-0.5 text-[10px] uppercase tracking-wider text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {editStatus.saving ? 'Saving...' : 'Save'}
            </button>
          </div>
          <textarea
            value={editText}
            onChange={(event) => {
              setEditText(event.target.value);
              setEditStatus((status) => ({ ...status, error: null, saved: false }));
            }}
            className="min-h-28 w-full resize-y rounded-md border border-slate-800 bg-slate-950 p-2 text-xs text-slate-200 outline-none focus:border-emerald-600"
            aria-label="Memory correction text"
          />
          {editStatus.error && (
            <p className="mt-1 rounded-md border border-red-900 bg-red-950/50 px-2 py-1 text-xs text-red-300">
              {editStatus.error}
            </p>
          )}
          {editStatus.saved && (
            <p className="mt-1 rounded-md border border-emerald-900 bg-emerald-950/40 px-2 py-1 text-xs text-emerald-300">
              Saved
            </p>
          )}
        </section>
      )}

      {node.kind === 'entity' && (
        <section>
          <div className="mb-1 flex items-center justify-between">
            <h3 className="text-xs font-semibold uppercase tracking-wider text-slate-400">
              Entity matches
            </h3>
            <button
              type="button"
              onClick={handleFindEntities}
              disabled={entities.loading && entities.forNodeId === selectedNodeId}
              className="rounded-md border border-slate-700 bg-slate-900 px-2 py-0.5 text-[10px] uppercase tracking-wider text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {entities.loading && entities.forNodeId === selectedNodeId ? 'Finding...' : 'Find'}
            </button>
          </div>
          {entitiesForCurrent?.error && (
            <p className="rounded-md border border-red-900 bg-red-950/50 px-2 py-1 text-xs text-red-300">
              {entitiesForCurrent.error}
            </p>
          )}
          {entitiesForCurrent?.result && <EntityList result={entitiesForCurrent.result} />}
        </section>
      )}

      <section className="grid grid-cols-2 gap-2 text-xs">
        <div className="col-span-2 rounded-md bg-slate-900 p-2">
          <div className="text-slate-400">Graph connections</div>
          <div className="text-base font-semibold text-slate-100">{graphConnectionCount}</div>
        </div>
        {node.kind === 'episode' && node.source_type && (
          <div className="rounded-md bg-slate-900 p-2">
            <div className="text-slate-400">Source</div>
            <div className="truncate text-xs font-medium text-slate-100">{node.source_type}</div>
          </div>
        )}
        {node.kind === 'episode' && typeof node.salience === 'number' && (
          <div className="rounded-md bg-slate-900 p-2">
            <div className="text-slate-400">Salience</div>
            <div className="text-xs font-medium text-slate-100">
              {Math.round(node.salience * 100)}%
            </div>
          </div>
        )}
        {node.ts_ms && (
          <div className="col-span-2 rounded-md bg-slate-900 p-2">
            <div className="text-slate-400">Created</div>
            <div className="font-mono text-xs text-slate-200">
              {new Date(node.ts_ms).toISOString()}
            </div>
          </div>
        )}
        {typeof displayedRefCount === 'number' && (
          <div className="col-span-2 rounded-md bg-slate-900 p-2">
            <div className="text-slate-400">Reference count</div>
            <div className="text-base font-semibold text-slate-100">{displayedRefCount}</div>
          </div>
        )}
      </section>

      {literalFacts.length > 0 && (
        <section>
          <h3 className="mb-1 text-xs font-semibold uppercase tracking-wider text-slate-400">
            Facts ({literalFacts.length})
          </h3>
          <ul className="space-y-1">
            {literalFacts.slice(0, 12).map((fact, index) => (
              <li
                key={`${fact.subject_id}-${fact.predicate}-${index}`}
                className="rounded-md bg-slate-900 px-2 py-1 text-xs"
              >
                <div className="flex items-center gap-2">
                  <span className="shrink-0 text-slate-400">{fact.predicate}</span>
                  <span className="min-w-0 flex-1 truncate font-mono text-slate-300">
                    {fact.object_value}
                  </span>
                  <span className="font-mono text-[10px] text-slate-500">
                    {Math.round(fact.confidence * 100)}%
                  </span>
                </div>
              </li>
            ))}
          </ul>
        </section>
      )}

      {triples_out.length > 0 && (
        <section>
          <h3 className="mb-1 text-xs font-semibold uppercase tracking-wider text-slate-400">
            Outgoing ({triples_out.length})
          </h3>
          <ul className="space-y-1">
            {triples_out.slice(0, 12).map((e) => (
              <li
                key={e.id}
                className="flex items-center gap-2 rounded-md bg-slate-900 px-2 py-1 text-xs"
              >
                <span className="text-slate-400">{e.predicate ?? e.kind}</span>
                <button
                  type="button"
                  onClick={() => setSelectedNodeId(e.target)}
                  title={e.target}
                  className="min-w-0 flex-1 truncate text-left text-sky-300 hover:text-sky-200 hover:underline"
                >
                  {graphNodeDisplayLabel(graphCache, e.target)}
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}

      {triples_in.length > 0 && (
        <section>
          <h3 className="mb-1 text-xs font-semibold uppercase tracking-wider text-slate-400">
            Incoming ({triples_in.length})
          </h3>
          <ul className="space-y-1">
            {triples_in.slice(0, 12).map((e) => (
              <li
                key={e.id}
                className="flex items-center gap-2 rounded-md bg-slate-900 px-2 py-1 text-xs"
              >
                <button
                  type="button"
                  onClick={() => setSelectedNodeId(e.source)}
                  title={e.source}
                  className="min-w-0 flex-1 truncate text-left text-sky-300 hover:text-sky-200 hover:underline"
                >
                  {graphNodeDisplayLabel(graphCache, e.source)}
                </button>
                <span className="text-slate-400">{e.predicate ?? e.kind}</span>
              </li>
            ))}
          </ul>
        </section>
      )}

      <section>
        <div className="mb-1 flex items-center justify-between">
          <h3 className="text-xs font-semibold uppercase tracking-wider text-slate-400">
            {neighborHeading}
          </h3>
          <button
            type="button"
            onClick={handleShowSimilar}
            disabled={similar.loading && similar.forNodeId === selectedNodeId}
            className="rounded-md border border-slate-700 bg-slate-900 px-2 py-0.5 text-[10px] uppercase tracking-wider text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
            title={
              neighborKind === 'semantic'
                ? 'Fetch HNSW-similar nodes via /v1/graph/neighbors/:id?kind=semantic'
                : 'Fetch graph-related nodes via /v1/graph/neighbors/:id?kind=explicit'
            }
          >
            {similar.loading && similar.forNodeId === selectedNodeId
              ? neighborLoadingLabel
              : neighborButtonLabel}
          </button>
        </div>
        {similarForCurrent && similarForCurrent.error && (
          <p className="rounded-md border border-red-900 bg-red-950/50 px-2 py-1 text-xs text-red-300">
            {similarForCurrent.error}
          </p>
        )}
        {similarForCurrent && similarForCurrent.result && (
          <SimilarList
            result={similarForCurrent.result}
            emptyLabel={neighborEmptyLabel}
            onSelect={setSelectedNodeId}
          />
        )}
      </section>

      <section>
        <div className="mb-1 flex items-center justify-between">
          <h3 className="text-xs font-semibold uppercase tracking-wider text-slate-400">
            Contradictions
          </h3>
          <button
            type="button"
            onClick={handleLoadContradictions}
            disabled={contradictions.loading}
            className="rounded-md border border-slate-700 bg-slate-900 px-2 py-0.5 text-[10px] uppercase tracking-wider text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {contradictions.loading ? 'Loading...' : 'Load'}
          </button>
        </div>
        {contradictions.error && (
          <p className="rounded-md border border-red-900 bg-red-950/50 px-2 py-1 text-xs text-red-300">
            {contradictions.error}
          </p>
        )}
        {contradictions.items && (
          <ContradictionList
            items={contradictions.items}
            resolvingKey={contradictions.resolvingKey}
            onResolve={handleResolveContradiction}
          />
        )}
      </section>
    </div>
  );
}

function SearchMatchList({
  nodes,
  onSelect,
}: {
  nodes: GraphNode[];
  onSelect: (id: string) => void;
}) {
  return (
    <div className="flex flex-col gap-3 text-sm">
      <h2 className="text-xs font-semibold uppercase tracking-wider text-slate-400">
        Search matches
      </h2>
      <ul className="space-y-1">
        {nodes.map((node) => (
          <li key={node.id}>
            <button
              type="button"
              onClick={() => onSelect(node.id)}
              className="flex w-full items-center gap-2 rounded-md bg-slate-900 px-2 py-1.5 text-left text-xs text-slate-200 hover:bg-slate-800"
            >
              <KindBadge kind={node.kind} />
              <span className="min-w-0 flex-1 truncate">{node.label}</span>
            </button>
          </li>
        ))}
      </ul>
    </div>
  );
}

function EntityList({ result }: { result: EntityHit[] }) {
  if (result.length === 0) {
    return <p className="text-xs text-slate-400">No entity matches found.</p>;
  }
  return (
    <ul className="space-y-1">
      {result.map((entity) => (
        <li key={entity.entity_id} className="rounded-md bg-slate-900 px-2 py-1 text-xs">
          <div className="font-mono text-slate-200">{entity.entity_id}</div>
          <div className="text-slate-400">
            {entity.fact_count} facts
            {entity.predicates.length > 0 ? ` · ${entity.predicates.slice(0, 4).join(', ')}` : ''}
          </div>
        </li>
      ))}
    </ul>
  );
}

function ContradictionList({
  items,
  resolvingKey,
  onResolve,
}: {
  items: ContradictionHit[];
  resolvingKey: string | null;
  onResolve: (hit: ContradictionHit) => void;
}) {
  if (items.length === 0) {
    return <p className="text-xs text-slate-400">No contradictions found.</p>;
  }
  return (
    <ul className="space-y-1">
      {items.slice(0, 10).map((hit) => {
        const key = `${hit.a_id}:${hit.b_id}:${hit.kind}`;
        const resolved = hit.status === 'resolved';
        return (
          <li key={key} className="rounded-md bg-slate-900 px-2 py-1 text-xs">
            <div className="flex items-start justify-between gap-2">
              <div>
                <div className="font-medium text-slate-200">{hit.kind}</div>
                <div className="text-slate-400">{hit.explanation}</div>
              </div>
              <button
                type="button"
                onClick={() => onResolve(hit)}
                disabled={resolved || resolvingKey === key}
                className="rounded-md border border-slate-700 px-2 py-0.5 text-[10px] uppercase tracking-wider text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {resolved ? 'Resolved' : resolvingKey === key ? 'Saving...' : 'Resolve'}
              </button>
            </div>
          </li>
        );
      })}
    </ul>
  );
}

function SimilarList({
  result,
  emptyLabel,
  onSelect,
}: {
  result: GraphResponse;
  emptyLabel: string;
  onSelect: (id: string) => void;
}) {
  if (result.nodes.length === 0) {
    return <p className="text-xs text-slate-400">{emptyLabel}</p>;
  }
  return (
    <ul className="space-y-1">
      {result.nodes.slice(0, 8).map((n: GraphNode) => (
        <li key={n.id}>
          <button
            type="button"
            onClick={() => onSelect(n.id)}
            className="flex w-full items-center gap-2 rounded-md bg-slate-900 px-2 py-1 text-left text-xs text-slate-200 hover:bg-slate-800"
          >
            <KindBadge kind={n.kind} />
            <span className="flex-1 truncate">{n.label}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}

function neighborQueryKind(kind: NodeKind): 'semantic' | 'explicit' {
  return kind === 'episode' || kind === 'chunk' ? 'semantic' : 'explicit';
}

function graphNodeDisplayLabel(graph: GraphResponse | null, nodeId: string): string {
  return graph?.nodes.find((node) => node.id === nodeId)?.label ?? nodeId;
}
