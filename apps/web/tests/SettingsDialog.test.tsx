/**
 * RTL tests for src/components/SettingsDialog.tsx.
 *
 * Covers: open/close, field rendering with current store values, draft
 * editing without committing, URL validation, session-scoped auth, cancel
 * discards, Esc closes, backdrop click closes, reset clears.
 */

import { fireEvent, render, screen, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { SettingsDialog } from '../src/components/SettingsDialog';
import { DEFAULT_SOLO_API_URL, MCP_BRIDGE_URL } from '../src/config/defaults';
import { useSettingsStore } from '../src/store/settingsStore';

function resetStore() {
  localStorage.clear();
  useSettingsStore.setState({
    apiUrl: DEFAULT_SOLO_API_URL,
    bearerToken: '',
  });
}

describe('SettingsDialog', () => {
  beforeEach(() => {
    resetStore();
  });

  it('renders nothing when closed', () => {
    const { container } = render(<SettingsDialog open={false} onClose={() => undefined} />);
    expect(container.firstChild).toBeNull();
  });

  it('renders the dialog when open', () => {
    render(<SettingsDialog open onClose={() => undefined} />);
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(screen.getByText('Settings')).toBeInTheDocument();
    expect(screen.getByText(/start or unlock Solo from the tray/i)).toBeInTheDocument();
    expect(screen.getByText(/bearer is kept only for this browser session/i)).toBeInTheDocument();
    expect(screen.queryByLabelText(/chat backend url/i)).not.toBeInTheDocument();
  });

  it('pre-fills fields with current store values', () => {
    useSettingsStore.setState({
      apiUrl: 'http://test.example:9000',
      bearerToken: 'tok-abc',
    });
    render(<SettingsDialog open onClose={() => undefined} />);
    expect(screen.getByLabelText(/solo api url/i)).toHaveValue('http://test.example:9000');
    expect(screen.getByLabelText(/bearer token/i)).toHaveValue('tok-abc');
    expect(screen.queryByLabelText(/chat backend url/i)).not.toBeInTheDocument();
  });

  it('save commits draft to the store + closes', () => {
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);

    fireEvent.change(screen.getByLabelText(/solo api url/i), {
      target: { value: 'http://new.example:17821' },
    });
    fireEvent.change(screen.getByLabelText(/bearer token/i), {
      target: { value: 'new-tok' },
    });

    fireEvent.click(screen.getByRole('button', { name: /^save$/i }));

    expect(useSettingsStore.getState().apiUrl).toBe('http://new.example:17821');
    expect(useSettingsStore.getState().bearerToken).toBe('new-tok');
    expect(onClose).toHaveBeenCalled();
  });

  it('save trims trailing slashes off URLs', () => {
    render(<SettingsDialog open onClose={() => undefined} />);
    fireEvent.change(screen.getByLabelText(/solo api url/i), {
      target: { value: 'http://test.example:17821/' },
    });
    fireEvent.click(screen.getByRole('button', { name: /^save$/i }));
    expect(useSettingsStore.getState().apiUrl).toBe('http://test.example:17821');
  });

  it('accepts and persists a clean HTTPS API URL', () => {
    render(<SettingsDialog open onClose={() => undefined} />);
    fireEvent.change(screen.getByLabelText(/solo api url/i), {
      target: { value: 'https://solo.example/api/' },
    });

    fireEvent.click(screen.getByRole('button', { name: /^save$/i }));

    expect(useSettingsStore.getState().apiUrl).toBe('https://solo.example/api');
    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).toMatchObject({
      apiUrl: 'https://solo.example/api',
    });
  });

  it('cancel does not commit + calls onClose', () => {
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);

    fireEvent.change(screen.getByLabelText(/bearer token/i), {
      target: { value: 'should-not-persist' },
    });
    fireEvent.click(screen.getByRole('button', { name: /cancel/i }));

    expect(useSettingsStore.getState().bearerToken).toBe('');
    expect(onClose).toHaveBeenCalled();
  });

  it('save button is disabled when URL is invalid', () => {
    render(<SettingsDialog open onClose={() => undefined} />);
    fireEvent.change(screen.getByLabelText(/solo api url/i), {
      target: { value: 'not-a-url' },
    });
    expect(screen.getByRole('button', { name: /^save$/i })).toBeDisabled();
  });

  it.each([
    {
      label: 'embedded credentials',
      url: 'https://solo-user:solo-pass@solo.example',
      error: /credentials are not allowed in the URL/i,
    },
    {
      label: 'a query string',
      url: 'https://solo.example?profile=private',
      error: /query strings and fragments are not allowed/i,
    },
    {
      label: 'a fragment',
      url: 'https://solo.example/#private',
      error: /query strings and fragments are not allowed/i,
    },
    {
      label: 'an empty query delimiter',
      url: 'https://solo.example?',
      error: /query strings and fragments are not allowed/i,
    },
    {
      label: 'an empty fragment delimiter',
      url: 'https://solo.example#',
      error: /query strings and fragments are not allowed/i,
    },
  ])('rejects $label without changing persisted settings', ({ url, error }) => {
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);

    fireEvent.change(screen.getByLabelText(/solo api url/i), { target: { value: url } });

    expect(screen.getByText(error)).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /^save$/i })).toBeDisabled();
    fireEvent.click(screen.getByRole('button', { name: /^save$/i }));
    expect(useSettingsStore.getState().apiUrl).toBe(DEFAULT_SOLO_API_URL);
    expect(localStorage.getItem('solo.settings')).toBeNull();
    expect(onClose).not.toHaveBeenCalled();
  });

  it('quick-fills the Solo API URL for Solo HTTP and developer bridge modes', () => {
    render(<SettingsDialog open onClose={() => undefined} />);
    const soloUrl = screen.getByLabelText(/solo api url/i);

    expect(soloUrl).toHaveValue(DEFAULT_SOLO_API_URL);
    expect(screen.getByRole('button', { name: /Solo HTTP/i })).toHaveAttribute(
      'aria-pressed',
      'true',
    );

    fireEvent.click(screen.getByRole('button', { name: /Developer bridge/i }));
    expect(soloUrl).toHaveValue(MCP_BRIDGE_URL);
    expect(screen.getByRole('button', { name: /Developer bridge/i })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
    expect(screen.getByText(/bridge mode is for local development/i)).toBeInTheDocument();
    expect(screen.getByText(/dev only/i)).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: /Solo HTTP/i }));
    expect(soloUrl).toHaveValue(DEFAULT_SOLO_API_URL);
  });

  it('shows a validation message under an invalid URL field', () => {
    render(<SettingsDialog open onClose={() => undefined} />);
    fireEvent.change(screen.getByLabelText(/solo api url/i), {
      target: { value: 'nope' },
    });
    expect(screen.getByText(/http\(s\) url required/i)).toBeInTheDocument();
  });

  it('Esc key closes the dialog', () => {
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(onClose).toHaveBeenCalled();
  });

  it('other keys do not close', () => {
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);
    fireEvent.keyDown(window, { key: 'Enter' });
    expect(onClose).not.toHaveBeenCalled();
  });

  it('toggles bearer visibility on show/hide click', () => {
    render(<SettingsDialog open onClose={() => undefined} />);
    const bearerInput = screen.getByLabelText(/bearer token/i);
    expect(bearerInput).toHaveAttribute('type', 'password');

    fireEvent.click(screen.getByRole('button', { name: /show/i }));
    expect(bearerInput).toHaveAttribute('type', 'text');

    fireEvent.click(screen.getByRole('button', { name: /hide/i }));
    expect(bearerInput).toHaveAttribute('type', 'password');
  });

  it('re-syncs draft fields when re-opened after editing was abandoned', () => {
    const { rerender } = render(<SettingsDialog open onClose={() => undefined} />);
    fireEvent.change(screen.getByLabelText(/bearer token/i), {
      target: { value: 'abandoned-edit' },
    });
    // Close (without save), then re-open.
    rerender(<SettingsDialog open={false} onClose={() => undefined} />);
    rerender(<SettingsDialog open onClose={() => undefined} />);
    expect(screen.getByLabelText(/bearer token/i)).toHaveValue('');
  });

  it('reset clears settings after confirm', () => {
    useSettingsStore.setState({
      apiUrl: 'http://x.test',
      bearerToken: 'will-be-gone',
    });
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(true);
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);
    fireEvent.click(screen.getByRole('button', { name: /reset/i }));
    expect(useSettingsStore.getState().bearerToken).toBe('');
    expect(useSettingsStore.getState().apiUrl).toBe(DEFAULT_SOLO_API_URL);
    expect(onClose).toHaveBeenCalled();
    confirmSpy.mockRestore();
  });

  it('reset is cancelled if confirm returns false', () => {
    useSettingsStore.setState({
      apiUrl: 'http://x.test',
      bearerToken: 'preserved',
    });
    const confirmSpy = vi.spyOn(window, 'confirm').mockReturnValue(false);
    render(<SettingsDialog open onClose={() => undefined} />);
    fireEvent.click(screen.getByRole('button', { name: /reset/i }));
    expect(useSettingsStore.getState().bearerToken).toBe('preserved');
    confirmSpy.mockRestore();
  });

  it('backdrop click closes the dialog', () => {
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);
    const dialog = screen.getByRole('dialog');
    const backdrop = dialog.parentElement;
    expect(backdrop).not.toBeNull();
    if (backdrop) fireEvent.click(backdrop);
    expect(onClose).toHaveBeenCalled();
  });

  it('clicking inside the dialog does not close it', () => {
    const onClose = vi.fn();
    render(<SettingsDialog open onClose={onClose} />);
    fireEvent.click(within(screen.getByRole('dialog')).getByText('Settings'));
    expect(onClose).not.toHaveBeenCalled();
  });
});
