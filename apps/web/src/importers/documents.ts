export const SEARCHABLE_DOCUMENT_EXTENSIONS = [
  'md',
  'markdown',
  'txt',
  'rs',
  'py',
  'toml',
  'yaml',
  'yml',
  'json',
  'jsonl',
  'ndjson',
  'pdf',
  'html',
  'htm',
  'csv',
  'tsv',
  'xlsx',
  'docx',
  'pptx',
] as const;

export const METADATA_DOCUMENT_EXTENSIONS = [
  'png',
  'jpg',
  'jpeg',
  'webp',
  'tif',
  'tiff',
  'blend',
  'zip',
  'gltf',
  'glb',
  'obj',
  'stl',
] as const;

export const SUPPORTED_DOCUMENT_EXTENSIONS = [
  ...SEARCHABLE_DOCUMENT_EXTENSIONS,
  ...METADATA_DOCUMENT_EXTENSIONS,
] as const;

export const DOCUMENT_FILE_ACCEPT = SUPPORTED_DOCUMENT_EXTENSIONS.map(
  (extension) => `.${extension}`,
).join(',');

export type DocumentSupport = 'searchable' | 'metadata_only' | 'unsupported';

const SEARCHABLE = new Set<string>(SEARCHABLE_DOCUMENT_EXTENSIONS);
const METADATA_ONLY = new Set<string>(METADATA_DOCUMENT_EXTENSIONS);

export function documentExtension(filename: string): string | null {
  const match = filename
    .trim()
    .toLowerCase()
    .match(/\.([^.]+)$/);
  return match?.[1] ?? null;
}

export function documentSupport(filename: string): DocumentSupport {
  const extension = documentExtension(filename);
  if (!extension) return 'unsupported';
  if (SEARCHABLE.has(extension)) return 'searchable';
  if (METADATA_ONLY.has(extension)) return 'metadata_only';
  return 'unsupported';
}

export function documentSupportLabel(support: DocumentSupport): string {
  switch (support) {
    case 'searchable':
      return 'Searchable text';
    case 'metadata_only':
      return 'Metadata / manifest only';
    case 'unsupported':
      return 'No default extractor';
  }
}
