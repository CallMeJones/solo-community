import AxeBuilder from '@axe-core/playwright';
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
  test(`has no serious accessibility violations on #${routeCase.hash}`, async ({ page }) => {
    await page.goto(`/#${routeCase.hash}`);

    for (const text of routeCase.texts) {
      await expect(page.getByText(text).first()).toBeVisible();
    }

    const results = await new AxeBuilder({ page }).analyze();
    const blockingViolations = results.violations.filter((violation) =>
      violation.impact === 'critical' || violation.impact === 'serious',
    );

    expect(blockingViolations).toEqual([]);
  });
}
