import { useMutation } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import {
  addProjectDecision,
  fetchProjectFacts,
  renderProjectPolicy,
  searchProjectDecisions,
} from '../api/client';
import type { ProjectDescriptor, ProjectPolicyClient } from '../api/types';
import { COMMUNITY_LIBRARY_NAME } from '../store/graphStore';
import { CopyButton } from './ui/CopyButton';

const PROJECT_STORAGE_KEY = 'solo.desktop.project';
const POLICY_CLIENTS: Array<{ value: ProjectPolicyClient; label: string }> = [
  { value: 'codex', label: 'Codex' },
  { value: 'claude', label: 'Claude' },
  { value: 'cursor', label: 'Cursor' },
  { value: 'generic', label: 'Generic' },
];

export function ProjectsView() {
  const [project, setProject] = useState<ProjectDescriptor>(() => loadStoredProject());
  const [policyClient, setPolicyClient] = useState<ProjectPolicyClient>('codex');
  const [factSubject, setFactSubject] = useState('');
  const [decision, setDecision] = useState('');
  const [decisionQuery, setDecisionQuery] = useState('');

  useEffect(() => {
    localStorage.setItem(PROJECT_STORAGE_KEY, JSON.stringify(project));
  }, [project]);

  const policy = useMutation({
    mutationFn: () => renderProjectPolicy(project, policyClient, {}),
  });
  const facts = useMutation({
    mutationFn: () => fetchProjectFacts(project, { subject: factSubject, limit: 20 }, {}),
  });
  const addDecision = useMutation({
    mutationFn: () => addProjectDecision(project, decision, {}),
    onSuccess: () => setDecision(''),
  });
  const searchDecisions = useMutation({
    mutationFn: () => searchProjectDecisions(project, decisionQuery, { limit: 10 }),
  });

  const ready = projectReady(project);

  return (
    <div className="grid gap-4 xl:grid-cols-[minmax(320px,0.8fr)_minmax(0,1.2fr)]">
      <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
        <div className="flex items-start justify-between gap-3">
          <div>
            <h2 className="text-sm font-semibold text-slate-100">Project</h2>
            <p className="mt-1 text-xs text-slate-400">Memory library: {COMMUNITY_LIBRARY_NAME}</p>
          </div>
          <StatusPill ready={ready} />
        </div>
        <div className="mt-4 space-y-3">
          <ProjectInput
            label="Name"
            value={project.name}
            onChange={(name) => setProject((current) => ({ ...current, name }))}
          />
          <ProjectInput
            label="ID"
            value={project.id}
            onChange={(id) => setProject((current) => ({ ...current, id }))}
          />
          <ProjectInput
            label="Root"
            value={project.root}
            mono
            onChange={(root) => setProject((current) => ({ ...current, root }))}
          />
          <ProjectInput
            label="Tags"
            value={project.tags.join(', ')}
            onChange={(tags) => setProject((current) => ({ ...current, tags: splitTags(tags) }))}
          />
        </div>
        <dl className="mt-5 space-y-3 text-sm">
          <ProjectDatum label="Policy client" value={policyClient} />
          <ProjectDatum label="Facts subject" value={factSubject.trim() || project.name || '-'} />
          <ProjectDatum label="Decision query" value={decisionQuery.trim() || '-'} />
        </dl>
      </section>

      <div className="grid gap-4">
        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <div className="flex flex-wrap items-end justify-between gap-3">
            <div>
              <h2 className="text-sm font-semibold text-slate-100">Agent Policy</h2>
              <select
                value={policyClient}
                onChange={(event) => setPolicyClient(event.target.value as ProjectPolicyClient)}
                className="mt-2 rounded-md border border-slate-700 bg-slate-950 px-2 py-1 text-sm text-slate-100 outline-none focus:border-sky-600"
                aria-label="Policy client"
              >
                {POLICY_CLIENTS.map((client) => (
                  <option key={client.value} value={client.value}>
                    {client.label}
                  </option>
                ))}
              </select>
            </div>
            <div className="flex flex-wrap gap-2">
              <button
                type="button"
                onClick={() => policy.mutate()}
                disabled={!ready || policy.isPending}
                className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
              >
                {policy.isPending ? 'Rendering...' : 'Render policy'}
              </button>
              {policy.data && <CopyButton label="Copy policy" value={policy.data.policy} />}
            </div>
          </div>
          <MutationError error={policy.error} />
          {policy.data && <PreBlock text={policy.data.policy} />}
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <div className="grid gap-3 md:grid-cols-[minmax(0,1fr)_auto]">
            <ProjectInput
              label="Facts"
              value={factSubject}
              onChange={setFactSubject}
              placeholder={project.name || 'Subject'}
            />
            <button
              type="button"
              onClick={() => facts.mutate()}
              disabled={!ready || facts.isPending}
              className="self-end rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {facts.isPending ? 'Loading...' : 'Load facts'}
            </button>
          </div>
          <MutationError error={facts.error} />
          {facts.data && (
            <div className="mt-4 space-y-2">
              {facts.data.facts.length === 0 ? (
                <EmptyState>No facts found.</EmptyState>
              ) : (
                facts.data.facts.map((fact) => (
                  <div
                    key={fact.triple_id}
                    className="rounded-md border border-slate-800 bg-slate-950/60 px-3 py-2"
                  >
                    <div className="truncate text-sm text-slate-100">
                      {fact.subject_id} <span className="text-slate-400">{fact.predicate}</span>{' '}
                      {fact.object_id}
                    </div>
                    <div className="mt-1 font-mono text-[11px] text-slate-400">
                      {fact.object_kind} - confidence {fact.confidence.toFixed(2)}
                    </div>
                  </div>
                ))
              )}
            </div>
          )}
        </section>

        <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
          <div className="grid gap-3 lg:grid-cols-2">
            <div>
              <label
                htmlFor="project-decision"
                className="text-xs font-medium uppercase text-slate-400"
              >
                Add Decision
              </label>
              <textarea
                id="project-decision"
                value={decision}
                onChange={(event) => setDecision(event.target.value)}
                className="mt-2 min-h-24 w-full resize-y rounded-md border border-slate-800 bg-slate-950 p-2 text-sm text-slate-100 outline-none focus:border-sky-600"
              />
              <button
                type="button"
                onClick={() => addDecision.mutate()}
                disabled={!ready || addDecision.isPending || decision.trim().length === 0}
                className="mt-2 rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
              >
                {addDecision.isPending ? 'Saving...' : 'Save decision'}
              </button>
              <MutationError error={addDecision.error} />
              {addDecision.data && (
                <p className="mt-2 truncate font-mono text-xs text-emerald-300">
                  {addDecision.data.memory_id}
                </p>
              )}
            </div>
            <div>
              <ProjectInput
                label="Search Decisions"
                value={decisionQuery}
                onChange={setDecisionQuery}
              />
              <button
                type="button"
                onClick={() => searchDecisions.mutate()}
                disabled={!ready || searchDecisions.isPending || decisionQuery.trim().length === 0}
                className="mt-2 rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-50"
              >
                {searchDecisions.isPending ? 'Searching...' : 'Search'}
              </button>
              <MutationError error={searchDecisions.error} />
              {searchDecisions.data && (
                <div className="mt-3 space-y-2">
                  {searchDecisions.data.hits.length === 0 ? (
                    <EmptyState>No decisions found.</EmptyState>
                  ) : (
                    searchDecisions.data.hits.map((hit) => (
                      <div
                        key={hit.memory_id}
                        className="rounded-md border border-slate-800 bg-slate-950/60 px-3 py-2"
                      >
                        <p className="line-clamp-3 text-sm leading-5 text-slate-100">
                          {hit.content}
                        </p>
                        <p className="mt-1 font-mono text-[11px] text-slate-400">
                          {hit.memory_id} - {hit.tier}
                        </p>
                      </div>
                    ))
                  )}
                </div>
              )}
            </div>
          </div>
        </section>
      </div>
    </div>
  );
}

