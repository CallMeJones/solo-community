import { defineConfig } from '@playwright/test';
import baseConfig from './playwright.config';

export default defineConfig(baseConfig, {
  use: {
    baseURL: 'http://127.0.0.1:4175',
  },
  webServer: {
    command: 'npm run build:pilot && npm run preview -- --host 127.0.0.1 --port 4175',
    url: 'http://127.0.0.1:4175',
    reuseExistingServer: false,
    timeout: 180_000,
    env: {
      VITE_SOLO_USE_MOCKS: '0',
    },
  },
});
