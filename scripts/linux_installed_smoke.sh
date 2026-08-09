#!/usr/bin/env bash
# Certify an installed Solo Community CLI and its packaged offline model.

set -euo pipefail

SOLO_BIN="${1:-/usr/bin/solo}"
DATA_DIR="${2:-}"
MODEL_DIR="${3:-/usr/share/solo/models/all-MiniLM-L6-v2}"
EXPECTED_TOOL_COUNT="${EXPECTED_TOOL_COUNT:-39}"
PASSPHRASE="${SOLO_SMOKE_PASSPHRASE:-solo-linux-installed-smoke-passphrase}"

if [[ ! -x "$SOLO_BIN" ]]; then
  echo "installed Solo executable is missing: $SOLO_BIN" >&2
  exit 1
fi

if [[ -z "$DATA_DIR" ]]; then
  DATA_DIR="$(mktemp -d "${TMPDIR:-/tmp}/solo-community-smoke.XXXXXXXX")"
elif [[ -e "$DATA_DIR" ]]; then
  if [[ ! -d "$DATA_DIR" ]]; then
    echo "smoke data path is not a directory: $DATA_DIR" >&2
    exit 1
  fi
  if find "$DATA_DIR" -mindepth 1 -maxdepth 1 -print -quit | grep -q .; then
    echo "refusing non-empty smoke data directory: $DATA_DIR" >&2
    exit 1
  fi
else
  mkdir -p "$DATA_DIR"
fi

for model_file in \
  model.onnx \
  tokenizer.json \
  config.json \
  special_tokens_map.json \
  tokenizer_config.json \
  embedding-model.json; do
  if [[ ! -f "$MODEL_DIR/$model_file" ]]; then
    echo "installed embedding asset is missing: $MODEL_DIR/$model_file" >&2
    exit 1
  fi
done

export SOLO_PASSPHRASE="$PASSPHRASE"
export SOLO_EMBEDDING_MODEL_DIR="$MODEL_DIR"

echo "==> binary and Community-only help"
"$SOLO_BIN" --version
HELP_OUTPUT="$("$SOLO_BIN" --help)"
for forbidden in \
  '/v1/tenants' \
  '/v1/settings/relay' \
  'X-Solo-Tenant' \
  '--tenant' \
  'relay_public' \
  'jar''vis'; do
  if grep -Fqi -- "$forbidden" <<<"$HELP_OUTPUT"; then
    echo "Community help contains forbidden term: $forbidden" >&2
    exit 1
  fi
done

echo "==> initialize one encrypted Memory Library"
"$SOLO_BIN" init --data-dir "$DATA_DIR"
[[ -f "$DATA_DIR/solo.db" ]]
[[ ! -e "$DATA_DIR/tenants" ]]
[[ ! -e "$DATA_DIR/tenants_index.db" ]]

STAMP="$(date +%s)-$$"
MEMORY_MARKER="linux-community-memory-$STAMP"
DOCUMENT_MARKER="linux-community-document-$STAMP"
POST_BACKUP_MARKER="linux-community-after-backup-$STAMP"
SOURCE_DIR="$DATA_DIR/smoke-source"
BACKUP_DIR="$DATA_DIR/smoke-backup"
BACKUP_PATH="$BACKUP_DIR/solo-backup.db"
MCP_OUTPUT="$DATA_DIR/mcp-output.jsonl"
mkdir -p "$SOURCE_DIR" "$BACKUP_DIR"

echo "==> semantic memory and Markdown import"
"$SOLO_BIN" remember --data-dir "$DATA_DIR" \
  "Remember marker $MEMORY_MARKER for the installed Linux Community certification."
RECALL_OUTPUT="$("$SOLO_BIN" recall --data-dir "$DATA_DIR" --limit 10 "$MEMORY_MARKER")"
grep -Fq -- "$MEMORY_MARKER" <<<"$RECALL_OUTPUT"

printf '# Linux Community smoke\n\nDocument marker %s verifies packaged-model import.\n' \
  "$DOCUMENT_MARKER" > "$SOURCE_DIR/community-smoke.md"
