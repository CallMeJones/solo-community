import { useMutation, useQuery, useQueryClient, type UseQueryResult } from '@tanstack/react-query';
import { lazy, Suspense, useEffect, useState, type ReactNode } from 'react';
import {
  consolidateMemory,
  errorMessage,
  extractTriplesNow,
  fetchFactsAbout,
  fetchInbox,
  fetchMemoryQualityAudit,
  fetchMemoryQualityReviews,
  probeMcpTools,
  repairDerivedMemory,
  restartSoloRuntime,
  switchOllamaEmbedder,
  switchStewardCadence,
  switchStewardLlm,
  startStewardBackfill,
  updateMemoryQualityReview,
  type LlmSettingsSummary,
  type OllamaEndpoint,
  type StewardLlmMode,
} from './api/client';
import type {
  ConsolidationReport,
  DerivedRepairReport,
  FactHit,
  GraphResponse,
  MemoryQualityAuditReport,
  MemoryQualityIssue,
  MemoryQualityReviewsResponse,
  TriplesExtractReport,
} from './api/types';
import { fetchSoloStatus, type SoloStatus } from './api/health';
import { SettingsDialog } from './components/SettingsDialog';
import { BackupView } from './components/BackupView';
import { MemoryPolicyPanel } from './components/MemoryPolicyPanel';
import { SetupGuideView } from './components/SetupGuideView';
import { StatusStrip } from './components/StatusStrip';
import { Toolbar } from './components/Toolbar';
import { CopyButton } from './components/ui/CopyButton';
import { DEFAULT_SOLO_API_URL, MCP_BRIDGE_URL } from './config/defaults';
import { useGraphData } from './hooks/useGraphData';
import { useGraphStream } from './hooks/useGraphStream';
import {
  claudeCodeHttpAddCommand,
  mcpEndpoint,
  setupClientDoctorCommand,
  setupClientHttpDryRunCommand,
  type SetupClientTarget,
} from './lib/soloRoutes';
import { COMMUNITY_LIBRARY_NAME, useGraphStore } from './store/graphStore';
import { useSettingsStore } from './store/settingsStore';
import {
  CORE_ROUTE_IDS,
  communityWebHost,
  type AppRouteId,
  type CoreRouteId,
  type SoloWebHost,
  type SoloWebModuleContext,
  type SoloWebSlotModule,
} from './host';

type AppMode = AppRouteId;

const GraphView = lazy(() =>
  import('./components/GraphView').then((m) => ({ default: m.GraphView })),
);
const ImportView = lazy(() =>
  import('./components/ImportView').then((m) => ({ default: m.ImportView })),
);
const InboxView = lazy(() =>
  import('./components/InboxView').then((m) => ({ default: m.InboxView })),
);
const LogsView = lazy(() => import('./components/LogsView').then((m) => ({ default: m.LogsView })));
const ProjectsView = lazy(() =>
  import('./components/ProjectsView').then((m) => ({ default: m.ProjectsView })),
);
const InspectorPanel = lazy(() =>
  import('./components/InspectorPanel').then((m) => ({ default: m.InspectorPanel })),
);

const APP_MODES: readonly CoreRouteId[] = CORE_ROUTE_IDS;

const NAV_ITEMS: Array<{ mode: CoreRouteId; label: string }> = [
  { mode: 'home', label: 'Home' },
  { mode: 'memories', label: 'Memories' },
  { mode: 'inbox', label: 'Inbox' },
  { mode: 'import', label: 'Import' },
  { mode: 'projects', label: 'Projects' },
  { mode: 'settings', label: 'Settings' },
];

const USE_MOCKS = import.meta.env.VITE_SOLO_USE_MOCKS === '1';

export default function App({ host = communityWebHost }: { host?: SoloWebHost }) {
  const [mode, setModeState] = useState<AppMode>(() => modeFromHash(window.location.hash, host));
  const setSelectedNodeId = useGraphStore((s) => s.setSelectedNodeId);
  const apiUrl = useSettingsStore((s) => s.apiUrl);

  const setMode = (next: AppMode) => {
    setModeState(next);
    if (window.location.hash !== `#${next}`) {
      window.history.replaceState(null, '', `#${next}`);
    }
  };

  useEffect(() => {
    const onHashChange = () => setModeState(modeFromHash(window.location.hash, host));
    window.addEventListener('hashchange', onHashChange);
    return () => window.removeEventListener('hashchange', onHashChange);
  }, [host]);

  // Subscribe to /v1/graph/stream so Solo writes refresh the visible graph.
  useGraphStream();

  const navItems = [
    ...NAV_ITEMS,
    ...host.routes
      .filter((module) => module.nav !== false)
      .map((module) => ({ mode: module.id, label: module.label })),
  ];
  const moduleContext: SoloWebModuleContext = { apiUrl, navigate: setMode };

  return (
    <div className="dune-theme flex h-screen w-screen flex-col bg-slate-950 text-slate-100 md:flex-row">
      <aside className="flex shrink-0 flex-col border-b border-slate-800 bg-slate-950 md:w-56 md:border-b-0 md:border-r">
        <div className="border-b border-slate-800 px-4 py-3 md:py-4">
          <div className="text-base font-semibold text-slate-100">{host.productName}</div>
          <div className="mt-1 text-xs text-slate-400">{host.tagline}</div>
        </div>

        <nav
          aria-label="Solo"
          className="flex gap-1 overflow-x-auto px-3 py-3 md:flex-1 md:flex-col md:overflow-x-visible"
        >
          {navItems.map((item) => (
            <button
              key={item.mode}
              type="button"
              onClick={() => setMode(item.mode)}
              aria-current={mode === item.mode ? 'page' : undefined}
              className={[
                'h-9 shrink-0 rounded-md px-3 text-left text-sm transition-colors md:w-full',
                mode === item.mode
                  ? 'bg-slate-800 text-white'
                  : 'text-slate-400 hover:bg-slate-900 hover:text-slate-100',
              ].join(' ')}
            >
              {item.label}
            </button>
          ))}
        </nav>

        <div className="hidden border-t border-slate-800 px-4 py-3 text-xs text-slate-400 md:block">
          Solo API <span className="font-mono text-slate-300">{compactHost(apiUrl)}</span>
        </div>
      </aside>

      <div className="flex min-h-0 min-w-0 flex-1 flex-col">
        <StatusStrip />
        <HostSlotModules modules={host.statusModules} context={moduleContext} />
        <div className="flex flex-1 overflow-hidden">
          <main className="relative min-w-0 flex-1">
            <Suspense fallback={<PanelLoading label="Loading" />}>
              <ModeView
                mode={mode}
                host={host}
                moduleContext={moduleContext}
                onModeChange={setMode}
                onSelectEpisode={(id) => {
                  setSelectedNodeId(id);
                  setMode('memories');
                }}
              />
            </Suspense>
          </main>
          {mode === 'memories' && (
            <aside
              className="w-96 shrink-0 overflow-y-auto border-l border-slate-800 bg-slate-900/60 p-4"
              tabIndex={0}
              aria-label="Inspector panel"
            >
              <Suspense fallback={<PanelLoading label="Inspector" compact />}>
                <InspectorPanel />
              </Suspense>
            </aside>
          )}
        </div>
      </div>
    </div>
  );
}

function ModeView({
  mode,
  host,
  moduleContext,
  onModeChange,
  onSelectEpisode,
}: {
  mode: AppMode;
  host: SoloWebHost;
  moduleContext: SoloWebModuleContext;
  onModeChange: (mode: AppMode) => void;
  onSelectEpisode: (id: string) => void;
}) {
  switch (mode) {
    case 'home':
      return <HomeView onModeChange={onModeChange} />;
    case 'setup':
      return (
        <PageChrome title="Setup" eyebrow="First Run">
          <SetupGuideView onModeChange={onModeChange} />
        </PageChrome>
      );
    case 'health':
      return <HealthView onModeChange={onModeChange} />;
    case 'connections':
      return <ConnectionsView />;
    case 'backups':
      return (
        <PageChrome title="Backups" eyebrow="Recovery">
          <BackupView />
        </PageChrome>
      );
    case 'projects':
      return (
        <PageChrome title="Projects" eyebrow="Project Memory">
          <ProjectsView />
        </PageChrome>
      );
    case 'logs':
      return <LogsView />;
    case 'memories':
      return (
        <>
          <Toolbar />
          <GraphView />
        </>
      );
    case 'inbox':
      return <InboxView onSelectEpisode={onSelectEpisode} />;
    case 'import':
      return (
        <PageChrome title="Import" eyebrow="Memory Intake">
          <ImportView />
        </PageChrome>
      );
    case 'settings':
      return (
        <SettingsView
          onModeChange={onModeChange}
          hostModules={host.settingsModules}
          moduleContext={moduleContext}
        />
      );
    default: {
      const module = host.routes.find((candidate) => candidate.id === mode);
      return module ? module.render(moduleContext) : <HomeView onModeChange={onModeChange} />;
    }
  }
}

function HostSlotModules({
  modules,
  context,
}: {
  modules: readonly SoloWebSlotModule[];
  context: SoloWebModuleContext;
}) {
  if (modules.length === 0) return null;
  return <>{modules.map((module) => <div key={module.id}>{module.render(context)}</div>)}</>;
}

