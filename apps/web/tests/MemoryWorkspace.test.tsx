/**
 * RTL tests for src/components/MemoryWorkspace.tsx.
 *
 * Scoped to the control row and the filters panel. The graph itself is mocked
 * out: it is lazily loaded and pulls the force-graph canvas stack, none of
 * which this file is about.
 *
 * There used to be a Toolbar.test.tsx covering the same controls. It kept
 * passing after the workspace redesign stopped rendering Toolbar at all, so a
 * control that had vanished from the app still looked tested. Rendering the
 * component the app actually mounts is the point of this file.
 */

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen } from '@testing-library/react';
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

/** The panel is collapsed until asked for. */
function openFilters() {
  fireEvent.click(screen.getByRole('button', { name: /^Filters/ }));
  return screen.getByLabelText('Graph labels') as HTMLInputElement;
}

describe('MemoryWorkspace graph label switch', () => {
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

  it('lives in the filters panel, not the control row', () => {
    renderWorkspace();
    expect(screen.queryByLabelText('Graph labels')).not.toBeInTheDocument();
    expect(openFilters()).toBeInTheDocument();
  });

  it('turns graph labels off and remembers the choice', () => {
    renderWorkspace();
    const checkbox = openFilters();
    expect(checkbox.checked).toBe(true);

    fireEvent.click(checkbox);

    expect(useThemeStore.getState().labels).toBe(false);
    // Persisted, so the graph comes back unlabelled after a reload.
    expect(localStorage.getItem('solo.graph.labels')).toBe('0');
  });

  it('turns them back on again', () => {
    useThemeStore.setState({ labels: false });
    renderWorkspace();
    const checkbox = openFilters();
    expect(checkbox.checked).toBe(false);

    fireEvent.click(checkbox);

    expect(useThemeStore.getState().labels).toBe(true);
    expect(localStorage.getItem('solo.graph.labels')).toBe('1');
  });

  it('stays reachable from the list view, where the graph is one click away', () => {
    renderWorkspace();
    expect(openFilters()).toBeInTheDocument();
  });
});

/**
 * The grouped overview is the default, and the escape hatch out of it lived
 * only in the breadcrumb row beside the item count — easy to miss when what
 * you want is simply "show me everything". It is now also a filter switch.
 */
describe('MemoryWorkspace show-every-memory switch', () => {
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
  });

  const openShowEvery = () => {
    fireEvent.click(screen.getByRole('button', { name: /^Filters/ }));
    return screen.getByLabelText('Show every memory') as HTMLInputElement;
  };

  /**
   * The breadcrumb button is hidden in list view, where there is no graph to
   * ungroup. The filter switch is not — same as the labels switch beside it.
   */
  const showGraph = () => fireEvent.click(screen.getByRole('button', { name: '2D' }));

  it('lives in the filters panel, not the control row', () => {
    renderWorkspace();
    expect(screen.queryByLabelText('Show every memory')).not.toBeInTheDocument();
    expect(openShowEvery()).toBeInTheDocument();
  });

  it('starts grouped and switches to every memory', () => {
    renderWorkspace();
    showGraph();
    const checkbox = openShowEvery();
    expect(checkbox.checked).toBe(false);
    // The breadcrumb offers the same thing under the name it has always had.
    expect(screen.getByRole('button', { name: 'Full graph' })).toBeInTheDocument();

    fireEvent.click(checkbox);

    expect((screen.getByLabelText('Show every memory') as HTMLInputElement).checked).toBe(true);
    expect(screen.getByRole('button', { name: 'Grouped overview' })).toBeInTheDocument();
  });

  it('agrees with the breadcrumb button, which drives the same state', () => {
    renderWorkspace();
    showGraph();
    expect(openShowEvery().checked).toBe(false);

    fireEvent.click(screen.getByRole('button', { name: 'Full graph' }));

    expect((screen.getByLabelText('Show every memory') as HTMLInputElement).checked).toBe(true);
  });

  it('is left alone by Clear filters, which is about what is shown, not how', () => {
    renderWorkspace();
    fireEvent.click(openShowEvery());

    fireEvent.click(screen.getByRole('button', { name: 'Clear filters' }));

    expect((screen.getByLabelText('Show every memory') as HTMLInputElement).checked).toBe(true);
  });
});
