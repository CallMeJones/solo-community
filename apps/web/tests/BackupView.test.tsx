import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { BackupView } from '../src/components/BackupView';
import { suggestedBackupPath } from '../src/lib/backupPaths';
import { useSettingsStore } from '../src/store/settingsStore';

function jsonResponse(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  });
}

function renderBackupView() {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  return render(
    <QueryClientProvider client={client}>
      <BackupView />
    </QueryClientProvider>,
  );
}

describe('BackupView', () => {
  beforeEach(() => {
    localStorage.clear();
    useSettingsStore.getState().setAll({
      apiUrl: 'http://solo.test',
      bearerToken: '',
    });
  });

  afterEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it('suggests platform-shaped backup paths', () => {
    const now = new Date('2026-05-29T15:04:05');
    expect(suggestedBackupPath('C:\\SoloData', now)).toBe(
      'C:\\SoloData\\solo-backup-20260529-150405.db',
    );
    expect(suggestedBackupPath('/home/alex/.solo', now)).toBe(
      '/home/alex/.solo/solo-backup-20260529-150405.db',
    );
  });

  it('prefills the destination from daemon status and runs a hot backup', async () => {
    const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith('/v1/status')) {
        return jsonResponse({
          ok: true,
          version: '0.11.9',
          build: { version: '0.11.9', version_with_build: '0.11.9' },
          library: { name: 'Community Memory Library', ready: true },
          embedder: { name: 'stub', version: 'v1', dim: 16, dtype: 'f32' },
          mcp: { sessions: 0 },
          runtime: {
            data_dir: 'C:\\SoloData',
          },
        });
      }
      if (url.endsWith('/backup')) {
        const body = JSON.parse(String(init?.body));
        expect(body.to).toMatch(/^C:\\SoloData\\solo-backup-/);
        expect(body.force).toBe(false);
        expect(init?.headers).toStrictEqual({
          Accept: 'application/json',
          'Content-Type': 'application/json',
        });
        return jsonResponse({ path: body.to, elapsed_ms: 7 });
      }
      throw new Error(`unexpected fetch: ${url}`);
    });
    vi.stubGlobal('fetch', fetchMock);

    renderBackupView();

    const input = await screen.findByLabelText('Backup destination');
    await waitFor(() =>
      expect((input as HTMLInputElement).value).toMatch(/^C:\\SoloData\\solo-backup-/),
    );

    fireEvent.click(screen.getByRole('button', { name: /^run backup$/i }));

    expect(await screen.findByText('7ms')).toBeInTheDocument();
    expect(fetchMock).toHaveBeenCalledWith(
      'http://solo.test/backup',
      expect.objectContaining({ method: 'POST' }),
    );
  });
});