function HomeView({ onModeChange }: { onModeChange: (mode: AppMode) => void }) {
  const graph = useGraphData();
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const solo = useQuery({
    queryKey: ['desktop-home', 'solo-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
    refetchInterval: (query) =>
      query.state.data?.steward?.backfill?.status === 'running' ? 1_000 : false,
  });
  const inbox = useQuery({
    queryKey: ['desktop-home', 'inbox', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchInbox(50, { signal }),
    retry: false,
  });
  const stats = summarizeGraph(graph.data);
  const topEntity = selectTopEntity(graph.data);
  const facts = useQuery({
    queryKey: [
      'desktop-home',
      'facts-about',
      apiUrl,
      connectionRevision,
      topEntity?.subject ?? null,
    ],
    queryFn: ({ signal }) =>
      fetchFactsAbout({ subject: topEntity?.subject ?? '', limit: 3 }, { signal }),
    retry: false,
    enabled: Boolean(topEntity) && !USE_MOCKS,
  });
  const inboxCount = inbox.data?.length;

  return (
    <PageChrome title="Home" eyebrow="Solo">
      <div className="grid gap-3 lg:grid-cols-4">
        <MetricTile label="Solo" value={solo.data?.ok ? 'online' : statusLabel(solo.status)} />
        <MetricTile label="Inbox" value={homeInboxCount(inbox.status, inboxCount)} />
        <MetricTile label="Memories" value={String(stats.episodes)} />
        <MetricTile label="Steward" value={stewardPendingClusters(solo.data)} />
      </div>

      <div className="mt-5 grid gap-4 xl:grid-cols-[1.3fr_1fr]">
        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Next actions</h2>
          <div className="mt-4 grid gap-2 sm:grid-cols-2">
            <ActionButton
              label="View memories"
              detail={`${stats.episodes} memories, ${stats.triples} triples`}
              onClick={() => onModeChange('memories')}
            />
            <ActionButton
              label="Review inbox"
              detail={homeInboxDetail(inbox.status, inboxCount)}
              onClick={() => onModeChange('inbox')}
            />
            <ActionButton
              label="Import data"
              detail="ChatGPT, Claude, bookmarks"
              onClick={() => onModeChange('import')}
            />
            <ActionButton
              label="Project memory"
              detail={`${stats.entities} entities, ${stats.clusters} clusters`}
              onClick={() => onModeChange('projects')}
            />
            <ActionButton
              label="Setup Solo"
              detail={solo.data?.ok ? 'review setup checklist' : 'start or unlock'}
              onClick={() => onModeChange('setup')}
            />
          </div>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Solo status</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow label="Unlocked" value={daemonStateValue(solo.status, solo.data)} />
            <StatusRow
              label="Memory library"
              value={solo.data?.library.name ?? COMMUNITY_LIBRARY_NAME}
            />
            <StatusRow label="Inbox items" value={homeInboxDetail(inbox.status, inboxCount)} />
            <StatusRow label="Memories" value={`${stats.episodes} episodes`} />
            <StatusRow label="Pending Steward" value={stewardPendingClusters(solo.data)} />
            <StatusRow
              label="Facts / triples"
              value={homeFactsTriplesStatus(
                topEntity ? facts.status : 'success',
                facts.data,
                stats.triples,
              )}
            />
            <StatusRow
              label="Tool clients"
              value={
                solo.data?.mcp.sessions
                  ? `${solo.data.mcp.sessions} connected`
                  : solo.data?.ok
                    ? 'none connected'
                    : statusLabel(solo.status)
              }
            />
            <StatusRow label="Solo API" value={apiUrl || DEFAULT_SOLO_API_URL} />
            <StatusRow
              label="Embedder"
              value={
                solo.data ? `${solo.data.embedder.name}@${solo.data.embedder.version}` : 'pending'
              }
            />
          </dl>
          <div className="mt-5 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => onModeChange('settings')}
              className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
            >
              Open diagnostics
            </button>
          </div>
        </section>
      </div>
    </PageChrome>
  );
}

function McpToolList({ toolNames }: { toolNames: string[] }) {
  if (toolNames.length === 0) return null;
  const visible = toolNames.slice(0, 10);
  const hidden = toolNames.length - visible.length;
  return (
    <div className="mt-4 flex flex-wrap gap-2">
      {visible.map((tool) => (
        <span
          key={tool}
          className="rounded border border-slate-800 bg-slate-950 px-2 py-1 font-mono text-xs text-slate-300"
        >
          {tool}
        </span>
      ))}
      {hidden > 0 && (
        <span className="rounded border border-slate-800 bg-slate-950 px-2 py-1 text-xs text-slate-400">
          +{hidden}
        </span>
      )}
    </div>
  );
}

function HealthView({ onModeChange }: { onModeChange: (mode: AppMode) => void }) {
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const solo = useQuery({
    queryKey: ['desktop-health', 'solo-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
  });
  const defaultSolo = useQuery({
    queryKey: ['desktop-health', 'solo-default-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
  });
  const mcpProbe = useMutation({
    mutationFn: () => probeMcpTools(),
  });
  const runtime = solo.data?.runtime;

  return (
    <PageChrome title="Health" eyebrow="Runtime">
      <div className="grid gap-3 lg:grid-cols-3">
        <MetricTile label="Daemon" value={solo.data?.ok ? 'running' : statusLabel(solo.status)} />
        <MetricTile
          label="Memory library"
          value={solo.data?.library.name ?? COMMUNITY_LIBRARY_NAME}
        />
        <MetricTile label="MCP ready" value={mcpProbeStatus(mcpProbe)} />
      </div>

      <div className="mt-5 grid gap-4 xl:grid-cols-2">
        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Daemon State</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow label="Status" value={daemonStateValue(solo.status, solo.data)} />
            <StatusRow label="Next action" value={daemonNextAction(solo.status, solo.data)} />
            <StatusRow label="Solo API" value={apiUrl || DEFAULT_SOLO_API_URL} />
            <StatusRow label="Version" value={solo.data?.version ?? 'not available'} />
            <StatusRow
              label="Process"
              value={runtime?.pid ? `pid ${runtime.pid}` : 'not reported'}
            />
            <StatusRow label="Platform" value={runtime?.platform ?? 'not reported'} />
            <StatusRow label="Data dir" value={runtime?.data_dir ?? 'not reported'} />
            <StatusRow
              label="Memory library"
              value={defaultSolo.data?.library.name ?? statusLabel(defaultSolo.status)}
            />
            <StatusRow label="Embedder" value={embedderSummary(solo.data)} />
            <StatusRow label="Embedder runtime" value={embedderRuntimeStatus(solo.data)} />
            <StatusRow label="Steward runtime" value={stewardRuntimeStatus(solo.data)} />
          </dl>
          {solo.isError && (
            <p className="mt-4 rounded-md border border-amber-800/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-100">
              {errorMessage(solo.error)}
            </p>
          )}
          <div className="mt-5 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => void solo.refetch()}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600"
            >
              Check daemon
            </button>
            <button
              type="button"
              onClick={() => onModeChange('logs')}
              className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
            >
              Open logs
            </button>
          </div>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Memory Library</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow label="Library" value={solo.data?.library.name ?? COMMUNITY_LIBRARY_NAME} />
            <StatusRow
              label="Database"
              value={runtime?.data_dir ? `${runtime.data_dir}/solo.db` : 'solo.db'}
            />
            <StatusRow label="Ready" value={libraryReadiness(solo.data)} />
            <StatusRow
              label="Physical databases"
              value={solo.data ? '1' : statusLabel(solo.status)}
            />
          </dl>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">MCP Status</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow label="Endpoint" value={mcpEndpoint(apiUrl)} />
            <StatusRow label="Sessions" value={String(solo.data?.mcp.sessions ?? 0)} />
            <StatusRow label="Tools" value={mcpProbeStatus(mcpProbe)} />
            <StatusRow label="Read-only call" value={mcpReadOnlyCallStatus(mcpProbe.data)} />
            {mcpProbe.data && (
              <>
                <StatusRow
                  label="Server"
                  value={`${mcpProbe.data.serverName} ${mcpProbe.data.serverVersion}`}
                />
                <StatusRow label="Protocol" value={mcpProbe.data.protocolVersion} />
              </>
            )}
            {mcpProbe.data && (
              <StatusRow
                label="Required"
                value={
                  mcpProbe.data.missingRequiredTools.length === 0
                    ? 'present'
                    : `missing ${mcpProbe.data.missingRequiredTools.join(', ')}`
                }
              />
            )}
          </dl>
          {mcpProbe.isError && (
            <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
              {errorMessage(mcpProbe.error)}
            </p>
          )}
          {mcpProbe.data && <McpToolList toolNames={mcpProbe.data.toolNames} />}
          <div className="mt-5 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => mcpProbe.mutate()}
              disabled={mcpProbe.isPending}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {mcpProbe.isPending ? 'Checking MCP' : 'Probe MCP'}
            </button>
            <button
              type="button"
              onClick={() => onModeChange('connections')}
              className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
            >
              Open connections
            </button>
          </div>
        </section>
      </div>
    </PageChrome>
  );
}

function ConnectionsView() {
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const bearerToken = useSettingsStore((s) => s.bearerToken);
  const solo = useQuery({
    queryKey: ['desktop-connections', 'solo-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
  });
  const libraryName = solo.data?.library.name ?? COMMUNITY_LIBRARY_NAME;
  const mcpUrl = mcpEndpoint(apiUrl);
  const doctorCommand = setupClientDoctorCommand(apiUrl);
  const claudeCodeCommand = claudeCodeHttpAddCommand(apiUrl);
  const mcpProbe = useMutation({
    mutationFn: () => probeMcpTools(),
  });
  const setupTargets: Array<{ target: SetupClientTarget; label: string }> = [
    { target: 'codex', label: 'Codex' },
    { target: 'claude-desktop', label: 'Claude Desktop' },
    { target: 'cursor', label: 'Cursor' },
  ];

  return (
    <PageChrome title="Connections" eyebrow="Tools">
      <div className="grid gap-3 lg:grid-cols-4">
        <MetricTile label="Solo MCP" value={solo.data?.ok ? 'ready' : statusLabel(solo.status)} />
        <MetricTile label="MCP sessions" value={String(solo.data?.mcp.sessions ?? 0)} />
        <MetricTile label="Memory library" value={libraryName} />
        <MetricTile
          label="MCP tools"
          value={
            mcpProbe.data
              ? String(mcpProbe.data.toolCount)
              : mcpProbe.isError
                ? 'failed'
                : 'not checked'
          }
        />
      </div>

      <div className="mt-5 grid gap-4 xl:grid-cols-2">
        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Solo MCP</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow label="Endpoint" value={mcpUrl} />
            <StatusRow label="Bearer auth" value={bearerToken ? 'stored' : 'empty'} />
            <StatusRow label="Daemon" value={solo.data?.version ?? statusLabel(solo.status)} />
            <StatusRow label="Doctor command" value={doctorCommand} />
            <StatusRow label="Claude Code add" value={claudeCodeCommand} />
            <StatusRow label="Daemon MCP" value={mcpProbeStatus(mcpProbe)} />
            <StatusRow label="Read-only call" value={mcpReadOnlyCallStatus(mcpProbe.data)} />
            {mcpProbe.data && (
              <StatusRow
                label="Required tools"
                value={
                  mcpProbe.data.missingRequiredTools.length === 0
                    ? 'present'
                    : `missing ${mcpProbe.data.missingRequiredTools.join(', ')}`
                }
              />
            )}
          </dl>
          {mcpProbe.isError && (
            <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
              {errorMessage(mcpProbe.error)}
            </p>
          )}
          {mcpProbe.data && <McpToolList toolNames={mcpProbe.data.toolNames} />}
          <div className="mt-5 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => mcpProbe.mutate()}
              disabled={mcpProbe.isPending}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {mcpProbe.isPending ? 'Checking MCP' : 'Probe MCP'}
            </button>
            <CopyButton label="Copy MCP URL" value={mcpUrl} />
            <CopyButton label="Copy Doctor" value={doctorCommand} />
            <CopyButton label="Copy Claude Code" value={claudeCodeCommand} />
          </div>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Connected Clients</h2>
          <div className="mt-4 space-y-3">
            {setupTargets.map(({ target, label }) => {
              const command = setupClientHttpDryRunCommand(target, apiUrl);
              const targetDoctorCommand = setupClientDoctorCommand(apiUrl, target);
              return (
                <div
                  key={target}
                  className="flex flex-col gap-3 rounded-md border border-slate-800 bg-slate-950/60 px-3 py-2 sm:flex-row sm:items-center sm:justify-between"
                >
                  <div className="min-w-0">
                    <div className="text-sm font-medium text-slate-100">{label}</div>
                    <div className="truncate font-mono text-xs text-slate-400">
                      {target === 'codex' ? 'native HTTP MCP' : 'HTTP MCP config'}
                    </div>
                  </div>
                  <div className="flex shrink-0 flex-wrap justify-end gap-2">
                    <CopyButton label="Copy dry-run" value={command} />
                    <CopyButton label="Copy Doctor" value={targetDoctorCommand} />
                  </div>
                </div>
              );
            })}
          </div>
        </section>
      </div>

      <MemoryPolicyPanel libraryName={libraryName} mcpUrl={mcpUrl} />
    </PageChrome>
  );
}

function SettingsView({
  onModeChange,
  hostModules,
  moduleContext,
}: {
  onModeChange: (mode: AppMode) => void;
  hostModules: readonly SoloWebSlotModule[];
  moduleContext: SoloWebModuleContext;
}) {
  const queryClient = useQueryClient();
  const [open, setOpen] = useState(false);
  const [ollamaModel, setOllamaModel] = useState('nomic-embed-text');
  const [ollamaDim, setOllamaDim] = useState('768');
  const [ollamaBaseUrl, setOllamaBaseUrl] = useState('http://localhost:11434');
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const bearerToken = useSettingsStore((s) => s.bearerToken);
  const transport = settingsTransport(apiUrl);
  const solo = useQuery({
    queryKey: ['desktop-settings', 'solo-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
    refetchInterval: (query) =>
      query.state.data?.steward?.backfill?.status === 'running' ? 1_000 : false,
  });
  const libraryName = solo.data?.library.name ?? COMMUNITY_LIBRARY_NAME;
  const mcpUrl = mcpEndpoint(apiUrl);
  const doctorCommand = setupClientDoctorCommand(apiUrl);
  const claudeDesktopCommand = setupClientHttpDryRunCommand('claude-desktop', apiUrl);
  const mcpProbe = useMutation({
    mutationFn: () => probeMcpTools(),
  });
  const ollamaSwitch = useMutation({
    mutationFn: () =>
      switchOllamaEmbedder(
        {
          model: ollamaModel.trim(),
          dim: Number(ollamaDim),
          base_url: ollamaBaseUrl.trim(),
        },
        {},
      ),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
    },
  });

  return (
    <PageChrome title="Settings" eyebrow="Desktop">
      <div className="grid max-w-6xl gap-4 xl:grid-cols-2">
        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
            <div>
              <h2 className="text-sm font-semibold text-slate-100">Endpoints</h2>
              <p className="mt-1 text-xs text-slate-400">Stored locally for this Desktop window.</p>
            </div>
            <button
              type="button"
              onClick={() => setOpen(true)}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600"
            >
              Edit settings
            </button>
          </div>

          <dl className="mt-5 divide-y divide-slate-800 text-sm">
            <SettingsRow
              label="Solo API"
              value={apiUrl}
              detail={`${transport.label} - ${transport.detail}`}
            />
            <SettingsRow
              label="Auth"
              value={bearerToken ? 'bearer token active' : 'no bearer token'}
              detail="Token value is hidden outside the editor and scoped to this browser session."
            />
            <SettingsRow
              label="Storage"
              value="endpoints persist; bearer is session-only"
              detail="The bearer is removed when the browser session ends and is never written to localStorage."
            />
          </dl>

          <div className="mt-5 flex flex-wrap gap-2">
            <CopyButton label="Copy Solo URL" value={apiUrl} />
          </div>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">MCP Connections</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow label="Endpoint" value={mcpUrl} />
            <StatusRow label="Memory library" value={libraryName} />
            <StatusRow label="Sessions" value={String(solo.data?.mcp.sessions ?? 0)} />
            <StatusRow label="Probe" value={mcpProbeStatus(mcpProbe)} />
            <StatusRow label="Claude Desktop" value={claudeDesktopCommand} />
            <StatusRow label="Doctor" value={doctorCommand} />
          </dl>
          {mcpProbe.isError && (
            <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
              {errorMessage(mcpProbe.error)}
            </p>
          )}
          <div className="mt-5 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => mcpProbe.mutate()}
              disabled={mcpProbe.isPending}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {mcpProbe.isPending ? 'Checking MCP' : 'Probe MCP'}
            </button>
            <button
              type="button"
              onClick={() => onModeChange('connections')}
              className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
            >
              Open details
            </button>
            <CopyButton label="Copy MCP URL" value={mcpUrl} />
            <CopyButton label="Copy Claude Desktop" value={claudeDesktopCommand} />
            <CopyButton label="Copy Doctor" value={doctorCommand} />
          </div>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Runtime &amp; Embedder</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow
              label="Daemon"
              value={solo.data?.ok ? 'running' : statusLabel(solo.status)}
            />
            <StatusRow label="Version" value={solo.data?.version ?? 'not reported'} />
            <StatusRow label="Data dir" value={solo.data?.runtime?.data_dir ?? 'not reported'} />
            <StatusRow label="Config file" value={soloConfigPath(solo.data)} />
            <StatusRow label="Embedder" value={embedderSummary(solo.data)} />
            <StatusRow label="Embedder runtime" value={embedderRuntimeStatus(solo.data)} />
            <StatusRow label="Backend" value={embedderBackendLabel(solo.data)} />
            <StatusRow label="Ollama" value={ollamaSwitchStatus(solo.data)} />
            <StatusRow label="Steward runtime" value={stewardRuntimeStatus(solo.data)} />
            <StatusRow label="Selector" value="Solo Controls action" />
            <StatusRow label="Switch path" value="tray-supervised migration" />
            <StatusRow label="Reembed" value="handled by migration" />
          </dl>
          <p className="mt-4 rounded-md border border-slate-800 bg-slate-950/60 px-3 py-2 text-xs text-slate-300">
            Use Solo Controls &gt; Settings &gt; Embedder Migration for the one-click stop, backup,
            re-embed, and restart path. This browser panel checks the daemon guard and copies the
            same CLI command.
          </p>
          <div className="mt-5 grid gap-3 md:grid-cols-[minmax(0,1fr)_6rem]">
            <label className="text-xs font-medium uppercase text-slate-400">
              Model
              <input
                value={ollamaModel}
                onChange={(event) => setOllamaModel(event.target.value)}
                className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500"
              />
            </label>
            <label className="text-xs font-medium uppercase text-slate-400">
              Dim
              <input
                value={ollamaDim}
                onChange={(event) => setOllamaDim(event.target.value)}
                inputMode="numeric"
                className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500"
              />
            </label>
            <label className="text-xs font-medium uppercase text-slate-400 md:col-span-2">
              Base URL
              <input
                value={ollamaBaseUrl}
                onChange={(event) => setOllamaBaseUrl(event.target.value)}
                className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500"
              />
            </label>
          </div>
          {solo.isError && (
            <p className="mt-4 rounded-md border border-amber-800/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-100">
              {errorMessage(solo.error)}
            </p>
          )}
          {ollamaSwitch.isError && (
            <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
              {errorMessage(ollamaSwitch.error)}
            </p>
          )}
          {ollamaSwitch.data && (
            <div className="mt-4 rounded-md border border-sky-800/70 bg-sky-950/25 px-3 py-2 text-xs text-sky-100">
              <div className="font-medium">
                Config {ollamaSwitch.data.changed ? 'updated' : 'already set'}:{' '}
                {ollamaSwitch.data.next.name}
              </div>
              <div className="mt-1 text-sky-200">{ollamaSwitch.data.note}</div>
              <ol className="mt-2 list-decimal space-y-1 pl-4 text-sky-200">
                {ollamaSwitch.data.next_steps.map((step) => (
                  <li key={step}>{step}</li>
                ))}
              </ol>
            </div>
          )}
          <div className="mt-5 flex flex-wrap gap-2">
            <button
              type="button"
              onClick={() => void solo.refetch()}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600"
            >
              Check runtime
            </button>
            <button
              type="button"
              onClick={() => onModeChange('health')}
              className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
            >
              Open health
            </button>
            <button
              type="button"
              onClick={() => ollamaSwitch.mutate()}
              disabled={ollamaSwitch.isPending}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {ollamaSwitch.isPending ? 'Checking' : 'Check config guard'}
            </button>
            <CopyButton
              label="Copy migration command"
              value={ollamaEmbedderSwitchPlan(solo.data, {
                model: ollamaModel,
                dim: ollamaDim,
                baseUrl: ollamaBaseUrl,
              })}
            />
          </div>
        </section>

        <CapabilityPanel solo={solo} />

        <StewardLlmPanel solo={solo} />

        <StewardCadencePanel solo={solo} />

        <DerivedMemoryPanel onModeChange={onModeChange} />

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Admin &amp; Diagnostics</h2>
          <div className="mt-4 grid gap-2">
            <button
              type="button"
              onClick={() => onModeChange('health')}
              className="rounded-md border border-slate-800 bg-slate-950 px-3 py-2 text-left text-sm text-slate-200 hover:border-slate-600 hover:bg-slate-900"
            >
              Runtime health
            </button>
            <button
              type="button"
              onClick={() => onModeChange('connections')}
              className="rounded-md border border-slate-800 bg-slate-950 px-3 py-2 text-left text-sm text-slate-200 hover:border-slate-600 hover:bg-slate-900"
            >
              MCP connections
            </button>
            <button
              type="button"
              onClick={() => onModeChange('backups')}
              className="rounded-md border border-slate-800 bg-slate-950 px-3 py-2 text-left text-sm text-slate-200 hover:border-slate-600 hover:bg-slate-900"
            >
              Backups
            </button>
            <button
              type="button"
              onClick={() => onModeChange('logs')}
              className="rounded-md border border-slate-800 bg-slate-950 px-3 py-2 text-left text-sm text-slate-200 hover:border-slate-600 hover:bg-slate-900"
            >
              Logs
            </button>
          </div>
        </section>
        <HostSlotModules modules={hostModules} context={moduleContext} />
      </div>
      <SettingsDialog open={open} onClose={() => setOpen(false)} />
    </PageChrome>
  );
}

function CapabilityPanel({ solo }: { solo: UseQueryResult<SoloStatus, Error> }) {
  const capabilities = solo.data?.capabilities;
  const rows = [
    ['memory_recall', 'Memory recall'],
    ['documents', 'Documents'],
    ['clustering', 'Clustering'],
    ['knowledge_extraction', 'Knowledge extraction'],
    ['themes', 'Themes'],
    ['facts', 'Facts'],
    ['entities', 'Entities'],
    ['graph', 'Relationships'],
    ['contradictions', 'Contradictions'],
  ] as const;

  return (
    <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
      <div className="flex items-start justify-between gap-3">
        <div>
          <h2 className="text-sm font-semibold text-slate-100">Memory Capabilities</h2>
          <p className="mt-1 text-xs text-slate-400">
            What works now, what is waiting, and why.
          </p>
        </div>
        <button
          type="button"
          onClick={() => void solo.refetch()}
          className="rounded-md border border-slate-700 px-2 py-1 text-xs text-slate-200 hover:bg-slate-800"
        >
          Refresh
        </button>
      </div>
      {!capabilities ? (
        <p className="mt-4 text-xs text-slate-500">
          {solo.isError ? errorMessage(solo.error) : 'Checking capability state'}
        </p>
      ) : (
        <div className="mt-4 grid gap-2 sm:grid-cols-2">
          {rows.map(([key, label]) => {
            const capability = capabilities[key];
            return (
              <div key={key} className="rounded-md border border-slate-800 bg-slate-950/60 p-3">
                <div className="flex items-center justify-between gap-2">
                  <span className="text-xs font-medium text-slate-200">{label}</span>
                  <span className={`rounded px-2 py-0.5 text-[11px] uppercase ${capabilityTone(capability.state)}`}>
                    {capability.state}
                  </span>
                </div>
                <p className="mt-2 text-xs leading-5 text-slate-400">{capability.explanation}</p>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}

function capabilityTone(state: string): string {
  if (state === 'ready') return 'bg-emerald-950 text-emerald-200';
  if (state === 'pending') return 'bg-sky-950 text-sky-200';
  if (state === 'failed') return 'bg-red-950 text-red-200';
  if (state === 'disabled') return 'bg-amber-950 text-amber-200';
  return 'bg-slate-800 text-slate-300';
}

type OllamaSetupRoute = OllamaEndpoint | 'signed_cloud';

function StewardLlmPanel({ solo }: { solo: UseQueryResult<SoloStatus, Error> }) {
  const queryClient = useQueryClient();
  const [dirty, setDirty] = useState(false);
  const [llmMode, setLlmMode] = useState<StewardLlmMode>('ollama');
  const [llmModel, setLlmModel] = useState('qwen3:8b');
  const [llmBaseUrl, setLlmBaseUrl] = useState('http://localhost:11434');
  const [llmApiKeyEnv, setLlmApiKeyEnv] = useState('ANTHROPIC_API_KEY');
  const [ollamaRoute, setOllamaRoute] = useState<OllamaSetupRoute>('local');
  const [hostedConsent, setHostedConsent] = useState(false);
  const [restartRequested, setRestartRequested] = useState(false);

  useEffect(() => {
    if (dirty) return;
    const mode = stewardLlmMode(solo.data);
    setLlmMode(mode);
    setLlmModel(stewardLlmModel(solo.data, mode));
    setLlmApiKeyEnv(solo.data?.steward?.api_key_env ?? defaultLlmApiKeyEnv(mode));
    const endpoint = solo.data?.steward?.endpoint ?? 'local';
    const model = stewardLlmModel(solo.data, mode);
    setOllamaRoute(endpoint === 'local' && model.endsWith('-cloud') ? 'signed_cloud' : endpoint);
    setLlmBaseUrl(stewardLlmBaseUrl(solo.data));
    setHostedConsent(solo.data?.steward?.hosted_processing_consent ?? false);
  }, [dirty, solo.data]);

  const llmSwitch = useMutation({
    mutationFn: () =>
      switchStewardLlm(
        {
          mode: llmMode,
          ...(llmMode !== 'none' ? { model: llmModel.trim() } : {}),
          ...(llmMode === 'ollama' ? { base_url: llmBaseUrl.trim() } : {}),
          ...(llmMode === 'ollama'
            ? { endpoint: ollamaRoute === 'signed_cloud' ? 'local' : ollamaRoute }
            : {}),
          ...(llmMode === 'anthropic' ||
          llmMode === 'openai' ||
          (llmMode === 'ollama' &&
            (ollamaRoute === 'cloud' || ollamaRoute === 'custom') &&
            llmApiKeyEnv.trim().length > 0)
            ? { api_key_env: llmApiKeyEnv.trim() }
            : {}),
          ...(llmMode !== 'none' ? { hosted_processing_consent: hostedConsent } : {}),
        },
        {},
      ),
    onSuccess: () => {
      setDirty(false);
      setRestartRequested(false);
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
    },
  });

  const runtimeRestart = useMutation({
    mutationFn: () => restartSoloRuntime(),
    onSuccess: () => {
      setRestartRequested(true);
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      window.setTimeout(() => {
        void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      }, 1500);
      window.setTimeout(() => {
        void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      }, 4000);
    },
  });
  const backfill = useMutation({
    mutationFn: () => startStewardBackfill(),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      void solo.refetch();
    },
  });

  const runtimeMatches = stewardRuntimeMatchesConfig(solo.data, llmSwitch.data?.next);
  const canRestartAfterSwitch = Boolean(
    llmSwitch.data?.restart_required && runtimeMatches !== true,
  );

  useEffect(() => {
    if (runtimeMatches === true) setRestartRequested(false);
  }, [runtimeMatches]);

  const chooseMode = (mode: StewardLlmMode) => {
    setDirty(true);
    setLlmMode(mode);
    setLlmModel(defaultLlmModel(mode));
    setLlmApiKeyEnv(defaultLlmApiKeyEnv(mode));
    setHostedConsent(false);
  };
  const chooseOllamaRoute = (route: OllamaSetupRoute) => {
    setDirty(true);
    setOllamaRoute(route);
    setHostedConsent(false);
    setLlmBaseUrl(route === 'cloud' ? 'https://ollama.com' : 'http://localhost:11434');
    setLlmModel(route === 'cloud' || route === 'signed_cloud' ? 'gpt-oss:120b-cloud' : 'qwen3:8b');
    setLlmApiKeyEnv(route === 'cloud' ? 'OLLAMA_API_KEY' : '');
  };
  const commands = llmSwitch.data?.environment_commands ?? [];
  const normalizedLlmModel = llmModel.trim();
  const hostedProcessing =
    llmMode === 'anthropic' ||
    llmMode === 'openai' ||
    (llmMode === 'ollama' &&
      (ollamaRoute === 'cloud' ||
        ollamaRoute === 'signed_cloud' ||
        normalizedLlmModel.endsWith('-cloud') ||
        (ollamaRoute === 'custom' && !isLoopbackOllamaUrl(llmBaseUrl))));
  const signedCloudModelInvalid =
    llmMode === 'ollama' &&
    ollamaRoute === 'signed_cloud' &&
    !normalizedLlmModel.endsWith('-cloud');
  const backfillStatus = solo.data?.steward?.backfill;

  return (
    <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
      <h2 className="text-sm font-semibold text-slate-100">Steward LLM</h2>
      <dl className="mt-4 space-y-3 text-sm">
        <StatusRow label="Config" value={stewardConfiguredLlm(solo.data)} />
        <StatusRow label="Runtime" value={stewardRuntimeLlm(solo.data)} />
        <StatusRow
          label="Runtime verification"
          value={stewardRuntimeVerification(solo.data, llmSwitch.data?.next, restartRequested)}
        />
        <StatusRow label="Auto run" value={stewardAutomaticStatus(solo.data)} />
        <StatusRow label="Cadence" value={stewardTriggerStatus(solo.data)} />
        <StatusRow label="Can write triples" value={stewardTripleCapability(solo.data)} />
      </dl>

      <div className="mt-5 grid grid-cols-2 gap-2 sm:grid-cols-4">
        {[
          ['ollama', 'Ollama'],
          ['anthropic', 'Anthropic'],
          ['openai', 'OpenAI'],
          ['none', 'Disabled'],
        ].map(([mode, label]) => (
          <button
            key={mode}
            type="button"
            onClick={() => chooseMode(mode as StewardLlmMode)}
            className={`rounded-md border px-3 py-2 text-sm ${
              llmMode === mode
                ? 'border-sky-500 bg-sky-950/60 text-sky-100'
                : 'border-slate-700 bg-slate-950 text-slate-300 hover:border-slate-500'
            }`}
            aria-pressed={llmMode === mode}
          >
            {label}
          </button>
        ))}
      </div>

      {llmMode === 'ollama' && (
        <div className="mt-4 grid grid-cols-2 gap-2 sm:grid-cols-4">
          {([
            ['local', 'Local model'],
            ['signed_cloud', 'Cloud via local'],
            ['cloud', 'Cloud direct'],
            ['custom', 'Custom'],
          ] as const).map(([route, label]) => (
            <button
              key={route}
              type="button"
              onClick={() => chooseOllamaRoute(route)}
              className={`rounded-md border px-3 py-2 text-xs ${
                ollamaRoute === route
                  ? 'border-emerald-500 bg-emerald-950/50 text-emerald-100'
                  : 'border-slate-700 bg-slate-950 text-slate-300 hover:border-slate-500'
              }`}
              aria-pressed={ollamaRoute === route}
            >
              {label}
            </button>
          ))}
        </div>
      )}

      {llmMode !== 'none' && (
        <div className="mt-5 grid gap-3">
          <label className="text-xs font-medium uppercase text-slate-400">
            Model
            <input
              value={llmModel}
              onChange={(event) => {
                setDirty(true);
                setLlmModel(event.target.value);
              }}
              className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500"
            />
          </label>
          {llmMode === 'ollama' ? (
            <>
            <label className="text-xs font-medium uppercase text-slate-400">
              Base URL
              <input
                value={llmBaseUrl}
                disabled={ollamaRoute !== 'custom'}
                onChange={(event) => {
                  setDirty(true);
                  setLlmBaseUrl(event.target.value);
                }}
                className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500 disabled:cursor-not-allowed disabled:text-slate-500"
              />
            </label>
            {(ollamaRoute === 'cloud' || ollamaRoute === 'custom') && (
              <label className="text-xs font-medium uppercase text-slate-400">
                API key env {ollamaRoute === 'custom' && '(optional)'}
                <input
                  value={llmApiKeyEnv}
                  onChange={(event) => {
                    setDirty(true);
                    setLlmApiKeyEnv(event.target.value);
                  }}
                  className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500"
                />
              </label>
            )}
            </>
          ) : (
            <label className="text-xs font-medium uppercase text-slate-400">
              API key env
              <input
                value={llmApiKeyEnv}
                onChange={(event) => {
                  setDirty(true);
                  setLlmApiKeyEnv(event.target.value);
                }}
                className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500"
              />
            </label>
          )}
        </div>
      )}

      {signedCloudModelInvalid && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          Cloud via local requires an Ollama Cloud model whose name ends in <code>-cloud</code>.
        </p>
      )}

      {llmMode !== 'none' && (
        <div className={`mt-4 rounded-md border px-3 py-3 text-xs ${hostedProcessing ? 'border-amber-800/70 bg-amber-950/25 text-amber-100' : 'border-emerald-800/70 bg-emerald-950/25 text-emerald-100'}`}>
          <div className="font-medium">
            {hostedProcessing
              ? `Memory content will be processed off device by ${llmMode === 'ollama' ? (ollamaRoute === 'cloud' || ollamaRoute === 'signed_cloud' || normalizedLlmModel.endsWith('-cloud') ? 'Ollama Cloud' : 'the configured Ollama host') : llmMode === 'anthropic' ? 'Anthropic' : 'OpenAI'}.`
              : 'Memory content stays on this device and is processed by local Ollama.'}
          </div>
          {hostedProcessing && (
            <label className="mt-3 flex items-start gap-2 text-amber-100">
              <input
                type="checkbox"
                checked={hostedConsent}
                onChange={(event) => {
                  setDirty(true);
                  setHostedConsent(event.target.checked);
                }}
                className="mt-0.5"
              />
              <span>I understand selected memory content will leave this device and consent to this provider processing it.</span>
            </label>
          )}
        </div>
      )}

      {solo.data?.steward?.note && (
        <p className="mt-4 rounded-md border border-slate-800 bg-slate-950/60 px-3 py-2 text-xs text-slate-300">
          {solo.data.steward.note}
        </p>
      )}

      {llmSwitch.isError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(llmSwitch.error)}
        </p>
      )}

      {llmSwitch.data && (
        <div className="mt-4 rounded-md border border-sky-800/70 bg-sky-950/25 px-3 py-2 text-xs text-sky-100">
          <div className="font-medium">
            Steward LLM {llmSwitch.data.changed ? 'saved' : 'already set'}:{' '}
            {llmSettingsLabel(llmSwitch.data.next)}
          </div>
          <div className="mt-1 text-sky-200">{llmSwitch.data.note}</div>
          <ol className="mt-2 list-decimal space-y-1 pl-4 text-sky-200">
            {llmSwitch.data.next_steps.map((step) => (
              <li key={step}>{step}</li>
            ))}
          </ol>
        </div>
      )}

      {runtimeRestart.isError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(runtimeRestart.error)}
        </p>
      )}

      {runtimeRestart.data && (
        <p className="mt-4 rounded-md border border-emerald-800/70 bg-emerald-950/25 px-3 py-2 text-xs text-emerald-100">
          {runtimeRestart.data.note}
        </p>
      )}

      {solo.data?.steward?.runtime_has_llm && (
        <div className="mt-4 rounded-md border border-emerald-800/70 bg-emerald-950/20 px-3 py-3 text-xs text-emerald-100">
          <div className="font-medium">Backfill existing memories now</div>
          <p className="mt-1 text-emerald-200">
            Run clustering and knowledge extraction immediately instead of waiting for the hourly schedule.
          </p>
          {backfillStatus && (
            <div className="mt-3">
              <div className="flex justify-between text-[11px] uppercase text-emerald-200">
                <span>{backfillStatus.phase.replaceAll('_', ' ')}</span>
                <span>{backfillStatus.progress_percent}%</span>
              </div>
              <div className="mt-1 h-2 overflow-hidden rounded bg-slate-900">
                <div
                  className="h-full rounded bg-emerald-500 transition-[width]"
                  style={{ width: `${backfillStatus.progress_percent}%` }}
                />
              </div>
              <p className="mt-2 text-emerald-200">{backfillStatus.note}</p>
              {backfillStatus.error && <p className="mt-1 text-red-200">{backfillStatus.error}</p>}
            </div>
          )}
          {backfill.isError && <p className="mt-2 text-red-200">{errorMessage(backfill.error)}</p>}
          <button
            type="button"
            onClick={() => backfill.mutate()}
            disabled={backfill.isPending || backfillStatus?.status === 'running'}
            className="mt-3 rounded-md bg-emerald-700 px-3 py-2 text-sm font-medium text-white hover:bg-emerald-600 disabled:cursor-not-allowed disabled:bg-slate-700"
          >
            {backfillStatus?.status === 'running' || backfill.isPending ? 'Backfill running' : 'Backfill existing memories'}
          </button>
        </div>
      )}

      <div className="mt-5 flex flex-wrap gap-2">
          <button
            type="button"
            onClick={() => llmSwitch.mutate()}
            disabled={
              llmSwitch.isPending ||
              signedCloudModelInvalid ||
              (hostedProcessing && !hostedConsent)
            }
          className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
        >
          {llmSwitch.isPending ? 'Applying' : 'Apply LLM config'}
        </button>
        {canRestartAfterSwitch && (
          <button
            type="button"
            onClick={() => runtimeRestart.mutate()}
            disabled={runtimeRestart.isPending}
            className="rounded-md border border-emerald-600 bg-emerald-950/40 px-3 py-2 text-sm font-medium text-emerald-100 hover:bg-emerald-900/50 disabled:cursor-not-allowed disabled:border-slate-700 disabled:bg-slate-800 disabled:text-slate-400"
          >
            {runtimeRestart.isPending ? 'Restarting' : 'Restart Solo now'}
          </button>
        )}
        <button
          type="button"
          onClick={() => void solo.refetch()}
          className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
        >
          Refresh status
        </button>
        {commands.length > 0 && (
          <CopyButton label="Copy env commands" value={commands.join('\n')} />
        )}
      </div>
    </section>
  );
}

function StewardCadencePanel({ solo }: { solo: UseQueryResult<SoloStatus, Error> }) {
  const queryClient = useQueryClient();
  const [dirty, setDirty] = useState(false);
  const [triggerInterval, setTriggerInterval] = useState('3600');
  const [triggerEpisodeCount, setTriggerEpisodeCount] = useState('50');
  const [consolidateInterval, setConsolidateInterval] = useState('3600');
  const [clusterTimeout, setClusterTimeout] = useState('60');
  const [clusterMinSize, setClusterMinSize] = useState('2');
  const [clusterThreshold, setClusterThreshold] = useState('0.55');
  const [localError, setLocalError] = useState<string | null>(null);
  const [restartRequested, setRestartRequested] = useState(false);

  useEffect(() => {
    if (dirty) return;
    const steward = solo.data?.steward;
    if (!steward) return;
    setTriggerInterval(String(steward.trigger_interval_secs));
    setTriggerEpisodeCount(String(steward.trigger_episode_count));
    setConsolidateInterval(String(steward.consolidate_interval_secs));
    setClusterTimeout(String(steward.cluster_timeout_secs));
    setClusterMinSize(String(steward.cluster_min_size));
    setClusterThreshold(formatThreshold(steward.cluster_cosine_threshold));
  }, [dirty, solo.data]);

  const cadenceSwitch = useMutation({
    mutationFn: () => {
      const next = {
        trigger_interval_secs: parseNonNegativeInteger(triggerInterval, 'Triple interval'),
        trigger_episode_count: parseNonNegativeInteger(triggerEpisodeCount, 'Episode trigger'),
        consolidate_interval_secs: parseNonNegativeInteger(
          consolidateInterval,
          'Consolidation interval',
        ),
        cluster_timeout_secs: parseNonNegativeInteger(clusterTimeout, 'Cluster timeout'),
        cluster_min_size: parsePositiveInteger(clusterMinSize, 'Cluster min size'),
        cluster_cosine_threshold: parseClusterThreshold(clusterThreshold),
      };
      setLocalError(null);
      return switchStewardCadence(next);
    },
    onSuccess: () => {
      setDirty(false);
      setRestartRequested(false);
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
    },
    onError: (error) => {
      setLocalError(errorMessage(error));
    },
  });

  const runtimeRestart = useMutation({
    mutationFn: () => restartSoloRuntime(),
    onSuccess: () => {
      setRestartRequested(true);
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      window.setTimeout(() => {
        void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      }, 1500);
      window.setTimeout(() => {
        void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      }, 4000);
    },
  });

  const canRestart = Boolean(cadenceSwitch.data?.restart_required && cadenceSwitch.data.changed);

  return (
    <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
      <h2 className="text-sm font-semibold text-slate-100">Steward Cadence</h2>
      <dl className="mt-4 space-y-3 text-sm">
        <StatusRow label="Triple extraction" value={stewardTriggerStatus(solo.data)} />
        <StatusRow
          label="Consolidation"
          value={`${solo.data?.steward?.consolidate_interval_secs ?? 0}s`}
        />
        <StatusRow
          label="Per-cluster timeout"
          value={`${solo.data?.steward?.cluster_timeout_secs ?? 0}s`}
        />
        <StatusRow
          label="Cluster min size"
          value={String(solo.data?.steward?.cluster_min_size ?? 0)}
        />
        <StatusRow
          label="Similarity threshold"
          value={formatThreshold(solo.data?.steward?.cluster_cosine_threshold ?? 0)}
        />
        <StatusRow
          label="Apply state"
          value={restartRequested ? 'waiting for restart' : 'restart required after save'}
        />
      </dl>

      <div className="mt-5 grid gap-3 sm:grid-cols-2">
        <CadenceInput
          label="Triple interval"
          value={triggerInterval}
          onChange={(value) => {
            setDirty(true);
            setTriggerInterval(value);
          }}
        />
        <CadenceInput
          label="Episode trigger"
          value={triggerEpisodeCount}
          onChange={(value) => {
            setDirty(true);
            setTriggerEpisodeCount(value);
          }}
        />
        <CadenceInput
          label="Consolidation interval"
          value={consolidateInterval}
          onChange={(value) => {
            setDirty(true);
            setConsolidateInterval(value);
          }}
        />
        <CadenceInput
          label="Cluster timeout"
          value={clusterTimeout}
          onChange={(value) => {
            setDirty(true);
            setClusterTimeout(value);
          }}
        />
        <CadenceInput
          label="Cluster min size"
          value={clusterMinSize}
          onChange={(value) => {
            setDirty(true);
            setClusterMinSize(value);
          }}
        />
        <CadenceInput
          label="Similarity threshold"
          value={clusterThreshold}
          inputMode="decimal"
          onChange={(value) => {
            setDirty(true);
            setClusterThreshold(value);
          }}
        />
      </div>

      {localError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {localError}
        </p>
      )}

      {cadenceSwitch.data && (
        <p className="mt-4 rounded-md border border-sky-800/70 bg-sky-950/25 px-3 py-2 text-xs text-sky-100">
          {cadenceSwitch.data.note}
        </p>
      )}

      {runtimeRestart.isError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(runtimeRestart.error)}
        </p>
      )}

      {runtimeRestart.data && (
        <p className="mt-4 rounded-md border border-emerald-800/70 bg-emerald-950/25 px-3 py-2 text-xs text-emerald-100">
          {runtimeRestart.data.note}
        </p>
      )}

      <div className="mt-5 flex flex-wrap gap-2">
        <button
          type="button"
          onClick={() => cadenceSwitch.mutate()}
          disabled={cadenceSwitch.isPending}
          className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
        >
          {cadenceSwitch.isPending ? 'Applying' : 'Apply cadence'}
        </button>
        {canRestart && (
          <button
            type="button"
            onClick={() => runtimeRestart.mutate()}
            disabled={runtimeRestart.isPending}
            className="rounded-md border border-emerald-600 bg-emerald-950/40 px-3 py-2 text-sm font-medium text-emerald-100 hover:bg-emerald-900/50 disabled:cursor-not-allowed disabled:border-slate-700 disabled:bg-slate-800 disabled:text-slate-400"
          >
            {runtimeRestart.isPending ? 'Restarting' : 'Restart Solo now'}
          </button>
        )}
        <button
          type="button"
          onClick={() => void solo.refetch()}
          className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
        >
          Refresh status
        </button>
      </div>
    </section>
  );
}

function DerivedMemoryPanel({ onModeChange }: { onModeChange: (mode: AppMode) => void }) {
  const queryClient = useQueryClient();
  const graph = useGraphData();
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const stats = summarizeGraph(graph.data);
  const topEntity = selectTopEntity(graph.data);
  const solo = useQuery({
    queryKey: ['desktop-settings', 'solo-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
  });
  const facts = useQuery({
    queryKey: [
      'desktop-settings',
      'facts-about',
      apiUrl,
      connectionRevision,
      topEntity?.subject ?? null,
    ],
    queryFn: ({ signal }) =>
      fetchFactsAbout(
        {
          subject: topEntity?.subject ?? '',
          includeAsObject: true,
          limit: 5,
        },
        { signal },
      ),
    enabled: Boolean(topEntity) && !USE_MOCKS,
    retry: false,
  });
  const qualityAudit = useQuery({
    queryKey: ['desktop-settings', 'memory-quality-audit', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchMemoryQualityAudit({}, { signal }),
    enabled: !USE_MOCKS,
    retry: false,
  });
  const qualityReviews = useQuery({
    queryKey: ['desktop-settings', 'memory-quality-reviews', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchMemoryQualityReviews(20, { signal }),
    enabled: !USE_MOCKS,
    retry: false,
  });
  const dismissQualityReview = useMutation({
    mutationFn: (reviewId: string) =>
      updateMemoryQualityReview(
        reviewId,
        {
          status: 'dismissed',
          note: 'Dismissed from Memory Quality inbox',
        },
        {},
      ),
    onSuccess: () => {
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-audit'],
      });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-reviews'],
      });
    },
  });
  const consolidate = useMutation({
    mutationFn: () => consolidateMemory({}),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'facts-about'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-audit'],
      });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-reviews'],
      });
    },
  });
  const triplesExtract = useMutation({
    mutationFn: () => extractTriplesNow({}),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'facts-about'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-audit'],
      });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-reviews'],
      });
    },
  });
  const repairStale = useMutation({
    mutationFn: () =>
      repairDerivedMemory(
        {
          mode: 'stale_abstractions',
          min_empty_abstraction_episode_count: 25,
        },
        {},
      ),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'facts-about'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-audit'],
      });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-reviews'],
      });
    },
  });
  const rebuildDerived = useMutation({
    mutationFn: () => repairDerivedMemory({ mode: 'rebuild_all' }, {}),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'facts-about'] });
      void queryClient.invalidateQueries({ queryKey: ['desktop-settings', 'solo-status'] });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-audit'],
      });
      void queryClient.invalidateQueries({
        queryKey: ['desktop-settings', 'memory-quality-reviews'],
      });
    },
  });

  return (
    <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4 xl:col-span-2">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h2 className="text-sm font-semibold text-slate-100">Derived Memory &amp; Triples</h2>
          <p className="mt-1 text-xs text-slate-400">
            Clusters, entities, and facts in the memory library.
          </p>
        </div>
        <span className="rounded border border-slate-700 px-2 py-1 text-xs text-slate-300">
          {graph.isError
            ? 'graph unavailable'
            : graph.isFetching
              ? 'checking graph'
              : 'graph ready'}
        </span>
      </div>

      <div className="mt-5 grid gap-x-6 gap-y-4 sm:grid-cols-2 lg:grid-cols-5">
        <CompactStat label="Episodes" value={String(stats.episodes)} />
        <CompactStat label="Clusters" value={String(stats.clusters)} />
        <CompactStat label="Entities" value={String(stats.entities)} />
        <CompactStat label="Triples" value={String(stats.triples)} />
        <CompactStat label="Semantic links" value={String(stats.semanticEdges)} />
      </div>

      <dl className="mt-5 grid gap-3 text-sm md:grid-cols-2">
        <StatusRow
          label="Sample subject"
          value={topEntity ? `${topEntity.subject} (${topEntity.refCount})` : 'no entity nodes yet'}
        />
        <StatusRow
          label="Fact sample"
          value={USE_MOCKS ? 'mock graph' : factsStatus(facts.status, facts.data)}
        />
        <StatusRow label="Can write triples" value={stewardTripleCapability(solo.data)} />
        <StatusRow label="Runtime LLM" value={stewardRuntimeLlm(solo.data)} />
        <StatusRow label="Auto triples" value={stewardAutomaticStatus(solo.data)} />
        <StatusRow label="Trigger" value={stewardTriggerStatus(solo.data)} />
        <StatusRow label="Consolidation" value={consolidationStatus(consolidate.data)} />
        <StatusRow
          label="Last run"
          value={
            consolidate.data
              ? `${consolidate.data.episodes_seen} episodes scanned`
              : 'not run from this page'
          }
        />
        <StatusRow label="Triple extraction" value={triplesExtractStatus(triplesExtract.data)} />
        <StatusRow
          label="Derived repair"
          value={derivedRepairStatus(rebuildDerived.data ?? repairStale.data)}
        />
      </dl>

      <div className="mt-5 border-t border-slate-800 pt-4">
        <h3 className="text-xs font-semibold uppercase text-slate-400">Steward Runtime</h3>
        <dl className="mt-3 grid gap-3 text-sm md:grid-cols-2">
          <StatusRow label="Active LLM" value={stewardRuntimeLlm(solo.data)} />
          <StatusRow label="Next automatic run" value={stewardNextAutomaticRun(solo.data)} />
          <StatusRow label="Pending Steward work" value={stewardPendingClusters(solo.data)} />
          <StatusRow label="Last triple run" value={stewardLastTripleRun(solo.data)} />
          <StatusRow label="Last issue" value={stewardLastTripleIssue(solo.data)} />
          <StatusRow label="Last batch counts" value={stewardLastBatchCounts(solo.data)} />
        </dl>
      </div>

      <MemoryQualityAuditPanel
        audit={qualityAudit.data}
        reviews={qualityReviews.data}
        error={qualityAudit.error}
        isError={qualityAudit.isError}
        isFetching={qualityAudit.isFetching || qualityReviews.isFetching}
        reviewActionError={dismissQualityReview.error}
        isReviewActionError={dismissQualityReview.isError}
        dismissingReviewId={
          dismissQualityReview.isPending ? dismissQualityReview.variables : undefined
        }
        onRefresh={() => {
          void qualityAudit.refetch();
          void qualityReviews.refetch();
        }}
        onDismissReview={(reviewId) => dismissQualityReview.mutate(reviewId)}
      />

      {stats.triples === 0 && (
        <p className="mt-4 rounded-md border border-amber-800/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-100">
          No triple edges are visible in the memory library yet. Consolidation can create clusters
          first; triples appear after the Steward/LLM abstraction batch writes facts.
        </p>
      )}

      {solo.data?.steward && !solo.data.steward.can_write_triples && (
        <p className="mt-4 rounded-md border border-amber-800/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-100">
          {solo.data.steward.note}
        </p>
      )}

      {consolidate.isError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(consolidate.error)}
        </p>
      )}

      {triplesExtract.isError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(triplesExtract.error)}
        </p>
      )}

      {repairStale.isError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(repairStale.error)}
        </p>
      )}

      {rebuildDerived.isError && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(rebuildDerived.error)}
        </p>
      )}

      {facts.isError && topEntity && (
        <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(facts.error)}
        </p>
      )}

      {facts.data && facts.data.length > 0 && (
        <div className="mt-4 divide-y divide-slate-800 rounded-md border border-slate-800">
          {facts.data.map((fact) => (
            <FactRow key={fact.triple_id} fact={fact} />
          ))}
        </div>
      )}

      <div className="mt-5 flex flex-wrap gap-2">
        <button
          type="button"
          onClick={() => consolidate.mutate()}
          disabled={consolidate.isPending}
          className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
        >
          {consolidate.isPending ? 'Consolidating' : 'Run consolidation'}
        </button>
        <button
          type="button"
          onClick={() => triplesExtract.mutate()}
          disabled={triplesExtract.isPending || !(solo.data?.steward?.can_write_triples ?? false)}
          className="rounded-md bg-emerald-700 px-3 py-2 text-sm font-medium text-white hover:bg-emerald-600 disabled:cursor-not-allowed disabled:bg-slate-700"
        >
          {triplesExtract.isPending ? 'Extracting triples' : 'Run triples now'}
        </button>
        <button
          type="button"
          onClick={() => repairStale.mutate()}
          disabled={repairStale.isPending}
          className="rounded-md border border-amber-700 px-3 py-2 text-sm text-amber-100 hover:bg-amber-950/40 disabled:cursor-not-allowed disabled:border-slate-700 disabled:text-slate-500"
        >
          {repairStale.isPending ? 'Repairing' : 'Repair stale graph'}
        </button>
        <button
          type="button"
          onClick={() => {
            if (
              window.confirm(
                'Rebuild derived graph data for this memory library? Raw memories and documents are kept.',
              )
            ) {
              rebuildDerived.mutate();
            }
          }}
          disabled={rebuildDerived.isPending}
          className="rounded-md border border-rose-700 px-3 py-2 text-sm text-rose-100 hover:bg-rose-950/40 disabled:cursor-not-allowed disabled:border-slate-700 disabled:text-slate-500"
        >
          {rebuildDerived.isPending ? 'Rebuilding' : 'Rebuild derived graph'}
        </button>
        <button
          type="button"
          onClick={() => void graph.refetch()}
          className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
        >
          Refresh graph
        </button>
        <button
          type="button"
          onClick={() => onModeChange('memories')}
          className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800"
        >
          Open memories
        </button>
        <CopyButton label="Copy CLI" value="solo consolidate" />
      </div>
    </section>
  );
}

