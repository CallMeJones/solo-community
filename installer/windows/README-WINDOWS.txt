Solo for Windows
================

This installer adds solo.exe and, when available, solo-tray.exe to:

  %LOCALAPPDATA%\Programs\Solo

It also adds that folder to your user PATH. Open Solo Controls from the Start
menu to create encrypted Solo memory, choose your passphrase, and start Solo.

For command-line help, open a new PowerShell after installing and run:

  solo --help
  solo doctor

Recommended desktop flow
------------------------

Use Solo Controls from the Start menu for first setup and day-to-day use. It
can create encrypted Solo memory on a new install, unlock and start the daemon,
open Solo, show Health/Logs/MCP status, and help wire clients without
copy/pasting long commands.

If you prefer PowerShell:

  # Initialize only when creating a new library:
  solo init
  solo daemon --http-port 17821

Solo's local HTTP/MCP endpoint defaults to:

  http://127.0.0.1:17821/mcp

Connect your assistants and enable automatic recall
--------------------------------------------------

Solo 0.12.11 and newer install the MCP connection and supported recall hooks
together. Keep Solo running and unlocked. In native Connected Tools, choose
the assistant and use Setup / repair. The browser Connected Clients screen
provides commands to run locally.

For command-line setup, run only the commands for the assistants you use:

  solo setup-client claude-code --apply
  solo setup-client codex --apply
  solo setup-client copilot --apply
  solo setup-client gemini --apply
  solo setup-client cursor --apply

Omit --apply to preview the affected paths first. Setup preserves other
settings and servers, backs up existing changed files, and updates Solo's
hooks without duplicates.

Restart the assistant after setup. For Codex, open /hooks, review the Solo
definitions, and trust them. SessionStart and UserPromptSubmit should each
show one active hook for a user-level installation. For VS Code Copilot,
start a new Copilot or Local agent session in a trusted workspace with hooks
enabled. Setup preserves native trust and disabled-hook settings.

Claude Code, Codex, and Gemini CLI receive session-start and prompt recall.
VS Code Copilot receives session-start recall. Cursor's session-start recall
is asynchronous and may arrive after the first answer.

The hook asks Solo for relevant memory and provides it to the assistant
automatically. Current instructions take precedence over history. Saving or
correcting memories still uses Solo's tools or app; conversations are not
automatically saved. Retrieved memory becomes context for the connected
assistant and is subject to its provider's normal data handling.

Check the clients you connected, for example:

  solo setup-client verify codex
  solo setup-client doctor codex

Verify reports installed definitions; doctor also checks the running MCP
endpoint. Confirm execution in a new assistant session's hook log: look for
"solo-memory-hook: context delivered". Installed configuration alone does
not prove the assistant has trusted or run a hook.

Solo Community always connects clients to its one local encrypted Memory
Library. Projects organize context inside that library; they do not create
additional databases.

Ordinary Claude Desktop chat can use "solo setup-client claude-desktop --apply"
for MCP tools. Claude Desktop and web chat have no supported automatic
lifecycle hook; Claude Code is the Claude client with this recall integration.

Upgrading an existing installation
---------------------------------

1. Choose Quit Solo from its system-tray menu. Closing the window can leave
   Solo running in the background.
2. Run the latest official Windows installer. It updates the program in
   place and preserves your existing Memory Library.
3. Open Solo again and unlock that library.
4. Rerun client setup for each assistant, even if its MCP connection already
   worked. This installs the new hooks on older connections.
5. Restart the assistants and review any new or changed hook trust requests.

Use "solo --version" and "solo setup-client --help" to check the installed
build. Help must list claude-code and gemini. If it does not, install the
released build and open a new terminal; some earlier development builds also
reported 0.12.11. "Get-Command solo" in PowerShell shows the selected binary.

To remove only Solo's automatic hooks while keeping MCP tools, use the
matching client name, for example:

  solo setup-client codex --apply --no-hooks

Full setup, coverage, authentication, and troubleshooting guide:
  https://github.com/CallMeJones/solo-community/blob/main/docs/client-memory-hooks.md

Advanced stdio mode
-------------------

Solo still supports stdio for hosts that cannot use HTTP:

  solo mcp-stdio

Prefer the daemon HTTP endpoint when multiple agents or Solo need to
share one running memory backend.

Optional memory instructions and skills
--------------------------------------

The installer includes Claude skills at:

  %LOCALAPPDATA%\Programs\Solo\skills

Skills and initial instructions can help an assistant choose when to save,
correct, or recall memory. Supported coding assistants receive automatic
recall through the hooks installed by client setup. Ordinary chat apps use
their MCP/connector tools and memory instructions or skills.

Using Ollama for steward work
-----------------------------

Ollama is installed separately from https://ollama.com. After Ollama is
installed and running:

  ollama pull qwen2.5-coder:7b
  solo daemon --consolidate-interval-secs 3600 --ollama-model qwen2.5-coder:7b

The --ollama-model flag points Solo's steward/consolidation work at
Ollama's OpenAI-compatible local endpoint.

What the installer does not require
-----------------------------------

The Windows installer does not require Rust, Cargo, cargo-binstall,
link.exe, Visual Studio, or Visual Studio Build Tools.

Those tools are only needed if you choose to compile Solo, cargo-binstall,
or another Rust crate from source.

More help
---------

User guide:
  https://github.com/CallMeJones/solo-community/tree/main/docs/book/src

Releases and downloads:
  https://github.com/CallMeJones/solo-community/releases/latest
