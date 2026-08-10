/**
 * RTL tests for src/components/Toolbar.tsx.
 *
 * Community exposes one Memory Library, so the toolbar has no database
 * picker or routing controls.
 */

import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen } from '@testing-library/react';
import type { ReactNode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { Toolbar } from '../src/components/Toolbar';
import { useGraphStore } from '../src/store/graphStore';

function wrap(node: ReactNode) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 } },
  });
  return <QueryClientProvider client={client}>{node}</QueryClientProvider>;
}

describe('Toolbar', () => {
  beforeEach(() => {
    useGraphStore.setState({
      selectedNodeId: null,
      viewMode: '2d',
      visibleKinds: new Set(['episode', 'document', 'cluster', 'entity']),
      searchQuery: '',
      expandedNodeIds: new Set(),
      recalledNodeIds: new Set(),
    });
  });

  afterEach(() => {
    vi.clearAllMocks();
  });

  it('renders memory branding and the kind filter checkboxes', () => {
    render(wrap(<Toolbar />));
    expect(screen.getByText('Memories')).toBeInTheDocument();
    for (const kind of ['episode', 'document', 'chunk', 'cluster', 'entity']) {
      expect(screen.getByText(kind)).toBeInTheDocument();
    }
  });

  it('toggles a kind in the store when its checkbox is clicked', () => {
    render(wrap(<Toolbar />));
    expect(useGraphStore.getState().visibleKinds.has('episode')).toBe(true);
    const episodeCheckbox = screen
      .getByText('episode')
      .closest('label')
      ?.querySelector('input[type="checkbox"]') as HTMLInputElement;
    expect(episodeCheckbox).toBeInTheDocument();
    fireEvent.click(episodeCheckbox);
    expect(useGraphStore.getState().visibleKinds.has('episode')).toBe(false);
  });

  it('switches view mode when 2D/3D is clicked', () => {
    render(wrap(<Toolbar />));
    expect(useGraphStore.getState().viewMode).toBe('2d');
    fireEvent.click(screen.getByRole('button', { name: '3D' }));
    expect(useGraphStore.getState().viewMode).toBe('3d');
    fireEvent.click(screen.getByRole('button', { name: '2D' }));
    expect(useGraphStore.getState().viewMode).toBe('2d');
  });

  it('writes searchQuery to the store as the user types', () => {
    render(wrap(<Toolbar />));
    const search = screen.getByPlaceholderText(/search nodes/i);
    fireEvent.change(search, { target: { value: 'sam berlin' } });
    expect(useGraphStore.getState().searchQuery).toBe('sam berlin');
  });

  it('reset clears searchQuery + selectedNodeId', () => {
    useGraphStore.setState({ searchQuery: 'pre-existing', selectedNodeId: 'ep:foo' });
    render(wrap(<Toolbar />));
    fireEvent.click(screen.getByRole('button', { name: /^reset$/i }));
    expect(useGraphStore.getState().searchQuery).toBe('');
    expect(useGraphStore.getState().selectedNodeId).toBeNull();
  });

  it('opens the settings dialog when the gear icon is clicked', () => {
    render(wrap(<Toolbar />));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: /^settings$/i }));
    expect(screen.getByRole('dialog')).toBeInTheDocument();
  });

  it('shows one active Memory Library without a database picker', () => {
    render(wrap(<Toolbar />));
    expect(screen.getByText('Community Memory Library')).toBeInTheDocument();
    expect(screen.queryByRole('combobox')).not.toBeInTheDocument();
  });
});
