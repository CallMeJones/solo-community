import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "sdks" / "python"))

from solo_client import SoloClient  # noqa: E402

solo = SoloClient(
    base_url=os.getenv("SOLO_URL", "http://127.0.0.1:17821"),
    bearer_token=os.getenv("SOLO_BEARER_TOKEN"),
)
content = " ".join(sys.argv[1:]) or "Avery prefers planning notes with owners and dates."

saved = solo.remember(content, source_type="sdk_example", salience=0.7)
print(f"remembered {saved['memory_id']}")

for hit in solo.recall("planning notes owners dates", limit=3)["hits"]:
    print(f"- {hit['memory_id']}: {hit['content']}")

context = solo.context("planning notes owners dates", subject="Avery", limit=3)
sections = context["sections"]
print(
    "context: "
    f"recall={sections['recall']['count']} "
    f"facts={sections['facts']['count']} "
    f"themes={sections['themes']['count']}"
)
