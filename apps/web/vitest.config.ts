import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

// jsdom for localStorage / fetch shims used by settingsStore, agentClient,
// useGraphStream tests. React plugin so JSX inside test files compiles.
export default defineConfig({
  plugins: [react()],
  test: {
    environment: 'jsdom',
    globals: false,
    include: ['tests/**/*.test.ts', 'tests/**/*.test.tsx'],
    setupFiles: ['./tests/setup.ts'],
  },
});
