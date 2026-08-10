import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import type { ReactNode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { LogsView } from '../src/components/LogsView';
import { DEFAULT_SOLO_API_URL } from '../src/config/defaults';
import { useSettingsStore } from '../src/store/settingsStore';

function makeWrapper(): ({ children }: { children: ReactNode }) => JSX.Element {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0, staleTime: 0 } },
  });
  const Wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return Wrapper;
}

function jsonResponse(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

describe('LogsView', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    useSettingsStore.getState().setAll({
      apiUrl: DEFAULT_SOLO_API_URL,
      bearerToken: '',
    });
  });

  it('renders sanitized tray log lines and copies the safe tail', async () => {
    const writeText = vi.fn(async () => undefined);
    Object.defineProperty(navigator, 'clipboard', {
      configurable: true,
      value: { writeText },
    });
    vi.stubGlobal(
      'fetch',
      vi.fn(async (input: RequestInfo | URL) => {
        expect(String(input)).toContain('/v1/logs?source=tray&limit=200');
        return jsonResponse({
          source: 'tray',
          path: 'C:\\SoloData\\tray.log',
          exists: true,
          limit: 200,
          size_bytes: 4096,
          modified_at_ms: 1779290000000,
          lines: [
            { level: 'info', text: 'INFO boot complete' },
            { level: 'warn', text: 'WARN token=[redacted]' },
            { level: 'error', text: 'ERROR bearer [redacted]' },
          ],
        });
      }),
    );

    render(<LogsView />, { wrapper: makeWrapper() });

    expect(await screen.findByRole('heading', { name: 'Logs' })).toBeInTheDocument();
    expect(await screen.findByText('Tray log')).toBeInTheDocument();
    expect(screen.getByText('INFO boot complete')).toBeInTheDocument();
    expect(screen.getByText('WARN token=[redacted]')).toBeInTheDocument();
    expect(screen.queryByText('secret-token')).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /^copy logs$/i }));
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(
        [
          '[info] INFO boot complete',
          '[warn] WARN token=[redacted]',
          '[error] ERROR bearer [redacted]',
        ].join('\n'),
      ),
    );
  });

  it('shows a missing-file state without treating it as an error', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        jsonResponse({
          source: 'tray',
          path: '/home/alex/.solo/tray.log',
          exists: false,
          limit: 200,
          size_bytes: null,
          modified_at_ms: null,
          lines: [],
        }),
      ),
    );

    render(<LogsView />, { wrapper: makeWrapper() });

    expect(await screen.findByText('tray.log has not been created yet.')).toBeInTheDocument();
    expect(screen.getByText('/home/alex/.solo/tray.log')).toBeInTheDocument();
  });

  it('surfaces daemon/API errors as actionable diagnostics', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => jsonResponse({ error: 'daemon locked' }, 503)),
    );

    render(<LogsView />, { wrapper: makeWrapper() });

    expect(await screen.findByText('Logs are unavailable.')).toBeInTheDocument();
    expect(screen.getByText(/daemon locked/i)).toBeInTheDocument();
    expect(screen.getByText(/Start or unlock Solo from the tray/i)).toBeInTheDocument();
  });
});
