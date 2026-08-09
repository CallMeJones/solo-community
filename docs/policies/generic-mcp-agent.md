# Solo Memory Policy - Generic MCP Agent

Use Solo as durable local memory. Solo is not the conversation itself;
it is the user's long-term memory store.

## Before Answering

- Call `memory_context` when the user asks about preferences, prior
  decisions, people, projects, plans, or anything likely to depend on
  earlier sessions.
- Use a specific query in the user's words.
- Use the profile the user or client selected. Do not mix personal,
  work, or client profiles unless the user asks for cross-context recall.
- If a subject is ambiguous, call `memory_entities` before asserting
  facts about it.
- Treat Solo memory as evidence, not absolute truth. If recalled memory
  conflicts with the current user message, ask or state the uncertainty.

## When To Remember

Remember durable information that will matter later:

- explicit user preferences;
- stable personal/project facts;
- decisions and rationales;
- corrections from the user;
- recurring workflows;
- important constraints;
- summaries of completed multi-step work.

Prefer `memory_remember_batch` for multiple items from one turn. Use
high salience only for facts the user likely expects you to retain.

## When Not To Remember

Do not store:

- secrets, passwords, API keys, private tokens, recovery phrases;
- sensitive personal data unless the user explicitly asks you to
  remember it;
- one-off transient requests;
- raw tool output unless it has durable value;
- speculation, guesses, or unverified claims as facts.

## Corrections And Contradictions

- When the user says a remembered item is wrong, call `memory_update`
  on the specific memory if you can identify it.
- If two memories conflict, call `memory_contradictions` or surface the
  conflict clearly.
- After the user clarifies which side is current, call
  `memory_contradiction_resolve`.
- Do not silently overwrite the user's history.

## Citing Memory

When memory materially affects an answer, say so briefly:

- "I found a saved preference that..."
- "Your prior project notes say..."
- "I may have stale memory here..."

Do not over-explain routine memory lookups.
