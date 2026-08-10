import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useMemo, useRef, useState } from 'react';
import {
  captureApiConnection,
  DocumentImportCleanupUncertainError,
  DocumentImportRecoveryTerminalError,
  DocumentImportUncertainError,
  forgetDocument,
  forgetRetainedAsset,
  importBrowserDocument,
  importDocumentPath,
  isAbortError,
  listAssetLifecycle,
  listDocumentLifecycle,
  rememberMemory,
  resumeUncertainDocumentImport,
  type BrowserDocumentImportProgress,
} from '../api/client';
import type {
  AssetLifecycleSummary,
  DocumentLifecycleSummary,
  NativeImportFile,
  NativeImportResponse,
  NativeImportResult,
  StagedDocumentIngestResponse,
} from '../api/types';
import {
  DOCUMENT_FILE_ACCEPT,
  documentSupport,
  documentSupportLabel,
  type DocumentSupport,
} from '../importers/documents';
import { parseImportFiles, readImportFiles } from '../importers/parse';
import type { ImportPreview, ImportRecord, ImportSource } from '../importers/types';
import { formatBytes } from '../lib/formatBytes';
import { useSettingsStore } from '../store/settingsStore';

type BrowserImportSource = 'documents' | ImportSource;
type RetentionChoice = 'solo_default' | 'retain' | 'discard';

const FILE_SOURCES: Array<{ id: BrowserImportSource; label: string; accept: string }> = [
  { id: 'documents', label: 'Documents', accept: DOCUMENT_FILE_ACCEPT },
  { id: 'chatgpt', label: 'ChatGPT', accept: '.json,application/json' },
  { id: 'claude', label: 'Claude', accept: '.json,application/json' },
  { id: 'bookmarks', label: 'Bookmarks', accept: '.html,.htm,.json,text/html,application/json' },
  { id: 'markdown', label: 'Markdown/Text', accept: '.md,.markdown,.txt,text/markdown,text/plain' },
];

type NativePathSource = 'documents' | 'markdown_text' | 'json' | 'chatgpt' | 'claude' | 'bookmarks';

const PATH_SOURCES: Array<{ id: NativePathSource; label: string }> = [
  { id: 'documents', label: 'Local files / Codex project' },
  { id: 'markdown_text', label: 'Markdown/Text only' },
  { id: 'json', label: 'JSON / Codex logs' },
  { id: 'chatgpt', label: 'ChatGPT export' },
  { id: 'claude', label: 'Claude export' },
  { id: 'bookmarks', label: 'Bookmarks export' },
];

const HISTORY_KEY = 'solo.import.history';
const HISTORY_LIMIT = 8;
const NATIVE_IMPORT_MAX_FILES = 500;
const DOCUMENT_RECOVERY_KEY = 'solo.import.document-recovery.session.v1';
const DOCUMENT_RECOVERY_LIMIT = 20;
const DOCUMENT_RECOVERY_MAX_AGE_MS = 24 * 60 * 60 * 1_000;
const RECOVERY_DOCUMENT_LABEL = 'Interrupted document upload';

interface ImportResult {
  record: ImportRecord;
  memoryId?: string;
  error?: string;
}

interface ImportHistoryEntry {
  id: string;
  source: BrowserImportSource | NativePathSource | 'browser_documents' | 'local';
  atMs: number;
  records: number;
  imported: number;
  deduped?: number;
  failed: number;
}

type DocumentImportStatus =
  | 'ready'
  | BrowserDocumentImportProgress['stage']
  | 'extracted'
  | 'stored_unparsed'
  | 'cancelled'
  | 'uncertain'
  | 'failed';

interface DocumentImportItem {
  id: string;
  file?: File;
  filename: string;
  sizeBytes: number;
  support: DocumentSupport;
  status: DocumentImportStatus;
  bytesSent: number;
  result?: StagedDocumentIngestResponse;
  error?: string;
  documentForgotten?: boolean;
  assetDeleted?: boolean;
  lifecycleAction?: 'forgetting_document' | 'deleting_asset';
  lifecycleError?: string;
  resumeUploadId?: string;
  resumeStagedUri?: string;
  resumeStoreOriginalFile?: boolean;
  resumeApiUrl?: string;
  resumeConnectionRevision?: number;
  recoveryOnly?: boolean;
}

interface DocumentImportOutcome {
  id: string;
  result?: StagedDocumentIngestResponse;
  error?: string;
  uncertain?: boolean;
}

interface PersistedDocumentRecovery {
  version: 1;
  sizeBytes: number;
  uploadId: string;
  stagedUri: string | null;
  storeOriginalFile: boolean;
  apiUrl: string;
  updatedAtMs: number;
}

interface NativeImportVariables {
  dryRun: boolean;
  path: string;
  source: NativePathSource;
}

function isDocumentImportable(item: DocumentImportItem): boolean {
  if (!item.file || item.sizeBytes === 0) return false;
  if (item.status === 'ready') return true;
  // A transport failure can be retried. An extraction failure already has a
  // committed asset/document result, so re-uploading it would create another
  // retained asset rather than retrying extraction in place.
  return ['failed', 'cancelled'].includes(item.status) && !item.result;
}

