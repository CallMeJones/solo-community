#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
patched_source="$repo_root/vendor/glib-0.18.5-solo/src/variant_iter.rs"
expected_sha256="a0f5ee8acb8faa089bcdfbc9a57372609fce7654026ccef7d9a224d05a654ccc"

test -f "$patched_source"
grep -Fq 'glib = { path = "vendor/glib-0.18.5-solo" }' "$repo_root/Cargo.toml"
grep -Fq 'let mut p: *mut libc::c_char = std::ptr::null_mut();' "$patched_source"
grep -Fq '&mut p,' "$patched_source"

actual_sha256="$(sed 's/\r$//' "$patched_source" | sha256sum | awk '{print $1}')"
if [[ "$actual_sha256" != "$expected_sha256" ]]; then
  echo "patched glib source drifted: expected $expected_sha256, got $actual_sha256" >&2
  exit 1
fi

resolved="$(cargo tree --locked --target all -i glib@0.18.5)"
if [[ "$resolved" != *"glib-0.18.5-solo"* ]]; then
  echo "Cargo did not resolve Solo's patched glib source: $resolved" >&2
  exit 1
fi

echo "verified patched glib 0.18.5 source and Cargo resolution"
