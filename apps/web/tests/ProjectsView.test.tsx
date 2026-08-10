import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ProjectsView } from '../src/components/ProjectsView';
import { useSettingsStore } from '../src/store/settingsStore';

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

function wrap(node: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  return <QueryClientProvider client={client}>{node}</QueryClientProvider>;
}

describe('ProjectsView', () => {
  beforeEach(() => {
    localStorage.clear();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
  });

  it('keeps project actions disabled until a descriptor is ready', () => {
    render(wrap(<ProjectsView />));

    expect(screen.getByText('incomplete')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^render policy$/i })).toBeDisabled();
    expect(screen.getByRole('button', { name: /^load facts$/i })).toBeDisabled();
    expect(screen.getByRole('button', { name: /^save decision$/i })).toBeDisabled();
  });

  it('renders policy, loads facts, and writes searchable decisions', async () => {
    const project = {
      name: 'Solo',
      id: 'solo',
      root: 'C:\\Projects\\solo',
      tags: ['memory', 'desktop'],
    };
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const body = JSON.parse(String(init?.body));
      expect(body.project).toStrictEqual(project);
      expect(init?.headers).toStrictEqual({
        Accept: 'application/json',
        'Content-Type': 'application/json',
      });

      if (url === 'http://solo.test/v1/project/policy') {
        return jsonResponse({
          command: 'project policy',
          client: 'codex',
          project,
          policy: 'Policy for Solo agents',
        });
      }
      if (url === 'http://solo.test/v1/project/facts') {
        return jsonResponse({
          command: 'project facts',
          project,
          subject: 'Solo',
          facts: [
            {
              triple_id: 'triple-1',
              subject_id: 'Solo',
              predicate: 'uses',
              object_id: 'Rust',
              object_kind: 'entity',
              valid_from_ms: 1780060000000,
              confidence: 0.91,
            },
          ],
        });
      }
      if (url === 'http://solo.test/v1/project/decisions') {
        return jsonResponse({
          command: 'project decisions',
          action: 'add',
          project,
          memory_id: 'mem-1',
          source_type: 'project_decision',
          source_id: 'project:solo:1',
          content: 'Project decision for Solo: Use Rust for the daemon.',
        });
      }
      if (url === 'http://solo.test/v1/project/decisions/search') {
        return jsonResponse({
          command: 'project decisions',
          action: 'query',
          project,
          query: 'daemon',
          limit: 10,
          hits: [
            {
              rowid: 1,
              memory_id: 'mem-1',
              cos_distance: 0.12,
              fused_score: 0.88,
              content: 'Use Rust for the daemon.',
              source_type: 'project_decision',
              tier: 'Hot',
            },
          ],
        });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    render(wrap(<ProjectsView />));

    fireEvent.change(screen.getByLabelText('Name'), { target: { value: project.name } });
    fireEvent.change(screen.getByLabelText('ID'), { target: { value: project.id } });
    fireEvent.change(screen.getByLabelText('Root'), { target: { value: project.root } });
    fireEvent.change(screen.getByLabelText('Tags'), { target: { value: project.tags.join(', ') } });

    expect(screen.getByText('ready')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^render policy$/i }));
    expect(await screen.findByText('Policy for Solo agents')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^copy policy$/i })).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Facts'), { target: { value: 'Solo' } });
    fireEvent.click(screen.getByRole('button', { name: /^load facts$/i }));
    expect(await screen.findByText('uses')).toBeInTheDocument();
    expect(screen.getByText(/confidence 0\.91/)).toBeInTheDocument();

    fireEvent.change(screen.getByLabelText('Add Decision'), {
      target: { value: 'Use Rust for the daemon.' },
    });
    fireEvent.click(screen.getByRole('button', { name: /^save decision$/i }));
    expect(await screen.findByText('mem-1')).toBeInTheDocument();
    await waitFor(() => expect(screen.getByLabelText('Add Decision')).toHaveValue(''));

    fireEvent.change(screen.getByLabelText('Search Decisions'), { target: { value: 'daemon' } });
    fireEvent.click(screen.getByRole('button', { name: /^search$/i }));
    expect(await screen.findByText('Use Rust for the daemon.')).toBeInTheDocument();

    expect(fetchMock).toHaveBeenCalledTimes(4);
  });
});
