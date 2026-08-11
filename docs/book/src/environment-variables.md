# Environment Variables

Solo reads the following environment variables at startup. All are
optional — sensible defaults exist for everything except the
two API key vars (which gate optional features) and
`SOLO_PASSPHRASE` (which enables non-interactive runs).

| variable | default | meaning |
|---|---|---|
| `SOLO_DATA_DIR` | `~/.solo` (Unix) or `%USERPROFILE%\.solo` (Windows) | Path to the data directory. Equivalent to `--data-dir`. |
| `SOLO_PASSPHRASE` | _none_ | Passphrase for the SQLCipher key derivation. If unset, Solo prompts on stdin. **Setting this in env makes it visible to other processes via `/proc` on Linux** — Solo prints a stderr warning when it reads from this var, then removes it from its own environ block (the kernel-side copy survives until the parent process exits). |
| `SOLO_EMBEDDER` | _unset_ | Runtime embedder override. Set to `bundled` or `ollama`. When unset, Solo uses the persisted `[embedder]` identity from `solo.config.toml`. |
| `SOLO_OLLAMA_BASE_URL` | `http://localhost:11434` | Base URL for Ollama embedding requests. Used when the active embedder is `ollama:*` or `SOLO_EMBEDDER=ollama`. |
| `SOLO_OLLAMA_EMBED_MODEL` | `nomic-embed-text` | Ollama embedding model used by `SOLO_EMBEDDER=ollama` and init-time probing. Persisted migrated data dirs store the chosen model in `[embedder].name` as `ollama:<model>`. |
| `SOLO_OLLAMA_KEEP_ALIVE` | `30s` | Request-level Ollama `keep_alive` value for Solo embedding calls. Set `5m` to restore Ollama's normal default, or `0` to ask Ollama to unload after each request. |
| `SOLO_OLLAMA_LLM_KEEP_ALIVE` | `30s` | Request-level Ollama `keep_alive` value for Steward chat calls. Falls back to `SOLO_OLLAMA_KEEP_ALIVE` when unset. |
| `OLLAMA_API_KEY` | _none_ | Bearer token for direct Ollama Cloud Steward calls. The config stores this variable name, not the key value. |
| `SOLO_HOSTED_PROCESSING_CONSENT` | _unset_ | Set to `true` only to acknowledge off-device processing for the deprecated environment-only hosted-provider path. The Web wizard records consent in `[llm]` instead. |
| `OLLAMA_KEEP_ALIVE` | _unset_ | Ollama server default keep-alive. When Solo Tray auto-starts local `ollama serve` and this variable is unset, it starts Ollama with `30s` to avoid leaving large models resident after idle Solo work. |
| `SOLO_REFUSE_STUB_EMBEDDER` | _unset_ | When set (any value), `solo consolidate` and the daemon's consolidate timer **refuse to run** if the active embedder is the 32-dim BLAKE3 stub. Without it, Solo emits a `tracing::error!` per consolidate run when the stub is active but still proceeds. Recommended for production deployments. v0.11.2+. |
| `SOLO_NO_LOCKFILE` | _unset_ | Proxy-friendly mode for `solo mcp-stdio` only. When set (any value), skips `solo.lock` acquisition so a gateway (Cloudflare Access, Pomerium, etc.) can spawn multiple ephemeral `mcp-stdio` subprocesses against one shared data dir. **Dangerous**: breaks the writer-actor single-process invariant. See [MCP Integration § Gateway / proxy mode](./mcp-integration.md#gateway--proxy-mode-no-lockfile) for safety guidance. Equivalent to `--no-lockfile` on the CLI. v0.11.5+. |
| `ANTHROPIC_API_KEY` | _none_ | Anthropic Claude API key. Persisted `[llm]` config stores this variable name, never the value. |
| `ANTHROPIC_MODEL` | `claude-sonnet-4-6` | Legacy environment-only Anthropic model override. |
| `OPENAI_API_KEY` | _none_ | OpenAI API key. Persisted `[llm]` config stores this variable name, never the value. |
| `OPENAI_MODEL` | `gpt-5.6-terra` | Legacy environment-only OpenAI/OpenAI-compatible model override. |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | Legacy OpenAI-compatible base URL override. Persisted Ollama settings use their native `base_url` field. |
| `RUST_LOG` | _none_ (defaults to WARN) | Standard `tracing-subscriber` filter. Examples: `RUST_LOG=info`, `RUST_LOG=solo_storage=debug,info`. |

## Precedence rules

  - **CLI flag > env var.** When a flag accepts an env-var
    fallback (e.g. `--data-dir` with `SOLO_DATA_DIR`), the
    flag wins. Useful for overriding the env in a single
    command without unsetting it.
  - **Persisted `[llm]` > legacy provider env detection.** Fresh installs
    write `mode = "none"`, so an inherited API key never turns hosted memory
    processing on. Only old configs without an `[llm]` block use the legacy
    Anthropic-then-OpenAI fallback, and that path also requires
    `SOLO_HOSTED_PROCESSING_CONSENT=true`.

## Where to set them

  - **Per-command shell.** `SOLO_PASSPHRASE=... solo init`
    works — env stays set only for that one process.
  - **Shell profile** (`~/.bashrc`, `~/.zshrc`,
    PowerShell `$PROFILE`). Survives across terminals.
  - **MCP host config file** (Claude Desktop, Cursor) —
    set in the `env` block of the `mcpServers` entry.
    `SOLO_PASSPHRASE` lives in plaintext in that JSON; see
    MCP Integration for the trade-off.
  - **systemd unit** (Linux daemons) — `Environment=...`
    directives in the unit file.
  - **Windows Service** — environment is configured at the
    service-creation step.

## Caveats

  - **Env-var passphrase is not as private as a prompt.**
    Other local users with `/proc` access (Linux) or
    Process Explorer (Windows) can read it. For a personal
    dev workstation, prompt-then-cache via your shell
    keychain integration is safer.
  - **Empty values are treated as unset for `SOLO_PASSPHRASE`.**
    `SOLO_PASSPHRASE=` errors out at startup instead of
    silently using an empty passphrase (which would
    derive a known-weak key).
  - **`RUST_LOG` is wide.** Setting `RUST_LOG=trace` will
    flood stdout with internal protocol messages. Prefer
    targeted filters like `RUST_LOG=solo_steward=debug`
    for diagnosing a specific subsystem.
