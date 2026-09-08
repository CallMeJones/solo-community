/**
 * RTL tests for src/components/MemoryWorkspace.tsx.
 *
 * Scoped to the control row. The graph itself is mocked out: it is lazily
 * loaded and pulls the force-graph canvas stack, none of which this file is
 * about.
 *
 * There used to be a Toolbar.test.tsx covering the same controls. It kept
 * passing after the workspace redesign stopped rendering Toolbar at all, so a
 * control that had vanished from the app still looked tested. Rendering the
 * component the app actually mounts is the point of this file.
 */

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { MemoryWorkspace } from '../src/components/MemoryWorkspace';
import { useGraphStore } from '../src/store/graphStore';
import { useThemeStore } from '../src/store/themeStore';

vi.mock('../src/hooks/useGraphData', () => ({
  useGraphData: () => ({
    data: {
      nodes: [
        { id: 'ep:1', kind: 'episode', label: 'Memory one' },
        { id: 'doc:1', kind: 'document', label: 'Doc one' },
      ],
      edges: [],
    },
    isError: false,
    isFetching: false,
    dataUpdatedAt: Date.now(),
  }),
}));

vi.mock('../src/components/GraphView', () => ({
  GraphView: () => <div data-testid="graph-view" />,
}));

vi.mock('../src/components/InspectorPanel', () => ({
  InspectorPanel: () => <div data-testid="inspector" />,
}));

function wrap(node: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  return <QueryClientProvider client={client}>{node}</QueryClientProvider>;
}

function renderWorkspace() {
  return render(wrap(<MemoryWorkspace onImport={() => undefined} />));
}

/** The workspace opens on the list; the switch only exists in the 2D graph. */
async function showGraph2d() {
  fireEvent.click(screen.getByRole('button', { name: '2D' }));
  return waitFor(() => screen.getByLabelText('Labels'));
}

describe('MemoryWorkspace label switch', () => {
  beforeEach(() => {
    useGraphStore.setState({
      selectedNodeId: null,
      viewMode: '2d',
      visibleKinds: new Set(['episode', 'document', 'cluster', 'entity']),
      searchQuery: '',
      expandedNodeIds: new Set(),
      recalledNodeIds: new Set(),
    });
    localStorage.clear();
    useThemeStore.setState({ labels: true });
  });

  it('turns 2D node labels off and remembers the choice', async () => {
    renderWorkspace();
    const checkbox = (await showGraph2d()) as HTMLInputElement;
    expect(checkbox.checked).toBe(true);

    fireEvent.click(checkbox);

    expect(useThemeStore.getState().labels).toBe(false);
    // Persisted, so the graph comes back unlabelled after a reload.
    expect(localStorage.getItem('solo.graph.labels')).toBe('0');
  });

  it('turns them back on again', async () => {
    useThemeStore.setState({ labels: false });
    renderWorkspace();
    const checkbox = (await showGraph2d()) as HTMLInputElement;
    expect(checkbox.checked).toBe(false);

    fireEvent.click(checkbox);

    expect(useThemeStore.getState().labels).toBe(true);
    expect(localStorage.getItem('solo.graph.labels')).toBe('1');
  });

  it('is absent from the list, which is already text', () => {
    renderWorkspace();
    expect(screen.queryByLabelText('Labels')).not.toBeInTheDocument();
  });

  it('is absent from 3D, which paints no labels to hide', async () => {
    renderWorkspace();
    await showGraph2d();
    fireEvent.click(screen.getByRole('button', { name: '3D' }));
    await waitFor(() => {
      expect(screen.queryByLabelText('Labels')).not.toBeInTheDocument();
    });
  });
});
