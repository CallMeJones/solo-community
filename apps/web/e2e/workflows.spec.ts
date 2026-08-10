import { expect, test } from '@playwright/test';
import {
  assertNoRuntimeIssues,
  createSoloMockState,
  installRuntimeIssueTracking,
  installSoloServiceMocks,
  type SoloMockState,
} from './solo-test-harness';

let state: SoloMockState;

test.beforeEach(async ({ page }) => {
  state = createSoloMockState();
  installRuntimeIssueTracking(page);
  await installSoloServiceMocks(page, state);
});

test.afterEach(async ({ page }) => {
  assertNoRuntimeIssues(page);
});

test('settings editor saves endpoints and navigates through quick checks', async ({ page }) => {
  await page.goto('/#settings');

  await page.getByRole('button', { name: 'Edit settings' }).click();
  await expect(page.getByRole('dialog', { name: 'Settings' })).toBeVisible();

  await page.getByLabel('Solo API URL').fill('http://127.0.0.1:17821/');
  await page.getByLabel('Bearer token (Solo HTTP auth)').fill('workflow-secret');
  await expect(page.getByLabel('Chat backend URL')).toHaveCount(0);
  await page.getByRole('button', { name: 'Save', exact: true }).click();

  await expect(page.getByRole('dialog', { name: 'Settings' })).toBeHidden();
  await expect(page.getByText('bearer token active')).toBeVisible();
  await expect(page.getByText('http://127.0.0.1:17821').first()).toBeVisible();
  await expect(page.getByText('endpoints persist; bearer is session-only')).toBeVisible();

  await page.getByRole('button', { name: 'MCP connections' }).click();
  await expect(page).toHaveURL(/#connections$/);
  await expect(page.getByRole('heading', { name: 'Connections' })).toBeVisible();
});

test('connections probe completes a read-only MCP tool call', async ({ page }) => {
  await page.goto('/#connections');

  await page.getByRole('button', { name: 'Probe MCP' }).click();

  await expect(page.getByText('memory_context passed').first()).toBeVisible();
  await expect(page.getByText('Required tools').first()).toBeVisible();
  await expect(page.getByText('present').first()).toBeVisible();
  const mcpMethods = state.mcpRequests.map((request) => (request as { method?: string }).method);
  expect(mcpMethods).toEqual(
    expect.arrayContaining(['initialize', 'notifications/initialized', 'tools/list', 'tools/call']),
  );
});

test('inbox filters, bulk approves, and resets visible review state', async ({ page }) => {
  await page.goto('/#inbox');

  await expect(page.getByText('Review queue route matrix memory')).toBeVisible();
  await expect(page.getByText('Already approved workflow memory')).toBeVisible();

  await page.getByLabel('Review filter').selectOption('needs_review');
  await expect(page.getByText('Already approved workflow memory')).toBeHidden();
  await expect(page.getByText('1 of 2')).toBeVisible();

  await page.getByRole('button', { name: 'Approve visible' }).click();
  await expect(page.getByText('No memories match the current filters.')).toBeVisible();
  expect(
    state.inboxItems.find((item) => item.memory_id === 'route-matrix-memory')?.review_state,
  ).toBe('approved');

  await page.getByLabel('Review filter').selectOption('all');
  await expect(page.getByText('2 of 2')).toBeVisible();
  await page.getByRole('button', { name: 'Reset visible' }).click();

  await expect(page.getByText('needs review').first()).toBeVisible();
  await expect.poll(() => state.inboxItems.every((item) => item.review_state === null)).toBe(true);
});

test('inbox edits, forgets, and resolves contradiction signals', async ({ page }) => {
  await page.goto('/#inbox');

  await page.getByRole('button', { name: 'Edit Review queue route matrix memory' }).click();
  const editor = page.getByLabel('Edit text for Review queue route matrix memory');
  await expect(editor).toHaveValue('Full text for Review queue route matrix memory');

  await editor.fill('Corrected inbox memory from Playwright');
  await page.getByRole('button', { name: 'Save' }).click();
  await expect(page.getByText('Saved')).toBeVisible();

  await page.getByRole('button', { name: 'Resolve' }).click();
  await expect(page.getByRole('button', { name: 'Resolved' })).toBeDisabled();
  await expect.poll(() => state.contradictions[0]?.status).toBe('resolved');

  page.once('dialog', (dialog) => dialog.accept());
  await page.getByRole('button', { name: 'Forget Review queue route matrix memory' }).click();
  await expect
    .poll(() => state.inboxItems.some((item) => item.memory_id === 'route-matrix-memory'))
    .toBe(false);
  await expect(page.getByText('Review queue route matrix memory', { exact: true })).toBeHidden();
});

test('import upload previews a ChatGPT export and writes selected records', async ({ page }) => {
  await page.goto('/#import');
  await page.getByRole('button', { name: 'ChatGPT', exact: true }).click();

  await page.getByLabel('Files').setInputFiles({
    name: 'conversations.json',
    mimeType: 'application/json',
    buffer: Buffer.from(
      JSON.stringify([
        {
          id: 'chat-e2e-1',
          title: 'Solo E2E plan',
          messages: [{ role: 'user', content: 'Audit the duplicated UI.' }],
        },
      ]),
    ),
  });

  await expect(page.getByText('Solo E2E plan', { exact: true })).toBeVisible();
  await expect(page.getByText('Audit the duplicated UI.')).toBeVisible();

  await page.getByRole('button', { name: 'Import selected' }).click();

  await expect(page.getByText('mem:e2e-1')).toBeVisible();
  await expect(page.getByText('1/1 imported')).toBeVisible();
  expect(state.memoryWrites).toHaveLength(1);
  expect(state.memoryWrites[0]).toMatchObject({
    source_type: 'import.chatgpt',
    source_id: 'chat-e2e-1',
  });
});

test('browser document import completes prepare, chunk, commit, and extraction', async ({
  page,
}) => {
  await page.goto('/#import');
  await page.getByLabel('Files').setInputFiles({
    name: 'pilot-e2e.txt',
    mimeType: 'text/plain',
    buffer: Buffer.from('pilot body'),
  });

  await expect(page.getByText('pilot-e2e.txt')).toBeVisible();
  await expect(page.getByText('Searchable text')).toBeVisible();
  await expect(page.getByLabel('Original-file retention')).toHaveValue('solo_default');
  await page.getByRole('button', { name: 'Import 1 file' }).click();

  await expect(
    page.getByText('Searchable - Solo indexed 2 chunks of document content.'),
  ).toBeVisible();
  await expect(page.getByText(/Original source file retained locally/)).toBeVisible();
  expect(state.documentTransportEvents).toEqual([
    'prepare',
    'patch:0',
    'patch:4',
    'patch:8',
    'commit',
    'ingest:true',
  ]);
});

test('import local path scans before running a native import', async ({ page }) => {
  await page.goto('/#import');

  await page.getByRole('button', { name: 'Markdown/Text only' }).click();
  await page.getByLabel('Local path').fill('C:\\Solo Imports');
  await page.getByRole('button', { name: 'Scan path' }).click();

  await expect(page.getByRole('heading', { name: 'Markdown/Text path scan' })).toBeVisible();
  await expect(page.getByText('2 files')).toBeVisible();
  await expect(page.getByText('notes.md')).toBeVisible();

  await page.getByRole('button', { name: 'Import path' }).click();

  await expect(page.getByRole('heading', { name: 'Markdown/Text path import' })).toBeVisible();
  await expect(page.getByText('1 new', { exact: true })).toBeVisible();
  await expect(page.getByText('1 new, 1 deduped / 2')).toBeVisible();
  expect(state.nativeImportRequests.map((request) => request.dry_run)).toEqual([true, false]);
});

test('backup workflow posts the selected destination and force flag', async ({ page }) => {
  await page.goto('/#backups');

  const destination = page.getByLabel('Backup destination');
  await expect(destination).toHaveValue(/C:\\SoloData\\solo-backup-/);

  await destination.fill('C:\\SoloData\\manual-backup.db');
  await page.getByLabel('Overwrite existing target').check();
  await page.getByRole('button', { name: 'Run backup' }).click();

  await expect(page.getByText('12ms')).toBeVisible();
  await expect(page.getByText('C:\\SoloData\\manual-backup.db').first()).toBeVisible();
  expect(state.backupRequests).toEqual([{ to: 'C:\\SoloData\\manual-backup.db', force: true }]);
});

test('logs workflow changes line limit and refreshes the current tail', async ({ page }) => {
  await page.goto('/#logs');

  await expect(page.getByText('2 / 200')).toBeVisible();
  await page.getByRole('combobox', { name: 'Lines' }).selectOption('500');
  await expect(page.getByText('2 / 500')).toBeVisible();
  await expect(page.getByText('INFO ready (limit 500)')).toBeVisible();

  const fetchesBeforeRefresh = state.logFetchCount;
  await page.getByRole('button', { name: 'Refresh' }).click();
  await expect.poll(() => state.logFetchCount).toBeGreaterThan(fetchesBeforeRefresh);
  await expect(page.getByText(`DEBUG fetch ${state.logFetchCount}`).first()).toBeVisible();
});

test('memories workflow searches nodes and clears graph UI state', async ({ page }) => {
  await page.goto('/#memories');

  await expect(page.getByText(/Graph \d+\/\d+ nodes/)).toBeVisible();
  await page.getByPlaceholder('Search nodes...').fill('alice');
  await expect(page.getByRole('heading', { name: 'Search matches' })).toBeVisible();
  await expect(page.getByRole('button', { name: /alice/i }).first()).toBeVisible();

  await page.getByRole('button', { name: 'Reset' }).click();
  await expect(page.getByRole('heading', { name: 'Search matches' })).toBeHidden();
  await expect(page.getByText('No node selected')).toBeVisible();
});
