import { expect, test } from '@playwright/test';
import { ROUTES } from './route-cases';
import {
  assertNoRuntimeIssues,
  installRuntimeIssueTracking,
  installSoloServiceMocks,
} from './solo-test-harness';

test.beforeEach(async ({ page }) => {
  installRuntimeIssueTracking(page);
  await installSoloServiceMocks(page);
});

test.afterEach(async ({ page }) => {
  assertNoRuntimeIssues(page);
});

for (const routeCase of ROUTES) {
  test(`matches visual layout for #${routeCase.hash}`, async ({ page }) => {
    await page.goto(`/#${routeCase.hash}`);

    for (const text of routeCase.texts) {
      await expect(page.getByText(text).first()).toBeVisible();
    }

    const overflow = await page.evaluate(() => ({
      documentOverflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
      bodyOverflow: document.body.scrollWidth - document.body.clientWidth,
    }));

    expect(Math.max(overflow.documentOverflow, overflow.bodyOverflow)).toBeLessThanOrEqual(2);
    await expect(page).toHaveScreenshot(`${routeCase.hash}.png`, {
      animations: 'disabled',
      fullPage: true,
      mask: [page.locator('canvas')],
      maxDiffPixelRatio: 0.01,
    });
  });
}
