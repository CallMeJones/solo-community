# Solo

[![Latest release](https://img.shields.io/github/v/release/CallMeJones/solo-community?logo=github&label=release)](https://github.com/CallMeJones/solo-community/releases/latest)
[![Crates.io](https://img.shields.io/crates/v/solo-cli?logo=rust&label=solo-cli)](https://crates.io/crates/solo-cli)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

Local-first personal memory for AI assistants. Solo keeps canonical memory in
an encrypted SQLite database on the user's machine, with local indexes and
retained application files beside it. It runs as an MCP server so Claude
Desktop, Cursor, Codex, and other MCP-aware clients can read and write the same
user-owned memory.

## Status

**v0.12.0 Community candidate** - Solo is now a full local memory stack:
an encrypted local Memory Library, a single-writer actor for durable
writes, HNSW/vector recall, document/RAG memory, the embedded Solo Web UI,
and MCP over stdio plus Streamable HTTP on `/mcp`.
This release adds resumable document uploads, retained source assets,
native document extraction, temporal relationship paths, provenance
explanations, and stricter memory-claim quality review. The Community product
ships Core + Solo Desktop. See the
[v0.12.0 release notes](docs/releases/v0.12.0.md).

```
remember/update -> embedding row + pending_index -> writer commit
        |
        | (daemon with --consolidate-interval-secs, or solo consolidate)
        v
cluster (deterministic)              -> clusters, cluster_episodes
        v
merge + absorb + survivor refresh    -> drift-resistant centroids
        v
abstract (LLM)                       -> semantic_abstractions, triples
        v
contradiction detect + lifecycle     -> contradictions, resolutions
        v
context/entity/doc/graph surfaces    -> MCP, HTTP, Solo Desktop
```

CI mirrors the publish gate: Rust workspace tests, the complete Web
typecheck/lint/unit/browser suite, the bundled embedder test pass, clippy,
mdBook, and an Ollama smoke. ADR-driven
design; the writer model contract is in
[`docs/adr/0003-writer-model.md`](docs/adr/0003-writer-model.md).

## Repository layout

- `crates/` contains Community Core, storage, API, CLI, and tray code.
- `apps/web/` contains the React/Vite Community UI embedded by `solo-api`.
- `crates/solo-api/assets/solo-web/` retains the verified production Web
  artifact so Rust users do not need Node.js merely to build Solo.
- `scripts/sync_solo_web_assets.ps1` rebuilds `apps/web` and atomically
  refreshes the embedded artifact plus its provenance.

For Web development, run `npm ci` and `npm run dev` from `apps/web`. A
release change to Web source must refresh the embedded artifact before merge;
CI rebuilds both and rejects any digest or source-commit mismatch.

Public Community releases must ship the supported OS set together: Windows
installer, Windows portable ZIP, Ubuntu 24.04 `.deb`, and checksums. See
[`docs/release-policy.md`](docs/release-policy.md).

## Install

### Windows installer (recommended for Windows)

Download `SoloSetup-<version>-x86_64.exe` from the
[latest GitHub release](https://github.com/CallMeJones/solo-community/releases/latest)
and run it.

The installer:

- installs `solo.exe` to `%LOCALAPPDATA%\Programs\Solo`;
- adds that folder to your user `PATH`;
- creates a Start Menu shortcut for a Solo PowerShell;
- includes a local Windows readme with MCP + Ollama next steps;
- does not require administrator privileges;
- does not require Rust, Cargo, `cargo-binstall`, `link.exe`, Visual
  Studio, Visual Studio Build Tools, or a separate Visual C++
  Redistributable install because the release bundles the needed
  app-local MSVC runtime DLLs.

Open a new PowerShell after install and run:

```powershell
solo --help
solo init
solo doctor
solo daemon --http-port 17821
```

The installed `solo.exe` can be used directly as the MCP memory server:

```powershell
solo mcp-stdio
```

Point Claude Desktop, Cursor, or another MCP host at command `solo` with
args `["mcp-stdio"]`. To let Solo's steward/consolidation work use a
local model, install Ollama separately, pull a model, then start Solo
with `--ollama-model`, for example:

```powershell
ollama pull qwen3:8b
solo daemon --http-port 17821 --consolidate-interval-secs 3600 --ollama-model qwen3:8b
```

### Ubuntu 24.04 installer (recommended for Linux Desktop)

Download `solo-<version>-ubuntu24.04-amd64.deb` from the
[latest GitHub release](https://github.com/CallMeJones/solo-community/releases/latest),
then install it with:

```bash
sudo apt install ./solo-<version>-ubuntu24.04-amd64.deb
```

The package includes `solo`, `solo-tray`, the pinned offline semantic model,
an application-menu entry, the embedded Solo Desktop, XDG login autostart
support, and Secret Service keyring integration. It targets Ubuntu 24.04 on
x86-64 and is the certified Linux installation path.

### Pre-built binary via Cargo

```bash
cargo binstall solo-cli
```

Pre-built ZIP/TGZ binaries are attached to every GitHub release from
v0.3.5 onward for terminal-first users and `cargo binstall`:

  - `x86_64-unknown-linux-gnu`
  - `aarch64-unknown-linux-gnu`
  - `x86_64-pc-windows-msvc`

The release ZIP/TGZ archives include `models/all-MiniLM-L6-v2`; keep that
directory beside `solo` when extracting an archive to preserve fully offline
semantic memory. `cargo binstall` installs only the executable, so its first
semantic operation may populate fastembed's model cache from the network.
Use the Windows setup EXE or Ubuntu DEB when offline-first installation is
required. Alpine/musl is not an official binary target because ONNX Runtime
does not provide a supported musl build; source builds can opt out of the
bundled embedder and configure Ollama explicitly.

`cargo binstall` is a separate tool, NOT bundled with cargo.
Bootstrap once with the [official installer](https://github.com/cargo-bins/cargo-binstall#installation)
if you don't already have it. On Windows, prefer that official
prebuilt bootstrap or the Solo setup EXE above; `cargo install
cargo-binstall` compiles `cargo-binstall` from source and therefore
needs the MSVC linker (`link.exe`) from Visual Studio Build Tools.

### From crates.io (source compile)

```bash
cargo install solo-cli
```

Falls back to building from source — needs Perl in PATH for
`openssl-sys`'s vendored OpenSSL. On Windows it also needs Visual
Studio Build Tools because Rust's MSVC target links with `link.exe`.
[Strawberry Perl](https://strawberryperl.com/) is the easiest Perl, and
Git Bash needs the prefix because msys's bundled Perl bombs OpenSSL's
`Configure`:

```bash
PATH="/c/Strawberry/perl/bin:/c/Strawberry/c/bin:$PATH" cargo install solo-cli
```

### From source (git clone)

```bash
git clone https://github.com/CallMeJones/solo-community
cd solo-community
cargo build --release -p solo-cli
# Binary: target/release/solo  (Windows: target\release\solo.exe)
```

Build prerequisites:

- Rust 1.88 or newer.
- Windows: Visual Studio 2022 Build Tools with the "Desktop development
  with C++" workload.
- Perl in `PATH` for vendored OpenSSL. On Windows,
  [Strawberry Perl](https://strawberryperl.com/) is the easiest path.

The `-p solo-cli` flag builds the user-facing `solo` binary without
building every workspace crate. If you set `CARGO_TARGET_DIR`, Cargo writes
the binary under that directory instead of `target/`.

On Windows from PowerShell, after installing Strawberry Perl:

```powershell
$env:PATH = "C:\Strawberry\perl\bin;C:\Strawberry\c\bin;$env:PATH"
cargo build --release -p solo-cli
.\target\release\solo.exe --help
```

On Windows from Git Bash:

```bash
PATH="/c/Strawberry/perl/bin:/c/Strawberry/c/bin:$PATH" cargo build --release -p solo-cli
./target/release/solo.exe --help
```

## Usage

Solo is a command-line app. Double-clicking `solo.exe` may look like
"nothing happened" because Windows opens a console window, prints the
command list, and closes it immediately. Open PowerShell in the binary
folder and run commands explicitly:

```powershell
.\solo.exe --help
.\solo.exe init
.\solo.exe doctor
.\solo.exe daemon --http-port 17821
```

### Setup

```bash
# Create ~/.solo/{solo.db,solo.config.toml} encrypted with your passphrase.
solo init
```

### One-shot operations

```bash
# Store a memory
solo remember "Sam moved to Berlin in May 2025"

# Vector search
solo recall "where does Sam live"

# Inspect a specific memory by id
solo inspect <memory-id>

# Soft-delete a memory
solo forget <memory-id>

# Run the consolidation pass once (clustering + abstraction + contradictions)
solo consolidate

# Re-embed every stored memory under the current embedder model
# (e.g. after switching from stub to Ollama)
solo reembed --gc
```

### Long-running daemon

```bash
# Plain daemon (writer + reader pool stays warm; no transports)
solo daemon

# With HTTP/JSON on 127.0.0.1
solo daemon --http-port 17821

# With periodic consolidation (every hour)
solo daemon --consolidate-interval-secs 3600

# With a real LLM backend for abstraction + contradiction
ANTHROPIC_API_KEY=sk-... solo daemon --consolidate-interval-secs 3600
```

### MCP (Claude Desktop, Cursor, …)

Add to `~/Library/Application Support/Claude/claude_desktop_config.json`
(macOS) or `%APPDATA%\Claude\claude_desktop_config.json` (Windows):

```json
{
  "mcpServers": {
    "solo": {
      "command": "solo",
      "args": ["mcp-stdio"],
      "env": { "SOLO_PASSPHRASE": "..." }
    }
  }
}
```

Solo exposes the memory surface under the `memory_*` namespace:

| Tool | Surface | Purpose |
|---|---|---|
| `memory_remember` | Episode | Store one new episode. |
| `memory_remember_batch` | Episode | Store many episodes in one transaction. |
| `memory_recall` | Episode | Vector search over active memories. |
| `memory_context` | Context | Return a bounded recall, themes, facts, contradictions, and section health bundle for an agent turn. |
| `memory_update` | Correction | Rewrite an active memory through the writer and re-embed it. |
| `memory_forget` | Episode | Soft-delete one memory. |
| `memory_inspect` | Episode | Full record by `MemoryId`. |
| `memory_themes` | Derived | List recent cluster abstractions. |
| `memory_entities` | Derived | Discover structured-graph entity ids for a query. |
| `memory_facts_about` | Derived | Query the SPO knowledge graph by subject and optional filters. |
| `memory_contradictions` | Derived | List flagged disagreements and lifecycle fields. |
| `memory_contradiction_resolve` | Derived | Resolve or reopen a contradiction with provenance. |
| `memory_inspect_cluster` | Derived | Drill into one cluster's abstraction and source episodes. |
| `memory_ingest_document` | Documents | Ingest and chunk a local document. |
| `memory_search_docs` | Documents | Search document chunks. |
| `memory_inspect_document` | Documents | Inspect document metadata and chunk previews. |
| `memory_list_documents` | Documents | Page through ingested documents. |
| `memory_forget_document` | Documents | Soft-delete an ingested document. |

The derived tools surface `semantic_abstractions`, `triples`, and
`contradictions` rows that the Steward writes during `solo
consolidate` cycles. Without an LLM backend wired (no
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `--ollama-model`), those
tables stay empty and the derived tools return empty arrays. See
the [Derived Layer chapter](docs/book/src/derived-layer.md) for
detail.

#### Tested MCP clients

Solo's MCP tools are tested against the following clients:

- **Claude Desktop** (Anthropic) — full support
- **Cursor** — full support
- **Claude Code** (Anthropic CLI) — full support

Tool names are validated against the function-calling name regex
of all three major LLM providers (Anthropic, OpenAI, Gemini) so
the same tool surface works across clients. Run `solo doctor
--check-mcp-compat` for a runtime per-provider compatibility
report (exits non-zero if any tool would fail — useful as a CI
guard for future tool additions).

Solo targets [MCP spec version `2024-11-05`](https://spec.modelcontextprotocol.io/specification/2024-11-05/)
over stdio and Streamable HTTP (`POST /mcp` plus resumable
`GET /mcp` SSE sessions).

### HTTP / JSON

Core HTTP routes include:

- Liveness/spec: `GET /health`, `GET /openapi.json`, `GET /v1/status`.
- Episodes: `POST /memory`, `POST /memory/search`,
  `POST /memory/context`, `PATCH /memory/{id}`,
  `GET /memory/{id}`, `DELETE /memory/{id}`,
  `POST /memory/consolidate`.
- Derived memory: `GET /memory/themes`, `GET /memory/facts_about`,
  `GET /memory/entities`, `GET /memory/contradictions`,
  `POST /memory/contradictions/resolve`.
- Graph for `solo-web`: `GET /v1/graph/expand`,
  `GET /v1/graph/nodes`, `GET /v1/graph/edges`,
  `GET /v1/graph/inspect/{id}`, `GET /v1/graph/neighbors/{id}`,
  `GET /v1/graph/stream`.
- MCP Streamable HTTP: `POST /mcp`, `GET /mcp`.
- Operations: `POST /backup`, `GET|POST /v1/steward/backfill`.

The localhost CORS policy covers the browser UI write path as well as reads,
including `PATCH /memory/{id}` for solo-web memory corrections.

```bash
solo http-serve --bind 0.0.0.0 --bearer-token-file /etc/solo/token
```

## Embeddings

**Default**: official Windows and Linux packages bundle
`all-MiniLM-L6-v2` (384 dimensions). Semantic recall works locally without
Ollama, an API key, or a first-use model download.

- **`StubEmbedder`** (tests/development only): deterministic BLAKE3
  hash → unit-norm f32 vectors. No model download. Useful for offline
  development; vectors have identity-only meaning, not semantic.
- **`OllamaEmbedder`** (optional): alternate semantic embeddings via Ollama
  daemon. `ollama pull nomic-embed-text`, then set
  `SOLO_EMBEDDER=ollama` (optionally `SOLO_OLLAMA_EMBED_MODEL=<model>`
  and `SOLO_OLLAMA_BASE_URL=<url>`).

Switching embedders on a populated DB requires `solo reembed --gc` to
regenerate every stored vector under the new model.

**v0.5.x → v0.6.0 migration**: BGE-M3 was supported in v0.5.x; removed
in v0.6.0. Operators running BGE-M3 must migrate to Ollama before
upgrading — see [Migrating from v0.5.x to v0.6.0](#migrating-from-v05x-to-v060)
below.

## LLM (consolidation)

The consolidation pass runs the SWS-equivalent clustering pass
without an LLM (cheap, deterministic). The REM-equivalent
abstraction pass and contradiction detection require an
`LlmClient`. Solo has native Ollama, Anthropic, and OpenAI clients.
Local Ollama is the recommended privacy-first path; direct Ollama Cloud and
hosted Anthropic/OpenAI are also supported with explicit consent.

### Ollama Steward: local or cloud

[Ollama](https://ollama.com) can run the Steward locally or provide cloud
models. In Solo Web, open **Settings → Steward LLM → Ollama** and choose:

- **Local** — the model and memory content stay on this device. No API key.
- **Cloud** — selected memory content is processed by Ollama Cloud. Solo
  requires explicit consent and stores only an environment-variable reference
  such as `OLLAMA_API_KEY`, never the key itself.
- **Custom** — an operator-controlled Ollama endpoint. Treat a non-loopback
  endpoint as off-device processing, use HTTPS outside the local machine, and
  review its logging/retention policy.

```bash
# 1. Install Ollama (Linux/macOS one-liner; Windows installer at ollama.com)
curl -fsSL https://ollama.com/install.sh | sh

# 2. Pull a model that fits your GPU (see hardware tiers below)
ollama pull qwen3:8b

# 3. Point Solo at it (one-flag shorthand)
solo consolidate --ollama-model qwen3:8b
# Or for the daemon:
solo daemon --consolidate-interval-secs 3600 --ollama-model qwen3:8b
```

For normal installations, prefer the Web setup wizard. It writes an explicit
`[llm]` block with `endpoint = "local"`, `"cloud"`, or `"custom"`. MiniLM is
only the recall encoder; a generative model such as Qwen3 is what creates
themes, facts, entities, relationships, and contradictions. After setup,
choose **Backfill existing memories now** to see progress immediately.

Hosted Anthropic, OpenAI, Ollama Cloud, and non-loopback custom endpoints are
refused until the user explicitly consents to off-device memory processing.
API keys remain in environment variables; Solo stores only the variable name.

For a temporary local-only CLI run, `--ollama-model <MODEL>` remains a
compatibility shorthand. Persisted local and cloud configuration belongs in
`solo.config.toml` and is easiest to manage through Solo Web.

Start with `qwen3:8b` when the machine can run it. `qwen3:4b` is the lighter
fallback. Larger local or hosted models can improve ambiguous entity linking
and contradiction judgments, but model changes should be evaluated against
the versioned retrieval and derivation corpus rather than assumed to help.

Ollama Cloud can be reached in either supported form:

- Directly at `https://ollama.com/api` using `endpoint = "cloud"` and a bearer
  token held in `OLLAMA_API_KEY`.
- Through a signed-in local Ollama daemon using `endpoint = "local"` and a
  `-cloud` model tag. The
  request still leaves the device, so Solo requires hosted-processing consent
  even though the API connection itself is loopback.

Ollama Cloud currently lacks the native structured-output switch available in
local Ollama. Solo therefore requests JSON in the prompt, validates the reply,
and performs one bounded repair attempt before reporting a failed extraction.

### Hosted LLM (Anthropic or OpenAI)

If you'd rather pay per call than run a model locally:

**Anthropic Claude:**

```bash
export ANTHROPIC_API_KEY=sk-...
export SOLO_HOSTED_PROCESSING_CONSENT=true
export ANTHROPIC_MODEL=claude-sonnet-4-6            # optional override
solo consolidate
```

**OpenAI:**

```bash
export OPENAI_API_KEY=sk-...
export SOLO_HOSTED_PROCESSING_CONSENT=true
export OPENAI_MODEL=gpt-5.6-terra                   # optional override
solo consolidate
```

Other OpenAI-compatible HTTP backends (LM Studio, Together, Groq,
Mistral, DeepInfra) work the same way — just override
`OPENAI_BASE_URL` to their `/v1` endpoint.

### Precedence + no-LLM fallback

Fresh installations persist `[llm] mode = "none"`; inherited API keys never
opt memory into hosted processing. Older configurations without an `[llm]`
block retain the legacy environment fallback, but hosted use also requires
`SOLO_HOSTED_PROCESSING_CONSENT=true`.

Without a Steward model, consolidation still performs deterministic
clustering. Abstractions, facts, entities, relationships, and contradiction
detection remain visibly disabled until the user enables knowledge extraction.

## Architecture

  - **`solo-core`** — traits + types. `Embedder`, `VectorIndex`,
    `LlmClient`, `Episode`, `Cluster`, `SemanticAbstraction`,
    `Triple`, `Contradiction`, ...
  - **`solo-storage`** — SQLCipher (encrypted SQLite) + HNSW
    sidecar + writer actor + reader pool + startup chain +
    embedder impls (Stub, Ollama) + LLM impls (Anthropic,
    OpenAI-compatible/Ollama).
  - **`solo-query`** — recall, inspect, context, correction, entity,
    contradiction, and document query/update pipelines (one source
    of truth across CLI, MCP, HTTP).
  - **`solo-steward`** — clustering algorithm + abstraction prompt
    + contradiction detection (rule filter + LLM judge).
  - **`solo-api`** — MCP stdio + HTTP/JSON transports.
  - **`solo-cli`** — subcommand dispatch + `solo` binary.

Single-writer actor on a dedicated OS thread; SQLite is
fundamentally single-writer in WAL mode and our concurrency model
makes that explicit. Reader pool via `deadpool-sqlite` for
parallel reads. See [`docs/adr/0003-writer-model.md`](docs/adr/0003-writer-model.md)
for the full design contract.

## Security posture (v0.12.0 Community)

Threat model: **single-user, local machine, default-loopback**.

  - Database encrypted at rest via SQLCipher (Argon2id-derived
    key from a user passphrase).
  - HTTP transport defaults to `127.0.0.1`. Binding to any other
    address requires `--bind <ip> --bearer-token-file <path>`.
  - MCP stdio runs as a child process spawned by the client.
    Streamable HTTP on `/mcp` uses the same authenticated boundary
    as the graph/API surface.
  - Lockfile + PID-alive recovery prevents concurrent writers.

What's **not** in Core's v0.12.0 threat model: directly exposing the Core HTTP
service to the public internet, hostile multi-user hosts, hardware attestation,
or side-channel resistance. Review [`SECURITY.md`](SECURITY.md) before handling
real user data.

## Migrating from v0.5.x to v0.6.0

The BGE-M3 embedder path is removed in v0.6.0. To upgrade:

1. Install Ollama: `ollama serve`
2. Pull an embedding model: `ollama pull nomic-embed-text`
3. Set env vars in your shell or `solo.config.toml`:
   `SOLO_EMBEDDER=ollama`, optionally
   `SOLO_OLLAMA_EMBED_MODEL=nomic-embed-text` and
   `SOLO_OLLAMA_BASE_URL=<url>` if Ollama runs somewhere other than
   `http://localhost:11434`.
4. Run `solo reembed` to re-embed your existing data with the new
   embedder (the persisted `embedder_id` changes).
5. Then upgrade to v0.6.0.

After upgrading, `SOLO_BGE_M3_DIR` is ignored (with a one-time stderr
warning); `SOLO_EMBEDDER=bge-m3` errors with a migration message
pointing at this section.

## License

Solo is licensed under [Apache-2.0](LICENSE). The "Solo" name and
logo are governed separately by the [trademark policy](TRADEMARKS.md)
— code is permissively licensed; the brand is protected. Community and
paid-module boundaries are documented in [`docs/editions.md`](docs/editions.md).

## Documentation

  - [v0.12.0 release notes](docs/releases/v0.12.0.md) - Community scope,
    upgrade safety, verification, and candidate limitations.
  - [Community, Pro, and Enterprise boundaries](docs/editions.md) - repository,
    licensing, and product-module policy.

  - [Architectural decision records](docs/adr/) — read these first
    if you want to contribute.
  - [`CHANGELOG.md`](CHANGELOG.md) — version highlights.
