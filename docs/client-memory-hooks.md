# Automatic memory recall in your assistants

**Available in Solo 0.12.11 and newer.** Client setup installs the MCP connection
and supported recall hooks together. You do not need to write hook scripts,
edit configuration files, or install a skill just to retrieve memory.

## How it works

MCP gives an assistant tools for reading and writing Solo memory. A recall hook
runs at a supported session or prompt event, asks Solo for relevant memories,
and puts that context into the assistant's conversation. Recall can therefore
run before the assistant decides which tools to use.

The supplied policy prefers Solo for previous preferences and project decisions.
The current conversation and verified workspace facts take precedence over
history; the assistant's system rules still apply. Its native memory remains
available. Hooks do not force the assistant to use every retrieved fact.

Recall hooks are read-only. Saving a durable fact, correcting a memory, or
forgetting something still uses Solo's MCP tools or app. A skill or initial
instruction can help the assistant choose when to save; the hook does not
automatically archive conversations.

## Set up your assistants

Codex and Claude Code also support the [Solo plugin](https://github.com/CallMeJones/solo-community/blob/main/docs/agent-plugins.md),
which bundles MCP, hooks, and memory skills. Use one installation method per
assistant; the plugin guide includes migration steps for existing direct setup.

1. Install a current Solo build, then open Solo and unlock your existing
   Memory Library. For a terminal-only setup, keep
   `solo daemon --http-port 17821` running.
2. In the native **Connected Tools** panel, choose your assistant and use
   **Setup / repair**. This installs its connection and supported hooks.
   On the **Connected Clients** screen, use **Copy install** and run the copied
   command in a terminal on the machine where your assistant is installed.
3. Open or restart the assistant, and complete its normal trust review as
   described below. Keep Solo running while you use it.

For command-line setup, run only the commands for the assistants you use:

```sh
solo setup-client claude-code --apply
solo setup-client codex --apply
solo setup-client copilot --apply
solo setup-client gemini --apply
solo setup-client cursor --apply
```

Without `--apply`, a command previews the affected paths and recall coverage.
Setup uses your installed Solo executable, preserves unrelated settings, and
backs up existing files it changes. Run it again if that executable moves.

### Finish in each assistant

| Assistant | After setup |
| --- | --- |
| Claude Code | Restart Claude Code and review any workspace or hook trust request. |
| Codex | Restart Codex, open `/hooks`, review Solo's definitions, and trust them. For one user-level installation, `SessionStart` and `UserPromptSubmit` should each show one active hook. |
| VS Code Copilot | Restart VS Code, use a trusted workspace, and start a new **Copilot** or **Local** agent session with hooks enabled. |
| Gemini CLI | Restart Gemini CLI and complete any hook trust review. |
| Cursor | Restart Cursor and begin a new session. Session-start context is asynchronous, so it may arrive after the first answer. |

Codex may also offer hook review during startup. Review the Solo entries from
your user `hooks.json` (or project `hooks.json` if you chose project scope).
On Windows the command appears as an encoded PowerShell wrapper around the
installed Solo executable. Use Codex's normal trust controls; no trust-bypass
flag is needed. Existing user and organization settings that disable hooks
remain in effect until changed through the assistant's normal controls.

## Upgrade an existing connection

Installing a newer Solo executable does not add hooks to connections created
by older versions. **Run client setup again for each assistant after updating.**
You can use **Setup / repair** or the same commands above, even if its MCP
connection already works.

On Windows:

1. Choose **Quit Solo** from the system-tray menu. Closing its window can leave
   Solo running in the background.
2. Run the installer from the
   [latest Solo Community release](https://github.com/CallMeJones/solo-community/releases/latest).
   It updates the program in place and preserves your existing library.
3. Open Solo again, unlock that library, and rerun client setup.
4. Restart the assistants and review any new or changed hooks. In Codex, check
   `/hooks` again if its definitions changed.

Initialization is for a new library; upgrading uses the existing library.
Check the active executable with `solo --version` and
`solo setup-client --help`. The latter must list `claude-code` and `gemini`.
Some early development builds also reported 0.12.11 without these commands;
install the released build if they are missing.

Solo 0.12.11 packages are Windows-only. The published Ubuntu package remains
0.12.3 and does not include this automatic setup. Linux/macOS users need a
current source build for the new hooks.

## Verify recall

With Solo running, check the clients you connected:

```sh
solo setup-client verify claude-code
solo setup-client verify codex
solo setup-client verify copilot
solo setup-client doctor codex
```

Use `doctor claude-code` or `doctor copilot` for the other clients, and add
`--format json` to any check for structured output.

| Check | What it proves |
| --- | --- |
| `verify` | Reports the MCP connection and managed hook installation status. |
| `doctor` | The local configuration is checked, and Solo's MCP endpoint is probed for the required memory tools. |
| A new assistant session's hook log | `solo-memory-hook: context delivered` confirms the hook retrieved context. |

For the selected client, expect a valid MCP entry and
`memory_hooks.status: installed`. A valid MCP entry alone does not mean the host
trusted or executed the hook. The configuration check leaves
`runtime_verified: false` even after you have tested a session; it does not
record host-side execution evidence.

For an answer-level check, save a harmless test preference with Solo's
`memory_remember` tool, then ask about it in a fresh assistant session without
including the answer in your question. Ask it to use context already provided
and inspect the hook log. A successful explicit MCP tool call proves the connection;
the hook log is needed to confirm automatic delivery. Remove the disposable
test memory afterward if you no longer want it.

The 0.12.11 release was live-tested with Claude Code 2.1.85, Codex 0.154.0,
and VS Code 1.141.0 / Copilot 0.69.0 Agent Host. Gemini, Cursor, and Copilot
Local have automated adapter coverage; they were not separately live-tested
for this release.

## Supported coverage

| Client | Automatically delivered context | Configuration |
| --- | --- | --- |
| Claude Code | `SessionStart`, `UserPromptSubmit` | User MCP in `~/.claude.json`; hooks in `~/.claude/settings.json` |
| Codex | `SessionStart`, `UserPromptSubmit` | MCP in `~/.codex/config.toml`; hooks in `~/.codex/hooks.json` |
| Gemini CLI | `SessionStart`, `BeforeAgent` | MCP and hooks in `~/.gemini/settings.json` |
| Cursor | `sessionStart` (asynchronous) | MCP in `~/.cursor/mcp.json`; hooks in `~/.cursor/hooks.json` |
| GitHub Copilot in VS Code | `SessionStart`, Copilot (Agent Host) and Local runtimes | MCP in VS Code's user `mcp.json`; hooks in `~/.copilot/hooks/solo-memory.json` |
| Ordinary Claude Desktop / web chat | No supported automatic context hook | Desktop MCP setup remains available with `claude-desktop`; web chat uses its connector |

This targets client versions supporting the linked hook contracts below. Other
Copilot runtimes (cloud agents, other editors) are not covered by the VS Code
setup. Its personal hook file also uses the Copilot CLI/SDK version-1 contract;
Solo returns both the SDK and Local context fields for compatibility. Cursor's
session-start hook is asynchronous and may finish after the
first answer. Neither adapter claims prompt-time context delivery. Their
assistants can use Solo MCP tools for later, topic-specific recall.

`CODEX_HOME` and `CLAUDE_CONFIG_DIR` are honored. Codex also supports
`--scope project --project-dir <directory>` for project MCP and hook files.
Project trust is still required. Installing in both Codex scopes may run both
sets of hooks; choose one scope for each project.

## What the hook does

The managed command invokes the installed `solo agent-hook` executable. It reads
the client's event JSON, calls Solo's `memory_context` over Streamable HTTP MCP,
and returns the client's documented context JSON. No extra runtime or downloaded
script is needed (Windows uses its built-in PowerShell to invoke Solo safely).
Re-run setup if the Solo executable moves.

Session start retrieves user preferences and project decisions using the workspace
folder name. Before a prompt, recall also uses up to 2,000 characters of the
prompt. The query does not include the full workspace path or conversation
transcript.
**The recall query is sent to the configured Solo endpoint**, which can be local
or remote. Retrieved memories become context for the connected assistant and
are subject to that assistant provider's normal data handling. The hook does
not persist the prompt as a memory.

Delivered context contains up to three memory records, their available evidence
IDs and timestamps, a conflict indicator, and a short memory policy. Output is
capped at 6,000 bytes. The policy prefers Solo over built-in persistent memory for
prior preferences and decisions, with current instructions and verified workspace
facts taking precedence. Retrieved records are historical data, not executable
instructions. The assistant still decides which facts apply and when to use MCP
to save confirmed durable information; the hook never writes memories itself.

If recall fails, authentication is missing, the daemon is offline, input is
invalid, or the deadline expires, the hook exits successfully with empty JSON.
The default deadline is seven seconds; host hook limits are ten seconds where
supported. Input and response sizes are bounded. Diagnostics omit prompts,
recalled text, URLs and credentials. No native memory files or settings are changed.

## Authentication and transport

The default endpoint is `http://127.0.0.1:17821/mcp`. Use `--url` to select another
HTTP(S) MCP endpoint, including a Relay URL. Hooks use that exact URL and do not
follow redirects. An authenticated endpoint needs its bearer token in the agent
process environment: `SOLO_BEARER_TOKEN` by default. A custom name can be selected
with `--hook-token-env NAME`. Setup stores the variable name, never its value.
The hook does not inherit or refresh an assistant's separate OAuth session.

Automatic recall requires HTTP. Choosing `--transport stdio` configures MCP only
and removes Solo's HTTP hooks, avoiding accidental recall from a different
daemon/library. Existing client authentication settings are preserved; the
assistant's own MCP connection must also be authorized as usual.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| `claude-code` or `gemini` is an unknown setup command | Check `solo --version` and the executable selected by your shell. Install the released build, open a new terminal, and rerun setup. On Windows, `Get-Command solo` shows the selected path. |
| MCP works, but automatic recall does not | Rerun setup after upgrading, restart the assistant, check hook trust and disabled-hook settings, then inspect its hook log. |
| Codex shows hooks needing review | Open `/hooks`, review Solo's definitions and trust them. Configuration installation alone does not grant trust. |
| The hook says `context unavailable; continuing without Solo` | Confirm Solo is unlocked and running. Run `doctor` for that client and check that its endpoint and hook authentication agree. |
| Authentication fails after OAuth sign-in | The hook needs its configured bearer-token environment variable; it cannot reuse the assistant's OAuth session. See [authentication and transport](#authentication-and-transport). |
| Solo hooks run twice in Codex | Check for both user- and project-level installations. Choose the scope you want and remove Solo's hooks from the other using `--no-hooks`. |
| Cursor's first answer lacks memory | Session-start recall is asynchronous. Use the Solo MCP tools for topic-specific recall while context is arriving. |
| Claude Desktop/web chat does not inject context automatically | Those chat apps have MCP/connector access but no supported lifecycle hook. Use Solo tools and the client's memory instructions or skill. |

Setup rejects malformed JSON/TOML without partial installation. JSON files
must be strict JSON; it does not rewrite commented JSONC files. If setup reports
a concurrent edit or incomplete rollback, keep the reported backups and inspect
the affected files before retrying.

## Update or remove the hooks

```sh
# Refresh the connection and Solo hooks.
solo setup-client codex --apply

# Keep MCP tools available, but remove Solo's automatic recall hooks.
solo setup-client codex --apply --no-hooks
```

Re-running setup updates Solo's marked commands without duplicates. It retains
unrelated servers, hooks and settings. All affected files are parsed before any
write; changed existing files receive timestamped backups. Each replacement is
atomic, and a later write failure triggers restoration of earlier replacements.
Use the matching `--scope project --project-dir <directory>` options when
updating or removing a project-level Codex installation.

`--no-hooks --apply` removes only handlers installed by Solo and leaves the MCP
connection available. Existing empty hook groups, user-disabled settings and
native memory remain under the user's control.

## Client contracts

- [Claude Code hooks](https://code.claude.com/docs/en/hooks)
- [Codex hooks](https://learn.chatgpt.com/docs/hooks)
- [Gemini CLI hooks reference](https://geminicli.com/docs/hooks/reference/)
- [Cursor hooks](https://cursor.com/docs/hooks)
- [VS Code agent hooks](https://code.visualstudio.com/docs/agent-customization/hooks)
- [Copilot SDK/CLI hook contract](https://docs.github.com/en/copilot/reference/hooks-reference)
