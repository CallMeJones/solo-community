import { describe, expect, it } from 'vitest';
import { parseImportFiles } from '../src/importers/parse';
import type { ImportFileInput } from '../src/importers/types';

function file(name: string, text: string): ImportFileInput {
  return { name, text, size: new Blob([text]).size };
}

describe('parseImportFiles', () => {
  it('extracts ChatGPT mapping conversations', () => {
    const preview = parseImportFiles('chatgpt', [
      file(
        'conversations.json',
        JSON.stringify([
          {
            id: 'chat-1',
            title: 'Solo plan',
            create_time: 1770000000,
            mapping: {
              a: {
                message: {
                  author: { role: 'user' },
                  content: { parts: ['What should Solo build next?'] },
                },
              },
              b: {
                message: {
                  author: { role: 'assistant' },
                  content: { parts: ['Build an import UI.'] },
                },
              },
            },
          },
        ]),
      ),
    ]);

    expect(preview.records).toHaveLength(1);
    expect(preview.records[0]).toMatchObject({
      title: 'Solo plan',
      sourceType: 'import.chatgpt',
      sourceId: 'chat-1',
    });
    expect(preview.records[0].content).toContain('user: What should Solo build next?');
  });

  it('extracts Claude chat_messages conversations', () => {
    const preview = parseImportFiles('claude', [
      file(
        'conversations.json',
        JSON.stringify([
          {
            uuid: 'claude-1',
            name: 'Desktop shell',
            chat_messages: [
              { sender: 'human', text: 'Open Assistant' },
              { sender: 'assistant', text: 'Opening Assistant.' },
            ],
          },
        ]),
      ),
    ]);

    expect(preview.records).toHaveLength(1);
    expect(preview.records[0].sourceType).toBe('import.claude');
    expect(preview.records[0].content).toContain('assistant: Opening Assistant.');
  });

  it('extracts bookmarks from Netscape HTML', () => {
    const preview = parseImportFiles('bookmarks', [
      file(
        'bookmarks.html',
        '<DL><p><DT><H3>Solo</H3><DL><p><DT><A HREF="https://example.test/solo">Solo Docs</A></DL></DL>',
      ),
    ]);

    expect(preview.records).toHaveLength(1);
    expect(preview.records[0]).toMatchObject({
      title: 'Solo Docs',
      sourceType: 'import.bookmark',
    });
    expect(preview.records[0].content).toContain('https://example.test/solo');
  });

  it('imports markdown files as one record per file', () => {
    const preview = parseImportFiles('markdown', [file('note.md', '# Launch\n\nShip import UX.')]);

    expect(preview.records).toHaveLength(1);
    expect(preview.records[0]).toMatchObject({
      title: 'Launch',
      sourceType: 'import.markdown',
    });
  });
});
