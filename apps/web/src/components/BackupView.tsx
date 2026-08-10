import { useMutation, useQuery } from '@tanstack/react-query';
import { useEffect, useMemo, useState } from 'react';
import { errorMessage, runBackup } from '../api/client';
import { fetchSoloStatus } from '../api/health';
import { configPath, libraryDbPath, suggestedBackupPath } from '../lib/backupPaths';
import { COMMUNITY_LIBRARY_NAME } from '../store/graphStore';
import { useSettingsStore } from '../store/settingsStore';
import { CopyButton } from './ui/CopyButton';

export function BackupView() {
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const status = useQuery({
    queryKey: ['desktop-backups', 'solo-status', apiUrl, connectionRevision],
    queryFn: ({ signal }) => fetchSoloStatus({ signal }),
    retry: false,
  });
  const libraryName = status.data?.library.name ?? COMMUNITY_LIBRARY_NAME;
  const dataDir = status.data?.runtime?.data_dir ?? '';
  const suggestedDestination = useMemo(() => suggestedBackupPath(dataDir), [dataDir]);
  const [destination, setDestination] = useState('');
  const [destinationEdited, setDestinationEdited] = useState(false);
  const [force, setForce] = useState(false);

  useEffect(() => {
    if (!destinationEdited && suggestedDestination) {
      setDestination(suggestedDestination);
    }
  }, [destinationEdited, suggestedDestination]);

  const backup = useMutation({
    mutationFn: () =>
      runBackup(
        {
          to: destination.trim(),
          force,
        },
        {},
      ),
  });
  const trimmedDestination = destination.trim();
  const httpBody = JSON.stringify({ to: trimmedDestination, force }, null, 2);
  const backupUrl = `${apiUrl.replace(/\/$/, '')}/backup`;
  const canRun = status.data?.ok === true && trimmedDestination.length > 0 && !backup.isPending;

  return (
    <div className="space-y-5">
      <div className="grid gap-3 lg:grid-cols-4">
        <MetricTile
          label="Daemon"
          value={status.data?.ok ? 'running' : statusLabel(status.status)}
        />
        <MetricTile label="Memory library" value={libraryName} />
        <MetricTile label="Backup mode" value="hot" />
        <MetricTile label="Last action" value={backupStatusLabel(backup.status)} />
      </div>

      <div className="grid gap-4 xl:grid-cols-[minmax(0,1.15fr)_minmax(20rem,0.85fr)]">
        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <div className="flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between">
            <div>
              <h2 className="text-sm font-semibold text-slate-100">Hot Backup</h2>
              <div className="mt-1 font-mono text-xs text-slate-400">{libraryName}</div>
            </div>
            <button
              type="button"
              onClick={() => backup.mutate()}
              disabled={!canRun}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {backup.isPending ? 'Backing up' : 'Run backup'}
            </button>
          </div>

          <label className="mt-5 block text-xs font-medium uppercase text-slate-400">
            Destination
          </label>
          <input
            aria-label="Backup destination"
            value={destination}
            onChange={(event) => {
              setDestinationEdited(true);
              setDestination(event.target.value);
            }}
            spellCheck={false}
            className="mt-2 w-full rounded-md border border-slate-800 bg-slate-950 px-3 py-2 font-mono text-xs text-slate-100 outline-none focus:border-sky-500"
            placeholder={dataDir ? suggestedDestination : 'Start Solo to detect the data directory'}
          />

          <label className="mt-4 flex items-center gap-2 text-sm text-slate-300">
            <input
              type="checkbox"
              checked={force}
              onChange={(event) => setForce(event.target.checked)}
              className="h-4 w-4 rounded border-slate-700 bg-slate-950 text-sky-500"
            />
            Overwrite existing target
          </label>

          {status.isError && (
            <p className="mt-4 rounded-md border border-amber-800/70 bg-amber-950/30 px-3 py-2 text-xs text-amber-100">
              {errorMessage(status.error)}
            </p>
          )}
          {backup.isError && (
            <p className="mt-4 rounded-md border border-red-800/70 bg-red-950/30 px-3 py-2 text-xs text-red-200">
              {errorMessage(backup.error)}
            </p>
          )}
          {backup.data && (
            <dl className="mt-4 space-y-3 rounded-md border border-emerald-900/60 bg-emerald-950/20 p-3 text-sm">
              <StatusRow label="Path" value={backup.data.path} />
              <StatusRow label="Elapsed" value={`${backup.data.elapsed_ms}ms`} />
            </dl>
          )}

          <div className="mt-5 flex flex-wrap gap-2">
            <CopyButton label="Copy backup URL" value={backupUrl} />
            {trimmedDestination && (
              <CopyButton label="Copy destination" value={trimmedDestination} />
            )}
            {trimmedDestination && <CopyButton label="Copy HTTP body" value={httpBody} />}
          </div>
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <h2 className="text-sm font-semibold text-slate-100">Recovery Surface</h2>
          <dl className="mt-4 space-y-3 text-sm">
            <StatusRow label="Solo API" value={apiUrl} />
            <StatusRow label="Data dir" value={dataDir || 'not reported'} />
            <StatusRow label="Library DB" value={libraryDbPath(dataDir)} />
            <StatusRow label="Config" value={configPath(dataDir)} />
            <StatusRow label="Restore command" value="solo restore --from <backup> --confirm" />
          </dl>
        </section>
      </div>
    </div>
  );
}

function statusLabel(status: 'pending' | 'error' | 'success'): string {
  if (status === 'pending') return 'checking';
  if (status === 'error') return 'offline';
  return 'online';
}

function backupStatusLabel(status: 'idle' | 'pending' | 'error' | 'success'): string {
  if (status === 'pending') return 'running';
  if (status === 'error') return 'failed';
  if (status === 'success') return 'complete';
  return 'none';
}

function MetricTile({ label, value }: { label: string; value: string }) {
  return (
    <section className="min-h-24 rounded-lg border border-slate-800 bg-slate-900/45 p-4">
      <div className="text-xs font-medium uppercase text-slate-400">{label}</div>
      <div className="mt-3 truncate text-2xl font-semibold text-slate-100">{value}</div>
    </section>
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
