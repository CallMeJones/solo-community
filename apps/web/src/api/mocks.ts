// Deterministic mock graph data for v1 scaffold. Replace with real `/v1/graph/*`
// calls (see api/client.ts) once the Solo P1 routes ship.

import type { GraphEdge, GraphNode, GraphResponse, InspectResponse } from './types';

// --- Deterministic seeded RNG (mulberry32) so reload shows the same graph. ---
function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function pick<T>(arr: T[], rng: () => number): T {
  return arr[Math.floor(rng() * arr.length)];
}

const EPISODE_FRAGMENTS = [
  'Met Alice for coffee at the new place downtown',
  'Decided to use Rust for the storage layer',
  'Reviewed PR #142 with Bob — merged after fixing the race',
  'Anthropic released a new Claude model',
  'Read paper on retrieval-augmented generation',
  'Shipped solo v0.7.1 with HNSW drift fix',
  'Lunch with Carol; discussed her startup',
  'Wrote ADR-0004 on multi-tenancy',
  'Debugged the writer-actor deadlock for three hours',
  'Watched Bryan Cantrill talk on dtrace',
  'Replaced jaeger with tempo for tracing',
  'Bought a mechanical keyboard with brown switches',
  'Configured tmux for split-pane editing',
  'Reread Designing Data-Intensive Applications chapter 5',
  'Set up GitHub Actions matrix for cross-platform builds',
  'Talked to Dave about the audit-pass-3 checklist',
  'Compiled SQLCipher from source again — Strawberry Perl required',
  'Walked the dog before standup',
  'Drafted release notes for solo v0.8.0',
  'Found a bug in the cluster_episodes join',
  'Pair-programmed with Erin on the auth middleware',
  'Took the train into the office for the first time this quarter',
  'Listened to a podcast on distributed consensus',
  'Wrote a long Slack message about the migration plan',
  'Watered the plants on the balcony',
  'Tested OIDC PKCE flow end-to-end',
  'Discovered a race in the recall pipeline',
  'Renamed `solo-storage` crate to clarify its scope',
  'Read Stripe engineering blog on idempotency keys',
  'Configured Prettier and ESLint in solo-web',
  'Cleaned up dead code in the steward module',
  'Met Frank — new hire on the platform team',
  'Reviewed the Q2 OKRs draft',
  'Triaged 8 incoming issues on the tracker',
  'Found stale TODO from 2024 and removed it',
  'Set up Grafana dashboard for ingest p50/p99',
  'Lunch + a podcast on Erlang/OTP',
  'Investigated the HNSW drift detector false positive',
  'Wrote runbook for restore-from-backup',
  'Added smoke test for the publish workflow',
  'Met with George re: pricing tiers',
  'Skipped the weekly all-hands; caught up async',
  'Helped Helen onboard to the codebase',
  'Tested an Anthropic model swap from 4.6 → 4.7',
  'Wrote a long email reply to Ingrid',
  'Refactored the audit-event sink for testability',
  'Closed 11 PRs in audit pass 4',
  'Drafted the solo-web scoping doc',
  'Restarted the daemon to pick up the new schema',
  'Filed a bug against react-force-graph-3d',
];

const ENTITY_NAMES = [
  'alice',
  'bob',
  'carol',
  'dave',
  'erin',
  'frank',
  'george',
  'helen',
  'ingrid',
  'anthropic',
  'solo',
  'rust',
  'sqlcipher',
  'hnsw',
  'office',
  'home',
  'mcpsas',
  'tracker',
  'github',
  'claude',
];

const PREDICATES = [
  'discussed_with',
  'works_on',
  'lives_in',
  'depends_on',
  'mentioned_in',
  'authored',
  'reviewed',
  'reported_by',
];

const CLUSTER_THEMES = [
  'engineering: solo storage layer',
  'people: weekly catch-ups',
  'reading: distributed systems',
  'release: solo v0.7.x publish cycle',
  'admin: tooling + infra',
  'ai: model evals',
  'health: walks + plants',
  'travel: office commutes',
  'projects: solo-web v1',
  'meta: audit pass discipline',
];

const DOC_TITLES = [
  'ADR-0003: Writer actor model',
  'ADR-0004: Multi-tenancy',
  'Designing Data-Intensive Applications — Ch.5',
  'Stripe blog: Idempotency keys',
  'Solo dev-log 0089: v0.7.1 shipped',
];

