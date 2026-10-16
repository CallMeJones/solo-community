# Getting Started

This path gets a new Windows, Ubuntu or Mac user from installation to verified
semantic recall without requiring Ollama or an API key. The
[0.12.12 Community preview](https://github.com/CallMeJones/solo-community/releases/tag/v0.12.12-community.1)
includes all four native builds. Start with
[Downloads and Supported Platforms](./downloads.md) to match your OS and
processor, including Intel versus Apple Silicon Macs.

**Already using Solo?** Follow the
[upgrade and client setup checklist](./automatic-memory-recall.md#upgrade-an-existing-connection).
Upgrading keeps your existing library; rerunning client setup adds the new
recall hooks to an existing connection.

## Install

### Windows

Download [Windows Setup for the 0.12.12 preview](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/SoloSetup-0.12.12-x86_64.exe),
or select the [latest stable release](https://github.com/CallMeJones/solo-community/releases/latest).
The preview is unsigned and may show an unknown-publisher prompt.
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

Download [the Ubuntu DEB for the 0.12.12 preview](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-0.12.12-ubuntu24.04-amd64.deb):

```bash
sudo apt install ./solo-0.12.12-ubuntu24.04-amd64.deb
solo --version
solo init
solo doctor --round-trip
```

The package includes the command-line tools, Solo Controls/Desktop, bundled
MiniLM assets, desktop integration, autostart support, and Secret Service
keyring integration. Ubuntu 24.04 x86-64 is the certified Linux target.
The 0.12.12 package includes automatic recall setup.

### Mac (macOS 15 or newer)

Choose the Apple Silicon or Intel DMG for your Mac using the
[download guide](./downloads.md#choose-the-right-build). Open the DMG, copy
**Solo.app** into **Applications**, and launch Solo. The preview is ad-hoc
signed and not notarized; review the signing notice and approval steps in the
[Mac installation guide](./downloads.md#mac).

For terminal commands and assistant hooks, run the included `install-cli.sh`
after copying the app and add `$HOME/.local/bin` to PATH. Follow the full Mac
steps in the download guide, then restart your terminal and assistant:

```bash
solo --version
solo doctor --round-trip
```

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
default is `~/.solo` on Linux/macOS and `%USERPROFILE%\.solo` on Windows.

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

## Connect your assistants

With Solo 0.12.11 or newer running and unlocked, open the native
**Connected Tools** panel and choose **Setup / repair** for the assistants you
use. The same setup is available from a terminal:

```sh
solo setup-client claude-code --apply
solo setup-client codex --apply
solo setup-client copilot --apply
```

Each command installs the MCP connection and supported automatic recall hooks.
Restart the assistant afterward. In Codex, open `/hooks`, review Solo's entries,
and trust them. Check the connection with, for example:

```sh
solo setup-client verify codex
solo setup-client doctor codex
```

See [Automatic Memory Recall](./automatic-memory-recall.md) for Gemini/Cursor,
upgrade steps, trust, and a check that proves a hook delivered memory.
For ordinary Claude Desktop chat, run
`solo setup-client claude-desktop --apply`; its MCP tools remain available
without automatic lifecycle hooks. [MCP Integration](./mcp-integration.md)
covers the transports and local security model.

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
