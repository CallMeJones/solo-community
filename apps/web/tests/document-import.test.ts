import { describe, expect, it } from 'vitest';
import {
  DOCUMENT_FILE_ACCEPT,
  documentExtension,
  documentSupport,
} from '../src/importers/documents';

describe('document import support', () => {
  it('classifies default searchable formats case-insensitively', () => {
    expect(documentSupport('notes.md')).toBe('searchable');
    expect(documentSupport('REPORT.PDF')).toBe('searchable');
    expect(documentSupport('briefing.pptx')).toBe('searchable');
  });

  it('distinguishes metadata-only formats from unsupported files', () => {
    expect(documentSupport('photo.png')).toBe('metadata_only');
    expect(documentSupport('scene.glb')).toBe('metadata_only');
    expect(documentSupport('program.exe')).toBe('unsupported');
    expect(documentSupport('README')).toBe('unsupported');
  });

  it('publishes the supported formats to the browser picker', () => {
    expect(DOCUMENT_FILE_ACCEPT).toContain('.docx');
    expect(DOCUMENT_FILE_ACCEPT).toContain('.xlsx');
    expect(DOCUMENT_FILE_ACCEPT).toContain('.zip');
    expect(DOCUMENT_FILE_ACCEPT).not.toContain('.exe');
    expect(documentExtension('archive.tar.gz')).toBe('gz');
  });
});