const rng = mulberry32(0xc0ffee);

function shortLabel(text: string): string {
  return text.length <= 80 ? text : text.slice(0, 77) + '...';
}

// --- Generate nodes ---

function nowMs(): number {
  // Stable timestamp anchor so reload doesn't reshuffle relative ts ordering.
  return Date.UTC(2026, 4, 18); // 2026-05-18
}
const NOW = nowMs();
const DAY_MS = 24 * 60 * 60 * 1000;

const episodes: GraphNode[] = EPISODE_FRAGMENTS.slice(0, 50).map((text, idx) => ({
  id: `ep:${String(idx + 1).padStart(4, '0')}`,
  kind: 'episode',
  label: shortLabel(text),
  preview: text,
  ts_ms: NOW - Math.floor(rng() * 30) * DAY_MS,
}));

const clusters: GraphNode[] = CLUSTER_THEMES.map((theme, idx) => ({
  id: `cl:${String(idx + 1).padStart(3, '0')}`,
  kind: 'cluster',
  label: theme,
  preview: `Cluster of ~${Math.floor(rng() * 8) + 3} related episodes.`,
}));

const entities: GraphNode[] = ENTITY_NAMES.map((name) => ({
  id: `ent:${name}`,
  kind: 'entity',
  label: name,
  preview: `Entity referenced across the memory graph.`,
  ref_count: 0, // recomputed after triples generated
}));

const documents: GraphNode[] = DOC_TITLES.map((title, idx) => ({
  id: `doc:${String(idx + 1).padStart(3, '0')}`,
  kind: 'document',
  label: title,
  preview: `Document: ${title}`,
  ts_ms: NOW - Math.floor(rng() * 60) * DAY_MS,
}));

const chunks: GraphNode[] = [];
for (const doc of documents) {
  const count = 4; // 5 docs × 4 chunks = 20 chunks
  for (let j = 0; j < count; j++) {
    chunks.push({
      id: `chunk:${doc.id.slice(4)}-${j + 1}`,
      kind: 'chunk',
      label: `${doc.label} — chunk ${j + 1}`,
      preview: `Chunk ${j + 1} of ${doc.label}.`,
      ts_ms: doc.ts_ms,
    });
  }
}

// --- Generate edges ---

const edges: GraphEdge[] = [];

// cluster_member: each episode belongs to one cluster
for (const ep of episodes) {
  const cl = pick(clusters, rng);
  edges.push({
    id: `${cl.id}--cluster_member--${ep.id}`,
    source: cl.id,
    target: ep.id,
    kind: 'cluster_member',
  });
}

// triple: 150 triples connecting episodes to entities
const TRIPLE_COUNT = 150;
const refCounts: Record<string, number> = {};
for (let i = 0; i < TRIPLE_COUNT; i++) {
  const ep = pick(episodes, rng);
  const ent = pick(entities, rng);
  const predicate = pick(PREDICATES, rng);
  edges.push({
    id: `${ep.id}--triple-${i}--${ent.id}`,
    source: ep.id,
    target: ent.id,
    kind: 'triple',
    predicate,
  });
  refCounts[ent.id] = (refCounts[ent.id] ?? 0) + 1;
}
// Fill ref_count on entity nodes.
for (const ent of entities) {
  ent.ref_count = refCounts[ent.id] ?? 0;
}

// document_chunk: doc → its chunks
for (const chunk of chunks) {
  // chunk:001-1 → doc:001
  const docNum = chunk.id.split(':')[1].split('-')[0];
  const docId = `doc:${docNum}`;
  edges.push({
    id: `${docId}--document_chunk--${chunk.id}`,
    source: docId,
    target: chunk.id,
    kind: 'document_chunk',
  });
}

const allNodes: GraphNode[] = [...episodes, ...clusters, ...entities, ...documents, ...chunks];

/** Mock for Community's single memory library. */
export function getMockGraph(): GraphResponse {
  return { nodes: allNodes, edges };
}

/** Mock inspect endpoint. */
export function getMockInspect(id: string): InspectResponse | null {
  const graph = getMockGraph();
  const node = graph.nodes.find((n) => n.id === id);
  if (!node) return null;
  const triples_in = graph.edges.filter((e) => e.target === id);
  const triples_out = graph.edges.filter((e) => e.source === id);
  return {
    node,
    full_text: node.preview,
    triples_in,
    triples_out,
  };
}
