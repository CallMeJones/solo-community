import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useMemo, useState } from 'react';
import {
  fetchContradictions,
  fetchInbox,
  fetchInspect,
  forgetMemory,
  reviewMemory,
  resolveContradiction,
  updateMemory,
} from '../api/client';
import type { ContradictionHit, MemoryInboxItem, MemoryReviewRequestState } from '../api/types';
import { COMMUNITY_LIBRARY_NAME, useGraphStore } from '../store/graphStore';
import { useSettingsStore } from '../store/settingsStore';
import { CopyButton } from './ui/CopyButton';

const EPISODE_LIMIT = 50;
const CONTRADICTION_LIMIT = 20;

type ReviewFilter = 'all' | 'needs_review' | 'approved' | 'dismissed';

const REVIEW_FILTERS: Array<{ value: ReviewFilter; label: string }> = [
  { value: 'all', label: 'All states' },
  { value: 'needs_review', label: 'Needs review' },
  { value: 'approved', label: 'Approved' },
  { value: 'dismissed', label: 'Dismissed' },
];

interface EpisodeEditState {
  id: string;
  label: string;
  text: string;
  loading: boolean;
  saving: boolean;
  error: string | null;
  saved: boolean;
}

interface InboxViewProps {
  onSelectEpisode?: (id: string) => void;
}

