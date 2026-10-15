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
      // The first match in the document is not always the one the route put on
      // screen: the sidebar carries the running version too, and it is hidden
      // below 760px. Assert that the route shows the text somewhere visible.
      await expect(page.getByText(text).filter({ visible: true }).first()).toBeVisible();
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
  await page.screenshot({
    path: process.env.SOLO_SETUP_SCREENSHOT || 'test-results/setup-guide.png',
    fullPage: true,
  });
});

test('connections explains managed memory hook coverage', async ({ page }, testInfo) => {
  await page.goto('/#connections');
  await expect(
    page.getByText('Setup installs the Solo connection and supported memory hooks together.', {
      exact: false,
    }),
  ).toBeVisible();
  await expect(
    page.getByText('Automatic recall at session start and before prompts', { exact: true }),
  ).toHaveCount(3);
  await expect(
    page.getByText('Automatic recall after session start', { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText('Automatic recall at session start in Copilot and Local modes', { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText('Memory tools; automatic hooks unavailable in ordinary chat', { exact: true }),
  ).toBeVisible();
  await expect(page.getByRole('button', { name: 'Copy install', exact: true })).toHaveCount(6);
  const clientSection = page
    .locator('section')
    .filter({ has: page.getByRole('heading', { name: 'Connected Clients', exact: true }) });
  await clientSection.scrollIntoViewIfNeeded();
  const bounds = await clientSection.boundingBox();
  expect(bounds).not.toBeNull();
  expect(bounds!.x + bounds!.width).toBeLessThanOrEqual(page.viewportSize()!.width);

  await page.screenshot({ path: testInfo.outputPath('client-memory-hooks.png'), fullPage: true });
});
