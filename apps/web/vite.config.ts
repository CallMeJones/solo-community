import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],
  base: './',
  build: {
    // 3D graph mode pulls Three.js as a lazy, non-initial chunk. Keep the
    // build warning focused on eager bundle regressions.
    chunkSizeWarningLimit: 1300,
    rollupOptions: {
      output: {
        manualChunks(id) {
          const normalized = id.replace(/\\/g, '/');
          if (!normalized.includes('/node_modules/')) return undefined;

          if (normalized.includes('/node_modules/three/')) {
            if (normalized.includes('/examples/')) return 'three-examples';
            if (normalized.includes('/src/renderers/')) return 'three-renderers';
            if (normalized.includes('/src/materials/')) return 'three-materials';
            if (normalized.includes('/src/geometries/')) return 'three-geometries';
            if (normalized.includes('/src/math/')) return 'three-math';
            if (normalized.includes('/src/objects/')) return 'three-objects';
            return 'three-core';
          }

          if (normalized.includes('/node_modules/react-force-graph-3d/')) return 'graph-3d';
          if (normalized.includes('/node_modules/3d-force-graph/')) return 'graph-3d-core';
          if (normalized.includes('/node_modules/react-force-graph-2d/')) return 'graph-2d';
          if (normalized.includes('/node_modules/force-graph/')) return 'graph-2d-core';
          if (normalized.includes('/node_modules/@tanstack/')) return 'tanstack';
          if (normalized.includes('/node_modules/react')) return 'react';
          return undefined;
        },
      },
    },
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  server: {
    port: 5173,
    strictPort: false,
  },
});
