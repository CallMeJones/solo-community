import {
  lazy,
  Suspense,
  useCallback,
  useDeferredValue,
  useMemo,
  useRef,
  useState,
  useEffect,
} from 'react';
import { useMutation, useQueryClient } from '@tanstack/react-query';
import {
  List,
  Graph,
  Cube,
  Plus,
  MagnifyingGlass,
  X,
  Funnel,
  FolderOpen,
  FileText,
  CaretRight,
} from '@phosphor-icons/react';
import { useGraphData } from '../hooks/useGraphData';
import { useGraphStore } from '../store/graphStore';
import { useThemeStore } from '../store/themeStore';
import { useSettingsStore } from '../store/settingsStore';
import { rememberMemory, errorMessage } from '../api/client';
import type { GraphResponse, NodeKind } from '../api/types';
import {
  exploreMemories,
  indexMemoryGroups,
  overviewGraph,
  sourceLabel,
} from '../lib/memoryExplorer';
import { buildGraphPresentation } from '../lib/graphPresentation';

const GraphView = lazy(() => import('./GraphView').then((m) => ({ default: m.GraphView })));
const InspectorPanel = lazy(() =>
  import('./InspectorPanel').then((m) => ({ default: m.InspectorPanel })),
);
const EMPTY: GraphResponse = { nodes: [], edges: [] };

/**
 * How wide the memory details panel may be dragged.
 *
 * The floor keeps the panel readable rather than letting it be dragged into a
 * sliver that then has to be dragged back out. The ceiling is a share of the
 * workspace rather than a fixed number, so widening the panel on a small
 * screen cannot leave the graph with nothing to draw in.
 */
const INSPECTOR_DEFAULT_WIDTH = 365;
const INSPECTOR_MIN_WIDTH = 300;
const INSPECTOR_MAX_FRACTION = 0.75;
const INSPECTOR_WIDTH_KEY = 'solo.memory.inspectorWidth';

function storedInspectorWidth(): number | null {
  try {
    const raw = Number(localStorage.getItem(INSPECTOR_WIDTH_KEY));
    return Number.isFinite(raw) && raw >= INSPECTOR_MIN_WIDTH ? raw : null;
  } catch {
    // Storage can be unavailable; the panel just opens at its default width.
    return null;
  }
}
const ALL_KINDS: NodeKind[] = ['episode', 'document', 'cluster', 'entity', 'chunk'];
const KIND_LABELS = {
  episode: 'Memories',
  document: 'Documents',
  cluster: 'Groups',
  entity: 'People & topics',
  chunk: 'Document sections',
};

