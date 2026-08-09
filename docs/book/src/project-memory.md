# Project Memory

`solo project` is the first codebase-memory mode. It gives coding
agents a stable project identity, a narrow repo-doc ingest path, and a
way to store durable implementation decisions without indexing the
whole repository.

```text
solo project init [path] [--name <name>] [--id <id>] [--tag <tag>] [--force]
solo project ingest [path] [--dry-run] [--json] [--max-files <n>]
solo project facts [path] [--subject <name>] [--limit <n>] [--json]
solo project decisions [path] --add <text> [--json]
solo project decisions [path] --query <text> [--limit <n>] [--json]
solo project policy [path] [--client generic|codex|claude|cursor] [--json]
```

`project init` writes `.solo/project.toml` with a project name, stable
id, tags, and ignored directories. The default ignore list skips
generated/vendor folders such as `.git`, `node_modules`, `target`,
`dist`, `build`, and `.cache`.

Solo Desktop's Projects view can save a project root, inspect that
`.solo/project.toml`, create it after an explicit init-write
confirmation, run the safe `project ingest --dry-run --json` preview,
and import exactly the previewed docs into the Memory Library through the
running daemon after a second confirmation. It can also copy `project
init`, `project ingest --dry-run --json`, and Codex project-scope setup
fallback commands. When the daemon is running, the same view can save
and search project decisions, and load project facts, in the Memory
Library through daemon HTTP project endpoints without opening a
separate CLI database session.

`project ingest` is document-first. It imports root files such as
`README.md`, `CHANGELOG.md`, and `ARCHITECTURE.md`, plus `.md`,
`.markdown`, and `.txt` files under `docs/`, `doc/`, `adr/`, `adrs/`,
and `rfcs/`. Use `--dry-run` first to see candidates without unlocking
the database; add `--json` when another tool, Desktop, or CI needs the
structured candidate list.

`project decisions --add` stores one durable coding decision as an
episodic memory with `source_type=project_decision` and project
identity in the text and encoding context. `--query` recalls decisions
using the same project identity. Both forms support `--json` for coding
agents and UI clients that need the new memory id or filtered recall
hits without parsing the human output.

`project facts --json` returns the selected project identity, subject,
and fact rows. The command still requires an unlocked Solo database
because facts are read from the derived memory layer.

The daemon exposes the same JSON envelope shapes for Desktop and local
integrations:

```text
POST /v1/project/facts
POST /v1/project/decisions
POST /v1/project/decisions/search
POST /v1/project/policy
```

These routes take an explicit project descriptor in the request body
instead of reading workspace files. That keeps the daemon on its normal
locked database path and avoids giving HTTP callers a new filesystem
read surface. Example decision write:

```json
{
  "project": {
    "name": "Solo",
    "id": "solo",
    "root": "C:\\Users\\Example\\Projects\\solo-community",
    "tags": ["memory", "desktop"]
  },
  "decision": "Use daemon HTTP endpoints for Desktop project memory."
}
```

Document import endpoints do read files from disk. To constrain those
reads to a project or workspace, configure `[workspace_file_access]`
`allowed_roots` in `solo.config.toml` or set `SOLO_WORKSPACE_FILE_ROOTS`
before starting the daemon. The guard applies before HTTP/MCP document
ingest opens the requested path.

`project policy` prints a repo-aware memory policy snippet for coding
agents. It reads `.solo/project.toml`, names the project id/root/tags,
and reminds the agent to retrieve project context before coding, store
durable decisions with project-scoped wording, read workspace files
before trusting memory, and avoid secrets. `--json` returns the same
policy text with project metadata for tools or setup UIs.

This is not a full code indexer. Source files, generated folders, and
live web crawling stay out of scope unless a future explicit command
adds them.