export function InboxView({ onSelectEpisode }: InboxViewProps) {
  const queryClient = useQueryClient();
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const setSelectedNodeId = useGraphStore((s) => s.setSelectedNodeId);
  const inboxQuery = useQuery<MemoryInboxItem[]>({
    queryKey: ['memory-inbox', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchInbox(EPISODE_LIMIT, { signal }),
    staleTime: 30_000,
  });
  const contradictionsQuery = useQuery<ContradictionHit[]>({
    queryKey: ['memory-contradictions', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchContradictions(CONTRADICTION_LIMIT, { signal }),
    staleTime: 30_000,
  });

  const recentEpisodes = useMemo(() => {
    return [...(inboxQuery.data ?? [])]
      .sort((a, b) => (b.ts_ms ?? 0) - (a.ts_ms ?? 0))
      .slice(0, EPISODE_LIMIT);
  }, [inboxQuery.data]);

  const contradictions = useMemo(() => {
    return [...(contradictionsQuery.data ?? [])]
      .sort(compareContradictions)
      .slice(0, CONTRADICTION_LIMIT);
  }, [contradictionsQuery.data]);

  const [editing, setEditing] = useState<EpisodeEditState | null>(null);
  const [forgettingId, setForgettingId] = useState<string | null>(null);
  const [reviewingId, setReviewingId] = useState<string | null>(null);
  const [episodeActionError, setEpisodeActionError] = useState<string | null>(null);
  const [bulkReviewingState, setBulkReviewingState] = useState<MemoryReviewRequestState | null>(
    null,
  );
  const [reviewFilter, setReviewFilter] = useState<ReviewFilter>('all');
  const [sourceFilter, setSourceFilter] = useState('all');
  const [resolvingKey, setResolvingKey] = useState<string | null>(null);
  const [contradictionActionError, setContradictionActionError] = useState<string | null>(null);

  const reviewCounts = useMemo(() => countReviewStates(recentEpisodes), [recentEpisodes]);
  const sourceOptions = useMemo(() => uniqueSources(recentEpisodes), [recentEpisodes]);
  const visibleEpisodes = useMemo(
    () =>
      recentEpisodes.filter(
        (episode) =>
          (reviewFilter === 'all' || reviewStateOf(episode) === reviewFilter) &&
          (sourceFilter === 'all' || episode.source_type === sourceFilter),
      ),
    [recentEpisodes, reviewFilter, sourceFilter],
  );
  const inboxSummary = useMemo(
    () =>
      buildInboxSummary({
        loadedCount: recentEpisodes.length,
        visibleCount: visibleEpisodes.length,
        reviewFilter,
        sourceFilter,
        sourceOptions,
        reviewCounts,
        contradictionCount: contradictions.length,
      }),
    [
      recentEpisodes.length,
      visibleEpisodes.length,
      reviewFilter,
      sourceFilter,
      sourceOptions,
      reviewCounts,
      contradictions.length,
    ],
  );

  const handleOpenEpisode = (id: string) => {
    const graphId = episodeGraphId(id);
    setSelectedNodeId(graphId);
    onSelectEpisode?.(graphId);
  };

  const handleEditEpisode = async (episode: MemoryInboxItem) => {
    setEpisodeActionError(null);
    setEditing({
      id: episode.memory_id,
      label: episode.label,
      text: '',
      loading: true,
      saving: false,
      error: null,
      saved: false,
    });
    try {
      const inspect = await fetchInspect(episodeGraphId(episode.memory_id), {});
      if (typeof inspect.full_text !== 'string' || inspect.full_text.trim().length === 0) {
        throw new Error('No editable episode text returned by inspect.');
      }
      setEditing((current) =>
        current?.id === episode.memory_id
          ? { ...current, text: inspect.full_text ?? '', loading: false }
          : current,
      );
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setEditing((current) =>
        current?.id === episode.memory_id
          ? { ...current, loading: false, error: message }
          : current,
      );
    }
  };

  const handleSaveEdit = async () => {
    if (!editing || editing.text.trim().length === 0) return;
    const id = editing.id;
    setEditing((current) =>
      current?.id === id ? { ...current, saving: true, error: null, saved: false } : current,
    );
    try {
      const updated = await updateMemory(id, editing.text, {});
      const graphId = episodeGraphId(id);
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['memory-inbox'] }),
        queryClient.invalidateQueries({ queryKey: ['graph'] }),
        queryClient.invalidateQueries({ queryKey: ['inspect', graphId] }),
      ]);
      setEditing((current) =>
        current?.id === id
          ? { ...current, text: updated.content, saving: false, error: null, saved: true }
          : current,
      );
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setEditing((current) =>
        current?.id === id ? { ...current, saving: false, error: message } : current,
      );
    }
  };

  const handleForgetEpisode = async (episode: MemoryInboxItem) => {
    if (!window.confirm(`Forget memory "${episode.label}"?`)) return;
    setEpisodeActionError(null);
    setForgettingId(episode.memory_id);
    try {
      const graphId = episodeGraphId(episode.memory_id);
      await forgetMemory(episode.memory_id, 'Forgotten from Solo Memory inbox', {});
      if (useGraphStore.getState().selectedNodeId === graphId) {
        setSelectedNodeId(null);
      }
      setEditing((current) => (current?.id === episode.memory_id ? null : current));
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['memory-inbox'] }),
        queryClient.invalidateQueries({ queryKey: ['graph'] }),
        queryClient.invalidateQueries({ queryKey: ['inspect', graphId] }),
      ]);
    } catch (err) {
      setEpisodeActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setForgettingId(null);
    }
  };

  const handleReviewEpisode = async (episode: MemoryInboxItem, state: MemoryReviewRequestState) => {
    setEpisodeActionError(null);
    setReviewingId(episode.memory_id);
    try {
      await reviewMemory(episode.memory_id, state, {
        note: reviewNoteFor(state),
      });
      await queryClient.invalidateQueries({ queryKey: ['memory-inbox'] });
    } catch (err) {
      setEpisodeActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setReviewingId(null);
    }
  };

  const handleBulkReview = async (state: MemoryReviewRequestState) => {
    const targets = bulkReviewTargets(visibleEpisodes, state);
    if (targets.length === 0) return;
    setEpisodeActionError(null);
    setBulkReviewingState(state);
    try {
      for (const episode of targets) {
        await reviewMemory(episode.memory_id, state, {
          note: bulkReviewNoteFor(state),
        });
      }
      await queryClient.invalidateQueries({ queryKey: ['memory-inbox'] });
    } catch (err) {
      setEpisodeActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setBulkReviewingState(null);
    }
  };

  const handleResolveContradiction = async (hit: ContradictionHit) => {
    const key = contradictionKey(hit);
    setContradictionActionError(null);
    setResolvingKey(key);
    try {
      const resolved = await resolveContradiction(hit, {
        note: 'Resolved from Solo Memory inbox',
        winningTripleId: hit.winning_triple_id ?? undefined,
      });
      queryClient.setQueryData<ContradictionHit[]>(
        ['memory-contradictions', apiUrl, connectionRevision],
        (items) =>
          items?.map((item) =>
            contradictionKey(item) === key ? { ...item, ...resolved } : item,
          ) ?? [],
      );
      await Promise.all([
        queryClient.invalidateQueries({ queryKey: ['graph'] }),
        queryClient.invalidateQueries({ queryKey: ['inspect'] }),
      ]);
    } catch (err) {
      setContradictionActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setResolvingKey(null);
    }
  };

  return (
    <div
      className="h-full overflow-y-auto bg-slate-950 px-6 py-5 text-sm text-slate-100"
      tabIndex={0}
      aria-label="Memory inbox"
    >
      <div className="mx-auto flex max-w-6xl flex-col gap-5">
        <div className="flex flex-wrap items-end justify-between gap-3 border-b border-slate-800 pb-4">
          <div>
            <h1 className="text-lg font-semibold tracking-tight">Memory inbox</h1>
            <p className="mt-1 text-xs text-slate-400">
              Recent episodes and contradiction signals in memory library{' '}
              <span className="text-slate-400">{COMMUNITY_LIBRARY_NAME}</span>
            </p>
          </div>
          <div className="flex gap-2 text-xs">
            <Metric label="Loaded" value={recentEpisodes.length} />
            <Metric label="Visible" value={visibleEpisodes.length} />
            <Metric label="Needs review" value={reviewCounts.needs_review} />
            <Metric label="Contradictions" value={contradictions.length} />
          </div>
        </div>

        <div className="grid gap-5 lg:grid-cols-[minmax(0,1.2fr)_minmax(320px,0.8fr)]">
          <section aria-label="Recent episodes" className="min-w-0">
            <SectionHeader title="Review queue" loading={inboxQuery.isLoading} />
            <ReviewControls
              reviewFilter={reviewFilter}
              sourceFilter={sourceFilter}
              sourceOptions={sourceOptions}
              visibleCount={visibleEpisodes.length}
              loadedCount={recentEpisodes.length}
              summary={inboxSummary}
              bulkReviewingState={bulkReviewingState}
              canApproveVisible={bulkReviewTargets(visibleEpisodes, 'approved').length > 0}
              canDismissVisible={bulkReviewTargets(visibleEpisodes, 'dismissed').length > 0}
              canResetVisible={bulkReviewTargets(visibleEpisodes, 'needs_review').length > 0}
              onReviewFilterChange={setReviewFilter}
              onSourceFilterChange={setSourceFilter}
              onBulkReview={handleBulkReview}
            />
            {inboxQuery.error && <ErrorMessage message={String(inboxQuery.error)} />}
            {episodeActionError && <ErrorMessage message={episodeActionError} />}
            {!inboxQuery.isLoading && !inboxQuery.error && (
              <EpisodeList
                episodes={visibleEpisodes}
                editing={editing}
                forgettingId={forgettingId}
                reviewingId={reviewingId}
                emptyMessage={
                  recentEpisodes.length === 0
                    ? 'No recent episode memories found.'
                    : 'No memories match the current filters.'
                }
                onOpenEpisode={handleOpenEpisode}
                onEditEpisode={handleEditEpisode}
                onEditTextChange={(text) =>
                  setEditing((current) =>
                    current ? { ...current, text, error: null, saved: false } : current,
                  )
                }
                onSaveEdit={handleSaveEdit}
                onCancelEdit={() => setEditing(null)}
                onForgetEpisode={handleForgetEpisode}
                onReviewEpisode={handleReviewEpisode}
              />
            )}
          </section>

          <section aria-label="Contradictions" className="min-w-0">
            <SectionHeader title="Contradictions" loading={contradictionsQuery.isLoading} />
            {contradictionsQuery.error && (
              <ErrorMessage message={String(contradictionsQuery.error)} />
            )}
            {contradictionActionError && <ErrorMessage message={contradictionActionError} />}
            {!contradictionsQuery.isLoading && !contradictionsQuery.error && (
              <ContradictionInboxList
                items={contradictions}
                resolvingKey={resolvingKey}
                onResolve={handleResolveContradiction}
              />
            )}
          </section>
        </div>
      </div>
    </div>
  );
}

