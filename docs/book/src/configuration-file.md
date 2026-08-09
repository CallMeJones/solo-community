# Configuration File

`solo init` writes one configuration file at the root of the
data directory: `solo.config.toml`. It's plaintext on
purpose — Solo needs to read the Argon2 salt and the
embedder identity **before** opening the encrypted database
to derive the key.

```toml
schema_version = 1
salt_hex = "c1bd1d1985c6a5fe3a0c4b8d6a18f9e2"

[embedder]
name    = "BAAI/bge-m3"
version = "v1"
dim     = 1024
dtype   = "f32"
```

## Top-level fields

| field | type | meaning |
|---|---|---|
| `schema_version` | u32 | Version of the config-file schema itself, **not** the database schema. Bumping this lets future Solo versions migrate old config files in-place. Currently always `1`. |
| `salt_hex` | string | 32-character lowercase hex string of the 16-byte Argon2 salt used to derive the SQLCipher key from your passphrase. Generated fresh on first `solo init`; stable forever after. **Do not edit** — changing it breaks the existing database. |
| `embedder` | table | Embedder identity persisted on first init. Solo refuses to start if the active embedder doesn't match (see below). |
| `workspace_file_access` | table | Optional allow-list for daemon-side document/file ingestion. Omit it for legacy unrestricted local behavior. |

## `[embedder]` fields

| field | type | meaning |
|---|---|---|
| `name` | string | Embedder name. Default `"BAAI/bge-m3"` (the name is used regardless of whether you actually have BGE-M3 weights — StubEmbedder uses the same identity for compatibility). |
| `version` | string | Embedder version. Default `"v1"`. |
| `dim` | u32 | Embedding dimension. Default `1024`. |
| `dtype` | string | Element type. One of `"f32"` (default), `"f16"`, `"i8"`, `"binary"`. v0.3.x writes only `"f32"`; the others are reserved for future quantised embedders. |

The embedder identity is what makes embedder migration safe.
When you switch from StubEmbedder to BGE-M3, the persisted
identity stays the same (both write `BAAI/bge-m3 v1`),
but the **vector space** changes. Solo emits a warning at
startup when it detects a likely mode switch against a
non-empty database.

If you genuinely want to switch to a different embedder,
use the supervised migration path instead of editing this
file directly:

```bash
solo migrate-embedder ollama --model nomic-embed-text
```

That command validates Ollama, backs up config and HNSW
snapshots, writes the new embedder identity, re-embeds the Memory
Library, garbage-collects stale embedding rows, and
deletes stale HNSW snapshots so the next daemon start rebuilds
from SQL embeddings.

Editing `solo.config.toml`'s embedder fields directly is
**not** supported and will likely produce incoherent
recall.

## `[workspace_file_access]` fields

This optional block constrains file-reading import paths served by the
daemon. It applies to HTTP document ingest/import and MCP
`memory_ingest_document` before Solo opens the requested file.

```toml
[workspace_file_access]
allowed_roots = ["C:\\Users\\Example\\Projects\\solo-community"]
```

When `allowed_roots` is absent, Solo keeps the historical unrestricted
loopback behavior. When it is present, every requested file or directory
must live under one of those roots. An explicit empty list disables
daemon-side file ingestion:

```toml
[workspace_file_access]
allowed_roots = []
```

For one run, `SOLO_WORKSPACE_FILE_ROOTS` can override the config value.
It uses the operating system path-list separator, so `;` on Windows and
`:` on macOS/Linux.

## What's deliberately NOT stored

  - **Passphrase / key.** Never. The config file is
    plaintext; storing the key (or any plaintext-derivable
    form of it) would defeat the SQLCipher encryption. The
    Argon2 salt + your passphrase + the Argon2 cost
    parameters together are enough to re-derive the key on
    every startup.
  - **API keys, model names, base URLs.** All LLM /
    embedder runtime config is via env vars, not the file.
    The persisted file describes only **what's true about
    the data on disk**, not how a future run should
    interpret it.
  - **CLI flag defaults, daemon intervals.** Per-run /
    per-deployment configuration; not persisted.

## Backup considerations

To restore a Solo data dir on a new machine, you need:

  - `solo.db` (the encrypted database).
  - `solo.config.toml` (the salt + embedder identity).
  - Optionally the HNSW snapshot files (`hnsw_episodes*` —
    Solo will rebuild from `embeddings` if absent, just
    slower on first startup).
  - Your passphrase (in your head or your password
    manager — **not** in the data dir).

`solo.config.toml` is small (~150 bytes) — back it up with
the database, or your restore won't be able to derive the
key even with the right passphrase.

`solo.lock` is runtime-only and should NOT be in your
backup; it'll prevent startup if it lingers from a previous
machine.
