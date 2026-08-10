#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf -- "$tmp"' EXIT

mkdir -p "$tmp/bin"
cat >"$tmp/bin/gh" <<'MOCK_GH'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  api)
    filter="${4:-}"
    name="$(printf '%s' "$filter" | sed -n 's/.*name == "\([^"]*\)".*/\1/p')"
    if [[ -z "$name" ]]; then
      echo "mock gh could not parse asset name from filter: $filter" >&2
      exit 1
    fi
    if [[ "$filter" == *"| length"* ]]; then
      awk -F '|' -v name="$name" '$1 == name { count++ } END { print count + 0 }' "$MOCK_ASSET_STATE"
    else
      awk -F '|' -v name="$name" '$1 == name { print $2 }' "$MOCK_ASSET_STATE"
    fi
    ;;
  release)
    if [[ "${2:-}" != "upload" ]]; then
      echo "unexpected mock gh release command: $*" >&2
      exit 1
    fi
    shift 3
    for asset in "$@"; do
      name="$(basename "$asset")"
      digest="sha256:$(sha256sum "$asset" | cut -d' ' -f1)"
      printf '%s|%s\n' "$name" "$digest" >>"$MOCK_ASSET_STATE"
      printf '%s\n' "$name" >>"$MOCK_UPLOAD_LOG"
    done
    ;;
  *)
    echo "unexpected mock gh command: $*" >&2
    exit 1
    ;;
esac
MOCK_GH
chmod +x "$tmp/bin/gh"

state="$tmp/assets.tsv"
uploads="$tmp/uploads.log"
: >"$state"
: >"$uploads"
asset="$tmp/solo-test.zip"
printf 'first certified build\n' >"$asset"

run_uploader() {
  PATH="$tmp/bin:$PATH" \
    MOCK_ASSET_STATE="$state" \
    MOCK_UPLOAD_LOG="$uploads" \
    GITHUB_REPOSITORY="CallMeJones/solo-community" \
    bash "$repo_root/scripts/upload_release_assets.sh" "$@"
}

run_uploader v1.2.3 "$asset" >"$tmp/first.log"
if [[ "$(wc -l <"$uploads" | tr -d ' ')" != "1" ]]; then
  echo "first publication did not upload exactly once" >&2
  exit 1
fi

run_uploader v1.2.3 "$asset" >"$tmp/retry.log"
if [[ "$(wc -l <"$uploads" | tr -d ' ')" != "1" ]]; then
  echo "matching retry unexpectedly uploaded again" >&2
  exit 1
fi
grep -Fq 'release asset already matches: solo-test.zip' "$tmp/retry.log"

second_asset="$tmp/solo-second.zip"
printf 'second certified build\n' >"$second_asset"
run_uploader v1.2.3 "$asset" "$second_asset" >"$tmp/partial.log"
if [[ "$(wc -l <"$uploads" | tr -d ' ')" != "2" ]]; then
  echo "partial retry did not upload exactly the missing asset" >&2
  exit 1
fi
grep -Fq 'release asset already matches: solo-test.zip' "$tmp/partial.log"
grep -Fxq 'solo-second.zip' "$uploads"

printf 'different rebuild\n' >"$asset"
if run_uploader v1.2.3 "$asset" >"$tmp/mismatch.log" 2>&1; then
  echo "mismatched retry was accepted" >&2
  exit 1
fi
grep -Fq 'refusing to replace release asset solo-test.zip' "$tmp/mismatch.log"
if [[ "$(wc -l <"$uploads" | tr -d ' ')" != "2" ]]; then
  echo "mismatched retry changed upload state" >&2
  exit 1
fi

printf 'first certified build\n' >"$asset"
if run_uploader v1.2.3 "$asset" "$asset" >"$tmp/duplicate.log" 2>&1; then
  echo "duplicate input asset name was accepted" >&2
  exit 1
fi
grep -Fq 'duplicate input release asset name: solo-test.zip' "$tmp/duplicate.log"

if run_uploader '../unsafe' "$asset" >"$tmp/tag.log" 2>&1; then
  echo "unsafe tag was accepted" >&2
  exit 1
fi
grep -Fq 'unsafe release tag' "$tmp/tag.log"

echo "release asset uploader guard tests passed"