function ProjectInput({
  label,
  value,
  onChange,
  mono = false,
  placeholder,
}: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  mono?: boolean;
  placeholder?: string;
}) {
  return (
    <label className="block">
      <span className="text-xs font-medium uppercase text-slate-400">{label}</span>
      <input
        value={value}
        placeholder={placeholder}
        onChange={(event) => onChange(event.target.value)}
        className={[
          'mt-1 w-full rounded-md border border-slate-800 bg-slate-950 px-2 py-2 text-sm text-slate-100 outline-none focus:border-sky-600',
          mono ? 'font-mono' : '',
        ].join(' ')}
      />
    </label>
  );
}

function ProjectDatum({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-start justify-between gap-4">
      <dt className="shrink-0 text-slate-400">{label}</dt>
      <dd className="min-w-0 truncate font-mono text-xs text-slate-200">{value}</dd>
    </div>
  );
}

function StatusPill({ ready }: { ready: boolean }) {
  return (
    <span
      className={[
        'rounded-full px-2 py-0.5 text-[10px] font-medium uppercase',
        ready ? 'bg-emerald-500/10 text-emerald-300' : 'bg-amber-500/10 text-amber-300',
      ].join(' ')}
    >
      {ready ? 'ready' : 'incomplete'}
    </span>
  );
}

function MutationError({ error }: { error: unknown }) {
  if (!error) return null;
  const message = error instanceof Error ? error.message : String(error);
  return (
    <p
      role="alert"
      className="mt-3 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-xs text-red-300"
    >
      {message}
    </p>
  );
}

function EmptyState({ children }: { children: string }) {
  return (
    <p className="rounded-md border border-slate-800 bg-slate-950/60 px-3 py-4 text-center text-xs text-slate-400">
      {children}
    </p>
  );
}

function PreBlock({ text }: { text: string }) {
  return (
    <pre
      className="mt-4 max-h-72 overflow-auto whitespace-pre-wrap rounded-md border border-slate-800 bg-slate-950 p-3 text-xs leading-5 text-slate-200"
      tabIndex={0}
    >
      {text}
    </pre>
  );
}

function loadStoredProject(): ProjectDescriptor {
  const fallback: ProjectDescriptor = { name: '', id: '', root: '', tags: [] };
  try {
    const parsed = JSON.parse(
      localStorage.getItem(PROJECT_STORAGE_KEY) ?? 'null',
    ) as Partial<ProjectDescriptor> | null;
    if (!parsed || typeof parsed !== 'object') return fallback;
    return {
      name: typeof parsed.name === 'string' ? parsed.name : '',
      id: typeof parsed.id === 'string' ? parsed.id : '',
      root: typeof parsed.root === 'string' ? parsed.root : '',
      tags: Array.isArray(parsed.tags)
        ? parsed.tags.filter((tag): tag is string => typeof tag === 'string')
        : [],
    };
  } catch {
    return fallback;
  }
}

function splitTags(value: string): string[] {
  return value
    .split(',')
    .map((tag) => tag.trim())
    .filter(Boolean);
}

function projectReady(project: ProjectDescriptor): boolean {
  return Boolean(project.name.trim() && project.id.trim() && project.root.trim());
}
