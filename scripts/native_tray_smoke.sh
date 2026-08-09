#!/usr/bin/env bash
# Native solo-tray smoke helper for macOS/Linux.
#
# This intentionally runs on the target OS. Cross-compiling from Windows does
# not prove Wry/WebKitGTK/WKWebView, tray icons, or keychain prompts work.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

log() {
  printf '==> %s\n' "$*"
}

print_dependency_hints() {
  case "$(uname -s)" in
    Linux)
      log "Linux desktop dependency hints"
      if command -v apt-get >/dev/null 2>&1; then
        cat <<'EOF'
Ubuntu/Debian:
  sudo apt-get update
  sudo apt-get install -y \
    pkg-config \
    libwebkit2gtk-4.1-dev \
    libgtk-3-dev \
    libxdo-dev \
    libayatana-appindicator3-dev \
    libxcb-render0-dev \
    libxcb-shape0-dev \
    libxcb-xfixes0-dev \
    libxkbcommon-dev \
    libssl-dev
EOF
      elif command -v dnf >/dev/null 2>&1; then
        cat <<'EOF'
Fedora:
  sudo dnf install -y \
    pkgconf-pkg-config \
    webkit2gtk4.1-devel \
    gtk3-devel \
    libxdo-devel \
    libappindicator-gtk3-devel \
    libxcb-devel \
    libxkbcommon-devel \
    openssl-devel
EOF
      elif command -v pacman >/dev/null 2>&1; then
        cat <<'EOF'
Arch:
  sudo pacman -S --needed \
    pkgconf \
    webkit2gtk-4.1 \
    gtk3 \
    xdotool \
    libayatana-appindicator \
    libxcb \
    libxkbcommon \
    openssl
EOF
      else
        echo "Install WebKitGTK 4.1, GTK3, AppIndicator, xdo, xcb, xkbcommon, pkg-config, and OpenSSL development packages for this distro."
      fi
      ;;
    Darwin)
      log "macOS dependency hints"
      cat <<'EOF'
macOS:
  xcode-select --install

WKWebView and Keychain are provided by macOS. Run the optional window smoke
from a logged-in desktop session so native permission prompts can appear.
EOF
      ;;
    *)
      log "Unknown OS; continuing with Cargo checks"
      ;;
  esac
}

run_cargo_checks() {
  log "cargo check solo-tray"
  cargo check --locked -p solo-tray --all-targets

  log "cargo test solo-tray"
  cargo test --locked -p solo-tray

  log "cargo clippy solo-tray"
  cargo clippy --locked -p solo-tray --all-targets -- -D warnings
}

run_optional_window_smoke() {
  if [[ "${SOLO_NATIVE_SMOKE_WINDOW:-0}" != "1" ]]; then
    log "Skipping owned-window smoke (set SOLO_NATIVE_SMOKE_WINDOW=1 to run it)"
    return
  fi
  if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 is required for the bounded window smoke" >&2
    exit 1
  fi

  local url="${SOLO_NATIVE_SMOKE_DESKTOP_URL:-http://127.0.0.1:17821/desktop/}"
  local seconds="${SOLO_NATIVE_SMOKE_WINDOW_SECONDS:-8}"
  local target_dir="${CARGO_TARGET_DIR:-target}"
  local tray_bin="${target_dir}/debug/solo-tray"
  log "cargo build solo-tray debug binary"
  cargo build --locked -p solo-tray
  if [[ ! -x "$tray_bin" ]]; then
    echo "solo-tray binary not found at $tray_bin" >&2
    exit 1
  fi

  log "Opening Solo Desktop window for ${seconds}s"
  python3 - "$tray_bin" "$url" "$seconds" <<'PY'
import subprocess
import sys

tray_bin = sys.argv[1]
url = sys.argv[2]
seconds = float(sys.argv[3])
cmd = [
    tray_bin,
    "--desktop-window",
    "--desktop-url",
    url,
]
proc = subprocess.Popen(cmd)
try:
    try:
        proc.wait(timeout=seconds)
    except subprocess.TimeoutExpired:
        print(f"Solo Desktop window stayed alive for {seconds:g}s: {url}")
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
        sys.exit(0)
    raise SystemExit(f"Solo Desktop window exited early with code {proc.returncode}")
finally:
    if proc.poll() is None:
        proc.kill()
PY
}

print_dependency_hints
run_cargo_checks
run_optional_window_smoke

log "Native solo-tray smoke completed"
