import '@testing-library/jest-dom/vitest';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';
import { fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../src/App';
import { communityWebHost, defineSoloWebHost } from '../src/host';

function renderHostedApp(host: ReturnType<typeof defineSoloWebHost>) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false, gcTime: 0 }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <App host={host} />
    </QueryClientProvider>,
  );
}

describe('Solo Web host composition', () => {
  beforeEach(() => {
    window.history.replaceState(null, '', '/');
    vi.stubGlobal(
      'fetch',
      vi.fn(async () =>
        new Response(JSON.stringify({ error: 'offline test' }), {
          status: 503,
          headers: { 'content-type': 'application/json' },
        }),
      ),
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it('keeps the Community composition free of downstream modules', () => {
    expect(communityWebHost.routes).toEqual([]);
    expect(communityWebHost.settingsModules).toEqual([]);
    expect(communityWebHost.statusModules).toEqual([]);
  });

  it('registers typed navigation, route, settings, and status modules', () => {
    const host = defineSoloWebHost({
      id: 'example-host',
      productName: 'Example Solo',
      tagline: 'composed from public Core',
      routes: [
        {
          id: 'insights',
          label: 'Insights',
          render: ({ navigate }) => (
            <button type="button" onClick={() => navigate('settings')}>
              Example route
            </button>
          ),
        },
      ],
      settingsModules: [
        { id: 'example-settings', render: () => <section>Example settings module</section> },
      ],
      statusModules: [{ id: 'example-status', render: () => <div>Example status module</div> }],
    });
    window.history.replaceState(null, '', '/#insights');

    renderHostedApp(host);

    expect(screen.getByText('Example Solo')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Insights' })).toHaveAttribute(
      'aria-current',
      'page',
    );
    expect(screen.getByText('Example status module')).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Example route' }));
    expect(screen.getByRole('heading', { name: 'Settings' })).toBeInTheDocument();
    expect(screen.getByText('Example settings module')).toBeInTheDocument();
  });

  it('rejects duplicate or Core route ids', () => {
    expect(() =>
      defineSoloWebHost({
        id: 'bad-host',
        productName: 'Bad',
        tagline: 'Bad',
        routes: [{ id: 'settings', label: 'Replace settings', render: () => null }],
      }),
    ).toThrow(/cannot replace a Core route/);
    expect(() =>
      defineSoloWebHost({
        id: 'bad-host',
        productName: 'Bad',
        tagline: 'Bad',
        routes: [
          { id: 'same', label: 'One', render: () => null },
          { id: 'same', label: 'Two', render: () => null },
        ],
      }),
    ).toThrow(/duplicate route module id/);
  });
});
