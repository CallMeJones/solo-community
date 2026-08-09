# Importers

Solo importers are document-first. They turn external exports into
stable local Markdown files, then reuse the normal document ingest
pipeline for chunking, embedding, and dedupe.

```text
solo import markdown <path> --dry-run
solo import text <path> --dry-run
solo import json <path> --dry-run
solo import chatgpt <export-dir-or-json> --dry-run
solo import claude <export-dir-or-json> --dry-run
solo import bookmarks <file> --dry-run
```

Use `--dry-run` first. It reports scanned records, candidate records,
filtered records, skipped records, and estimated chunk candidates
without opening the encrypted database.

Add `--json` to any dry-run command for structured output that setup
UIs and scripts can consume without scraping the human report. `--json`
is intentionally dry-run only.

Solo Desktop's Data view can run the same dry-run previews and show the
candidate output without importing records. The Data view can also
import Markdown, text, JSON, ChatGPT, Claude, and bookmark exports
through the running daemon after an explicit "Allow import into Memory
Library" confirmation. The same view lists recent documents in the
Memory Library so you can confirm the import result without switching
to the CLI, search imported document chunks, and use Inspect to open the
document metadata plus chunk previews. Selected active documents can be
forgotten from the same panel after an explicit confirmation.

## Schema-Aware Sources

`chatgpt` expects either a `conversations.json` file or an export
directory containing `conversations.json`. It extracts conversation
id/title/timestamps and message transcripts from the standard ChatGPT
`mapping` shape. Use `--conversation <id-or-title>` to select one or
more conversations.

`claude` expects a JSON export file or a directory containing
`conversations.json`. It supports the common `chat_messages` shape and
the simpler `messages` shape.

`bookmarks` supports browser bookmark JSON trees and Netscape bookmark
HTML files. Bookmarks are imported as metadata only: title, URL, folder
when known, and added timestamp when present. Solo does not crawl or
snapshot bookmarked pages.

## Materialized Files

Non-dry-run schema imports write Markdown files to:

```text
<data-dir>/imports/<source>/
```

File names are stable across reruns, based on record title plus a
source-id hash. This keeps the import understandable on disk and lets
the existing document ingest path handle dedupe.
