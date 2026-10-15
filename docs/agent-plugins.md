# Solo plugins for Codex and Claude Code

Install Solo's MCP connection, automatic recall hooks, and memory skills as
one plugin. The same package supports Claude Code and local Codex sessions.

## Before installing

- Install Solo **0.12.11 or newer**. The plugin does not install the Solo runtime.
- Open Solo Controls and unlock your existing Memory Library, or keep
  `solo daemon --http-port 17821` running in a terminal.
- Make sure `solo --version` works in the terminal that starts your assistant.
  Restart the assistant after installing Solo so it sees the updated PATH.

Plugin version 0.1.0 targets the default local endpoint,
`http://127.0.0.1:17821/mcp`. For Relay, another port, or an authenticated
endpoint, use Solo's
[direct client setup](https://github.com/CallMeJones/solo-community/blob/main/docs/client-memory-hooks.md#authentication-and-transport).
Do not enable both installation methods for the same client.

## Install from the repository marketplace

Use a local source checkout instead to test an unpublished branch.

### Claude Code

```sh
claude plugin marketplace add CallMeJones/solo-community --sparse .claude-plugin plugins/solo
claude plugin install solo@solo-community
```

Restart Claude Code. The plugin adds its Solo MCP connection, session-start
and prompt recall hooks, and the `/solo:memory` and `/solo:setup` skills.
Run `/solo:setup` to check the connection.

### Codex

```sh
codex plugin marketplace add CallMeJones/solo-community --ref main --sparse .agents/plugins --sparse plugins/solo
codex plugin add solo@solo-community
```

Restart Codex. Open `/hooks` in Codex CLI and review and trust the plugin's
`SessionStart` and `UserPromptSubmit` hooks. Installation does not bypass
trust. In Codex desktop, use the Plugins Directory's Solo Community source
to install the package; the CLI commands above are also available in
Codex CLI 0.154.0.

The package includes a setup skill for connection checks and a memory skill
for recall, saving, correction, documents, and library management.

Codex supports manually distributed plugins with lifecycle hooks. A package
containing hooks is not eligible for OpenAI's public plugin directory.
This repository marketplace is the distribution source; installing it in a
web chat does not deploy local hook commands.
See the [official packaging guide](https://developers.openai.com/plugins/build/plugins).

## Install a local checkout or extracted package

From this repository's root, build the self-contained marketplace ZIP with
Python 3.10 or newer:

```sh
python scripts/package_agent_plugin.py
```

It writes `target/agent-plugins/solo-agent-plugin-0.1.0.zip` and its SHA-256 file.
Extract the ZIP and register the extracted `solo-agent-plugin-0.1.0` directory
as the marketplace. No source checkout or Python is needed by someone using
an already built package.

For a checkout or extracted directory, substitute its absolute path:

```sh
claude plugin marketplace add /absolute/path/to/marketplace
claude plugin install solo@solo-community

codex plugin marketplace add /absolute/path/to/marketplace
codex plugin add solo@solo-community
```

On Windows, quote paths such as `"C:\\Tools\\solo-agent-plugin-0.1.0"`.
Claude Code does not accept a bare `.` as a marketplace source in some
versions; use the absolute directory path.

For a temporary Claude Code session, `claude --plugin-dir /absolute/path/to/plugins/solo`
loads the plugin without permanent installation.

## Move from direct client setup

Existing users who ran `solo setup-client` already have a separate MCP entry
and recall hooks. Installing a plugin does not remove those definitions.

1. Install the plugin and confirm its own MCP connection works with
   `/solo:setup`, or ask the assistant to call the plugin's `memory_context`
   tool with a harmless query and `limit: 1`.
2. Remove Solo's old direct hooks and old user-level MCP entry for that client:

   ```sh
   # Codex user-level setup:
   solo setup-client codex --apply --no-hooks
   codex mcp remove solo

   # Claude Code user-level setup:
   solo setup-client claude-code --apply --no-hooks
   claude mcp remove --scope user solo
   ```

3. Restart the assistant, complete normal hook review, and check `/hooks`
   for one plugin handler per event.

Run only the commands for the client you are moving. These commands preserve
other servers and unrelated hooks. Project-level or manually customized
connections need their matching scope reviewed before removal.
Do not rerun ordinary `solo setup-client <client> --apply` while using the
plugin: it creates another direct connection and duplicate recall hooks.

To return to direct setup, disable or uninstall the plugin first, then rerun
the ordinary Solo client setup command.

## Verify what loaded

- `claude plugin list --json` lists the enabled plugin and its MCP server.
- `codex plugin list --marketplace solo-community --json` lists the installed
  and enabled Codex plugin.
- `/hooks` in each CLI shows the hook definitions and their source.
- A successful call through the plugin's `memory_context` tool proves the
  memory connection works.
- The Solo connection's tool list includes writes such as `memory_remember`,
  `memory_update`, and `memory_forget`, alongside document, asset, and review
  tools. Client interfaces may add a plugin prefix to these names.
- `solo-memory-hook: context delivered` in a new session's hook diagnostics
  proves that the recall command retrieved context.

`solo setup-client verify` checks the separate direct setup, not a plugin
installation. Use the plugin inventory, MCP call, and hook diagnostics above
for this package.

Hooks return at most three memories, with a bounded context size and a policy
preferring Solo for previous preferences and decisions. Current instructions
and verified workspace facts take precedence. Saving still uses MCP tools;
hooks do not save transcripts. A failed or timed-out recall continues without
Solo. Retrieved memories are supplied to the assistant provider as context.
See [recall behavior and privacy](https://github.com/CallMeJones/solo-community/blob/main/docs/client-memory-hooks.md#what-the-hook-does).

## Use the full Solo toolset

The plugin exposes every tool advertised by the connected Solo MCP server.
It does not filter the connection to read-only tools. In addition to recall,
the assistant can save, update, and forget memories; ingest and import documents;
upload files; manage assets and attachments; prepare downloads; inspect entities,
relationships, and provenance; and review or correct library knowledge.
The exact tools depend on your Solo version.

Ask for the operation in chat, for example: "Remember that this project uses
PostgreSQL", "Import this document into Solo", or "Review the conflicting
facts about this project". The bundled memory skill helps the assistant choose
the appropriate tools. Your assistant's normal permission prompts and Solo's
library and server policies still apply.

Only the automatic session-start and prompt hooks are read-only: they retrieve
relevant context without saving or deleting anything. The harmless read-only
connection test above verifies connectivity; it does not disable write tools.
A visible tool can still fail if the library is locked or a required backend,
such as the embedding service used for saving memories, is unavailable.

## Update and uninstall

Use the assistant's plugin manager to update, disable, or uninstall
`solo@solo-community`. Restart after changing its installed version.
Codex may require review when hook definitions change.

```sh
claude plugin uninstall solo@solo-community
codex plugin remove solo@solo-community
```

Uninstalling removes the plugin's MCP connection, hooks, and skills from the
assistant. It does not delete Solo or its Memory Library, or remove an older
direct setup that you have not migrated.

## Maintainers

Native installation, removal, MCP tool calls, and automatic context delivery
were checked on Windows with Claude Code 2.1.85 and Codex CLI 0.154.0.
Both clients discovered all 39 tools advertised by Solo 0.12.11, including
write, document, attachment, and review tools. Discovery checks did not modify
the library; the live MCP smoke calls used read-only recall.
Codex hooks were reviewed and trusted through its normal interface in an
isolated test profile. No hook-trust bypass was used.

The package has native `.codex-plugin/plugin.json` and
`.claude-plugin/plugin.json` manifests, with shared `.mcp.json` and skills.
Codex explicitly selects
`hooks/codex.json`; Claude automatically discovers `hooks/hooks.json`.
Both clients discover the shared `skills/` directory. Do not declare
Claude's default hook file again in its manifest, which would load it twice.

Run `python scripts/package_agent_plugin.py --check`, validate with
`claude plugin validate plugins/solo` and `claude plugin validate <absolute-repo-path>`,
and test native installation in disposable client configuration directories.
The build script packages an explicit file list and never includes credentials,
local settings, caches, the Solo executable, or a memory library.