function MemoryQualityAuditPanel({
  audit,
  reviews,
  error,
  isError,
  isFetching,
  isReviewActionError,
  reviewActionError,
  dismissingReviewId,
  onRefresh,
  onDismissReview,
}: {
  audit?: MemoryQualityAuditReport;
  reviews?: MemoryQualityReviewsResponse;
  error: unknown;
  isError: boolean;
  isFetching: boolean;
  isReviewActionError: boolean;
  reviewActionError: unknown;
  dismissingReviewId?: string;
  onRefresh: () => void;
  onDismissReview: (reviewId: string) => void;
}) {
  const topIssues = audit ? topQualityIssues(audit.issues) : [];
  const reviewItems = reviews?.items ?? [];
  const literalPct =
    audit && audit.totals.active_triples > 0
      ? Math.round((audit.totals.literal_triples / audit.totals.active_triples) * 100)
      : 0;

  return (
    <div className="mt-5 border-t border-slate-800 pt-4">
      <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
        <div>
          <h3 className="text-xs font-semibold uppercase text-slate-400">Memory Quality</h3>
          {audit && (
            <div className="mt-1 text-xs text-slate-500">
              {formatEpochMs(audit.generated_at_ms)}
            </div>
          )}
        </div>
        <button
          type="button"
          onClick={onRefresh}
          disabled={isFetching}
          className="rounded-md border border-slate-700 px-3 py-1.5 text-xs text-slate-200 hover:bg-slate-800 disabled:cursor-not-allowed disabled:text-slate-500"
        >
          {isFetching ? 'Auditing' : 'Run quality audit'}
        </button>
      </div>

      {isError && (
        <p className="mt-3 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(error)}
        </p>
      )}

      {isReviewActionError && (
        <p className="mt-3 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
          {errorMessage(reviewActionError)}
        </p>
      )}

      {!audit && !isError && (
        <p className="mt-3 text-xs text-slate-500">
          {isFetching ? 'Checking memory quality' : 'No quality report loaded'}
        </p>
      )}

      {audit && (
        <>
          <div className="mt-4 grid gap-x-6 gap-y-4 sm:grid-cols-2 lg:grid-cols-6">
            <CompactStat label="Score" value={`${audit.health.score} ${audit.health.grade}`} />
            <CompactStat label="Facts" value={String(audit.totals.active_triples)} />
            <CompactStat label="Entity facts" value={String(audit.totals.entity_triples)} />
            <CompactStat label="Literal facts" value={`${literalPct}%`} />
            <CompactStat label="Issues" value={String(audit.issues.length)} />
            <CompactStat label="Review" value={String(reviewItems.length)} />
          </div>

          {reviewItems.length > 0 && (
            <div className="mt-4 rounded-md border border-amber-900/70 bg-amber-950/20">
              <div className="border-b border-amber-900/60 px-3 py-2 text-xs font-semibold uppercase text-amber-100">
                Needs Review ({reviewItems.length})
              </div>
              <div className="divide-y divide-amber-900/50">
                {reviewItems.slice(0, 5).map((item) => (
                  <div key={item.review_id} className="px-3 py-2 text-xs">
                    <div className="flex flex-col gap-2 sm:flex-row sm:items-start sm:justify-between">
                      <div className="min-w-0">
                        <div className="flex flex-wrap items-center gap-2">
                          <span className="rounded border border-amber-800 px-1.5 py-0.5 font-mono text-[11px] text-amber-100">
                            {item.reason_code}
                          </span>
                          <span className="break-all font-mono text-slate-300">
                            {item.subject_id} {item.predicate} {item.object_id}
                          </span>
                          <span className="font-mono text-slate-500">
                            {Math.round(item.confidence * 100)}%
                          </span>
                        </div>
                        <div className="mt-1 text-slate-400">{item.reason}</div>
                      </div>
                      <button
                        type="button"
                        onClick={() => onDismissReview(item.review_id)}
                        disabled={dismissingReviewId === item.review_id}
                        className="shrink-0 rounded-md border border-amber-800 px-2 py-1 text-[11px] text-amber-100 hover:bg-amber-900/40 disabled:cursor-not-allowed disabled:text-amber-700"
                      >
                        {dismissingReviewId === item.review_id ? 'Dismissing' : 'Dismiss'}
                      </button>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )}

          {topIssues.length === 0 ? (
            <p className="mt-4 rounded-md border border-emerald-800/70 bg-emerald-950/25 px-3 py-2 text-xs text-emerald-100">
              No quality issues detected in the active derived memory.
            </p>
          ) : (
            <div className="mt-4 divide-y divide-slate-800 rounded-md border border-slate-800">
              {topIssues.map((issue) => (
                <div key={issue.code} className="px-3 py-3">
                  <div className="flex flex-col gap-2 sm:flex-row sm:items-start sm:justify-between">
                    <div className="min-w-0">
                      <div className="flex flex-wrap items-center gap-2">
                        <span
                          className={[
                            'rounded border px-1.5 py-0.5 text-[11px] uppercase',
                            issueSeverityClass(issue.severity),
                          ].join(' ')}
                        >
                          {issue.severity}
                        </span>
                        <span className="font-mono text-xs text-slate-400">{issue.code}</span>
                      </div>
                      <div className="mt-1 text-sm text-slate-200">{issue.summary}</div>
                    </div>
                    <span className="shrink-0 font-mono text-xs text-slate-400">{issue.count}</span>
                  </div>
                  {issue.samples.length > 0 && (
                    <div className="mt-2 space-y-1">
                      {issue.samples.slice(0, 3).map((sample) => (
                        <div
                          key={sample}
                          className="break-all font-mono text-[11px] leading-relaxed text-slate-500"
                        >
                          {sample}
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              ))}
            </div>
          )}

          {audit.alias_groups.length > 0 && (
            <div className="mt-3 text-xs text-slate-400">
              Alias candidates:{' '}
              <span className="font-mono text-slate-300">
                {audit.alias_groups
                  .slice(0, 3)
                  .map((group) => group.labels.join(' / '))
                  .join(' · ')}
              </span>
            </div>
          )}
        </>
      )}
    </div>
  );
}

function topQualityIssues(issues: MemoryQualityIssue[]): MemoryQualityIssue[] {
  const rank = new Map([
    ['critical', 0],
    ['warning', 1],
    ['info', 2],
  ]);
  return issues
    .slice()
    .sort((a, b) => {
      const severity = (rank.get(a.severity) ?? 3) - (rank.get(b.severity) ?? 3);
      if (severity !== 0) return severity;
      return b.count - a.count;
    })
    .slice(0, 5);
}

function issueSeverityClass(severity: string): string {
  if (severity === 'critical') return 'border-red-700 bg-red-950/40 text-red-200';
  if (severity === 'warning') return 'border-amber-700 bg-amber-950/40 text-amber-200';
  return 'border-slate-700 bg-slate-950 text-slate-300';
}

function CompactStat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-xs font-medium uppercase text-slate-500">{label}</div>
      <div className="mt-1 text-2xl font-semibold text-slate-100">{value}</div>
    </div>
  );
}

function FactRow({ fact }: { fact: FactHit }) {
  return (
    <div className="grid gap-1 px-3 py-2 text-sm sm:grid-cols-[minmax(0,1fr)_7rem] sm:gap-3">
      <div className="min-w-0">
        <div className="truncate font-mono text-xs text-slate-100">
          {fact.subject_id} {fact.predicate} {fact.object_id}
        </div>
        <div className="mt-1 truncate text-xs text-slate-400">{fact.triple_id}</div>
      </div>
      <div className="text-xs text-slate-400 sm:text-right">
        {(fact.confidence * 100).toFixed(0)}%
      </div>
    </div>
  );
}

function PageChrome({
  title,
  eyebrow,
  flush = false,
  children,
}: {
  title: string;
  eyebrow: string;
  flush?: boolean;
  children: ReactNode;
}) {
  return (
    <div className="flex h-full flex-col">
      <header className="border-b border-slate-800 bg-slate-950 px-5 py-4">
        <div className="text-xs font-medium uppercase text-slate-400">{eyebrow}</div>
        <h1 className="mt-1 text-xl font-semibold text-slate-100">{title}</h1>
      </header>
      <div
        className={flush ? 'min-h-0 flex-1' : 'min-h-0 flex-1 overflow-y-auto p-5'}
        tabIndex={flush ? undefined : 0}
        aria-label={flush ? undefined : `${title} content`}
      >
        {children}
      </div>
    </div>
  );
}

function PanelLoading({ label, compact = false }: { label: string; compact?: boolean }) {
  return (
    <div
      className={
        compact
          ? 'p-4 text-sm text-slate-400'
          : 'flex h-full items-center justify-center text-sm text-slate-400'
      }
    >
      {label}...
    </div>
  );
}

function MetricTile({ label, value }: { label: string; value: string }) {
  return (
    <section className="min-h-24 rounded-lg border border-slate-800 bg-slate-900/45 p-4">
      <div className="text-xs font-medium uppercase text-slate-400">{label}</div>
      <div className="mt-3 truncate text-2xl font-semibold text-slate-100">{value}</div>
    </section>
  );
}

function ActionButton({
  label,
  detail,
  onClick,
}: {
  label: string;
  detail: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="rounded-lg border border-slate-800 bg-slate-950 px-3 py-3 text-left transition hover:border-slate-600 hover:bg-slate-900"
    >
      <span className="block text-sm font-medium text-slate-100">{label}</span>
      <span className="mt-1 block text-xs text-slate-400">{detail}</span>
    </button>
  );
}

function StatusRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-start justify-between gap-4">
      <dt className="shrink-0 text-slate-400">{label}</dt>
      <dd className="min-w-0 truncate font-mono text-xs text-slate-200">{value}</dd>
    </div>
  );
}

function SettingsRow({ label, value, detail }: { label: string; value: string; detail: string }) {
  return (
    <div className="grid gap-1 py-3 sm:grid-cols-[9rem_minmax(0,1fr)] sm:gap-4">
      <dt className="text-slate-400">{label}</dt>
      <dd className="min-w-0">
        <div className="truncate font-mono text-xs text-slate-100">{value}</div>
        <div className="mt-1 text-xs text-slate-400">{detail}</div>
      </dd>
    </div>
  );
}

function CadenceInput({
  label,
  value,
  inputMode = 'numeric',
  onChange,
}: {
  label: string;
  value: string;
  inputMode?: 'numeric' | 'decimal';
  onChange: (value: string) => void;
}) {
  return (
    <label className="text-xs font-medium uppercase text-slate-400">
      {label}
      <input
        value={value}
        onChange={(event) => onChange(event.target.value)}
        inputMode={inputMode}
        className="mt-1 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm font-normal normal-case text-slate-100 outline-none focus:border-sky-500"
      />
    </label>
  );
}

function settingsTransport(apiUrl: string): { label: string; detail: string } {
  const normalized = apiUrl.trim().replace(/\/$/, '');
  if (normalized === DEFAULT_SOLO_API_URL) {
    return { label: 'Solo HTTP', detail: 'installed desktop default' };
  }
  if (normalized === MCP_BRIDGE_URL) {
    return { label: 'Developer bridge', detail: 'local development fallback' };
  }
  return { label: 'Custom endpoint', detail: compactHost(apiUrl) };
}

function statusLabel(status: 'pending' | 'error' | 'success'): string {
  if (status === 'pending') return 'checking';
  if (status === 'error') return 'offline';
  return 'online';
}

function parseNonNegativeInteger(raw: string, label: string): number {
  const trimmed = raw.trim();
  if (!/^\d+$/.test(trimmed)) {
    throw new Error(`${label} must be a non-negative whole number.`);
  }
  const value = Number(trimmed);
  if (!Number.isSafeInteger(value)) {
    throw new Error(`${label} is too large.`);
  }
  return value;
}

function parsePositiveInteger(raw: string, label: string): number {
  const value = parseNonNegativeInteger(raw, label);
  if (value < 1) {
    throw new Error(`${label} must be at least 1.`);
  }
  return value;
}

function parseClusterThreshold(raw: string): number {
  const value = Number(raw.trim());
  if (!Number.isFinite(value) || value <= 0 || value > 1) {
    throw new Error('Similarity threshold must be a number greater than 0 and at most 1.');
  }
  return value;
}

function formatThreshold(value: number): string {
  if (!Number.isFinite(value)) return '0';
  return Number(value.toFixed(3)).toString();
}

function daemonFallbackState(status: 'pending' | 'error' | 'success'): string {
  if (status === 'pending') return 'Checking daemon';
  if (status === 'error') return 'Daemon unavailable';
  return 'Daemon running';
}

function daemonStateValue(status: 'pending' | 'error' | 'success', data?: SoloStatus): string {
  if (data?.ok) return 'Daemon running and unlocked';
  return daemonFallbackState(status);
}

function daemonNextAction(status: 'pending' | 'error' | 'success', data?: SoloStatus): string {
  if (data?.ok) return 'MCP clients can connect';
  if (status === 'pending') return 'Checking local Solo API';
  return 'Start Solo or enter passphrase from tray';
}

function homeInboxCount(status: 'pending' | 'error' | 'success', count?: number): string {
  if (typeof count === 'number') return String(count);
  if (status === 'pending') return 'checking';
  if (status === 'error') return 'unavailable';
  return '0';
}

function homeInboxDetail(status: 'pending' | 'error' | 'success', count?: number): string {
  if (typeof count === 'number') return `${count} item${count === 1 ? '' : 's'}`;
  if (status === 'pending') return 'checking inbox';
  if (status === 'error') return 'inbox unavailable';
  return '0 items';
}

function homeFactsTriplesStatus(
  status: 'pending' | 'error' | 'success',
  facts: FactHit[] | undefined,
  triples: number,
): string {
  const tripleLabel = `${triples} triple${triples === 1 ? '' : 's'}`;
  if (facts && facts.length > 0) {
    const fact = facts[0];
    return `${tripleLabel}, ${fact.predicate} -> ${compactEntityLabel(fact.object_id)}`;
  }
  if (status === 'pending') return `${tripleLabel}, facts checking`;
  if (status === 'error') return `${tripleLabel}, facts unavailable`;
  return triples > 0 ? tripleLabel : 'no facts or triples yet';
}

function compactEntityLabel(id: string): string {
  const label = id.startsWith('ent:') ? id.slice(4) : id;
  return label.length > 32 ? `${label.slice(0, 29)}...` : label;
}

function libraryReadiness(status?: SoloStatus): string {
  if (!status) return 'not checked';
  return status.library.ready ? 'ready' : 'not ready';
}

function embedderSummary(status?: SoloStatus): string {
  if (!status) return 'not reported';
  const { name, version, dim, dtype } = status.embedder;
  return `${name}@${version} ${dim}d ${dtype}`;
}

function embedderRuntimeStatus(status?: SoloStatus): string {
  const runtime = status?.embedder.runtime;
  if (!runtime) return 'not reported';
  if (runtime.running) return runtime.status || 'ready';
  const detail = runtime.detail ? `: ${runtime.detail}` : '';
  return `${runtime.status || 'offline'}${detail}`;
}

function embedderBackendLabel(status?: SoloStatus): string {
  if (!status) return 'not reported';
  const name = status.embedder.name;
  if (name.startsWith('bundled:')) return 'bundled local model';
  if (name.startsWith('ollama:')) return 'Ollama';
  if (name === 'stub') return 'stub/test vectors';
  return name;
}

function ollamaSwitchStatus(status?: SoloStatus): string {
  if (!status) return 'not reported';
  if (status.embedder.name.startsWith('ollama:')) return 'active';
  return 'supported by config switch';
}

function stewardConfiguredLlm(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  if (steward.provider && steward.model) return `${steward.provider}:${steward.model}`;
  if (steward.provider) return steward.provider;
  if (steward.config_mode === 'none') return 'disabled';
  return steward.config_mode;
}

function stewardLlmMode(status?: SoloStatus): StewardLlmMode {
  const steward = status?.steward;
  const mode = steward?.config_mode;
  if (mode === 'none' || mode === 'anthropic' || mode === 'openai' || mode === 'ollama') {
    return mode;
  }
  if (steward?.provider === 'anthropic') return 'anthropic';
  if (steward?.provider === 'openai') return 'openai';
  if (steward?.provider === 'ollama') return 'ollama';
  return 'ollama';
}

function stewardLlmModel(status: SoloStatus | undefined, mode: StewardLlmMode): string {
  const model = status?.steward?.model?.trim();
  return model || defaultLlmModel(mode);
}

function stewardLlmBaseUrl(status?: SoloStatus): string {
  const configured = status?.steward?.base_url?.trim();
  if (configured) return configured;
  return status?.steward?.endpoint === 'cloud' ? 'https://ollama.com' : 'http://localhost:11434';
}

function isLoopbackOllamaUrl(value: string): boolean {
  try {
    const host = new URL(value.trim()).hostname.replace(/^\[|\]$/g, '').toLowerCase();
    return host === 'localhost' || host === '::1' || /^127(?:\.\d{1,3}){3}$/.test(host);
  } catch {
    return false;
  }
}

function defaultLlmModel(mode: StewardLlmMode): string {
  if (mode === 'anthropic') return 'claude-sonnet-4-6';
  if (mode === 'openai') return 'gpt-5.6-terra';
  if (mode === 'ollama') return 'qwen3:8b';
  return '';
}

function defaultLlmApiKeyEnv(mode: StewardLlmMode): string {
  if (mode === 'anthropic') return 'ANTHROPIC_API_KEY';
  if (mode === 'openai') return 'OPENAI_API_KEY';
  return '';
}

function llmSettingsLabel(llm: {
  mode: string;
  provider: string | null;
  model: string | null;
  base_url: string | null;
}): string {
  if (llm.mode === 'none') return 'disabled';
  if (llm.provider && llm.model) return `${llm.provider}:${llm.model}`;
  if (llm.provider) return llm.provider;
  return llm.mode;
}

function stewardTripleCapability(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  if (steward.can_write_triples) return 'yes';
  if (!steward.automatic) return 'disabled';
  if (!steward.runtime_wired) return 'no Steward wired';
  if (!steward.runtime_has_llm) return 'LLM missing';
  return 'not ready';
}

function stewardRuntimeStatus(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  if (steward.status === 'ready') return 'ready';
  if (steward.status === 'disabled') return 'disabled';
  if (steward.status === 'not_wired') return 'not wired';
  if (steward.status === 'no_llm') return 'no LLM';
  if (steward.running ?? steward.runtime_wired) return steward.status || 'running';
  return steward.status || 'not running';
}

function stewardRuntimeLlm(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  if (steward.runtime_llm) return steward.runtime_llm;
  if (steward.provider && steward.model) return `${steward.provider}:${steward.model}`;
  if (steward.provider) return steward.provider;
  return steward.config_mode === 'none' ? 'none configured' : steward.config_mode;
}

function stewardRuntimeMatchesConfig(
  status?: SoloStatus,
  expected?: LlmSettingsSummary,
): boolean | null {
  const steward = status?.steward;
  if (!steward) return null;
  const mode = expected?.mode ?? steward.config_mode;
  const model = expected?.model ?? steward.model;
  if (mode === 'none') return !steward.runtime_has_llm;
  if (!model || !steward.runtime_llm) return false;
  if (mode === 'ollama') {
    return (
      steward.runtime_llm === `ollama:${model}` ||
      steward.runtime_llm === `ollama-local:${model}` ||
      steward.runtime_llm === `ollama-cloud:${model}` ||
      steward.runtime_llm === `ollama-remote:${model}` ||
      steward.runtime_llm === model
    );
  }
  if (mode === 'anthropic' || mode === 'openai') return steward.runtime_llm === model;
  return steward.runtime_has_llm;
}

function stewardRuntimeVerification(
  status?: SoloStatus,
  expected?: LlmSettingsSummary,
  restartRequested = false,
): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  const matches = stewardRuntimeMatchesConfig(status, expected);
  if (matches === true) return 'matches configured';
  if (restartRequested) return 'waiting for restart';
  if (!steward.runtime_wired) return 'no Steward runtime';
  return 'restart required';
}

function stewardAutomaticStatus(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  return steward.automatic ? 'enabled' : 'disabled';
}

function stewardTriggerStatus(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  const time = steward.trigger_interval_secs > 0 ? `${steward.trigger_interval_secs}s` : 'time off';
  const count =
    steward.trigger_episode_count > 0 ? `${steward.trigger_episode_count} episodes` : 'count off';
  return `${time} or ${count}`;
}

function stewardNextAutomaticRun(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  if (!steward.automatic) return 'automatic off';
  if (steward.next_triples_run_at_ms) return formatEpochMs(steward.next_triples_run_at_ms);
  if (steward.trigger_episode_count > 0) return `after ${steward.trigger_episode_count} episodes`;
  return 'time trigger off';
}

function stewardPendingClusters(status?: SoloStatus): string {
  const pending = status?.steward?.pending_clusters;
  if (pending === undefined) return 'not reported';
  return `${pending} cluster${pending === 1 ? '' : 's'}`;
}

function stewardLastTripleRun(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  if (!steward.last_triples_run_at_ms) return 'not run yet';
  const trigger = steward.last_triples_trigger ? ` (${steward.last_triples_trigger})` : '';
  return `${formatEpochMs(steward.last_triples_run_at_ms)}${trigger}`;
}

function stewardLastTripleIssue(status?: SoloStatus): string {
  const steward = status?.steward;
  if (!steward) return 'not reported';
  if (steward.last_triples_error) return steward.last_triples_error;
  if (steward.last_triples_timed_out) return 'timeout or deferred clusters';
  return 'none';
}

function stewardLastBatchCounts(status?: SoloStatus): string {
  const batch = status?.steward?.last_triples_batch;
  if (!batch) return 'no batch yet';
  const state = batch.ran ? 'ran' : 'skipped';
  const quarantined = batch.triples_quarantined ?? 0;
  return `${state}: ${batch.abstractions_built} abstractions, ${batch.triples_extracted} triples, ${quarantined} review, ${batch.clusters_failed} failed, ${batch.clusters_deferred} deferred`;
}

function soloConfigPath(status?: SoloStatus): string {
  const dataDir = status?.runtime?.data_dir?.replace(/[\\/]+$/, '');
  if (!dataDir) return 'not reported';
  const separator = dataDir.includes('\\') ? '\\' : '/';
  return `${dataDir}${separator}solo.config.toml`;
}

function ollamaEmbedderSwitchPlan(
  status?: SoloStatus,
  options: { model: string; dim: string; baseUrl: string } = {
    model: 'nomic-embed-text',
    dim: '768',
    baseUrl: 'http://localhost:11434',
  },
): string {
  const model = options.model.trim() || 'nomic-embed-text';
  const dim = options.dim.trim();
  const baseUrl = options.baseUrl.trim() || 'http://localhost:11434';
  const dataDir = status?.runtime?.data_dir?.trim();
  const command = [
    'solo',
    'migrate-embedder',
    'ollama',
    '--model',
    shellArg(model),
    '--base-url',
    shellArg(baseUrl),
    ...(dim ? ['--dim', shellArg(dim)] : []),
    ...(dataDir ? ['--data-dir', shellArg(dataDir)] : []),
  ].join(' ');
  return [
    `ollama pull ${shellArg(model)}`,
    '',
    '# Native one-click path: Solo Controls > Settings > Embedder Migration.',
    '# CLI equivalent; stop Solo first, then enter the passphrase when prompted:',
    command,
  ].join('\n');
}

function shellArg(value: string): string {
  if (/^[A-Za-z0-9_./:\\-]+$/.test(value)) return value;
  return `"${value.replace(/"/g, '`"')}"`;
}

function summarizeGraph(graph?: GraphResponse): {
  episodes: number;
  documents: number;
  chunks: number;
  clusters: number;
  entities: number;
  triples: number;
  semanticEdges: number;
} {
  const summary = {
    episodes: 0,
    documents: 0,
    chunks: 0,
    clusters: 0,
    entities: 0,
    triples: 0,
    semanticEdges: 0,
  };
  if (!graph) return summary;
  for (const node of graph.nodes) {
    if (node.kind === 'episode') summary.episodes += 1;
    if (node.kind === 'document') summary.documents += 1;
    if (node.kind === 'chunk') summary.chunks += 1;
    if (node.kind === 'cluster') summary.clusters += 1;
    if (node.kind === 'entity') summary.entities += 1;
  }
  for (const edge of graph.edges) {
    if (edge.kind === 'triple') summary.triples += 1;
    if (edge.kind === 'semantic') summary.semanticEdges += 1;
  }
  return summary;
}

function selectTopEntity(graph?: GraphResponse): { subject: string; refCount: number } | null {
  const entity = graph?.nodes
    .filter((node) => node.kind === 'entity')
    .sort((a, b) => (b.ref_count ?? 0) - (a.ref_count ?? 0))[0];
  if (!entity) return null;
  return {
    subject: entity.id.startsWith('ent:') ? entity.id.slice(4) : entity.label,
    refCount: entity.ref_count ?? 0,
  };
}

function factsStatus(
  status: 'pending' | 'error' | 'success',
  facts: FactHit[] | undefined,
): string {
  if (status === 'pending') return 'checking';
  if (status === 'error') return 'unavailable';
  if (!facts) return 'not checked';
  return `${facts.length} fact${facts.length === 1 ? '' : 's'}`;
}

function consolidationStatus(report?: ConsolidationReport): string {
  if (!report) return 'not run here';
  return `${report.clusters_built} clusters, ${report.abstractions_built} abstractions, ${report.triples_built} triples`;
}

function triplesExtractStatus(report?: TriplesExtractReport): string {
  if (!report) return 'not run here';
  if (!report.ran) return report.note;
  return `${report.abstractions_built} abstractions, ${report.triples_extracted} triples, ${report.triples_quarantined} review`;
}

function derivedRepairStatus(report?: DerivedRepairReport): string {
  if (!report) return 'not run here';
  if (report.dry_run) return `would repair ${report.clusters_repaired} clusters`;
  if (report.mode === 'rebuild_all') {
    return `rebuilt: ${report.clusters_deleted} clusters, ${report.cluster_memberships_deleted} memberships`;
  }
  return `repaired ${report.clusters_repaired} clusters, ${report.abstractions_deleted} abstractions`;
}

function formatEpochMs(value: number | null | undefined): string {
  if (value === null) return 'not recorded';
  if (value === undefined) return 'not reported';
  return new Date(value).toLocaleString([], {
    dateStyle: 'medium',
    timeStyle: 'short',
  });
}

function mcpProbeStatus(probe: {
  isPending: boolean;
  isError: boolean;
  data?: {
    missingRequiredTools: string[];
    readOnlyCall?: { status: 'passed' | 'skipped' };
    toolCount: number;
  };
}): string {
  if (probe.isPending) return 'checking';
  if (probe.isError) return 'failed';
  if (!probe.data) return 'not checked';
  if (probe.data.missingRequiredTools.length > 0) return 'missing tools';
  if (probe.data.readOnlyCall?.status === 'passed') return 'ready';
  if (probe.data.readOnlyCall?.status === 'skipped') return 'listed tools';
  return 'ready';
}

function mcpReadOnlyCallStatus(report?: {
  readOnlyCall?: { toolName: string; status: 'passed' | 'skipped'; detail: string };
}): string {
  if (!report?.readOnlyCall) return 'not checked';
  const { toolName, status, detail } = report.readOnlyCall;
  if (status === 'passed') return `${toolName} passed`;
  return `skipped: ${detail}`;
}

function compactHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

function modeFromHash(hash: string, host: SoloWebHost): AppMode {
  const value = hash.replace(/^#/, '').trim().toLowerCase();
  if (APP_MODES.includes(value as CoreRouteId)) return value as CoreRouteId;
  return host.routes.some((module) => module.id === value) ? value : 'home';
}
