# Command-Line Reference

Every flag accepted by every `solo` subcommand. Sourced from
the v0.3.1 binary; new flags will land in this page as they
ship.

Common flags repeated across commands:

  - **`--data-dir <path>`** (env: `SOLO_DATA_DIR`) — override
    the default data directory (`~/.solo` on Unix,
    `%USERPROFILE%\.solo` on Windows). Available on every
    one-shot, `daemon`, `mcp-stdio`, `http-serve`,
    `doctor`, `download-model`.

## `solo init`

Initialise a fresh data directory.

```text
solo init [--data-dir <path>] [--force]
```

  - **`--force`** — wipe Solo-owned files in `--data-dir`
    and re-initialise. **DESTRUCTIVE: all stored memories
    will be lost.** Solo-owned files only — other files in
    the directory are untouched.

Reads `SOLO_PASSPHRASE` from env (with a stderr warning) or
prompts twice on stdin and verifies a match.

## `solo daemon`

Run the long-lived writer + reader pool. Optionally serves
HTTP on a loopback port.

```text
solo daemon [--data-dir <path>]
            [--snapshot-interval-secs <N>]
            [--http-port <N>]
            [--consolidate-interval-secs <N>]
            [--consolidate-window-days <N>]
```

  - **`--snapshot-interval-secs <N>`** — how often to flush
    the in-memory HNSW to disk. Default `300` (5 min). `0`
    disables the timer (snapshot still saves on graceful
    shutdown).
  - **`--http-port <N>`** — also serve the HTTP/JSON API on
    `127.0.0.1:<N>`. Loopback only; no auth flag exposed
    in this mode (use `solo http-serve` for auth + LAN).
  - **`--consolidate-interval-secs <N>`** — auto-trigger
    `consolidate` every N seconds. Default `0` (disabled).
  - **`--consolidate-window-days <N>`** — passed through
    to the auto-consolidate. Default unbounded.

## `solo remember`

One-shot write.

```text
solo remember [<text>]
              [--source-type <type>]
              [--source-id <id>]
              [--data-dir <path>]
```

  - **Positional `<text>`** — the text to remember. Read
    from stdin if omitted.
  - **`--source-type <type>`** — free-form tag. Default
    `user_message`. `solo recall` can filter on this in
    future; for now it's metadata.
  - **`--source-id <id>`** — upstream id (e.g. a chat
    message id) for traceability.

Strips trailing whitespace so `echo "foo" | solo remember`
and `solo remember foo` produce identical embeddings.

Prints the new `MemoryId` (UUID v7) on success.

## `solo recall`

Vector-search.

```text
solo recall [<query>] [--limit <N>] [--data-dir <path>]
```

  - **Positional `<query>`** — query text. Read from stdin
    if omitted.
  - **`--limit <N>`** — max results. Default `5`. Clamped
    to `[1, 100]`.

Output format: one line per hit, columns
`<rowid>  cos_dist=<distance>  <content (≤80 chars)>
[<source_type>/<tier>]`.

`cos_dist` is cosine distance (0.0 = identical, larger =
less similar) — see Model Selection for embedder-specific
caveats.

## `solo consolidate`

Trigger one consolidation pass.

```text
solo consolidate [--window-days <N>]
                 [--force-merge]
                 [--ollama-model <MODEL>]
                 [--data-dir <path>]
```

  - **`--window-days <N>`** — only consider memories with
    `ts_ms >= now - N * 86_400_000`. Default unbounded.
  - **`--force-merge`** — run the existing-vs-existing
    merge + abstraction-regen passes even when there are
    no new candidates. See Consolidation Cycle.
  - **`--ollama-model <MODEL>`** — use local Ollama as the
    Steward LLM backend for this one-shot run. This affects
    abstractions/triples, not the embedder used for vector search.

## `solo reembed`

Re-embed stored memories with the currently-active embedder.
This is the lower-level primitive used by migration workflows.
For an existing data directory moving to a different persisted
backend, prefer `solo migrate-embedder`.

```text
solo reembed [--from-name <name>] [--from-version <v>]
             [--dry-run] [--gc] [--data-dir <path>]
```

  - **`--from-name`** + **`--from-version`** — only re-embed
    memories whose existing embedding identity matches.
    Required together (one without the other is a CLI
    error). Default: every memory regardless of source.
  - **`--dry-run`** — print the migration plan without
    writing.
  - **`--gc`** — delete unreferenced rows from the
    `embeddings` table after the rebuild.

