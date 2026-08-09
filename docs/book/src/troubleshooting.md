# Troubleshooting

A catalog of failure modes you might hit, organised by where
they tend to show up. For each: what you see, what's
actually happening, and how to fix or work around it.

## Install / build

### `cargo install solo-cli` fails on Windows with "Perl not found"

```text
error: failed to run custom build command for `openssl-sys vX.Y.Z`
```

The vendored OpenSSL build script needs `perl` on `PATH`.
Install [Strawberry Perl](https://strawberryperl.com/), then
re-run `cargo install` with the Strawberry directories
prefixed:

```bash
PATH="/c/Strawberry/perl/bin:/c/Strawberry/c/bin:$PATH" \
  cargo install solo-cli
```

The Strawberry installer adds these to `PATH` system-wide,
but the prefix-on-the-command-line form is foolproof — it
works even if a previous shell session inherited a
stripped-down `PATH`.

### `cargo install` fails on Linux with missing system libs

On a fresh Debian / Ubuntu container:

```bash
apt-get update && apt-get install -y \
  build-essential perl pkg-config
```

`build-essential` covers the C compiler + linker; `perl` is
needed for vendored OpenSSL; `pkg-config` is occasionally
needed by transitive deps.

### Long compile times

A clean install builds SQLCipher, HNSW, the rmcp framework,
axum, and reqwest — none of which are tiny. 5-10 minutes is
typical for the first compile. Subsequent rebuilds use the
cargo cache and finish in seconds.

If you're iterating on Solo itself rather than installing,
work from a `git clone` and let cargo's incremental
compilation help you.

## Startup

### `solo.lock already held`

```text
Error: acquire solo.lock — daemon or another one-shot
already running?
```

Solo enforces single-writer access to the data directory via
a lockfile. Common causes:

  - **A daemon is running** on the same data dir. Either
    stop the daemon (Ctrl+C), or run the one-shot against a
    different `--data-dir`.
  - **A previous Solo process crashed** without releasing
    the lock. Check whether `solo` shows in `ps` /
    Task Manager. If not, the lockfile is stale — `rm
    /path/to/.solo/solo.lock` and retry. (Solo doesn't
    auto-clean stale lockfiles; the explicit step is
    deliberate so you don't race a real daemon by accident.)
  - **An MCP host has Solo loaded.** Closing the host
    releases the lock. See
    [MCP Integration](./mcp-integration.md) for the
    one-host-at-a-time constraint.

### Wrong passphrase

```text
Error: open SQLCipher with derived key
   caused by: SqliteFailure(...) "file is not a database"
```

SQLCipher's wrong-key error mode is to look like a corrupt
database. If you're sure the data dir is intact, the most
likely cause is a typo in `SOLO_PASSPHRASE` or a wrong
input at the prompt. Try again carefully.

If you've actually forgotten the passphrase, **the data is
unrecoverable**. SQLCipher is doing its job — without the
passphrase there's no key, and without the key the
ciphertext is opaque. Restore from a backup (see
[Backups & Recovery](./backups-and-recovery.md)) or
`solo init --force` to start fresh.

### `live HNSW snapshot pair missing` warning on first run

Expected on a fresh data dir. Solo logs:

```text
WARN live HNSW snapshot failed; trying .bak
WARN no HNSW snapshot available; starting fresh empty index
```

The startup chain looks for `hnsw_episodes.hnsw.{data,graph}`
(live snapshot) and `hnsw_episodes_bak.hnsw.{data,graph}`
(backup). On a brand-new data dir neither exists yet — the
first `remember` triggers a save-on-shutdown, which leaves
both for the next run.

If you see this on a non-fresh dir where you've stored
memories, that's a real problem — the rebuild-from-SQL
fallback runs but it's slower and doesn't preserve graph
quality. Diagnose with `solo doctor` and check whether
`embeddings` table has rows.

### `HNSW vs episodes drift detected`

```text
WARN hot_episodes=N index_len=M diff=K HNSW vs episodes
drift detected
```

The HNSW snapshot's vector count doesn't match the
`embeddings` table's row count for active+hot memories. Solo
proceeds anyway (the recall path tolerates drift) but recall
quality may suffer.

Causes:

  - **Crash mid-write**: an episode wrote to SQL but the
    HNSW snapshot didn't save before the process died.
    `solo daemon` recovers via `pending_index` replay on
    next startup; one-shot commands recover via the same
    path, but only the pending row is replayed, not arbitrary
    drift.
  - **Manual file deletion**: someone deleted the snapshot
    files but kept the database.
  - **Mixed embedder writes** (see below): if old StubEmbedder
    rows are still in `embeddings` but the active embedder
    is BGE-M3, the row count mismatch surfaces as drift.

Fix: `solo reembed`. Rebuilds HNSW from the active embedder's
vectors, drops drifted state.

#### Pre-v0.11.2: drift accumulates from forgotten document chunks

If you saw recurring "drift detected" warnings on every restart on
v0.11.0 or v0.11.1 and you had ever forgotten a document, that was a
known bug — the in-memory tombstone set was rebuilt only from
forgotten episodes, never from forgotten chunks. Fixed in v0.11.2
via a new `rebuild_chunk_tombstones_from_sql` startup pass. A single
post-upgrade startup zeroes the historical drift.

### `pending_index orphan rows GC'd`

```text
INFO orphan_episodes=N orphan_chunks=M pending_index orphan rows
GC'd (target was forgotten or missing)
```

New in v0.11.2. Emitted at startup when the recovery pass found
`pending_index` rows whose target episode or document has been
soft-deleted (or whose underlying row no longer exists). Solo
DELETEs the orphan rows so they don't re-replay every startup.
Counts > 0 are informational, not an error — they mean the outbox
was healing itself.

### `consolidation running with StubEmbedder`

```text
ERROR consolidation running with StubEmbedder — cluster membership
is BLAKE3-hash proximity, not semantic.
```

New in v0.11.2. Solo emits this on every consolidate pass when the
active embedder is the 32-dim stub. The pass still proceeds (so
test suites don't break), but clusters group by surface-text hash
similarity, not meaning. For production:

  - Set `SOLO_EMBEDDER=bundled` (uses the in-process embedder) or
    `SOLO_EMBEDDER=ollama` + `SOLO_OLLAMA_EMBED_MODEL=...` (uses a
    local Ollama instance) to switch to a real embedder.
  - Or set `SOLO_REFUSE_STUB_EMBEDDER=1` to make consolidation
    refuse to run with the stub — a hard error is easier to spot
    in CI than a warning in a log stream.

## Recall

### Recall returns nothing despite having stored memories

  - **Index empty**: `(no results — index has 0 vectors)`
    means the HNSW snapshot didn't load. See the drift
    section above.
  - **Index non-empty but no hits**: the StubEmbedder caveat
    — exact-text-match only. `recall("memory")` doesn't find
    a memory containing `"remembrance"` because BLAKE3
    hashes of those strings are unrelated. Switch to BGE-M3
    for semantic recall (see Model Selection).
  - **All matches forgotten**: `(no results — index has N
    vector(s); HNSW returned no hits or all were forgotten)`.
    Check `solo doctor --with-stats` for the
    active/forgotten breakdown.

### Mixed-embedder corruption

```text
WARN embedder identity changed; existing vectors may be
incoherent — run `solo reembed`
```

You stored some memories with the StubEmbedder, then later
set `SOLO_BGE_M3_DIR` and stored more. The HNSW now
contains both kinds of vectors, which live in different
spaces. Recall results become incoherent — sometimes the
right episode comes back, sometimes a stub-hash collision
returns garbage.

Fix: `solo reembed`. Regenerates every vector with the
currently-active embedder. Time scales with corpus size.

### `BGE-M3 model dim ({}) does not match persisted config`

```text
Error: BGE-M3 model dim (1024) does not match persisted
config.embedder.dim (768). Run `solo init --force` if you
really meant to switch embedders.
```

The persisted embedder identity in `solo.config.toml` says
`dim = 768` (you initialised against a different model
once), but the BGE-M3 model files at `SOLO_BGE_M3_DIR`
report dim 1024. Solo refuses to start to prevent silent
mis-comparison.

Fix: either point `SOLO_BGE_M3_DIR` at the right model, or
`solo init --force` to wipe the data dir and start over with
the new dim. There's no in-place migration — vectors of
different dims aren't compatible.

## Daemon

### Daemon doesn't shut down cleanly

Ctrl+C should drain in-flight HTTP requests, save the HNSW
snapshot, and exit within a few seconds. If it hangs:

  - **A long-running consolidate** is in progress. Watch
    `RUST_LOG=info` output — you'll see `consolidate
    complete` when it's done. Hitting Ctrl+C again forces
    abort, which may leave the snapshot un-saved (the next
    run will fall back to rebuild-from-SQL).
  - **An LLM call is mid-retry**. The retry loop sleeps up
    to 30s (3 retries × 10s cap). Wait it out or Ctrl+C
    twice.
  - **An axum graceful-shutdown stuck**. Open issue with
    `RUST_LOG=axum=trace,info` output.

### `--http-port` collides with another service

```text
Error: Address already in use (os error 48 / 98 / 10048)
```

Another process owns the port. Pick a different port via
`--http-port <N>` (daemon co-mode) or `--port <N>` (`solo
http-serve`).

### `--bind 0.0.0.0` rejected without bearer token

```text
Error: binding to 0.0.0.0 (non-loopback) requires
--bearer-token-file. Refusing to expose the API without
authentication.
```

By design — see [HTTP API](./http-api.md)'s loopback-vs-LAN
section. Pass `--bearer-token-file <path>` to authenticate.

## MCP

### MCP host shows `solo` as failed / disconnected

The most common cause is the lockfile — your MCP host
spawned `solo mcp-stdio` against a data dir already locked
by another Solo process (see _Startup_).

Other causes:

  - **`command` not on PATH**. Use the absolute path to
    `solo` / `solo.exe` in the MCP host's config file.
  - **Missing `SOLO_PASSPHRASE`**. The MCP host launches
    Solo with `env` from the config file; if you forgot to
    include the passphrase, Solo prompts on stdin (which
    the host has wired to JSON-RPC, not a TTY) and hangs.
  - **Wrong `SOLO_DATA_DIR`**. If the path doesn't exist
    or doesn't contain a `solo.config.toml`, Solo bails with
    "Run `solo init` first."
  - **stdin trickle / framing mismatch**. Rare. If
    everything else looks right, run `solo mcp-stdio`
    manually with the [MCP Inspector](https://github.com/modelcontextprotocol/inspector)
    to verify the server side is healthy.

### Tools list works but every call returns errors

Likely the data dir is initialised but the embedder isn't
set up. Check whether the host's `env` block sets
`SOLO_BGE_M3_DIR` (if you intended BGE-M3) and re-launch
the host.

## Consolidate

### `consolidate` produces 0 abstractions despite LLM key set

```text
consolidate complete: episodes_seen=X clusters_built=Y
episodes_clustered=Z abstractions_built=0 ...
```

The Steward isn't being constructed. Check:

  - **Env var spelling**: `ANTHROPIC_API_KEY` (no
    `_TOKEN` / `_KEY_VALUE` / etc.). Same for `OPENAI_API_KEY`.
  - **Empty value**: `ANTHROPIC_API_KEY=` (blank) is treated
    as unset. Use a real key.
  - **Value truncation**: some shells silently truncate at
    quoted-string boundaries. Use single quotes or set the
    env via the MCP host config (`env` block) where
    quoting is unambiguous.
  - **Precedence shadowing**: if both vars are set,
    Anthropic wins. If `ANTHROPIC_API_KEY` is invalid but
    set, Solo doesn't fall through to OpenAI — the
    Anthropic call fails. `unset ANTHROPIC_API_KEY` to
    fall through.

The startup log line confirms which provider got wired:

```text
INFO LLM backend wired model=claude-3-5-sonnet-20241022
INFO consolidate will produce abstractions + contradictions
```

If you don't see this line, the Steward is `None` and only
clustering will run.

### `consolidate` runs but `--force-merge` doesn't help

`--force-merge` only kicks in when there are no new
candidates. If `episodes_seen > 0`, the merge passes are
already running as part of the normal flow. The flag is for
quiet corpora where the empty-candidates early-return
short-circuits.

If you're seeing drift accumulate despite frequent
`consolidate` runs, the cause is more likely the
existing-vs-existing merge being LLM-gated. Without an LLM,
existing clusters can't merge. Set `ANTHROPIC_API_KEY` (or
similar) and re-run.

## Performance

### Recall is slow on large corpora

HNSW recall is sub-linear in corpus size, so "large" usually
means tens of millions of vectors before performance is
visible. Two more likely causes for what feels slow:

  - **Cold start**: first recall after `solo daemon` boots
    has to deserialize the HNSW snapshot (1-2 sec for ~1
    GB graphs).
  - **BGE-M3 first inference**: the initial model load
    (1-2 sec for the safetensors mmap + tokenizer parse)
    happens lazily on first `remember` or `recall`.
    Subsequent calls reuse the loaded model.

Profile with `RUST_LOG=info` — Solo logs each phase's
timing. If the embedder phase dominates, `solo reembed`
won't help (you're already on the right embedder); if the
HNSW search phase dominates on a small corpus, file an
issue with the snapshot size and recall latency.

### Daemon memory grows unbounded

Reader connections in the pool are bounded; the writer
holds one connection. The HNSW graph grows linearly with
corpus size — at ~1 KB per 1024-dim f32 vector plus graph
overhead, expect ~1.5 GB resident for a million memories.

If memory grows beyond what the corpus size predicts, file
an issue with `RUST_LOG=info` startup output and an
in-flight memory snapshot.

## Filing an issue

When `RUST_LOG=info` doesn't surface the cause, file an
issue at [github.com/CallMeJones/solo-community/issues](https://github.com/CallMeJones/solo-community/issues)
with:

  - The exact `solo --version` output.
  - The command line that triggered the failure.
  - The full log output (set `RUST_LOG=info` or the
    relevant subsystem at `=debug`).
  - `solo doctor --with-stats` output.
  - For HTTP API issues: the request body and response.
  - For MCP issues: the host config (with the passphrase
    redacted) and any visible error message in the host
    UI.

The doctor output + log output together usually pin
diagnosis to a single subsystem.
