import json
import os
import sys
from pathlib import Path
from typing import Any, TypedDict

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdks" / "python"))

from langgraph.graph import END, START, StateGraph  # noqa: E402
from solo_client import SoloClient  # noqa: E402


solo = SoloClient(
    base_url=os.getenv("SOLO_URL", "http://127.0.0.1:17821"),
    bearer_token=os.getenv("SOLO_BEARER_TOKEN"),
)


class MemoryState(TypedDict, total=False):
    query: str
    subject: str
    memory: dict[str, Any]
    answer: str


def retrieve_solo_memory(state: MemoryState) -> MemoryState:
    return {
        "memory": solo.context(
            state["query"],
            subject=state.get("subject"),
            limit=5,
        )
    }


def draft_answer(state: MemoryState) -> MemoryState:
    memory = state["memory"]
    return {
        "answer": (
            "Solo memory is available for the next model node.\n"
            f"Recall hits: {memory['sections']['recall']['count']}\n"
            f"Facts: {memory['sections']['facts']['count']}\n"
            f"Context JSON:\n{json.dumps(memory, indent=2)}"
        )
    }


builder = StateGraph(MemoryState)
builder.add_node("retrieve_solo_memory", retrieve_solo_memory)
builder.add_node("draft_answer", draft_answer)
builder.add_edge(START, "retrieve_solo_memory")
builder.add_edge("retrieve_solo_memory", "draft_answer")
builder.add_edge("draft_answer", END)
graph = builder.compile()

query = " ".join(sys.argv[1:]) or "Use my memory to plan Avery's next weekly review."
result = graph.invoke({"query": query, "subject": "Avery"})
print(result["answer"])
