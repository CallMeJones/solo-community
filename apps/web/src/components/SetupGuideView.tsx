import { useQuery } from '@tanstack/react-query';
import { errorMessage, fetchInbox } from '../api/client';
import { fetchSoloStatus } from '../api/health';
import { useGraphData } from '../hooks/useGraphData';
import { COMMUNITY_LIBRARY_NAME } from '../store/graphStore';
import { useSettingsStore } from '../store/settingsStore';

type SetupMode = 'settings' | 'import' | 'inbox';

type StepState = 'done' | 'ready' | 'blocked' | 'checking';

interface SetupStep {
  id: string;
  label: string;
  detail: string;
  value: string;
  state: StepState;
  action: string;
  mode: SetupMode;
}

export function SetupGuideView({ onModeChange }: { onModeChange: (mode: SetupMode) => void }) {
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const solo = useQuery({
    queryKey: ['desktop-setup', 'solo-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
  });
  const inbox = useQuery({
    queryKey: ['desktop-setup', 'inbox', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchInbox(100, { signal }),
    retry: false,
  });
  const graph = useGraphData();
  const graphNodes = graph.data?.nodes ?? [];
  const documentCount = graphNodes.filter((node) => node.kind === 'document').length;
  const memoryCount = graphNodes.filter((node) => node.kind === 'episode').length;
  const reviewedCount = inbox.data?.filter((item) => item.review_state).length ?? 0;
  const inboxCount = inbox.data?.length ?? 0;
  const daemonRunning = solo.data?.ok === true;
  const libraryReady = solo.data?.library.ready === true;
  const libraryName = solo.data?.library.name ?? COMMUNITY_LIBRARY_NAME;
  const libraryState: StepState = libraryReady
    ? 'done'
    : solo.status === 'pending'
      ? 'checking'
      : daemonRunning
        ? 'ready'
        : 'blocked';

  const steps: SetupStep[] = [
    {
      id: 'daemon',
      label: 'Start Solo',
      detail: 'daemon and unlock state',
      value: daemonRunning ? 'running' : statusLabel(solo.status),
      state: daemonRunning ? 'done' : solo.status === 'pending' ? 'checking' : 'blocked',
      action: daemonRunning ? 'View diagnostics' : 'Open settings',
      mode: 'settings',
    },
    {
      id: 'library',
      label: 'Memory library',
      detail: 'one private local library',
      value: libraryName,
      state: libraryState,
      action: 'View settings',
      mode: 'settings',
    },
    {
      id: 'codex',
      label: 'Connect Codex',
      detail: 'native HTTP MCP config',
      value: solo.data?.mcp.sessions ? `${solo.data.mcp.sessions} sessions` : 'not connected',
      state: solo.data?.mcp.sessions ? 'done' : daemonRunning ? 'ready' : 'blocked',
      action: 'Open connections',
      mode: 'settings',
    },
    {
      id: 'claude',
      label: 'Connect Claude',
      detail: 'Claude Desktop MCP config',
      value: solo.data?.mcp.sessions ? `${solo.data.mcp.sessions} sessions` : 'not connected',
      state: solo.data?.mcp.sessions ? 'done' : daemonRunning ? 'ready' : 'blocked',
      action: 'Open connections',
      mode: 'settings',
    },
    {
      id: 'import',
      label: 'Import memory',
      detail: `${memoryCount} memories, ${documentCount} documents`,
      value: documentCount > 0 ? `${documentCount} docs` : 'not started',
      state: documentCount > 0 ? 'done' : daemonRunning ? 'ready' : 'blocked',
      action: 'Import data',
      mode: 'import',
    },
    {
      id: 'inbox',
      label: 'Review inbox',
      detail: `${reviewedCount} reviewed of ${inboxCount}`,
      value: inboxStatusValue(inbox.status, inboxCount, reviewedCount),
      state:
        reviewedCount > 0 ? 'done' : inboxCount > 0 ? 'ready' : daemonRunning ? 'ready' : 'blocked',
      action: 'Open inbox',
      mode: 'inbox',
    },
    {
      id: 'backup',
      label: 'Create backup',
      detail: solo.data?.runtime?.data_dir ?? 'data dir pending',
      value: daemonRunning ? 'ready' : statusLabel(solo.status),
      state: daemonRunning ? 'ready' : solo.status === 'pending' ? 'checking' : 'blocked',
      action: 'Backup settings',
      mode: 'settings',
    },
  ];
  const doneCount = steps.filter((step) => step.state === 'done').length;

  return (
    <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_22rem]">
      <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
        <div className="flex flex-col gap-2 sm:flex-row sm:items-start sm:justify-between">
          <div>
            <h2 className="text-sm font-semibold text-slate-100">First Run</h2>
            <div className="mt-1 text-xs text-slate-400">
              {doneCount} of {steps.length} complete
            </div>
          </div>
          <div className="rounded-md border border-slate-800 bg-slate-950 px-3 py-2 font-mono text-xs text-slate-300">
            {libraryName}
          </div>
        </div>

        <div className="mt-5 divide-y divide-slate-800">
          {steps.map((step) => (
            <button
              key={step.id}
              type="button"
              onClick={() => onModeChange(step.mode)}
              className="flex w-full flex-col gap-3 py-4 text-left first:pt-0 last:pb-0 sm:flex-row sm:items-center sm:justify-between"
            >
              <span className="flex min-w-0 items-start gap-3">
                <span
                  className={['mt-1 h-2.5 w-2.5 shrink-0 rounded-full', stateDot(step.state)].join(
                    ' ',
                  )}
                />
                <span className="min-w-0">
                  <span className="block text-sm font-medium text-slate-100">{step.label}</span>
                  <span className="mt-1 block truncate text-xs text-slate-400">{step.detail}</span>
                </span>
              </span>
              <span className="flex shrink-0 items-center gap-3 sm:justify-end">
                <span className="font-mono text-xs text-slate-300">{step.value}</span>
                <span className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200">
                  {step.action}
                </span>
              </span>
            </button>
          ))}
        </div>
      </section>

      <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
        <h2 className="text-sm font-semibold text-slate-100">Readiness</h2>
        <dl className="mt-4 space-y-3 text-sm">
          <StatusRow label="Daemon" value={daemonRunning ? 'running' : statusLabel(solo.status)} />
          <StatusRow label="Memory library" value={libraryName} />
          <StatusRow label="Clients" value={String(solo.data?.mcp.sessions ?? 0)} />
          <StatusRow label="Documents" value={String(documentCount)} />
          <StatusRow
            label="Inbox"
            value={inboxStatusValue(inbox.status, inboxCount, reviewedCount)}
          />
        </dl>
        {(solo.isError || inbox.isError || graph.isError) && (
          <p className="mt-4 rounded-md border border-amber-800/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-100">
            {setupErrorSummary([
              solo.isError ? solo.error : null,
              inbox.isError ? inbox.error : null,
              graph.isError ? graph.error : null,
            ])}
          </p>
        )}
      </section>
    </div>
  );
}

function setupErrorSummary(errors: unknown[]): string {
  const first = errors.find(Boolean);
  return first ? errorMessage(first) : 'Setup status is not fully available.';
}

function statusLabel(status: 'pending' | 'error' | 'success'): string {
  if (status === 'pending') return 'checking';
  if (status === 'error') return 'offline';
  return 'online';
}

function inboxStatusValue(
  status: 'pending' | 'error' | 'success',
  inboxCount: number,
  reviewedCount: number,
): string {
  if (status === 'pending') return 'checking';
  if (status === 'error') return 'offline';
  if (inboxCount === 0) return 'empty';
  return reviewedCount > 0 ? `${reviewedCount} reviewed` : `${inboxCount} pending`;
}

function stateDot(state: StepState): string {
  if (state === 'done') return 'bg-emerald-400';
  if (state === 'ready') return 'bg-sky-400';
  if (state === 'checking') return 'bg-amber-400';
  return 'bg-rose-400';
}

function StatusRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-start justify-between gap-4">
      <dt className="shrink-0 text-slate-400">{label}</dt>
      <dd className="min-w-0 truncate font-mono text-xs text-slate-200">{value}</dd>
    </div>
  );
}
