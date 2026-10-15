# Automatic memory in agent clients

Solo's client setup installs the MCP connection **and** supported recall hooks.
Users do not need to write hook scripts or copy a skill just to retrieve memory.
Use the native desktop Connected Tools setup action, or run:

```sh
solo setup-client claude-code --apply
solo setup-client codex --apply
solo setup-client gemini --apply
solo setup-client cursor --apply
solo setup-client copilot --apply
```

The web Connected Clients screen provides these same installation commands.
Without `--apply`, setup previews the affected paths and recall coverage. Start
the Solo daemon before using the assistant. Restart the assistant after setup;
approve any trust request the assistant presents. Codex users can inspect and
trust the installed definitions in `/hooks`. Solo does not bypass client trust,
workspace restrictions, organization policy, or settings that disable hooks.

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
prompt. The full path and conversation transcript are not read or uploaded.
**The recall query is sent to the configured Solo endpoint**, which can be local
or remote. The hook does not persist that prompt as a memory.

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

## Maintenance and diagnosis

```sh
solo setup-client verify claude-code --format json
solo setup-client doctor codex --format json
solo setup-client gemini --apply --no-hooks
```

`verify`, `doctor`, and the JSON client listing include `memory_hooks`. An
`installed` status means managed definitions are present, **not** that the host
trusted or ran them (`runtime_verified` remains false). Check the host's hook
logs for `solo-memory-hook: context delivered` after a new session or prompt.
An offline hook reports `context unavailable; continuing without Solo` on stderr.

Re-running setup updates Solo's marked commands without duplicates. It retains
unrelated servers, hooks and settings. All affected files are parsed before any
write; changed existing files receive timestamped backups. Each replacement is
atomic, and a later write failure triggers restoration of earlier replacements.
Malformed configurations are rejected without partial installation. JSON files
must be strict JSON; setup does not rewrite commented JSONC files.

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
