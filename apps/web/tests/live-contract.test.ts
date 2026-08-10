/**
 * Live contract smoke for solo-web's Solo REST client.
 *
 * Run with a Solo daemon already listening:
 *
 *   SOLO_API_URL=http://127.0.0.1:17821 npm run test:live
 *
 * Optional:
 *
 *   SOLO_BEARER=<token>
 *
 * The suite skips automatically when SOLO_API_URL is unset.
 */

import { existsSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { afterEach, describe, expect, it } from 'vitest';
import {
  fetchGraph,
  fetchInspect,
  fetchLogs,
  importDocumentPath,
  reviewMemory,
  runBackup,
  updateMemory,
} from '../src/api/client';
import { fetchSoloStatus } from '../src/api/health';
import { readSSE, type SseEvent } from '../src/lib/sse';
import { useSettingsStore } from '../src/store/settingsStore';

const SOLO_API_URL = process.env.SOLO_API_URL;
const SOLO_BEARER = process.env.SOLO_BEARER ?? '';

async function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms}ms`)), ms);
  });
  try {
    return await Promise.race([promise, timeout]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

async function nextSseEvent(
  iterator: AsyncIterator<SseEvent>,
  predicate: (event: SseEvent) => boolean,
  label: string,
): Promise<SseEvent> {
  for (;;) {
    const next = await withTimeout(iterator.next(), 10_000, label);
    if (next.done) {
      throw new Error(`${label} ended before the expected SSE event`);
    }
    if (predicate(next.value)) return next.value;
  }
}

describe.skipIf(!SOLO_API_URL)('solo-web live Solo REST contract', () => {
  const cleanupPaths: string[] = [];

  afterEach(async () => {
    useSettingsStore.getState().reset();
    while (cleanupPaths.length > 0) {
      const target = cleanupPaths.pop();
      if (target) {
        await rm(target, { recursive: true, force: true }).catch(() => undefined);
      }
    }
  });

  function pointClientAtLiveSolo() {
    useSettingsStore.getState().setAll({
      apiUrl: SOLO_API_URL ?? '',
      bearerToken: SOLO_BEARER,
    });
  }

  function liveHeaders(): Record<string, string> {
    return {
      Accept: 'application/json',
      'Content-Type': 'application/json',
      ...(SOLO_BEARER ? { Authorization: `Bearer ${SOLO_BEARER}` } : {}),
    };
  }

  it('loads graph nodes and edges from the Community Memory Library', async () => {
    pointClientAtLiveSolo();

    const graph = await fetchGraph();

    expect(Array.isArray(graph.nodes)).toBe(true);
    expect(Array.isArray(graph.edges)).toBe(true);
    for (const node of graph.nodes) {
      expect(typeof node.id).toBe('string');
      expect(typeof node.kind).toBe('string');
      expect(typeof node.label).toBe('string');
      expect(node).not.toHaveProperty('tenant_id');
    }
  });

  it('streams graph invalidation after a live memory write', async () => {
    pointClientAtLiveSolo();
    const controller = new AbortController();
    const stream = await fetch(`${SOLO_API_URL}/v1/graph/stream`, {
      headers: {
        Accept: 'text/event-stream',
        ...(SOLO_BEARER ? { Authorization: `Bearer ${SOLO_BEARER}` } : {}),
      },
      signal: controller.signal,
    });
    expect(stream.ok).toBe(true);
    expect(stream.body).not.toBeNull();
    if (!stream.body) throw new Error('/v1/graph/stream returned no body');

    const iterator = readSSE(stream.body, controller.signal)[Symbol.asyncIterator]();
    try {
      const init = await nextSseEvent(iterator, (event) => event.event === 'init', 'graph stream init');
      expect(JSON.parse(init.data)).toMatchObject({
        connected: true,
      });

      const marker = Date.now();
      const remember = await fetch(`${SOLO_API_URL}/memory`, {
        method: 'POST',
        headers: liveHeaders(),
        body: JSON.stringify({
          content: `solo-web live graph stream invalidate ${marker}`,
          source_type: 'user_clarification',
        }),
      });
      expect(remember.ok).toBe(true);

      const invalidate = await nextSseEvent(
        iterator,
        (event) => event.event === 'invalidate',
        'graph stream invalidate',
      );
      expect(JSON.parse(invalidate.data)).toMatchObject({
        kind: 'episode',
        reason: 'memory.remember',
      });
    } finally {
      controller.abort();
      await iterator.return?.();
    }
  });

  it('loads /v1/status for the Community status strip', async () => {
    pointClientAtLiveSolo();

    const status = await fetchSoloStatus();

    expect(status.ok).toBe(true);
    expect(typeof status.version).toBe('string');
    expect(status.version.length).toBeGreaterThan(0);
    expect(status.library.name).toBe('Community Memory Library');
    expect(status.library.ready).toBe(true);
    expect(typeof status.embedder.name).toBe('string');
    expect(typeof status.embedder.version).toBe('string');
    expect(typeof status.embedder.dim).toBe('number');
    expect(status.embedder.dim).toBeGreaterThan(0);
    expect(typeof status.embedder.dtype).toBe('string');
    expect(typeof status.mcp.sessions).toBe('number');
  });

  it('loads tray logs with the web client log shape', async () => {
    pointClientAtLiveSolo();

    const logs = await fetchLogs(10);

    expect(logs.source).toBe('tray');
    expect(typeof logs.path).toBe('string');
    expect(typeof logs.exists).toBe('boolean');
    expect(logs.limit).toBe(10);
    expect(Array.isArray(logs.lines)).toBe(true);
    for (const line of logs.lines) {
      expect(['trace', 'debug', 'info', 'warn', 'error']).toContain(line.level);
      expect(typeof line.text).toBe('string');
    }
  });

  it('corrects a fresh episode through PATCH /memory/{id} and inspect sees it', async () => {
    pointClientAtLiveSolo();
    const marker = Date.now();
    const original = `solo-web live correction original ${marker}`;
    const corrected = `solo-web live correction updated ${marker}`;

    const remember = await fetch(`${SOLO_API_URL}/memory`, {
      method: 'POST',
      headers: liveHeaders(),
      body: JSON.stringify({
        content: original,
        source_type: 'user_clarification',
      }),
    });
    expect(remember.ok).toBe(true);
    const remembered = (await remember.json()) as { memory_id: string };
    expect(remembered.memory_id).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
    );

    await expect(updateMemory(`ep:${remembered.memory_id}`, corrected))
      .resolves.toMatchObject({
        memory_id: remembered.memory_id,
        content: corrected,
      });
    await expect(fetchInspect(`ep:${remembered.memory_id}`)).resolves
      .toMatchObject({
        full_text: corrected,
      });
  });

  it('resets a fresh inbox review and preserves null response state', async () => {
    pointClientAtLiveSolo();
    const marker = Date.now();
    const content = `solo-web live review reset ${marker}`;

    const remember = await fetch(`${SOLO_API_URL}/memory`, {
      method: 'POST',
      headers: liveHeaders(),
      body: JSON.stringify({
        content,
        source_type: 'user_clarification',
      }),
    });
    expect(remember.ok).toBe(true);
    const remembered = (await remember.json()) as { memory_id: string };

    await expect(reviewMemory(remembered.memory_id, 'approved'))
      .resolves.toMatchObject({
        memory_id: remembered.memory_id,
        state: 'approved',
      });
    await expect(reviewMemory(remembered.memory_id, 'reset')).resolves
      .toMatchObject({
        memory_id: remembered.memory_id,
        state: null,
        reviewed_at_ms: null,
      });
  });

  it('dry-runs and imports a local markdown path through document memory', async () => {
    pointClientAtLiveSolo();
    const importDir = await mkdtemp(join(tmpdir(), 'solo-web-live-import-'));
    cleanupPaths.push(importDir);
    const marker = Date.now();
    writeFileSync(
      join(importDir, 'live-note.md'),
      `# Solo web live import ${marker}\n\nThis fixture proves native path import.`,
      'utf8',
    );

    const dryRun = await importDocumentPath(
      {
        path: importDir,
        source: 'markdown_text',
        dry_run: true,
        recursive: true,
        max_files: 10,
      },
      {},
    );
    expect(dryRun.dry_run).toBe(true);
    expect(dryRun.total_files).toBeGreaterThan(0);
    expect(dryRun.files.some((file) => file.path.endsWith('live-note.md'))).toBe(true);

    const imported = await importDocumentPath(
      {
        path: importDir,
        source: 'markdown_text',
        dry_run: false,
        recursive: true,
        max_files: 10,
      },
      {},
    );
    expect(imported.dry_run).toBe(false);
    expect(imported.total_files).toBeGreaterThan(0);
    expect(imported.imported + imported.deduped).toBeGreaterThan(0);
    expect(imported.failed).toBe(0);
  });

  it('imports a schema-aware ChatGPT export and exposes the document in graph nodes', async () => {
    pointClientAtLiveSolo();
    const importDir = await mkdtemp(join(tmpdir(), 'solo-web-live-chatgpt-'));
    cleanupPaths.push(importDir);
    const marker = Date.now();
    const title = `Solo web schema live ${marker}`;
    writeFileSync(
      join(importDir, 'conversations.json'),
      JSON.stringify([
        {
          id: `schema-live-${marker}`,
          title,
          messages: [
            { role: 'user', content: 'Please audit schema-aware import coverage.' },
            { role: 'assistant', content: 'Add a live daemon contract test.' },
          ],
        },
      ]),
      'utf8',
    );

    const imported = await importDocumentPath(
      {
        path: importDir,
        source: 'chatgpt',
        dry_run: false,
        recursive: false,
        max_files: 10,
      },
      {},
    );
    expect(imported.source).toBe('chatgpt');
    expect(imported.source_label).toBe('ChatGPT');
    expect(imported.dry_run).toBe(false);
    expect(imported.total_files).toBe(1);
    expect(imported.imported + imported.deduped).toBeGreaterThan(0);
    expect(imported.failed).toBe(0);

    const graph = await fetchGraph();
    expect(graph.nodes.some((node) => node.kind === 'document' && node.label.includes(title)))
      .toBe(true);
  });

  it('creates a hot backup at a caller-provided destination', async () => {
    pointClientAtLiveSolo();
    const backupPath = join(tmpdir(), `solo-web-live-backup-${Date.now()}.db`);
    cleanupPaths.push(backupPath);

    const backup = await runBackup({ to: backupPath, force: false });

    expect(backup.path).toBe(backupPath);
    expect(backup.elapsed_ms).toBeGreaterThanOrEqual(0);
    expect(existsSync(backupPath)).toBe(true);
    expect(statSync(backupPath).size).toBeGreaterThan(0);

    await expect(runBackup({ to: backupPath, force: false })).rejects
      .toThrow(/backup|exists|failed/i);

    rmSync(backupPath, { force: true });
  });
});
