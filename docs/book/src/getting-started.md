# Getting Started

This chapter takes you from **nothing installed** to **Solo
running with one memory stored and recalled**, in about ten
minutes.

## Prerequisites

  - **A Rust toolchain** (stable, 1.78 or newer). Solo is
    distributed as Rust source on crates.io, so the install
    step compiles it locally.
  - **Perl in PATH** for the vendored OpenSSL build. On macOS
    and Linux, Perl ships by default. On Windows, install
    [Strawberry Perl](https://strawberryperl.com/) and ensure
    `perl.exe` and `gcc.exe` are on `PATH` (Strawberry's
    standard install puts them at `C:\Strawberry\perl\bin`
    and `C:\Strawberry\c\bin`).
  - **About 2 GB of free disk** if you plan to use the BGE-M3
    embedder (1.2 GB for the model weights plus headroom for
    embeddings). The default StubEmbedder fallback is fine for
    a smoke test and uses negligible disk.
  - **An LLM API key** if you want consolidation to produce
    abstractions and detect contradictions. Solo currently
    supports Anthropic and OpenAI. Without a key the cluster
    pass still runs; abstractions / contradictions stay at 0.
    See [Model Selection](./model-selection.md) for the
    trade-offs.

## Install

### From crates.io (recommended)

```bash
cargo install solo-cli
```

On Windows, prefix the command with the Strawberry Perl PATH so
the SQLCipher build can find Perl:

```bash
PATH="/c/Strawberry/perl/bin:/c/Strawberry/c/bin:$PATH" \
  cargo install solo-cli
```

The build takes about five minutes the first time (SQLCipher,
HNSW, and the rmcp framework are the heavy bits). When it
finishes, the `solo` binary lives in `~/.cargo/bin/solo` (Linux
/ macOS) or `%USERPROFILE%\.cargo\bin\solo.exe` (Windows). Make
sure `~/.cargo/bin` is on `PATH`.

Verify:

```bash
solo --version
# solo 0.3.1
```

### From source

```bash
git clone https://github.com/CallMeJones/solo-community
cd solo
cargo build --release
# Binary at target/release/solo
```

The from-source path is useful if you want to track `main` for
unreleased fixes, or if you plan to contribute.

### Native Desktop smoke on macOS/Linux

The tray/Desktop app uses native webview stacks, so it must be built and
smoked on the target OS. On macOS or Linux, run:

```bash
./scripts/native_tray_smoke.sh
```

The script prints dependency hints, then runs the same `solo-tray`
check/test/clippy gates used by CI. To also launch the owned Solo
Desktop window from a logged-in desktop session:

```bash
SOLO_NATIVE_SMOKE_WINDOW=1 ./scripts/native_tray_smoke.sh
```

Set `SOLO_NATIVE_SMOKE_DESKTOP_URL` if your daemon is not serving
`http://127.0.0.1:17821/desktop/`.

## Initialise a data directory

Solo stores everything inside a single data directory. Default
location is `~/.solo` (Unix) or `%USERPROFILE%\.solo`
(Windows). Override with `SOLO_DATA_DIR` or `--data-dir`.

```bash
solo init
```

You'll be prompted twice for a passphrase. Pick something you
will not lose — the passphrase derives the SQLCipher key, and
**there is no recovery if you forget it**. If you want to skip
the prompt for scripts and tests, set `SOLO_PASSPHRASE`:

```bash
SOLO_PASSPHRASE='choose a real passphrase' solo init
```

Solo emits a stderr warning when it reads the passphrase from
the environment — env-var values are visible to other processes
on Linux via `/proc`, so prefer the prompt for daily use.

After `solo init` you should see something like:

```text
Initializing Solo data directory at /home/me/.solo
Deriving key (~500ms with Argon2id) ...

✓ Created /home/me/.solo/solo.db
✓ Wrote   /home/me/.solo/solo.config.toml
  Schema  v2

Done. Run `solo daemon` to start the memory daemon.
```

Two files matter:

  - `solo.db` — the encrypted SQLite database. The file is
    unintelligible without your passphrase.
  - `solo.config.toml` — the persisted Argon2 salt + chosen
    embedder identity. Plaintext on purpose; needed to derive
    the key on every subsequent open.

## Store your first memory

```bash
solo remember "Solo is the local-first memory daemon I started using on 2026-05-07."
```

Output:

```text
✓ remembered: 019e0425-51fa-7ff2-a095-871df676d440
```

That hex string is a UUID v7 — timestamp-prefixed, so memory
ids sort chronologically. Save it if you want to look the
record back up by id with `solo inspect`.

## Recall what you wrote

```bash
solo recall "memory daemon"
```

Output:

```text
     1  cos_dist= 0.0000  Solo is the local-first memory daemon I…  [user_message/hot]
```

The `cos_dist` column is **cosine distance** — `0.0` means the
query embeds to the exact same vector as the stored memory
(StubEmbedder is deterministic, so identical inputs give
identical vectors). Larger numbers = less similar. The format
mirrors the wire field name (`cos_distance`) returned by the
HTTP and MCP transports.

Solo Desktop exposes the same first-memory loop in the Memory view:
use Inbox to save a durable note into the Memory Library, then use
Recent memories to review the latest 100 items and confirm it landed,
and use Recall to query the library through the running daemon. The
Desktop path requires the
daemon to be unlocked first; it does not pass the database passphrase
through command-line arguments or environment variables. Memories saved
this way use source type `solo_desktop.inbox` so they can be
distinguished from agent-written memories later. Use Inspect on a
recent or recalled row to view the full content, source, status, and
scoring signals. Recent rows can also be approved or dismissed from
the Inbox filter without changing the underlying memory content.
The review queue shows the Memory Library, loaded/visible counts, and
the selected review/source filters. The visible set can be approved,
dismissed, reset back to Needs review, or copied as a summary; those
review actions are stored in the Solo daemon for the Memory Library,
with a local Desktop compatibility cache.
When the daemon exposes source and salience metadata, the same Inbox
can narrow the queue to high-salience, user-created, agent-created,
tool-output, document-derived, or Solo Desktop memories.
Active memories can be corrected from the same panel;
the update path rewrites the memory and refreshes its recall embedding.
The same detail panel also has a guarded Forget action for active
memories: it requires an explicit checkbox before Solo sends the delete
request and refreshes the recent-memory list.
Context preview builds the same `/memory/context` bundle that agent
clients can request, showing recall, facts, themes, and section health
for the Memory Library before you rely on an MCP client.
The Memory view also shows the latest Steward-flagged contradictions for
the Memory Library, including lifecycle state and both joined triples
when those triples are still available. After reviewing the conflict,
use A current, B current, or Reopen to update the contradiction
lifecycle through the daemon.

Two important caveats for first-run recall:

  - **StubEmbedder vs. BGE-M3.** Without a model weights
    directory, Solo uses the StubEmbedder — a deterministic
    BLAKE3 hash of the input text. It can do exact-match
    recall (identical text → identical vector → distance 0)
    but **not semantic recall**: `recall("memory")` won't find
    the episode that says "remembrance" because the BLAKE3
    hashes are unrelated. For real semantic recall, set
    `SOLO_BGE_M3_DIR` or run `solo download-model`. See
    [Model Selection](./model-selection.md).
  - **Mixing modes corrupts recall.** Stub vectors and BGE-M3
    vectors live in different vector spaces. If you store
    some memories with the stub and some with BGE-M3, recall
    results become incoherent. Solo logs a warning when it
    detects a mode switch against a non-empty database; the
    fix is `solo reembed` to regenerate every vector with the
    active embedder.

## Inspect the data dir

```bash
solo doctor --with-stats
```

This prints a health report: which files exist, sizes, the
embedder identity, lockfile state, and live database stats
(episode counts by tier, HNSW vector count, drift state). It's
the first command to reach for if anything looks wrong.

## What runs continuously

The above commands are **one-shot** — they open the database,
do their work, and close it. For a long-running setup that
your AI assistant connects to:

```bash
solo daemon
```

This boots the writer actor + reader pool, starts a periodic
HNSW snapshot saver (every 5 minutes by default), and blocks.
Add `--http-port 17821` to also serve the HTTP/JSON API on
that port (loopback only — see [HTTP API](./http-api.md) for
the LAN-capable `solo http-serve` subcommand), or run
`solo mcp-stdio` instead for a subprocess that an MCP client
spawns directly.

The daemon and one-shot commands compete for the same
data-dir lockfile (`solo.lock`). You can't run both at once
on the same data dir. If you need to test something while
your daemon is running, point the one-shot at a different
`--data-dir`.

## What's next

  - **Wire your AI assistant.** Solo is most useful when an
    LLM client speaks MCP to it. Claude Desktop, Cursor, and
    any other MCP host can spawn `solo mcp-stdio`. See
    [MCP Integration](./mcp-integration.md) for the config
    files.
  - **Pick a real embedder + LLM.** For semantic recall and
    distilled abstractions you'll want BGE-M3 + an Anthropic
    or OpenAI key. See [Model Selection](./model-selection.md)
    for the trade-offs.
  - **Understand consolidation.** Once you have a few hundred
    memories, the consolidation cycle starts to matter — it
    is what turns "a pile of episodes" into "themes and
    facts." See [Consolidation Cycle](./consolidation-cycle.md)
    for the walkthrough.