export function MemoryWorkspace({ onImport }: { onImport: () => void }) {
  const query = useGraphData();
  const data = query.data ?? EMPTY;
  const state = useGraphStore();
  const labels = useThemeStore((s) => s.labels);
  const setLabels = useThemeStore((s) => s.setLabels);
  const connection = useSettingsStore((s) => s.connectionRevision);
  const lastConnection = useRef(connection);
  const [view, setView] = useState<'list' | '2d' | '3d'>('list');
  const [groupId, setGroupId] = useState<string | null>(null);
  const [focusId, setFocusId] = useState<string | null>(null);
  const [fullGraph, setFullGraph] = useState(false);
  const [source, setSource] = useState('');
  const [days, setDays] = useState(0);
  const [limit, setLimit] = useState(100);
  const [groupLimit, setGroupLimit] = useState(60);
  const [compact, setCompact] = useState(false);
  const groupPageSize = compact ? 4 : 60;
  useEffect(() => {
    if (typeof window.matchMedia !== 'function') return;
    const media = window.matchMedia('(max-width: 760px)');
    const update = () => setCompact(media.matches);
    update();
    media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, []);
  const [inspectorWidth, setInspectorWidth] = useState<number | null>(storedInspectorWidth);
  const stageRef = useRef<HTMLDivElement | null>(null);
  const [filters, setFilters] = useState(false);
  const [adding, setAdding] = useState(false);
  const [showMatches, setShowMatches] = useState(false);
  const deferredQuery = useDeferredValue(state.searchQuery);
  const index = useMemo(() => indexMemoryGroups(data), [data]);
  const since = useMemo(() => (days ? Date.now() - days * 86400000 : 0), [days]);
  const explored = useMemo(
    () =>
      exploreMemories(data, index, {
        groupId,
        focusId,
        query: deferredQuery,
        source,
        since,
        kinds: state.visibleKinds,
        expanded: state.expandedNodeIds,
        limit,
      }),
    [
      data,
      index,
      groupId,
      focusId,
      deferredQuery,
      source,
      since,
      state.visibleKinds,
      state.expandedNodeIds,
      limit,
    ],
  );
  const overview = useMemo(
    () => overviewGraph(data, index, explored.matches, groupLimit),
    [data, index, explored.matches, groupLimit],
  );
  const isOverview = !groupId && !focusId && !fullGraph && !deferredQuery.trim();
  const displayGraph = isOverview ? overview.graph : explored.graph;
  const presentation = useMemo(
    () => buildGraphPresentation(displayGraph, new Set(ALL_KINDS), new Set(), deferredQuery),
    [displayGraph, deferredQuery],
  );
  const selected = index.byId.get(state.selectedNodeId || '');
  const selectedGroup = index.groups.find((g) => g.id === groupId);
  const sources = useMemo(
    () => [...new Set(data.nodes.map((n) => n.source_type).filter((s): s is string => !!s))].sort(),
    [data],
  );
  useEffect(() => {
    setLimit(100);
    setGroupLimit(groupPageSize);
  }, [groupId, focusId, deferredQuery, source, days, state.visibleKinds, groupPageSize]);
  useEffect(() => {
    if (lastConnection.current === connection) return;
    lastConnection.current = connection;
    setGroupId(null);
    setFocusId(null);
    setSource('');
    state.setSelectedNodeId(null);
    state.clearExpansions();
  }, [connection]); // eslint-disable-line react-hooks/exhaustive-deps
  /** Clamp to the floor, and to a share of the workspace actually on screen. */
  const clampInspector = useCallback((width: number) => {
    const stage = stageRef.current?.getBoundingClientRect().width ?? 0;
    const ceiling = stage > 0 ? stage * INSPECTOR_MAX_FRACTION : width;
    return Math.round(
      Math.min(Math.max(width, INSPECTOR_MIN_WIDTH), Math.max(ceiling, INSPECTOR_MIN_WIDTH)),
    );
  }, []);

  const applyInspectorWidth = useCallback(
    (width: number) => {
      const next = clampInspector(width);
      setInspectorWidth(next);
      try {
        localStorage.setItem(INSPECTOR_WIDTH_KEY, String(next));
      } catch {
        // The drag still works for this session.
      }
    },
    [clampInspector],
  );

  /**
   * Drag the divider.
   *
   * Pointer capture matters here: without it the drag dies the moment the
   * pointer crosses the graph canvas, which handles its own pointer events.
   */
  const startInspectorDrag = (event: React.PointerEvent<HTMLDivElement>) => {
    event.preventDefault();
    const handle = event.currentTarget;
    handle.setPointerCapture(event.pointerId);
    const move = (moved: PointerEvent) => {
      const right = stageRef.current?.getBoundingClientRect().right ?? window.innerWidth;
      applyInspectorWidth(right - moved.clientX);
    };
    const stop = () => {
      handle.releasePointerCapture?.(event.pointerId);
      handle.removeEventListener('pointermove', move);
      handle.removeEventListener('pointerup', stop);
      handle.removeEventListener('pointercancel', stop);
    };
    handle.addEventListener('pointermove', move);
    handle.addEventListener('pointerup', stop);
    handle.addEventListener('pointercancel', stop);
  };

  const openGroup = (id: string) => {
    setGroupId(id);
    setFocusId(null);
    setShowMatches(false);
    state.setSelectedNodeId(null);
  };
  const showEverything = (on: boolean) => {
    setFullGraph(on);
    setGroupId(null);
    setFocusId(null);
  };
  const resetScope = () => {
    setGroupId(null);
    setFocusId(null);
    setFullGraph(false);
    state.clearExpansions();
  };
  const switchView = (next: typeof view) => {
    setView(next);
    if (next !== 'list') state.setViewMode(next);
  };

  return (
    <section className="memory-workspace" aria-label="Memories workspace">
      <header className="memory-header">
        <div>
          <h1>Memories</h1>
          <p>Find, explore, and make the most of what you know.</p>
        </div>
        <label className="memory-search">
          <MagnifyingGlass size={20} />
          <span className="sr-only">Search your memories</span>
          <input
            placeholder="Search your memories…"
            value={state.searchQuery}
            onChange={(e) => state.setSearchQuery(e.target.value)}
          />
          {state.searchQuery && (
            <button aria-label="Clear search" onClick={() => state.setSearchQuery('')}>
              <X />
            </button>
          )}
        </label>
        <button className="workspace-primary" onClick={() => setAdding(true)}>
          <Plus size={19} />
          Add memory
        </button>
      </header>
      <div className="memory-controls">
        <div className="view-switch" role="group" aria-label="Memory view">
          {(
            [
              { id: 'list', label: 'List', Icon: List },
              { id: '2d', label: '2D', Icon: Graph },
              { id: '3d', label: '3D', Icon: Cube },
            ] as const
          ).map(({ id, label, Icon }) => (
            <button key={id} aria-pressed={view === id} onClick={() => switchView(id)}>
              <Icon size={20} />
              {label}
            </button>
          ))}
        </div>
        <span className="library-label">Local library</span>
        <button
          className="workspace-button"
          aria-expanded={filters}
          onClick={() => setFilters(!filters)}
        >
          <Funnel />
          Filters{source || days || state.visibleKinds.size !== 4 ? ' · active' : ''}
        </button>
        <button className="workspace-button" onClick={onImport}>
          Import
        </button>
      </div>
      {filters && (
        <div className="memory-filters">
          <label>
            Source
            <select value={source} onChange={(e) => setSource(e.target.value)}>
              <option value="">All sources</option>
              {sources.map((s) => (
                <option key={s} value={s}>
                  {sourceLabel(s)}
                </option>
              ))}
            </select>
          </label>
          <label>
            Added
            <select value={days} onChange={(e) => setDays(Number(e.target.value))}>
              <option value={0}>Any time</option>
              <option value={7}>Last 7 days</option>
              <option value={30}>Last 30 days</option>
              <option value={90}>Last 90 days</option>
            </select>
          </label>
          {ALL_KINDS.map((kind) => (
            <label key={kind}>
              <input
                type="checkbox"
                checked={state.visibleKinds.has(kind)}
                onChange={() => state.toggleKind(kind)}
              />
              {KIND_LABELS[kind]}
            </label>
          ))}
          {/* Neither of these filters what is shown, but they belong with the
              other checkboxes rather than in the control row: they are the
              graph's display switches and there is nowhere else for them to
              live. The full-graph one is also in the breadcrumb, which is
              where it is reachable from inside a group -- both go through
              showEverything so they cannot disagree. */}
          <label>
            <input
              type="checkbox"
              checked={labels}
              onChange={(e) => setLabels(e.target.checked)}
            />
            Graph labels
          </label>
          <label>
            <input
              type="checkbox"
              checked={fullGraph}
              onChange={(e) => showEverything(e.target.checked)}
            />
            Show every memory
          </label>
          <button
            className="workspace-button"
            onClick={() => {
              setSource('');
              setDays(0);
              state.setSearchQuery('');
              useGraphStore.setState({
                visibleKinds: new Set(ALL_KINDS.filter((k) => k !== 'chunk')),
              });
            }}
          >
            Clear filters
          </button>
        </div>
      )}
      <div className="memory-context">
        <button onClick={resetScope}>All memories</button>
        {selectedGroup && (
          <>
            <CaretRight />
            <button onClick={() => setFocusId(null)}>{selectedGroup.label}</button>
          </>
        )}
        {focusId && (
          <>
            <CaretRight />
            <span>{index.byId.get(focusId)?.label || 'Memory connections'}</span>
          </>
        )}
        <span className="context-count">
          {view !== 'list' && isOverview
            ? `${overview.graph.nodes.length} of ${overview.groups.length} groups · ${explored.matches.length} items`
            : `Showing ${explored.graph.nodes.length} of ${explored.matches.length} items`}
        </span>
        {view !== 'list' && (
          <button onClick={() => showEverything(!fullGraph)}>
            {fullGraph ? 'Grouped overview' : 'Full graph'}
          </button>
        )}
      </div>
      {query.isError ? (
        <div className="workspace-empty" role="alert">
          <h2>Couldn’t open your library</h2>
          <p>{errorMessage(query.error)}</p>
          <button className="workspace-button" onClick={() => void query.refetch()}>
            Try again
          </button>
        </div>
      ) : query.isLoading ? (
        <div className="workspace-empty" role="status">
          Opening your library…
        </div>
      ) : !data.nodes.length ? (
        <div className="workspace-empty">
          <FolderOpen size={48} />
          <h2>Your memory starts here</h2>
          <p>Add something worth remembering, or import a conversation.</p>
          <button className="workspace-primary" onClick={() => setAdding(true)}>
            Add your first memory
          </button>
          <button className="workspace-button" onClick={onImport}>
            Import conversations
          </button>
        </div>
      ) : (
        <div className="memory-body" ref={stageRef}>
          <div className="memory-content">
            {view === 'list' ? (
              <div className="memory-list" aria-label="Memory list">
                {!groupId && !focusId && !deferredQuery && (
                  <div className="group-browser">
                    <h2>Browse groups</h2>
                    <div>
                      {overview.groups.slice(0, showMatches ? undefined : 6).map((g) => (
                        <button key={g.id} onClick={() => openGroup(g.id)}>
                          <FolderOpen />
                          <span>{g.label}</span>
                          <small>{g.members.length}</small>
                          <CaretRight />
                        </button>
                      ))}
                    </div>
                    {overview.groups.length > 6 && (
                      <button
                        className="workspace-button"
                        onClick={() => setShowMatches(!showMatches)}
                      >
                        {showMatches ? 'Fewer groups' : `All ${overview.groups.length} groups`}
                      </button>
                    )}
                  </div>
                )}
                {explored.graph.nodes.map((n) => (
                  <button
                    className="memory-row"
                    key={n.id}
                    aria-pressed={n.id === state.selectedNodeId}
                    onClick={() => state.setSelectedNodeId(n.id)}
                  >
                    <FileText size={23} />
                    <span>
                      <strong>{n.label}</strong>
                      <span>{n.preview || KIND_LABELS[n.kind]}</span>
                      <small>
                        {n.source_type ? sourceLabel(n.source_type) : KIND_LABELS[n.kind]}
                        {n.ts_ms
                          ? ` · ${new Date(n.ts_ms).toLocaleDateString(undefined, { day: 'numeric', month: 'short', year: 'numeric' })}`
                          : ''}
                      </small>
                    </span>
                    <CaretRight />
                  </button>
                ))}
                {!explored.matches.length && (
                  <div className="workspace-empty">
                    <h2>No matching memories</h2>
                    <p>Try another search or clear a filter.</p>
                  </div>
                )}
              </div>
            ) : (
              <>
                <Suspense fallback={<div className="workspace-empty">Preparing graph…</div>}>
                  <GraphView
                    presentation={presentation}
                    onOpenGroup={isOverview ? openGroup : undefined}
                    onFocusNode={(id) => {
                      setFocusId(id);
                      state.setSearchQuery('');
                    }}
                  />
                </Suspense>
                {isOverview && (
                  <div className="graph-group-guide">
                    <span>Choose a group to explore</span>
                    <select
                      aria-label="Open graph group"
                      value=""
                      onChange={(e) => openGroup(e.target.value)}
                    >
                      <option value="" disabled>
                        Select a group…
                      </option>
                      {overview.groups.map((g) => (
                        <option key={g.id} value={g.id}>
                          {g.label} ({g.members.length})
                        </option>
                      ))}
                    </select>
                  </div>
                )}
                {!isOverview && !explored.matches.length && (
                  <div className="graph-no-results">
                    No matching memories. Try another search or filter.
                  </div>
                )}
              </>
            )}
            {isOverview && view !== 'list' && overview.groups.length > groupLimit && (
              <div className="memory-load-more">
                <button
                  className="workspace-button"
                  onClick={() => setGroupLimit(groupLimit + groupPageSize)}
                >
                  Show {groupPageSize} more groups · {overview.groups.length - groupLimit} remaining
                </button>
              </div>
            )}
            {(!isOverview || view === 'list') && explored.matches.length > limit && (
              <div className="memory-load-more">
                <button className="workspace-button" onClick={() => setLimit(limit + 100)}>
                  Show 100 more · {explored.matches.length - limit} remaining
                </button>
              </div>
            )}
          </div>
          {selected && (
            <div
              className="inspector-resize"
              role="separator"
              aria-orientation="vertical"
              aria-label="Resize memory details"
              tabIndex={0}
              onPointerDown={startInspectorDrag}
              onDoubleClick={() => applyInspectorWidth(INSPECTOR_DEFAULT_WIDTH)}
              onKeyDown={(event) => {
                // Arrow keys move it too: dragging is not available to
                // everyone, and this is a control rather than decoration.
                const step = event.shiftKey ? 64 : 16;
                const current = inspectorWidth ?? INSPECTOR_DEFAULT_WIDTH;
                if (event.key === 'ArrowLeft') {
                  event.preventDefault();
                  applyInspectorWidth(current + step);
                } else if (event.key === 'ArrowRight') {
                  event.preventDefault();
                  applyInspectorWidth(current - step);
                }
              }}
            />
          )}
          {selected && (
            <aside
              className="memory-inspector"
              aria-label="Memory details"
              style={
                inspectorWidth ? { flexBasis: inspectorWidth, width: inspectorWidth } : undefined
              }
            >
              <div className="inspector-actions">
                <button
                  className="workspace-button"
                  onClick={() => {
                    setFocusId(selected.id);
                    state.setSearchQuery('');
                    state.clearExpansions();
                    switchView(view === 'list' ? '2d' : view);
                  }}
                >
                  Explore connections
                </button>
                <button
                  className="workspace-button"
                  aria-label="Close memory details"
                  onClick={() => state.setSelectedNodeId(null)}
                >
                  <X size={20} />
                </button>
              </div>
              <Suspense fallback={<p>Loading memory…</p>}>
                <InspectorPanel
                  embedded
                  onRevealConnections={(id) => {
                    setFocusId(id);
                    state.setSearchQuery('');
                    switchView(view === 'list' ? '2d' : view);
                  }}
                />
              </Suspense>
              {focusId && (
                <button
                  className="workspace-button"
                  onClick={() => state.toggleExpansion(selected.id)}
                >
                  {' '}
                  {state.expandedNodeIds.has(selected.id) ? 'Collapse' : 'Expand'} connections
                </button>
              )}
            </aside>
          )}
        </div>
      )}
      {adding && <AddMemory onClose={() => setAdding(false)} />}
    </section>
  );
}

function AddMemory({ onClose }: { onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [content, setContent] = useState('');
  const client = useQueryClient();
  const save = useMutation({
    mutationFn: () => rememberMemory({ content: content.trim(), source_type: 'user_message' }),
    onSuccess: async (result) => {
      await client.invalidateQueries({ queryKey: ['graph'] });
      useGraphStore.getState().setSelectedNodeId(`ep:${result.memory_id}`);
      onClose();
    },
  });
  useEffect(() => {
    dialog.current?.showModal();
    dialog.current?.querySelector('textarea')?.focus();
  }, []);
  return (
    <dialog
      ref={dialog}
      className="memory-dialog"
      onCancel={(e) => {
        if (save.isPending) e.preventDefault();
        else onClose();
      }}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (content.trim() && !save.isPending) save.mutate();
        }}
      >
        <div className="dialog-heading">
          <h2>Add a memory</h2>
          <button
            tabIndex={-1}
            type="button"
            disabled={save.isPending}
            aria-label="Close"
            onClick={onClose}
          >
            <X size={22} />
          </button>
        </div>
        <label htmlFor="new-memory">What would you like Solo to remember?</label>
        <textarea
          id="new-memory"
          autoFocus
          required
          value={content}
          onChange={(e) => setContent(e.target.value)}
          rows={7}
        />
        <p>Saved to your local library.</p>
        {save.isError && <p role="alert">{errorMessage(save.error)}</p>}
        <button className="workspace-primary" disabled={!content.trim() || save.isPending}>
          {save.isPending ? 'Saving…' : 'Save memory'}
        </button>
      </form>
    </dialog>
  );
}
