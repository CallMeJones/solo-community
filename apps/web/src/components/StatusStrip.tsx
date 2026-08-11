import { useQuery } from '@tanstack/react-query';
import { fetchSoloStatus, type SoloStatus } from '../api/health';
import type { NodeKind } from '../api/types';
import { DEFAULT_SOLO_API_URL, MCP_BRIDGE_URL } from '../config/defaults';
import { useGraphData } from '../hooks/useGraphData';
import { buildGraphPresentation } from '../lib/graphPresentation';
import { COMMUNITY_LIBRARY_NAME, useGraphStore } from '../store/graphStore';
import { useSettingsStore } from '../store/settingsStore';

const HEALTH_REFETCH_MS = 15_000;

export function StatusStrip() {
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const visibleKinds = useGraphStore((s) => s.visibleKinds);
  const lastGraphInvalidateAtMs = useGraphStore((s) => s.lastGraphInvalidateAtMs);
  const graph = useGraphData();

  const solo = useQuery({
    queryKey: ['status', 'solo', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    refetchInterval: HEALTH_REFETCH_MS,
    retry: false,
  });

  const soloEndpoint = describeSoloEndpoint(apiUrl);
  const graphStatus = graphStatusText(graph, visibleKinds);

  return (
    <div className="flex min-h-9 flex-wrap items-center gap-x-3 gap-y-1 overflow-x-hidden border-b border-slate-800 bg-slate-950/95 px-4 py-2 text-xs text-slate-300 md:flex-nowrap md:py-0">
      <HealthPill
        label={soloEndpoint.label}
        status={statusOf(solo.status, solo.fetchStatus)}
        error={solo.error}
        detail={solo.data ? soloStatusDetail(solo.data) : undefined}
        endpoint={soloEndpoint.endpoint}
        endpointLabel={soloEndpoint.host}
        hint={soloEndpoint.hint}
        onRetry={() => void solo.refetch()}
      />
      <StatusDivider />
      <span className="whitespace-nowrap">
        <span className="text-slate-400">Memory library</span>{' '}
        <span className="font-medium text-slate-100">
          {solo.data?.library.name ?? COMMUNITY_LIBRARY_NAME}
        </span>
      </span>
      {solo.data && (
        <>
          <StatusDivider />
          <span className="whitespace-nowrap text-slate-400">
            <span className="text-slate-400">Embedder</span>{' '}
            <span className="font-medium text-slate-100">
              {solo.data.embedder.name}@{solo.data.embedder.version}
            </span>{' '}
            {solo.data.embedder.dim}d {solo.data.embedder.dtype}
          </span>
          <StatusDivider />
          <span className="whitespace-nowrap text-slate-400">
            <span className="text-slate-400">MCP</span>{' '}
            <span className="font-medium text-slate-100">{solo.data.mcp.sessions}</span> sessions
          </span>
        </>
      )}
      <StatusDivider />
      <span
        className="min-w-0 basis-full truncate text-slate-400 md:basis-auto"
        title={graphStatus}
      >
        {graphStatus}
      </span>
      {lastGraphInvalidateAtMs !== null && (
        <>
          <StatusDivider />
          <span className="whitespace-nowrap text-slate-400">
            <span className="text-slate-400">Stream</span> update{' '}
            <span className="font-medium text-slate-100">
              {formatTime(lastGraphInvalidateAtMs)}
            </span>
          </span>
        </>
      )}
    </div>
  );
}

function StatusDivider() {
  return <span className="hidden h-4 w-px bg-slate-800 md:inline-block" aria-hidden="true" />;
}

type HealthStatus = 'checking' | 'online' | 'offline';
type GraphStatus = ReturnType<typeof useGraphData>;
type LoadedGraph = NonNullable<GraphStatus['data']>;

function HealthPill({
  label,
  status,
  error,
  detail,
  endpoint,
  endpointLabel,
  hint,
  onRetry,
}: {
  label: string;
  status: HealthStatus;
  error: unknown;
  detail?: string;
  endpoint: string;
  endpointLabel: string;
  hint: string;
  onRetry: () => void;
}) {
  const color =
    status === 'online' ? 'bg-emerald-400' : status === 'offline' ? 'bg-rose-400' : 'bg-amber-300';
  const text = status === 'online' ? 'online' : status === 'offline' ? 'offline' : 'checking';
  const titleBase =
    error instanceof Error ? error.message : error ? String(error) : (detail ?? `${label} ${text}`);
  const title = `${titleBase} Endpoint: ${endpoint}. ${hint}`;

  return (
    <button
      type="button"
      onClick={onRetry}
      title={title}
      aria-label={`${label} ${text} at ${endpoint}; retry health check`}
      className="inline-flex items-center gap-1.5 whitespace-nowrap rounded-sm outline-none transition hover:text-slate-100 focus-visible:ring-2 focus-visible:ring-sky-500"
    >
      <span className={`h-2 w-2 rounded-full ${color}`} aria-hidden="true" />
      <span className="text-slate-400">{label}</span>
      <span className="font-medium text-slate-100">{text}</span>
      <span className="hidden font-mono text-[11px] text-slate-400 lg:inline">{endpointLabel}</span>
    </button>
  );
}

function statusOf(
  status: 'pending' | 'error' | 'success',
  fetchStatus: 'fetching' | 'paused' | 'idle',
): HealthStatus {
  if (status === 'success') return 'online';
  if (status === 'error') return 'offline';
  return fetchStatus === 'fetching' ? 'checking' : 'offline';
}

function graphStatusText(graph: GraphStatus, visibleKinds: ReadonlySet<NodeKind>): string {
  if (graph.isError) {
    return `Graph error: ${graph.error instanceof Error ? graph.error.message : String(graph.error)}`;
  }
  if (graph.isFetching) {
    return 'Graph refreshing';
  }
  if (graph.dataUpdatedAt > 0) {
    const data = graph.data;
    const loadedNodes = data?.nodes.length ?? 0;
    const loadedLinks = data?.edges.length ?? 0;
    const visible = data
      ? countVisibleGraphItems(data, visibleKinds)
      : { nodes: loadedNodes, links: loadedLinks };
    const counts =
      visible.nodes === loadedNodes && visible.links === loadedLinks
        ? `${loadedNodes} nodes, ${loadedLinks} links`
        : `${visible.nodes}/${loadedNodes} nodes, ${visible.links}/${loadedLinks} links`;
    return `Graph ${counts} - updated ${formatTime(graph.dataUpdatedAt)}`;
  }
  return 'Graph not loaded';
}

function countVisibleGraphItems(
  graph: LoadedGraph,
  visibleKinds: ReadonlySet<NodeKind>,
): { nodes: number; links: number } {
  const presented = buildGraphPresentation(graph, visibleKinds, new Set(), '');
  return { nodes: presented.nodes.length, links: presented.links.length };
}

function soloStatusDetail(status: SoloStatus): string {
  const library = status.library.ready ? status.library.name : `${status.library.name} not ready`;
  return `Solo ${status.version} - ${library} - ${status.embedder.name}@${status.embedder.version} ${status.embedder.dim}d ${status.embedder.dtype} - MCP ${status.mcp.sessions}`;
}

interface EndpointDescriptor {
  label: string;
  endpoint: string;
  host: string;
  hint: string;
}

function describeSoloEndpoint(apiUrl: string): EndpointDescriptor {
  const endpoint = normalizeEndpoint(apiUrl);
  if (endpoint === DEFAULT_SOLO_API_URL) {
    return {
      label: 'Solo HTTP',
      endpoint,
      host: endpointHost(endpoint),
      hint: 'Start or unlock Solo from the tray if this stays offline.',
    };
  }
  if (endpoint === MCP_BRIDGE_URL) {
    return {
      label: 'Development bridge',
      endpoint,
      host: endpointHost(endpoint),
      hint: 'Local development fallback. Use Solo HTTP for the installed desktop app.',
    };
  }
  return {
    label: 'Solo custom',
    endpoint,
    host: endpointHost(endpoint),
    hint: 'Check Settings > Solo API URL if this stays offline.',
  };
}

function normalizeEndpoint(url: string): string {
  return url.trim().replace(/\/$/, '');
}

function endpointHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
  });
}
