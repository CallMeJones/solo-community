// Global test setup.
//
//   1. Loads `@testing-library/jest-dom/vitest` so matchers like
//      `toBeInTheDocument()` are available on every `expect(...)`.
//   2. Runs RTL's `cleanup` between tests so previous renders don't
//      leak into the next test's DOM (vitest doesn't auto-cleanup
//      the way jest with `testEnvironment: 'jsdom'` does).
//   3. Clears localStorage between cases so settingsStore migrations
//      don't bleed state.

import '@testing-library/jest-dom/vitest';

import { cleanup } from '@testing-library/react';
import { afterEach, beforeEach, vi } from 'vitest';

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.unstubAllEnvs();
  localStorage.clear();
});
