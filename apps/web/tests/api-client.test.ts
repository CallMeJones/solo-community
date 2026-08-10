import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  DocumentImportCleanupUncertainError,
  DocumentImportUncertainError,
  consolidateMemory,
  addProjectDecision,
  fetchContradictions,
  fetchFactsAbout,
  fetchGraph,
  fetchLogs,
  fetchMemoryQualityAudit,
  fetchProjectFacts,
  forgetDocument,
  forgetMemory,
  forgetRetainedAsset,
  importBrowserDocument,
  importDocumentPath,
  listDocumentLifecycle,
  probeMcpTools,
  repairDerivedMemory,
  rememberMemory,
  restartSoloRuntime,
  renderProjectPolicy,
  reviewMemory,
  resumeUncertainDocumentImport,
  runBackup,
  searchProjectDecisions,
  switchStewardCadence,
  switchOllamaEmbedder,
  switchStewardLlm,
  extractTriplesNow,
  updateMemory,
  updateMemoryQualityReview,
} from '../src/api/client';
import { fetchSoloStatus } from '../src/api/health';
import { DEFAULT_SOLO_API_URL } from '../src/config/defaults';
import { useSettingsStore } from '../src/store/settingsStore';

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

describe('api client', () => {
  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('probeMcpTools initializes MCP and checks required memory tools', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi
      .fn()
      .mockResolvedValueOnce(
        jsonResponse({
          result: {
            protocolVersion: '2025-03-26',
            serverInfo: { name: 'solo', version: '0.11.9' },
          },
        }),
      )
      .mockResolvedValueOnce(new Response(null, { status: 202 }))
      .mockResolvedValueOnce(
        jsonResponse({
          result: {
            tools: [
              { name: 'memory_context' },
              { name: 'memory_inbox' },
              { name: 'memory_review' },
            ],
          },
        }),
      )
      .mockResolvedValueOnce(
        jsonResponse({
          result: {
            content: [{ type: 'text', text: 'Solo MCP readiness check ok' }],
          },
        }),
      )
      .mockResolvedValueOnce(new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', async (input: RequestInfo | URL, init?: RequestInit) => {
      const response = await fetchMock(input, init);
      if (fetchMock.mock.calls.length === 1) {
        return new Response(await response.text(), {
          status: response.status,
          headers: {
            'content-type': 'application/json',
            'Mcp-Session-Id': 'session-1',
          },
        });
      }
      return response;
    });

    await expect(probeMcpTools()).resolves.toMatchObject({
      sessionId: 'session-1',
      serverName: 'solo',
      serverVersion: '0.11.9',
      toolCount: 3,
      missingRequiredTools: [],
      readOnlyCall: {
        toolName: 'memory_context',
        status: 'passed',
        detail: 'returned 1 content item',
        contentItems: 1,
      },
    });
    expect(fetchMock).toHaveBeenNthCalledWith(
      1,
      'http://solo.test/mcp',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          'Content-Type': 'application/json',
        }),
      }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      3,
      'http://solo.test/mcp',
      expect.objectContaining({
        headers: expect.objectContaining({
          'Mcp-Session-Id': 'session-1',
        }),
      }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      4,
      'http://solo.test/mcp',
      expect.objectContaining({
        method: 'POST',
        body: expect.stringContaining('"tools/call"'),
      }),
    );
    expect(fetchMock).toHaveBeenNthCalledWith(
      5,
      'http://solo.test/mcp',
      expect.objectContaining({
        method: 'DELETE',
        headers: expect.objectContaining({
          'Mcp-Session-Id': 'session-1',
        }),
      }),
    );
  });

  it('posts project policy, facts, and decisions through the Community connection', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
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
        expect(body.client).toBe('codex');
        return jsonResponse({
          command: 'project policy',
          client: 'codex',
          project,
          policy: 'Policy for Solo',
        });
      }
      if (url === 'http://solo.test/v1/project/facts') {
        expect(body.subject).toBe('Solo');
        expect(body.limit).toBe(20);
        return jsonResponse({
          command: 'project facts',
          project,
          subject: 'Solo',
          facts: [],
        });
      }
      if (url === 'http://solo.test/v1/project/decisions') {
        expect(body.decision).toBe('Use Rust for the daemon.');
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
        expect(body.query).toBe('daemon');
        expect(body.limit).toBe(10);
        return jsonResponse({
          command: 'project decisions',
          action: 'query',
          project,
          query: 'daemon',
          limit: 10,
          hits: [],
        });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      renderProjectPolicy(project, 'codex'),
    ).resolves.toMatchObject({
      policy: 'Policy for Solo',
    });
    await expect(
      fetchProjectFacts(project, { subject: ' Solo ', limit: 20 }),
    ).resolves.toMatchObject({ subject: 'Solo' });
    await expect(
      addProjectDecision(project, 'Use Rust for the daemon.'),
    ).resolves.toMatchObject({ memory_id: 'mem-1' });
    await expect(
      searchProjectDecisions(project, 'daemon', { limit: 10 }),
    ).resolves.toMatchObject({ query: 'daemon' });

    expect(fetchMock).toHaveBeenCalledTimes(4);
  });

  it('fetchContradictions calls GET /memory/contradictions with a limit', async () => {
    const fetchMock = vi.fn(async () => jsonResponse([]));
    vi.stubGlobal('fetch', fetchMock);

    await expect(fetchContradictions(20)).resolves.toStrictEqual([]);

    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringContaining('/memory/contradictions?limit=20'),
      expect.objectContaining({
        method: 'GET',
        headers: expect.objectContaining({
          Accept: 'application/json',
        }),
      }),
    );
  });

  it('fetchFactsAbout calls GET /memory/facts_about with filters', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async () =>
      jsonResponse([
        {
          triple_id: 'tr:1',
          subject_id: 'solo',
          predicate: 'uses',
          object_id: 'ollama',
          object_kind: 'entity',
          valid_from_ms: 1,
          confidence: 0.92,
          cluster_id: 'cl:1',
        },
      ]),
    );
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      fetchFactsAbout(
        {
          subject: 'solo',
          predicate: 'uses',
          includeAsObject: true,
          limit: 5,
        },
        {},
      ),
    ).resolves.toHaveLength(1);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/facts_about?subject=solo&limit=5&predicate=uses&include_as_object=true',
      expect.objectContaining({
        method: 'GET',
        headers: expect.objectContaining({
          Accept: 'application/json',
        }),
      }),
    );
  });

  it('fetchMemoryQualityAudit calls GET /memory/quality/audit with thresholds', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const response = {
      generated_at_ms: 1,
      config: {
        low_confidence_below: 0.86,
        low_coherence_below: 0.7,
        long_literal_chars: 80,
        sample_limit: 3,
      },
      totals: {
        active_episodes: 10,
        clustered_episodes: 9,
        clusters: 2,
        abstractions: 2,
        active_triples: 5,
        entity_triples: 2,
        literal_triples: 3,
        triple_reviews_needs_review: 0,
        distinct_entities: 4,
        contradictions: 0,
      },
      health: {
        score: 93,
        grade: 'excellent',
        critical_issues: 0,
        warning_issues: 1,
        info_issues: 0,
      },
      issues: [],
      alias_groups: [],
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      fetchMemoryQualityAudit(
        {
          lowConfidenceBelow: 0.86,
          lowCoherenceBelow: 0.7,
          longLiteralChars: 80,
          sampleLimit: 3,
        },
        {},
      ),
    ).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/quality/audit?low_confidence_below=0.86&low_coherence_below=0.7&long_literal_chars=80&sample_limit=3',
      expect.objectContaining({
        method: 'GET',
        headers: expect.objectContaining({
          Accept: 'application/json',
        }),
      }),
    );
  });

  it('updateMemoryQualityReview posts status changes', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const response = {
      review_id: 'review/needs encoding',
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
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      updateMemoryQualityReview(
        'review/needs encoding',
        { status: 'dismissed', note: 'not useful' },
        {},
      ),
    ).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/quality/reviews/review%2Fneeds%20encoding',
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({ status: 'dismissed', note: 'not useful' }),
        headers: expect.objectContaining({
          Accept: 'application/json',
          'Content-Type': 'application/json',
        }),
      }),
    );
  });

  it('consolidateMemory posts to /memory/consolidate', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const response = {
      episodes_seen: 12,
      clusters_built: 2,
      episodes_clustered: 10,
      clusters_merged: 0,
      clusters_absorbed: 0,
      existing_clusters_merged: 0,
      abstractions_regenerated: 0,
      abstractions_built: 1,
      triples_built: 4,
      contradictions_found: 0,
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(consolidateMemory()).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/consolidate',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
        }),
      }),
    );
  });

  it('extractTriplesNow posts to /memory/triples/extract', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const response = {
      ran: true,
      limit: 50,
      cluster_timeout_secs: 60,
      abstractions_built: 1,
      triples_extracted: 4,
      clusters_failed: 0,
      clusters_deferred: 0,
      note: 'done',
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(extractTriplesNow()).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/triples/extract',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
        }),
      }),
    );
  });

  it('repairDerivedMemory posts to /memory/derived/repair', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const response = {
      mode: 'rebuild_all',
      dry_run: false,
      clusters_scanned: 1,
      clusters_repaired: 1,
      abstractions_deleted: 1,
      triples_deleted: 0,
      contradictions_deleted: 0,
      clusters_deleted: 1,
      cluster_memberships_deleted: 3,
      candidate_samples: [],
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      repairDerivedMemory({ mode: 'rebuild_all' }),
    ).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/derived/repair',
      expect.objectContaining({
        method: 'POST',
        body: JSON.stringify({ mode: 'rebuild_all' }),
        headers: expect.objectContaining({
          Accept: 'application/json',
          'Content-Type': 'application/json',
        }),
      }),
    );
  });

  it('fetchGraph follows node and edge cursors so links keep both endpoints', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (url === 'http://solo.test/v1/graph/nodes?limit=500') {
        return jsonResponse({
          nodes: [{ id: 'ep:1', kind: 'episode', label: 'Episode 1', ts_ms: 1 }],
          next_cursor: 'nodes-page-2',
        });
      }
      if (url === 'http://solo.test/v1/graph/nodes?limit=500&cursor=nodes-page-2') {
        return jsonResponse({
          nodes: [{ id: 'cl:1', kind: 'cluster', label: 'Cluster 1', ts_ms: 2 }],
          next_cursor: null,
        });
      }
      if (url === 'http://solo.test/v1/graph/edges?limit=500') {
        return jsonResponse({
          edges: [{ id: 'e:1', source: 'cl:1', target: 'ep:1', kind: 'cluster_member' }],
          next_cursor: 'edges-page-2',
        });
      }
      if (url === 'http://solo.test/v1/graph/edges?limit=500&cursor=edges-page-2') {
        return jsonResponse({
          edges: [{ id: 'e:2', source: 'doc:1', target: 'chunk:1', kind: 'document_chunk' }],
          next_cursor: null,
        });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const graph = await fetchGraph();

    expect(graph.nodes.map((node) => node.id)).toStrictEqual(['ep:1', 'cl:1']);
    expect(graph.edges.map((edge) => edge.id)).toStrictEqual(['e:1', 'e:2']);
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/graph/nodes?limit=500&cursor=nodes-page-2',
      expect.objectContaining({
        headers: expect.objectContaining({
        }),
      }),
    );
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/graph/edges?limit=500&cursor=edges-page-2',
      expect.objectContaining({
        headers: expect.objectContaining({
        }),
      }),
    );
  });

  it('switchOllamaEmbedder posts model config to /v1/settings/embedder/ollama', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: 'secret-token',
    });
    const response = {
      changed: true,
      config_path: 'C:\\SoloData\\solo.config.toml',
      previous: { name: 'stub', version: 'v1', dim: 16, dtype: 'f32' },
      next: { name: 'ollama:nomic-embed-text', version: 'v1', dim: 768, dtype: 'f32' },
      restart_required: true,
      reembed_required: true,
      reembed_command: 'solo reembed --gc',
      environment_commands: ['setx SOLO_OLLAMA_EMBED_MODEL nomic-embed-text'],
      next_steps: ['Stop Solo', 'solo reembed --gc', 'Start Solo'],
      note: 'Config updated.',
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      switchOllamaEmbedder(
        {
          model: 'nomic-embed-text',
          dim: 768,
          base_url: 'http://localhost:11434',
        },
        {},
      ),
    ).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/settings/embedder/ollama',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          Authorization: 'Bearer secret-token',
          'Content-Type': 'application/json',
        }),
        body: JSON.stringify({
          model: 'nomic-embed-text',
          dim: 768,
          base_url: 'http://localhost:11434',
        }),
      }),
    );
  });

  it('switchStewardLlm posts provider config to /v1/settings/llm', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: 'secret-token',
    });
    const response = {
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
        mode: 'ollama',
        provider: 'ollama',
        model: 'qwen2.5-coder:7b',
        base_url: 'http://localhost:11434',
        api_key_env: null,
      },
      restart_required: true,
      environment_commands: ['ollama pull qwen2.5-coder:7b'],
      next_steps: ['Restart Solo from Solo Controls.'],
      note: 'Config saved.',
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      switchStewardLlm(
        {
          mode: 'ollama',
          model: 'qwen2.5-coder:7b',
          base_url: 'http://localhost:11434',
        },
        {},
      ),
    ).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/settings/llm',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          Authorization: 'Bearer secret-token',
          'Content-Type': 'application/json',
        }),
        body: JSON.stringify({
          mode: 'ollama',
          model: 'qwen2.5-coder:7b',
          base_url: 'http://localhost:11434',
        }),
      }),
    );
  });

  it('restartSoloRuntime posts to /v1/runtime/restart with bearer auth', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: 'secret-token',
    });
    const response = {
      accepted: true,
      restart_expected: true,
      note: 'Solo accepted the restart request.',
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(restartSoloRuntime()).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/runtime/restart',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          Authorization: 'Bearer secret-token',
        }),
      }),
    );
  });

  it('switchStewardCadence posts cadence settings to /v1/settings/steward/cadence', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: 'secret-token',
    });
    const response = {
      changed: true,
      config_path: 'C:\\SoloData\\solo.config.toml',
      previous: {
        trigger_interval_secs: 3600,
        trigger_episode_count: 50,
        consolidate_interval_secs: 3600,
        cluster_timeout_secs: 60,
      },
      next: {
        trigger_interval_secs: 900,
        trigger_episode_count: 25,
        consolidate_interval_secs: 1800,
        cluster_timeout_secs: 45,
      },
      restart_required: true,
      note: 'Cadence saved.',
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      switchStewardCadence(
        {
          trigger_interval_secs: 900,
          trigger_episode_count: 25,
          consolidate_interval_secs: 1800,
          cluster_timeout_secs: 45,
        },
        {},
      ),
    ).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/settings/steward/cadence',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          Authorization: 'Bearer secret-token',
          'Content-Type': 'application/json',
        }),
        body: JSON.stringify({
          trigger_interval_secs: 900,
          trigger_episode_count: 25,
          consolidate_interval_secs: 1800,
          cluster_timeout_secs: 45,
        }),
      }),
    );
  });

  it('forgetMemory calls DELETE /memory/:id with a reason', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async () => new Response(null, { status: 204 }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      forgetMemory('ep:01935b9c-1234-7abc-89de-fedcba987654', 'inbox cleanup'),
    ).resolves.toBeUndefined();

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/01935b9c-1234-7abc-89de-fedcba987654?reason=inbox+cleanup',
      expect.objectContaining({
        method: 'DELETE',
        headers: expect.objectContaining({
          Accept: 'application/json',
        }),
      }),
    );
  });

  it('reviewMemory accepts reset and preserves a null review response state', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async () =>
      jsonResponse({
        memory_id: 'review-reset',
        state: null,
        reviewed_at_ms: null,
      }),
    );
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      reviewMemory('ep:review-reset', 'reset', { note: 'Reset from test' }),
    ).resolves.toStrictEqual({
      memory_id: 'review-reset',
      state: null,
      reviewed_at_ms: null,
    });

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/inbox/review-reset/review',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          'Content-Type': 'application/json',
        }),
        body: JSON.stringify({
          state: 'reset',
          note: 'Reset from test',
        }),
      }),
    );
  });

  it('includes response error detail for memory mutation failures', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(
        async () =>
          new Response(JSON.stringify({ error: 'memory row is locked' }), {
            status: 409,
            statusText: 'Conflict',
            headers: { 'content-type': 'application/json' },
          }),
      ),
    );

    await expect(
      updateMemory('ep:locked-memory', 'updated'),
    ).rejects.toThrow(
      'Solo API PATCH /memory/locked-memory failed (409 Conflict): memory row is locked',
    );
  });

  it('rememberMemory posts imported content with source metadata', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async () => jsonResponse({ memory_id: 'mem-1' }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      rememberMemory(
        {
          content: 'Imported conversation',
          source_type: 'import.chatgpt',
          source_id: 'chat-1',
          salience: 0.55,
        },
        {},
      ),
    ).resolves.toStrictEqual({ memory_id: 'mem-1' });

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          'Content-Type': 'application/json',
        }),
        body: JSON.stringify({
          content: 'Imported conversation',
          source_type: 'import.chatgpt',
          source_id: 'chat-1',
          salience: 0.55,
        }),
      }),
    );
  });

  it('importDocumentPath scans local paths through the document import endpoint', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const response = {
      path: 'C:\\Notes',
      source: 'markdown_text',
      source_label: 'Markdown/Text',
      dry_run: true,
      recursive: true,
      truncated: false,
      total_files: 1,
      total_bytes: 12,
      imported: 0,
      deduped: 0,
      failed: 0,
      chunks_persisted: 0,
      files: [{ path: 'C:\\Notes\\a.md', bytes: 12 }],
      results: [],
    };
    const fetchMock = vi.fn(async () => jsonResponse(response));
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      importDocumentPath(
        {
          path: 'C:\\Notes',
          source: 'markdown_text',
          dry_run: true,
          recursive: true,
          max_files: 500,
        },
        {},
      ),
    ).resolves.toStrictEqual(response);

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/documents/import',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          'Content-Type': 'application/json',
        }),
        body: JSON.stringify({
          path: 'C:\\Notes',
          source: 'markdown_text',
          dry_run: true,
          recursive: true,
          max_files: 500,
        }),
      }),
    );
  });

  it('imports browser files through prepare, chunked PATCH, commit, and staged ingest', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: 'local-bearer',
    });
    const patchOffsets: string[] = [];
    const progress: string[] = [];
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === 'http://solo.test/memory/documents/uploads' && init?.method === 'POST') {
        return jsonResponse({
          upload_id: 'upload-1',
          upload_url: '/uploads/upload-1',
          upload_path: '/uploads/upload-1',
          route_kind: 'direct_local',
          upload_method: 'PATCH',
          upload_offset_header: 'upload-offset',
          upload_length_header: 'upload-length',
          required_headers: {
            'content-type': 'application/octet-stream',
            'upload-offset': '0',
            'upload-length': '10',
          },
          max_file_bytes: 104857600,
          max_chunk_bytes: 5,
          recommended_chunk_bytes: 5,
          expires_at_ms: Date.now() + 60_000,
          default_store_original_file: true,
        });
      }
      if (url === 'http://solo.test/uploads/upload-1' && init?.method === 'PATCH') {
        const headers = init.headers as Record<string, string>;
        patchOffsets.push(headers['upload-offset']);
        expect(headers).toStrictEqual({
          'content-type': 'application/octet-stream',
          'upload-offset': String(patchOffsets.length === 1 ? 0 : 5),
          'upload-length': '10',
          Authorization: 'Bearer local-bearer',
        });
        expect(init.body).toBeInstanceOf(Blob);
        return new Response(null, { status: 204 });
      }
      if (
        url === 'http://solo.test/memory/documents/uploads/upload-1/commit' &&
        init?.method === 'POST'
      ) {
        return jsonResponse({
          upload_id: 'upload-1',
          staged_uri: 'solo-staged://upload/upload-1',
          filename: 'pilot.pdf',
          mime_type: 'application/pdf',
          size_bytes: 10,
          sha256: 'abc',
        });
      }
      if (url === 'http://solo.test/memory/documents/staged/ingest' && init?.method === 'POST') {
        expect(JSON.parse(String(init.body))).toMatchObject({
          staged_uri: 'solo-staged://upload/upload-1',
          retain_source_file: false,
          store_original_file: true,
        });
        return jsonResponse({
          staged_uri: 'solo-staged://upload/upload-1',
          document_id: 'doc-1',
          chunks_persisted: 3,
          bytes_ingested: 10,
          deduped: false,
          stored_original_file: true,
          asset: {
            asset_id: 'asset-1',
            sha256: 'abc',
            mime_type: 'application/pdf',
            filename: 'pilot.pdf',
            size_bytes: 10,
            storage_path: 'assets/abc',
            deduped: false,
          },
          document_asset_link: {
            link_id: 'link-1',
            doc_id: 'doc-1',
            asset_id: 'asset-1',
          },
          extraction_status: 'extracted',
          extraction_error: null,
          deleted_staged_file: true,
          retained_source_file: false,
        });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const recoveryCheckpoints: unknown[] = [];
    const result = await importBrowserDocument(
      new File(['0123456789'], 'pilot.pdf', { type: 'application/pdf' }),
      {
        storeOriginalFile: true,
        onProgress: (event) => progress.push(`${event.stage}:${event.bytesSent}`),
        onRecoveryCheckpoint: (checkpoint) => recoveryCheckpoints.push(checkpoint),
      },
    );

    expect(result).toMatchObject({ document_id: 'doc-1', extraction_status: 'extracted' });
    expect(patchOffsets).toStrictEqual(['0', '5']);
    expect(progress).toStrictEqual([
      'preparing:0',
      'uploading:0',
      'uploading:5',
      'uploading:10',
      'committing:10',
      'extracting:10',
      'complete:10',
    ]);
    expect(recoveryCheckpoints).toStrictEqual([
      { uploadId: 'upload-1', stagedUri: null, storeOriginalFile: true },
      {
        uploadId: 'upload-1',
        stagedUri: 'solo-staged://upload/upload-1',
        storeOriginalFile: true,
      },
    ]);
  });

  it('keeps one immutable endpoint and bearer through every upload phase', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo-a.test',
      bearerToken: 'token-a',
    });
    const calls: Array<{ url: string; method: string; headers: Record<string, string> }> = [];
    let patchCount = 0;
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      const headers = init?.headers as Record<string, string>;
      calls.push({ url, method, headers });
      if (url.endsWith('/memory/documents/uploads') && method === 'POST') {
        return jsonResponse({
          upload_id: 'upload-stable',
          upload_url: '/uploads/upload-stable',
          upload_path: '/uploads/upload-stable',
          route_kind: 'direct_local',
          upload_method: 'PATCH',
          upload_offset_header: 'upload-offset',
          upload_length_header: 'upload-length',
          required_headers: {},
          max_file_bytes: 100,
          max_chunk_bytes: 2,
          recommended_chunk_bytes: 2,
          expires_at_ms: Date.now() + 60_000,
          default_store_original_file: false,
        });
      }
      if (url.endsWith('/uploads/upload-stable') && method === 'PATCH') {
        patchCount += 1;
        if (patchCount === 1) {
          useSettingsStore.getState().setAll({
            apiUrl: 'http://solo-b.test',
            bearerToken: 'token-b',
          });
        }
        return new Response(null, { status: 204 });
      }
      if (url.endsWith('/memory/documents/uploads/upload-stable/commit') && method === 'POST') {
        return jsonResponse({
          upload_id: 'upload-stable',
          staged_uri: 'solo-staged://upload/upload-stable',
          filename: 'stable.txt',
          mime_type: 'text/plain',
          size_bytes: 4,
          sha256: 'abc',
        });
      }
      if (url.endsWith('/memory/documents/staged/ingest') && method === 'POST') {
        expect(JSON.parse(String(init?.body))).toMatchObject({ store_original_file: false });
        return jsonResponse({
          staged_uri: 'solo-staged://upload/upload-stable',
          document_id: 'doc-stable',
          chunks_persisted: 1,
          bytes_ingested: 4,
          deduped: false,
          stored_original_file: false,
          asset: null,
          document_asset_link: null,
          extraction_status: 'extracted',
          extraction_error: null,
          deleted_staged_file: true,
          retained_source_file: false,
        });
      }
      throw new Error(`unexpected fetch: ${method} ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    await importBrowserDocument(new File(['data'], 'stable.txt', { type: 'text/plain' }), {});

    expect(calls).toHaveLength(5);
    for (const call of calls) {
      expect(call.url).toMatch(/^http:\/\/solo-a\.test\//);
      expect(call.headers).toMatchObject({
        Authorization: 'Bearer token-a',
      });
      expect(
        Object.keys(call.headers).every((name) =>
          ['accept', 'authorization', 'content-type', 'upload-length', 'upload-offset'].includes(
            name.toLowerCase(),
          ),
        ),
      ).toBe(true);
    }
  });

  it('recovers a lost commit response across the status-to-abort race without duplicating bytes', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    let statusReads = 0;
    let abortAttempts = 0;
    let ingestAttempts = 0;
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? 'GET';
        if (url.endsWith('/memory/documents/uploads') && method === 'POST') {
          return jsonResponse({
            upload_id: 'upload-lost-commit',
            upload_url: '/uploads/upload-lost-commit',
            upload_path: '/uploads/upload-lost-commit',
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: {},
            max_file_bytes: 100,
            max_chunk_bytes: 100,
            recommended_chunk_bytes: 100,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: false,
          });
        }
        if (url.endsWith('/uploads/upload-lost-commit') && method === 'PATCH') {
          return new Response(null, { status: 204 });
        }
        if (
          url.endsWith('/memory/documents/uploads/upload-lost-commit/commit') &&
          method === 'POST'
        ) {
          throw new TypeError('connection closed after Solo accepted commit');
        }
        if (url.endsWith('/memory/documents/uploads/upload-lost-commit') && method === 'GET') {
          statusReads += 1;
          const base = {
            upload_id: 'upload-lost-commit',
            bytes_received: 4,
            size_bytes: 4,
            next_offset: 4,
            expires_at_ms: Date.now() + 60_000,
            staged_uri: null,
            commit_result: null,
            ingest_result: null,
            terminal: false,
          };
          if (statusReads === 1) {
            return jsonResponse({
              ...base,
              status: 'busy',
              operation_in_progress: true,
              active_operation: 'commit',
            });
          }
          if (statusReads === 2) {
            return jsonResponse({
              ...base,
              status: 'open',
              operation_in_progress: false,
              active_operation: null,
            });
          }
          const commitResult = {
            upload_id: 'upload-lost-commit',
            staged_uri: 'solo-staged://upload/upload-lost-commit',
            filename: 'lost-commit.txt',
            mime_type: 'text/plain',
            size_bytes: 4,
            sha256: 'abc',
          };
          return jsonResponse({
            ...base,
            status: 'committed',
            operation_in_progress: false,
            active_operation: null,
            staged_uri: commitResult.staged_uri,
            commit_result: commitResult,
          });
        }
        if (url.endsWith('/memory/documents/uploads/upload-lost-commit') && method === 'DELETE') {
          abortAttempts += 1;
          return new Response('commit won the upload lock', { status: 409 });
        }
        if (url.endsWith('/memory/documents/staged/ingest') && method === 'POST') {
          ingestAttempts += 1;
          return jsonResponse({
            staged_uri: 'solo-staged://upload/upload-lost-commit',
            document_id: 'doc-lost-commit',
            chunks_persisted: 1,
            bytes_ingested: 4,
            deduped: false,
            stored_original_file: false,
            asset: null,
            document_asset_link: null,
            extraction_status: 'extracted',
            extraction_error: null,
            deleted_staged_file: true,
            retained_source_file: false,
            idempotent_replay: false,
            ingest_completed_at_ms: 1_700_000_000_000,
          });
        }
        throw new Error(`unexpected fetch: ${method} ${url}`);
      }),
    );

    await expect(
      importBrowserDocument(new File(['data'], 'lost-commit.txt', { type: 'text/plain' }), {}),
    ).resolves.toMatchObject({ document_id: 'doc-lost-commit' });
    expect(statusReads).toBe(3);
    expect(abortAttempts).toBe(1);
    expect(ingestAttempts).toBe(1);
  });

  it('returns the durable ingest receipt when the successful response is lost', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    let statusReads = 0;
    const receipt = {
      staged_uri: 'solo-staged://upload/upload-lost-ingest',
      document_id: 'doc-lost-ingest',
      chunks_persisted: 2,
      bytes_ingested: 4,
      deduped: false,
      stored_original_file: false,
      asset: null,
      document_asset_link: null,
      extraction_status: 'extracted',
      extraction_error: null,
      deleted_staged_file: true,
      retained_source_file: false,
      idempotent_replay: false,
      ingest_completed_at_ms: 1_700_000_000_123,
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? 'GET';
        if (url.endsWith('/memory/documents/uploads') && method === 'POST') {
          return jsonResponse({
            upload_id: 'upload-lost-ingest',
            upload_url: '/uploads/upload-lost-ingest',
            upload_path: '/uploads/upload-lost-ingest',
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: {},
            max_file_bytes: 100,
            max_chunk_bytes: 100,
            recommended_chunk_bytes: 100,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: false,
          });
        }
        if (url.endsWith('/uploads/upload-lost-ingest') && method === 'PATCH') {
          return new Response(null, { status: 204 });
        }
        if (
          url.endsWith('/memory/documents/uploads/upload-lost-ingest/commit') &&
          method === 'POST'
        ) {
          return jsonResponse({
            upload_id: 'upload-lost-ingest',
            staged_uri: receipt.staged_uri,
            filename: 'lost-ingest.txt',
            mime_type: 'text/plain',
            size_bytes: 4,
            sha256: 'abc',
          });
        }
        if (url.endsWith('/memory/documents/staged/ingest') && method === 'POST') {
          throw new TypeError('connection closed after Solo persisted the ingest receipt');
        }
        if (url.endsWith('/memory/documents/uploads/upload-lost-ingest') && method === 'GET') {
          statusReads += 1;
          return jsonResponse({
            upload_id: 'upload-lost-ingest',
            status: statusReads === 1 ? 'busy' : 'ingested',
            bytes_received: 4,
            size_bytes: 4,
            next_offset: 4,
            expires_at_ms: Date.now() + 60_000,
            operation_in_progress: statusReads === 1,
            active_operation: statusReads === 1 ? 'ingest' : null,
            staged_uri: receipt.staged_uri,
            commit_result: null,
            ingest_result: statusReads === 1 ? null : receipt,
            terminal: statusReads !== 1,
          });
        }
        throw new Error(`unexpected fetch: ${method} ${url}`);
      }),
    );

    await expect(
      importBrowserDocument(new File(['data'], 'lost-ingest.txt', { type: 'text/plain' }), {}),
    ).resolves.toStrictEqual(receipt);
    expect(statusReads).toBe(2);
  });

  it('lets the user recover by upload id after every automatic commit-status poll failed', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    let userInitiatedRecovery = false;
    let patchCalls = 0;
    let commitCalls = 0;
    let statusCalls = 0;
    const receipt = {
      staged_uri: 'solo-staged://upload/upload-recover-by-id',
      document_id: 'doc-recover-by-id',
      chunks_persisted: 1,
      bytes_ingested: 4,
      deduped: false,
      stored_original_file: false,
      asset: null,
      document_asset_link: null,
      extraction_status: 'extracted' as const,
      extraction_error: null,
      deleted_staged_file: true,
      retained_source_file: false,
      idempotent_replay: false,
      ingest_completed_at_ms: 1_700_000_000_456,
    };
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        const method = init?.method ?? 'GET';
        if (url.endsWith('/memory/documents/uploads') && method === 'POST') {
          return jsonResponse({
            upload_id: 'upload-recover-by-id',
            upload_url: '/uploads/upload-recover-by-id',
            upload_path: '/uploads/upload-recover-by-id',
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: {},
            max_file_bytes: 100,
            max_chunk_bytes: 100,
            recommended_chunk_bytes: 100,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: false,
          });
        }
        if (url.endsWith('/uploads/upload-recover-by-id') && method === 'PATCH') {
          patchCalls += 1;
          return new Response(null, { status: 204 });
        }
        if (
          url.endsWith('/memory/documents/uploads/upload-recover-by-id/commit') &&
          method === 'POST'
        ) {
          commitCalls += 1;
          if (!userInitiatedRecovery) {
            throw new TypeError('commit response lost while status endpoint was offline');
          }
          return jsonResponse({
            upload_id: 'upload-recover-by-id',
            staged_uri: receipt.staged_uri,
            filename: 'recover.txt',
            mime_type: 'text/plain',
            size_bytes: 4,
            sha256: 'abc',
          });
        }
        if (url.endsWith('/memory/documents/uploads/upload-recover-by-id') && method === 'GET') {
          statusCalls += 1;
          if (!userInitiatedRecovery) {
            return new Response('status temporarily unavailable', { status: 503 });
          }
          return jsonResponse({
            upload_id: 'upload-recover-by-id',
            status: 'open',
            bytes_received: 4,
            size_bytes: 4,
            next_offset: 4,
            expires_at_ms: Date.now() + 60_000,
            operation_in_progress: false,
            active_operation: null,
            staged_uri: null,
            commit_result: null,
            ingest_result: null,
            terminal: false,
          });
        }
        if (url.endsWith('/memory/documents/staged/ingest') && method === 'POST') {
          return jsonResponse(receipt);
        }
        throw new Error(`unexpected fetch: ${method} ${url}`);
      }),
    );

    const uncertain = await importBrowserDocument(
      new File(['data'], 'recover.txt', { type: 'text/plain' }),
      {},
    ).catch((error: unknown) => error);
    expect(uncertain).toMatchObject({
      name: 'DocumentImportUncertainError',
      uploadId: 'upload-recover-by-id',
      stagedUri: null,
      phase: 'commit',
      storeOriginalFile: false,
    });
    expect(statusCalls).toBe(5);

    userInitiatedRecovery = true;
    await expect(
      resumeUncertainDocumentImport('upload-recover-by-id', null, false),
    ).resolves.toStrictEqual(receipt);
    expect(patchCalls).toBe(1);
    expect(commitCalls).toBe(2);
    expect(statusCalls).toBe(6);
  });

  it('does not blindly abort a committed upload when extraction has an uncertain outcome', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const methods: string[] = [];
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const method = init?.method ?? 'GET';
      methods.push(`${method} ${url}`);
      if (url.endsWith('/memory/documents/uploads') && method === 'POST') {
        return jsonResponse({
          upload_id: 'upload-committed',
          upload_url: '/uploads/upload-committed',
          upload_path: '/uploads/upload-committed',
          route_kind: 'direct_local',
          upload_method: 'PATCH',
          upload_offset_header: 'upload-offset',
          upload_length_header: 'upload-length',
          required_headers: {},
          max_file_bytes: 100,
          max_chunk_bytes: 100,
          recommended_chunk_bytes: 100,
          expires_at_ms: Date.now() + 60_000,
          default_store_original_file: true,
        });
      }
      if (url.endsWith('/uploads/upload-committed') && method === 'PATCH') {
        return new Response(null, { status: 204 });
      }
      if (url.endsWith('/memory/documents/uploads/upload-committed/commit') && method === 'POST') {
        return jsonResponse({
          upload_id: 'upload-committed',
          staged_uri: 'solo-staged://upload/upload-committed',
          filename: 'uncertain.txt',
          mime_type: 'text/plain',
          size_bytes: 4,
          sha256: 'abc',
        });
      }
      if (url.endsWith('/memory/documents/staged/ingest') && method === 'POST') {
        return new Response('database temporarily unavailable', { status: 503 });
      }
      throw new Error(`unexpected fetch: ${method} ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    const error = await importBrowserDocument(
      new File(['data'], 'uncertain.txt', { type: 'text/plain' }),
      {},
    ).catch((caught: unknown) => caught);

    expect(error).toBeInstanceOf(DocumentImportUncertainError);
    expect(error).toMatchObject({
      uploadId: 'upload-committed',
      stagedUri: 'solo-staged://upload/upload-committed',
      phase: 'ingest',
      storeOriginalFile: true,
    });
    expect(methods.some((call) => call.startsWith('DELETE '))).toBe(false);
  });

  it('resumes from Solo reported offset when a chunk response is lost', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const offsets: string[] = [];
    const progress: string[] = [];
    let firstPatch = true;
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.endsWith('/memory/documents/uploads') && init?.method === 'POST') {
          return jsonResponse({
            upload_id: 'upload-resume',
            upload_url: '/uploads/upload-resume',
            upload_path: '/uploads/upload-resume',
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: {},
            max_file_bytes: 100,
            max_chunk_bytes: 2,
            recommended_chunk_bytes: 2,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: false,
          });
        }
        if (url.endsWith('/uploads/upload-resume') && init?.method === 'PATCH') {
          offsets.push((init.headers as Record<string, string>)['upload-offset']);
          if (firstPatch) {
            firstPatch = false;
            throw new TypeError('connection reset after server write');
          }
          return new Response(null, { status: 204 });
        }
        if (url.endsWith('/memory/documents/uploads/upload-resume') && init?.method === 'GET') {
          return jsonResponse({
            upload_id: 'upload-resume',
            status: 'open',
            bytes_received: 2,
            size_bytes: 4,
            next_offset: 2,
            expires_at_ms: Date.now() + 60_000,
          });
        }
        if (url.endsWith('/memory/documents/uploads/upload-resume/commit')) {
          return jsonResponse({
            upload_id: 'upload-resume',
            staged_uri: 'solo-staged://upload/upload-resume',
            filename: 'resume.txt',
            mime_type: 'text/plain',
            size_bytes: 4,
            sha256: 'abc',
          });
        }
        if (url.endsWith('/memory/documents/staged/ingest')) {
          return jsonResponse({
            staged_uri: 'solo-staged://upload/upload-resume',
            document_id: 'doc-resume',
            chunks_persisted: 1,
            bytes_ingested: 4,
            deduped: false,
            stored_original_file: false,
            asset: null,
            document_asset_link: null,
            extraction_status: 'extracted',
            extraction_error: null,
            deleted_staged_file: true,
            retained_source_file: false,
          });
        }
        throw new Error(`unexpected fetch: ${init?.method ?? 'GET'} ${url}`);
      }),
    );

    await importBrowserDocument(new File(['data'], 'resume.txt', { type: 'text/plain' }), {
      onProgress: (event) => progress.push(`${event.stage}:${event.bytesSent}`),
    });

    expect(offsets).toStrictEqual(['0', '2']);
    expect(progress).toContain('resuming:2');
  });

  it('aborts staged browser uploads after a byte-transfer failure', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith('/memory/documents/uploads') && init?.method === 'POST') {
        return jsonResponse({
          upload_id: 'upload-fail',
          upload_url: '/uploads/upload-fail',
          upload_path: '/uploads/upload-fail',
          route_kind: 'direct_local',
          upload_method: 'PATCH',
          upload_offset_header: 'upload-offset',
          upload_length_header: 'upload-length',
          required_headers: {},
          max_file_bytes: 100,
          max_chunk_bytes: 100,
          recommended_chunk_bytes: 100,
          expires_at_ms: Date.now() + 60_000,
          default_store_original_file: true,
        });
      }
      if (url.endsWith('/uploads/upload-fail') && init?.method === 'PATCH') {
        return new Response('disk full', { status: 507, statusText: 'Insufficient Storage' });
      }
      if (url.endsWith('/memory/documents/uploads/upload-fail') && init?.method === 'DELETE') {
        return jsonResponse({
          upload_id: 'upload-fail',
          status: 'aborted',
          cleanup_performed: true,
          already_aborted: false,
          removed_partial_file: true,
          removed_staged_file: false,
        });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      importBrowserDocument(new File(['data'], 'pilot.txt', { type: 'text/plain' }), {
        storeOriginalFile: false,
      }),
    ).rejects.toThrow(/507 Insufficient Storage\): disk full/);
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/documents/uploads/upload-fail',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });

  it('surfaces cleanup uncertainty instead of claiming a failed upload was discarded', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (url.endsWith('/memory/documents/uploads') && init?.method === 'POST') {
          return jsonResponse({
            upload_id: 'upload-cleanup-unknown',
            upload_url: '/uploads/upload-cleanup-unknown',
            upload_path: '/uploads/upload-cleanup-unknown',
            route_kind: 'direct_local',
            upload_method: 'PATCH',
            upload_offset_header: 'upload-offset',
            upload_length_header: 'upload-length',
            required_headers: {},
            max_file_bytes: 100,
            max_chunk_bytes: 100,
            recommended_chunk_bytes: 100,
            expires_at_ms: Date.now() + 60_000,
            default_store_original_file: false,
          });
        }
        if (url.endsWith('/uploads/upload-cleanup-unknown') && init?.method === 'PATCH') {
          return new Response('disk full', { status: 507 });
        }
        if (
          url.endsWith('/memory/documents/uploads/upload-cleanup-unknown') &&
          init?.method === 'DELETE'
        ) {
          return new Response('control plane unavailable', { status: 503 });
        }
        throw new Error(`unexpected fetch: ${init?.method ?? 'GET'} ${url}`);
      }),
    );

    await expect(
      importBrowserDocument(new File(['data'], 'cleanup.txt', { type: 'text/plain' }), {}),
    ).rejects.toMatchObject({
      name: 'DocumentImportCleanupUncertainError',
      uploadId: 'upload-cleanup-unknown',
    } satisfies Partial<DocumentImportCleanupUncertainError>);
  });

  it('forgets the searchable document through the HTTP lifecycle route', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async () => jsonResponse({ doc_id: 'doc-1', chunks_tombstoned: 3 }));
    vi.stubGlobal('fetch', fetchMock);

    await expect(forgetDocument('doc-1')).resolves.toStrictEqual({
      doc_id: 'doc-1',
      chunks_tombstoned: 3,
    });
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/memory/documents/doc-1',
      expect.objectContaining({ method: 'DELETE' }),
    );
  });

  it('deletes retained source bytes through memory_forget_asset', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url !== 'http://solo.test/mcp') throw new Error(`unexpected fetch: ${url}`);
      if (init?.method === 'DELETE') return new Response(null, { status: 204 });
      const body = JSON.parse(String(init?.body));
      if (body.method === 'initialize') {
        return new Response(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }), {
          status: 200,
          headers: { 'content-type': 'application/json', 'Mcp-Session-Id': 'session-1' },
        });
      }
      if (body.method === 'notifications/initialized') return new Response(null, { status: 202 });
      if (body.method === 'tools/call') {
        expect(body.params).toStrictEqual({
          name: 'memory_forget_asset',
          arguments: { asset_id: 'asset-1' },
        });
        return jsonResponse({
          jsonrpc: '2.0',
          id: 2,
          result: {
            content: [],
            structuredContent: {
              asset_id: 'asset-1',
              blob_deleted: true,
              already_deleted: false,
              document_links: 1,
              memory_attachments: 0,
            },
          },
        });
      }
      throw new Error(`unexpected MCP request: ${body.method}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    await expect(forgetRetainedAsset('asset-1')).resolves.toMatchObject({
      asset_id: 'asset-1',
      blob_deleted: true,
    });
  });

  it('paginates the durable lifecycle catalog beyond the newest 100 documents', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const offsets: number[] = [];
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        if (String(input) !== 'http://solo.test/mcp') throw new Error('unexpected endpoint');
        if (init?.method === 'DELETE') return new Response(null, { status: 204 });
        const body = JSON.parse(String(init?.body));
        if (body.method === 'initialize') {
          return new Response(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }), {
            status: 200,
            headers: { 'content-type': 'application/json', 'Mcp-Session-Id': 'page-session' },
          });
        }
        if (body.method === 'notifications/initialized') return new Response(null, { status: 202 });
        if (body.method === 'tools/call') {
          expect(body.params.name).toBe('memory_list_documents');
          const offset = Number(body.params.arguments.offset);
          offsets.push(offset);
          const count = offset === 0 ? 100 : 1;
          return jsonResponse({
            jsonrpc: '2.0',
            id: 2,
            result: {
              structuredContent: {
                documents: Array.from({ length: count }, (_, index) => ({
                  doc_id: `doc-${offset + index}`,
                  title: `Document ${offset + index}`,
                  ingested_at_ms: 1_700_000_000_000 - offset - index,
                  chunk_count: 1,
                  status: 'active',
                })),
              },
            },
          });
        }
        throw new Error(`unexpected MCP request: ${body.method}`);
      }),
    );

    const catalog = await listDocumentLifecycle();
    expect(catalog.items).toHaveLength(101);
    expect(catalog).toMatchObject({ truncated: false, limit: 1_000 });
    expect(offsets).toStrictEqual([0, 100]);
  });

  it('runBackup posts to /backup with the force flag', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    const fetchMock = vi.fn(async () =>
      jsonResponse({ path: 'C:\\SoloData\\solo-backup.db', elapsed_ms: 12 }),
    );
    vi.stubGlobal('fetch', fetchMock);

    await expect(
      runBackup(
        {
          to: 'C:\\SoloData\\solo-backup.db',
          force: true,
        },
        {},
      ),
    ).resolves.toStrictEqual({ path: 'C:\\SoloData\\solo-backup.db', elapsed_ms: 12 });

    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/backup',
      expect.objectContaining({
        method: 'POST',
        headers: expect.objectContaining({
          Accept: 'application/json',
          'Content-Type': 'application/json',
        }),
        body: JSON.stringify({
          to: 'C:\\SoloData\\solo-backup.db',
          force: true,
        }),
      }),
    );
  });

  it('adds the tray startup hint when the default Solo endpoint is unreachable', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: DEFAULT_SOLO_API_URL,
      bearerToken: '',
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => {
        throw new TypeError('fetch failed');
      }),
    );

    await expect(fetchSoloStatus()).rejects.toThrow(
      /Start or unlock Solo from the tray/,
    );
  });

  it('fetchSoloStatus calls /v1/status with bearer auth and no library selector', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: 'secret-token',
    });
    const fetchMock = vi.fn(async () =>
      jsonResponse({
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
        mcp: {
          sessions: 0,
        },
      }),
    );
    vi.stubGlobal('fetch', fetchMock);

    await expect(fetchSoloStatus()).resolves.toMatchObject({
      ok: true,
      library: { name: 'Community Memory Library', ready: true },
    });
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/status',
      expect.objectContaining({
        headers: expect.objectContaining({
          Accept: 'application/json',
          Authorization: 'Bearer secret-token',
        }),
      }),
    );
    const headers = (fetchMock.mock.calls[0]?.[1] as RequestInit).headers;
    expect(headers).toStrictEqual({
      Accept: 'application/json',
      Authorization: 'Bearer secret-token',
    });
  });

  it('fetchLogs calls /v1/logs with a bounded tray source query', async () => {
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: 'secret-token',
    });
    const fetchMock = vi.fn(async () =>
      jsonResponse({
        source: 'tray',
        path: 'C:\\SoloData\\tray.log',
        exists: true,
        limit: 100,
        size_bytes: 12,
        modified_at_ms: 1779290000000,
        lines: [{ level: 'info', text: 'INFO ready' }],
      }),
    );
    vi.stubGlobal('fetch', fetchMock);

    await expect(fetchLogs(100)).resolves.toMatchObject({
      source: 'tray',
      lines: [{ level: 'info', text: 'INFO ready' }],
    });
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/v1/logs?source=tray&limit=100',
      expect.objectContaining({
        headers: expect.objectContaining({
          Accept: 'application/json',
          Authorization: 'Bearer secret-token',
        }),
      }),
    );
  });

});