export function ImportView() {
  const queryClient = useQueryClient();
  const apiUrl = useSettingsStore((s) => s.apiUrl);
  const connectionRevision = useSettingsStore((s) => s.connectionRevision);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const documentAbortControllers = useRef(new Map<string, AbortController>());
  const [fileSource, setFileSource] = useState<BrowserImportSource>('documents');
  const [nativeSource, setNativeSource] = useState<NativePathSource>('documents');
  const [rawFiles, setRawFiles] = useState<File[]>([]);
  const [documentItems, setDocumentItems] =
    useState<DocumentImportItem[]>(loadDocumentRecoveryItems);
  const [retentionChoice, setRetentionChoice] = useState<RetentionChoice>('solo_default');
  const [showLifecycle, setShowLifecycle] = useState(false);
  const [lifecycleAction, setLifecycleAction] = useState<string | null>(null);
  const [lifecycleError, setLifecycleError] = useState<string | null>(null);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  const [parseError, setParseError] = useState<string | null>(null);
  const [results, setResults] = useState<ImportResult[]>([]);
  const [history, setHistory] = useState<ImportHistoryEntry[]>(loadImportHistory);
  const [nativePath, setNativePath] = useState('');
  const [nativeResult, setNativeResult] = useState<NativeImportResponse | null>(null);
  const nativePathPlaceholder = useMemo(defaultNativePathPlaceholder, []);

  useEffect(() => {
    const controllers = documentAbortControllers.current;
    return () => {
      for (const controller of controllers.values()) controller.abort();
      controllers.clear();
    };
  }, []);

  useEffect(() => {
    const currentApiUrl = privacySafeApiUrl(apiUrl);
    setDocumentItems((current) => {
      let changed = false;
      const next = current.map((item) => {
        const belongsToAnotherConnection =
          item.resumeUploadId !== undefined &&
          ((item.resumeApiUrl ?? currentApiUrl) !== currentApiUrl ||
            (item.resumeConnectionRevision ?? connectionRevision) !== connectionRevision);
        if (!belongsToAnotherConnection || item.recoveryOnly) return item;
        changed = true;
        return {
          ...item,
          file: undefined,
          filename: RECOVERY_DOCUMENT_LABEL,
          support: 'unsupported' as const,
          recoveryOnly: true,
        };
      });
      return changed ? next : current;
    });
  }, [apiUrl, connectionRevision]);

  const selectedRecords = useMemo(
    () => preview?.records.filter((record) => selectedIds.has(record.id)) ?? [],
    [preview, selectedIds],
  );
  const selectedFileSource = FILE_SOURCES.find((item) => item.id === fileSource) ?? FILE_SOURCES[0];
  const selectedPathSource =
    PATH_SOURCES.find((item) => item.id === nativeSource) ?? PATH_SOURCES[0];

  const updateDocumentItem = (
    id: string,
    update: Partial<DocumentImportItem> | ((item: DocumentImportItem) => DocumentImportItem),
  ) => {
    setDocumentItems((current) =>
      current.map((item) => {
        if (item.id !== id) return item;
        return typeof update === 'function' ? update(item) : { ...item, ...update };
      }),
    );
  };

  const lifecycleCatalog = useQuery({
    queryKey: ['document-lifecycle-catalog', apiUrl, connectionRevision],
    queryFn: async ({ signal }) => {
      const [documents, assets] = await Promise.all([
        listDocumentLifecycle({ signal }),
        listAssetLifecycle({ signal }),
      ]);
      return {
        documents: documents.items,
        documentsTruncated: documents.truncated,
        assets: assets.items,
        assetsTruncated: assets.truncated,
      };
    },
    enabled: showLifecycle,
    retry: false,
  });

  const importMutation = useMutation({
    mutationFn: async (records: ImportRecord[]) => {
      const nextResults: ImportResult[] = [];
      const connection = captureApiConnection();
      for (const record of records) {
        try {
          const response = await rememberMemory(
            {
              content: record.content,
              source_type: record.sourceType,
              source_id: record.sourceId,
              salience: 0.55,
            },
            { connection },
          );
          nextResults.push({ record, memoryId: response.memory_id });
          setResults([...nextResults]);
        } catch (err) {
          nextResults.push({
            record,
            error: err instanceof Error ? err.message : String(err),
          });
          setResults([...nextResults]);
        }
      }
      return nextResults;
    },
    onSuccess: (nextResults) => {
      const entry = {
        id: `${Date.now()}:${fileSource}`,
        source: fileSource,
        atMs: Date.now(),
        records: nextResults.length,
        imported: nextResults.filter((result) => result.memoryId).length,
        deduped: 0,
        failed: nextResults.filter((result) => result.error).length,
      };
      setHistory((current) => saveImportHistory([entry, ...current].slice(0, HISTORY_LIMIT)));
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
    },
  });

  const documentMutation = useMutation({
    mutationFn: async (items: DocumentImportItem[]) => {
      const outcomes: DocumentImportOutcome[] = [];
      const connection = captureApiConnection();
      const batchConnectionRevision = connectionRevision;
      const recoveryApiUrl = privacySafeApiUrl(connection.apiUrl);
      for (const item of items) {
        const file = item.file;
        if (!file || item.sizeBytes === 0) {
          const error = 'Empty files cannot be imported.';
          updateDocumentItem(item.id, { status: 'failed', error });
          outcomes.push({ id: item.id, error });
          continue;
        }
        if (item.support === 'unsupported' && retentionChoice === 'discard') {
          const error =
            'This file has no default extractor. Enable original-file retention to let Solo retain it without searchable content.';
          updateDocumentItem(item.id, { status: 'failed', error });
          outcomes.push({ id: item.id, error });
          continue;
        }
        const controller = new AbortController();
        let recoveryUploadId: string | undefined;
        documentAbortControllers.current.set(item.id, controller);
        try {
          const result = await importBrowserDocument(file, {
            connection,
            signal: controller.signal,
            storeOriginalFile:
              retentionChoice === 'solo_default' ? undefined : retentionChoice === 'retain',
            onProgress: (progress) =>
              updateDocumentItem(item.id, {
                status: progress.stage,
                bytesSent: progress.bytesSent,
                error: undefined,
              }),
            onRecoveryCheckpoint: (checkpoint) => {
              recoveryUploadId = checkpoint.uploadId;
              const persisted: PersistedDocumentRecovery = {
                version: 1,
                sizeBytes: item.sizeBytes,
                uploadId: checkpoint.uploadId,
                stagedUri: checkpoint.stagedUri,
                storeOriginalFile: checkpoint.storeOriginalFile,
                apiUrl: recoveryApiUrl,
                updatedAtMs: Date.now(),
              };
              saveDocumentRecoveryCheckpoint(persisted);
              updateDocumentItem(item.id, {
                resumeUploadId: checkpoint.uploadId,
                resumeStagedUri: checkpoint.stagedUri ?? undefined,
                resumeStoreOriginalFile: checkpoint.storeOriginalFile,
                resumeApiUrl: recoveryApiUrl,
                resumeConnectionRevision: batchConnectionRevision,
              });
            },
          });
          if (recoveryUploadId) removeDocumentRecoveryCheckpoint(recoveryUploadId);
          updateDocumentItem(item.id, {
            status: result.extraction_status,
            bytesSent: item.sizeBytes,
            result,
            resumeUploadId: undefined,
            resumeStagedUri: undefined,
            resumeStoreOriginalFile: undefined,
            resumeApiUrl: undefined,
            resumeConnectionRevision: undefined,
            error:
              result.extraction_status === 'failed'
                ? (result.extraction_error ?? 'Document extraction failed.')
                : undefined,
          });
          outcomes.push({
            id: item.id,
            result,
            error:
              result.extraction_status === 'failed'
                ? (result.extraction_error ?? 'Document extraction failed.')
                : undefined,
          });
        } catch (err) {
          const error = err instanceof Error ? err.message : String(err);
          if (err instanceof DocumentImportCleanupUncertainError) {
            updateDocumentItem(item.id, { status: 'uncertain', error });
          } else if (isAbortError(err)) {
            if (recoveryUploadId) removeDocumentRecoveryCheckpoint(recoveryUploadId);
            updateDocumentItem(item.id, {
              status: 'cancelled',
              error: 'Upload cancelled. Solo discarded the uncommitted staged bytes.',
            });
          } else if (err instanceof DocumentImportUncertainError) {
            recoveryUploadId = err.uploadId;
            updateDocumentRecoveryCheckpoint(err.uploadId, {
              stagedUri: err.stagedUri,
              storeOriginalFile: err.storeOriginalFile ?? undefined,
              updatedAtMs: Date.now(),
            });
            updateDocumentItem(item.id, {
              status: 'uncertain',
              error,
              resumeUploadId: err.uploadId,
              resumeStagedUri: err.stagedUri ?? undefined,
              resumeStoreOriginalFile: err.storeOriginalFile ?? undefined,
              resumeApiUrl: recoveryApiUrl,
              resumeConnectionRevision: batchConnectionRevision,
            });
          } else {
            if (recoveryUploadId) removeDocumentRecoveryCheckpoint(recoveryUploadId);
            updateDocumentItem(item.id, { status: 'failed', error });
          }
          outcomes.push({
            id: item.id,
            error,
            uncertain:
              err instanceof DocumentImportCleanupUncertainError ||
              err instanceof DocumentImportUncertainError,
          });
        } finally {
          documentAbortControllers.current.delete(item.id);
        }
      }
      return outcomes;
    },
    onSuccess: (outcomes) => {
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
      const terminalOutcomes = outcomes.filter((outcome) => !outcome.uncertain);
      if (terminalOutcomes.length === 0) return;
      const successes = terminalOutcomes.filter((outcome) => outcome.result && !outcome.error);
      const deduped = successes.filter((outcome) => outcome.result?.deduped).length;
      const entry = {
        id: `${Date.now()}:documents`,
        source: 'browser_documents' as const,
        atMs: Date.now(),
        records: terminalOutcomes.length,
        imported: successes.length - deduped,
        deduped,
        failed: terminalOutcomes.filter((outcome) => outcome.error).length,
      };
      setHistory((current) => saveImportHistory([entry, ...current].slice(0, HISTORY_LIMIT)));
    },
  });

  const nativeMutation = useMutation({
    mutationFn: async ({ dryRun, path, source }: NativeImportVariables) =>
      importDocumentPath(
        {
          path,
          source: daemonPathImportSource(source),
          dry_run: dryRun,
          recursive: true,
          max_files: NATIVE_IMPORT_MAX_FILES,
        },
        {},
      ),
    onSuccess: (result, variables) => {
      setNativeResult(result);
      if (!result.dry_run) {
        const entry = {
          id: `${Date.now()}:${variables.source}:local`,
          source: variables.source,
          atMs: Date.now(),
          records: result.total_files,
          imported: result.imported,
          deduped: result.deduped,
          failed: result.failed,
        };
        setHistory((current) => saveImportHistory([entry, ...current].slice(0, HISTORY_LIMIT)));
        void queryClient.invalidateQueries({ queryKey: ['graph'] });
      }
    },
  });
  const sourceControlsBusy = documentMutation.isPending || importMutation.isPending;
  const nativePathReady =
    nativePath.trim().length > 0 && !nativeMutation.isPending && !documentMutation.isPending;
  const documentImportableCount = documentItems.filter(isDocumentImportable).length;

  const handleFiles = async (fileList: FileList | null) => {
    if (sourceControlsBusy || !fileList || fileList.length === 0) return;
    const selected = Array.from(fileList);
    setRawFiles(selected);
    setParseError(null);
    setResults([]);
    if (fileSource === 'documents') {
      setPreview(null);
      setSelectedIds(new Set());
      setDocumentItems(
        mergeDocumentItems(
          loadDocumentRecoveryItems(),
          createDocumentItems(selected),
          privacySafeApiUrl(apiUrl),
        ),
      );
      return;
    }
    try {
      const nextFiles = await readImportFiles(selected);
      const nextPreview = parseImportFiles(fileSource, nextFiles);
      setDocumentItems([]);
      setPreview(nextPreview);
      setSelectedIds(new Set(nextPreview.records.map((record) => record.id)));
    } catch (err) {
      setPreview(null);
      setSelectedIds(new Set());
      setParseError(err instanceof Error ? err.message : String(err));
    }
  };

  const handleSourceChange = async (next: BrowserImportSource) => {
    if (sourceControlsBusy) return;
    setFileSource(next);
    setResults([]);
    setParseError(null);
    if (next === 'documents') {
      setPreview(null);
      setSelectedIds(new Set());
      setDocumentItems(
        mergeDocumentItems(
          loadDocumentRecoveryItems(),
          createDocumentItems(rawFiles),
          privacySafeApiUrl(apiUrl),
        ),
      );
      return;
    }
    setDocumentItems([]);
    if (rawFiles.length > 0) {
      try {
        const nextFiles = await readImportFiles(rawFiles);
        const nextPreview = parseImportFiles(next, nextFiles);
        setPreview(nextPreview);
        setSelectedIds(new Set(nextPreview.records.map((record) => record.id)));
      } catch (err) {
        setPreview(null);
        setSelectedIds(new Set());
        setParseError(err instanceof Error ? err.message : String(err));
      }
    }
  };

  const toggleRecord = (id: string) => {
    setSelectedIds((current) => {
      const next = new Set(current);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  };

  const selectAll = () => {
    setSelectedIds(new Set(preview?.records.map((record) => record.id) ?? []));
  };

  const clearSelection = () => {
    setSelectedIds(new Set());
  };

  const handleForgetDocument = async (item: DocumentImportItem) => {
    const documentId = item.result?.document_id;
    if (!documentId) return;
    if (
      !window.confirm(
        `Forget the searchable document for "${item.filename}"? It will stop appearing in search, but Solo keeps soft-deleted chunk rows and any retained original file is separate.`,
      )
    ) {
      return;
    }
    updateDocumentItem(item.id, {
      lifecycleAction: 'forgetting_document',
      lifecycleError: undefined,
    });
    try {
      await forgetDocument(documentId);
      updateDocumentItem(item.id, {
        documentForgotten: true,
        lifecycleAction: undefined,
      });
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
    } catch (err) {
      updateDocumentItem(item.id, {
        lifecycleAction: undefined,
        lifecycleError: err instanceof Error ? err.message : String(err),
      });
    }
  };

  const handleDeleteAsset = async (item: DocumentImportItem) => {
    const assetId = item.result?.asset?.asset_id;
    if (!assetId) return;
    if (
      !window.confirm(
        `Delete the retained original bytes for "${item.filename}"? Searchable document text remains unless you forget it separately. Solo keeps a deleted provenance record.`,
      )
    ) {
      return;
    }
    updateDocumentItem(item.id, {
      lifecycleAction: 'deleting_asset',
      lifecycleError: undefined,
    });
    try {
      await forgetRetainedAsset(assetId);
      updateDocumentItem(item.id, {
        assetDeleted: true,
        lifecycleAction: undefined,
      });
    } catch (err) {
      updateDocumentItem(item.id, {
        lifecycleAction: undefined,
        lifecycleError: err instanceof Error ? err.message : String(err),
      });
    }
  };

  const handleCancelDocument = (item: DocumentImportItem) => {
    documentAbortControllers.current.get(item.id)?.abort();
  };

  const handleResumeDocument = async (item: DocumentImportItem) => {
    if (item.resumeUploadId === undefined || item.resumeStoreOriginalFile === undefined) return;
    const recoveryApiUrl = item.resumeApiUrl ?? privacySafeApiUrl(apiUrl);
    if (recoveryApiUrl !== privacySafeApiUrl(apiUrl)) {
      updateDocumentItem(item.id, {
        error: `This upload belongs to the Solo connection at ${recoveryApiUrl}. Switch back to that connection before recovering it.`,
      });
      return;
    }
    updateDocumentItem(item.id, { status: 'extracting', error: undefined });
    try {
      const result = await resumeUncertainDocumentImport(
        item.resumeUploadId,
        item.resumeStagedUri ?? null,
        item.resumeStoreOriginalFile,
        {},
      );
      removeDocumentRecoveryCheckpoint(item.resumeUploadId);
      const extractionError =
        result.extraction_status === 'failed'
          ? (result.extraction_error ?? 'Document extraction failed.')
          : undefined;
      updateDocumentItem(item.id, {
        status: result.extraction_status,
        bytesSent: item.sizeBytes,
        result,
        resumeUploadId: undefined,
        resumeStagedUri: undefined,
        resumeStoreOriginalFile: undefined,
        resumeApiUrl: undefined,
        resumeConnectionRevision: undefined,
        error: extractionError,
      });
      const deduped = !extractionError && Boolean(result.deduped);
      const entry: ImportHistoryEntry = {
        id: `${Date.now()}:documents:recovered`,
        source: 'browser_documents',
        atMs: Date.now(),
        records: 1,
        imported: extractionError || deduped ? 0 : 1,
        deduped: deduped ? 1 : 0,
        failed: extractionError ? 1 : 0,
      };
      setHistory((currentHistory) =>
        saveImportHistory([entry, ...currentHistory].slice(0, HISTORY_LIMIT)),
      );
      await queryClient.invalidateQueries({ queryKey: ['graph'] });
      if (showLifecycle) await lifecycleCatalog.refetch();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      if (err instanceof DocumentImportRecoveryTerminalError) {
        removeDocumentRecoveryCheckpoint(item.resumeUploadId);
        updateDocumentItem(item.id, {
          status: err.reason === 'incomplete' ? 'uncertain' : 'failed',
          error: message,
          resumeUploadId: undefined,
          resumeStagedUri: undefined,
          resumeStoreOriginalFile: undefined,
          resumeApiUrl: undefined,
          resumeConnectionRevision: undefined,
        });
        return;
      }
      if (err instanceof DocumentImportUncertainError) {
        updateDocumentRecoveryCheckpoint(item.resumeUploadId, {
          stagedUri: err.stagedUri ?? item.resumeStagedUri ?? null,
          storeOriginalFile: err.storeOriginalFile ?? item.resumeStoreOriginalFile,
          updatedAtMs: Date.now(),
        });
      }
      updateDocumentItem(item.id, {
        status: 'uncertain',
        error: message,
        ...(err instanceof DocumentImportUncertainError
          ? {
              resumeUploadId: err.uploadId,
              resumeStagedUri: err.stagedUri ?? item.resumeStagedUri,
              resumeStoreOriginalFile: err.storeOriginalFile ?? item.resumeStoreOriginalFile,
            }
          : {}),
      });
    }
  };

  const handleCatalogForgetDocument = async (document: DocumentLifecycleSummary) => {
    if (
      !window.confirm(
        `Forget searchable document "${document.title ?? document.doc_id}"? Retained source bytes are managed separately.`,
      )
    ) {
      return;
    }
    setLifecycleAction(`document:${document.doc_id}`);
    setLifecycleError(null);
    try {
      await forgetDocument(document.doc_id);
      await lifecycleCatalog.refetch();
      void queryClient.invalidateQueries({ queryKey: ['graph'] });
    } catch (err) {
      setLifecycleError(err instanceof Error ? err.message : String(err));
    } finally {
      setLifecycleAction(null);
    }
  };

  const handleCatalogDeleteAsset = async (asset: AssetLifecycleSummary) => {
    if (
      !window.confirm(
        `Delete retained source bytes for "${asset.filename ?? asset.asset_id}"? Provenance metadata remains.`,
      )
    ) {
      return;
    }
    setLifecycleAction(`asset:${asset.asset_id}`);
    setLifecycleError(null);
    try {
      await forgetRetainedAsset(asset.asset_id);
      await lifecycleCatalog.refetch();
    } catch (err) {
      setLifecycleError(err instanceof Error ? err.message : String(err));
    } finally {
      setLifecycleAction(null);
    }
  };

  return (
    <div className="grid min-h-full gap-4 xl:grid-cols-[320px_1fr]">
      <section className="rounded-lg border border-slate-800 bg-slate-900/45 p-4">
        <h2 className="text-sm font-semibold text-slate-100">Source</h2>
        <p className="mt-1 text-xs leading-5 text-slate-400">
          Import files into document memory, or convert supported exports into memory records.
        </p>
        <div className="mt-3 grid gap-2">
          {FILE_SOURCES.map((item) => (
            <button
              key={item.id}
              type="button"
              aria-pressed={fileSource === item.id}
              onClick={() => void handleSourceChange(item.id)}
              disabled={sourceControlsBusy}
              className={[
                'rounded-md border px-3 py-2 text-left text-sm transition-colors disabled:cursor-not-allowed disabled:opacity-50',
                fileSource === item.id
                  ? 'border-sky-500 bg-sky-950 text-sky-100'
                  : 'border-slate-800 bg-slate-950 text-slate-300 hover:border-slate-600',
              ].join(' ')}
            >
              {item.label}
            </button>
          ))}
        </div>

        <div className="mt-5">
          <label
            className="block text-xs font-medium uppercase text-slate-400"
            htmlFor="import-files"
          >
            Files
          </label>
          <input
            id="import-files"
            ref={fileInputRef}
            type="file"
            multiple
            accept={selectedFileSource.accept}
            onChange={(event) => void handleFiles(event.target.files)}
            disabled={sourceControlsBusy}
            className="mt-2 block w-full text-sm text-slate-300 file:mr-3 file:rounded-md file:border-0 file:bg-slate-800 file:px-3 file:py-2 file:text-sm file:font-medium file:text-slate-100 hover:file:bg-slate-700"
          />
          {fileSource === 'documents' && (
            <div className="mt-3 space-y-2 text-xs leading-5 text-slate-400">
              <p>
                Searchable by default: text, code, Markdown, JSON, PDF, HTML, CSV, Excel, Word, and
                PowerPoint. Images, archives, and 3D files currently contribute metadata or a
                manifest, not OCR or full media understanding.
              </p>
              <label className="block rounded-md border border-slate-800 bg-slate-950 px-3 py-2 text-slate-300">
                <span className="block font-medium">Original-file retention</span>
                <select
                  aria-label="Original-file retention"
                  value={retentionChoice}
                  onChange={(event) => setRetentionChoice(event.target.value as RetentionChoice)}
                  disabled={documentMutation.isPending}
                  className="mt-2 w-full rounded-md border border-slate-700 bg-slate-900 px-2 py-1.5 text-xs text-slate-100"
                >
                  <option value="solo_default">Use Solo configured default</option>
                  <option value="retain">Keep originals locally</option>
                  <option value="discard">Extract text, then discard originals</option>
                </select>
                <span className="mt-2 block text-slate-400">
                  The default is read from Solo when each upload is prepared. Unsupported files are
                  useful only when their original bytes are retained.
                </span>
              </label>
            </div>
          )}
        </div>

        <div className="mt-5 border-t border-slate-800 pt-4">
          <label
            className="block text-xs font-medium uppercase text-slate-400"
            htmlFor="native-import-path"
          >
            Local path
          </label>
          <div className="mt-1 text-xs text-slate-400">Mode: {selectedPathSource.label}</div>
          <div className="mt-3 grid gap-2">
            {PATH_SOURCES.map((item) => (
              <button
                key={item.id}
                type="button"
                aria-pressed={nativeSource === item.id}
                onClick={() => {
                  setNativeSource(item.id);
                  setNativeResult(null);
                }}
                className={[
                  'rounded-md border px-3 py-2 text-left text-xs transition-colors',
                  nativeSource === item.id
                    ? 'border-teal-500 bg-teal-950 text-teal-100'
                    : 'border-slate-800 bg-slate-950 text-slate-300 hover:border-slate-600',
                ].join(' ')}
              >
                {item.label}
              </button>
            ))}
          </div>
          <input
            id="native-import-path"
            type="text"
            value={nativePath}
            onChange={(event) => {
              setNativePath(event.target.value);
              setNativeResult(null);
            }}
            placeholder={nativePathPlaceholder}
            className="mt-2 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-100 placeholder:text-slate-400 focus:border-sky-500 focus:outline-none"
          />
          <div className="mt-3 grid grid-cols-2 gap-2">
            <button
              type="button"
              onClick={() =>
                nativeMutation.mutate({
                  dryRun: true,
                  path: nativePath.trim(),
                  source: nativeSource,
                })
              }
              disabled={!nativePathReady}
              className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-50"
            >
              Scan path
            </button>
            <button
              type="button"
              onClick={() =>
                nativeMutation.mutate({
                  dryRun: false,
                  path: nativePath.trim(),
                  source: nativeSource,
                })
              }
              disabled={!nativePathReady}
              className="rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {nativeMutation.isPending ? 'Working' : 'Import path'}
            </button>
          </div>
          {nativeMutation.error && (
            <div className="mt-3 rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-xs text-red-200">
              {nativeMutation.error instanceof Error
                ? nativeMutation.error.message
                : String(nativeMutation.error)}
            </div>
          )}
        </div>

        {fileSource === 'documents' ? (
          <>
            <div className="mt-5 grid grid-cols-2 gap-2">
              <Metric label="Files" value={String(documentItems.length)} />
              <Metric
                label="Ready"
                value={String(documentItems.filter((item) => item.status === 'ready').length)}
              />
              <Metric
                label="Imported"
                value={String(
                  documentItems.filter((item) =>
                    ['extracted', 'stored_unparsed'].includes(item.status),
                  ).length,
                )}
              />
              <Metric
                label="Size"
                value={formatBytes(
                  documentItems.reduce((sum, item) => sum + item.sizeBytes, 0),
                  { precision: 'fixed1' },
                )}
              />
            </div>
            <button
              type="button"
              onClick={() => documentMutation.mutate(documentItems.filter(isDocumentImportable))}
              disabled={
                documentItems.length === 0 ||
                documentMutation.isPending ||
                documentImportableCount === 0
              }
              className="mt-4 w-full rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {documentMutation.isPending
                ? 'Importing files'
                : `Import ${documentImportableCount} file${documentImportableCount === 1 ? '' : 's'}`}
            </button>
          </>
        ) : (
          <>
            <div className="mt-5 grid grid-cols-2 gap-2">
              <Metric label="Files" value={String(preview?.files ?? 0)} />
              <Metric label="Records" value={String(preview?.records.length ?? 0)} />
              <Metric label="Selected" value={String(selectedRecords.length)} />
              <Metric
                label="Size"
                value={formatBytes(preview?.bytes ?? 0, { precision: 'fixed1' })}
              />
            </div>

            <div className="mt-5 flex gap-2">
              <button
                type="button"
                onClick={selectAll}
                disabled={!preview || preview.records.length === 0}
                className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-50"
              >
                Select all
              </button>
              <button
                type="button"
                onClick={clearSelection}
                disabled={selectedRecords.length === 0}
                className="rounded-md border border-slate-700 px-3 py-2 text-sm text-slate-200 hover:bg-slate-800 disabled:cursor-not-allowed disabled:opacity-50"
              >
                Clear
              </button>
            </div>

            <button
              type="button"
              onClick={() => {
                setResults([]);
                importMutation.mutate(selectedRecords);
              }}
              disabled={selectedRecords.length === 0 || importMutation.isPending}
              className="mt-3 w-full rounded-md bg-sky-700 px-3 py-2 text-sm font-medium text-white hover:bg-sky-600 disabled:cursor-not-allowed disabled:bg-slate-700"
            >
              {importMutation.isPending ? 'Importing' : 'Import selected'}
            </button>
          </>
        )}

        {history.length > 0 && (
          <div className="mt-5 border-t border-slate-800 pt-4">
            <h2 className="text-sm font-semibold text-slate-100">History</h2>
            <div className="mt-3 space-y-2">
              {history.map((entry) => (
                <div
                  key={entry.id}
                  className="rounded-md border border-slate-800 bg-slate-950 px-3 py-2"
                >
                  <div className="flex items-center justify-between gap-2 text-sm">
                    <span className="font-medium text-slate-200">{sourceLabel(entry.source)}</span>
                    <span className="text-xs text-slate-400">{formatTime(entry.atMs)}</span>
                  </div>
                  <div className="mt-1 text-xs text-slate-400">{historySummary(entry)}</div>
                </div>
              ))}
            </div>
          </div>
        )}
      </section>

      <section className="min-w-0 rounded-lg border border-slate-800 bg-slate-900/45">
        <div className="flex min-h-14 items-center justify-between gap-3 border-b border-slate-800 px-4">
          <h2 className="text-sm font-semibold text-slate-100">
            {fileSource === 'documents' ? 'Files and extraction' : 'Preview'}
          </h2>
          {fileSource === 'documents' && documentItems.length > 0 && (
            <span className="text-xs text-slate-400">
              {documentItems.filter((item) => item.status === 'extracted').length} searchable,{' '}
              {documentItems.filter((item) => item.status === 'stored_unparsed').length} retained
              only, {documentItems.filter((item) => item.status === 'failed').length} failed
            </span>
          )}
          {fileSource !== 'documents' && results.length > 0 && (
            <span className="text-xs text-slate-400">
              {results.filter((result) => result.memoryId).length} imported,{' '}
              {results.filter((result) => result.error).length} failed
            </span>
          )}
        </div>

        <div
          className="max-h-[calc(100vh-220px)] overflow-y-auto p-4"
          tabIndex={0}
          aria-label="Import preview"
        >
          {parseError && (
            <div className="rounded-md border border-red-900 bg-red-950/50 px-3 py-2 text-sm text-red-200">
              {parseError}
            </div>
          )}

          {fileSource === 'documents' && documentItems.length > 0 && (
            <DocumentImportPanel
              items={documentItems}
              retentionChoice={retentionChoice}
              onForgetDocument={(item) => void handleForgetDocument(item)}
              onDeleteAsset={(item) => void handleDeleteAsset(item)}
              onCancel={handleCancelDocument}
              onResume={(item) => void handleResumeDocument(item)}
            />
          )}

          {fileSource === 'documents' && (
            <DocumentLifecycleCatalog
              visible={showLifecycle}
              loading={lifecycleCatalog.isFetching}
              error={
                lifecycleError ??
                (lifecycleCatalog.error instanceof Error
                  ? lifecycleCatalog.error.message
                  : lifecycleCatalog.error
                    ? String(lifecycleCatalog.error)
                    : null)
              }
              documents={lifecycleCatalog.data?.documents ?? []}
              assets={lifecycleCatalog.data?.assets ?? []}
              documentsTruncated={lifecycleCatalog.data?.documentsTruncated ?? false}
              assetsTruncated={lifecycleCatalog.data?.assetsTruncated ?? false}
              action={lifecycleAction}
              onShow={() => setShowLifecycle(true)}
              onRefresh={() => void lifecycleCatalog.refetch()}
              onForgetDocument={(document) => void handleCatalogForgetDocument(document)}
              onDeleteAsset={(asset) => void handleCatalogDeleteAsset(asset)}
            />
          )}

          {preview?.issues.map((issue) => (
            <div
              key={`${issue.file}:${issue.message}`}
              className="mb-2 rounded-md border border-amber-900 bg-amber-950/40 px-3 py-2 text-sm text-amber-100"
            >
              <span className="font-medium">{issue.file}</span>: {issue.message}
            </div>
          ))}

          {nativeResult && <NativeImportPanel result={nativeResult} />}

          {fileSource === 'documents' && documentItems.length === 0 && !nativeResult && (
            <div className="rounded-md border border-slate-800 bg-slate-950 px-4 py-12 text-center text-sm text-slate-400">
              Choose one or more files. Solo will show each extraction result and whether it kept
              the original bytes.
            </div>
          )}

          {fileSource !== 'documents' && !preview && !parseError && !nativeResult && (
            <div className="rounded-md border border-slate-800 bg-slate-950 px-4 py-12 text-center text-sm text-slate-400">
              No files selected
            </div>
          )}

          {preview && preview.records.length === 0 && (
            <div className="rounded-md border border-slate-800 bg-slate-950 px-4 py-12 text-center text-sm text-slate-400">
              No records parsed
            </div>
          )}

          {preview && preview.records.length > 0 && (
            <div className="space-y-2">
              {preview.records.map((record) => {
                const result = results.find((item) => item.record.id === record.id);
                return (
                  <label
                    key={record.id}
                    className="block rounded-lg border border-slate-800 bg-slate-950 px-3 py-3 hover:border-slate-600"
                  >
                    <div className="flex items-start gap-3">
                      <input
                        type="checkbox"
                        checked={selectedIds.has(record.id)}
                        onChange={() => toggleRecord(record.id)}
                        className="mt-1 h-4 w-4 accent-sky-500"
                      />
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center justify-between gap-3">
                          <span className="truncate text-sm font-medium text-slate-100">
                            {record.title}
                          </span>
                          <span className="shrink-0 text-xs text-slate-400">
                            {formatBytes(record.bytes, { precision: 'fixed1' })}
                          </span>
                        </div>
                        <p className="mt-1 text-sm text-slate-400">{record.preview}</p>
                        <div className="mt-2 flex flex-wrap gap-2 text-xs">
                          <span className="rounded-sm bg-slate-800 px-2 py-1 text-slate-300">
                            {record.sourceType}
                          </span>
                          <span className="rounded-sm bg-slate-800 px-2 py-1 font-mono text-slate-400">
                            {record.sourceId}
                          </span>
                          {result?.memoryId && (
                            <span className="rounded-sm bg-emerald-950 px-2 py-1 text-emerald-200">
                              {result.memoryId}
                            </span>
                          )}
                          {result?.error && (
                            <span className="rounded-sm bg-red-950 px-2 py-1 text-red-200">
                              {result.error}
                            </span>
                          )}
                        </div>
                      </div>
                    </div>
                  </label>
                );
              })}
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

function DocumentImportPanel({
  items,
  retentionChoice,
  onForgetDocument,
  onDeleteAsset,
  onCancel,
  onResume,
}: {
  items: DocumentImportItem[];
  retentionChoice: RetentionChoice;
  onForgetDocument: (item: DocumentImportItem) => void;
  onDeleteAsset: (item: DocumentImportItem) => void;
  onCancel: (item: DocumentImportItem) => void;
  onResume: (item: DocumentImportItem) => void;
}) {
  return (
    <div className="space-y-3">
      <div className="rounded-md border border-sky-900 bg-sky-950/35 px-3 py-2 text-xs leading-5 text-sky-100">
        Document forget and source-file deletion are separate. Forget removes a document from search
        but keeps soft-deleted database rows. Deleting a retained original removes its raw bytes
        while keeping a provenance record. This is not a hard purge.
      </div>
      {items.map((item) => {
        const progress = item.sizeBytes > 0 ? item.bytesSent / item.sizeBytes : 0;
        return (
          <article
            key={item.id}
            className="rounded-lg border border-slate-800 bg-slate-950 px-4 py-3"
          >
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="min-w-0">
                <h3 className="truncate text-sm font-medium text-slate-100">{item.filename}</h3>
                <div className="mt-1 flex flex-wrap gap-2 text-xs text-slate-400">
                  <span>{formatBytes(item.sizeBytes, { precision: 'fixed1' })}</span>
                  <span>
                    {item.recoveryOnly ? 'Recovery receipt' : documentSupportLabel(item.support)}
                  </span>
                </div>
              </div>
              <span className={documentStatusClass(item.status)}>{documentStatusLabel(item)}</span>
            </div>

            {item.status === 'uploading' && (
              <div
                className="mt-3 h-1.5 overflow-hidden rounded-full bg-slate-800"
                role="progressbar"
                aria-label={`Upload ${item.filename}`}
                aria-valuemin={0}
                aria-valuemax={item.sizeBytes}
                aria-valuenow={item.bytesSent}
              >
                <div
                  className="h-full bg-sky-500 transition-[width]"
                  style={{ width: `${Math.min(100, Math.round(progress * 100))}%` }}
                />
              </div>
            )}

            {item.status === 'ready' && item.support === 'metadata_only' && (
              <p className="mt-3 text-xs leading-5 text-amber-200">
                Solo will index metadata or a manifest, not the full visual or 3D content.
              </p>
            )}
            {item.status === 'ready' && item.support === 'unsupported' && (
              <p className="mt-3 text-xs leading-5 text-amber-200">
                No default extractor exists for this extension. The daemon may reject it; if it
                accepts asset-only uploads, it will be retained but not searchable.
                {retentionChoice === 'discard' && ' Original-file retention is explicitly off.'}
              </p>
            )}

            {item.result && (
              <div className="mt-3 space-y-2 text-xs leading-5 text-slate-300">
                <p>{documentExtractionMessage(item)}</p>
                <p>
                  {item.result.stored_original_file
                    ? item.assetDeleted
                      ? 'Original source bytes deleted; the provenance record remains.'
                      : 'Original source file retained locally as a separate asset.'
                    : 'Original source file was not retained after extraction.'}
                </p>
                {!item.result.deleted_staged_file && (
                  <p className="rounded-md border border-amber-800 bg-amber-950/40 px-2 py-1 text-amber-100">
                    Cleanup warning: Solo retained the staged upload. Retry cleanup from diagnostics
                    or wait for the staging TTL before treating the source bytes as deleted.
                  </p>
                )}
                <div className="flex flex-wrap gap-2 font-mono text-slate-400">
                  {item.result.document_id && <span>doc {item.result.document_id}</span>}
                  {item.result.asset?.asset_id && <span>asset {item.result.asset.asset_id}</span>}
                </div>
              </div>
            )}

            {item.error && (
              <p className="mt-3 rounded-md border border-red-900 bg-red-950/40 px-3 py-2 text-xs leading-5 text-red-200">
                {item.error}
              </p>
            )}

            {['preparing', 'uploading', 'resuming'].includes(item.status) && (
              <button
                type="button"
                aria-label={`Cancel upload ${item.filename}`}
                onClick={() => onCancel(item)}
                className="mt-3 rounded-md border border-slate-700 px-3 py-1.5 text-xs text-slate-200 hover:bg-slate-800"
              >
                Cancel upload
              </button>
            )}

            {item.status === 'uncertain' &&
              item.resumeUploadId &&
              item.resumeStoreOriginalFile !== undefined && (
                <button
                  type="button"
                  aria-label={`Recover import ${item.filename}`}
                  onClick={() => onResume(item)}
                  className="mt-3 rounded-md border border-sky-700 px-3 py-1.5 text-xs text-sky-100 hover:bg-sky-950"
                >
                  Recover without re-uploading
                </button>
              )}

            {item.result && (item.result.document_id || item.result.asset) && (
              <div className="mt-3 flex flex-wrap gap-2 border-t border-slate-800 pt-3">
                {item.result.document_id && !item.documentForgotten ? (
                  <button
                    type="button"
                    aria-label={`Forget searchable document ${item.filename}`}
                    onClick={() => onForgetDocument(item)}
                    disabled={Boolean(item.lifecycleAction)}
                    className="rounded-md border border-amber-800 px-3 py-1.5 text-xs text-amber-100 hover:bg-amber-950 disabled:opacity-50"
                  >
                    {item.lifecycleAction === 'forgetting_document'
                      ? 'Forgetting document'
                      : 'Forget searchable document'}
                  </button>
                ) : item.documentForgotten ? (
                  <span className="rounded-sm bg-amber-950 px-2 py-1 text-xs text-amber-200">
                    Searchable document forgotten
                  </span>
                ) : null}
                {item.result.asset && !item.assetDeleted ? (
                  <button
                    type="button"
                    aria-label={`Delete retained original ${item.filename}`}
                    onClick={() => onDeleteAsset(item)}
                    disabled={Boolean(item.lifecycleAction)}
                    className="rounded-md border border-red-900 px-3 py-1.5 text-xs text-red-200 hover:bg-red-950 disabled:opacity-50"
                  >
                    {item.lifecycleAction === 'deleting_asset'
                      ? 'Deleting original'
                      : 'Delete retained original'}
                  </button>
                ) : null}
              </div>
            )}

            {item.lifecycleError && (
              <p className="mt-2 text-xs leading-5 text-red-300">{item.lifecycleError}</p>
            )}
          </article>
        );
      })}
    </div>
  );
}

function DocumentLifecycleCatalog({
  visible,
  loading,
  error,
  documents,
  assets,
  documentsTruncated,
  assetsTruncated,
  action,
  onShow,
  onRefresh,
  onForgetDocument,
  onDeleteAsset,
}: {
  visible: boolean;
  loading: boolean;
  error: string | null;
  documents: DocumentLifecycleSummary[];
  assets: AssetLifecycleSummary[];
  documentsTruncated: boolean;
  assetsTruncated: boolean;
  action: string | null;
  onShow: () => void;
  onRefresh: () => void;
  onForgetDocument: (document: DocumentLifecycleSummary) => void;
  onDeleteAsset: (asset: AssetLifecycleSummary) => void;
}) {
  const renderPageSize = 100;
  const [visibleDocumentCount, setVisibleDocumentCount] = useState(renderPageSize);
  const [visibleAssetCount, setVisibleAssetCount] = useState(renderPageSize);

  useEffect(() => setVisibleDocumentCount(renderPageSize), [documents]);
  useEffect(() => setVisibleAssetCount(renderPageSize), [assets]);

  if (!visible) {
    return (
      <button
        type="button"
        onClick={onShow}
        className="mt-4 w-full rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-sm text-slate-200 hover:border-slate-500"
      >
        Manage saved documents and retained originals
      </button>
    );
  }

  return (
    <section
      className="mt-4 rounded-lg border border-slate-800 bg-slate-950 p-4"
      aria-label="Saved document lifecycle"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold text-slate-100">Saved lifecycle records</h3>
          <p className="mt-1 text-xs text-slate-400">
            Loaded from Solo, so these controls remain available after navigation or restart.
          </p>
        </div>
        <button
          type="button"
          onClick={onRefresh}
          disabled={loading}
          className="rounded-md border border-slate-700 px-3 py-1.5 text-xs text-slate-200 disabled:opacity-50"
        >
          {loading ? 'Refreshing' : 'Refresh'}
        </button>
      </div>

      {error && (
        <p className="mt-3 rounded-md border border-red-900 bg-red-950/40 px-3 py-2 text-xs text-red-200">
          {error}
        </p>
      )}

      {(documentsTruncated || assetsTruncated) && (
        <p className="mt-3 rounded-md border border-amber-800 bg-amber-950/30 px-3 py-2 text-xs text-amber-100">
          This memory library exceeds the browser catalog limit. Showing the newest loaded lifecycle
          records; use Solo administrative tooling for older records.
        </p>
      )}

      {!error && !loading && documents.length === 0 && assets.length === 0 && (
        <p className="mt-3 text-xs text-slate-400">No saved document or asset records.</p>
      )}

      {documents.length > 0 && (
        <div className="mt-4">
          <h4 className="text-xs font-medium uppercase tracking-wide text-slate-400">
            Searchable documents
          </h4>
          <div className="mt-2 space-y-2">
            {documents.slice(0, visibleDocumentCount).map((document) => (
              <div
                key={document.doc_id}
                className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-slate-800 bg-slate-900/60 px-3 py-2 text-xs"
              >
                <div className="min-w-0">
                  <div className="truncate text-slate-200">{document.title ?? document.doc_id}</div>
                  <div className="mt-1 flex flex-wrap gap-2 text-slate-400">
                    <span>{document.chunk_count} chunks</span>
                    <span>{document.status}</span>
                    <span>{formatTime(document.ingested_at_ms)}</span>
                  </div>
                </div>
                {document.status === 'active' && (
                  <button
                    type="button"
                    onClick={() => onForgetDocument(document)}
                    disabled={Boolean(action)}
                    className="rounded-md border border-amber-800 px-2 py-1 text-amber-100 disabled:opacity-50"
                  >
                    {action === `document:${document.doc_id}` ? 'Forgetting' : 'Forget'}
                  </button>
                )}
              </div>
            ))}
          </div>
          {visibleDocumentCount < documents.length && (
            <button
              type="button"
              onClick={() => setVisibleDocumentCount((count) => count + renderPageSize)}
              className="mt-2 rounded-md border border-slate-700 px-3 py-1.5 text-xs text-slate-200"
            >
              Show more documents
            </button>
          )}
        </div>
      )}

      {assets.length > 0 && (
        <div className="mt-4">
          <h4 className="text-xs font-medium uppercase tracking-wide text-slate-400">
            Retained originals
          </h4>
          <div className="mt-2 space-y-2">
            {assets.slice(0, visibleAssetCount).map((asset) => (
              <div
                key={asset.asset_id}
                className="flex flex-wrap items-center justify-between gap-3 rounded-md border border-slate-800 bg-slate-900/60 px-3 py-2 text-xs"
              >
                <div className="min-w-0">
                  <div className="truncate text-slate-200">{asset.filename ?? asset.asset_id}</div>
                  <div className="mt-1 flex flex-wrap gap-2 text-slate-400">
                    <span>{formatBytes(asset.size_bytes, { precision: 'fixed1' })}</span>
                    <span>{asset.status}</span>
                    <span>{asset.mime_type}</span>
                  </div>
                </div>
                {asset.status === 'active' && (
                  <button
                    type="button"
                    onClick={() => onDeleteAsset(asset)}
                    disabled={Boolean(action)}
                    className="rounded-md border border-red-900 px-2 py-1 text-red-200 disabled:opacity-50"
                  >
                    {action === `asset:${asset.asset_id}` ? 'Deleting' : 'Delete bytes'}
                  </button>
                )}
              </div>
            ))}
          </div>
          {visibleAssetCount < assets.length && (
            <button
              type="button"
              onClick={() => setVisibleAssetCount((count) => count + renderPageSize)}
              className="mt-2 rounded-md border border-slate-700 px-3 py-1.5 text-xs text-slate-200"
            >
              Show more retained originals
            </button>
          )}
        </div>
      )}
    </section>
  );
}

function NativeImportPanel({ result }: { result: NativeImportResponse }) {
  const rows = result.dry_run ? result.files : result.results;
  const title = `${importResultSourceLabel(result)} path ${result.dry_run ? 'scan' : 'import'}`;
  return (
    <div className="mb-4 rounded-lg border border-slate-800 bg-slate-950 px-4 py-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h3 className="text-sm font-semibold text-slate-100">{title}</h3>
          <div className="mt-1 max-w-3xl truncate font-mono text-xs text-slate-400">
            {result.path}
          </div>
        </div>
        <div className="flex flex-wrap gap-2 text-xs">
          <span className="rounded-sm bg-slate-800 px-2 py-1 text-slate-300">
            {result.total_files} files
          </span>
          <span className="rounded-sm bg-slate-800 px-2 py-1 text-slate-300">
            {formatBytes(result.total_bytes, { precision: 'fixed1' })}
          </span>
          {!result.dry_run && (
            <>
              <span className="rounded-sm bg-emerald-950 px-2 py-1 text-emerald-200">
                {result.imported} new
              </span>
              {result.deduped > 0 && (
                <span className="rounded-sm bg-slate-800 px-2 py-1 text-slate-300">
                  {result.deduped} deduped
                </span>
              )}
            </>
          )}
          {result.failed > 0 && (
            <span className="rounded-sm bg-red-950 px-2 py-1 text-red-200">
              {result.failed} failed
            </span>
          )}
          {result.truncated && (
            <span className="rounded-sm bg-amber-950 px-2 py-1 text-amber-200">truncated</span>
          )}
        </div>
      </div>

      {rows.length > 0 && (
        <div
          className="mt-3 max-h-72 space-y-2 overflow-y-auto"
          tabIndex={0}
          aria-label="Native import file results"
        >
          {rows.slice(0, 40).map((row) => {
            const resultRow = isNativeImportResult(row) ? row : null;
            return (
              <div
                key={row.path}
                className="grid gap-2 rounded-md border border-slate-800 bg-slate-900/70 px-3 py-2 text-xs md:grid-cols-[1fr_auto]"
              >
                <span className="min-w-0 truncate font-mono text-slate-300">{row.path}</span>
                <span className="flex flex-wrap gap-2 text-slate-400">
                  <span>{formatBytes(row.bytes, { precision: 'fixed1' })}</span>
                  {resultRow?.doc_id && <span>{resultRow.doc_id}</span>}
                  {resultRow?.error && <span className="text-red-300">{resultRow.error}</span>}
                </span>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

function isNativeImportResult(
  row: NativeImportFile | NativeImportResult,
): row is NativeImportResult {
  return 'bytes_ingested' in row;
}

function createDocumentItems(files: File[]): DocumentImportItem[] {
  return files.map((file, index) => ({
    id: `${file.name}:${file.size}:${file.lastModified}:${index}`,
    file,
    filename: file.name,
    sizeBytes: file.size,
    support: documentSupport(file.name),
    status: 'ready',
    bytesSent: 0,
  }));
}

function mergeDocumentItems(
  recoveries: DocumentImportItem[],
  selected: DocumentImportItem[],
  apiUrl: string,
): DocumentImportItem[] {
  const pending = recoveries.filter((item) => item.resumeUploadId);
  const currentScopeHasPendingRecovery = pending.some(
    (item) => (item.resumeApiUrl ?? apiUrl) === apiUrl,
  );
  return [...pending, ...(currentScopeHasPendingRecovery ? [] : selected)];
}

function loadDocumentRecoveryItems(): DocumentImportItem[] {
  return readDocumentRecoveryCheckpoints().map((checkpoint) => ({
    id: `recovery:${checkpoint.uploadId}`,
    filename: RECOVERY_DOCUMENT_LABEL,
    sizeBytes: checkpoint.sizeBytes,
    support: 'unsupported',
    status: 'uncertain',
    bytesSent: 0,
    error:
      'This browser session was interrupted after Solo created a staged upload. Recover it before selecting the source file again.',
    resumeUploadId: checkpoint.uploadId,
    resumeStagedUri: checkpoint.stagedUri ?? undefined,
    resumeStoreOriginalFile: checkpoint.storeOriginalFile,
    resumeApiUrl: checkpoint.apiUrl,
    recoveryOnly: true,
  }));
}

function readDocumentRecoveryCheckpoints(): PersistedDocumentRecovery[] {
  try {
    const raw = sessionStorage.getItem(DOCUMENT_RECOVERY_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) {
      sessionStorage.removeItem(DOCUMENT_RECOVERY_KEY);
      return [];
    }
    const cutoff = Date.now() - DOCUMENT_RECOVERY_MAX_AGE_MS;
    const valid = parsed
      .filter(isPersistedDocumentRecovery)
      .filter((checkpoint) => checkpoint.updatedAtMs >= cutoff)
      .slice(0, DOCUMENT_RECOVERY_LIMIT);
    const checkpoints = valid.map(sanitizeDocumentRecoveryCheckpoint);
    const containsLegacySensitiveFields = valid.some(
      (checkpoint) => 'filename' in checkpoint || 'id' in checkpoint,
    );
    if (checkpoints.length !== parsed.length || containsLegacySensitiveFields) {
      writeDocumentRecoveryCheckpoints(checkpoints);
    }
    return checkpoints;
  } catch {
    return [];
  }
}

function sanitizeDocumentRecoveryCheckpoint(
  checkpoint: PersistedDocumentRecovery,
): PersistedDocumentRecovery {
  return {
    version: 1,
    sizeBytes: checkpoint.sizeBytes,
    uploadId: checkpoint.uploadId,
    stagedUri: checkpoint.stagedUri,
    storeOriginalFile: checkpoint.storeOriginalFile,
    apiUrl: checkpoint.apiUrl,
    updatedAtMs: checkpoint.updatedAtMs,
  };
}

function isPersistedDocumentRecovery(value: unknown): value is PersistedDocumentRecovery {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const candidate = value as Partial<PersistedDocumentRecovery>;
  return (
    candidate.version === 1 &&
    typeof candidate.sizeBytes === 'number' &&
    Number.isFinite(candidate.sizeBytes) &&
    candidate.sizeBytes >= 0 &&
    typeof candidate.uploadId === 'string' &&
    candidate.uploadId.length > 0 &&
    (candidate.stagedUri === null || typeof candidate.stagedUri === 'string') &&
    typeof candidate.storeOriginalFile === 'boolean' &&
    typeof candidate.apiUrl === 'string' &&
    candidate.apiUrl.length > 0 &&
    typeof candidate.updatedAtMs === 'number' &&
    Number.isFinite(candidate.updatedAtMs)
  );
}

function saveDocumentRecoveryCheckpoint(checkpoint: PersistedDocumentRecovery): void {
  const checkpoints = readDocumentRecoveryCheckpoints().filter(
    (current) => current.uploadId !== checkpoint.uploadId,
  );
  writeDocumentRecoveryCheckpoints([checkpoint, ...checkpoints].slice(0, DOCUMENT_RECOVERY_LIMIT));
}

function updateDocumentRecoveryCheckpoint(
  uploadId: string,
  update: {
    stagedUri?: string | null;
    storeOriginalFile?: boolean;
    updatedAtMs: number;
  },
): void {
  const checkpoints = readDocumentRecoveryCheckpoints();
  const index = checkpoints.findIndex((checkpoint) => checkpoint.uploadId === uploadId);
  if (index < 0) return;
  const current = checkpoints[index];
  checkpoints[index] = {
    ...current,
    ...(update.stagedUri !== undefined ? { stagedUri: update.stagedUri } : {}),
    ...(update.storeOriginalFile !== undefined
      ? { storeOriginalFile: update.storeOriginalFile }
      : {}),
    updatedAtMs: update.updatedAtMs,
  };
  writeDocumentRecoveryCheckpoints(checkpoints);
}

function removeDocumentRecoveryCheckpoint(uploadId: string): void {
  const remaining = readDocumentRecoveryCheckpoints().filter(
    (checkpoint) => checkpoint.uploadId !== uploadId,
  );
  writeDocumentRecoveryCheckpoints(remaining);
}

function writeDocumentRecoveryCheckpoints(checkpoints: PersistedDocumentRecovery[]): void {
  try {
    if (checkpoints.length === 0) {
      sessionStorage.removeItem(DOCUMENT_RECOVERY_KEY);
      return;
    }
    sessionStorage.setItem(DOCUMENT_RECOVERY_KEY, JSON.stringify(checkpoints));
  } catch {
    // The live workflow remains usable when session storage is unavailable.
  }
}

function privacySafeApiUrl(apiUrl: string): string {
  try {
    const url = new URL(apiUrl);
    url.username = '';
    url.password = '';
    url.hash = '';
    return url.toString().replace(/\/$/, '');
  } catch {
    return 'invalid-endpoint';
  }
}

function documentStatusLabel(item: DocumentImportItem): string {
  switch (item.status) {
    case 'ready':
      return 'Ready';
    case 'preparing':
      return 'Preparing';
    case 'uploading':
      return `Uploading ${Math.round((item.bytesSent / Math.max(1, item.sizeBytes)) * 100)}%`;
    case 'resuming':
      return `Resuming ${Math.round((item.bytesSent / Math.max(1, item.sizeBytes)) * 100)}%`;
    case 'committing':
      return 'Verifying upload';
    case 'extracting':
      return 'Extracting';
    case 'complete':
      return 'Finishing';
    case 'extracted':
      return item.result?.deduped ? 'Already indexed' : 'Searchable';
    case 'stored_unparsed':
      return 'Retained only';
    case 'cancelled':
      return 'Cancelled';
    case 'uncertain':
      return 'Needs attention';
    case 'failed':
      return 'Failed';
  }
}

function documentStatusClass(status: DocumentImportStatus): string {
  const base = 'shrink-0 rounded-sm px-2 py-1 text-xs';
  if (status === 'extracted') return `${base} bg-emerald-950 text-emerald-200`;
  if (status === 'stored_unparsed') return `${base} bg-amber-950 text-amber-200`;
  if (status === 'failed' || status === 'uncertain') return `${base} bg-red-950 text-red-200`;
  if (status === 'cancelled') return `${base} bg-slate-800 text-slate-300`;
  if (status === 'ready') return `${base} bg-slate-800 text-slate-300`;
  return `${base} bg-sky-950 text-sky-200`;
}

function documentExtractionMessage(item: DocumentImportItem): string {
  const result = item.result;
  if (!result) return '';
  if (result.extraction_status === 'stored_unparsed') {
    return `Retained only - Solo did not create searchable chunks${
      result.extraction_error ? `: ${result.extraction_error}` : '.'
    }`;
  }
  if (result.extraction_status === 'failed') {
    return `Extraction failed${result.extraction_error ? `: ${result.extraction_error}` : '.'}`;
  }
  const content = item.support === 'metadata_only' ? 'metadata or manifest' : 'document content';
  return result.deduped
    ? `Already indexed - Solo matched existing ${content}.`
    : `Searchable - Solo indexed ${result.chunks_persisted} chunk${
        result.chunks_persisted === 1 ? '' : 's'
      } of ${content}.`;
}

function Metric({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-md border border-slate-800 bg-slate-950 px-3 py-2">
      <div className="text-xs uppercase text-slate-400">{label}</div>
      <div className="mt-1 truncate text-sm font-semibold text-slate-100">{value}</div>
    </div>
  );
}

function sourceLabel(
  source: BrowserImportSource | NativePathSource | 'browser_documents' | 'local',
): string {
  if (source === 'local') return 'Local path';
  if (source === 'browser_documents') return 'Document files';
  return (
    PATH_SOURCES.find((item) => item.id === source)?.label ??
    FILE_SOURCES.find((item) => item.id === source)?.label ??
    source
  );
}

function historySummary(entry: ImportHistoryEntry): string {
  const deduped = entry.deduped ?? 0;
  const failed = entry.failed > 0 ? `, ${entry.failed} failed` : '';
  if (deduped > 0) {
    return `${entry.imported} new, ${deduped} deduped / ${entry.records}${failed}`;
  }
  return `${entry.imported}/${entry.records} imported${failed}`;
}

function daemonPathImportSource(source: NativePathSource): string {
  return source === 'documents' ? 'native' : source;
}

function importResultSourceLabel(result: NativeImportResponse): string {
  switch (result.source) {
    case 'native':
      return 'Local files / Codex project';
  }
  const label = result.source_label?.trim();
  if (label) return label;
  switch (result.source) {
    case 'chatgpt':
      return 'ChatGPT';
    case 'claude':
      return 'Claude';
    case 'bookmarks':
      return 'Bookmarks';
    case 'markdown':
      return 'Markdown';
    case 'markdown_text':
      return 'Markdown/Text';
    case 'text':
      return 'Text';
    case 'json':
      return 'JSON';
    default:
      return 'Local';
  }
}

function formatTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
  });
}

function loadImportHistory(): ImportHistoryEntry[] {
  try {
    const raw = localStorage.getItem(HISTORY_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as ImportHistoryEntry[];
    return Array.isArray(parsed) ? parsed.slice(0, HISTORY_LIMIT) : [];
  } catch {
    return [];
  }
}

function saveImportHistory(entries: ImportHistoryEntry[]): ImportHistoryEntry[] {
  try {
    localStorage.setItem(HISTORY_KEY, JSON.stringify(entries));
  } catch {
    // Non-critical. The visible in-memory history still updates.
  }
  return entries;
}

function defaultNativePathPlaceholder(): string {
  const platform = navigator.platform.toLowerCase();
  if (platform.includes('win')) return 'C:\\Users\\you\\Documents';
  if (platform.includes('mac')) return '/Users/you/Documents or ~/Documents';
  return '/home/you/Documents or ~/Documents';
}
