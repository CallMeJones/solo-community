// TypeScript types matching the eventual Solo /v1/graph/* response shapes.
// These types mirror Solo Community's public graph HTTP contract.

export type NodeKind = 'episode' | 'document' | 'chunk' | 'cluster' | 'entity';

export interface GraphNode {
  /** Stable id (memory_id for episodes, document_id for documents, etc.). */
  id: string;
  kind: NodeKind;
  /** Short display label (first ~80 chars of text, or document title). */
  label: string;
  /** Creation timestamp (episodes/documents/chunks). */
  ts_ms?: number;
  /** For entity nodes: how many triples reference this entity. */
  ref_count?: number;
  /** Longer preview text (for inspector panel). */
  preview?: string;
  /** Original episode source, such as user_message or codex. */
  source_type?: string;
  /** Episode salience in the range 0..1. */
  salience?: number;
  /** Episode lifecycle state. */
  status?: string;
}

export type EdgeKind = 'triple' | 'document_chunk' | 'cluster_member' | 'semantic';

export interface GraphEdge {
  /** Stable edge identity. */
  id: string;
  /** Source node id. */
  source: string;
  /** Target node id. */
  target: string;
  kind: EdgeKind;
  /** For triple edges. */
  predicate?: string;
  /** For semantic edges (HNSW similarity score 0..1). */
  weight?: number;
  /** First-class relationship evidence and temporal metadata when available. */
  meta?: {
    relationship_edge_id?: string;
    subject_entity_id?: string;
    object_entity_id?: string;
    object_kind?: string;
    confidence?: number;
    strength?: number;
    evidence_count?: number;
    valid_from_ms?: number;
    valid_to_ms?: number | null;
    status?: string;
    evidence_memory_id?: string | null;
    [key: string]: unknown;
  };
}

export interface GraphLiteralFact {
  subject_id: string;
  predicate: string;
  object_value: string;
  confidence: number;
}

export interface GraphResponse {
  nodes: GraphNode[];
  edges: GraphEdge[];
  /** Opaque pagination cursor. */
  next_cursor?: string;
}

export interface InspectResponse {
  node: GraphNode;
  full_text?: string;
  /** Edges where this node is the target. */
  triples_in: GraphEdge[];
  /** Edges where this node is the source. */
  triples_out: GraphEdge[];
  /** Literal evidence facts for this node or source episode. */
  literal_facts?: GraphLiteralFact[];
}

export interface MemoryUpdateResult {
  memory_id: string;
  rowid: number;
  content: string;
  updated_at_ms: number;
}

export interface RememberResponse {
  memory_id: string;
}

export interface IngestReport {
  doc_id: string;
  chunks_persisted: number;
  bytes_ingested: number;
  deduped: boolean;
}

export interface DocumentUploadPrepareResponse {
  upload_id: string;
  upload_url: string;
  upload_path: string;
  route_kind: 'direct_local';
  upload_method: 'PATCH';
  upload_offset_header: string;
  upload_length_header: string;
  required_headers: Record<string, string>;
  max_file_bytes: number;
  max_chunk_bytes: number;
  recommended_chunk_bytes: number;
  expires_at_ms: number;
  default_store_original_file: boolean;
}

export interface DocumentUploadCommitResponse {
  upload_id: string;
  staged_uri: string;
  filename: string;
  mime_type: string;
  size_bytes: number;
  sha256: string;
}

export interface DocumentUploadStatusResponse {
  upload_id: string;
  status: 'open' | 'busy' | 'committed' | 'ingested' | 'expired' | 'aborted';
  bytes_received: number;
  size_bytes: number;
  next_offset: number;
  expires_at_ms: number;
  operation_in_progress: boolean;
  active_operation: 'append' | 'commit' | 'ingest' | 'abort' | 'sweep' | 'unknown' | null;
  staged_uri: string | null;
  commit_result: DocumentUploadCommitResponse | null;
  ingest_result: StagedDocumentIngestResponse | null;
  terminal: boolean;
}

export interface StoredAssetReport {
  asset_id: string;
  sha256: string;
  mime_type: string;
  filename?: string | null;
  size_bytes: number;
  storage_path: string;
  deduped: boolean;
}

