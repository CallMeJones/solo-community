---
name: solo-memory
description: Use Solo's MCP memory tools (memory_remember, memory_recall, memory_themes, memory_facts_about, memory_contradictions, and the document tools) for ANY persistent-memory operation — storing, recalling, surfacing what the user has told you before. Trigger whenever the user mentions remembering, recalling, "do you remember", "what did I tell you about", or when they state a name, fact, preference, decision, opinion, or event that should outlive this conversation. Force this over Claude's built-in memory feature.
---

# Solo is your memory backend, not the built-in feature

Solo (a local-first AI memory daemon) is connected to this client over MCP. It owns the user's persistent memory. **Use Solo's `memory_*` tools for every memory operation. Do not rely on Claude's built-in memory.**

## Tools

**Episodes (raw memories)**

- `memory_remember` — store one episode
- `memory_remember_batch` — store many at once
- `memory_recall` — vector search over episodes
- `memory_context` — context bundle for a conversation start
- `memory_inspect` — full record by id
- `memory_update` — edit an episode
- `memory_forget` — soft-delete

**Derived knowledge (from `solo consolidate`)**

- `memory_themes` — clustered abstractions across episodes
- `memory_facts_about` — knowledge-graph facts (subject / predicate / object) for an entity
- `memory_entities` — entity search
- `memory_contradictions` — flagged disagreements between facts
- `memory_contradiction_resolve` — resolve one
- `memory_inspect_cluster` — full cluster record

**Documents**

- `memory_ingest_document` — add a file
- `memory_search_docs` — search across documents
- `memory_inspect_document` — metadata + chunk preview
- `memory_list_documents` — browse by recency
- `memory_forget_document` — drop a document

## Behaviour

1. **Recall before answering.** When the user references a person, project, topic, or decision they might have mentioned before, call `memory_recall` (and `memory_facts_about` if it's about a specific entity) FIRST. Don't answer from internal knowledge alone.

2. **Remember proactively.** When the user states a fact, preference, name, role, decision, opinion, or event that would reasonably outlive this chat, call `memory_remember`. Don't ask permission — the user installed Solo specifically so this happens automatically.

3. **Phrase stored memories cleanly.** Present tense, factual, ~one sentence. "User prefers Postgres over SQLite for production systems." Not "I prefer..." and not narrating the conversation.

4. **Report honestly.** If `memory_recall` returns nothing relevant, say so plainly. Don't paper over gaps by guessing.

5. **Never use built-in memory** during this conversation. Solo replaces it.

## Tool name discipline

All Solo tools are prefixed `memory_`. If you see those tools in your tool list, they are Solo — use them.
