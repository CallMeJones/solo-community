# Configuration File

`solo init` writes `solo.config.toml` in the Solo data directory. The file is
plaintext because Solo must read the Argon2 salt and embedder identity before
it can open the encrypted SQLCipher database.

Never place passphrases or API-key values in this file.

## Typical Community configuration

Official Windows and Linux packages initialize the bundled MiniLM embedder and
leave knowledge extraction disabled until the user chooses a Steward model:

```toml
schema_version = 1
salt_hex = "c1bd1d1985c6a5fe3a0c4b8d6a18f9e2"

[embedder]
name = "bundled:all-MiniLM-L6-v2"
version = "v2"
dim = 384
dtype = "f32"

[llm]
mode = "none"
```

The exact file can contain additional default blocks written by the installed
version. Preserve unknown fields when editing by hand.

## Top-level fields

| Field | Meaning |
|---|---|
| `schema_version` | Version of the config-file schema, not the database schema. |
| `salt_hex` | Stable Argon2 salt used with the user's passphrase. Changing it makes the existing database unreadable. |
| `embedder` | Persisted vector-model identity. Changing it requires a supervised re-embed. |
| `llm` | Optional Steward provider selection and privacy consent. |
| `triples` | Background clustering/extraction cadence and batch controls. |
| `steward` | Clustering threshold and minimum cluster-size overrides. |
| `documents` | Document chunking, retention, and extension settings. |
| `workspace_file_access` | Optional allow-list for daemon-side file ingestion. |
| `auth`, `audit`, `redaction` | HTTP access and local governance controls. |

## `[embedder]`

The identity is tied to vectors stored in SQL and HNSW. Official packages use:

```toml
[embedder]
name = "bundled:all-MiniLM-L6-v2"
version = "v2"
dim = 384
dtype = "f32"
```

To switch a populated library to an Ollama embedding model, use the supervised
migration instead of editing these fields:

```bash
ollama pull nomic-embed-text
solo migrate-embedder ollama --model nomic-embed-text
```

Solo validates the endpoint, backs up config and HNSW snapshots, rewrites all
vectors, removes stale vector rows, and rebuilds the index from SQL.

## `[llm]`

The Steward is independent from the embedder. MiniLM powers recall; a
generative Steward model creates the advanced knowledge layer.

Disabled:

```toml
[llm]
mode = "none"
```

Local Ollama:

```toml
[llm]
mode = "ollama"
endpoint = "local"
base_url = "http://localhost:11434"
model = "qwen3:8b"
hosted_processing_consent = false
```

Direct Ollama Cloud:

```toml
[llm]
mode = "ollama"
endpoint = "cloud"
base_url = "https://ollama.com"
model = "gpt-oss:120b-cloud"
api_key_env = "OLLAMA_API_KEY"
hosted_processing_consent = true
```

Ollama Cloud through a signed-in local daemon uses `endpoint = "cloud"`, a
loopback `base_url`, a `-cloud` model, and can omit `api_key_env`. Although the
HTTP hop is local, model processing is off device and consent is still
required.

Hosted Anthropic or OpenAI:

```toml
[llm]
mode = "anthropic" # or "openai"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-sonnet-4-6"
hosted_processing_consent = true
```

`api_key_env` is the name of an environment variable, not the key itself.
Solo refuses hosted processing until `hosted_processing_consent = true` has
been recorded through an explicit user choice.

Use **Solo Web → Settings → Steward LLM** when possible. The setup flow explains
the processing location, validates consent, and offers an immediate backfill.

## `[triples]` and `[steward]`

These blocks control when derived work runs and how clustering is formed. The
Web settings screen is the supported editing surface. Set an interval or count
to zero only when intentionally disabling that trigger.

```toml
[triples]
trigger_interval_secs = 3600
trigger_episode_count = 50
consolidate_interval_secs = 3600
cluster_timeout_secs = 60

[steward]
cluster_min_size = 2
cluster_cosine_threshold = 0.55
```

After changing runtime provider or cadence settings, restart Solo so the daemon
loads the new configuration.

## `[workspace_file_access]`

Restrict daemon-side document/file ingestion to known roots:

```toml
[workspace_file_access]
allowed_roots = ["C:\\Users\\Example\\Projects"]
```

On Linux:

```toml
[workspace_file_access]
allowed_roots = ["/home/example/projects"]
```

An explicit empty list disables daemon-side file reads. If the field is absent,
Solo retains the historical unrestricted loopback behavior.

## What is not stored

- The SQLCipher passphrase or derived key.
- LLM API-key values. LLM configuration stores only environment-variable
  names. A configured HTTP bearer token is a separate local access-control
  setting and makes `solo.config.toml` sensitive; prefer a protected token file
  when exposing HTTP beyond loopback.
- Hosted-provider consent inferred from an inherited API key. Fresh installs
  always start with knowledge extraction disabled.

## Backup

To restore a Community library, retain:

- `solo.db`;
- `solo.config.toml`;
- optionally the HNSW snapshots (otherwise Solo rebuilds them from SQL);
- the passphrase in a password manager, never in the data directory.

Do not back up a live `solo.lock`; it is runtime-only.
