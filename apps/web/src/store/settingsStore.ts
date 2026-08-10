// Zustand store for client-side settings (Solo URL and bearer token).
// Endpoint settings persist across browser sessions; bearer credentials
// are deliberately session-scoped so closing the browser clears them.

import { create } from 'zustand';
import { DEFAULT_SOLO_API_URL } from '../config/defaults';
import { soloApiUrlError } from '../lib/endpointValidation';

const STORAGE_KEY = 'solo.settings';
const LEGACY_BEARER_KEY = 'solo.bearer';
const SESSION_BEARER_KEY = 'solo.settings.bearer.session';
const LEGACY_DEFAULT_SOLO_API_URLS = new Set(['http://127.0.0.1:7437', DEFAULT_SOLO_API_URL]);
export interface Settings {
  apiUrl: string;
  bearerToken: string;
}

export interface SettingsState extends Settings {
  /**
   * Non-secret cache identity for the active Solo connection. This changes
   * whenever the endpoint or session credential changes, so cached responses
   * from one connection cannot be reused by another without putting the
   * bearer itself in query keys.
   */
  connectionRevision: number;
  setApiUrl: (url: string) => void;
  setBearerToken: (token: string) => void;
  setAll: (next: Settings) => void;
  reset: () => void;
}

interface EnvWithVite {
  VITE_SOLO_API_URL?: string;
}

function readEnvDefaults(): Partial<Settings> {
  const env = import.meta.env as EnvWithVite;
  return {
    apiUrl: env.VITE_SOLO_API_URL,
  };
}

function loadFromStorage(): Partial<Settings> {
  let stored: Partial<Settings> = {};
  let migratedBearer: string | undefined;
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (raw) {
      try {
        const parsed: unknown = JSON.parse(raw);
        if (parsed && typeof parsed === 'object' && !Array.isArray(parsed)) {
          const candidate = parsed as Record<string, unknown>;
          stored = stringSettings(candidate);
          if (typeof stored.bearerToken === 'string' && stored.bearerToken) {
            migratedBearer = stored.bearerToken;
          }
          // Rewrite settings from older builds without the persisted credential,
          // and discard malformed known fields before they reach the typed store.
          if (requiresSanitizedRewrite(candidate) && !savePersistentSettings(stored)) {
            // If a sanitized rewrite is blocked (for example by a quota
            // failure), prefer dropping endpoint preferences over leaving a
            // legacy credential persistent.
            localStorage.removeItem(STORAGE_KEY);
          }
        } else {
          localStorage.removeItem(STORAGE_KEY);
        }
      } catch {
        // A corrupt endpoint preference must not prevent credential migration
        // and cleanup from the older persistent key.
        localStorage.removeItem(STORAGE_KEY);
      }
    }

    const legacy = localStorage.getItem(LEGACY_BEARER_KEY);
    if (!migratedBearer && legacy) migratedBearer = legacy;
    if (legacy) localStorage.removeItem(LEGACY_BEARER_KEY);
  } catch {
    // Storage may be unavailable in sandboxed or private contexts.
  }

  const sessionBearer = readSessionBearer();
  const bearerToken = sessionBearer ?? migratedBearer;
  if (!sessionBearer && migratedBearer) saveSessionBearer(migratedBearer);
  return { ...stored, bearerToken };
}

function saveToStorage(settings: Settings): void {
  savePersistentSettings(settings);
  saveSessionBearer(settings.bearerToken);
}

function savePersistentSettings(settings: Partial<Settings>): boolean {
  try {
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        ...(typeof settings.apiUrl === 'string' ? { apiUrl: settings.apiUrl } : {}),
      }),
    );
    return true;
  } catch {
    // Non-critical; the in-memory store remains usable.
    return false;
  }
}

function stringSettings(candidate: Record<string, unknown>): Partial<Settings> {
  return {
    ...(typeof candidate.apiUrl === 'string' && soloApiUrlError(candidate.apiUrl) === null
      ? { apiUrl: candidate.apiUrl }
      : {}),
    ...(typeof candidate.bearerToken === 'string' ? { bearerToken: candidate.bearerToken } : {}),
  };
}

