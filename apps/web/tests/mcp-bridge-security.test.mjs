import { Readable } from 'node:stream';
import { describe, expect, it, vi } from 'vitest';
import {
  handle,
  isAuthorized,
  readJsonBody,
  resolveBridgeToken,
  soloProcessEnvironment,
} from '../scripts/mcp-bridge.mjs';

describe('MCP bridge request boundary', () => {
  function responseRecorder() {
    return {
      body: undefined,
      headers: {},
      status: undefined,
      setHeader(name, value) {
        this.headers[name] = value;
      },
      writeHead(status, headers = {}) {
        this.status = status;
        Object.assign(this.headers, headers);
      },
      end(body) {
        this.body = body;
      },
    };
  }

  it('requires an exact bearer token', () => {
    expect(isAuthorized('Bearer bridge-secret', 'bridge-secret')).toBe(true);
    expect(isAuthorized('bearer bridge-secret', 'bridge-secret')).toBe(true);
    expect(isAuthorized('Bearer wrong-secret', 'bridge-secret')).toBe(false);
    expect(isAuthorized(undefined, 'bridge-secret')).toBe(false);
  });

  it('rejects weak configured tokens and generates a strong default', () => {
    expect(() => resolveBridgeToken('too-short')).toThrow(/at least 32 bytes/);
    expect(resolveBridgeToken('x'.repeat(32))).toBe('x'.repeat(32));
    expect(resolveBridgeToken(undefined)).toMatch(/^[0-9a-f]{64}$/);
  });

  it('does not forward the HTTP bridge token to the Solo child process', () => {
    expect(
      soloProcessEnvironment({ SOLO_BRIDGE_TOKEN: 'x'.repeat(32), SOLO_PASSPHRASE: 'database' }),
    ).toEqual({ SOLO_PASSPHRASE: 'database' });
  });

  it('rejects an unauthenticated mutation before calling Solo', async () => {
    const client = { tool: vi.fn() };
    const response = responseRecorder();

    await handle(
      client,
      { headers: {}, method: 'DELETE', url: '/memory/01935b9c-1234-7abc-89de-fedcba987654' },
      response,
      'x'.repeat(32),
    );

    expect(response.status).toBe(401);
    expect(client.tool).not.toHaveBeenCalled();
  });

  it('allows the same mutation with the exact bearer token', async () => {
    const client = { tool: vi.fn().mockResolvedValue(undefined) };
    const response = responseRecorder();
    const token = 'x'.repeat(32);

    await handle(
      client,
      {
        headers: { authorization: `Bearer ${token}` },
        method: 'DELETE',
        url: '/memory/01935b9c-1234-7abc-89de-fedcba987654',
      },
      response,
      token,
    );

    expect(response.status).toBe(204);
    expect(client.tool).toHaveBeenCalledWith('memory_forget', {
      memory_id: '01935b9c-1234-7abc-89de-fedcba987654',
      reason: 'solo-web bridge',
    });
  });

  it('rejects request bodies above the configured limit', async () => {
    const request = Readable.from([Buffer.from('{"value":"too large"}')]);

    await expect(readJsonBody(request, 8)).rejects.toMatchObject({ statusCode: 413 });
  });

  it('parses JSON within the configured limit', async () => {
    const request = Readable.from([Buffer.from('{"ok":true}')]);

    await expect(readJsonBody(request, 64)).resolves.toEqual({ ok: true });
  });

  it('rejects malformed JSON as a client error', async () => {
    const request = Readable.from([Buffer.from('{not-json}')]);

    await expect(readJsonBody(request, 64)).rejects.toMatchObject({ statusCode: 400 });
  });
});