export interface StagedDocumentIngestResponse {
  staged_uri: string;
  document_id: string | null;
  chunks_persisted: number;
  bytes_ingested: number;
  deduped: boolean;
  stored_original_file: boolean;
  asset: StoredAssetReport | null;
  document_asset_link: { link_id: string; doc_id: string; asset_id: string } | null;
  extraction_status: 'extracted' | 'stored_unparsed' | 'failed';
  extraction_error: string | null;
  deleted_staged_file: boolean;
  retained_source_file: boolean;
  idempotent_replay: boolean;
  ingest_completed_at_ms: number;
}

export interface DocumentUploadAbortResponse {
  upload_id: string;
  status: 'aborted';
  cleanup_performed: boolean;
  already_aborted: boolean;
  removed_partial_file: boolean;
  removed_staged_file: boolean;
}

export interface ForgetDocumentReport {
  doc_id: string;
  chunks_tombstoned: number;
}

export interface ForgetAssetReport {
  asset_id: string;
  blob_deleted: boolean;
  already_deleted: boolean;
  document_links: number;
  memory_attachments: number;
}

export interface DocumentLifecycleSummary {
  doc_id: string;
  title?: string | null;
  source?: string | null;
  mime_type?: string | null;
  ingested_at_ms: number;
  chunk_count: number;
  status: 'active' | 'forgotten';
  extraction_status?: string | null;
  extraction_error?: string | null;
}

export interface AssetLifecycleSummary {
  asset_id: string;
  sha256: string;
  mime_type: string;
  filename?: string | null;
  size_bytes: number;
  storage_path: string;
  source?: string | null;
  status: 'active' | 'deleted';
  created_by_principal?: string | null;
  created_at_ms: number;
  updated_at_ms: number;
}

export interface NativeImportFile {
  path: string;
  bytes: number;
}

export interface NativeImportResult extends NativeImportFile {
  doc_id?: string;
  chunks_persisted: number;
  bytes_ingested: number;
  deduped: boolean;
  error?: string;
}

export interface NativeImportResponse {
  path: string;
  source?: string;
  source_label?: string;
  dry_run: boolean;
  recursive: boolean;
  truncated: boolean;
  total_files: number;
  total_bytes: number;
  imported: number;
  deduped: number;
  failed: number;
  chunks_persisted: number;
  files: NativeImportFile[];
  results: NativeImportResult[];
}

export type MemoryReviewState = 'approved' | 'dismissed';

export type MemoryReviewRequestState = MemoryReviewState | 'needs_review' | 'reset' | null;
export type MemoryReviewResponseState = MemoryReviewState | 'needs_review' | null;

export interface MemoryInboxItem {
  memory_id: string;
  label: string;
  preview: string;
  ts_ms: number;
  source_type: string;
  salience: number;
  status: string;
  review_state?: MemoryReviewState | null;
  reviewed_at_ms?: number | null;
  review_note?: string | null;
}

export interface MemoryInboxResponse {
  items: MemoryInboxItem[];
}

export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

export interface LogTailLine {
  level: LogLevel;
  text: string;
}

export interface LogsResponse {
  source: string;
  path: string;
  exists: boolean;
  limit: number;
  size_bytes: number | null;
  modified_at_ms: number | null;
  lines: LogTailLine[];
}

export interface MemoryReviewReport {
  memory_id: string;
  state: MemoryReviewResponseState;
  reviewed_at_ms: number | null;
}