function ReviewControls({
  reviewFilter,
  sourceFilter,
  sourceOptions,
  visibleCount,
  loadedCount,
  summary,
  bulkReviewingState,
  canApproveVisible,
  canDismissVisible,
  canResetVisible,
  onReviewFilterChange,
  onSourceFilterChange,
  onBulkReview,
}: {
  reviewFilter: ReviewFilter;
  sourceFilter: string;
  sourceOptions: string[];
  visibleCount: number;
  loadedCount: number;
  summary: string;
  bulkReviewingState: MemoryReviewRequestState | null;
  canApproveVisible: boolean;
  canDismissVisible: boolean;
  canResetVisible: boolean;
  onReviewFilterChange: (filter: ReviewFilter) => void;
  onSourceFilterChange: (filter: string) => void;
  onBulkReview: (state: MemoryReviewRequestState) => void;
}) {
  const busy = bulkReviewingState !== null;
  return (
    <div
      aria-label="Inbox review controls"
      className="mb-3 grid gap-3 rounded-md border border-slate-800 bg-slate-900 p-3 xl:grid-cols-[minmax(140px,0.8fr)_minmax(160px,0.9fr)_minmax(0,1.6fr)]"
    >
      <label className="grid gap-1 text-xs text-slate-400">
        Review
        <select
          value={reviewFilter}
          onChange={(event) => onReviewFilterChange(event.target.value as ReviewFilter)}
          className="h-9 rounded-md border border-slate-700 bg-slate-950 px-2 text-sm text-slate-100 outline-none focus:border-sky-500"
          aria-label="Review filter"
        >
          {REVIEW_FILTERS.map((filter) => (
            <option key={filter.value} value={filter.value}>
              {filter.label}
            </option>
          ))}
        </select>
      </label>
      <label className="grid gap-1 text-xs text-slate-400">
        Source
        <select
          value={sourceFilter}
          onChange={(event) => onSourceFilterChange(event.target.value)}
          className="h-9 rounded-md border border-slate-700 bg-slate-950 px-2 text-sm text-slate-100 outline-none focus:border-sky-500"
          aria-label="Source filter"
        >
          <option value="all">All sources</option>
          {sourceOptions.map((source) => (
            <option key={source} value={source}>
              {formatSourceType(source)}
            </option>
          ))}
        </select>
      </label>
      <div className="flex flex-wrap items-end justify-start gap-2">
        <CopyButton label="Copy summary" value={summary} />
        <BulkReviewButton
          label="Approve visible"
          busyLabel="Approving..."
          busy={bulkReviewingState === 'approved'}
          disabled={busy || !canApproveVisible}
          onClick={() => onBulkReview('approved')}
        />
        <BulkReviewButton
          label="Dismiss visible"
          busyLabel="Dismissing..."
          busy={bulkReviewingState === 'dismissed'}
          disabled={busy || !canDismissVisible}
          onClick={() => onBulkReview('dismissed')}
        />
        <BulkReviewButton
          label="Reset visible"
          busyLabel="Resetting..."
          busy={bulkReviewingState === 'needs_review'}
          disabled={busy || !canResetVisible}
          onClick={() => onBulkReview('needs_review')}
        />
        <span className="ml-auto self-center whitespace-nowrap text-xs text-slate-400">
          {visibleCount} of {loadedCount}
        </span>
      </div>
    </div>
  );
}

