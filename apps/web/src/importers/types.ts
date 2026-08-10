export type ImportSource = 'chatgpt' | 'claude' | 'bookmarks' | 'markdown';

export interface ImportFileInput {
  name: string;
  text: string;
  size: number;
}

export interface ImportRecord {
  id: string;
  source: ImportSource;
  sourceType: string;
  sourceId: string;
  title: string;
  content: string;
  preview: string;
  timestampMs?: number;
  bytes: number;
}

export interface ImportParseIssue {
  file: string;
  message: string;
}

export interface ImportPreview {
  source: ImportSource;
  files: number;
  bytes: number;
  records: ImportRecord[];
  issues: ImportParseIssue[];
}
