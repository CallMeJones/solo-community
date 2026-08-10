import type { ImportFileInput, ImportPreview, ImportRecord, ImportSource } from './types';

const MAX_CONTENT_CHARS = 24_000;
const PREVIEW_CHARS = 180;

export async function readImportFiles(files: FileList | File[]): Promise<ImportFileInput[]> {
  const list = Array.from(files);
  return Promise.all(
    list.map(async (file) => ({
      name: file.name,
      size: file.size,
      text: stripBom(await file.text()),
    })),
  );
}

export function parseImportFiles(source: ImportSource, files: ImportFileInput[]): ImportPreview {
  const preview: ImportPreview = {
    source,
    files: files.length,
    bytes: files.reduce((sum, file) => sum + file.size, 0),
    records: [],
    issues: [],
  };

  for (const file of files) {
    try {
      preview.records.push(...parseFile(source, file));
    } catch (err) {
      preview.issues.push({
        file: file.name,
        message: err instanceof Error ? err.message : String(err),
      });
    }
  }

  if (preview.records.length === 0 && preview.issues.length === 0 && files.length > 0) {
    preview.issues.push({ file: '(all files)', message: 'No importable records found.' });
  }

  return preview;
}

function parseFile(source: ImportSource, file: ImportFileInput): ImportRecord[] {
  switch (source) {
    case 'chatgpt':
      return parseChatGpt(file);
    case 'claude':
      return parseClaude(file);
    case 'bookmarks':
      return parseBookmarks(file);
    case 'markdown':
      return parseMarkdown(file);
  }
}

function parseChatGpt(file: ImportFileInput): ImportRecord[] {
  const json = parseJson(file);
  const conversations = Array.isArray(json) ? json : readArray(json, 'conversations');
  return conversations.flatMap((conversation, index) => {
    if (!isRecord(conversation)) return [];
    const title = stringValue(conversation.title) || `ChatGPT conversation ${index + 1}`;
    const sourceId = stringValue(conversation.id) || stableId('chatgpt', title, index);
    const timestampMs = timestampFromSeconds(conversation.create_time ?? conversation.update_time);
    const messages = chatGptMessages(conversation);
    if (messages.length === 0) return [];
    return [
      buildRecord({
        source: 'chatgpt',
        sourceType: 'import.chatgpt',
        sourceId,
        title,
        timestampMs,
        parts: [`ChatGPT conversation: ${title}`, ...messages],
      }),
    ];
  });
}

function chatGptMessages(conversation: Record<string, unknown>): string[] {
  const simple = readArray(conversation, 'messages', false);
  if (simple.length > 0) {
    return simple.map(formatSimpleMessage).filter(Boolean);
  }

  const mapping = conversation.mapping;
  if (!isRecord(mapping)) return [];
  return Object.values(mapping)
    .map((node) => {
      if (!isRecord(node) || !isRecord(node.message)) return '';
      return formatChatGptMappingMessage(node.message);
    })
    .filter(Boolean);
}

function formatChatGptMappingMessage(message: Record<string, unknown>): string {
  const role = isRecord(message.author) ? stringValue(message.author.role) : '';
  const content = isRecord(message.content) ? message.content : {};
  const parts = Array.isArray(content.parts) ? content.parts.map(stringValue).filter(Boolean) : [];
  if (!role || parts.length === 0) return '';
  return `${role}: ${parts.join('\n')}`;
}

function parseClaude(file: ImportFileInput): ImportRecord[] {
  const json = parseJson(file);
  const conversations = Array.isArray(json) ? json : readArray(json, 'conversations');
  return conversations.flatMap((conversation, index) => {
    if (!isRecord(conversation)) return [];
    const title =
      stringValue(conversation.name) ||
      stringValue(conversation.title) ||
      `Claude conversation ${index + 1}`;
    const sourceId =
      stringValue(conversation.uuid) ||
      stringValue(conversation.id) ||
      stableId('claude', title, index);
    const timestampMs = timestampFromIso(conversation.created_at ?? conversation.updated_at);
    const rawMessages = readArray(conversation, 'chat_messages', false);
    const messages = (
      rawMessages.length > 0 ? rawMessages : readArray(conversation, 'messages', false)
    )
      .map(formatSimpleMessage)
      .filter(Boolean);
    if (messages.length === 0) return [];
    return [
      buildRecord({
        source: 'claude',
        sourceType: 'import.claude',
        sourceId,
        title,
        timestampMs,
        parts: [`Claude conversation: ${title}`, ...messages],
      }),
    ];
  });
}

function parseBookmarks(file: ImportFileInput): ImportRecord[] {
  const trimmed = file.text.trim();
  if (trimmed.startsWith('{') || trimmed.startsWith('[')) {
    return parseBookmarkJson(file);
  }
  return parseBookmarkHtml(file);
}

function parseBookmarkJson(file: ImportFileInput): ImportRecord[] {
  const json = parseJson(file);
  const records: ImportRecord[] = [];
  const visit = (value: unknown, folders: string[]) => {
    if (!isRecord(value)) return;
    const title = stringValue(value.name) || stringValue(value.title);
    const url = stringValue(value.url) || stringValue(value.uri);
    if (url) {
      records.push(bookmarkRecord(title || url, url, folders));
    }
    const nextFolders = url || !title ? folders : [...folders, title];
    const children = Array.isArray(value.children) ? value.children : [];
    for (const child of children) visit(child, nextFolders);
    if (isRecord(value.roots)) {
      for (const root of Object.values(value.roots)) visit(root, nextFolders);
    }
  };
  visit(json, []);
  return records;
}

