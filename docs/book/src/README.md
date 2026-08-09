# Introduction

Solo is **local-first personal memory for AI assistants**. One
binary, one encrypted SQLite file, no cloud.

You run Solo on your own machine. Your AI assistant — Claude
Desktop, Cursor, an OpenAI-compatible chat client, anything that
speaks the Model Context Protocol (MCP) — connects to it as a
memory backend. Conversations get embedded, clustered into
themes, and distilled into structured facts. Everything stays
on disk inside an encrypted SQLite database that only you hold
the passphrase to.

## What Solo gives you

  - **Persistent memory across sessions.** Your assistant
    remembers what you told it last week even though it had
    no chat history when this session started.
  - **Recall by meaning, not just text match.** Semantic vector
    search via HNSW. "What did I say about the migration?"
    finds the right episode even if it never used the word
    "migration."
  - **Structured distillation.** A background pass periodically
    runs an LLM over your stored memories to extract themes
    (clusters), summarised facts (abstractions), and explicit
    contradictions. You can query memories, the abstractions,
    or both.
  - **Encryption at rest, by default.** SQLCipher with an
    Argon2id-derived key from your passphrase. The database
    file is unintelligible without it.
  - **Single binary.** No services, no docker, no cloud
    accounts. Run `solo daemon` and you're done.

## What Solo isn't

  - **Not a chatbot.** Solo doesn't talk to you — it stores
    what your AI assistant tells it to and serves it back via
    MCP or HTTP.
  - **Not a hosted service.** No SaaS. No remote API. The
    assistant talks to your local Solo over `stdin`/`stdout`
    or `127.0.0.1`.
  - **Not a hosted identity system.** Community has one Memory Library
    per Solo data directory. Run multiple daemons with different data
    directories, passphrases, and ports when you need hard operational
    separation.
  - **Not a replacement for your assistant's context window.**
    Solo augments it. The assistant still does the
    conversational reasoning; Solo just makes long-term recall
    cheap.

## How this guide is organised

  - **[Getting Started](./getting-started.md)** — install,
    initialise, store your first memory, get something out.
    Read this first.
  - **[Model Selection](./model-selection.md)** — picking an
    embedder (StubEmbedder for offline / BGE-M3 for real
    semantic recall) and a Steward LLM (Anthropic, OpenAI,
    or none).
  - **[Consolidation Cycle](./consolidation-cycle.md)** —
    what `solo consolidate` actually does, the four-pass
    re-consolidation tetralogy, and when to run it manually
    vs. let the daemon schedule it.
  - **[MCP Integration](./mcp-integration.md)** — wiring
    Solo into Claude Desktop, Cursor, and other MCP-aware
    clients.
  - **[HTTP API](./http-api.md)** — using Solo over
    HTTP/JSON for non-MCP clients.
  - **Reference** — [flag-by-flag](./cli-reference.md)
    and [env-var-by-env-var](./environment-variables.md)
    listings for power users and ops folk, plus the
    [`solo.config.toml` schema](./configuration-file.md).
  - **[Troubleshooting](./troubleshooting.md)** — diagnosing
    common failure modes (lockfile contention, drift,
    corrupted snapshots, mixed embedder vectors).
  - **[Backups & Recovery](./backups-and-recovery.md)** —
    what to back up, how to restore, when recovery isn't
    possible.

## Where the source lives

The Rust workspace is on GitHub at
[`CallMeJones/solo-community`](https://github.com/CallMeJones/solo-community).
The canonical writer-model contract is
[`docs/adr/0003-writer-model.md`](https://github.com/CallMeJones/solo-community/blob/main/docs/adr/0003-writer-model.md).
