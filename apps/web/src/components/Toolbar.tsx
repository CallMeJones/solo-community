// Top toolbar: library context, 2D/3D toggle, kind filters, search box.

import { useState } from 'react';
import { NODE_KINDS, NODE_KIND_COLORS } from '../lib/nodeKindTheme';
import { COMMUNITY_LIBRARY_NAME, useGraphStore } from '../store/graphStore';
import { SettingsDialog } from './SettingsDialog';
import { Button } from './ui/Button';
import { Input } from './ui/Input';

export function Toolbar() {
  const viewMode = useGraphStore((s) => s.viewMode);
  const setViewMode = useGraphStore((s) => s.setViewMode);
  const visibleKinds = useGraphStore((s) => s.visibleKinds);
  const toggleKind = useGraphStore((s) => s.toggleKind);
  const searchQuery = useGraphStore((s) => s.searchQuery);
  const setSearchQuery = useGraphStore((s) => s.setSearchQuery);
  const [settingsOpen, setSettingsOpen] = useState(false);

  return (
    <header className="flex items-center gap-4 border-b border-slate-800 bg-slate-900/70 px-4 py-2 text-sm">
      <span className="font-semibold tracking-tight text-slate-100">Memories</span>

      <span className="rounded-md border border-slate-700 bg-slate-900 px-2 py-1 text-xs text-slate-300">
        <span className="font-medium text-slate-100">{COMMUNITY_LIBRARY_NAME}</span>
      </span>

      {/* 2D / 3D toggle */}
      <div className="flex overflow-hidden rounded-md border border-slate-700">
        <button
          onClick={() => setViewMode('2d')}
          className={`px-3 py-1 text-xs font-medium ${
            viewMode === '2d'
              ? 'bg-sky-700 text-white'
              : 'bg-slate-900 text-slate-300 hover:bg-slate-800'
          }`}
        >
          2D
        </button>
        <button
          onClick={() => setViewMode('3d')}
          className={`px-3 py-1 text-xs font-medium ${
            viewMode === '3d'
              ? 'bg-sky-700 text-white'
              : 'bg-slate-900 text-slate-300 hover:bg-slate-800'
          }`}
        >
          3D
        </button>
      </div>

      {/* Kind filters */}
      <div className="flex items-center gap-2">
        {NODE_KINDS.map((kind) => (
          <label
            key={kind}
            className="flex cursor-pointer items-center gap-1.5 rounded-md border border-slate-700 bg-slate-900 px-2 py-1 text-xs text-slate-300 hover:border-slate-600"
            title={`Toggle ${kind} nodes`}
          >
            <input
              type="checkbox"
              checked={visibleKinds.has(kind)}
              onChange={() => toggleKind(kind)}
              className="h-3 w-3 accent-sky-500"
            />
            <span
              className="inline-block h-2 w-2 rounded-full"
              style={{ backgroundColor: NODE_KIND_COLORS[kind] }}
            />
            <span>{kind}</span>
          </label>
        ))}
      </div>

      {/* Search */}
      <Input
        type="text"
        placeholder="Search nodes..."
        value={searchQuery}
        onChange={(e) => setSearchQuery(e.target.value)}
        className="w-56"
      />

      <div className="flex-1" />

      <Button
        variant="ghost"
        onClick={() => {
          useGraphStore.setState({ searchQuery: '', selectedNodeId: null });
        }}
      >
        Reset
      </Button>
      <button
        type="button"
        onClick={() => setSettingsOpen(true)}
        className="rounded-md border border-slate-700 bg-slate-900 px-2 py-1 text-xs text-slate-300 hover:border-slate-600 hover:bg-slate-800"
        title="Settings"
        aria-label="Settings"
      >
        ⚙
      </button>
      <SettingsDialog open={settingsOpen} onClose={() => setSettingsOpen(false)} />
    </header>
  );
}