function BulkReviewButton({
  label,
  busyLabel,
  busy,
  disabled,
  onClick,
}: {
  label: string;
  busyLabel: string;
  busy: boolean;
  disabled: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-50"
    >
      {busy ? busyLabel : label}
    </button>
  );
}

function SectionHeader({ title, loading }: { title: string; loading: boolean }) {
  return (
    <div className="mb-2 flex items-center justify-between">
      <h2 className="text-xs font-semibold uppercase tracking-wider text-slate-400">{title}</h2>
      {loading && <span className="text-xs text-slate-400">Loading...</span>}
    </div>
  );
}

function EpisodeList({
  episodes,
  editing,
  forgettingId,
  reviewingId,
  emptyMessage,
  onOpenEpisode,
  onEditEpisode,
  onEditTextChange,
  onSaveEdit,
  onCancelEdit,
  onForgetEpisode,
  onReviewEpisode,
}: {
  episodes: MemoryInboxItem[];
  editing: EpisodeEditState | null;
  forgettingId: string | null;
  reviewingId: string | null;
  emptyMessage: string;
  onOpenEpisode: (id: string) => void;
  onEditEpisode: (episode: MemoryInboxItem) => void;
  onEditTextChange: (text: string) => void;
  onSaveEdit: () => void;
  onCancelEdit: () => void;
  onForgetEpisode: (episode: MemoryInboxItem) => void;
  onReviewEpisode: (episode: MemoryInboxItem, state: MemoryReviewRequestState) => void;
}) {
  if (episodes.length === 0) {
    return <EmptyMessage>{emptyMessage}</EmptyMessage>;
  }

  return (
    <ul className="space-y-2">
      {episodes.map((episode) => (
        <li key={episode.memory_id} className="rounded-md border border-slate-800 bg-slate-900 p-3">
          {editing?.id === episode.memory_id ? (
            <EpisodeEditor
              editing={editing}
              onTextChange={onEditTextChange}
              onSave={onSaveEdit}
              onCancel={onCancelEdit}
            />
          ) : null}
          <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
            <div className="min-w-0">
              <div className="flex flex-wrap items-center gap-2">
                <KindPill />
                <SourcePill source={episode.source_type} />
                <ReviewPill state={episode.review_state ?? null} />
                <time className="font-mono text-[11px] text-slate-400">
                  {formatTimestamp(episode.ts_ms)}
                </time>
              </div>
              <p className="mt-2 text-sm leading-5 text-slate-100">{episode.label}</p>
              {episode.preview && episode.preview !== episode.label && (
                <p className="mt-1 line-clamp-2 text-xs leading-5 text-slate-400">
                  {episode.preview}
                </p>
              )}
              <p className="mt-2 truncate font-mono text-[11px] text-slate-400">
                {episode.memory_id}
              </p>
              {episode.review_note && (
                <p className="mt-2 line-clamp-2 text-xs text-slate-400">{episode.review_note}</p>
              )}
            </div>
            <div className="grid shrink-0 grid-cols-2 gap-1 sm:flex sm:min-w-24 sm:flex-col">
              <button
                type="button"
                onClick={() => onOpenEpisode(episode.memory_id)}
                className="shrink-0 rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:border-sky-600 hover:text-sky-300"
                aria-label={`Open ${episode.label} in graph`}
              >
                Open
              </button>
              <button
                type="button"
                onClick={() => onEditEpisode(episode)}
                disabled={editing?.loading || editing?.saving}
                className="shrink-0 rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
                aria-label={`Edit ${episode.label}`}
              >
                Edit
              </button>
              <button
                type="button"
                onClick={() => onForgetEpisode(episode)}
                disabled={forgettingId === episode.memory_id}
                className="shrink-0 rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:border-red-600 hover:text-red-300 disabled:cursor-not-allowed disabled:opacity-50"
                aria-label={`Forget ${episode.label}`}
              >
                {forgettingId === episode.memory_id ? 'Forgetting...' : 'Forget'}
              </button>
              <ReviewButtons
                episode={episode}
                busy={reviewingId === episode.memory_id}
                onReview={onReviewEpisode}
              />
            </div>
          </div>
        </li>
      ))}
    </ul>
  );
}