## `solo migrate-embedder`

Safely switch Solo's persisted embedder identity and re-embed the
Memory Library. Use this instead of editing `solo.config.toml`
directly when moving an existing data directory to Ollama.

```text
solo migrate-embedder ollama [--model <MODEL>]
                             [--dim <N>]
                             [--base-url <URL>]
                             [--dry-run]
                             [--data-dir <path>]
```

  - **`--model <MODEL>`** — Ollama embedding model. Default
    `nomic-embed-text`.
  - **`--dim <N>`** — expected dimension. If omitted, Solo
    probes Ollama and records the returned dimension.
  - **`--base-url <URL>`** — Ollama base URL. Default
    `http://localhost:11434`.
  - **`--dry-run`** — validate Ollama and print the plan
    without changing config, databases, or snapshots.

The command requires Solo to be stopped so it can acquire
`solo.lock`. It backs up `solo.config.toml`, backs up existing
HNSW snapshots, re-embeds first without deleting stale rows,
aborts before GC if any row fails, then runs stale-row GC and
removes HNSW snapshots so the next daemon start rebuilds the
graph from SQL embeddings.

## `solo forget`

Soft-delete a memory.

```text
solo forget <memory_id> [--reason <text>] [--data-dir <path>]
```

  - **Positional `<memory_id>`** — UUID v7 of the memory
    to forget.
  - **`--reason <text>`** — free-form note. Default
    `user-initiated`. Logged but not yet persisted.

Status flips to `forgotten`. The HNSW vector stays in the
graph but recall results filter it out.

## `solo inspect`

Print the full record for a memory.

```text
solo inspect <memory_id> [--data-dir <path>]
```

Returns a JSON dump with timestamps, source, status,
scoring values, and content. Soft-deleted memories
(`status = forgotten`) are still returned.

## `solo mcp-stdio`

Run the MCP server over stdin/stdout. Spawned by an MCP
host like Claude Desktop or Cursor — see
[MCP Integration](./mcp-integration.md).

