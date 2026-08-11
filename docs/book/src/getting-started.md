# Getting Started

This path gets a new Windows or Ubuntu user from installation to verified
semantic recall without requiring Ollama or an API key.

## Install

### Windows

Download the latest Windows installer from the
[Solo Community releases](https://github.com/CallMeJones/solo-community/releases/latest).
The per-user installer includes `solo.exe`, Solo Controls/Desktop, the bundled
MiniLM model, and the required runtime DLLs. It does not require Rust, Visual
Studio, or administrator privileges.

Open a new PowerShell after installation:

```powershell
solo --version
solo init
solo doctor --round-trip
```

### Ubuntu 24.04

Download `solo-<version>-ubuntu24.04-amd64.deb` from the latest release:

```bash
sudo apt install ./solo-<version>-ubuntu24.04-amd64.deb
solo --version
solo init
solo doctor --round-trip
```

The package includes the command-line tools, Solo Controls/Desktop, bundled
MiniLM assets, desktop integration, autostart support, and Secret Service
keyring integration. Ubuntu 24.04 x86-64 is the certified Linux target.

### From source

Contributors can build the Rust workspace directly:

```bash
git clone https://github.com/CallMeJones/solo-community
cd solo-community
cargo build --release
```

Packaged builds are recommended for normal users because they include the
pinned offline model assets in the expected layout.

## Initialize the Community library

Solo Community owns one encrypted Memory Library per data directory. The
default is `~/.solo` on Linux and `%USERPROFILE%\.solo` on Windows.

```bash
solo init
```

Choose a passphrase you will not lose. It derives the SQLCipher key and cannot
be recovered by Solo. Initialization creates:

- `solo.db`, the encrypted SQLite database;
- `solo.config.toml`, the plaintext salt, model identity, and non-secret
  runtime configuration.

Fresh installs use bundled `all-MiniLM-L6-v2` for local semantic recall and
persist `[llm] mode = "none"`. An inherited cloud API key does not enable
hosted memory processing.

## Validate setup safely

```bash
solo doctor --round-trip
```

The round-trip test creates a temporary encrypted library, writes a diagnostic
memory, embeds it with bundled MiniLM, indexes it, recalls it, and deletes the
temporary library. It never writes test data to the real library.

Use this for file/config health too:

```bash
solo doctor --with-stats
```

If the daemon owns `solo.lock`, doctor reads live status from the daemon rather
than failing because it cannot open the database a second time.

## Start Solo

The installed Solo Controls/Desktop app is the easiest path. From a terminal:

```bash
solo daemon --http-port 17821
```

The daemon owns the single writer, reader pool, scheduled clustering and
extraction, HNSW snapshots, and local HTTP API. One-shot commands cannot open
the same data directory while it holds `solo.lock`.

## Store and recall a memory

From Solo Desktop, save a note in the Memory Inbox and use Recall to find it.
For a stopped-daemon CLI smoke test:

```bash
solo remember "Solo is the local-first memory system I installed today."
solo recall "what memory system did I install?"
```

Recall works immediately with bundled MiniLM. The returned `cos_distance` is
smaller for more similar vectors. Solo's hybrid retrieval also uses lexical
signals when the embedding model misses an abstract phrasing.

## Connect an MCP client

Point Claude Desktop, Cursor, Codex, or another MCP host at:

```text
command: solo
args: ["mcp-stdio"]
```

See [MCP Integration](./mcp-integration.md) for host-specific configuration and
the local security model.

## Enable advanced knowledge extraction (optional)

Recall and documents work without a Steward model. To generate facts,
entities, relationships, abstractions, and contradiction candidates, open:

**Solo Web → Settings → Steward LLM**

Choose one of:

- **Local Ollama** — processing stays on the device; `qwen3:8b` is the normal
  starting model and `qwen3:4b` is the lighter option.
- **Ollama Cloud direct** — bearer-authenticated access to `ollama.com`.
- **Ollama Cloud via local** — a signed-in loopback daemon using a `-cloud`
  model; inference is still off device.
- **Anthropic** or **OpenAI** — hosted providers referenced through environment
  variables.
- **Disabled** — keep recall/documents/clustering without generative extraction.

Solo states exactly where content will be processed. Hosted choices cannot be
saved until the user explicitly consents, and key values are never persisted.
After restarting Solo, choose **Backfill existing memories now** and watch the
coverage/progress panel instead of waiting for the hourly schedule.

See [Steward Setup](./steward-setup.md) and
[Model Selection](./model-selection.md) for details.
