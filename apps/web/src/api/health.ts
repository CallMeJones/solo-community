import { jsonFetch, type RequestOptions } from './client';

export interface ServiceHealth {
  ok: true;
  checked_at_ms: number;
}

export interface SoloStatus {
  ok: boolean;
  version: string;
  build: {
    version: string;
    version_with_build: string;
    git_sha?: string;
    git_sha_short?: string;
    git_dirty?: 'clean' | 'dirty' | 'unknown';
    build_number?: string;
    build_attempt?: string;
    build_ref?: string;
    build_timestamp?: string;
  };
  library: {
    name: string;
    ready: boolean;
  };
  embedder: {
    name: string;
    version: string;
    dim: number;
    dtype: string;
    runtime?: {
      running: boolean;
      status: string;
      detail: string;
      checked_at_ms: number;
    };
  };
  mcp: {
    sessions: number;
  };
  steward?: {
    configured: boolean;
    config_mode: string;
    provider: string | null;
    model: string | null;
    runtime_llm: string | null;
    runtime_wired: boolean;
    runtime_has_llm: boolean;
    running?: boolean;
    status?: string;
    automatic: boolean;
    can_write_triples: boolean;
    trigger_interval_secs: number;
    trigger_episode_count: number;
    consolidate_interval_secs: number;
    cluster_timeout_secs: number;
    cluster_min_size: number;
    cluster_cosine_threshold: number;
    next_triples_run_at_ms: number | null;
    last_triples_run_at_ms: number | null;
    last_triples_trigger: string | null;
    last_triples_error: string | null;
    last_triples_timed_out: boolean;
    pending_clusters: number;
    last_triples_batch: {
      ran: boolean;
      limit: number;
      cluster_timeout_secs: number;
      abstractions_built: number;
      triples_extracted: number;
      triples_quarantined: number;
      clusters_failed: number;
      clusters_deferred: number;
      note: string;
    } | null;
    note: string;
  };
  runtime?: {
    pid?: number;
    platform?: string;
    data_dir?: string;
  };
}

export async function fetchSoloHealth(
  signalOrOptions?: AbortSignal | RequestOptions,
): Promise<ServiceHealth> {
  const opts =
    signalOrOptions instanceof AbortSignal ? { signal: signalOrOptions } : signalOrOptions;
  await fetchSoloStatus(opts);
  return { ok: true, checked_at_ms: Date.now() };
}

export async function fetchSoloStatus(opts: RequestOptions = {}): Promise<SoloStatus> {
  return jsonFetch<SoloStatus>('/v1/status', opts);
}
