/**
 * settingsStore reads browser storage on module import, so each scenario uses
 * a fresh module evaluation to verify initialization and migration behavior.
 */

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { DEFAULT_SOLO_API_URL } from '../src/config/defaults';

async function importStore() {
  vi.resetModules();
  return (await import('../src/store/settingsStore')).useSettingsStore;
}

const DEFAULT_API_URL = DEFAULT_SOLO_API_URL;
const SESSION_BEARER_KEY = 'solo.settings.bearer.session';

function setUrl(url: string) {
  window.history.replaceState(null, '', url);
}

describe('settingsStore - initialization', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
    setUrl('/');
  });

  afterEach(() => {
    setUrl('/');
  });

  it('uses built-in defaults when browser storage is empty', async () => {
    const store = await importStore();
    const state = store.getState();
    expect(state.apiUrl).toBe(DEFAULT_API_URL);
    expect(state.bearerToken).toBe('');
  });

  it('reads the endpoint and migrates a bearer out of persistent settings', async () => {
    localStorage.setItem(
      'solo.settings',
      JSON.stringify({
        apiUrl: 'http://1.2.3.4:9999',
        bearerToken: 'tok-xyz',
      }),
    );

    const store = await importStore();
    const state = store.getState();

    expect(state.apiUrl).toBe('http://1.2.3.4:9999');
    expect(state.bearerToken).toBe('tok-xyz');
    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).not.toHaveProperty(
      'bearerToken',
    );
    expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBe('tok-xyz');
  });

  it('reads partial persisted endpoint settings', async () => {
    localStorage.setItem('solo.settings', JSON.stringify({ apiUrl: 'http://solo.test' }));

    const store = await importStore();

    expect(store.getState().apiUrl).toBe('http://solo.test');
    expect(store.getState().bearerToken).toBe('');
  });

  it('falls back to defaults on malformed JSON', async () => {
    localStorage.setItem('solo.settings', '{not valid json');
    const store = await importStore();
    expect(store.getState().apiUrl).toBe(DEFAULT_API_URL);
    expect(localStorage.getItem('solo.settings')).toBeNull();
  });

  it('rejects non-object settings values', async () => {
    localStorage.setItem('solo.settings', 'null');

    const store = await importStore();

    expect(store.getState().apiUrl).toBe(DEFAULT_API_URL);
    expect(localStorage.getItem('solo.settings')).toBeNull();
  });

  it('drops non-string and retired persisted fields', async () => {
    localStorage.setItem(
      'solo.settings',
      JSON.stringify({
        apiUrl: 17821,
        bearerToken: { value: 'must-not-load' },
        retiredEndpoint: 'http://retired.test',
      }),
    );

    const store = await importStore();

    expect(store.getState().apiUrl).toBe(DEFAULT_API_URL);
    expect(store.getState().bearerToken).toBe('');
    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).toStrictEqual({});
  });

  it('retains a valid API URL while removing non-string bearer and unknown fields', async () => {
    localStorage.setItem(
      'solo.settings',
      JSON.stringify({
        apiUrl: 'https://solo.example',
        bearerToken: 12345,
        retiredEndpoint: ['http://invalid.test'],
      }),
    );

    const store = await importStore();

    expect(store.getState().apiUrl).toBe('https://solo.example');
    expect(store.getState().bearerToken).toBe('');
    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).toStrictEqual({
      apiUrl: 'https://solo.example',
    });
  });

  it.each([
    ['embedded credentials', 'https://solo-user:solo-pass@solo.example'],
    ['a query string', 'https://solo.example?profile=private'],
    ['a fragment', 'https://solo.example/#private'],
  ])('drops persisted API URLs containing %s and rewrites valid settings', async (_, apiUrl) => {
    localStorage.setItem(
      'solo.settings',
      JSON.stringify({ apiUrl, retiredEndpoint: 'http://retired.test' }),
    );

    const store = await importStore();

    expect(store.getState().apiUrl).toBe(DEFAULT_API_URL);
    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).toStrictEqual({});
  });

  it('uses the current origin when served from the daemon desktop route', async () => {
    setUrl('/desktop/#home');

    const store = await importStore();

    expect(store.getState().apiUrl).toBe(window.location.origin);
  });

  it('migrates a persisted built-in API default to the daemon desktop origin', async () => {
    localStorage.setItem('solo.settings', JSON.stringify({ apiUrl: DEFAULT_API_URL }));
    setUrl('/desktop/#home');

    const store = await importStore();

    expect(store.getState().apiUrl).toBe(window.location.origin);
  });

  it('keeps custom persisted API URLs on the daemon desktop route', async () => {
    localStorage.setItem('solo.settings', JSON.stringify({ apiUrl: 'http://10.0.0.8:17821' }));
    setUrl('/desktop/#home');

    const store = await importStore();

    expect(store.getState().apiUrl).toBe('http://10.0.0.8:17821');
  });
});

