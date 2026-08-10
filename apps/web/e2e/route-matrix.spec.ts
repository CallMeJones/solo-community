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

for (const hash of ['unknown']) {
  test(`returns an unknown Community route to Home for #${hash}`, async ({ page }) => {
    await page.goto(`/#${hash}`);

    await expect(page.getByRole('heading', { name: 'Home' })).toBeVisible();
  });
}
