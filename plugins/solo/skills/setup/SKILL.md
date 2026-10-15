---
name: setup
description: Check the Solo plugin's prerequisites, MCP connection, recall hooks, and existing direct-setup conflicts. Use when setting up or troubleshooting this plugin.
---

# Set up Solo Memory

This plugin connects to the user's local Solo daemon at
`http://127.0.0.1:17821/mcp`. It includes MCP configuration, session-start and
prompt recall hooks, and the memory skill. Solo 0.12.11 or newer must already be
installed; its `solo` executable must be on the assistant process's PATH.

1. Check that Solo is running with the intended Memory Library unlocked.
   If necessary, guide the user to open Solo Controls. Never ask them to paste
   their library passphrase into chat or initialize over an existing library.
2. Call this plugin's `memory_context` tool with a harmless setup query and
   `limit: 1`. Report success only if the tool call actually succeeds.
   A configuration file or health check alone does not prove MCP works.
3. Ask the user to inspect `/hooks` in their assistant. Codex requires normal
   review and trust before plugin hooks can run. Never bypass trust. Look for
   one plugin handler for each of `SessionStart` and `UserPromptSubmit`;
   a new session's diagnostic `solo-memory-hook: context delivered` proves
   the hook retrieved context.
4. Check for an earlier direct installation if duplicate Solo hooks or tools
   appear. Direct and plugin setup are alternative installation methods.
   Do not run `solo setup-client <client> --apply` to activate a plugin:
   that adds separate user-level hooks and MCP configuration.

For migration, first verify the plugin's own MCP connection. Explain the two
changes before applying them: `solo setup-client <client> --apply --no-hooks`
removes only Solo-managed direct hooks, then the assistant's MCP remove command
removes the old direct `solo` server. Use `codex mcp remove solo` for user-level
Codex setup, or `claude mcp remove --scope user solo` for user-level Claude Code.
Retain unrelated settings and do not remove unknown or project-level definitions.
Complete normal restart/trust review and check again for duplicate hooks.

If `solo` is missing from PATH, open a new terminal or restart the assistant
after installing Solo. This package uses the default local endpoint and does
not configure Relay, alternate ports, or authenticated endpoints. Use Solo's
direct client setup for those configurations; do not silently change libraries.
Disabling or uninstalling the plugin removes its capabilities from the
assistant, not the user's Solo library.
