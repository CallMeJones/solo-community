# Consolidation Cycle

Solo's recall pipeline doesn't stop at "find episodes by vector
similarity." Periodically - either on demand via `solo
consolidate` or on timers inside `solo daemon` - it turns a
flat pile of episodes into structured layers:

```text
episodes  ─►  clusters       (group related episodes)
          ─►  abstractions   (one-paragraph summary per cluster)
          ─►  triples        (subject-predicate-object facts)
          ─►  contradictions (pairs of triples that disagree)
```

This chapter explains what each stage does, when consolidation
runs, what the report fields mean, and the few flags you might
want to tune.

## The three forward stages

### 1. Clustering

The clustering pass is **pure-deterministic**. It reads every
`active`+`hot` episode that hasn't already been assigned to a
cluster, runs a cosine-similarity threshold over the
embeddings, and groups episodes whose vectors land above the
threshold into clusters via union-find. No LLM involved.

Two consequences:

  - The clustering algorithm gives the same answer on the same
    inputs, every time. If you re-run `consolidate` with no
    new data, the cluster set is unchanged.
  - The threshold is the only knob. It defaults to a value
    that's tuned for BGE-M3 + typical conversational
    memories. If you switch to a different embedder, the
    "right" threshold may differ; today this isn't user-
    exposed (it's a `StewardConfig` field). Open an issue
    if your corpus needs tuning.

Each newly-formed cluster gets:

  - A unique `cluster_id` (UUID v7, timestamp-prefixed).
  - A list of member `memory_id`s (the episodes that joined).
  - A **centroid** — the count-weighted mean of member
    embeddings, re-normalised to unit length.
  - A **coherence** score — the mean pairwise cosine within
    the cluster. Higher = tighter theme.

### 2. Abstraction (Steward LLM)

If a Steward LLM is configured (`ANTHROPIC_API_KEY` or
`OPENAI_API_KEY`), Solo then asks it to summarise each new
cluster. The prompt feeds the cluster's member episodes (in
chronological order) and asks for a one-paragraph
abstraction in JSON.

Each abstraction also gets a **provenance** record: which LLM
produced it, against which model name, at which timestamp.
You can inspect abstractions later via `solo recall` (which
also returns abstraction matches) or directly via the HTTP
API.

Without a Steward, this stage is skipped. `consolidate`
reports `abstractions_built=0`; the cluster rows still
persist.

### 3. Triple extraction + contradiction detection

The Steward also extracts **triples** from each abstraction:
short subject-predicate-object facts like `(I, prefer, dark
mode)` or `(project_acme, deadline, 2026-06-15)`.

Triples carry validity windows (`valid_from_ms`, optional
`valid_to_ms`) so "I used to use Vim, now I use Helix"
produces two triples with non-overlapping windows rather
than one contradiction.

A two-stage contradiction detector runs over every pair of
triples in the new round:

  - **Stage 1** (rule filter, pure-Rust): cheap short-circuits
    — same subject + same predicate + different object +
    overlapping validity windows = candidate. Most pairs
    don't survive this stage.
  - **Stage 2** (LLM judge): the survivors get sent to the
    Steward, which decides whether they actually disagree
    (vs. e.g. compatible facts the rule filter couldn't
    distinguish).

Detected contradictions persist in the `contradictions` table
with both triple ids and the LLM's rationale. They surface
in the consolidate report as `contradictions_found=N`.

## The re-consolidation tetralogy

A flat "cluster what's new" pass is enough for a freshly-
populated database. Once memories accumulate over months,
clusters that should be the same theme drift apart. v0.3
added four passes that fold that drift back together. They
run automatically as part of every `solo consolidate`:

### a. In-run merge

Two clusters built in the **same** consolidate run sometimes
have nearly-identical centroids — typical case is a
conversation that straddled UTC midnight, splitting across
the per-day clustering window into two clusters with the
same theme.

**In-run merge** is a post-pass over the just-built clusters
that folds any pair above the merge cosine threshold into
one. Pure-Rust, no LLM.

Reports as `clusters_merged=N`.

### b. Cross-run absorb

A new cluster (built this run) sometimes has a centroid
that closely matches an **existing** cluster from a prior
run — the same theme came up again with new episodes.

**Cross-run absorb** folds the new episodes into the existing
cluster: same `cluster_id`, refreshed centroid + coherence,
no new cluster row. The existing abstraction may now be
stale (it didn't know about the freshly-absorbed episodes),
which kicks off the regen pass below.

Reports as `clusters_absorbed=N`.

### c. Existing-vs-existing merge

Two **existing** clusters can drift toward each other across
many consolidate runs (every absorb pulls a centroid
slightly), eventually crossing the merge threshold.

**Existing-vs-existing merge** is the long-tail case: catch
two pre-existing DB clusters that should now be one and
coalesce them. This pass is **Steward-gated** — it requires
an LLM to resolve which cluster wins (the LLM judges based
on which abstraction better represents the merged set).
Without an LLM, this pass doesn't fire even if drift is
present.

Reports as `existing_clusters_merged=N`.

### d. Abstraction regeneration

Every absorb or existing-merge that fires invalidates the
existing cluster's abstraction (the abstraction was written
about a smaller set of episodes). The **abstraction-regen
pass** drops the stale `semantic_abstraction` row, walks the
new episode set, and asks the Steward for a fresh
abstraction. Triples derived from the old abstraction
cascade-delete via the `triples.cluster_id` FK introduced
in schema migration 0002.

Reports as `abstractions_regenerated=N`.

### Why it matters

Without re-consolidation, your "pasta cooking" cluster from
March and your "Italian dinner ideas" cluster from April end
up as separate themes forever — even after fifty more
"pasta" conversations have made it obvious they're the same
theme. The tetralogy keeps the cluster topology coherent
over time without you having to think about it.

## When does consolidation run?

### One-shot (manual)

```bash
solo consolidate
```

Reads everything that's clusterable (active+hot, current
embedder, not-already-clustered), runs the full pipeline,
prints the report, exits. Good for:

  - After a bulk import — get the new memories clustered now
    rather than waiting on the daemon.
  - End-to-end smoke tests where you want deterministic
    timing.
  - Operator-driven catch-up.

### Daemon-scheduled

```bash
solo daemon --consolidate-interval-secs 3600
```

Inside `solo daemon`, an interval-driven background task
fires `consolidate` every N seconds. Default is `0`, so the
daemon serves recall but doesn't auto-consolidate. Set a
positive value to enable.

Triple extraction is also daemon-backed. The `[triples]`
timer wakes on its own cadence, or after enough new episodes,
and asks the Steward LLM to attach abstractions and triples
for clustered memories. That means a manual `solo consolidate`
can report newly built clusters before the daemon's triples
batch has written triples.

Pick the interval based on your write rate. For typical
personal use (dozens of memories per day), once an hour is
plenty. For a high-write integration (an agent dumping
thousands of memories), every few minutes might make sense.

### Drift catch-up: `--force-merge`

The empty-candidates path normally short-circuits cheaply:
if there are no new memories to cluster, `consolidate`
reports zeros and exits without running the merge passes.

For a quiet corpus that hasn't seen new memories in a
while but has accumulated drift between existing clusters,
you can force the merge passes to run anyway:

```bash
solo consolidate --force-merge
```

This bypasses the empty-candidates early return and runs the
existing-vs-existing merge + abstraction-regen passes. Useful
when:

  - You're cleaning up an old corpus that hasn't had fresh
    writes in weeks.
  - You bumped the merge threshold and want existing clusters
    re-evaluated against the new bound.
  - You're investigating recall quality and suspect drift.

`--force-merge` only matters when there are no new
candidates; with new data, the merge passes always run as
part of the normal flow.

## Reading the report

A successful `solo consolidate` prints a one-line summary:

```text
consolidate complete: episodes_seen=42 clusters_built=3 \
  episodes_clustered=27 abstractions_built=3 triples_built=12 \
  contradictions_found=1
