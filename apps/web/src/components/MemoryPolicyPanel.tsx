import { useMemo, useState } from 'react';
import {
  MEMORY_POLICY_TARGETS,
  renderMemoryPolicy,
  type MemoryPolicyTarget,
} from '../lib/memoryPolicies';
import { CopyButton } from './ui/CopyButton';

export function MemoryPolicyPanel({
  libraryName,
  mcpUrl,
}: {
  libraryName: string;
  mcpUrl: string;
}) {
  const [target, setTarget] = useState<MemoryPolicyTarget>('codex');
  const policy = useMemo(
    () => renderMemoryPolicy({ target, libraryName, mcpUrl }),
    [target, libraryName, mcpUrl],
  );
  const selected = MEMORY_POLICY_TARGETS.find((item) => item.value === target);

  return (
    <section className="mt-4 rounded-lg border border-slate-800 bg-slate-900/45 p-4">
      <div className="flex flex-col gap-3 md:flex-row md:items-end md:justify-between">
        <div>
          <h2 className="text-sm font-semibold text-slate-100">Memory Policy</h2>
          <select
            aria-label="Policy target"
            value={target}
            onChange={(event) => setTarget(event.target.value as MemoryPolicyTarget)}
            className="mt-2 rounded-md border border-slate-700 bg-slate-950 px-2 py-1 text-sm text-slate-100 outline-none focus:border-sky-600"
          >
            {MEMORY_POLICY_TARGETS.map((item) => (
              <option key={item.value} value={item.value}>
                {item.label}
              </option>
            ))}
          </select>
        </div>
        <CopyButton label="Copy policy" value={policy} />
      </div>

      <div className="mt-4 grid gap-4 lg:grid-cols-[16rem_minmax(0,1fr)]">
        <dl className="space-y-3 text-sm">
          <PolicyDatum label="Target" value={selected?.label ?? target} />
          <PolicyDatum label="Memory library" value={libraryName} />
          <PolicyDatum label="Endpoint" value={mcpUrl} />
        </dl>
        <pre
          className="max-h-80 overflow-auto whitespace-pre-wrap rounded-md border border-slate-800 bg-slate-950 p-3 text-xs leading-5 text-slate-200"
          tabIndex={0}
        >
          {policy}
        </pre>
      </div>
    </section>
  );
}

function PolicyDatum({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-start justify-between gap-4">
      <dt className="shrink-0 text-slate-400">{label}</dt>
      <dd className="min-w-0 truncate font-mono text-xs text-slate-200">{value}</dd>
    </div>
  );
}
