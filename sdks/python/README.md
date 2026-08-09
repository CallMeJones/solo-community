# Solo Python Starter

`solo_client.py` uses only the Python standard library.

```python
from solo_client import SoloClient

solo = SoloClient(
    base_url="http://127.0.0.1:17821",
)
status = solo.status()
saved = solo.remember(
    "Dana likes weekly summaries on Friday.",
    source_type="sdk_example",
    salience=0.7,
)
inbox = solo.remember_inbox("Review Dana's Friday summary preference.", salience=0.6)
inbox_items = solo.memory_inbox(limit=10)
solo.review_memory(inbox["memory_id"], state="approved")
recall = solo.recall("weekly summaries", limit=3)
context = solo.context("weekly summaries", subject="Dana")
facts = solo.facts_about("Dana", include_as_object=True, limit=3)
recent = solo.recent_memories(limit=10)
updated = solo.update(saved["memory_id"], "Dana likes concise weekly summaries on Friday.")
```

The starter always addresses Community's one Memory Library. Run separate Solo
instances with separate data directories when you need hard isolation.

For MCP smoke and custom agent tools:

```python
session = solo.mcp_connect("my-agent", "0.1.0")
tools = solo.mcp_list_tools(session)
result = solo.mcp_call_tool(session, "memory_context", {
    "query": "weekly summaries",
    "subject": "Dana",
})
```

## Smoke test

```bash
python sdks/python/smoke_test.py
```

The smoke test uses a local mock server, not a Solo daemon.

The cross-SDK matrix also compiles the Python examples:

```bash
python sdks/smoke_matrix.py
```

## Structured memory and documents

```python
entities = solo.entities("Dana", limit=5)
facts = solo.facts_about("Dana", include_as_object=True, limit=10)
documents = solo.list_documents(limit=5)
hits = solo.search_documents("weekly summaries", limit=3)
inspected = solo.inspect_document(documents[0]["doc_id"]) if documents else None
```

`ingest_document(path)` accepts a path readable by the Solo daemon process,
which is usually a local filesystem path on the same machine.

## Manual daemon check

```bash
SOLO_PASSPHRASE=change-me solo daemon --http-port 17821
python examples/python/remember_recall.py "Dana likes weekly summaries on Friday."
```

For a zero-dependency HTTP smoke without importing the SDK client:

```bash
python examples/python/direct_http.py "Dana likes weekly summaries on Friday."
```

For framework starters:

```bash
pip install openai-agents
python examples/python/openai_agents_memory.py "Use my memory for Dana's next weekly review."

pip install langgraph
python examples/python/langgraph_memory.py "Use my memory for Dana's next weekly review."
```

The OpenAI Agents example exposes Solo retrieval and durable-write tools. The
LangGraph example keeps Solo as the long-term memory source and returns context
for the next model node.