```

Field meanings:

| field | meaning |
|---|---|
| `episodes_seen` | count of clusterable episodes the run considered (active+hot, current embedder, not-already-clustered, optionally within `--window-days`). |
| `clusters_built` | count of new clusters this run formed. |
| `episodes_clustered` | count of episodes that actually got assigned to a cluster (non-cluster outliers don't count). |
| `abstractions_built` | count of new abstractions written. Without a Steward, always 0. |
| `triples_built` | count of triples extracted during this run. In daemon mode, triples may instead arrive on the triples-batch cadence. Without a Steward, always 0. |
| `contradictions_found` | count of contradictions detected during this run. In daemon mode, later triples batches can create more work for contradiction checks. Without a Steward, always 0. |

The report doesn't surface the re-consolidation counts
directly in the CLI today (`clusters_merged`,
`clusters_absorbed`, `existing_clusters_merged`,
`abstractions_regenerated`). They're available via the HTTP
API's response body. Will surface in the CLI report once we
ship a v0.3.x with a wider one-line summary.

A few common report shapes:

```text
# fresh database, no LLM, just clustered
episodes_seen=10 clusters_built=2 episodes_clustered=8 \
  abstractions_built=0 triples_built=0 contradictions_found=0

# nothing new, --force-merge bypassing the early return
episodes_seen=0 clusters_built=0 episodes_clustered=0 \
  abstractions_built=0 triples_built=0 contradictions_found=0