"$SOLO_BIN" import markdown --data-dir "$DATA_DIR" "$SOURCE_DIR/community-smoke.md"
# `documents list` prints id/title/chunks/status/ingested, and the title
# comes from the first Markdown heading, never the file name. Assert the
# listing on the derived title, then confirm the source path and the
# embedded marker through `documents inspect`, which does print both.
DOCUMENTS_OUTPUT="$("$SOLO_BIN" documents list --data-dir "$DATA_DIR" --limit 20)"
grep -Fq -- 'Linux Community smoke' <<<"$DOCUMENTS_OUTPUT"
DOCUMENT_ID="$(awk 'NR > 2 { print $1; exit }' <<<"$DOCUMENTS_OUTPUT")"
DOCUMENT_DETAIL="$("$SOLO_BIN" documents inspect --data-dir "$DATA_DIR" \
  --full-content "$DOCUMENT_ID")"
grep -Fq -- 'community-smoke.md' <<<"$DOCUMENT_DETAIL"
grep -Fq -- "$DOCUMENT_MARKER" <<<"$DOCUMENT_DETAIL"

echo "==> MCP initialize, tool inventory, and context recall"
{
  printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"solo-linux-community-smoke","version":"1.0"}}}'
  printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
  printf '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_context","arguments":{"query":"%s","limit":5}}}\n' \
    "$MEMORY_MARKER"
} | "$SOLO_BIN" mcp-stdio --data-dir "$DATA_DIR" > "$MCP_OUTPUT"

python3 - "$MCP_OUTPUT" "$EXPECTED_TOOL_COUNT" "$MEMORY_MARKER" <<'PY'
import json
import sys

path, expected_raw, memory_marker = sys.argv[1:]
expected = int(expected_raw)
messages = []
with open(path, encoding="utf-8") as stream:
    for line in stream:
        line = line.strip()
        if line:
            messages.append(json.loads(line))

initialized = next((item for item in messages if item.get("id") == 1), None)
listed = next((item for item in messages if item.get("id") == 2), None)
context = next((item for item in messages if item.get("id") == 3), None)
if initialized is None or "result" not in initialized:
    raise SystemExit("MCP initialize response is missing")
if listed is None or "result" not in listed:
    raise SystemExit("MCP tools/list response is missing")
if context is None or "result" not in context:
    raise SystemExit("MCP memory_context response is missing")
tools = listed["result"].get("tools", [])
if len(tools) != expected:
    raise SystemExit(f"expected {expected} MCP tools, got {len(tools)}")
for tool in tools:
    text = json.dumps(tool, sort_keys=True).lower()
    for forbidden in ("x-solo-tenant", "tenant_id", "relay_public", "jar" + "vis"):
        if forbidden in text:
            raise SystemExit(f"MCP tool surface contains forbidden term {forbidden}: {tool.get('name')}")
context_text = "\n".join(
    item.get("text", "")
    for item in context["result"].get("content", [])
    if item.get("type") == "text"
)
if memory_marker not in context_text:
    raise SystemExit("MCP memory_context did not recall the installed-package marker")
PY

echo "==> encrypted backup and restore drill"
"$SOLO_BIN" backup --data-dir "$DATA_DIR" --to "$BACKUP_PATH"
[[ -s "$BACKUP_PATH" ]]
"$SOLO_BIN" remember --data-dir "$DATA_DIR" \
  "Temporary marker $POST_BACKUP_MARKER must disappear after restore."
"$SOLO_BIN" restore --data-dir "$DATA_DIR" --from "$BACKUP_PATH" --confirm

RESTORED_OUTPUT="$("$SOLO_BIN" recall --data-dir "$DATA_DIR" --limit 20 "$MEMORY_MARKER")"
grep -Fq -- "$MEMORY_MARKER" <<<"$RESTORED_OUTPUT"
if "$SOLO_BIN" recall --data-dir "$DATA_DIR" --limit 20 "$POST_BACKUP_MARKER" | \
  grep -Fq -- "$POST_BACKUP_MARKER"; then
  echo "post-backup marker survived restore" >&2
  exit 1
fi

"$SOLO_BIN" doctor --data-dir "$DATA_DIR" --with-stats

echo "Linux installed Community smoke passed"
echo "  binary: $SOLO_BIN"
echo "  data:   $DATA_DIR"
echo "  model:  $MODEL_DIR"
