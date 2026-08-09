#!/usr/bin/env bash
# Package prebuilt x86_64 GNU/Linux Solo binaries for Ubuntu 24.04.

set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <version> <release-dir> <output-dir>" >&2
  exit 2
fi

VERSION="$1"
RELEASE_DIR="$(cd "$2" && pwd)"
OUTPUT_DIR="$3"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "invalid package version: $VERSION" >&2
  exit 2
fi
for binary in solo solo-tray; do
  if [[ ! -x "$RELEASE_DIR/$binary" ]]; then
    echo "missing executable: $RELEASE_DIR/$binary" >&2
    exit 1
  fi
done
MODEL_DIR="$RELEASE_DIR/models/all-MiniLM-L6-v2"
for model_file in model.onnx tokenizer.json config.json special_tokens_map.json tokenizer_config.json embedding-model.json; do
  if [[ ! -f "$MODEL_DIR/$model_file" ]]; then
    echo "missing packaged embedding asset: $MODEL_DIR/$model_file" >&2
    exit 1
  fi
done

mkdir -p "$OUTPUT_DIR"
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/solo-deb.XXXXXXXX")"
trap 'rm -rf -- "$STAGE"' EXIT

PACKAGE_ROOT="$STAGE/solo-memory_${VERSION}_amd64"
install -d \
  "$PACKAGE_ROOT/DEBIAN" \
  "$PACKAGE_ROOT/usr/bin" \
  "$PACKAGE_ROOT/usr/share/applications" \
  "$PACKAGE_ROOT/usr/share/doc/solo-memory" \
  "$PACKAGE_ROOT/usr/share/icons/hicolor/32x32/apps" \
  "$PACKAGE_ROOT/usr/share/solo/models/all-MiniLM-L6-v2"

install -m 0755 "$RELEASE_DIR/solo" "$PACKAGE_ROOT/usr/bin/solo"
install -m 0755 "$RELEASE_DIR/solo-tray" "$PACKAGE_ROOT/usr/bin/solo-tray"
install -m 0644 "$ROOT/installer/linux/solo.desktop" \
  "$PACKAGE_ROOT/usr/share/applications/solo.desktop"
install -m 0644 "$ROOT/crates/solo-tray/assets/s_tray_icon_32.png" \
  "$PACKAGE_ROOT/usr/share/icons/hicolor/32x32/apps/solo.png"
install -m 0644 "$ROOT/installer/linux/README-UBUNTU.txt" \
  "$PACKAGE_ROOT/usr/share/doc/solo-memory/README-UBUNTU.txt"
install -m 0644 "$ROOT/LICENSE" "$PACKAGE_ROOT/usr/share/doc/solo-memory/copyright"
install -m 0644 "$MODEL_DIR"/* "$PACKAGE_ROOT/usr/share/solo/models/all-MiniLM-L6-v2/"

INSTALLED_SIZE="$(du -sk "$PACKAGE_ROOT/usr" | cut -f1)"
{
  echo "Package: solo-memory"
  echo "Version: $VERSION"
  echo "Section: utils"
  echo "Priority: optional"
  echo "Architecture: amd64"
  echo "Installed-Size: $INSTALLED_SIZE"
  echo "Maintainer: Solo Project <support@solo.local>"
  echo "Depends: libayatana-appindicator3-1, libgtk-3-0t64, libsecret-1-0, libwebkit2gtk-4.1-0, libxdo3, libxkbcommon-x11-0"
  echo "Homepage: https://github.com/CallMeJones/solo-community"
  echo "Description: Private local memory and project context for AI tools"
  echo " Solo Community includes the encrypted local daemon, CLI, Desktop,"
  echo " system tray, imports, projects, graph, inbox, and MCP integrations."
} > "$PACKAGE_ROOT/DEBIAN/control"
chmod 0644 "$PACKAGE_ROOT/DEBIAN/control"

ARTIFACT="$OUTPUT_DIR/solo-${VERSION}-ubuntu24.04-amd64.deb"
dpkg-deb --root-owner-group --build "$PACKAGE_ROOT" "$ARTIFACT"
echo "$ARTIFACT"
