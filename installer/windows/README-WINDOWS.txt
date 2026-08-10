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

  solo init
  solo daemon

Solo's local HTTP/MCP endpoint defaults to:

  http://127.0.0.1:17821/mcp

Using Solo as an MCP memory server
----------------------------------

The recommended path is the daemon HTTP endpoint above. Let Solo preview or
repair client config safely:

  solo setup-client list
  solo setup-client doctor
  solo setup-client codex --scope user --transport http --dry-run

Add --apply only after reviewing the dry-run output. Solo creates backups
before writing supported client config files.

Solo Community always connects clients to its one local encrypted Memory
Library. Projects organize context inside that library; they do not create
additional databases.

Claude Code can be added directly with its MCP CLI:

  claude mcp add --transport http --scope user solo http://127.0.0.1:17821/mcp

Claude Desktop and Cursor are configured through Solo's setup-client helper.
Restart the MCP host after changing its config.

Advanced stdio mode
-------------------

Solo still supports stdio for hosts that cannot use HTTP:

  solo mcp-stdio

Prefer the daemon HTTP endpoint when multiple agents or Solo need to
share one running memory backend.

Claude Agent Skills
-------------------

The installer includes Agent Skills for Claude that automate memory operations
and enforce Solo as the memory backend. Skills are installed to:

  %LOCALAPPDATA%\Programs\Solo\skills

These skills are pre-configured to use Solo's MCP memory tools. You can add
them to Claude Desktop's Capabilities > Skills menu for one-click activation
in conversations.

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

Release verification
--------------------

Before shipping a Windows build, verify the installed app rather than only the
build output. Run these from the repository root:

  Push-Location .\apps\web
  npm.cmd run build
  Pop-Location
  .\scripts\sync_solo_web_assets.ps1
  cargo build --locked -p solo-cli -p solo-tray --release
  $sourceDir = "D:\solo-target\release"
  & "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" /DAppVersion=0.12.0 /DSourceDir="$sourceDir" /DOutputDir="." installer\windows\SoloSetup.iss
  & .\installer\windows\SoloSetup-0.12.0-x86_64.exe /VERYSILENT /SUPPRESSMSGBOXES /NORESTART
  .\scripts\windows_installed_smoke.ps1 -DesktopClickSmoke -TimeoutSeconds 60 -DesktopClickSmokeTimeoutSeconds 30

Use a `SourceDir` that matches your Cargo target directory. On this
workstation, release binaries land in `D:\solo-target\release`; a default
checkout may use `target\release` instead.

If Inno Setup (`ISCC.exe`) is unavailable on a development machine, you can
refresh `%LOCALAPPDATA%\Programs\Solo\solo.exe` and `solo-tray.exe` from the
release directory as a local fallback, then run the same installed smoke. That
fallback verifies the installed binaries and embedded Desktop assets, but it
does not produce a distributable setup EXE.
