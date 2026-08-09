# Solo Memory Policy - Codex

Use Solo as durable local memory for coding sessions across repositories.

## Setup

Use first-class setup-client support instead of hand-editing config when
possible:

```bash
solo setup-client codex --scope user --dry-run
solo setup-client codex --scope user --apply
solo setup-client verify codex
```

For repo-specific memory tools in trusted projects, run from the project
root with `--scope project`. The generated Codex config uses
`[mcp_servers.solo]` and never writes passphrases or bearer-token values.
`solo setup-client verify codex` treats those values in config as
plaintext secret leaks.

## Retrieval

Before making changes, call `memory_context` when prior decisions,
release constraints, project conventions, or user preferences may be
relevant. For repo-specific work, include the repository and feature name
in the query.

Use the configured Solo profile for the current workspace. Do not mix
private, work, or client profiles unless the user explicitly asks for
cross-profile context.

Do not let memory override the current workspace. Read the files first
when behavior depends on code.

## Writing Memory

At the end of meaningful work, remember durable facts such as:

- decisions made;
- implementation approach;
- commands/tests that verify the change;
- known follow-ups;
- project-specific gotchas;
- user preferences that affected the work.

Use `memory_remember_batch` for several concise items. Include source
terms like repository, crate/package, feature, and date when helpful.

## Safety

Never store secrets, credentials, tokens, private keys, raw proprietary
logs, or sensitive personal data unless the user explicitly requests it.

When the user corrects you or a remembered fact is stale, use
`memory_update` when you can identify the memory. Otherwise store a clear
correction and avoid presenting stale memory as current.
