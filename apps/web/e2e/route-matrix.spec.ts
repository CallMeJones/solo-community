import { expect, test } from '@playwright/test';
import {
  assertNoRuntimeIssues,
  installRuntimeIssueTracking,
  installSoloServiceMocks,
} from './solo-test-harness';
import { ROUTES } from './route-cases';

test.beforeEach(async ({ page }) => {
  installRuntimeIssueTracking(page);
  await installSoloServiceMocks(page);
});

test.afterEach(async ({ page }) => {
  assertNoRuntimeIssues(page);
});

for (const routeCase of ROUTES) {
  test(`renders #${routeCase.hash}`, async ({ page }) => {
    await page.goto(`/#${routeCase.hash}`);

    for (const text of routeCase.texts) {
      await expect(page.getByText(text).first()).toBeVisible();
    }
  });
}

// Unknown routes land on Memories, not Home: the unified workspace made the
// memory library the startup surface.
for (const hash of ['unknown']) {
  test(`returns an unknown Community route to Memories for #${hash}`, async ({ page }) => {
    await page.goto(`/#${hash}`);

    await expect(page.getByRole('heading', { name: 'Memories' })).toBeVisible();
  });
}

// One session cannot establish that two distinct assistants are connected.
test('setup explains cross-client recall without claiming verified clients', async ({ page }) => {
  await page.goto('/#setup');
  await expect(page.getByText('verify in client')).toHaveCount(2);
  await expect(page.getByText(/Active sessions do not identify/)).toBeVisible();
  await expect(page.getByText(/Return to Codex and ask again/)).toBeVisible();
  await page.screenshot({ path: process.env.SOLO_SETUP_SCREENSHOT || 'test-results/setup-guide.png', fullPage: true });
});
