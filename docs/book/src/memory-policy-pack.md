# Memory Policy Pack

Solo works best when MCP clients follow a consistent memory policy:
retrieve before answering when prior context matters, remember only
durable facts, and route corrections through Solo instead of silently
overwriting history.

Copy the policy that matches your client:

- [Generic MCP agent](./policies/generic-mcp-agent.md)
- [Claude Desktop](./policies/claude-desktop.md)
- [Cursor](./policies/cursor.md)
- [Codex](./policies/codex.md)

Solo Desktop also exposes these policies in **Connected Tools** so you
can copy the matching client instructions beside the setup and verify
actions.

## Recommended Defaults

- Prefer `memory_context` as the first lookup for prior preferences,
  decisions, people, projects, and plans.
- Use `memory_entities` before asserting facts about an ambiguous name.
- Use `memory_remember_batch` for multiple durable memories from one
  turn.
- Use `memory_update` for direct corrections.
- Use `memory_contradiction_resolve` only after the user clarifies which
  side is current.

## Isolation

Community reads and writes one Memory Library. If personal, work, or client
memories must be isolated, run separate Solo instances with separate data
directories, passphrases, and ports; do not copy memory between them unless
the user explicitly asks.

## Safety Rules

Do not store secrets, API keys, passwords, private tokens, recovery
phrases, raw credentials, or sensitive personal data unless the user
explicitly asks you to remember it.

Solo is durable memory, not scratch space. Store information the user is
likely to want available in future sessions.
