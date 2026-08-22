import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App';
import { DEFAULT_SOLO_API_URL } from '../src/config/defaults';
import { useSettingsStore } from '../src/store/settingsStore';

vi.mock('../src/hooks/useGraphStream', () => ({
  useGraphStream: vi.fn(),
}));

vi.mock('../src/hooks/useGraphData', () => ({
  useGraphData: () => ({
    data: {
      nodes: [
        { id: 'ep:1', kind: 'episode', label: 'Memory' },
        { id: 'doc:1', kind: 'document', label: 'Doc' },
      ],
      edges: [],
    },
    isError: false,
    isFetching: false,
    dataUpdatedAt: Date.now(),
  }),
}));

vi.mock('../src/api/health', () => ({
  fetchSoloStatus: vi.fn(async () => ({
    ok: true,
    version: '0.11.1',
    build: { version: '0.11.1', version_with_build: '0.11.1' },
    library: { name: 'Community Memory Library', ready: true },
    embedder: {
      name: 'stub',
      version: 'v1',
      dim: 16,
      dtype: 'f32',
    },
    mcp: { sessions: 2 },
    steward: {
      configured: false,
      config_mode: 'none',
      provider: null,
      model: null,
      base_url: null,
      endpoint: null,
      processing_location: 'knowledge extraction disabled',
      hosted_processing_consent: false,
      runtime_llm: null,
      runtime_wired: false,
      runtime_has_llm: false,
      automatic: true,
      can_write_triples: false,
      trigger_interval_secs: 3600,
      trigger_episode_count: 50,
      consolidate_interval_secs: 3600,
      cluster_timeout_secs: 60,
      cluster_min_size: 2,
      cluster_cosine_threshold: 0.55,
      next_triples_run_at_ms: Date.UTC(2026, 5, 14, 12, 0),
      last_triples_run_at_ms: Date.UTC(2026, 5, 14, 11, 0),
      last_triples_trigger: 'manual',
      last_triples_error: null,
      last_triples_timed_out: false,
      pending_clusters: 7,
      coverage: {
        active_episodes: 89,
        clusters: 7,
        clustered_episodes: 80,
        abstractions: 0,
        pending_clusters: 7,
        triples: 0,
        entities: 0,
        relationships: 0,
        contradictions: 0,
      },
      next_consolidation_run_at_ms: Date.UTC(2026, 5, 14, 12, 0),
      last_consolidation_run_at_ms: Date.UTC(2026, 5, 14, 11, 0),
      last_consolidation_error: null,
      backfill: null,
      last_triples_batch: {
        ran: true,
        limit: 50,
        cluster_timeout_secs: 60,
        abstractions_built: 1,
        triples_extracted: 2,
        triples_quarantined: 0,
        clusters_failed: 0,
        clusters_deferred: 0,
        note: 'batch complete',
      },
      note: 'no Steward is wired in this daemon; clustering can run but triples will stay at zero',
    },
    capabilities: {
      memory_recall: { state: 'ready', explanation: 'Bundled local recall is ready.' },
      documents: { state: 'ready', explanation: 'Document memory is ready.' },
      clustering: { state: 'ready', explanation: 'Clustering has run.' },
      knowledge_extraction: { state: 'disabled', explanation: 'No Steward model is active.' },
      themes: { state: 'ready', explanation: 'Seven themes are available.' },
      facts: { state: 'disabled', explanation: 'No Steward model is active.' },
      entities: { state: 'disabled', explanation: 'No Steward model is active.' },
      graph: { state: 'disabled', explanation: 'No Steward model is active.' },
      contradictions: { state: 'disabled', explanation: 'No Steward model is active.' },
    },
    runtime: {
      pid: 4242,
      platform: 'win32',
      data_dir: 'C:\\SoloData',
    },
  })),
}));

vi.mock('../src/components/Toolbar', () => ({
  Toolbar: () => <div>Toolbar stub</div>,
}));

vi.mock('../src/components/StatusStrip', () => ({
  StatusStrip: () => <div>Status stub</div>,
}));