function EpisodeEditor({
  editing,
  onTextChange,
  onSave,
  onCancel,
}: {
  editing: EpisodeEditState;
  onTextChange: (text: string) => void;
  onSave: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="mb-3 rounded-md border border-slate-700 bg-slate-950 p-3">
      <div className="mb-2 flex items-center justify-between gap-2">
        <span className="truncate text-xs font-medium text-slate-300">Editing {editing.label}</span>
        <button
          type="button"
          onClick={onCancel}
          className="text-xs text-slate-400 hover:text-slate-300"
        >
          Cancel
        </button>
      </div>
      {editing.loading ? (
        <p className="text-xs text-slate-400">Loading full text...</p>
      ) : (
        <>
          <textarea
            value={editing.text}
            onChange={(event) => onTextChange(event.target.value)}
            className="min-h-28 w-full resize-y rounded-md border border-slate-800 bg-slate-950 p-2 text-xs text-slate-200 outline-none focus:border-emerald-600"
            aria-label={`Edit text for ${editing.label}`}
          />
          <div className="mt-2 flex items-center justify-between gap-2">
            <div>
              {editing.error && (
                <span role="alert" className="text-xs text-red-300">
                  {editing.error}
                </span>
              )}
              {editing.saved && <span className="text-xs text-emerald-300">Saved</span>}
            </div>
            <button
              type="button"
              onClick={onSave}
              disabled={editing.saving || editing.text.trim().length === 0}
              className="rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {editing.saving ? 'Saving...' : 'Save'}
            </button>
          </div>
        </>
      )}
    </div>
  );
}

