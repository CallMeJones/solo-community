import { useMemo, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { errorMessage, fetchLogs } from '../api/client';
import type { LogLevel, LogsResponse } from '../api/types';
import { DEFAULT_SOLO_API_URL } from '../config/defaults';
import { formatBytes } from '../lib/formatBytes';
import { useSettingsStore } from '../store/settingsStore';
import { CopyButton } from './ui/CopyButton';

const LOG_LIMITS = [100, 200, 500] as const;

export function LogsView() {
  const [limit, setLimit] = useState<(typeof LOG_LIMITS)[number]>(200);
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const logs = useQuery({
    queryKey: ['desktop-logs', apiUrl, connectionRevision, limit],
    queryFn: ({ signal }) => fetchLogs(limit, { signal }),
    retry: false,
  });
  const copyValue = useMemo(() => formatLogCopy(logs.data), [logs.data]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="shrink-0 border-b border-slate-800 bg-slate-950 px-5 py-4">
        <div className="text-xs font-medium uppercase text-slate-400">Diagnostics</div>
        <div className="mt-1 flex flex-col gap-3 sm:flex-row sm:items-end sm:justify-between">
          <div>
            <h1 className="text-xl font-semibold text-slate-100">Logs</h1>
            <p className="mt-1 text-xs text-slate-400">
              Sanitized tray log tail from {compactHost(apiUrl || DEFAULT_SOLO_API_URL)}
            </p>
          </div>
          <div className="flex flex-wrap gap-2">
            <label className="flex items-center gap-2 rounded-md border border-slate-800 bg-slate-950 px-3 py-2 text-sm text-slate-300">
              <span className="text-xs text-slate-400">Lines</span>
              <select
                value={limit}
                onChange={(event) =>
                  setLimit(Number(event.target.value) as (typeof LOG_LIMITS)[number])
                }
                className="bg-transparent text-sm text-slate-100 outline-none"
              >
                {LOG_LIMITS.map((value) => (
                  <option key={value} value={value} className="bg-slate-950 text-slate-100">
                    {value}
                  </option>
                ))}
              </select>
            </label>
            <button
              type="button"
              onClick={() => void logs.refetch()}
              disabled={logs.isFetching}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {logs.isFetching ? 'Refreshing' : 'Refresh'}
            </button>
            <CopyButton label="Copy logs" value={copyValue} />
            {logs.data?.path && <CopyButton label="Copy path" value={logs.data.path} />}
          </div>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto p-5" tabIndex={0} aria-label="Log diagnostics">
        {logs.isError ? (
          <LogError error={logs.error} />
        ) : (
          <>
            <LogSummary data={logs.data} status={logs.status} />
            <LogBody data={logs.data} isLoading={logs.isPending} />
          </>
        )}
      </div>
    </div>
  );
}

function LogSummary({
  data,
  status,
}: {
  data?: LogsResponse;
  status: 'pending' | 'error' | 'success';
}) {
  const values = [
    { label: 'Source', value: data?.source ?? 'tray' },
    {
      label: 'File',
      value: data
        ? data.exists
          ? 'available'
          : 'not created'
        : status === 'pending'
          ? 'checking'
          : 'unknown',
    },
    {
      label: 'Lines',
      value: data
        ? `${data.lines.length} / ${data.limit}`
        : status === 'pending'
          ? 'checking'
          : 'unknown',
    },
    { label: 'Modified', value: formatEpochMs(data?.modified_at_ms) },
  ];

  return (
    <div className="grid gap-3 lg:grid-cols-4">
      {values.map((item) => (
        <section
          key={item.label}
          className="min-h-24 rounded-lg border border-slate-800 bg-slate-900/45 p-4"
        >
          <div className="text-xs font-medium uppercase text-slate-400">{item.label}</div>
          <div className="mt-3 truncate text-lg font-semibold text-slate-100">{item.value}</div>
        </section>
      ))}
    </div>
  );
}

function LogBody({ data, isLoading }: { data?: LogsResponse; isLoading: boolean }) {
  if (isLoading) {
    return (
      <section className="mt-5 rounded-lg border border-slate-800 bg-slate-900/45 p-5 text-sm text-slate-400">
        Loading tray log...
      </section>
    );
  }

  if (!data) return null;

  if (!data.exists) {
    return (
      <section className="mt-5 rounded-lg border border-amber-800/70 bg-amber-950/25 p-5">
        <h2 className="text-sm font-semibold text-amber-100">tray.log has not been created yet.</h2>
        <p className="mt-2 text-sm text-amber-100/80">
          Start or restart Solo from the tray, then refresh this page.
        </p>
        <div className="mt-4 truncate rounded-md border border-amber-800/50 bg-slate-950 px-3 py-2 font-mono text-xs text-amber-100/80">
          {data.path}
        </div>
      </section>
    );
  }

  if (data.lines.length === 0) {
    return (
      <section className="mt-5 rounded-lg border border-slate-800 bg-slate-900/45 p-5">
        <h2 className="text-sm font-semibold text-slate-100">No log lines in this tail.</h2>
        <p className="mt-2 text-sm text-slate-400">
          The log file exists, but the selected line limit returned an empty tail.
        </p>
      </section>
    );
  }

  return (
    <section className="mt-5 overflow-hidden rounded-lg border border-slate-800 bg-slate-950">
      <div className="flex flex-col gap-1 border-b border-slate-800 px-4 py-3 sm:flex-row sm:items-center sm:justify-between">
        <h2 className="text-sm font-semibold text-slate-100">Tray log</h2>
        <div className="truncate font-mono text-xs text-slate-400">
          {formatBytes(data.size_bytes, {
            nullValue: 'unknown size',
            undefinedValue: 'checking size',
          })}{' '}
          - {data.path}
        </div>
      </div>
      <div
        className="max-h-[calc(100vh-22rem)] min-h-80 overflow-auto"
        tabIndex={0}
        aria-label="Tray log entries"
      >
        <ol className="divide-y divide-slate-900">
          {data.lines.map((line, index) => (
            <li key={`${index}-${line.text}`} className="grid grid-cols-[4.5rem_minmax(0,1fr)]">
              <div className="border-r border-slate-900 px-3 py-2">
                <span
                  className={`rounded px-2 py-1 text-[11px] font-semibold ${levelClass(line.level)}`}
                >
                  {line.level}
                </span>
              </div>
              <pre className="whitespace-pre-wrap break-words px-3 py-2 font-mono text-xs leading-5 text-slate-300">
                {line.text}
              </pre>
            </li>
          ))}
        </ol>
      </div>
    </section>
  );
}

function LogError({ error }: { error: unknown }) {
  return (
    <section className="rounded-lg border border-red-800/70 bg-red-950/25 p-5">
      <h2 className="text-sm font-semibold text-red-100">Logs are unavailable.</h2>
      <p className="mt-2 text-sm text-red-100/80">{errorMessage(error)}</p>
      <p className="mt-3 text-xs text-red-100/70">
        Start or unlock Solo from the tray, then refresh Logs.
      </p>
    </section>
  );
}

function formatLogCopy(data?: LogsResponse): string {
  if (!data) return '';
  return data.lines.map((line) => `[${line.level}] ${line.text}`).join('\n');
}

function levelClass(level: LogLevel): string {
  switch (level) {
    case 'error':
      return 'bg-red-950 text-red-200';
    case 'warn':
      return 'bg-amber-950 text-amber-200';
    case 'debug':
      return 'bg-slate-800 text-slate-300';
    case 'trace':
      return 'bg-slate-900 text-slate-400';
    case 'info':
      return 'bg-sky-950 text-sky-200';
  }
}

function compactHost(url: string): string {
  try {
    return new URL(url).host;
  } catch {
    return url;
  }
}

function formatEpochMs(value: number | null | undefined): string {
  if (value === null) return 'not recorded';
  if (value === undefined) return 'checking';
  return new Date(value).toLocaleString([], {
    dateStyle: 'medium',
    timeStyle: 'short',
  });
}