vi.mock('../src/components/GraphView', () => ({
  GraphView: () => <div>Graph view stub</div>,
}));

vi.mock('../src/components/InspectorPanel', () => ({
  InspectorPanel: () => <div>Inspector stub</div>,
}));

vi.mock('../src/components/InboxView', () => ({
  InboxView: ({ onSelectEpisode }: { onSelectEpisode: (id: string) => void }) => (
    <div>
      <div>Inbox view stub</div>
      <button type="button" onClick={() => onSelectEpisode('ep:1')}>
        Select episode
      </button>
    </div>
  ),
}));

vi.mock('../src/components/LogsView', () => ({
  LogsView: () => <div>Logs view stub</div>,
}));

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

function stubSetupFetch() {
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url.includes('/v1/inbox')) {
        return jsonResponse({
          items: [
            {
              memory_id: 'm1',
              label: 'Reviewed memory',
              preview: 'Reviewed memory',
              ts_ms: 1,
              source_type: 'test',
              salience: 0.8,
              status: 'active',
              review_state: 'approved',
            },
          ],
        });
      }
      if (url.includes('/memory/quality/reviews')) {
        return jsonResponse({ items: [] });
      }
      return new Response(JSON.stringify({ error: `unexpected request: ${url}` }), {
        status: 404,
        headers: { 'content-type': 'application/json' },
      });
    }),
  );
}

function renderApp(
  client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  }),
) {
  return render(
    <QueryClientProvider client={client}>
      <App />
    </QueryClientProvider>,
  );
}