```text
solo mcp-stdio [--data-dir <path>]
               [--no-lockfile]
```

  - **`--data-dir <path>`** — override data dir
    (`SOLO_DATA_DIR`).
  - **`--no-lockfile`** (`SOLO_NO_LOCKFILE=1`) — proxy-friendly
    mode. Skips `solo.lock` acquisition so a gateway can spawn
    multiple ephemeral `mcp-stdio` subprocesses against one shared
    data dir. **Dangerous**: breaks the writer-actor
    single-process invariant. See
    [MCP Integration § Gateway / proxy mode](./mcp-integration.md#gateway--proxy-mode-no-lockfile)
    for safety guidance. v0.11.5+.

## `solo http-serve`

Run the standalone HTTP/JSON server.

```text
solo http-serve [--bind <ip>]
                [--port <N>]
                [--bearer-token-file <path>]
                [--data-dir <path>]
```

  - **`--bind <ip>`** — IP to bind. Default `127.0.0.1`.
    Non-loopback values require `--bearer-token-file`.
  - **`--port <N>`** — TCP port. Default `17821`.
  - **`--bearer-token-file <path>`** — file whose first
    whitespace-trimmed line is the bearer token. Required
    for non-loopback `--bind`.

## `solo doctor`

Print health and setup state without changing the real memory library.

```text
solo doctor [--with-stats]
            [--round-trip]
            [--daemon-url <url>]
            [--data-dir <path>]
```

  - **`--with-stats`** — print database and derived-coverage statistics. If
    the live daemon owns `solo.lock`, doctor queries its authenticated local
    `/v1/status` endpoint instead of trying to open the database and failing
    on the lock.
  - **`--round-trip`** — create an isolated temporary encrypted library and
    verify write → bundled MiniLM embedding → HNSW indexing → recall. The
    temporary library is deleted afterward, so setup validation never
    pollutes the user's memories.
  - **`--daemon-url <url>`** — live daemon base URL used by `--with-stats`.
    Default `http://127.0.0.1:17821`.

Without `--with-stats`, doctor reads only the unencrypted
files (config, lockfile, snapshot file presence + sizes)
and does not need the passphrase.

## `solo backup`

Online encrypted backup. Writes a self-contained SQLCipher
database to `--to <path>`, encrypted with the same Argon2id-
derived key as the source. See [Backups & Recovery](./backups-and-recovery.md)
for restore procedures.

```text
solo backup --to <path> [--force] [--data-dir <path>]
```

  - **`--to <path>`** — destination file. Required. Parent
    directory must exist; the file itself is created.
  - **`--force`** — overwrite `--to` if it already exists.
    Removes the existing file before the backup runs.

Holds `solo.lock` for the duration. Refuses if a daemon or
another one-shot is touching the data dir.

## `solo import`

Preview or import local source folders through narrow importer
subcommands.

```text
solo import markdown <path> [--dry-run] [--json] [--data-dir <path>]
solo import text <path> [--dry-run] [--json] [--data-dir <path>]
solo import json <path> [--dry-run] [--json] [--data-dir <path>]
solo import chatgpt <export-dir-or-json> [--conversation <id-or-title>] [--dry-run] [--json] [--data-dir <path>]
solo import claude <export-dir-or-json> [--conversation <id-or-title>] [--dry-run] [--json] [--data-dir <path>]
solo import bookmarks <file> [--dry-run] [--json] [--data-dir <path>]
```

  - **`markdown <path>`** — scans a Markdown file or directory
    for `.md` and `.markdown` files.
  - **`text <path>`** — scans a plain text file or directory for
    `.txt` files.
  - **`json <path>`** — scans a JSON/JSONL file or directory for
    `.json`, `.jsonl`, and `.ndjson` files.
  - **`chatgpt <path>`** — parses ChatGPT `conversations.json`
    exports into stable Markdown transcript documents.
  - **`claude <path>`** — parses Claude conversation exports into
    stable Markdown transcript documents.
  - **`bookmarks <file>`** — parses browser bookmark HTML/JSON
    exports into metadata-only Markdown documents. It does not crawl
    pages.
  - **`--dry-run`** — prints scanned files, candidate files,
    skipped hidden/unsupported files, and an estimated chunk count
    without opening the encrypted database.
  - **`--json`** — emits structured dry-run counts and metadata.
    It is only supported with `--dry-run`.
  - **`--data-dir <path>`** — data dir used for config and
    materialized schema-import files. Defaults to `~/.solo`
    (`SOLO_DATA_DIR`).
Extension importers honor `[documents].allowed_extensions` from
`solo.config.toml`. Schema-aware importers materialize records under
`<data-dir>/imports/<source>/` before using the same writer document
ingest path as `solo ingest`, so chunking and document memory storage
behave the same way.

## `solo project`

Codebase memory helpers for local projects.

```text
solo project init [path] [--name <name>] [--id <id>] [--tag <tag>] [--force]
solo project ingest [path] [--dry-run] [--json] [--max-files <n>]
solo project facts [path] [--subject <name>] [--limit <n>] [--json]
solo project decisions [path] --add <text> [--json]
solo project decisions [path] --query <text> [--limit <n>] [--json]
solo project policy [path] [--client generic|codex|claude|cursor] [--json]
```

`init` writes `.solo/project.toml`. `ingest` imports root README-style
files and docs/ADR folders while skipping generated/vendor directories;
`--json` emits the dry-run candidate list without opening the encrypted
database. `decisions --add` stores a project-scoped durable memory;
`--query` recalls decisions with the same project identity. `policy`
prints a repo-aware memory policy snippet for coding agents; `--json`
returns the policy text with project metadata. `facts --json` and
`decisions --json` expose project-scoped facts, new decision ids, and
filtered decision recall hits for agents and UI clients.

## `solo eval`

Offline deterministic memory-quality checks.

```text
solo eval list [--json]
solo eval run [<fixture-or-path>] [--all] [--json] [--top-k <n>] [--save] [--report-dir <path>]
solo eval report <run-id-or-path> [--json] [--report-dir <path>]
```

`list` shows bundled fixtures. `run` scores a bundled fixture name or a
JSON fixture file, and `--all` scores every bundled fixture for CI.
`--save` writes a JSON report to `.solo/eval-runs/` by default and
prints the generated `run_id` in JSON output. `report` reads that saved
run id, or a direct path to a saved JSON report.
