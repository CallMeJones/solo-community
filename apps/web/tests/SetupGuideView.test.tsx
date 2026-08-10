import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen } from '@testing-library/react';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { SetupGuideView } from '../src/components/SetupGuideView';
import { useGraphStore } from '../src/store/graphStore';
import { useSettingsStore } from '../src/store/settingsStore';

const mocks = vi.hoisted(() => ({
  fetchInbox: vi.fn(),
  fetchSoloStatus: vi.fn(),
  useGraphData: vi.fn(),
}));

vi.mock('../src/api/client', () => ({
  errorMessage: (error: unknown) => (error instanceof Error ? error.message : String(error)),
  fetchInbox: mocks.fetchInbox,
}));

vi.mock('../src/api/health', () => ({
  fetchSoloStatus: mocks.fetchSoloStatus,
}));

vi.mock('../src/hooks/useGraphData', () => ({
  useGraphData: mocks.useGraphData,
}));

function wrap(node: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  return <QueryClientProvider client={client}>{node}</QueryClientProvider>;
}

function setupOkState() {
  mocks.fetchSoloStatus.mockResolvedValue({
    ok: true,
    version: '0.11.9',
    build: { version: '0.11.9', version_with_build: '0.11.9' },
    library: { name: 'Community Memory Library', ready: true },
    embedder: { name: 'stub', version: 'v1', dim: 16, dtype: 'f32' },
    mcp: { sessions: 1 },
    runtime: { data_dir: 'C:\\SoloData' },
  });
  mocks.fetchInbox.mockResolvedValue([
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
  ]);
  mocks.useGraphData.mockReturnValue({
    data: {
      nodes: [
        { id: 'ep:1', kind: 'episode', label: 'Memory' },
        { id: 'doc:1', kind: 'document', label: 'Doc' },
      ],
      edges: [],
    },
    isError: false,
    error: null,
  });
}

describe('SetupGuideView', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
    useGraphStore.setState({
      selectedNodeId: null,
      searchQuery: '',
      expandedNodeIds: new Set(),
      recalledNodeIds: new Set(),
    });
    setupOkState();
  });

  it('summarizes first-run readiness and routes to the next product surface', async () => {
    const onModeChange = vi.fn();
    render(wrap(<SetupGuideView onModeChange={onModeChange} />));

    expect(await screen.findByText('5 of 6 complete')).toBeInTheDocument();
    expect(screen.getAllByText('Community Memory Library').length).toBeGreaterThan(0);
    expect(screen.getByText('Start Solo')).toBeInTheDocument();
    expect(screen.getAllByText('Memory library').length).toBeGreaterThan(0);
    expect(screen.getByText('one private local library')).toBeInTheDocument();
    expect(screen.getByText('Connect tools')).toBeInTheDocument();
    expect(screen.getByText('Import memory')).toBeInTheDocument();
    expect(screen.getByText('Review inbox')).toBeInTheDocument();
    expect(screen.getByText('Create backup')).toBeInTheDocument();
    expect(screen.getByText('1 sessions')).toBeInTheDocument();
    expect(screen.getByText('1 docs')).toBeInTheDocument();
    expect(screen.getAllByText('1 reviewed').length).toBeGreaterThan(0);
    expect(screen.queryByText('Open health')).not.toBeInTheDocument();
    expect(screen.queryByText('Open connections')).not.toBeInTheDocument();
    expect(screen.queryByText('Open backups')).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /connect tools/i }));
    expect(onModeChange).toHaveBeenCalledWith('settings');
  });

  it('shows an actionable offline state without marking blocked steps complete', async () => {
    mocks.fetchSoloStatus.mockRejectedValue(new Error('daemon locked'));
    mocks.fetchInbox.mockRejectedValue(new Error('inbox unavailable'));
    mocks.useGraphData.mockReturnValue({
      data: undefined,
      isError: true,
      error: new Error('graph unavailable'),
    });

    render(wrap(<SetupGuideView onModeChange={vi.fn()} />));

    expect(await screen.findByText('daemon locked')).toBeInTheDocument();
    expect(screen.getByText('0 of 6 complete')).toBeInTheDocument();
    expect(screen.getAllByText('Memory library').length).toBeGreaterThan(0);
    expect(screen.getAllByText('offline').length).toBeGreaterThan(0);
    expect(screen.getByText('Open settings')).toBeInTheDocument();
  });
});
