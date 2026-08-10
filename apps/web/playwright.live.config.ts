import { defineConfig, devices } from '@playwright/test';

const liveSoloUrl = process.env.SOLO_API_URL ?? 'http://127.0.0.1:17821';

export default defineConfig({
  testDir: './e2e',
  testMatch: '**/*.live.spec.ts',
  timeout: 45_000,
  expect: {
    timeout: 8_000,
  },
  fullyParallel: false,
  reporter: [['list'], ['html', { open: 'never', outputFolder: 'playwright-report-live' }]],
  use: {
    baseURL: 'http://127.0.0.1:4174',
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
  projects: [
    {
      name: 'chromium-live-desktop',
      use: {
        ...devices['Desktop Chrome'],
        viewport: { width: 1440, height: 1000 },
      },
    },
  ],
  webServer: {
    command: 'npm run dev -- --host 127.0.0.1 --port 4174',
    url: 'http://127.0.0.1:4174',
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
    env: {
      VITE_SOLO_API_URL: liveSoloUrl,
    },
  },
});