function ContradictionInboxList({
  items,
  resolvingKey,
  onResolve,
}: {
  items: ContradictionHit[];
  resolvingKey: string | null;
  onResolve: (hit: ContradictionHit) => void;
}) {
  if (items.length === 0) {
    return <EmptyMessage>No contradictions found.</EmptyMessage>;
  }

  return (
    <ul className="space-y-2">
      {items.map((hit) => {
        const key = contradictionKey(hit);
        const resolved = isResolved(hit);
        return (
          <li
            key={`${hit.a_id}:${hit.b_id}:${hit.kind}`}
            className="rounded-md border border-slate-800 bg-slate-900 p-3"
          >
            <div className="flex items-start justify-between gap-3">
              <div className="min-w-0">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-amber-300">
                    {hit.kind}
                  </span>
                  <span
                    className={`rounded-full px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider ${
                      resolved ? 'bg-slate-800 text-slate-400' : 'bg-red-500/10 text-red-300'
                    }`}
                  >
                    {hit.status}
                  </span>
                </div>
                <p className="mt-2 text-sm leading-5 text-slate-100">{hit.explanation}</p>
                <div className="mt-2 grid gap-1 font-mono text-[11px] text-slate-400">
                  <span className="truncate">a: {hit.a_id}</span>
                  <span className="truncate">b: {hit.b_id}</span>
                  <time>{formatTimestamp(hit.detected_at_ms)}</time>
                </div>
              </div>
              <button
                type="button"
                onClick={() => onResolve(hit)}
                disabled={resolved || resolvingKey === key}
                className="shrink-0 rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
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

function Metric({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-md border border-slate-800 bg-slate-900 px-3 py-2">
      <div className="text-[10px] uppercase tracking-wider text-slate-400">{label}</div>
      <div className="text-base font-semibold text-slate-100">{value}</div>
    </div>
  );
}

function KindPill() {
  return (
    <span className="rounded-full bg-sky-500/10 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-sky-300">
      episode
    </span>
  );
}

function SourcePill({ source }: { source: string }) {
  return (
    <span className="rounded-full bg-indigo-500/10 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-indigo-300">
      {formatSourceType(source)}
    </span>
  );
}

function ReviewPill({ state }: { state: MemoryInboxItem['review_state'] }) {
  if (state === 'approved') {
    return (
      <span className="rounded-full bg-emerald-500/10 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-emerald-300">
        approved
      </span>
    );
  }
  if (state === 'dismissed') {
    return (
      <span className="rounded-full bg-slate-800 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-slate-400">
        dismissed
      </span>
    );
  }
  return (
    <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[10px] font-medium uppercase tracking-wider text-amber-300">
      needs review
    </span>
  );
}

function ReviewButtons({
  episode,
  busy,
  onReview,
}: {
  episode: MemoryInboxItem;
  busy: boolean;
  onReview: (episode: MemoryInboxItem, state: MemoryReviewRequestState) => void;
}) {
  const reviewed = Boolean(episode.review_state);
  return (
    <div className="col-span-2 mt-1 grid grid-cols-3 gap-1 border-t border-slate-800 pt-1 sm:flex sm:flex-col">
      <button
        type="button"
        onClick={() => onReview(episode, 'approved')}
        disabled={busy || episode.review_state === 'approved'}
        className="shrink-0 rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:border-emerald-600 hover:text-emerald-300 disabled:cursor-not-allowed disabled:opacity-50"
        aria-label={`Approve ${episode.label}`}
      >
        {busy ? 'Saving...' : 'Approve'}
      </button>
      <button
        type="button"
        onClick={() => onReview(episode, 'dismissed')}
        disabled={busy || episode.review_state === 'dismissed'}
        className="shrink-0 rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-300 hover:border-amber-600 hover:text-amber-300 disabled:cursor-not-allowed disabled:opacity-50"
        aria-label={`Dismiss ${episode.label}`}
      >
        {busy ? 'Saving...' : 'Dismiss'}
      </button>
      {reviewed && (
        <button
          type="button"
          onClick={() => onReview(episode, 'needs_review')}
          disabled={busy}
          className="shrink-0 rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-400 hover:border-slate-500 hover:text-slate-200 disabled:cursor-not-allowed disabled:opacity-50"
          aria-label={`Reset review for ${episode.label}`}
        >
          Reset
        </button>
      )}
    </div>
  );
}

function ErrorMessage({ message }: { message: string }) {
  return (
    <p
      role="alert"
      className="rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-xs text-red-300"
    >
      {message}
    </p>
  );
}

function EmptyMessage({ children }: { children: string }) {
  return (
    <p className="rounded-md border border-slate-800 bg-slate-900 px-3 py-6 text-center text-xs text-slate-400">
      {children}
    </p>
  );
}

function compareContradictions(a: ContradictionHit, b: ContradictionHit): number {
  const status = Number(isResolved(a)) - Number(isResolved(b));
  if (status !== 0) return status;
  return (b.detected_at_ms ?? 0) - (a.detected_at_ms ?? 0);
}

function contradictionKey(hit: Pick<ContradictionHit, 'a_id' | 'b_id' | 'kind'>): string {
  return `${hit.a_id}:${hit.b_id}:${hit.kind}`;
}

function isResolved(hit: ContradictionHit): boolean {
  return hit.status.toLowerCase() === 'resolved';
}

function episodeGraphId(memoryId: string): string {
  return memoryId.startsWith('ep:') ? memoryId : `ep:${memoryId}`;
}

function reviewNoteFor(state: MemoryReviewRequestState): string | undefined {
  if (state === 'approved') return 'Approved from Solo Memory inbox';
  if (state === 'dismissed') return 'Dismissed from Solo Memory inbox';
  return undefined;
}

function bulkReviewNoteFor(state: MemoryReviewRequestState): string | undefined {
  if (state === 'approved') return 'Bulk approved from Solo Memory inbox';
  if (state === 'dismissed') return 'Bulk dismissed from Solo Memory inbox';
  return undefined;
}

function reviewStateOf(episode: MemoryInboxItem): Exclude<ReviewFilter, 'all'> {
  return episode.review_state ?? 'needs_review';
}

function countReviewStates(
  episodes: MemoryInboxItem[],
): Record<Exclude<ReviewFilter, 'all'>, number> {
  return episodes.reduce(
    (counts, episode) => {
      counts[reviewStateOf(episode)] += 1;
      return counts;
    },
    { needs_review: 0, approved: 0, dismissed: 0 },
  );
}

function uniqueSources(episodes: MemoryInboxItem[]): string[] {
  return Array.from(new Set(episodes.map((episode) => episode.source_type).filter(Boolean))).sort(
    (a, b) => formatSourceType(a).localeCompare(formatSourceType(b)),
  );
}

function bulkReviewTargets(
  episodes: MemoryInboxItem[],
  state: MemoryReviewRequestState,
): MemoryInboxItem[] {
  const targetState = state === 'approved' || state === 'dismissed' ? state : 'needs_review';
  return episodes.filter((episode) => reviewStateOf(episode) !== targetState);
}

function buildInboxSummary({
  loadedCount,
  visibleCount,
  reviewFilter,
  sourceFilter,
  sourceOptions,
  reviewCounts,
  contradictionCount,
}: {
  loadedCount: number;
  visibleCount: number;
  reviewFilter: ReviewFilter;
  sourceFilter: string;
  sourceOptions: string[];
  reviewCounts: Record<Exclude<ReviewFilter, 'all'>, number>;
  contradictionCount: number;
}): string {
  return [
    'Solo Memory Inbox Summary',
    `Memory library: ${COMMUNITY_LIBRARY_NAME}`,
    `Loaded episodes: ${loadedCount}`,
    `Visible episodes: ${visibleCount}`,
    `Review filter: ${formatReviewFilter(reviewFilter)}`,
    `Source filter: ${sourceFilter === 'all' ? 'All sources' : formatSourceType(sourceFilter)}`,
    `Needs review: ${reviewCounts.needs_review}`,
    `Approved: ${reviewCounts.approved}`,
    `Dismissed: ${reviewCounts.dismissed}`,
    `Contradictions: ${contradictionCount}`,
    `Sources: ${sourceOptions.length > 0 ? sourceOptions.map(formatSourceType).join(', ') : 'none'}`,
  ].join('\n');
}

function formatReviewFilter(filter: ReviewFilter): string {
  return REVIEW_FILTERS.find((item) => item.value === filter)?.label ?? filter;
}

function formatSourceType(source: string): string {
  const normalized = source.trim().replace(/[_-]+/g, ' ');
  return normalized.length > 0 ? normalized : 'Unknown source';
}

function formatTimestamp(ts?: number): string {
  if (typeof ts !== 'number') return 'No timestamp';
  return new Date(ts).toISOString();
}
