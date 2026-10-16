# Download and install Solo

[Solo 0.12.12 Community preview](https://github.com/CallMeJones/solo-community/releases/tag/v0.12.12-community.1)
is available for Windows, Ubuntu and Mac. This is a prerelease; GitHub's
[latest stable release](https://github.com/CallMeJones/solo-community/releases/latest)
can show an older version. Use the preview link for the downloads below.

## Choose the right build

Solo is a native app. Each download is built for a particular operating system
and processor type, using the same Solo source and including the offline memory
model. Choose the row matching your machine:

| Your machine | Processor label | Installer | Portable or archive |
| --- | --- | --- | --- |
| Windows x64, Intel or AMD processor | `x86_64` / `AMD64` | [Windows Setup EXE](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/SoloSetup-0.12.12-x86_64.exe) | [Windows ZIP](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-cli-0.12.12-x86_64-pc-windows-msvc.zip) |
| Ubuntu 24.04 x64, Intel or AMD processor | `x86_64` / `amd64` | [Ubuntu DEB](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-0.12.12-ubuntu24.04-amd64.deb) | [Linux archive](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-cli-0.12.12-x86_64-unknown-linux-gnu.tar.gz) |
| Apple Silicon Mac, macOS 15 or newer | `aarch64` / `arm64` | [Apple Silicon DMG](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-0.12.12-aarch64-apple-darwin.dmg) | [Apple Silicon archive](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-0.12.12-aarch64-apple-darwin.tar.gz) |
| Intel Mac, macOS 15 or newer | `x86_64` | [Intel Mac DMG](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-0.12.12-x86_64-apple-darwin.dmg) | [Intel Mac archive](https://github.com/CallMeJones/solo-community/releases/download/v0.12.12-community.1/solo-0.12.12-x86_64-apple-darwin.tar.gz) |

**Intel and Apple Silicon describe the two Mac processor families.** On a Mac,
open **Apple menu → About This Mac**. A **Chip** entry naming an Apple M-series
chip means Apple Silicon; a **Processor** entry naming Intel means Intel Mac.
Check the macOS version there too. Both Mac downloads require macOS 15 or newer.

On Windows, check **Settings → System → About → System type** for x64. On Linux,
`uname -m` reports the processor architecture; use the Ubuntu x64 package when
it reports `x86_64` on Ubuntu 24.04. The labels `x86_64`, `x64`, `AMD64` and
`amd64` describe the same processor architecture, used by both Intel and AMD.

This preview does not include native Windows ARM or Linux ARM packages. Older
macOS versions and other Linux distributions are not certified download targets.
A separate build is needed for each supported OS and processor combination;
one executable cannot run across all of them. See the repository's
[source build instructions](https://github.com/CallMeJones/solo-community#from-source-git-clone)
if a packaged target is unavailable; compatibility and prerequisites vary.

## Install

### Windows

Run the Setup EXE. It installs Solo for your user and adds the command-line tool
to PATH without requiring Rust, Visual Studio or administrator privileges.
This preview is unsigned, so Windows may show an unknown-publisher prompt.
Verify that you downloaded it from the Solo GitHub release before approving.

Open **Solo Controls** from the Start menu to create or unlock your library.
Restart your terminal and coding assistant after installation so they see Solo
on PATH. For the portable ZIP, extract the entire archive and keep its `models`
directory beside `solo.exe`.

### Ubuntu 24.04

From the directory containing the downloaded DEB, run:

```bash
sudo apt install ./solo-0.12.12-ubuntu24.04-amd64.deb
```

Then launch Solo from the application menu. The package includes desktop and
keyring integration, command-line tools and the offline model. Use `apt` so
required desktop dependencies are installed too. If you use the archive,
extract it fully and keep `models` beside the executable.

### Mac

1. Open the DMG matching your processor and drag **Solo.app** into **Applications**.
2. Open Solo and create or unlock your library. The app has an ad-hoc integrity
   signature, but is not Apple Developer ID signed or notarized. macOS may
   require approval in **System Settings → Privacy & Security**. Verify the
   source and checksum before approving.
3. For terminal commands and assistant recall hooks, keep the DMG mounted and
   run its included helper after copying the app:

   ```bash
   cd /Volumes/Solo
   bash ./install-cli.sh
   ```

4. Add `$HOME/.local/bin` to your shell PATH. For the current terminal:

   ```bash
   export PATH="$HOME/.local/bin:$PATH"
   solo --version
   ```

   Put the same PATH setting in your shell's startup file for future sessions,
   then restart your coding assistant so it inherits the updated PATH.

The helper creates links to `/Applications/Solo.app` and preserves existing
regular files and unrelated links. Archive users can extract the full archive,
copy its `Solo.app` into Applications, and run `bash ./install-cli.sh` from the
extracted directory. Keep the app bundle intact; it contains the offline model.

## Update an existing installation

Quit Solo from its tray/menu-bar menu before replacing the program; closing
its window can leave it running. Install the new package, reopen Solo, and
unlock your existing library. Initialization is only needed for a new library.
This release keeps an existing library's configured embedder and does not
migrate its stored vectors.

After updating, rerun **Connected Tools → Setup / repair** for each assistant,
then restart the assistant and complete its normal hook trust review. See
[automatic recall and upgrade steps](https://github.com/CallMeJones/solo-community/blob/main/docs/client-memory-hooks.md).
Codex and Claude Code can instead use the
[Solo plugin](https://github.com/CallMeJones/solo-community/blob/main/docs/agent-plugins.md),
which bundles MCP, hooks and skills. Use one connection method per assistant.
The plugin ZIP is separate from the Solo runtime; install Solo first.

## Verify your download and setup

Each release asset has a matching `.sha256` file. Compare the checksum in that
file with the hash of your download. For example:

```powershell
# Windows PowerShell
Get-FileHash .\SoloSetup-0.12.12-x86_64.exe -Algorithm SHA256
```

```bash
# Ubuntu
sha256sum solo-0.12.12-ubuntu24.04-amd64.deb
# Mac: substitute the Intel filename if needed
shasum -a 256 solo-0.12.12-aarch64-apple-darwin.dmg
```

Once the CLI is on PATH, run `solo --version` to confirm the selected version
and `solo doctor --round-trip` to check memory storage and semantic recall in
a temporary library. For connected assistants, follow the hook and MCP checks
in the automatic recall guide linked above.

The [0.12.12 release notes](https://github.com/CallMeJones/solo-community/blob/main/docs/releases/v0.12.12.md)
record validation and known limitations, including the existing automatic
contradiction-detection gap. Physical Mac desktop acceptance remains manual.