describe('settingsStore - bearer migration', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('reads the legacy solo.bearer key', async () => {
    localStorage.setItem('solo.bearer', 'legacy-tok');
    const store = await importStore();
    expect(store.getState().bearerToken).toBe('legacy-tok');
  });

  it('moves the legacy bearer into session storage and removes the persistent copy', async () => {
    localStorage.setItem('solo.bearer', 'legacy-tok');

    await importStore();

    expect(localStorage.getItem('solo.bearer')).toBeNull();
    expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBe('legacy-tok');
    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).not.toHaveProperty(
      'bearerToken',
    );
  });

  it('still migrates the legacy bearer when endpoint settings are corrupt', async () => {
    localStorage.setItem('solo.settings', '{not valid json');
    localStorage.setItem('solo.bearer', 'legacy-tok');

    const store = await importStore();

    expect(store.getState().bearerToken).toBe('legacy-tok');
    expect(localStorage.getItem('solo.bearer')).toBeNull();
    expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBe('legacy-tok');
  });

  it('drops persistent settings if a sanitized bearer rewrite is blocked', async () => {
    localStorage.setItem(
      'solo.settings',
      JSON.stringify({ apiUrl: 'http://solo.test', bearerToken: 'legacy-tok' }),
    );
    const originalSetItem = Storage.prototype.setItem;
    const setItemSpy = vi
      .spyOn(Storage.prototype, 'setItem')
      .mockImplementation(function (key, value) {
        if (this === localStorage && key === 'solo.settings') {
          throw new DOMException('quota exceeded', 'QuotaExceededError');
        }
        return originalSetItem.call(this, key, value);
      });

    try {
      const store = await importStore();

      expect(store.getState().bearerToken).toBe('legacy-tok');
      expect(localStorage.getItem('solo.settings')).toBeNull();
      expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBe('legacy-tok');
    } finally {
      setItemSpy.mockRestore();
    }
  });

  it('prefers the newer settings bearer and still removes the legacy key', async () => {
    localStorage.setItem('solo.settings', JSON.stringify({ bearerToken: 'new-tok' }));
    localStorage.setItem('solo.bearer', 'legacy-tok');

    const store = await importStore();

    expect(store.getState().bearerToken).toBe('new-tok');
    expect(localStorage.getItem('solo.bearer')).toBeNull();
    expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBe('new-tok');
  });

  it('prefers an existing session bearer over stale persistent credentials', async () => {
    sessionStorage.setItem(SESSION_BEARER_KEY, 'current-session');
    localStorage.setItem('solo.settings', JSON.stringify({ bearerToken: 'stale-persistent' }));

    const store = await importStore();

    expect(store.getState().bearerToken).toBe('current-session');
  });
});

describe('settingsStore - actions', () => {
  beforeEach(() => {
    localStorage.clear();
    sessionStorage.clear();
  });

  it('setBearerToken stores the credential only for the browser session', async () => {
    const store = await importStore();
    store.getState().setBearerToken('new-bearer');

    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).not.toHaveProperty(
      'bearerToken',
    );
    expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBe('new-bearer');
    expect(store.getState().bearerToken).toBe('new-bearer');
  });

  it('rotates a non-secret connection revision for endpoint and bearer changes', async () => {
    const store = await importStore();
    const initialRevision = store.getState().connectionRevision;

    store.getState().setBearerToken('new-bearer');
    expect(store.getState().connectionRevision).toBe(initialRevision + 1);

    store.getState().setBearerToken('new-bearer');
    expect(store.getState().connectionRevision).toBe(initialRevision + 1);

    store.getState().setApiUrl('http://next-solo.test');
    expect(store.getState().connectionRevision).toBe(initialRevision + 2);

  });

  it('restores the bearer within the same browser session', async () => {
    const store = await importStore();
    store.getState().setBearerToken('tab-session-bearer');

    const reloaded = await importStore();

    expect(reloaded.getState().bearerToken).toBe('tab-session-bearer');
  });

  it('setApiUrl persists the endpoint', async () => {
    const store = await importStore();
    store.getState().setApiUrl('http://example.com:8080');
    const persisted = JSON.parse(localStorage.getItem('solo.settings') ?? '{}');
    expect(persisted.apiUrl).toBe('http://example.com:8080');
  });

  it('setAll splits the endpoint and credential across storage scopes', async () => {
    const store = await importStore();
    store.getState().setAll({
      apiUrl: 'http://a.test',
      bearerToken: 'bearer',
    });

    expect(store.getState()).toMatchObject({
      apiUrl: 'http://a.test',
      bearerToken: 'bearer',
    });
    expect(JSON.parse(localStorage.getItem('solo.settings') ?? '{}')).toStrictEqual({
      apiUrl: 'http://a.test',
    });
    expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBe('bearer');
  });

  it('reset restores defaults and clears both storage scopes', async () => {
    localStorage.setItem(
      'solo.settings',
      JSON.stringify({ bearerToken: 'will-be-gone', apiUrl: 'http://gone.test' }),
    );
    const store = await importStore();
    expect(store.getState().bearerToken).toBe('will-be-gone');

    store.getState().reset();

    expect(store.getState().bearerToken).toBe('');
    expect(store.getState().apiUrl).toBe(DEFAULT_API_URL);
    expect(localStorage.getItem('solo.settings')).toBeNull();
    expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBeNull();
  });

  it('reset clears the session bearer even when localStorage cleanup fails', async () => {
    const store = await importStore();
    store.getState().setBearerToken('will-be-gone');
    const originalRemoveItem = Storage.prototype.removeItem;
    const removeItemSpy = vi
      .spyOn(Storage.prototype, 'removeItem')
      .mockImplementation(function (key) {
        if (this === localStorage) throw new DOMException('blocked', 'SecurityError');
        return originalRemoveItem.call(this, key);
      });

    try {
      store.getState().reset();

      expect(store.getState().bearerToken).toBe('');
      expect(sessionStorage.getItem(SESSION_BEARER_KEY)).toBeNull();
    } finally {
      removeItemSpy.mockRestore();
    }
  });
});
