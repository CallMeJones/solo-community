/**
 * Tests for src/lib/sse.ts — the manual parser used by useGraphStream
 * because browser EventSource cannot carry bearer headers.
 */

import { describe, expect, it } from 'vitest';
import { readSSE } from '../src/lib/sse';

function streamOf(text: string): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  return new ReadableStream({
    start(controller) {
      controller.enqueue(encoder.encode(text));
      controller.close();
    },
  });
}

function streamOfChunks(chunks: string[]): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  let i = 0;
  return new ReadableStream({
    pull(controller) {
      if (i < chunks.length) {
        controller.enqueue(encoder.encode(chunks[i] ?? ''));
        i += 1;
      } else {
        controller.close();
      }
    },
  });
}

async function collect(s: ReadableStream<Uint8Array>) {
  const events = [];
  for await (const e of readSSE(s)) {
    events.push(e);
  }
  return events;
}

describe('readSSE', () => {
  it('parses a single event with data only (defaults to event="message")', async () => {
    const events = await collect(streamOf('data: hello\n\n'));
    expect(events).toStrictEqual([{ event: 'message', data: 'hello' }]);
  });

  it('parses a named event', async () => {
    const events = await collect(streamOf('event: chunk\ndata: payload\n\n'));
    expect(events).toStrictEqual([{ event: 'chunk', data: 'payload' }]);
  });

  it('parses multiple events in sequence', async () => {
    const events = await collect(
      streamOf('event: a\ndata: 1\n\nevent: b\ndata: 2\n\n'),
    );
    expect(events).toStrictEqual([
      { event: 'a', data: '1' },
      { event: 'b', data: '2' },
    ]);
  });

  it('joins multi-line data fields with \\n', async () => {
    const events = await collect(streamOf('data: line1\ndata: line2\ndata: line3\n\n'));
    expect(events[0]?.data).toBe('line1\nline2\nline3');
  });

  it('strips the single leading space after `field:`', async () => {
    const events = await collect(streamOf('data: spaced\n\n'));
    expect(events[0]?.data).toBe('spaced');
  });

  it('preserves further leading spaces', async () => {
    const events = await collect(streamOf('data:  two-spaces\n\n'));
    // First space stripped, second preserved.
    expect(events[0]?.data).toBe(' two-spaces');
  });

  it('handles CRLF line endings', async () => {
    const events = await collect(streamOf('event: chunk\r\ndata: x\r\n\r\n'));
    expect(events).toStrictEqual([{ event: 'chunk', data: 'x' }]);
  });

  it('ignores comment lines (starting with `:`)', async () => {
    const events = await collect(streamOf(': comment\ndata: hi\n\n'));
    expect(events).toStrictEqual([{ event: 'message', data: 'hi' }]);
  });

  it('ignores unknown fields', async () => {
    const events = await collect(streamOf('foo: bar\ndata: real\n\n'));
    expect(events[0]?.data).toBe('real');
  });

  it('captures id field', async () => {
    const events = await collect(streamOf('id: 42\ndata: x\n\n'));
    expect(events[0]).toStrictEqual({ event: 'message', data: 'x', id: '42' });
  });

  it('handles fields with no value after the colon', async () => {
    const events = await collect(streamOf('data:\n\n'));
    expect(events).toStrictEqual([{ event: 'message', data: '' }]);
  });

  it('survives splits across read boundaries', async () => {
    // Split mid-event to exercise the buffer-accumulation loop.
    const events = await collect(
      streamOfChunks([
        'event: chu',
        'nk\nda',
        'ta: one\nd',
        'ata: two\n\n',
      ]),
    );
    expect(events).toStrictEqual([{ event: 'chunk', data: 'one\ntwo' }]);
  });

  it('flushes a final event when the stream ends without a trailing blank line', async () => {
    const events = await collect(streamOf('event: chunk\ndata: tail'));
    expect(events).toStrictEqual([{ event: 'chunk', data: 'tail' }]);
  });

  it('emits an empty event with non-default name when only `event:` was set', async () => {
    const events = await collect(streamOf('event: heartbeat\n\n'));
    expect(events).toStrictEqual([{ event: 'heartbeat', data: '' }]);
  });

  it('does not emit a blank "message" / "" event for an entirely-empty record', async () => {
    const events = await collect(streamOf('\n\n'));
    expect(events).toStrictEqual([]);
  });

  it('aborts via signal mid-stream', async () => {
    const encoder = new TextEncoder();
    let pulled = 0;
    const stream = new ReadableStream<Uint8Array>({
      pull(controller) {
        pulled += 1;
        if (pulled > 5) {
          // Should never reach here once aborted.
          controller.close();
          return;
        }
        controller.enqueue(encoder.encode(`data: ${pulled}\n\n`));
      },
    });
    const controller = new AbortController();
    const seen: unknown[] = [];
    const iter = readSSE(stream, controller.signal);
    for await (const e of iter) {
      seen.push(e);
      if (seen.length === 2) controller.abort();
    }
    // We saw at least the two events; abort stops further pulls.
    expect(seen.length).toBeGreaterThanOrEqual(2);
    expect(pulled).toBeLessThan(20);
  });
});
