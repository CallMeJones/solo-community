/**
 * Tests for src/store/graphStore.ts — recalled / expanded sets, reset
 * cascades, and kind toggles.
 */

import { beforeEach, describe, expect, it } from 'vitest';
import { useGraphStore } from '../src/store/graphStore';

const DEFAULT_KINDS = ['episode', 'document', 'cluster', 'entity'] as const;

describe('graphStore', () => {
  beforeEach(() => {
    // Reset to a known state. The store doesn't expose a `reset()` action;
    // we set the relevant fields by name.
    useGraphStore.setState({
      selectedNodeId: null,
      viewMode: '2d',
      visibleKinds: new Set(DEFAULT_KINDS),
      searchQuery: '',
      expandedNodeIds: new Set(),
      recalledNodeIds: new Set(),
    });
  });

  describe('default state', () => {
    it('has chunk hidden by default', () => {
      const kinds = useGraphStore.getState().visibleKinds;
      expect(kinds.has('episode')).toBe(true);
      expect(kinds.has('chunk')).toBe(false);
    });
  });

  describe('toggleKind', () => {
    it('flips a kind on then off', () => {
      const store = useGraphStore.getState();
      expect(store.visibleKinds.has('chunk')).toBe(false);
      store.toggleKind('chunk');
      expect(useGraphStore.getState().visibleKinds.has('chunk')).toBe(true);
      useGraphStore.getState().toggleKind('chunk');
      expect(useGraphStore.getState().visibleKinds.has('chunk')).toBe(false);
    });

    it('does not mutate the previous set (immutable update)', () => {
      const before = useGraphStore.getState().visibleKinds;
      useGraphStore.getState().toggleKind('chunk');
      const after = useGraphStore.getState().visibleKinds;
      expect(before).not.toBe(after);
      // `before` still reflects the pre-toggle state.
      expect(before.has('chunk')).toBe(false);
    });
  });

  describe('toggleExpansion', () => {
    it('adds then removes a node id', () => {
      useGraphStore.getState().toggleExpansion('ep:abc');
      expect(useGraphStore.getState().expandedNodeIds.has('ep:abc')).toBe(true);
      useGraphStore.getState().toggleExpansion('ep:abc');
      expect(useGraphStore.getState().expandedNodeIds.has('ep:abc')).toBe(false);
    });

    it('handles multiple distinct ids', () => {
      useGraphStore.getState().toggleExpansion('ep:a');
      useGraphStore.getState().toggleExpansion('cl:b');
      const expanded = useGraphStore.getState().expandedNodeIds;
      expect(expanded.size).toBe(2);
      expect(expanded.has('ep:a')).toBe(true);
      expect(expanded.has('cl:b')).toBe(true);
    });
  });

  describe('clearExpansions', () => {
    it('empties the set', () => {
      useGraphStore.getState().toggleExpansion('ep:a');
      useGraphStore.getState().toggleExpansion('cl:b');
      useGraphStore.getState().clearExpansions();
      expect(useGraphStore.getState().expandedNodeIds.size).toBe(0);
    });
  });

  describe('addRecalled / clearRecalled', () => {
    it('addRecalled folds in new ids while preserving existing', () => {
      useGraphStore.getState().addRecalled(['ep:1', 'ep:2']);
      useGraphStore.getState().addRecalled(['ep:2', 'ep:3']);
      const recalled = useGraphStore.getState().recalledNodeIds;
      expect(recalled.size).toBe(3);
      expect(recalled.has('ep:1')).toBe(true);
      expect(recalled.has('ep:2')).toBe(true);
      expect(recalled.has('ep:3')).toBe(true);
    });

    it('addRecalled accepts any Iterable', () => {
      const ids = new Set(['ep:x', 'ep:y']);
      useGraphStore.getState().addRecalled(ids);
      expect(useGraphStore.getState().recalledNodeIds.size).toBe(2);
    });

    it('clearRecalled empties the set', () => {
      useGraphStore.getState().addRecalled(['ep:1', 'ep:2']);
      useGraphStore.getState().clearRecalled();
      expect(useGraphStore.getState().recalledNodeIds.size).toBe(0);
    });

    it('addRecalled returns a NEW set (immutable update)', () => {
      useGraphStore.getState().addRecalled(['ep:1']);
      const a = useGraphStore.getState().recalledNodeIds;
      useGraphStore.getState().addRecalled(['ep:2']);
      const b = useGraphStore.getState().recalledNodeIds;
      expect(a).not.toBe(b);
      expect(a.size).toBe(1);
      expect(b.size).toBe(2);
    });
  });

  describe('setSelectedNodeId', () => {
    it('round-trips an id and accepts null to clear', () => {
      useGraphStore.getState().setSelectedNodeId('ep:abc');
      expect(useGraphStore.getState().selectedNodeId).toBe('ep:abc');
      useGraphStore.getState().setSelectedNodeId(null);
      expect(useGraphStore.getState().selectedNodeId).toBeNull();
    });
  });
});