function parseBookmarkHtml(file: ImportFileInput): ImportRecord[] {
  if (typeof DOMParser === 'undefined') {
    throw new Error('Bookmark HTML parsing requires DOMParser.');
  }
  const doc = new DOMParser().parseFromString(file.text, 'text/html');
  const anchors = Array.from(doc.querySelectorAll('a[href]'));
  return anchors.map((anchor) => {
    const title = anchor.textContent?.trim() || anchor.getAttribute('href') || 'Bookmark';
    const url = anchor.getAttribute('href') || '';
    const folders = bookmarkFolders(anchor);
    return bookmarkRecord(title, url, folders);
  });
}

function bookmarkFolders(anchor: Element): string[] {
  const folders: string[] = [];
  let current: Element | null = anchor.parentElement;
  while (current) {
    const heading = previousFolderHeading(current);
    if (heading) folders.unshift(heading);
    current = current.parentElement;
  }
  return folders;
}

function previousFolderHeading(element: Element): string | null {
  let sibling = element.previousElementSibling;
  while (sibling) {
    if (sibling.tagName.toLowerCase() === 'h3') return sibling.textContent?.trim() || null;
    sibling = sibling.previousElementSibling;
  }
  return null;
}

function bookmarkRecord(title: string, url: string, folders: string[]): ImportRecord {
  const folder = folders.filter(Boolean).join(' / ');
  return buildRecord({
    source: 'bookmarks',
    sourceType: 'import.bookmark',
    sourceId: stableId('bookmark', url, folder),
    title,
    parts: [`Bookmark: ${title}`, `URL: ${url}`, ...(folder ? [`Folder: ${folder}`] : [])],
  });
}

function parseMarkdown(file: ImportFileInput): ImportRecord[] {
  const title = markdownTitle(file.text) || file.name.replace(/\.[^.]+$/, '') || file.name;
  return [
    buildRecord({
      source: 'markdown',
      sourceType: 'import.markdown',
      sourceId: stableId('markdown', file.name, file.text.length),
      title,
      parts: [`Document: ${title}`, `Source file: ${file.name}`, file.text],
    }),
  ];
}

function markdownTitle(text: string): string | null {
  const heading = text.match(/^#\s+(.+)$/m);
  return heading?.[1]?.trim() || null;
}

function formatSimpleMessage(value: unknown): string {
  if (!isRecord(value)) return '';
  const role =
    stringValue(value.role) ||
    stringValue(value.sender) ||
    (isRecord(value.author)
      ? stringValue(value.author.role) || stringValue(value.author.name)
      : stringValue(value.author));
  const text =
    stringValue(value.text) ||
    stringValue(value.content) ||
    stringValue(value.message) ||
    stringValue(value.summary);
  if (!text) return '';
  return role ? `${role}: ${text}` : text;
}

function buildRecord({
  source,
  sourceType,
  sourceId,
  title,
  timestampMs,
  parts,
}: {
  source: ImportSource;
  sourceType: string;
  sourceId: string;
  title: string;
  timestampMs?: number;
  parts: string[];
}): ImportRecord {
  const raw = parts.filter(Boolean).join('\n\n').trim();
  const content = truncate(raw, MAX_CONTENT_CHARS);
  return {
    id: stableId(source, sourceId, content.length),
    source,
    sourceType,
    sourceId,
    title: title || sourceId,
    content,
    preview: truncate(content.replace(/\s+/g, ' '), PREVIEW_CHARS),
    timestampMs,
    bytes: new Blob([content]).size,
  };
}

function parseJson(file: ImportFileInput): unknown {
  try {
    return JSON.parse(file.text);
  } catch (err) {
    throw new Error(
      `Invalid JSON in ${file.name}: ${err instanceof Error ? err.message : String(err)}`,
    );
  }
}

function readArray(record: unknown, key: string, required = true): unknown[] {
  if (isRecord(record) && Array.isArray(record[key])) return record[key];
  if (required) throw new Error(`Expected ${key} array.`);
  return [];
}

function timestampFromSeconds(value: unknown): number | undefined {
  return typeof value === 'number' && Number.isFinite(value) ? Math.round(value * 1000) : undefined;
}

function timestampFromIso(value: unknown): number | undefined {
  if (typeof value !== 'string') return undefined;
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : undefined;
}

function stringValue(value: unknown): string {
  return typeof value === 'string' ? value.trim() : '';
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function stripBom(text: string): string {
  return text.replace(/^\uFEFF/, '');
}

function truncate(text: string, max: number): string {
  if (text.length <= max) return text;
  return `${text.slice(0, max - 28).trimEnd()}\n\n[truncated for import preview]`;
}

function stableId(...parts: Array<string | number>): string {
  let hash = 2166136261;
  const input = parts.join('\u001f');
  for (let i = 0; i < input.length; i += 1) {
    hash ^= input.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(16).padStart(8, '0');
}
