import asyncio
import json
import os
import sys
from pathlib import Path
from typing import Optional

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdks" / "python"))

from agents import Agent, Runner, function_tool  # noqa: E402
from solo_client import SoloClient  # noqa: E402


solo = SoloClient(
    base_url=os.getenv("SOLO_URL", "http://127.0.0.1:17821"),
    bearer_token=os.getenv("SOLO_BEARER_TOKEN"),
)


@function_tool
def memory_context(query: str, subject: Optional[str] = None) -> str:
    """Retrieve durable Solo memory context before answering."""
    return json.dumps(solo.context(query, subject=subject, limit=5))


@function_tool
def remember_durable_fact(content: str, salience: float = 0.7) -> str:
    """Store a durable, user-approved fact in Solo memory."""
    return json.dumps(
        solo.remember(
            content,
            source_type="openai_agents_sdk",
            salience=salience,
        )
    )


async def main():
    prompt = " ".join(sys.argv[1:]) or "Use my memory to plan Avery's next weekly review."
    prior_context = solo.context(prompt, limit=5)
    agent = Agent(
        name="Solo-aware assistant",
        instructions=(
            "Use the preloaded Solo context when it is relevant. "
            "Call memory_context if you need more context. "
            "Call remember_durable_fact only for durable, user-approved facts.\n"
            f"Preloaded Solo context: {json.dumps(prior_context)}"
        ),
        tools=[memory_context, remember_durable_fact],
    )
    result = await Runner.run(agent, prompt)
    print(result.final_output)


if __name__ == "__main__":
    asyncio.run(main())
