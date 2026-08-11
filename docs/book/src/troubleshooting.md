# Troubleshooting

Start with:

```bash
solo doctor
solo doctor --with-stats
solo doctor --round-trip
```

The first checks local files and lock state. `--with-stats` uses the live
daemon when it owns the database. `--round-trip` verifies the bundled model,
encrypted write path, HNSW index, and recall in a temporary isolated library.

## Installation

### Windows installer

Open a new PowerShell after installation so the updated user `PATH` is loaded.
The official installer does not require Rust, Visual Studio, or administrator
privileges.

### Ubuntu package dependencies

Install the `.deb` with `apt`, not `dpkg -i`, so required desktop and keyring
packages are resolved:

```bash
sudo apt install ./solo-<version>-ubuntu24.04-amd64.deb
```

Ubuntu 24.04 x86-64 is the certified Linux target. Source builds on other
distributions need the normal Rust, compiler, OpenSSL/SQLCipher build, and
native webview dependencies.

## Startup and locking

### `solo.lock already held`

One Solo process owns the Community library. This is normally healthy when the
daemon or Solo Controls is running.

- Use `solo doctor --with-stats` to query the live daemon.
- Stop Solo before running one-shot commands that need direct database access.
- Use another `--data-dir` for an isolated test.
- Do not delete a lock merely because it exists; doctor reports whether its PID
  is live.

### Wrong passphrase

Solo cannot recover the SQLCipher passphrase. Verify that the command is using
the intended data directory and keyring entry. Do not run `solo init --force`:
that wipes Solo-owned data.

### HNSW snapshot missing or stale

The encrypted SQL database is authoritative. Solo can rebuild HNSW snapshots
from stored embeddings. A warning during first startup or after a supervised
embedder migration is expected; persistent drift after a clean restart should
be reported with doctor output and sanitized logs.

## Recall

### Recall returns no useful match

Check:

1. `solo doctor --round-trip` passes.
2. `/v1/status` reports memory recall as `ready` and the bundled MiniLM
   identity/dimension are correct.
3. The memory is active rather than forgotten.
4. Filters, project scope, or source restrictions are not excluding it.

Solo uses hybrid vector and lexical retrieval. A very abstract query can be
weak in MiniLM while still being rescued by lexical ranking. Record failures in
the versioned retrieval corpus before changing the default model.

### After changing embedders

Never mix vector spaces. Use:

```bash
solo migrate-embedder ollama --model nomic-embed-text
```

The migration validates the new model, re-embeds all content, removes stale
vectors, and rebuilds snapshots.

## Capability states and derived memory

The capability panel and `memory_context` distinguish:

- `ready`: available;
- `disabled`: the required Steward is off;
- `pending`: work is scheduled or queued;
- `empty`: the pipeline ran but has no matching output;
- `failed`: the readiness query or latest run failed.

An empty result is not the same as a disabled graph. Read the explanation next
to each state.

### Knowledge extraction is disabled

Open **Settings → Steward LLM** and choose local Ollama, Ollama Cloud,
Anthropic, or OpenAI. Restart Solo after saving. Hosted choices require an
explicit consent checkbox and store only an environment-variable reference.

### Local Ollama is not ready

Confirm Ollama is running and the selected model is installed:

```bash
ollama list
ollama pull qwen3:8b
```

The default base URL is `http://localhost:11434`. Local Ollama needs no API
key. `qwen3:4b` is a lighter fallback for constrained machines.

### Ollama Cloud authentication fails

For direct Cloud API access, set the key in the environment that launches Solo:

```text
OLLAMA_API_KEY=<secret>
```

The config must contain `api_key_env = "OLLAMA_API_KEY"`, not the secret. To
use a signed-in local daemon instead, select Cloud, use the loopback base URL,
choose a `-cloud` model, and leave the key reference blank. Consent is required
in both cases because inference is off device.

### Hosted configuration is refused

Solo will not infer consent from an API key. Review the processing-location
disclosure and explicitly consent in the setup screen. For legacy configs with
no `[llm]` block, the environment-only path requires:

```text
SOLO_HOSTED_PROCESSING_CONSENT=true
```

### Extraction fails with invalid JSON

Local Ollama uses native JSON mode. Ollama Cloud does not currently support
that switch, so Solo validates output and makes one repair attempt. If the
model still returns invalid JSON, the extraction is marked failed and the raw
memory remains intact. Try a stronger model or inspect the sanitized error.

### Backfill is pending or failed

The backfill performs clustering first, then bounded extraction batches. A
single job can run at a time. Check its phase, remaining cluster count, and
last error in Settings or `GET /v1/status`. Correct the provider/model problem
and start backfill again; successful existing derived rows are preserved.

Some memories legitimately remain unclustered until enough related material
exists. After a successful clustering pass, partial episode coverage is not by
itself a failure.

## Daemon and HTTP

### Port already in use

Use a different local port or stop the conflicting process. Keep Web/Desktop
and doctor `--daemon-url` pointed at the same port.

### Non-loopback bind rejected

Solo requires bearer authentication for LAN-facing HTTP. Use
`solo http-serve --bind <ip> --bearer-token-file <path>` and protect the
network path with an appropriate firewall or reverse proxy.

## MCP

### Client shows Solo as disconnected

Run `solo mcp-stdio` manually to expose startup errors, confirm the configured
command is the installed `solo` executable, and verify the client launches it
with the correct environment/data directory. Do not start a second writer for
the same library unless using the documented proxy architecture.

### Tools exist but derived calls are empty

Use `memory_context` and inspect its section states. Raw remember/recall can be
healthy while facts/entities/graph are intentionally disabled or still
pending extraction.

## Filing an issue

Include:

- `solo --version`;
- operating system and install method;
- `solo doctor` and `solo doctor --with-stats` output;
- the relevant capability states and explanations;
- sanitized logs with secrets and memory content removed;
- exact reproduction steps.

Never attach `solo.db`, a passphrase, bearer token, API key, or raw private
memory content to a public issue.
