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
    expect(communityWebHost.projectTabs).toEqual([]);
    expect(communityWebHost.settingsModules).toEqual([]);
    expect(communityWebHost.statusModules).toEqual([]);
  });

  it('calls an undecorated composition Community, and takes a host at its word', () => {
    // The sidebar prints this. A composition that loaded paid modules while
    // the app still said Community was the bug this replaced.
    expect(communityWebHost.editionLabel).toBe('Community');
    expect(
      defineSoloWebHost({ id: 'paid-host', productName: 'Solo Pro', tagline: 'more' })
        .editionLabel,
    ).toBe('Community');
    expect(
      defineSoloWebHost({
        id: 'paid-host',
        productName: 'Solo Pro',
        tagline: 'more',
        editionLabel: 'Pro',
      }).editionLabel,
    ).toBe('Pro');
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

  it('shows no tab strip in Projects until a host adds a tab', async () => {
    renderHostedApp(communityWebHost);
    fireEvent.click(screen.getByRole('button', { name: 'Projects' }));

    // One tab is not a choice. Drawing a strip anyway would put furniture in
    // front of the reader in place of a feature they do not have.
    expect(screen.queryByRole('tablist')).not.toBeInTheDocument();
    expect(await screen.findByRole('heading', { name: 'Project' })).toBeInTheDocument();
  });

  it('adds a host tab beside the project, without displacing it', async () => {
    const host = defineSoloWebHost({
      id: 'tabbed-host',
      productName: 'Example Solo',
      tagline: 'composed from public Core',
      projectTabs: [
        { id: 'shared-brains', label: 'Shared brains', render: () => <p>Example tab body</p> },
      ],
    });
    renderHostedApp(host);
    fireEvent.click(screen.getByRole('button', { name: 'Projects' }));

    // The project stays the tab you land on: a composition adds to the view,
    // it does not take it over.
    const project = screen.getByRole('tab', { name: 'Project' });
    expect(project).toHaveAttribute('aria-selected', 'true');
    expect(await screen.findByRole('heading', { name: 'Project' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('tab', { name: 'Shared brains' }));
    expect(screen.getByText('Example tab body')).toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: 'Project' })).not.toBeInTheDocument();

    // And it is a tab, not a route: nothing new appears in the sidebar.
    expect(screen.queryByRole('button', { name: 'Shared brains' })).not.toBeInTheDocument();
  });

  it('rejects duplicate or Core project tab ids', () => {
    expect(() =>
      defineSoloWebHost({
        id: 'bad-host',
        productName: 'Bad',
        tagline: 'Bad',
        projectTabs: [{ id: 'project', label: 'Replace the project', render: () => null }],
      }),
    ).toThrow(/cannot replace the project itself/);
    expect(() =>
      defineSoloWebHost({
        id: 'bad-host',
        productName: 'Bad',
        tagline: 'Bad',
        projectTabs: [
          { id: 'same', label: 'One', render: () => null },
          { id: 'same', label: 'Two', render: () => null },
        ],
      }),
    ).toThrow(/duplicate project tab module id/);
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
