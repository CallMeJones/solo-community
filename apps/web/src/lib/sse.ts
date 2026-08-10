/**
 * Tiny SSE parser for fetch responses.
 *
 * Browser `EventSource` cannot attach Solo's bearer header, so authenticated
 * graph streams use `fetch` and parse the response body manually. Handles the
 * `event:`, `data:`, `id:`, and `retry:` fields plus blank-line
 * dispatch. Stops on stream end.
 *
 * Use:
 *   for await (const event of readSSE(response.body)) {
 *     // event.event, event.data
 *   }
 */

export interface SseEvent {
  event: string;
  data: string;
  id?: string;
}

export async function* readSSE(
  stream: ReadableStream<Uint8Array>,
  signal?: AbortSignal,
): AsyncGenerator<SseEvent, void, void> {
  const reader = stream.getReader();
  const decoder = new TextDecoder('utf-8');
  let buffer = '';
  let event = 'message';
  let data = '';
  let id: string | undefined;
  // Tracks whether ANY field has been set since the last dispatch. The SSE
  // spec says an event fires when a blank line is seen and at least one
  // field was present, even if `data` is the empty string.
  let pending = false;

  const onAbort = () => reader.cancel().catch(() => {});
  signal?.addEventListener('abort', onAbort);

  // Called for non-blank lines only — the blank-line dispatch happens in
  // the outer loop because it can `yield` directly.
  const processLine = (line: string): void => {
    if (line.startsWith(':')) return; // comment
    const colon = line.indexOf(':');
    const field = colon === -1 ? line : line.slice(0, colon);
    let value = colon === -1 ? '' : line.slice(colon + 1);
    if (value.startsWith(' ')) value = value.slice(1);
    switch (field) {
      case 'event':
        event = value;
        pending = true;
        break;
      case 'data':
        data = data.length === 0 ? value : `${data}\n${value}`;
        pending = true;
        break;
      case 'id':
        id = value;
        pending = true;
        break;
      case 'retry':
        // Spec says ignore unless it's a digit string; we don't surface it.
        break;
      default:
        break;
    }
  };

  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffer += decoder.decode(value, { stream: true });

      while (true) {
        const nl = buffer.indexOf('\n');
        if (nl === -1) break;
        const rawLine = buffer.slice(0, nl);
        buffer = buffer.slice(nl + 1);
        // Trim a single trailing \r (handle CRLF gracefully).
        const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;

        if (line === '') {
          if (pending) {
            yield { event, data, ...(id !== undefined ? { id } : {}) };
            event = 'message';
            data = '';
            id = undefined;
            pending = false;
          }
          continue;
        }
        processLine(line);
      }
    }

    // Stream ended mid-record. Process any final partial line that lacks
    // its terminating newline (e.g. servers that close without the
    // closing blank line).
    if (buffer.length > 0) {
      const line = buffer.endsWith('\r') ? buffer.slice(0, -1) : buffer;
      if (line.length > 0) processLine(line);
      buffer = '';
    }
    // Flush a final event if any field was set since the last dispatch.
    if (pending) {
      yield { event, data, ...(id !== undefined ? { id } : {}) };
    }
  } finally {
    signal?.removeEventListener('abort', onAbort);
    reader.releaseLock();
  }
}