function requiresSanitizedRewrite(candidate: Record<string, unknown>): boolean {
  return Object.entries(candidate).some(
    ([key, value]) =>
      key !== 'apiUrl' || typeof value !== 'string' || soloApiUrlError(value) !== null,
  );
}

function readSessionBearer(): string | undefined {
  try {
    const bearer = sessionStorage.getItem(SESSION_BEARER_KEY)?.trim();
    return bearer || undefined;
  } catch {
    return undefined;
  }
}

function saveSessionBearer(bearerToken: string): void {
  try {
    const bearer = bearerToken.trim();
    if (bearer) {
      sessionStorage.setItem(SESSION_BEARER_KEY, bearer);
    } else {
      sessionStorage.removeItem(SESSION_BEARER_KEY);
    }
  } catch {
    // Non-critical; the in-memory store remains usable.
  }
}

function hostedApiUrl(): string | undefined {
  if (typeof window === 'undefined') return undefined;
  const { protocol, pathname, origin } = window.location;
  const isHttp = protocol === 'http:' || protocol === 'https:';
  const isDesktopRoute = pathname === '/desktop' || pathname.startsWith('/desktop/');
  return isHttp && isDesktopRoute ? origin : undefined;
}

function defaultApiUrl(): string {
  return readEnvDefaults().apiUrl ?? hostedApiUrl() ?? DEFAULT_SOLO_API_URL;
}

function normalizeStoredApiUrl(apiUrl: string | undefined): string | undefined {
  if (!apiUrl) return undefined;
  const hosted = hostedApiUrl();
  const normalized = apiUrl.replace(/\/$/, '');
  if (hosted && LEGACY_DEFAULT_SOLO_API_URLS.has(normalized)) return hosted;
  return apiUrl;
}

function buildInitial(): Settings {
  const stored = loadFromStorage();
  const env = readEnvDefaults();
  return {
    apiUrl:
      normalizeStoredApiUrl(stored.apiUrl) ?? env.apiUrl ?? hostedApiUrl() ?? DEFAULT_SOLO_API_URL,
    bearerToken: stored.bearerToken ?? '',
  };
}

export const useSettingsStore = create<SettingsState>((set, get) => ({
  ...buildInitial(),
  connectionRevision: 0,
  setApiUrl: (apiUrl) => {
    set((state) => ({
      apiUrl,
      connectionRevision:
        state.apiUrl === apiUrl ? state.connectionRevision : state.connectionRevision + 1,
    }));
    saveToStorage(getCurrentSettings(get()));
  },
  setBearerToken: (bearerToken) => {
    set((state) => ({
      bearerToken,
      connectionRevision:
        state.bearerToken === bearerToken ? state.connectionRevision : state.connectionRevision + 1,
    }));
    saveToStorage(getCurrentSettings(get()));
  },
  setAll: (next) => {
    set((state) => ({
      ...next,
      connectionRevision:
        state.apiUrl === next.apiUrl && state.bearerToken === next.bearerToken
          ? state.connectionRevision
          : state.connectionRevision + 1,
    }));
    saveToStorage(next);
  },
  reset: () => {
    const fresh: Settings = {
      apiUrl: defaultApiUrl(),
      bearerToken: '',
    };
    set((state) => ({
      ...fresh,
      connectionRevision:
        state.apiUrl === fresh.apiUrl && state.bearerToken === fresh.bearerToken
          ? state.connectionRevision
          : state.connectionRevision + 1,
    }));
    try {
      localStorage.removeItem(STORAGE_KEY);
      localStorage.removeItem(LEGACY_BEARER_KEY);
    } catch {
      // Ignore local persistence cleanup failures.
    }
    try {
      sessionStorage.removeItem(SESSION_BEARER_KEY);
    } catch {
      // Ignore session cleanup failures.
    }
  },
}));

function getCurrentSettings(state: SettingsState): Settings {
  return {
    apiUrl: state.apiUrl,
    bearerToken: state.bearerToken,
  };
}
