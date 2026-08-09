#!/usr/bin/env bash
# Certify that the Linux tray actually starts under a graphical session.
#
# CI's Ubuntu certification is headless, so it cannot run this: it exercises
# the CLI and daemon only. That gap let a crash-on-launch defect ship green —
# solo-tray built, linked, passed clippy and every unit test, and then aborted
# immediately on any real desktop because GTK was never initialized.
# See docs/adr/0016-linux-gtk-initialization.md.
#
# Usage: linux_tray_gui_smoke.sh <tray-binary> <solo-binary> [alive-seconds]

set -euo pipefail

TRAY_BIN="${1:?usage: $0 <tray-binary> <solo-binary> [alive-seconds]}"
SOLO_BIN="${2:?usage: $0 <tray-binary> <solo-binary> [alive-seconds]}"
ALIVE_SECONDS="${3:-15}"

for bin in "$TRAY_BIN" "$SOLO_BIN"; do
  [[ -x "$bin" ]] || { echo "not executable: $bin" >&2; exit 1; }
done

if [[ -z "${DISPLAY:-}" && -z "${WAYLAND_DISPLAY:-}" ]]; then
  echo "no DISPLAY or WAYLAND_DISPLAY; run this under xvfb-run or a real session" >&2
  exit 1
fi

DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/solo-tray-gui.XXXXXXXX")"
TRAY_LOG="$DATA_DIR/tray.log"
export SOLO_PASSPHRASE="${SOLO_TRAY_SMOKE_PASSPHRASE:-solo-tray-gui-smoke-passphrase}"
# Kill by recorded pid, never by pattern: this script's own command line
# contains the tray path as an argument, so `pkill -f "$TRAY_BIN"` matches
# the script too and terminates it, turning a passing smoke into exit 143.
TRAY_PID=""
cleanup() {
  if [[ -n "$TRAY_PID" ]] && kill -0 "$TRAY_PID" 2>/dev/null; then
    kill "$TRAY_PID" 2>/dev/null || true
    for _ in 1 2 3; do
      kill -0 "$TRAY_PID" 2>/dev/null || break
      sleep 1
    done
    kill -9 "$TRAY_PID" 2>/dev/null || true
  fi
  rm -rf -- "$DATA_DIR"
}
trap cleanup EXIT

echo "==> initialize a disposable library"
"$SOLO_BIN" init --data-dir "$DATA_DIR" >/dev/null
[[ -f "$DATA_DIR/solo.db" ]]

echo "==> launch the tray under the graphical session"
setsid "$TRAY_BIN" --data-dir "$DATA_DIR" >"$TRAY_LOG" 2>&1 &
TRAY_PID=$!

# Poll rather than sleeping the whole budget: a launch crash shows up in
# well under a second, and waiting the full window just to report it is waste.
for _ in $(seq 1 "$ALIVE_SECONDS"); do
  if ! kill -0 "$TRAY_PID" 2>/dev/null; then
    echo "tray exited within the liveness window" >&2
    echo "--- tray log ---" >&2
    sed 's/^/  /' "$TRAY_LOG" >&2
    exit 1
  fi
  sleep 1
done

echo "==> tray still running after ${ALIVE_SECONDS}s"

# A living process is necessary but not sufficient: assert the specific
# failure signatures are absent so a future regression reports its cause
# rather than a bare timeout.
if grep -Fq "GTK has not been initialized" "$TRAY_LOG"; then
  echo "tray logged the GTK initialization failure this smoke exists to catch" >&2
  exit 1
fi
if grep -Fq "panicked at" "$TRAY_LOG"; then
  echo "tray panicked:" >&2
  grep -A3 "panicked at" "$TRAY_LOG" | sed 's/^/  /' >&2
  exit 1
fi

echo "Linux tray GUI smoke passed"
echo "  tray:   $TRAY_BIN"
echo "  uptime: >= ${ALIVE_SECONDS}s with no panic and no GTK init failure"
