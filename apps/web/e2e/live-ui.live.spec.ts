import { expect, test } from '@playwright/test';
import { assertNoRuntimeIssues, installRuntimeIssueTracking } from './solo-test-harness';

const SOLO_API_URL = process.env.SOLO_API_URL;
const SOLO_BEARER = process.env.SOLO_BEARER ?? '';

test.describe('live Solo UI smoke', () => {
  test.skip(!SOLO_API_URL, 'Set SOLO_API_URL to run live UI checks without browser mocks.');

  test.beforeEach(async ({ page }) => {
    installRuntimeIssueTracking(page);
    await page.addInitScript(
      (settings) => {
        window.localStorage.setItem('solo.settings', JSON.stringify(settings));
      },
      {
        apiUrl: SOLO_API_URL,
        bearerToken: SOLO_BEARER,
      },
    );
  });

  test.afterEach(async ({ page }) => {
    assertNoRuntimeIssues(page);
  });

  test('renders core routes against a real Solo daemon', async ({ page }) => {
    await page.goto('/#health');

    await expect(page.getByRole('heading', { name: 'Health' })).toBeVisible();
    await expect(page.getByText('Daemon State')).toBeVisible();
    await expect(page.getByText('Community Memory Library').first()).toBeVisible();

    await page.getByRole('button', { name: 'Open logs' }).click();
    await expect(page).toHaveURL(/#logs$/);
    await expect(page.getByRole('heading', { name: 'Logs' })).toBeVisible();

    await page.goto('/#connections');
    await expect(page.getByRole('heading', { name: 'Connections' })).toBeVisible();
    await expect(page.getByText('Solo MCP').first()).toBeVisible();

    await page.getByRole('button', { name: 'Probe MCP' }).click();
    await expect(page.getByText('Read-only call').first()).toBeVisible();
    await expect(page.getByText(/memory_context passed|skipped:/).first()).toBeVisible();
  });
});