# fresh data + Steward producing abstractions + triples
episodes_seen=42 clusters_built=3 episodes_clustered=27 \
  abstractions_built=3 triples_built=12 contradictions_found=1
```

## Idempotency

Running `consolidate` twice on the same data is safe — the
second run sees nothing new and exits cleanly. The
guarantees:

  - Episodes already in `cluster_episodes` aren't re-
    clustered.
  - Abstractions already written aren't re-written (until
    their cluster gets absorbed or merged).
  - Re-runs don't duplicate cluster rows, triple rows, or
    contradiction rows.

This means safe scheduling is simple: pick a daemon
interval that's frequent enough for your write rate, and
don't worry about overlap with manual runs.

The lockfile (`solo.lock`) prevents two consolidate runs at
once on the same data dir, so even if you run `solo
consolidate` while a daemon is auto-consolidating, the
later command waits or refuses.

## When you don't need a Steward

If you're using Solo as pure semantic-recall storage — store
episodes, query by vector similarity, never look at the
distilled layers — you don't need an LLM at all.
`consolidate` still runs; it just stops at the clustering
stage. The recall pipeline doesn't care about abstractions
or triples.

The forward path "add a Steward later" works without
re-importing data: when you configure a Steward LLM, the
daemon triples-batch path can walk already-clustered rows
and generate abstractions/triples for them. No embedder
migration or reimport is needed.

## What you can't tune today

  - **Cosine thresholds** (cluster, merge, absorb) — set
    inside `StewardConfig`. Reasonable defaults for BGE-M3
    + conversational memories. CLI flags would be a
    follow-up if real-world corpora need tuning.
  - **Per-cluster max episode count** — there's no upper
    bound; very-popular themes accumulate without bound. In
    practice this hasn't been a recall-quality issue.
  - **Re-consolidation cadence** — the four passes are part
    of every consolidate. You can't run "just the merge
    passes" or "just the regen pass." `--force-merge` is
    the closest thing to a partial-run flag.

If any of these limitations bite, file an issue with your
use case.

## What lives where

| layer | table | written by |
|---|---|---|
| episodes | `episodes` | `solo remember` |
| embeddings | `embeddings` | `solo remember` (and `solo reembed`) |
| clusters | `clusters` + `cluster_episodes` | clustering pass |
| abstractions | `semantic_abstractions` | Steward (abstraction pass) |
| triples | `triples` | Steward (triple-extraction pass) |
| contradictions | `contradictions` | Steward (contradiction-detection pass) |

All in the encrypted SQLCipher database. Everything cascades
correctly: deleting a cluster row drops its episode
assignments, abstractions, triples, and contradictions in
one go (schema migration 0002 wired the FKs).
