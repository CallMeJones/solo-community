# Changelog

This public repository begins with Solo Community 0.12.0. Earlier development
history remains in the project's private archival repositories and is not part
of the Community source distribution.

## 0.12.11

- Client setup now installs automatic Solo recall hooks for Claude Code, Codex,
  and Gemini CLI; Cursor and VS Code Copilot receive session-start integrations.
- Added managed hook removal, configuration status, bounded read-only recall,
  and preservation of existing client settings and hooks.
- Added Claude Code, Gemini CLI, and VS Code Copilot to native client setup;
  updated the web client setup commands and coverage descriptions.
- Copilot hooks support both VS Code's Copilot (Agent Host) and Local runtimes.
- Fixed update selection so numeric versions win over GitHub's release order;
  corrected the regression fixture to cover Windows and Linux packages.

## 0.12.0

- One encrypted local Memory Library stored in `solo.db`.
- Native Windows and Ubuntu desktop/tray packaging.
- Embedded Community Web interface with memories, projects, imports, inbox,
  graph exploration, health, logs, settings, backup, and recovery workflows.
- Local MCP, HTTP API, SDK starter, and offline embedding-model support.
- No chat-agent runtime, tenant administration, database selector, hosted
  control plane, Pro module, or Enterprise module in the Community artifact.
