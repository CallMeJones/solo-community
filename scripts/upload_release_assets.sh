#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 ]]; then
  echo "usage: $0 <tag> <asset> [<asset> ...]" >&2
  exit 2
fi

tag="$1"
shift

if [[ ! "$tag" =~ ^[A-Za-z0-9._-]+$ ]]; then
  echo "::error::unsafe release tag: $tag"
  exit 1
fi
if [[ ! "${GITHUB_REPOSITORY:-}" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "::error::GITHUB_REPOSITORY is missing or invalid"
  exit 1
fi

for command_name in gh sha256sum basename cut; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "::error::required command is unavailable: $command_name"
    exit 1
  fi
done

release_endpoint="repos/${GITHUB_REPOSITORY}/releases/tags/${tag}"
uploads=()
seen_names="|"

for asset in "$@"; do
  if [[ ! -f "$asset" ]]; then
    echo "::error::release asset is not a regular file: $asset"
    exit 1
  fi
  name="$(basename "$asset")"
  if [[ ! "$name" =~ ^[A-Za-z0-9._-]+$ ]]; then
    echo "::error::unsafe release asset name: $name"
    exit 1
  fi
  if [[ "$seen_names" == *"|${name}|"* ]]; then
    echo "::error::duplicate input release asset name: $name"
    exit 1
  fi
  seen_names+="${name}|"

  expected="sha256:$(sha256sum "$asset" | cut -d' ' -f1)"
  existing_count="$(
    gh api "$release_endpoint" \
      --jq "[.assets[] | select(.name == \"${name}\")] | length"
  )"
  if [[ "$existing_count" == "0" ]]; then
    uploads+=("$asset")
    continue
  fi
  if [[ "$existing_count" != "1" ]]; then
    echo "::error::release $tag has duplicate asset name $name"
    exit 1
  fi
  existing_digest="$(
    gh api "$release_endpoint" \
      --jq ".assets[] | select(.name == \"${name}\") | .digest // empty"
  )"
  if [[ -z "$existing_digest" || "$existing_digest" != "$expected" ]]; then
    echo "::error::refusing to replace release asset $name: expected $expected, found ${existing_digest:-no digest}"
    exit 1
  fi
  echo "release asset already matches: $name ($expected)"
done

if (( ${#uploads[@]} > 0 )); then
  gh release upload "$tag" "${uploads[@]}"
fi

for asset in "$@"; do
  name="$(basename "$asset")"
  expected="sha256:$(sha256sum "$asset" | cut -d' ' -f1)"
  final_count="$(
    gh api "$release_endpoint" \
      --jq "[.assets[] | select(.name == \"${name}\")] | length"
  )"
  final_digest=""
  if [[ "$final_count" == "1" ]]; then
    final_digest="$(
      gh api "$release_endpoint" \
        --jq ".assets[] | select(.name == \"${name}\") | .digest // empty"
    )"
  fi
  if [[ "$final_count" != "1" || "$final_digest" != "$expected" ]]; then
    echo "::error::published release asset $name has count $final_count and digest ${final_digest:-missing}; expected one asset with $expected"
    exit 1
  fi
  echo "release asset verified: $name ($expected)"
done
