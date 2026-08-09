export interface SoloClientOptions {
  baseUrl?: string;
  bearerToken?: string;
  fetchImpl?: typeof fetch;
}

export interface RememberOptions {
  sourceType?: string;
  sourceId?: string;
  salience?: number;
}

export interface RememberResponse {
  memory_id: string;
}

export type MemoryReviewState = "approved" | "dismissed" | "needs_review" | "reset" | null;

export interface MemoryInboxOptions {
  limit?: number;
}

export interface MemoryInboxItem {
  memory_id: string;
  label: string;
  preview: string;
  ts_ms: number;
  source_type: string;
  salience: number;
  status: string;
  review_state?: "approved" | "dismissed" | null;
  reviewed_at_ms?: number | null;
  review_note?: string | null;
}

export interface MemoryInboxResponse {
  items: MemoryInboxItem[];
}

export interface MemoryReviewOptions {
  state?: MemoryReviewState;
  note?: string | null;
}

export interface MemoryReviewResponse {
  memory_id: string;
  state?: "approved" | "dismissed" | null;
  reviewed_at_ms?: number | null;
}

export interface RecallOptions {
  limit?: number;
}

export interface RecallHit {
  rowid: number;
  memory_id: string;
  cos_distance: number;
  content: string;
  source_type: string;
  tier: string;
}

export interface RecallResponse {
  hits: RecallHit[];
  index_len: number;
  candidates_considered: number;
}

export interface ContextOptions {
  subject?: string;
  windowDays?: number;
  limit?: number;
}

export interface FactsAboutOptions {
  predicate?: string;
  sinceMs?: number;
  untilMs?: number;
  includeAsObject?: boolean;
  limit?: number;
}

export interface EntitiesOptions {
  limit?: number;
}

export interface ContextSectionHealth {
  status: string;
  count: number;
  warning?: string | null;
}

export interface ContextSections {
  recall: ContextSectionHealth;
  themes: ContextSectionHealth;
  entities: ContextSectionHealth;
  facts: ContextSectionHealth;
  contradictions: ContextSectionHealth;
  graph?: ContextSectionHealth;
}

export interface ContextGraphAlias {
  alias: string;
  canonical_id: string;
  display_label: string;
  confidence: number;
}

export interface ContextGraphFact {
  edge_id: string;
  subject_id: string;
  predicate: string;
  object_id: string;
  object_kind: string;
  confidence: number;
  strength: number;
  evidence_count: number;
  valid_from_ms: number;
  valid_to_ms?: number | null;
  cluster_id?: string | null;
  source_episode_id?: number | null;
  memory_id?: string | null;
  evidence_preview?: string | null;
}

export interface ContextGraphReviewWarning {
  review_id: string;
  reason_code: string;
  reason: string;
  subject_id: string;
  predicate: string;
  object_id: string;
  object_kind: string;
  confidence: number;
}

export interface ContextGraph {
  seed_entities: string[];
  aliases: ContextGraphAlias[];
  relationship_facts: ContextGraphFact[];
  literal_facts: ContextGraphFact[];
  review_warnings: ContextGraphReviewWarning[];
}

export interface ThemeHit {
  cluster_id: string;
  abstraction_id?: string | null;
  abstraction_text?: string | null;
  episode_count: number;
  coherence: number;
  created_at_ms: number;
}

export interface FactHit {
  triple_id: string;
  subject_id: string;
  predicate: string;
  object_id: string;
  object_kind: string;
  valid_from_ms: number;
  valid_to_ms?: number | null;
  confidence: number;
  cluster_id?: string | null;
}

export interface EntityHit {
  entity_id: string;
  subject_count: number;
  object_count: number;
  fact_count: number;
  predicates: string[];
  match_score: number;
}

export interface ContextResponse {
  query: string;
  subject?: string | null;
  resolved_subject?: string | null;
  sections: ContextSections;
  recall: RecallResponse;
  themes: ThemeHit[];
  entities: EntityHit[];
  facts: FactHit[];
  contradictions: unknown[];
  graph?: ContextGraph;
}

export interface StatusResponse {
  ok: boolean;
  version: string;
  build: Record<string, unknown>;
  library: {
    name: string;
    ready: boolean;
  };
  embedder: {
    name: string;
    version: string;
    dim: number;
    dtype: string;
  };
  mcp: {
    sessions: number;
  };
  steward: Record<string, unknown>;
  runtime: {
    pid: number;
    platform: string;
    data_dir: string;
  };
}

export interface ForgetOptions {
  reason?: string;
}

export interface DocumentListOptions {
  limit?: number;
  offset?: number;
  includeForgotten?: boolean;
}