describe('App desktop shell', () => {
  beforeEach(() => {
    window.history.replaceState(null, '', '/');
    vi.unstubAllGlobals();
    localStorage.clear();
    useSettingsStore.getState().setAll({
      apiUrl: DEFAULT_SOLO_API_URL,
      bearerToken: '',
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        const url = String(input);
        return new Response(JSON.stringify({ error: `unexpected request: ${url}` }), {
          status: 404,
          headers: { 'content-type': 'application/json' },
        });
      }),
    );
  });

  it('opens on Home and keeps graph, inbox, and inspector paths reachable', async () => {
    stubSetupFetch();
    renderApp();

    expect(screen.getByRole('heading', { name: 'Home' })).toBeInTheDocument();
    await waitFor(() => expect(screen.getByText('Next actions')).toBeInTheDocument());
    expect(screen.getByText('Solo status')).toBeInTheDocument();
    expect(await screen.findByText('Pending Steward')).toBeInTheDocument();
    expect(screen.getAllByText('7 clusters').length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText('1 item').length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText('no facts or triples yet')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /view memories/i })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /review inbox/i })).toBeInTheDocument();
    expect(screen.queryByText('Memory Surface')).not.toBeInTheDocument();

    const nav = screen.getByRole('navigation', { name: 'Solo' });
    for (const label of [
      'Home',
      'Memories',
      'Inbox',
      'Import',
      'Projects',
      'Settings',
    ]) {
      expect(within(nav).getByRole('button', { name: label })).toBeInTheDocument();
    }
    for (const hidden of [
      'Setup',
      'Health',
      'Connections',
      'Profiles',
      'Backups',
      'Logs',
    ]) {
      expect(within(nav).queryByRole('button', { name: hidden })).not.toBeInTheDocument();
    }

    fireEvent.click(screen.getByRole('button', { name: /^memories$/i }));
    expect(await screen.findByText('Toolbar stub')).toBeInTheDocument();
    expect(await screen.findByText('Graph view stub')).toBeInTheDocument();
    expect(await screen.findByText('Inspector stub')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /^inbox$/i }));
    expect(await screen.findByText('Inbox view stub')).toBeInTheDocument();
    await waitFor(() => expect(screen.queryByText('Graph view stub')).not.toBeInTheDocument());

    fireEvent.click(screen.getByRole('button', { name: /^select episode$/i }));
    expect(await screen.findByText('Graph view stub')).toBeInTheDocument();
    expect(await screen.findByText('Inspector stub')).toBeInTheDocument();
  });

  it('surfaces document import and backup workflows', async () => {
    renderApp();

    fireEvent.click(screen.getByRole('button', { name: /^import$/i }));
    expect(await screen.findByRole('heading', { name: 'Import' })).toBeInTheDocument();
    expect(await screen.findByRole('button', { name: 'Documents' })).toBeInTheDocument();
    expect(await screen.findByRole('button', { name: 'ChatGPT' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Import 0 files' })).toBeDisabled();

    fireEvent.click(screen.getByRole('button', { name: /^settings$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /^backups$/i }));
    expect(await screen.findByRole('heading', { name: 'Backups' })).toBeInTheDocument();
    expect(screen.getByText('Hot Backup')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^run backup$/i })).toBeInTheDocument();
  });

  it('opens the first-run setup checklist from the shell', async () => {
    stubSetupFetch();
    renderApp();

    fireEvent.click(screen.getByRole('button', { name: /setup solo/i }));

    expect(await screen.findByRole('heading', { level: 1, name: 'Setup' })).toBeInTheDocument();
    await waitFor(() => expect(screen.getAllByText('First Run').length).toBeGreaterThan(0));
    expect(screen.getByText('Readiness')).toBeInTheDocument();
    expect(screen.getByText('Start Solo')).toBeInTheDocument();
    expect(screen.getByText('Connect Codex')).toBeInTheDocument();
    expect(screen.getByText('Connect Claude')).toBeInTheDocument();
    expect(screen.getByText('Import memory')).toBeInTheDocument();
    expect(screen.getByText('Review inbox')).toBeInTheDocument();
    expect(screen.getByText('Create backup')).toBeInTheDocument();
    expect(screen.getByText('6 of 7 complete')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /connect codex/i }));
    expect(await screen.findByRole('heading', { name: 'Settings' })).toBeInTheDocument();
    expect(screen.getByText('MCP Connections')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'MCP connections' })).toBeInTheDocument();
    expect(screen.getByText(/solo setup-client codex .* --apply/)).toBeInTheDocument();
    expect(screen.getByText(/solo setup-client claude-desktop .* --apply/)).toBeInTheDocument();
  });

  it('surfaces connection endpoints in the shell', async () => {
    window.history.replaceState(null, '', '/#connections');
    renderApp();

    expect(await screen.findByRole('heading', { name: 'Connections' })).toBeInTheDocument();
    expect(screen.getAllByText('http://127.0.0.1:17821/mcp').length).toBeGreaterThan(0);
    expect(screen.getAllByText('Copy dry-run')).toHaveLength(3);
    expect(screen.getAllByText('Copy install')).toHaveLength(3);
    expect(screen.getAllByText('Copy Doctor')).toHaveLength(4);
    expect(
      screen.getByText(
        'solo setup-client doctor --url http://127.0.0.1:17821/mcp',
      ),
    ).toBeInTheDocument();
    expect(screen.getByText(/claude mcp add --transport http --scope user/)).toBeInTheDocument();
    expect(screen.getByText('Copy Claude Code')).toBeInTheDocument();
    expect(screen.getByText('Memory Policy')).toBeInTheDocument();
    expect(screen.getByText(/Solo Memory Policy - Codex/)).toBeInTheDocument();
    await waitFor(() => expect(screen.getByText('2')).toBeInTheDocument());
  });

  it('derives the rail MCP host from settings', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test:9000',
      bearerToken: '',
    });

    renderApp();

    expect(await screen.findByText('solo.test:9000')).toBeInTheDocument();
  });

  it('surfaces daemon health and the Memory Library in the shell', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async (_input: RequestInfo | URL) => {
        return new Response(JSON.stringify({ error: 'unexpected request' }), { status: 404 });
      }),
    );

    window.history.replaceState(null, '', '/#health');
    renderApp();

    expect(await screen.findByRole('heading', { name: 'Health' })).toBeInTheDocument();
    expect(await screen.findByText('Daemon State')).toBeInTheDocument();
    expect(screen.getByText('Daemon running and unlocked')).toBeInTheDocument();
    expect(screen.getByText('pid 4242')).toBeInTheDocument();
    expect(screen.getByText('C:\\SoloData')).toBeInTheDocument();
    expect(screen.getByText('stub@v1 16d f32')).toBeInTheDocument();
    expect(screen.getByText('MCP clients can connect')).toBeInTheDocument();
    expect(screen.queryByText('gpt-5.4')).not.toBeInTheDocument();
    expect(screen.queryByText('Chat backend')).not.toBeInTheDocument();
    expect(screen.getByRole('heading', { name: 'Memory Library' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^probe MCP$/i })).toBeInTheDocument();
  });

  it('opens connections from a hash deep link', async () => {
    window.history.replaceState(null, '', '/#connections');

    renderApp();

    expect(await screen.findByRole('heading', { name: 'Connections' })).toBeInTheDocument();
  });

  it('opens health from a hash deep link', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ ok: true }), {
            status: 200,
            headers: { 'content-type': 'application/json' },
          }),
      ),
    );
    window.history.replaceState(null, '', '/#health');

    renderApp();

    expect(await screen.findByRole('heading', { name: 'Health' })).toBeInTheDocument();
  });

  it('opens logs from diagnostics and hash deep link', async () => {
    const rendered = renderApp();

    fireEvent.click(screen.getByRole('button', { name: /^settings$/i }));
    fireEvent.click(await screen.findByRole('button', { name: /^logs$/i }));
    expect(await screen.findByText('Logs view stub')).toBeInTheDocument();
    rendered.unmount();

    window.history.replaceState(null, '', '/#logs');
    renderApp();
    expect(await screen.findByText('Logs view stub')).toBeInTheDocument();
  });

  it('surfaces settings endpoints and links to checks', async () => {
    let reviewDismissed = false;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, _init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith('/v1/settings/llm')) {
        const request = JSON.parse(String(_init?.body ?? '{}')) as {
          mode?: string;
          model?: string;
          base_url?: string;
          api_key_env?: string;
          endpoint?: string;
        };
        return jsonResponse({
          changed: true,
          config_path: 'C:\\SoloData\\solo.config.toml',
          previous: {
            mode: 'none',
            provider: null,
            model: null,
            base_url: null,
            api_key_env: null,
          },
          next: {
            mode: request.mode ?? 'ollama',
            provider: request.mode === 'none' ? null : (request.mode ?? 'ollama'),
            model: request.model ?? null,
            base_url: request.base_url ?? null,
            api_key_env: request.api_key_env ?? null,
            endpoint: request.endpoint ?? null,
          },
          restart_required: true,
          environment_commands: ['ollama pull qwen2.5-coder:7b'],
          next_steps: ['Restart Solo now to load qwen2.5-coder:7b.'],
          note: 'Config saved.',
        });
      }
      if (url.endsWith('/v1/settings/steward/cadence')) {
        return jsonResponse({
          changed: true,
          config_path: 'C:\\SoloData\\solo.config.toml',
          previous: {
            trigger_interval_secs: 3600,
            trigger_episode_count: 50,
            consolidate_interval_secs: 3600,
            cluster_timeout_secs: 60,
          },
          next: {
            trigger_interval_secs: 3600,
            trigger_episode_count: 50,
            consolidate_interval_secs: 3600,
            cluster_timeout_secs: 60,
          },
          restart_required: true,
          note: 'Cadence saved.',
        });
      }
      if (url.includes('/memory/quality/audit')) {
        return jsonResponse({
          generated_at_ms: 1,
          config: {
            low_confidence_below: 0.85,
            low_coherence_below: 0.72,
            long_literal_chars: 70,
            sample_limit: 8,
          },
          totals: {
            active_episodes: 89,
            clustered_episodes: 80,
            clusters: 7,
            abstractions: 7,
            active_triples: 12,
            entity_triples: 4,
            literal_triples: 8,
            triple_reviews_needs_review: 1,
            distinct_entities: 10,
            contradictions: 0,
          },
          health: {
            score: 86,
            grade: 'good',
            critical_issues: 0,
            warning_issues: 2,
            info_issues: 0,
          },
          issues: [
            {
              severity: 'warning',
              code: 'literal_fact_dominance',
              count: 8,
              summary: 'Most extracted facts are literals.',
              samples: [],
            },
          ],
          alias_groups: [],
        });
      }
      if (url.includes('/memory/quality/reviews/review-1')) {
        reviewDismissed = true;
        return jsonResponse({
          review_id: 'review-1',
          triple_id: null,
          cluster_id: 'cluster-1',
          source_episode_id: null,
          subject_id: 'assistant',
          predicate: 'said',
          object_id: 'unable to help',
          object_kind: 'literal',
          confidence: 0.4,
          reason_code: 'assistant_or_tool_chatter',
          reason: 'not durable user memory',
          status: 'dismissed',
          created_at_ms: 1,
        });
      }
      if (url.includes('/memory/quality/reviews')) {
        return jsonResponse({
          items: reviewDismissed
            ? []
            : [
                {
                  review_id: 'review-1',
                  triple_id: null,
                  cluster_id: 'cluster-1',
                  source_episode_id: null,
                  subject_id: 'assistant',
                  predicate: 'said',
                  object_id: 'unable to help',
                  object_kind: 'literal',
                  confidence: 0.4,
                  reason_code: 'assistant_or_tool_chatter',
                  reason: 'not durable user memory',
                  status: 'needs_review',
                  created_at_ms: 1,
                },
              ],
        });
      }
      return new Response(JSON.stringify({ error: `unexpected request: ${url}` }), {
        status: 404,
        headers: { 'content-type': 'application/json' },
      });
    });
    vi.stubGlobal('fetch', fetchMock);
    renderApp();

    fireEvent.click(screen.getByRole('button', { name: /^settings$/i }));

    expect(await screen.findByRole('heading', { name: 'Settings' })).toBeInTheDocument();
    expect(screen.getByText('Solo HTTP - installed desktop default')).toBeInTheDocument();
    expect(screen.getByText('endpoints persist; bearer is session-only')).toBeInTheDocument();
    expect(screen.getByText('MCP Connections')).toBeInTheDocument();
    expect(screen.getByText('Runtime & Embedder')).toBeInTheDocument();
    expect(await screen.findByText('stub@v1 16d f32')).toBeInTheDocument();
    expect(screen.getByText('supported by config switch')).toBeInTheDocument();
    expect(screen.getByText('Solo Controls action')).toBeInTheDocument();
    expect(screen.getByText('tray-supervised migration')).toBeInTheDocument();
    expect(screen.getByText('handled by migration')).toBeInTheDocument();
    expect(screen.getByText(/Embedder Migration/)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Check config guard' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Copy Claude Desktop' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Copy migration command' })).toBeInTheDocument();
    expect(screen.getByText('Steward LLM')).toBeInTheDocument();
    expect(screen.getByText('Memory Capabilities')).toBeInTheDocument();
    expect(screen.getByText('Bundled local recall is ready.')).toBeInTheDocument();
    expect(screen.getAllByText('No Steward model is active.').length).toBeGreaterThanOrEqual(1);
    const stewardPanel = screen.getByText('Steward LLM').closest('section');
    expect(stewardPanel).not.toBeNull();
    expect(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Ollama' }),
    ).toBeInTheDocument();
    expect(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Anthropic' }),
    ).toBeInTheDocument();
    expect(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'OpenAI' }),
    ).toBeInTheDocument();
    expect(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Apply LLM config' }),
    ).toBeInTheDocument();
    expect(
      within(stewardPanel as HTMLElement).getByText('Runtime verification'),
    ).toBeInTheDocument();
    fireEvent.click(within(stewardPanel as HTMLElement).getByRole('button', { name: 'Ollama' }));
    fireEvent.click(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Cloud direct' }),
    );
    expect(
      within(stewardPanel as HTMLElement).getByText(/processed off device by Ollama Cloud/),
    ).toBeInTheDocument();
    expect(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Apply LLM config' }),
    ).toBeDisabled();
    fireEvent.click(within(stewardPanel as HTMLElement).getByRole('checkbox'));
    expect(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Apply LLM config' }),
    ).toBeEnabled();
    fireEvent.click(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Apply LLM config' }),
    );
    expect(
      await within(stewardPanel as HTMLElement).findByText(/daemon-only restart cannot inherit/),
    ).toBeInTheDocument();
    expect(
      within(stewardPanel as HTMLElement).queryByRole('button', { name: 'Restart Solo now' }),
    ).not.toBeInTheDocument();
    fireEvent.click(within(stewardPanel as HTMLElement).getByRole('button', { name: 'Ollama' }));
    fireEvent.click(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Local model' }),
    );
    fireEvent.click(
      within(stewardPanel as HTMLElement).getByRole('button', { name: 'Apply LLM config' }),
    );
    expect(await screen.findByRole('button', { name: 'Restart Solo now' })).toBeInTheDocument();
    expect(screen.getByText('Steward Cadence')).toBeInTheDocument();
    expect(screen.getByText('Triple interval')).toBeInTheDocument();
    expect(screen.getByText('Episode trigger')).toBeInTheDocument();
    expect(screen.getByText('Consolidation interval')).toBeInTheDocument();
    expect(screen.getByText('Cluster timeout')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Apply cadence' }));
    expect(await screen.findByText('Cadence saved.')).toBeInTheDocument();
    expect(screen.getByText('Derived Memory & Triples')).toBeInTheDocument();
    expect(screen.getByText('Steward Runtime')).toBeInTheDocument();
    expect(screen.getByText('Memory Quality')).toBeInTheDocument();
    expect(screen.getByText('literal_fact_dominance')).toBeInTheDocument();
    expect(await screen.findByText('assistant_or_tool_chatter')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    await waitFor(() => {
      expect(screen.queryByText('assistant_or_tool_chatter')).not.toBeInTheDocument();
    });
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringContaining('/memory/quality/reviews/review-1'),
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({
          status: 'dismissed',
          note: 'Dismissed from Memory Quality inbox',
        }),
      }),
    );
    expect(screen.getByText('Pending Steward work')).toBeInTheDocument();
    expect(screen.getByText('7 clusters')).toBeInTheDocument();
    expect(
      screen.getByText('ran: 1 abstractions, 2 triples, 0 review, 0 failed, 0 deferred'),
    ).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Run consolidation' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Run triples now' })).toBeInTheDocument();
    expect(screen.getAllByText('no Steward wired').length).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText('3600s or 50 episodes').length).toBeGreaterThanOrEqual(1);
    expect(screen.getByText(/No triple edges are visible/)).toBeInTheDocument();

    expect(screen.getByText('Admin & Diagnostics')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /^MCP connections$/i }));
    expect(await screen.findByRole('heading', { name: 'Connections' })).toBeInTheDocument();
  });

  it('rejects the retired profiles hash deep link', async () => {
    window.history.replaceState(null, '', '/#profiles');

    renderApp();

    expect(await screen.findByRole('heading', { level: 1, name: 'Home' })).toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: 'Profiles' })).not.toBeInTheDocument();
  });

  it('opens projects from a hash deep link', async () => {
    window.history.replaceState(null, '', '/#projects');

    renderApp();

    expect(await screen.findByRole('heading', { level: 1, name: 'Projects' })).toBeInTheDocument();
    expect(screen.getByText('Agent Policy')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^render policy$/i })).toBeDisabled();
  });

  it('opens backups from a hash deep link', async () => {
    window.history.replaceState(null, '', '/#backups');

    renderApp();

    expect(await screen.findByRole('heading', { level: 1, name: 'Backups' })).toBeInTheDocument();
    expect(screen.getByText('Recovery Surface')).toBeInTheDocument();
  });

  it('opens setup from a hash deep link', async () => {
    stubSetupFetch();
    window.history.replaceState(null, '', '/#setup');

    renderApp();

    expect(await screen.findByRole('heading', { level: 1, name: 'Setup' })).toBeInTheDocument();
    await waitFor(() => expect(screen.getAllByText('First Run').length).toBeGreaterThan(0));
  });
});
