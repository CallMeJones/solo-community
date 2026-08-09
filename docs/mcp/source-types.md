# `source_type` convention values

Status: **conventions, not enums** — Solo stores `source_type` as plain
`TEXT` on the `episodes` row and the `memory_recall` / `memory_inspect`
pipelines do not enforce a closed value set. Anything an MCP client
writes is accepted. This page documents the **canonical six** values
that the Solo ecosystem and compatible MCP clients agree
on so that consolidation, dashboards, and future audit features can
speak the same vocabulary.

When in doubt, use one of these. If a new convention is needed (say a
crawler agent that wants `web_fetch_result`), pick a snake-case
name + propose it in a PR + add it here.

## `user_message`

The user's own utterance. Default value when `source_type` is omitted
on `memory_remember` / `memory_remember_batch`.

  - Source: the user typed it.
  - Salience: typically 0.5–0.7.
  - Example: `"I'm interviewing at Quotient next Tuesday"`.

## `user_preference`

A preference, identity fact, or other explicit "this is who I am /
what I want" statement from the user. Higher salience than a plain
`user_message` so consolidation gives it more weight. Pinned by
an MCP client during onboarding (name, role, working style).

  - Source: the user typed it AND the agent has detected/inferred
    it's an enduring preference, not a transient remark.
  - Recommended salience: **0.8–1.0**.
  - Example: `"I prefer pull requests under 400 lines"`.

## `user_clarification`

The user resolved an ambiguity or chose a side in a contradiction
the agent surfaced. Written by an MCP client when the agent fires a
`memory_contradictions` lookup, surfaces conflicting facts, and the
user picks one. The new clarification supersedes the older fact for
the next consolidation cycle.

  - Source: agent-surfaced contradiction + user response.
  - Recommended salience: 0.7+ (the user's most recent word wins).
  - Example: `"actually I switched roles — I'm at Modus now, not
    Quotient"`.

## `user_confirmation`

The user explicitly agreed with a fact the agent surfaced — "yes,
that's right", "correct", "still true". Bumps the matching triple's
confidence on the next consolidation cycle so the agent can rely on
it more aggressively in subsequent turns.

  - Source: agent-surfaced fact + user assent.
  - Recommended salience: matches the fact's existing salience (the
    confirmation is a reinforcement, not a new claim).
  - Example: `"yes, still vegetarian"`.

## `agent_response`

The assistant's response from the previous turn. Written by an MCP
client at turn boundaries so the agent can recall what it said
without re-reading the conversation. Useful for "what did you tell
me last time about X?" lookups.

  - Source: the assistant.
  - Recommended salience: ≤ 0.5 (the user's words generally outrank
    the agent's).
  - Example: `"I suggested upgrading to PostgreSQL 17 for the
    JSONB performance fix"`.

## `tool_output`

A tool's structured result (e.g. `fs_read`, `web_fetch`, `bash`).
Written by an MCP client when a tool produces information the agent
wants to surface in future turns. Consolidation may demote these to
`tier=cold` once they age out — they're snapshots of point-in-time
tool results, not durable preferences.

  - Source: a tool call's output.
  - Recommended salience: ≤ 0.3.
  - Example: `"git log shows 3 commits on main since yesterday:
    abc123, def456, ghi789"`.

## Adding a new value

These are conventions, not an enum, so additions don't require a
Solo release:

  1. Pick a snake-case name (matches the other values' shape).
  2. Document it here (the canonical reference for MCP client
     authors).
  3. Use it from your client. Solo, consolidation, and audit will
     accept the new value as TEXT.

The advantage of the documentation-level convention (vs a Rust
enum) is that an experimental crawler / browser-extension / IDE
plugin can add its own `source_type` and start recording episodes
the same day, without waiting for a Solo release to bless the
value.
