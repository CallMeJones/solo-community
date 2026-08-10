// Modal settings dialog for Solo API URL and bearer token.
//
// Pops over the graph view; backdrop click + Escape close. Save persists
// endpoints to localStorage and the bearer to sessionStorage. Bearer is
// has a show/hide toggle.
//
// Transport quick-fill:
//   HTTP mode  -> http://127.0.0.1:17821 (solo http-serve)
//   Dev bridge -> http://127.0.0.1:7436  (local development fallback)

import { useEffect, useId, useState } from 'react';
import { DEFAULT_SOLO_API_URL, MCP_BRIDGE_URL } from '../config/defaults';
import { soloApiUrlError } from '../lib/endpointValidation';
import { useSettingsStore } from '../store/settingsStore';
import type { Settings } from '../store/settingsStore';
import { Button } from './ui/Button';

interface SettingsDialogProps {
  open: boolean;
  onClose: () => void;
}

export function SettingsDialog({ open, onClose }: SettingsDialogProps) {
  // Subscribe to each field individually. Zustand's default `Object.is`
  // equality check would loop forever on a selector that returns a fresh
  // object literal every render.
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const bearerToken = useSettingsStore((s) => s.bearerToken);
  const setAll = useSettingsStore((s) => s.setAll);
  const reset = useSettingsStore((s) => s.reset);

  const [draft, setDraft] = useState<Settings>({ apiUrl, bearerToken });
  const [showBearer, setShowBearer] = useState(false);

  // Re-sync the draft each time the dialog opens so cancel-then-reopen
  // shows the saved value, not the abandoned edit.
  useEffect(() => {
    if (open) {
      setDraft({ apiUrl, bearerToken });
      setShowBearer(false);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // Esc-to-close.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, [open, onClose]);

  const apiUrlId = useId();
  const bearerId = useId();

  const HTTP_URL = DEFAULT_SOLO_API_URL;
  const BRIDGE_URL = MCP_BRIDGE_URL;
  const activeTransport =
    draft.apiUrl === HTTP_URL ? 'http' : draft.apiUrl === BRIDGE_URL ? 'bridge' : 'custom';
  const soloUrlHint =
    activeTransport === 'http'
      ? 'Default live Solo API endpoint. Start or unlock Solo from the tray.'
      : activeTransport === 'bridge'
        ? 'Developer bridge endpoint for local development and compatibility checks.'
        : 'Custom Solo-compatible API endpoint.';

  if (!open) return null;

  const apiUrlError = soloApiUrlError(draft.apiUrl);
  const apiUrlValid = apiUrlError === null;
  const valid = apiUrlValid;

  const handleSave = () => {
    if (!valid) return;
    setAll({
      apiUrl: draft.apiUrl.trim().replace(/\/$/, ''),
      bearerToken: draft.bearerToken.trim(),
    });
    onClose();
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-slate-950/70 backdrop-blur-sm"
      onClick={onClose}
      onKeyDown={() => undefined}
      role="presentation"
    >
      <div
        className="w-full max-w-md rounded-lg border border-slate-700 bg-slate-900 p-6 shadow-2xl"
        onClick={(e) => e.stopPropagation()}
        onKeyDown={() => undefined}
        role="dialog"
        aria-modal="true"
        aria-labelledby="settings-title"
      >
        <h2 id="settings-title" className="mb-1 text-lg font-semibold text-slate-100">
          Settings
        </h2>
        <p className="mb-5 text-xs text-slate-400">
          Endpoints persist in this browser. The bearer is kept only for this browser session; empty
          bearer means unauthenticated requests.
        </p>

        <div className="space-y-4">
          {/* Transport quick-fill */}
          <div>
            <span className="mb-1 block text-xs uppercase tracking-wider text-slate-400">
              Transport
            </span>
            <div className="flex gap-2">
              <TransportChip
                label="Solo HTTP"
                subtitle="solo http-serve"
                active={activeTransport === 'http'}
                onClick={() => setDraft((d) => ({ ...d, apiUrl: HTTP_URL }))}
              />
              <TransportChip
                label="Developer bridge"
                subtitle="dev only"
                active={activeTransport === 'bridge'}
                onClick={() => setDraft((d) => ({ ...d, apiUrl: BRIDGE_URL }))}
              />
            </div>
            {activeTransport === 'bridge' && (
              <p className="mt-1.5 text-xs text-amber-400">
                Bridge mode is for local development. Solo HTTP is the installed desktop default.
              </p>
            )}
          </div>

          <Field label="Solo API URL" htmlFor={apiUrlId} error={apiUrlError} hint={soloUrlHint}>
            <input
              id={apiUrlId}
              type="text"
              value={draft.apiUrl}
              onChange={(e) => setDraft((d) => ({ ...d, apiUrl: e.target.value }))}
              placeholder={DEFAULT_SOLO_API_URL}
              className="w-full rounded border border-slate-700 bg-slate-950 px-2 py-1.5 text-sm text-slate-100 placeholder-slate-500 focus:border-indigo-500 focus:outline-none"
              spellCheck={false}
              autoComplete="off"
            />
          </Field>

          <Field label="Bearer token (Solo HTTP auth)" htmlFor={bearerId} error={null}>
            <div className="relative">
              <input
                id={bearerId}
                type={showBearer ? 'text' : 'password'}
                value={draft.bearerToken}
                onChange={(e) => setDraft((d) => ({ ...d, bearerToken: e.target.value }))}
                placeholder="(optional)"
                className="w-full rounded border border-slate-700 bg-slate-950 px-2 py-1.5 pr-16 text-sm text-slate-100 placeholder-slate-500 focus:border-indigo-500 focus:outline-none"
                spellCheck={false}
                autoComplete="off"
              />
              <button
                type="button"
                onClick={() => setShowBearer((s) => !s)}
                className="absolute inset-y-0 right-2 my-auto text-xs text-slate-400 hover:text-slate-200"
              >
                {showBearer ? 'hide' : 'show'}
              </button>
            </div>
          </Field>
        </div>

        <div className="mt-6 flex items-center justify-between gap-2">
          <Button
            variant="ghost"
            onClick={() => {
              if (window.confirm('Reset all settings to defaults?')) {
                reset();
                onClose();
              }
            }}
          >
            Reset
          </Button>
          <div className="flex gap-2">
            <Button variant="ghost" onClick={onClose}>
              Cancel
            </Button>
            <Button onClick={handleSave} disabled={!valid}>
              Save
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}

function Field({
  label,
  htmlFor,
  error,
  hint,
  children,
}: {
  label: string;
  htmlFor: string;
  error: string | null;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <label htmlFor={htmlFor} className="block">
      <span className="mb-1 block text-xs uppercase tracking-wider text-slate-400">{label}</span>
      {children}
      {error && <span className="mt-1 block text-xs text-red-400">{error}</span>}
      {hint && <span className="mt-1 block text-xs text-slate-400">{hint}</span>}
    </label>
  );
}

function TransportChip({
  label,
  subtitle,
  active,
  onClick,
}: {
  label: string;
  subtitle: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={[
        'flex flex-1 flex-col items-start rounded border px-3 py-2 text-left transition-colors',
        active
          ? 'border-indigo-500 bg-indigo-950 text-indigo-200'
          : 'border-slate-700 bg-slate-950 text-slate-300 hover:border-slate-500',
      ].join(' ')}
    >
      <span className="text-sm font-medium">{label}</span>
      <span className="text-xs text-slate-400">{subtitle}</span>
    </button>
  );
}