export interface DocumentSearchOptions {
  limit?: number;
}

export interface RecentMemoriesOptions {
  limit?: number;
  cursor?: string;
  sinceMs?: number;
  untilMs?: number;
}

export interface MemoryRecord {
  [key: string]: unknown;
}

export interface MemoryUpdateResponse {
  [key: string]: unknown;
}

export interface DocumentSummary {
  doc_id: string;
  title?: string | null;
  source?: string | null;
  mime_type?: string | null;
  ingested_at_ms: number;
  chunk_count: number;
  status: string;
}

export interface DocumentRecord extends DocumentSummary {
  modified_at_ms?: number | null;
  content_hash?: string | null;
  byte_size?: number | null;
}

export interface DocumentChunkSummary {
  chunk_id: string;
  chunk_index: number;
  content_preview: string;
  token_count: number;
}

export interface DocumentInspectResponse {
  document: DocumentRecord;
  chunks: DocumentChunkSummary[];
}

export interface DocumentIngestResponse {
  doc_id: string;
  chunks_persisted: number;
  bytes_ingested: number;
  deduped: boolean;
}

export interface DocumentForgetResponse {
  doc_id: string;
  chunks_tombstoned: number;
}

export interface DocumentSearchHit {
  chunk_id: string;
  doc_id: string;
  doc_title?: string | null;
  doc_source?: string | null;
  doc_mime_type?: string | null;
  chunk_index: number;
  content: string;
  cos_distance: number;
  start_offset: number;
  end_offset: number;
}

export interface GraphNode {
  id: string;
  kind: string;
  label: string;
  ts_ms?: number | null;
  preview?: string | null;
  source_type?: string | null;
  salience?: number | null;
  status?: string | null;
}

export interface RecentMemoriesResponse {
  nodes: GraphNode[];
  next_cursor: string | null;
}

export interface JsonRpcResponse<T = unknown> {
  jsonrpc: "2.0";
  id?: string | number | null;
  result?: T;
  error?: unknown;
}

export interface McpSession {
  sessionId: string;
  result: unknown;
  raw: JsonRpcResponse;
}

export interface McpToolCallResult {
  content?: Array<Record<string, unknown>>;
  isError?: boolean;
  [key: string]: unknown;
}

export class SoloHttpError extends Error {
  status: number;
  body: unknown;
  constructor(message: string, status: number, body: unknown);
}

export class SoloMcpError extends Error {
  error: unknown;
  constructor(message: string, error: unknown);
}

export class SoloClient {
  baseUrl: string;
  bearerToken?: string;
  constructor(options?: SoloClientOptions);
  status(): Promise<StatusResponse>;
  remember(content: string, options?: RememberOptions): Promise<RememberResponse>;
  rememberInbox(content: string, options?: RememberOptions): Promise<RememberResponse>;
  memoryInbox(options?: MemoryInboxOptions): Promise<MemoryInboxResponse>;
  reviewMemory(memoryId: string, options?: MemoryReviewOptions): Promise<MemoryReviewResponse>;
  recall(query: string, options?: RecallOptions): Promise<RecallResponse>;
  context(query: string, options?: ContextOptions): Promise<ContextResponse>;
  factsAbout(subject: string, options?: FactsAboutOptions): Promise<FactHit[]>;
  entities(query: string, options?: EntitiesOptions): Promise<EntityHit[]>;
  inspect(memoryId: string): Promise<MemoryRecord>;
  update(memoryId: string, content: string): Promise<MemoryUpdateResponse>;
  forget(memoryId: string, options?: ForgetOptions): Promise<unknown>;
  listDocuments(options?: DocumentListOptions): Promise<DocumentSummary[]>;
  ingestDocument(path: string): Promise<DocumentIngestResponse>;
  searchDocuments(query: string, options?: DocumentSearchOptions): Promise<DocumentSearchHit[]>;
  inspectDocument(docId: string): Promise<DocumentInspectResponse>;
  forgetDocument(docId: string): Promise<DocumentForgetResponse>;
  recentMemories(options?: RecentMemoriesOptions): Promise<RecentMemoriesResponse>;
  mcpConnect(clientInfo?: { name: string; version: string }): Promise<McpSession>;
  mcpInitialize(clientInfo?: { name: string; version: string }): Promise<{
    sessionId: string | null;
    result: unknown;
    raw: JsonRpcResponse;
  }>;
  mcpNotifyInitialized(session: string | { sessionId: string }): Promise<void>;
  mcpListTools(session: string | { sessionId: string }): Promise<unknown[]>;
  mcpCallTool(
    session: string | { sessionId: string },
    name: string,
    args?: Record<string, unknown>,
  ): Promise<McpToolCallResult>;
}