export interface EntityHit {
  entity_id: string;
  subject_count: number;
  object_count: number;
  fact_count: number;
  predicates: string[];
  match_score: number;
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

export interface MemoryQualityAuditConfig {
  low_confidence_below: number;
  low_coherence_below: number;
  long_literal_chars: number;
  sample_limit: number;
}

export interface MemoryQualityTotals {
  active_episodes: number;
  clustered_episodes: number;
  clusters: number;
  abstractions: number;
  active_triples: number;
  entity_triples: number;
  literal_triples: number;
  triple_reviews_needs_review: number;
  distinct_entities: number;
  contradictions: number;
}

export interface MemoryQualityHealth {
  score: number;
  grade: 'excellent' | 'good' | 'needs_review' | 'weak' | string;
  critical_issues: number;
  warning_issues: number;
  info_issues: number;
}

export interface MemoryQualityIssue {
  severity: 'critical' | 'warning' | 'info' | string;
  code: string;
  count: number;
  summary: string;
  samples: string[];
}

export interface MemoryQualityAliasGroup {
  canonical_key: string;
  labels: string[];
}

export interface MemoryQualityAuditReport {
  generated_at_ms: number;
  config: MemoryQualityAuditConfig;
  totals: MemoryQualityTotals;
  health: MemoryQualityHealth;
  issues: MemoryQualityIssue[];
  alias_groups: MemoryQualityAliasGroup[];
}

export interface MemoryQualityReviewItem {
  review_id: string;
  triple_id?: string | null;
  cluster_id?: string | null;
  source_episode_id?: number | null;
  subject_id: string;
  predicate: string;
  object_id: string;
  object_kind: 'entity' | 'literal' | string;
  confidence: number;
  reason_code: string;
  reason: string;
  status: 'needs_review' | 'approved' | 'dismissed' | 'rewritten' | string;
  created_at_ms: number;
}

export interface MemoryQualityReviewsResponse {
  items: MemoryQualityReviewItem[];
}

export interface MemoryQualityReviewUpdateRequest {
  status: 'needs_review' | 'approved' | 'dismissed' | 'rewritten';
  note?: string | null;
}

export interface ConsolidationReport {
  episodes_seen: number;
  clusters_built: number;
  episodes_clustered: number;
  clusters_merged: number;
  clusters_absorbed: number;
  existing_clusters_merged: number;
  abstractions_regenerated: number;
  abstractions_built: number;
  triples_built: number;
  contradictions_found: number;
}

export interface TriplesExtractReport {
  ran: boolean;
  limit: number;
  cluster_timeout_secs: number;
  abstractions_built: number;
  triples_extracted: number;
  triples_quarantined: number;
  clusters_failed: number;
  clusters_deferred: number;
  note: string;
}

export type DerivedRepairMode = 'stale_abstractions' | 'rebuild_all';

export interface DerivedRepairRequest {
  mode?: DerivedRepairMode;
  dry_run?: boolean;
  min_empty_abstraction_episode_count?: number;
  max_clusters?: number;
}

export interface DerivedRepairCandidate {
  cluster_id: string;
  episode_count: number;
  abstraction_count: number;
  triple_count: number;
  abstraction_preview: string;
  reasons: string[];
}

export interface DerivedRepairReport {
  mode: DerivedRepairMode;
  dry_run: boolean;
  clusters_scanned: number;
  clusters_repaired: number;
  abstractions_deleted: number;
  triples_deleted: number;
  contradictions_deleted: number;
  clusters_deleted: number;
  cluster_memberships_deleted: number;
  candidate_samples: DerivedRepairCandidate[];
}

export interface RecallHit {
  rowid: number;
  memory_id: string;
  cos_distance: number;
  bm25_score?: number | null;
  fused_score: number;
  vector_rank?: number | null;
  lexical_rank?: number | null;
  content: string;
  source_type: string;
  tier: string;
}

export interface ContradictionHit {
  a_id: string;
  b_id: string;
  kind: string;
  explanation: string;
  detected_at_ms: number;
  status: string;
  resolved_at_ms?: number | null;
  resolution_note?: string | null;
  winning_triple_id?: string | null;
}

export type ProjectPolicyClient = 'generic' | 'codex' | 'claude' | 'cursor';

export interface ProjectDescriptor {
  name: string;
  id: string;
  root: string;
  tags: string[];
}

export interface ProjectFactsResponse {
  command: string;
  project: ProjectDescriptor;
  subject: string;
  facts: FactHit[];
}

export interface ProjectDecisionAddResponse {
  command: string;
  action: string;
  project: ProjectDescriptor;
  memory_id: string;
  source_type: string;
  source_id: string;
  content: string;
}

export interface ProjectDecisionSearchResponse {
  command: string;
  action: string;
  project: ProjectDescriptor;
  query: string;
  limit: number;
  hits: RecallHit[];
}

export interface ProjectPolicyResponse {
  command: string;
  client: ProjectPolicyClient;
  project: ProjectDescriptor;
  policy: string;
}

export interface ContradictionResolution {
  a_id: string;
  b_id: string;
  kind: string;
  status: string;
  resolved_at_ms?: number | null;
  resolution_note?: string | null;
  winning_triple_id?: string | null;
}
