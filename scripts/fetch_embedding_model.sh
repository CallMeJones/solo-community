#!/usr/bin/env bash
# This release helper is also invoked from WSL checkouts; keep it LF-only.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <destination>" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MANIFEST="$ROOT/installer/models/embedding-model.json"
DESTINATION="$1"
mkdir -p "$DESTINATION"

REPOSITORY="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["source_repository"])' "$MANIFEST")"
REVISION="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1], encoding="utf-8"))["revision"])' "$MANIFEST")"
BASE_URL="https://huggingface.co/$REPOSITORY/resolve/$REVISION"

while IFS=$'\t' read -r source target expected; do
  destination="$DESTINATION/$target"
  if [[ -f "$destination" ]] && [[ "$(sha256sum "$destination" | cut -d' ' -f1)" == "$expected" ]]; then
    echo "embedding asset already verified: $target"
    continue
  fi
  partial="$destination.partial"
  rm -f -- "$partial"
  echo "fetching embedding asset: $target"
  curl --fail --location --retry 3 --output "$partial" "$BASE_URL/$source?download=true"
  echo "$expected  $partial" | sha256sum --check --status
  mv -f -- "$partial" "$destination"
done < <(python3 -c 'import json,sys; data=json.load(open(sys.argv[1], encoding="utf-8")); [print("{}\t{}\t{}".format(item["source"], item["target"], item["sha256"])) for item in data["files"]]' "$MANIFEST")

cp "$MANIFEST" "$DESTINATION/embedding-model.json"
echo "packaged embedding model ready: $DESTINATION"
